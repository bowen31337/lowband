//! End-to-end full-duplex voice loop (FR-2): mic → codec → E2EE session →
//! codec → speaker.
//!
//! This is the piece that turns the tested-but-unwired parts into an actual
//! voice call. Given an established [`SecureSession`], it splits the session
//! into send/receive halves, opens the microphone and speaker (cpal) at each
//! device's native rate, resampling to/from the codec's 8 kHz so any hardware
//! works, and runs two loops:
//!
//! - **capture/send:** pull 20 ms of mic audio → resample to 8 kHz →
//!   [`VoiceSender`] (VAD + DTX + codec) → seal + transmit on the send half.
//! - **receive/playout:** receive on the recv half → decode voice frames →
//!   resample 8 kHz up to the speaker's native rate → push PCM to the speaker.
//!
//! Enabled with `--features audio`; the daemon runs this instead of the
//! receive-only worker when audio is compiled in and a session is established.
//! Verified to build against real ALSA in the `audio-io` CI job; audible
//! confirmation requires real mic/speaker hardware.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use lowband_crypto::SecureSession;

use crate::audio_io::{resample, Microphone, SharedPcm, Speaker, VOICE_SAMPLE_RATE};
use crate::voice::{VoiceFrame, VoiceReceiver, VoiceSender};

/// Run the full-duplex voice loop until the daemon shuts down. Blocks; the
/// caller runs it on its own thread. `_data_dir` is reserved for spooling
/// (e.g. local recording under consent) — unused today.
pub fn run(session: SecureSession, _data_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let (mut tx, mut rx) = session.split()?;
    // Short read timeout so the receive loop notices shutdown promptly.
    rx.set_read_timeout(Some(Duration::from_millis(500)))?;

    // Microphone → mic_buf; spk_buf → speaker. Streams open at each device's
    // NATIVE rate and are resampled to/from the codec's 8 kHz, so any device
    // works. Streams are held for the loop's lifetime.
    let mic_buf = SharedPcm::new();
    let (_mic, mic_hz) = Microphone::open(mic_buf.clone())?;
    let spk_buf = SharedPcm::new();
    let (_spk, spk_hz) = Speaker::open(spk_buf.clone())?;

    eprintln!("lowbandd: voice loop running (mic {mic_hz} Hz, speaker {spk_hz} Hz, codec 8 kHz)");

    // Capture/send thread: accumulate device-rate mic audio, resample 20 ms
    // chunks to 8 kHz, run through the codec, and transmit.
    let capture = thread::spawn(move || {
        let mut sender = VoiceSender::new();
        // 20 ms of device-rate audio per outbound frame.
        let frame_dev = (mic_hz as usize / 50).max(1);
        let mut dev_acc: Vec<i16> = Vec::new();
        while !crate::SHUTDOWN.load(Ordering::Relaxed) {
            let avail = mic_buf.len();
            if avail > 0 {
                let mut buf = vec![0i16; avail];
                let got = mic_buf.pop_into(&mut buf);
                buf.truncate(got);
                dev_acc.extend_from_slice(&buf);
            }
            while dev_acc.len() >= frame_dev {
                let chunk: Vec<i16> = dev_acc.drain(..frame_dev).collect();
                // Resample this 20 ms chunk to the codec's 8 kHz (~160 samples).
                let frame8k = resample(&chunk, mic_hz, VOICE_SAMPLE_RATE);
                let (_outcome, bytes) = sender.process(&frame8k);
                if let Some(b) = bytes {
                    if tx.send(&b).is_err() {
                        return; // peer gone
                    }
                }
            }
            if avail == 0 {
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    // Receive/playout loop: decode 8 kHz frames, resample up to the speaker's
    // native rate, and enqueue for playout.
    let mut receiver = VoiceReceiver::new();
    while !crate::SHUTDOWN.load(Ordering::Relaxed) {
        match rx.recv() {
            Ok(bytes) => {
                if let Some(frame) = VoiceFrame::decode(&bytes) {
                    let pcm8k = receiver.decode(frame);
                    let pcm_dev = resample(&pcm8k, VOICE_SAMPLE_RATE, spk_hz);
                    spk_buf.push(&pcm_dev);
                }
            }
            // Read timeout or a transient error — poll again for shutdown.
            Err(_) => continue,
        }
    }

    let _ = capture.join();
    Ok(())
}

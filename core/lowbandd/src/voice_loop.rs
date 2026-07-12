//! End-to-end full-duplex voice loop (FR-2): mic → codec → E2EE session →
//! codec → speaker.
//!
//! This is the piece that turns the tested-but-unwired parts into an actual
//! voice call. Given an established [`SecureSession`], it splits the session
//! into send/receive halves, opens the microphone and speaker (cpal, 8 kHz
//! mono to match the codec), and runs two loops:
//!
//! - **capture/send:** pull 20 ms frames from the mic → [`VoiceSender`] (VAD +
//!   DTX + codec) → seal + transmit on the send half.
//! - **receive/playout:** receive on the recv half → decode voice frames →
//!   push PCM to the speaker.
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

use crate::audio_io::{Microphone, SharedPcm, Speaker};
use crate::voice::{VoiceFrame, VoiceReceiver, VoiceSender, FRAME_SAMPLES};

/// Run the full-duplex voice loop until the daemon shuts down. Blocks; the
/// caller runs it on its own thread. `_data_dir` is reserved for spooling
/// (e.g. local recording under consent) — unused today.
pub fn run(session: SecureSession, _data_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let (mut tx, mut rx) = session.split()?;
    // Short read timeout so the receive loop notices shutdown promptly.
    rx.set_read_timeout(Some(Duration::from_millis(500)))?;

    // Microphone → mic_buf; spk_buf → speaker. Streams are held for the loop's
    // lifetime (dropping them stops capture/playout).
    let mic_buf = SharedPcm::new();
    let _mic = Microphone::open(mic_buf.clone())?;
    let spk_buf = SharedPcm::new();
    let _spk = Speaker::open(spk_buf.clone())?;

    eprintln!("lowbandd: voice loop running (mic + speaker, 8 kHz)");

    // Capture/send thread: 20 ms frames from the mic → codec → session.
    let capture = thread::spawn(move || {
        let mut sender = VoiceSender::new();
        let mut frame = vec![0i16; FRAME_SAMPLES];
        while !crate::SHUTDOWN.load(Ordering::Relaxed) {
            if mic_buf.len() >= FRAME_SAMPLES {
                mic_buf.pop_into(&mut frame);
                let (_outcome, bytes) = sender.process(&frame);
                if let Some(b) = bytes {
                    if tx.send(&b).is_err() {
                        break; // peer gone
                    }
                }
            } else {
                // Wait for ~a quarter frame of audio to accumulate.
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    // Receive/playout loop on this thread.
    let mut receiver = VoiceReceiver::new();
    while !crate::SHUTDOWN.load(Ordering::Relaxed) {
        match rx.recv() {
            Ok(bytes) => {
                if let Some(frame) = VoiceFrame::decode(&bytes) {
                    let pcm = receiver.decode(frame);
                    spk_buf.push(&pcm);
                }
            }
            // Read timeout or a transient error — poll again for shutdown.
            Err(_) => continue,
        }
    }

    let _ = capture.join();
    Ok(())
}

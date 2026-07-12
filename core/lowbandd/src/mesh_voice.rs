//! Full-duplex **mesh** voice loop (FR-14, v1.2/M5): a group call across a
//! [`MeshSession`](crate::mesh::MeshSession).
//!
//! The 1:1 loop ([`crate::voice_loop`]) bridges one mic and one speaker over a
//! single session. A group call fans that out:
//!
//! - **uplink:** capture once, resample to 8 kHz, encode **once** per 20 ms
//!   frame, and send the same sealed frame to *every* peer's send half.
//! - **downlink:** one receive thread per peer decodes that peer's stream into
//!   its own playout buffer; the playout loop pulls a frame from each peer,
//!   [`mix`](crate::mesh::mix)es them into one conference stream, resamples up
//!   to the speaker's rate, and plays it.
//!
//! Enabled with `--features audio`; the daemon runs this when a mesh room was
//! established. Build-verified against real ALSA in the `audio-io` CI job;
//! audible confirmation needs real mic/speaker hardware on each participant.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::audio_io::{resample, Microphone, SharedPcm, Speaker, VOICE_SAMPLE_RATE};
use crate::mesh::{mix, MeshSession};
use crate::voice::{VoiceFrame, VoiceReceiver, VoiceSender, FRAME_SAMPLES};

/// Run the mesh voice loop until shutdown. Blocks on the caller's thread.
pub fn run(mesh: MeshSession, _data_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let mic_buf = SharedPcm::new();
    let (_mic, mic_hz) = Microphone::open(mic_buf.clone())?;
    let spk_buf = SharedPcm::new();
    let (_spk, spk_hz) = Speaker::open(spk_buf.clone())?;

    eprintln!(
        "lowbandd: mesh voice loop running ({} peers, mic {mic_hz} Hz, speaker {spk_hz} Hz)",
        mesh.peers.len()
    );

    // Split each peer session; keep the send halves for the uplink and spawn a
    // decode thread per peer feeding a dedicated playout buffer.
    let mut senders = Vec::with_capacity(mesh.peers.len());
    let mut peer_bufs = Vec::with_capacity(mesh.peers.len());
    let mut rx_threads = Vec::new();
    for peer in mesh.peers {
        let (tx, mut rx) = peer.session.split()?;
        rx.set_read_timeout(Some(Duration::from_millis(500)))?;
        senders.push(tx);
        let buf = SharedPcm::new();
        peer_bufs.push(buf.clone());
        rx_threads.push(thread::spawn(move || {
            let mut receiver = VoiceReceiver::new();
            while !crate::SHUTDOWN.load(Ordering::Relaxed) {
                match rx.recv() {
                    Ok(bytes) => {
                        if let Some(f) = VoiceFrame::decode(&bytes) {
                            buf.push(&receiver.decode(f));
                        }
                    }
                    Err(_) => continue, // read timeout — re-check shutdown
                }
            }
        }));
    }

    // Uplink: capture → 8 kHz → encode once → send to all peers.
    let capture = thread::spawn(move || {
        let mut sender = VoiceSender::new();
        let mut senders = senders;
        let frame_dev = (mic_hz as usize / 50).max(1);
        let mut dev_acc: Vec<i16> = Vec::new();
        while !crate::SHUTDOWN.load(Ordering::Relaxed) {
            let avail = mic_buf.len();
            if avail > 0 {
                let mut b = vec![0i16; avail];
                let got = mic_buf.pop_into(&mut b);
                b.truncate(got);
                dev_acc.extend_from_slice(&b);
            }
            while dev_acc.len() >= frame_dev {
                let chunk: Vec<i16> = dev_acc.drain(..frame_dev).collect();
                let f8k = resample(&chunk, mic_hz, VOICE_SAMPLE_RATE);
                let (_o, bytes) = sender.process(&f8k);
                if let Some(b) = bytes {
                    // Drop any peer whose channel has died; keep the rest.
                    senders.retain_mut(|s| s.send(&b).is_ok());
                }
            }
            if avail == 0 {
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    // Downlink: mix a frame from every peer that has one, resample, play.
    while !crate::SHUTDOWN.load(Ordering::Relaxed) {
        let mut frames: Vec<Vec<i16>> = Vec::new();
        for buf in &peer_bufs {
            if buf.len() >= FRAME_SAMPLES {
                let mut f = vec![0i16; FRAME_SAMPLES];
                buf.pop_into(&mut f);
                frames.push(f);
            }
        }
        if frames.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let refs: Vec<&[i16]> = frames.iter().map(Vec::as_slice).collect();
        let mixed = mix(&refs);
        spk_buf.push(&resample(&mixed, VOICE_SAMPLE_RATE, spk_hz));
    }

    let _ = capture.join();
    for t in rx_threads {
        let _ = t.join();
    }
    Ok(())
}

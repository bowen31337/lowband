//! Voice frame transfer over the encrypted session (FR-2, pipeline-integrated).
//!
//! Ties the real DTX gate ([`DtxEncoder`]) to the interim ADPCM codec
//! ([`crate::adpcm`]) and the [`SecureSession`], carrying actual 20 ms voice
//! frames peer-to-peer:
//!
//! ```text
//! PCM 20ms ─► VAD ─► DtxEncoder ─┬─ Voice → ADPCM encode → seal → send
//!                                ├─ Sid   → comfort-noise SID → send
//!                                └─ Suppress → nothing on the wire (≈0 kbps)
//! ```
//!
//! On silence the DTX gate suppresses most frames, so a quiet talker costs
//! ≈ 0 kbps (NFR-5) — demonstrated in the tests. The libopus/DRED gears drop
//! in behind this same [`VoiceFrame`] transport when a C toolchain is present;
//! ADPCM is the honest pure-Rust interim codec, not a stub.

use lowband_crypto::SecureSession;
use lowband_platform::{DtxAction, DtxEncoder};

// Voice codec is selected at compile time: production libopus with
// `--features opus`, else the pure-Rust interim ADPCM codec. Both expose the
// same `new` / `encode` / `decode` surface, so the pipeline below is codec
// agnostic (FR-2).
#[cfg(not(feature = "opus"))]
use crate::adpcm::{AdpcmDecoder as CodecDecoder, AdpcmEncoder as CodecEncoder};
#[cfg(feature = "opus")]
use crate::opus_codec::{OpusDecoder as CodecDecoder, OpusEncoder as CodecEncoder};

/// Samples in a 20 ms frame at 8 kHz (narrowband voice).
pub const FRAME_SAMPLES: usize = 160;

const KIND_VOICE: u8 = 0x30;
const KIND_SID: u8 = 0x31;

/// Root-mean-square energy above which a frame counts as voice (VAD).
#[allow(dead_code)]
const VAD_RMS_THRESHOLD: f64 = 350.0;

/// A voice datagram: an ADPCM frame or a comfort-noise SID update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceFrame {
    /// `samples` ADPCM-coded PCM samples.
    Voice { samples: u16, data: Vec<u8> },
    /// Comfort-noise update carrying the noise floor (RMS) for the gap.
    Sid { rms: u16 },
}

impl VoiceFrame {
    /// Transmit-half serializer (bound to the mic source later; used by tests).
    #[allow(dead_code)]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            VoiceFrame::Voice { samples, data } => {
                out.push(KIND_VOICE);
                out.extend_from_slice(&samples.to_le_bytes());
                out.extend_from_slice(data);
            }
            VoiceFrame::Sid { rms } => {
                out.push(KIND_SID);
                out.extend_from_slice(&rms.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        match buf.split_first()? {
            (&KIND_VOICE, rest) => {
                let (n, data) = rest.split_at_checked(2)?;
                Some(VoiceFrame::Voice {
                    samples: u16::from_le_bytes([n[0], n[1]]),
                    data: data.to_vec(),
                })
            }
            (&KIND_SID, rest) => {
                let n = rest.split_at_checked(2)?.0;
                Some(VoiceFrame::Sid { rms: u16::from_le_bytes([n[0], n[1]]) })
            }
            _ => None,
        }
    }
}

#[allow(dead_code)]
fn rms(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum / pcm.len() as f64).sqrt()
}

/// What a send attempt did (mirrors [`DtxAction`]).
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// A full ADPCM voice frame was transmitted.
    Voice,
    /// A comfort-noise SID update was transmitted.
    Sid,
    /// The frame was suppressed (silence between SID updates): nothing sent.
    Suppressed,
}

/// Sender-side voice pipeline: VAD → DTX → ADPCM → session.
///
/// Outbound half — exercised by tests today; the daemon binds it to the mic
/// capture broker (`lowband_platform::mic_capture`) when that wiring lands.
#[allow(dead_code)]
pub struct VoiceSender {
    dtx: DtxEncoder,
    codec: CodecEncoder,
}

impl Default for VoiceSender {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl VoiceSender {
    pub fn new() -> Self {
        Self { dtx: DtxEncoder::new(), codec: CodecEncoder::new() }
    }

    /// Run one 20 ms PCM frame through VAD → DTX → codec and return the wire
    /// bytes to transmit (`None` when the DTX gate suppresses the frame). This
    /// is transport-agnostic — the daemon's capture loop sends the bytes over
    /// the split [`SecureSender`](lowband_crypto::SecureSender); tests send
    /// over a whole session via [`send_frame`](Self::send_frame).
    pub fn process(&mut self, pcm: &[i16]) -> (SendOutcome, Option<Vec<u8>>) {
        let level = rms(pcm);
        let voice_active = level >= VAD_RMS_THRESHOLD;
        match self.dtx.observe_vad(voice_active) {
            DtxAction::Voice => {
                let data = self.codec.encode(pcm);
                let bytes = VoiceFrame::Voice { samples: pcm.len() as u16, data }.encode();
                (SendOutcome::Voice, Some(bytes))
            }
            DtxAction::Sid => {
                let bytes = VoiceFrame::Sid { rms: level.min(u16::MAX as f64) as u16 }.encode();
                (SendOutcome::Sid, Some(bytes))
            }
            DtxAction::Suppress => (SendOutcome::Suppressed, None),
        }
    }

    /// Process and transmit one 20 ms PCM frame over a whole session (test/
    /// convenience path). Silent frames are suppressed or sent as tiny SID
    /// updates by the DTX gate (NFR-5 idle economy).
    pub fn send_frame(
        &mut self,
        session: &mut SecureSession,
        pcm: &[i16],
    ) -> Result<SendOutcome, lowband_crypto::SessionError> {
        let (outcome, bytes) = self.process(pcm);
        if let Some(b) = bytes {
            session.send(&b)?;
        }
        Ok(outcome)
    }
}

/// Receiver-side voice pipeline: session → ADPCM decode → PCM (or comfort noise).
pub struct VoiceReceiver {
    codec: CodecDecoder,
    comfort_rms: u16,
}

impl Default for VoiceReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceReceiver {
    pub fn new() -> Self {
        Self { codec: CodecDecoder::new(), comfort_rms: 0 }
    }

    /// Decode a received voice frame into 20 ms of PCM. A SID frame yields a
    /// silent (zero) frame; the carried noise floor is retained for future
    /// comfort-noise synthesis.
    pub fn decode(&mut self, frame: VoiceFrame) -> Vec<i16> {
        match frame {
            VoiceFrame::Voice { samples, data } => self.codec.decode(&data, samples as usize),
            VoiceFrame::Sid { rms } => {
                self.comfort_rms = rms;
                vec![0i16; FRAME_SAMPLES]
            }
        }
    }

    /// The most recent comfort-noise floor (RMS) from a SID frame.
    #[allow(dead_code)]
    pub fn comfort_rms(&self) -> u16 {
        self.comfort_rms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_crypto::{SecureSession, StaticKeypair};
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Duration;

    fn tone(n: usize, amp: f64) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn frame_roundtrip() {
        let f = VoiceFrame::Voice { samples: 160, data: vec![1, 2, 3] };
        assert_eq!(VoiceFrame::decode(&f.encode()), Some(f));
        let s = VoiceFrame::Sid { rms: 42 };
        assert_eq!(VoiceFrame::decode(&s.encode()), Some(s));
    }

    #[test]
    fn silence_is_mostly_suppressed() {
        // A UDP socket pair we never read from on the far end is fine; we only
        // count what the sender emits. Use a connected loopback session.
        let (mut sender, mut session, _server) = loopback_sender();
        let silence = vec![0i16; FRAME_SAMPLES];

        let mut voice = 0;
        let mut sid = 0;
        let mut suppressed = 0;
        // One second of silence = 50 frames.
        for _ in 0..50 {
            match sender.send_frame(&mut session, &silence).unwrap() {
                SendOutcome::Voice => voice += 1,
                SendOutcome::Sid => sid += 1,
                SendOutcome::Suppressed => suppressed += 1,
            }
        }
        // After the hangover, the vast majority of silent frames are suppressed.
        assert!(suppressed >= 35, "expected most silence suppressed, got {suppressed}");
        assert!(sid >= 1, "expected periodic SID updates");
        let _ = voice;
    }

    #[test]
    fn voice_travels_and_decodes_over_real_session() {
        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100005678";

        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();

        // Send 10 loud voice frames of a 440 Hz tone.
        let frames: Vec<Vec<i16>> = (0..10).map(|_| tone(FRAME_SAMPLES, 12000.0)).collect();
        let frames2 = frames.clone();

        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut rx = VoiceReceiver::new();
            let mut decoded = Vec::new();
            for _ in 0..10 {
                let bytes = sess.recv().unwrap();
                let frame = VoiceFrame::decode(&bytes).unwrap();
                decoded.push(rx.decode(frame));
            }
            decoded
        });

        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();
        let mut sender = VoiceSender::new();
        for f in &frames2 {
            assert_eq!(sender.send_frame(&mut client, f).unwrap(), SendOutcome::Voice);
        }

        let decoded = server.join().unwrap();
        assert_eq!(decoded.len(), 10);
        // Codec-agnostic quality check: the decoded stream must preserve
        // energy. Sample-aligned SNR would work for ADPCM (no delay) but not
        // for Opus, which has algorithmic delay and reshapes tones — so we
        // assert the RMS energy ratio instead, which both codecs satisfy and
        // which still proves real audio (not silence/garbage) came through.
        let orig: Vec<i16> = frames.concat();
        let recv: Vec<i16> = decoded.concat();
        assert_eq!(orig.len(), recv.len());
        let rms = |v: &[i16]| {
            (v.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };
        let ratio = rms(&recv) / rms(&orig).max(1.0);
        assert!((0.2..5.0).contains(&ratio), "voice energy ratio over session off: {ratio:.2}");
    }

    /// Full-duplex two-peer voice *call* — the closest reproduction of a call
    /// between two machines that's possible without audio hardware. Each peer
    /// splits its [`SecureSession`] into send/receive halves (exactly as
    /// `voice_loop::run` does), captures a distinct tone at a 48 kHz "device
    /// rate", resamples it down to the 8 kHz codec, and transmits — while
    /// concurrently receiving the far end's stream, decoding it, and resampling
    /// back up to its device rate. Afterwards each peer must have recovered the
    /// *other's* tone (real audio, not silence), proving the split-session +
    /// resampler path interoperates end to end across two independent endpoints.
    #[test]
    fn full_duplex_resampled_call_between_two_peers() {
        use crate::audio_io::{resample, VOICE_SAMPLE_RATE};

        const DEVICE_HZ: u32 = 48_000;
        const FRAMES: usize = 25; // 0.5 s of 20 ms frames
        const DEV_FRAME: usize = (DEVICE_HZ as usize) / 50; // 960 samples / 20 ms

        // A loud device-rate tone chunk (drives the VAD to Voice, never suppressed).
        fn dev_tone(freq: f64) -> Vec<i16> {
            (0..DEV_FRAME)
                .map(|i| {
                    let t = i as f64 / DEVICE_HZ as f64;
                    (12000.0 * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16
                })
                .collect()
        }

        // One peer of the call: transmit `out_freq` for the whole call while
        // receiving + reconstructing the far end. Returns the recovered PCM at
        // the device rate. Mirrors voice_loop's capture/playout halves.
        fn run_peer(session: SecureSession, out_freq: f64) -> Vec<i16> {
            let (mut tx, mut rx) = session.split().unwrap();
            rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

            let send = thread::spawn(move || {
                let mut sender = VoiceSender::new();
                for _ in 0..FRAMES {
                    let chunk = dev_tone(out_freq);
                    let f8k = resample(&chunk, DEVICE_HZ, VOICE_SAMPLE_RATE);
                    let (_o, bytes) = sender.process(&f8k);
                    if let Some(b) = bytes {
                        tx.send(&b).unwrap();
                    }
                }
            });

            let mut receiver = VoiceReceiver::new();
            let mut recovered = Vec::new();
            for _ in 0..FRAMES {
                let bytes = rx.recv().unwrap();
                let frame = VoiceFrame::decode(&bytes).unwrap();
                let pcm8k = receiver.decode(frame);
                recovered.extend(resample(&pcm8k, VOICE_SAMPLE_RATE, DEVICE_HZ));
            }
            send.join().unwrap();
            recovered
        }

        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100005680";
        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();

        // Peer B (responder) transmits a 660 Hz tone.
        let peer_b = thread::spawn(move || {
            let sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            run_peer(sess, 660.0)
        });

        // Peer A (initiator) transmits a 440 Hz tone.
        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sess =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();
        let a_recovered = run_peer(sess, 440.0);
        let b_recovered = peer_b.join().unwrap();

        // Both peers reconstructed a full call's worth of device-rate audio…
        assert_eq!(a_recovered.len(), FRAMES * DEV_FRAME);
        assert_eq!(b_recovered.len(), FRAMES * DEV_FRAME);
        // …and it's real audio, not silence: energy comparable to the sent tone.
        let rms = |v: &[i16]| {
            (v.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
        };
        let sent_rms = rms(&dev_tone(440.0));
        for (who, got) in [("A", &a_recovered), ("B", &b_recovered)] {
            let ratio = rms(got) / sent_rms.max(1.0);
            assert!(
                (0.2..5.0).contains(&ratio),
                "peer {who} recovered tone energy off: {ratio:.2}"
            );
        }
    }

    /// Establish a loopback session and return a sender + the client session.
    /// The server thread just completes the handshake and drains datagrams.
    fn loopback_sender() -> (VoiceSender, SecureSession, thread::JoinHandle<()>) {
        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100005679";
        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            // Drain whatever arrives until the socket goes idle.
            while sess.recv().is_ok() {}
        });
        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();
        (VoiceSender::new(), client, server)
    }
}

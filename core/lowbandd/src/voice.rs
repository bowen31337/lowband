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

    /// Process and transmit one 20 ms PCM frame. Silent frames are suppressed
    /// or sent as tiny SID updates by the DTX gate (NFR-5 idle economy).
    pub fn send_frame(
        &mut self,
        session: &mut SecureSession,
        pcm: &[i16],
    ) -> Result<SendOutcome, lowband_crypto::SessionError> {
        let level = rms(pcm);
        let voice_active = level >= VAD_RMS_THRESHOLD;
        match self.dtx.observe_vad(voice_active) {
            DtxAction::Voice => {
                let data = self.codec.encode(pcm);
                session.send(&VoiceFrame::Voice { samples: pcm.len() as u16, data }.encode())?;
                Ok(SendOutcome::Voice)
            }
            DtxAction::Sid => {
                session.send(&VoiceFrame::Sid { rms: level.min(u16::MAX as f64) as u16 }.encode())?;
                Ok(SendOutcome::Sid)
            }
            DtxAction::Suppress => Ok(SendOutcome::Suppressed),
        }
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
        // The decoded tone should track the original within ADPCM tolerance.
        let orig: Vec<i16> = frames.concat();
        let recv: Vec<i16> = decoded.concat();
        assert_eq!(orig.len(), recv.len());
        let signal: f64 = orig.iter().map(|&s| (s as f64).powi(2)).sum();
        let noise: f64 =
            orig.iter().zip(&recv).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum();
        let snr_db = 10.0 * (signal / noise.max(1.0)).log10();
        assert!(snr_db > 15.0, "voice SNR over session too low: {snr_db:.1} dB");
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

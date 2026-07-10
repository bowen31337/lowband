//! Unified inbound router for the encrypted session.
//!
//! One [`SecureSession`] carries every application channel — control messages
//! (chat / clipboard / panic, via [`MessageFrame`]) and bulk file transfer
//! (via [`XferFrame`]). Their wire kind bytes are disjoint by construction
//! (`MessageFrame` uses `0x01–0x04`, `XferFrame` uses `0x10–0x12`), so a
//! single receiver can peek the first byte and route each datagram to the
//! right decoder without any envelope. This is the one place that knows both
//! protocols share the channel; without it a receiver would have to guess
//! which decoder to apply.

use lowband_crypto::SecureSession;
use lowband_messaging::clipboard::ClipboardSession;
use lowband_messaging::grants::ControlSession;
use lowband_messaging::panic_key::{PanicController, PanicNoticeReceiver};
use lowband_messaging::MessageFrame;

use crate::dataplane::{dispatch, Delivered};
use crate::file_transfer::{FileReceiver, Progress, XferError, XferFrame};
use crate::screen_transfer::{ScreenFrame, ScreenReceiver};
use crate::voice::{VoiceFrame, VoiceReceiver};

/// Which protocol a received datagram belongs to, by its kind byte.
#[derive(Debug, PartialEq, Eq)]
pub enum Channel {
    /// Control-plane message (chat / clipboard / panic).
    Message,
    /// Bulk file-transfer frame.
    File,
    /// Screen-frame transfer.
    Screen,
    /// Voice frame (ADPCM / SID).
    Voice,
    /// Unrecognized leading byte.
    Unknown(u8),
}

/// Classify a datagram by its first (kind) byte without fully decoding it.
pub fn classify(bytes: &[u8]) -> Channel {
    match bytes.first() {
        Some(0x01..=0x04) => Channel::Message,
        Some(0x10..=0x12) => Channel::File,
        Some(0x20..=0x22) => Channel::Screen,
        Some(0x30..=0x31) => Channel::Voice,
        Some(&b) => Channel::Unknown(b),
        None => Channel::Unknown(0),
    }
}

/// What handling one inbound datagram produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Handled {
    /// A control message was dispatched through its subsystem gate.
    Message(Delivered),
    /// A file-transfer frame advanced the receiver.
    File(Progress),
    /// A screen tile was applied; `Some(bytes)` on a completed frame.
    Screen { complete: bool },
    /// A voice frame decoded to `samples` PCM samples of playout.
    Voice { samples: usize },
    /// The datagram's kind byte matched no known channel.
    Unknown(u8),
}

/// Receiver state for one peer session: the control-plane subsystems plus the
/// file receiver, dispatched from a single inbound stream.
pub struct InboundRouter {
    pub clipboard: ClipboardSession,
    pub control: ControlSession,
    pub panic: PanicController,
    pub panic_rx: PanicNoticeReceiver,
    pub files: FileReceiver,
    pub screen: ScreenReceiver,
    /// The most recently completed screen framebuffer (BGRA8), if any.
    pub last_screen: Option<Vec<u8>>,
    pub voice: VoiceReceiver,
    /// The most recently decoded voice PCM frame (playout), if any.
    pub last_voice: Option<Vec<i16>>,
}

impl InboundRouter {
    pub fn new(files: FileReceiver) -> Self {
        Self {
            clipboard: ClipboardSession::new(),
            control: ControlSession::new(),
            panic: PanicController::new(),
            panic_rx: PanicNoticeReceiver::new(),
            files,
            screen: ScreenReceiver::new(),
            last_screen: None,
            voice: VoiceReceiver::new(),
            last_voice: None,
        }
    }

    /// Decode and handle one already-received datagram's plaintext.
    pub fn handle(&mut self, bytes: &[u8]) -> Result<Handled, XferError> {
        match classify(bytes) {
            Channel::Message => {
                // MessageFrame decode errors are treated as a dropped frame,
                // surfaced as a rejected delivery rather than tearing down.
                match MessageFrame::decode(bytes) {
                    Ok(frame) => Ok(Handled::Message(dispatch(
                        frame,
                        &self.clipboard,
                        &mut self.control,
                        &mut self.panic,
                        &mut self.panic_rx,
                    ))),
                    Err(_) => Ok(Handled::Message(Delivered::Rejected("undecodable message"))),
                }
            }
            Channel::File => {
                let frame = XferFrame::decode(bytes)?;
                Ok(Handled::File(self.files.apply(frame)?))
            }
            Channel::Screen => {
                let frame = ScreenFrame::decode(bytes).map_err(|_| XferError::Truncated)?;
                let done = self.screen.apply(frame).map_err(|_| XferError::Truncated)?;
                let complete = done.is_some();
                if complete {
                    self.last_screen = done;
                }
                Ok(Handled::Screen { complete })
            }
            Channel::Voice => {
                let frame = VoiceFrame::decode(bytes).ok_or(XferError::Truncated)?;
                let pcm = self.voice.decode(frame);
                let samples = pcm.len();
                self.last_voice = Some(pcm);
                Ok(Handled::Voice { samples })
            }
            Channel::Unknown(b) => Ok(Handled::Unknown(b)),
        }
    }

    /// Receive one datagram from `session` and handle it.
    pub fn recv_and_handle(&mut self, session: &mut SecureSession) -> Result<Handled, XferError> {
        let bytes = session.recv()?;
        self.handle(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_crypto::{SecureSession, StaticKeypair};
    use lowband_messaging::clipboard::ClipboardGrant;
    use std::net::UdpSocket;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("lb-inbound-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn classify_routes_by_kind_byte() {
        assert_eq!(classify(&MessageFrame::Chat("x".into()).encode()), Channel::Message);
        assert_eq!(classify(&XferFrame::Complete.encode()), Channel::File);
        assert_eq!(classify(&[0x7F]), Channel::Unknown(0x7F));
        assert_eq!(classify(&[]), Channel::Unknown(0));
    }

    #[test]
    fn one_session_carries_chat_and_a_file() {
        let dst = tmp("dst");
        let resume = tmp("resume");
        let src = tmp("src");
        let data: Vec<u8> = (0..2500u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(&src, &data).unwrap();

        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100000999";

        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();
        let dst2 = dst.clone();
        let resume2 = resume.clone();

        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut router = InboundRouter::new(FileReceiver::new(dst2, resume2));
            router.clipboard.set_grant(Some(ClipboardGrant::new()));

            let mut got_chat = None;
            // File completes last (sent last), so break there and read the
            // screen the router reconstructed along the way.
            loop {
                match router.recv_and_handle(&mut sess).unwrap() {
                    Handled::Message(Delivered::Chat(t)) => got_chat = Some(t),
                    Handled::File(Progress::Complete) => break,
                    _ => {}
                }
            }
            let got_screen = router.last_screen.clone();
            let got_voice = router.last_voice.clone();
            (got_chat, got_screen, got_voice)
        });

        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();

        // Interleave every plane — chat, voice, screen, file — on one channel;
        // all four must route correctly.
        crate::dataplane::send_message(&mut client, &MessageFrame::Chat("starting".into())).unwrap();
        let mut vsender = crate::voice::VoiceSender::new();
        let tone: Vec<i16> = (0..crate::voice::FRAME_SAMPLES)
            .map(|i| (12000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 8000.0).sin()) as i16)
            .collect();
        vsender.send_frame(&mut client, &tone).unwrap();
        let screen = crate::screen_transfer::text_screen(64, 32);
        crate::screen_transfer::send_frame(&mut client, 64, 32, &screen).unwrap();
        crate::file_transfer::send_file(&mut client, &src).unwrap();

        let (chat, screen_out, voice_out) = server.join().unwrap();
        assert_eq!(chat.as_deref(), Some("starting"), "chat routed to the message plane");
        assert_eq!(screen_out.as_deref(), Some(&screen[..]), "screen routed + pixel-perfect");
        assert_eq!(
            voice_out.map(|v| v.len()),
            Some(crate::voice::FRAME_SAMPLES),
            "voice routed + decoded to a full frame"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), data, "file routed to the xfer plane, intact");
        for p in [&dst, &resume, &src] {
            let _ = std::fs::remove_file(p);
        }
    }
}

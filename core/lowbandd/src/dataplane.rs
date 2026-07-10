//! Application data plane over the encrypted session.
//!
//! Carries the non-media channels — chat, clipboard, and the panic notice —
//! as [`MessageFrame`]s sealed into the peer's [`SecureSession`].  This is
//! what turns those messaging types from tested library values into real
//! peer-to-peer exchanges: every frame is ChaCha20-Poly1305 sealed on send
//! and authenticated on receipt, so an on-path relay sees ciphertext only.
//!
//! The receive side gates each frame through the owning subsystem before it
//! takes effect: clipboard text/files require a live [`ClipboardSession`]
//! grant, and a panic notice severs the local control grant. Chat is always
//! delivered (FR-10). Anything rejected by a gate is dropped without effect.

use lowband_crypto::SecureSession;
use lowband_messaging::clipboard::ClipboardSession;
use lowband_messaging::grants::ControlSession;
use lowband_messaging::panic_key::{PanicController, PanicNoticeReceiver};
use lowband_messaging::MessageFrame;

/// Send one application message to the peer over the encrypted channel.
pub fn send_message(
    session: &mut SecureSession,
    frame: &MessageFrame,
) -> Result<(), lowband_crypto::SessionError> {
    session.send(&frame.encode())
}

/// What a received frame did once dispatched through its subsystem gate.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivered {
    /// A chat message was accepted (always delivered — FR-10).
    Chat(String),
    /// Clipboard text accepted under a live grant.
    ClipboardText(String),
    /// A clipboard file offer accepted (count/size/name-safety all passed).
    ClipboardFiles(usize),
    /// A panic notice severed the local control grant (`true`) or was a
    /// duplicate retransmit (`false`).
    Panic { severed: bool },
    /// The frame was rejected by its subsystem gate (e.g. no clipboard grant,
    /// unsafe file name) and had no effect. Carries a short reason for logging.
    Rejected(&'static str),
}

/// Receive and dispatch one application message.
///
/// Blocks on the session socket for the next datagram, authenticates and
/// decodes it, then routes it through the relevant subsystem:
///
/// - **Chat** → returned as [`Delivered::Chat`] (no gate).
/// - **ClipboardText / ClipboardFiles** → gated by `clipboard`.
/// - **Panic** → applied to `control`/`panic` via `panic_rx`.
///
/// A datagram that fails authentication or decode surfaces as an error; a
/// well-formed frame rejected by a gate surfaces as [`Delivered::Rejected`].
pub fn recv_and_dispatch(
    session: &mut SecureSession,
    clipboard: &ClipboardSession,
    control: &mut ControlSession,
    panic: &mut PanicController,
    panic_rx: &mut PanicNoticeReceiver,
) -> Result<Delivered, DataPlaneError> {
    let bytes = session.recv().map_err(DataPlaneError::Session)?;
    let frame = MessageFrame::decode(&bytes).map_err(DataPlaneError::Frame)?;
    Ok(dispatch(frame, clipboard, control, panic, panic_rx))
}

/// Route a decoded frame through its subsystem gate. Split out from
/// [`recv_and_dispatch`] so the routing logic is unit-testable without a
/// live socket.
pub fn dispatch(
    frame: MessageFrame,
    clipboard: &ClipboardSession,
    control: &mut ControlSession,
    panic: &mut PanicController,
    panic_rx: &mut PanicNoticeReceiver,
) -> Delivered {
    match frame {
        MessageFrame::Chat(text) => Delivered::Chat(text),
        MessageFrame::ClipboardText(text) => match clipboard.apply_remote(&text) {
            Ok(()) => Delivered::ClipboardText(text),
            Err(_) => Delivered::Rejected("clipboard text rejected"),
        },
        MessageFrame::ClipboardFiles(offer) => match clipboard.apply_remote_files(&offer) {
            Ok(()) => Delivered::ClipboardFiles(offer.entries.len()),
            Err(_) => Delivered::Rejected("clipboard files rejected"),
        },
        MessageFrame::Panic(notice) => {
            let severed = panic_rx.apply(notice, control, panic);
            Delivered::Panic { severed }
        }
    }
}

/// Error receiving/decoding an application frame.
#[derive(Debug)]
pub enum DataPlaneError {
    Session(lowband_crypto::SessionError),
    Frame(lowband_messaging::FrameError),
}

impl std::fmt::Display for DataPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataPlaneError::Session(e) => write!(f, "data plane session: {e}"),
            DataPlaneError::Frame(e) => write!(f, "data plane frame: {e}"),
        }
    }
}

impl std::error::Error for DataPlaneError {}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_messaging::clipboard::{ClipboardFileEntry, ClipboardFileOffer, ClipboardGrant};
    use lowband_messaging::grants::ControlGrant;
    use lowband_messaging::panic_key::PanicNotice;

    fn parts() -> (ClipboardSession, ControlSession, PanicController, PanicNoticeReceiver) {
        (
            ClipboardSession::new(),
            ControlSession::new(),
            PanicController::new(),
            PanicNoticeReceiver::new(),
        )
    }

    #[test]
    fn chat_always_delivers() {
        let (cb, mut ctrl, mut pc, mut rx) = parts();
        let d = dispatch(MessageFrame::Chat("hi".into()), &cb, &mut ctrl, &mut pc, &mut rx);
        assert_eq!(d, Delivered::Chat("hi".into()));
    }

    #[test]
    fn clipboard_text_needs_grant() {
        let (mut cb, mut ctrl, mut pc, mut rx) = parts();
        // No grant → rejected.
        assert_eq!(
            dispatch(MessageFrame::ClipboardText("x".into()), &cb, &mut ctrl, &mut pc, &mut rx),
            Delivered::Rejected("clipboard text rejected")
        );
        // With grant → delivered.
        cb.set_grant(Some(ClipboardGrant::new()));
        assert_eq!(
            dispatch(MessageFrame::ClipboardText("x".into()), &cb, &mut ctrl, &mut pc, &mut rx),
            Delivered::ClipboardText("x".into())
        );
    }

    #[test]
    fn unsafe_clipboard_file_offer_is_rejected() {
        let (mut cb, mut ctrl, mut pc, mut rx) = parts();
        cb.set_grant(Some(ClipboardGrant::new()));
        let evil = ClipboardFileOffer {
            entries: vec![ClipboardFileEntry { name: "../../etc/passwd".into(), size: 1 }],
        };
        assert_eq!(
            dispatch(MessageFrame::ClipboardFiles(evil), &cb, &mut ctrl, &mut pc, &mut rx),
            Delivered::Rejected("clipboard files rejected")
        );
    }

    #[test]
    fn frames_travel_encrypted_over_a_real_session() {
        use lowband_crypto::{SecureSession, StaticKeypair};
        use std::net::UdpSocket;
        use std::thread;
        use std::time::Duration;

        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100000777";

        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();

        // Responder: establish, then dispatch two inbound frames.
        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut cb = ClipboardSession::new();
            cb.set_grant(Some(ClipboardGrant::new()));
            let mut ctrl = ControlSession::new();
            let mut pc = PanicController::new();
            let mut rx = PanicNoticeReceiver::new();

            let first = recv_and_dispatch(&mut sess, &cb, &mut ctrl, &mut pc, &mut rx).unwrap();
            let second = recv_and_dispatch(&mut sess, &cb, &mut ctrl, &mut pc, &mut rx).unwrap();
            (first, second)
        });

        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();

        send_message(&mut client, &MessageFrame::Chat("fix applied".into())).unwrap();
        send_message(
            &mut client,
            &MessageFrame::ClipboardText("copied config".into()),
        )
        .unwrap();

        let (first, second) = server.join().unwrap();
        assert_eq!(first, Delivered::Chat("fix applied".into()));
        assert_eq!(second, Delivered::ClipboardText("copied config".into()));
    }

    #[test]
    fn panic_notice_severs_control() {
        let (cb, mut ctrl, mut pc, mut rx) = parts();
        ctrl.set_grant(Some(ControlGrant::new()));
        pc.set_transport_up(true);
        assert!(ctrl.apply_event().is_ok());

        let d = dispatch(MessageFrame::Panic(PanicNotice { seq: 1 }), &cb, &mut ctrl, &mut pc, &mut rx);
        assert_eq!(d, Delivered::Panic { severed: true });
        assert!(ctrl.apply_event().is_err(), "control must be severed after panic");

        // Retransmit is a no-op.
        let d2 = dispatch(MessageFrame::Panic(PanicNotice { seq: 1 }), &cb, &mut ctrl, &mut pc, &mut rx);
        assert_eq!(d2, Delivered::Panic { severed: false });
    }
}

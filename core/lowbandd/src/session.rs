//! Peer session establishment for the daemon — join code to E2EE channel.
//!
//! This is the production code path the PRD eval flagged as missing: the
//! `lowbandd` daemon turning a signaling rendezvous into a live, encrypted
//! [`SecureSession`].  It ties [`SignalingClient`] (the rendezvous) to
//! [`SecureSession`] (Noise-IK over UDP), exchanging each peer's UDP
//! transport address — and the responder's static public key — as ICE
//! candidates through the signaling server.
//!
//! # Role mapping
//!
//! | UX role | Noise-IK role | Does |
//! |---------|---------------|------|
//! | host (technician) | initiator | creates the code, learns the joiner's static key + address via signaling, sends msg1 |
//! | join (assisted)   | responder | enters the code, publishes its static key + address, answers msg1 |
//!
//! Noise-IK requires the initiator to know the responder's static public key
//! ahead of time; the joiner publishes it as a `key:<hex>` candidate, which
//! the host reads before connecting.  The `session_code` binds the handshake
//! prologue, so a wrong code fails the AEAD tag on the first message.

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use lowband_crypto::{SecureSession, StaticKeypair};
use lowband_signaling::{ClientError, SignalingClient};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Errors establishing a peer session.
#[derive(Debug)]
pub enum EstablishError {
    Signaling(ClientError),
    Session(lowband_crypto::SessionError),
    Io(std::io::Error),
    /// The peer did not publish the expected candidate before `timeout`.
    Timeout(&'static str),
    /// A `key:` candidate was malformed.
    BadPeerKey,
}

impl std::fmt::Display for EstablishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EstablishError::Signaling(e) => write!(f, "signaling: {e}"),
            EstablishError::Session(e) => write!(f, "session: {e}"),
            EstablishError::Io(e) => write!(f, "io: {e}"),
            EstablishError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            EstablishError::BadPeerKey => write!(f, "peer published a malformed static key"),
        }
    }
}

impl std::error::Error for EstablishError {}

impl From<ClientError> for EstablishError {
    fn from(e: ClientError) -> Self {
        EstablishError::Signaling(e)
    }
}
impl From<lowband_crypto::SessionError> for EstablishError {
    fn from(e: lowband_crypto::SessionError) -> Self {
        EstablishError::Session(e)
    }
}
impl From<std::io::Error> for EstablishError {
    fn from(e: std::io::Error) -> Self {
        EstablishError::Io(e)
    }
}

type Result<T> = std::result::Result<T, EstablishError>;

/// Host side (technician / Noise initiator).
///
/// Creates a session and invokes `on_code` with the join code **immediately**
/// — before the blocking wait — so the caller can read it to the assisted
/// user (who needs it to join). Then publishes the offer + transport address,
/// waits for the joiner's static key and address, and completes Noise-IK as
/// initiator. Returns the code and the live channel.
pub fn establish_host(
    sig: &SignalingClient,
    static_key: &StaticKeypair,
    handshake_timeout: Duration,
    on_code: impl FnOnce(&str),
) -> Result<(String, SecureSession)> {
    let code = sig.create_session()?;
    on_code(&code);

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(handshake_timeout))?;
    let my_addr = sock.local_addr()?;

    sig.post_offer(&code, "lowband/1 offer")?;
    sig.post_candidate(&code, &format!("udp:{my_addr}"))?;

    let deadline = Instant::now() + handshake_timeout;
    let peer_pub = wait_for(deadline, "joiner static key", || {
        peer_candidate(sig, &code, "key:", |v| decode_key(v))
    })?;
    // Exclude our own address inside the parse closure: the host posted its
    // own `udp:` candidate first, so filtering after `find_map` would always
    // stop on it and never reach the joiner's.
    let peer_addr = wait_for(deadline, "joiner address", || {
        peer_candidate(sig, &code, "udp:", |v| {
            v.parse::<SocketAddr>().ok().filter(|a| *a != my_addr)
        })
    })?;

    let session = SecureSession::connect(sock, peer_addr, static_key, peer_pub, &code)?;
    sig.mark_connected(&code)?;
    Ok((code, session))
}

/// Join side (assisted user / Noise responder).
///
/// Validates the code against the server, publishes this peer's static key +
/// transport address, posts an answer, then completes Noise-IK as responder.
pub fn establish_join(
    sig: &SignalingClient,
    code: &str,
    static_key: &StaticKeypair,
    handshake_timeout: Duration,
) -> Result<SecureSession> {
    // Confirm the code is live (and surface a 404 early) before binding.
    sig.join(code)?;

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(handshake_timeout))?;
    let my_addr = sock.local_addr()?;

    sig.post_candidate(code, &format!("key:{}", encode_key(&static_key.public_key_bytes())))?;
    sig.post_candidate(code, &format!("udp:{my_addr}"))?;
    sig.post_answer(code, "lowband/1 answer")?;

    // accept() blocks on the socket for msg1 (bounded by the read timeout).
    let session = SecureSession::accept(sock, static_key, code)?;
    Ok(session)
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Poll `f` until it yields `Some`, or `deadline` passes.
fn wait_for<T>(deadline: Instant, what: &'static str, mut f: impl FnMut() -> Option<T>) -> Result<T> {
    loop {
        if let Some(v) = f() {
            return Ok(v);
        }
        if Instant::now() >= deadline {
            return Err(EstablishError::Timeout(what));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Fetch candidates for `code` and return the first one with `prefix` that
/// `parse` accepts.
fn peer_candidate<T>(
    sig: &SignalingClient,
    code: &str,
    prefix: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let info = sig.join(code).ok()?;
    info.candidates
        .iter()
        .filter_map(|c| c.strip_prefix(prefix))
        .find_map(|v| parse(v))
}

fn encode_key(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = hex.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_signaling::{router, AppState};
    use std::sync::mpsc;

    #[test]
    fn key_hex_roundtrip() {
        let key = [0xABu8; 32];
        let hex = encode_key(&key);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_key(&hex), Some(key));
        assert_eq!(decode_key("nothex"), None);
        assert_eq!(decode_key("ab"), None);
    }

    fn spawn_signaling() -> SocketAddr {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, router(AppState::new())).await.unwrap();
            });
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn client(addr: SocketAddr) -> SignalingClient {
        SignalingClient::connect(addr, "127.0.0.1").unwrap().with_timeout(Duration::from_secs(5))
    }

    /// The daemon's own `establish_host` + `establish_join` drive a full
    /// join-code → E2EE channel exchange against the real signaling server
    /// over real UDP — the production path, not a test-only reimplementation.
    #[test]
    fn daemon_host_and_join_establish_encrypted_channel() {
        let sig_addr = spawn_signaling();
        let timeout = Duration::from_secs(10);

        // The host publishes its code via the callback; hand it to the joiner.
        let (code_tx, code_rx) = mpsc::channel();

        let joiner = thread::spawn(move || {
            let code: String = code_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let sig = client(sig_addr);
            let key = StaticKeypair::generate();
            let mut sess = establish_join(&sig, &code, &key, timeout).unwrap();
            let got = sess.recv().unwrap();
            assert_eq!(&got, b"hello from host");
            sess.send(b"hello from joiner").unwrap();
            sess.remote_static_pubkey()
        });

        let sig = client(sig_addr);
        let host_key = StaticKeypair::generate();
        let (_code, mut host_sess) =
            establish_host(&sig, &host_key, timeout, |code| code_tx.send(code.to_string()).unwrap())
                .unwrap();

        host_sess.send(b"hello from host").unwrap();
        let reply = host_sess.recv().unwrap();
        assert_eq!(&reply, b"hello from joiner");

        let joiner_saw_host = joiner.join().unwrap();
        assert_eq!(joiner_saw_host, host_key.public_key_bytes());
        assert!(host_sess.is_initiator());
    }
}

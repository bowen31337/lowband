//! Capstone: the complete peer-connection path the PRD eval found missing —
//! a 9-digit join code turning into an end-to-end-encrypted media channel,
//! with the real signaling server and real UDP sockets, no in-process shims.
//!
//! Chain under test (FR-1 + NFR-6 together):
//!
//! 1. Technician creates a session code on the real signaling server.
//! 2. Both peers bind UDP sockets and publish their address as an ICE
//!    candidate through signaling; each reads the other's via the API.
//! 3. They run Noise-IK across those sockets (session_code binds the
//!    handshake) and derive per-direction ChaCha20-Poly1305 ciphers.
//! 4. They exchange encrypted application datagrams.
//! 5. Either peer marks the session connected; signaling evicts it and
//!    leaves the media path.

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lowband_crypto::{SecureSession, StaticKeypair};
use lowband_signaling::{router, AppState, SignalingClient};

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
    SignalingClient::connect(addr, "127.0.0.1")
        .unwrap()
        .with_timeout(Duration::from_secs(5))
}

/// Read the peer's UDP address from the candidates published for `code`,
/// retrying briefly since the two peers race.
fn await_peer_candidate(c: &SignalingClient, code: &str, own: SocketAddr) -> SocketAddr {
    for _ in 0..50 {
        let info = c.join(code).unwrap();
        if let Some(cand) = info
            .candidates
            .iter()
            .filter_map(|s| s.strip_prefix("udp:"))
            .filter_map(|s| s.parse::<SocketAddr>().ok())
            .find(|a| *a != own)
        {
            return cand;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("peer candidate never appeared");
}

#[test]
fn join_code_to_encrypted_channel_end_to_end() {
    let sig_addr = spawn_signaling();

    // Responder's static key is shared with the initiator out of band
    // (alongside the code); model that with a channel back to the main thread.
    let (respkey_tx, respkey_rx) = mpsc::channel();

    // Technician (initiator) creates the session; the code travels out of band.
    let tech = client(sig_addr);
    let code = tech.create_session().unwrap();
    let code_for_assisted = code.clone();

    // ── Assisted peer (responder) thread ─────────────────────────────────
    let assisted = thread::spawn(move || {
        let sig = client(sig_addr);
        let code = code_for_assisted;

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let my_addr = sock.local_addr().unwrap();

        let static_key = StaticKeypair::generate();
        respkey_tx.send(static_key.public_key_bytes()).unwrap();

        // Publish our transport address and post the answer.
        sig.post_candidate(&code, &format!("udp:{my_addr}")).unwrap();
        sig.post_answer(&code, "v=0\r\no=assisted").unwrap();

        // Complete the handshake as responder, then converse.
        let mut session = SecureSession::accept(sock, &static_key, &code).unwrap();
        let got = session.recv().unwrap();
        assert_eq!(&got, b"config pushed: dns=1.1.1.1");
        session.send(b"applied, thanks").unwrap();
        session.remote_static_pubkey()
    });

    // ── Technician (initiator) main thread ───────────────────────────────
    let resp_pub = respkey_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let my_addr = sock.local_addr().unwrap();
    let init_static = StaticKeypair::generate();

    tech.post_offer(&code, "v=0\r\no=tech").unwrap();
    tech.post_candidate(&code, &format!("udp:{my_addr}")).unwrap();

    // Wait for the assisted peer's answer + candidate through signaling.
    let peer_addr = await_peer_candidate(&tech, &code, my_addr);
    for _ in 0..50 {
        if tech.poll_answer(&code).unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(tech.poll_answer(&code).unwrap().as_deref(), Some("v=0\r\no=assisted"));

    // Noise-IK over the real UDP path, then send encrypted app data.
    let mut session =
        SecureSession::connect(sock, peer_addr, &init_static, resp_pub, &code).unwrap();
    session.send(b"config pushed: dns=1.1.1.1").unwrap();
    let reply = session.recv().unwrap();
    assert_eq!(&reply, b"applied, thanks");

    // Direct connection established — signaling leaves the path.
    tech.mark_connected(&code).unwrap();

    let recovered_init_pub = assisted.join().unwrap();
    assert_eq!(recovered_init_pub, init_static.public_key_bytes());
    assert_eq!(session.remote_static_pubkey(), resp_pub);
    assert!(session.is_initiator());
}

//! End-to-end signaling: the real axum server on a loopback TCP port driven
//! by the blocking [`SignalingClient`] over actual sockets (not `oneshot`).
//!
//! This exercises FR-1 from the peer's side — the code path that was missing
//! per the PRD eval: create code → offer/candidate → join → answer → poll →
//! connected.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lowband_signaling::{router, AppState, ClientError, SignalingClient};

/// Spawn the real server on 127.0.0.1:0 and return its bound address.
fn spawn_server() -> std::net::SocketAddr {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, router(AppState::new())).await.unwrap();
        });
    });
    rx.recv_timeout(Duration::from_secs(5)).expect("server bind")
}

fn client(addr: std::net::SocketAddr) -> SignalingClient {
    SignalingClient::connect(addr, "127.0.0.1")
        .unwrap()
        .with_timeout(Duration::from_secs(5))
}

#[test]
fn full_rendezvous_over_real_sockets() {
    let addr = spawn_server();
    let tech = client(addr);
    let assisted = client(addr);

    // Technician creates the session and publishes the offer + a candidate.
    let code = tech.create_session().expect("create session");
    assert_eq!(code.len(), 9, "9-digit join code");
    tech.post_offer(&code, "v=0\r\no=tech").expect("post offer");
    tech.post_candidate(&code, "candidate:tech-udp").expect("post candidate");

    // Before the answer exists, polling returns None.
    assert_eq!(tech.poll_answer(&code).expect("poll"), None);

    // Assisted user joins with the code and sees the offer + candidate.
    let info = assisted.join(&code).expect("join");
    assert_eq!(info.offer.as_deref(), Some("v=0\r\no=tech"));
    assert_eq!(info.candidates, vec!["candidate:tech-udp"]);

    // Assisted user answers.
    assisted.post_answer(&code, "v=0\r\no=assisted").expect("post answer");
    assisted.post_candidate(&code, "candidate:assisted-udp").expect("post candidate");

    // Technician now polls the answer successfully.
    assert_eq!(
        tech.poll_answer(&code).expect("poll answer").as_deref(),
        Some("v=0\r\no=assisted")
    );

    // A TURN credential is available to either peer.
    let turn = assisted.turn_credential().expect("turn");
    assert!(!turn.urls.is_empty());
    assert!(!turn.credential.is_empty());
    assert_eq!(turn.ttl_secs, 86_400);

    // On direct connect, the session is evicted and further use 404s.
    tech.mark_connected(&code).expect("mark connected");
    assert!(matches!(assisted.join(&code), Err(ClientError::Status(404))));
}

#[test]
fn unknown_code_is_rejected() {
    let addr = spawn_server();
    let c = client(addr);
    assert!(matches!(c.join("999999999"), Err(ClientError::Status(404))));
    assert!(matches!(c.poll_answer("999999999"), Err(ClientError::Status(404))));
}

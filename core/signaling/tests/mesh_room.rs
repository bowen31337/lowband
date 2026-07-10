//! Mesh group rendezvous (FR-14) driven by the real server over loopback:
//! four participants join a room, publish keys + candidates, and each reads a
//! full roster of the other three — the connectivity foundation for a mesh
//! group call. A fifth join is rejected (cap = 4).

use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lowband_signaling::{router, AppState, ClientError, SignalingClient, MESH_MAX_PARTICIPANTS};

fn spawn_server() -> SocketAddr {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
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

#[test]
fn four_participants_form_a_roster() {
    let addr = spawn_server();
    let host = client(addr);
    let code = host.create_room().unwrap();

    // Four participants join, each publishing a distinct key + candidate.
    let ids = ["alice", "bob", "carol", "dave"];
    for (i, id) in ids.iter().enumerate() {
        let c = client(addr);
        let pubkey = format!("{:064x}", i + 1);
        c.join_room(&code, id, &pubkey).unwrap();
        c.post_room_candidate(&code, id, &format!("udp:127.0.0.1:{}", 5000 + i)).unwrap();
    }

    // Every participant sees all four in the roster, with keys and candidates.
    let roster = host.room_roster(&code).unwrap();
    assert_eq!(roster.participants.len(), MESH_MAX_PARTICIPANTS);
    for (i, id) in ids.iter().enumerate() {
        let p = roster.participants.iter().find(|p| p.id == *id).expect("participant present");
        assert_eq!(p.pubkey, format!("{:064x}", i + 1));
        assert_eq!(p.candidates, vec![format!("udp:127.0.0.1:{}", 5000 + i)]);
    }

    // From alice's view, peers() yields the other three.
    let peer_ids: Vec<&str> = roster.peers("alice").map(|p| p.id.as_str()).collect();
    assert_eq!(peer_ids.len(), 3);
    assert!(!peer_ids.contains(&"alice"));
}

#[test]
fn fifth_participant_is_rejected() {
    let addr = spawn_server();
    let host = client(addr);
    let code = host.create_room().unwrap();

    for i in 0..MESH_MAX_PARTICIPANTS {
        client(addr).join_room(&code, &format!("p{i}"), &format!("{:064x}", i)).unwrap();
    }
    // The room is full; the fifth distinct participant gets 409.
    let fifth = client(addr).join_room(&code, "overflow", &format!("{:064x}", 9));
    assert!(matches!(fifth, Err(ClientError::Status(409))), "got {fifth:?}");

    // But an existing participant may re-join (idempotent) without a 409.
    client(addr).join_room(&code, "p0", &format!("{:064x}", 0)).unwrap();
}

#[test]
fn unknown_room_and_unregistered_candidate_rejected() {
    let addr = spawn_server();
    let c = client(addr);
    assert!(matches!(c.room_roster("999999999"), Err(ClientError::Status(404))));
    assert!(matches!(
        c.join_room("999999999", "x", &format!("{:064x}", 1)),
        Err(ClientError::Status(404))
    ));

    // Candidate from a non-member is forbidden.
    let code = c.create_room().unwrap();
    assert!(matches!(
        c.post_room_candidate(&code, "ghost", "udp:127.0.0.1:1"),
        Err(ClientError::Status(403))
    ));
}

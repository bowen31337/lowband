//! Mesh group calls (FR-14, v1.2/M5): the daemon-side fan-out that turns a
//! signaling **room roster** into a full mesh of pairwise E2EE sessions.
//!
//! The 1:1 path ([`crate::session`]) is two-party. A group call of up to four
//! is a full mesh: every participant runs an independent Noise-IK
//! [`SecureSession`] to every *other* participant, so no media touches the
//! server. This module is the missing integration the v1.2 eval flagged — it
//! consumes [`SignalingClient::room_roster`] and establishes that mesh over
//! real UDP.
//!
//! # How the mesh forms
//!
//! Each node joins the room (publishing its static public key), waits for the
//! roster to reach the group size, then — for every peer — binds a **dedicated
//! socket** and publishes a candidate *tagged for that peer*
//! (`mesh:<peer_id>:udp:<addr>`). A pair `(x, y)` needs one deterministic
//! initiator: the lexicographically smaller id runs `connect`, the larger runs
//! `accept`. All pairwise handshakes run **concurrently** (a scoped thread per
//! peer) so no node blocks in `accept()` waiting on another node that is itself
//! still in `accept()`.
//!
//! Voice across the mesh is send-once-per-peer on the uplink (each peer's
//! budget is the session budget divided by peer count, [`per_peer_budget`]) and
//! **mixed** on the downlink ([`mix`] sums the decoded streams for playout).

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use lowband_crypto::{SecureSession, StaticKeypair};
use lowband_signaling::{RoomParticipant, SignalingClient};

use crate::session::EstablishError;

type Result<T> = std::result::Result<T, EstablishError>;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One connected peer in the mesh: its participant id and the live E2EE
/// session this node holds to it.
pub struct MeshPeer {
    pub id: String,
    pub session: SecureSession,
}

/// This node's view of an established mesh: its own id and a session to every
/// other participant.
pub struct MeshSession {
    pub me: String,
    pub peers: Vec<MeshPeer>,
}

/// Establish the full mesh for `me` in room `code`, blocking until a session to
/// every one of the `group_size - 1` peers is up (or `timeout` elapses).
///
/// `me` is this node's participant id (unique within the room); `static_key` is
/// its long-term identity. Returns a [`MeshSession`] holding one live
/// [`SecureSession`] per peer.
pub fn establish_mesh(
    sig: &SignalingClient,
    code: &str,
    me: &str,
    static_key: &StaticKeypair,
    group_size: usize,
    timeout: Duration,
) -> Result<MeshSession> {
    let my_pub_hex = encode_key(&static_key.public_key_bytes());
    sig.join_room(code, me, &my_pub_hex)?;

    let deadline = Instant::now() + timeout;

    // Phase 1 — wait for the whole party to have joined.
    let roster = wait_for(deadline, "full room roster", || {
        let r = sig.room_roster(code).ok()?;
        (r.participants.len() >= group_size).then_some(r)
    })?;

    let mut peers: Vec<RoomParticipant> = roster.peers(me).cloned().collect();
    peers.sort_by(|a, b| a.id.cmp(&b.id));

    // Phase 2 — one socket per peer; publish an address tagged *for that peer*
    // so the peer knows which of our sockets is its pair's endpoint.
    let mut sockets = Vec::with_capacity(peers.len());
    for p in &peers {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_read_timeout(Some(timeout))?;
        let addr = sock.local_addr()?;
        sig.post_room_candidate(code, me, &format!("mesh:{}:udp:{addr}", p.id))?;
        sockets.push(sock);
    }

    // Phase 3a — resolve each peer's socket-for-me address and static key.
    // (Done outside the handshake scope: it borrows `sig` and polls signaling.)
    let want = format!("mesh:{me}:udp:");
    let mut pairs: Vec<(RoomParticipant, UdpSocket, SocketAddr, [u8; 32])> = Vec::new();
    for (p, sock) in peers.into_iter().zip(sockets) {
        let peer_pub =
            decode_key(&p.pubkey).ok_or(EstablishError::Timeout("peer static key (malformed)"))?;
        let pid = p.id.clone();
        let peer_addr = wait_for(deadline, "peer mesh candidate", || {
            let r = sig.room_roster(code).ok()?;
            let pp = r.participants.iter().find(|x| x.id == pid)?;
            pp.candidates
                .iter()
                .filter_map(|c| c.strip_prefix(&want))
                .find_map(|a| a.parse::<SocketAddr>().ok())
        })?;
        pairs.push((p, sock, peer_addr, peer_pub));
    }

    // Phase 3b — run every pairwise handshake concurrently. Scoped threads
    // borrow `static_key`/`code` directly, so no key clone or 'static bound.
    let peers: Result<Vec<MeshPeer>> = thread::scope(|s| {
        let handles: Vec<_> = pairs
            .into_iter()
            .map(|(p, sock, peer_addr, peer_pub)| {
                let initiator = me < p.id.as_str();
                s.spawn(move || -> Result<MeshPeer> {
                    let session = if initiator {
                        SecureSession::connect(sock, peer_addr, static_key, peer_pub, code)?
                    } else {
                        SecureSession::accept(sock, static_key, code)?
                    };
                    Ok(MeshPeer { id: p.id, session })
                })
            })
            .collect();

        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.join().map_err(|_| EstablishError::Timeout("handshake thread panicked"))??);
        }
        Ok(out)
    });

    let mut peers = peers?;
    peers.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(MeshSession { me: me.to_string(), peers })
}

/// Per-peer uplink budget: the session's total bitrate split evenly across the
/// mesh peers (each peer is a separate encode + send). With no peers the whole
/// budget is available (degenerate 1:1 / idle case).
pub fn per_peer_budget(total_bps: u32, peers: usize) -> u32 {
    if peers == 0 {
        total_bps
    } else {
        total_bps / peers as u32
    }
}

/// Conference downlink mix: sum decoded PCM from every peer into one playout
/// stream, saturating at the i16 rails so a loud overlap clips instead of
/// wrapping. Streams of unequal length are mixed up to the longest (missing
/// tail samples count as silence). Consumed by the mesh voice loop under
/// `--features audio`; always tested.
#[cfg_attr(not(feature = "audio"), allow(dead_code))]
pub fn mix(streams: &[&[i16]]) -> Vec<i16> {
    let len = streams.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut out = vec![0i16; len];
    for s in streams {
        for (o, &x) in out.iter_mut().zip(s.iter()) {
            *o = (*o as i32 + x as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
    }
    out
}

// ── helpers ─────────────────────────────────────────────────────────────────

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
    use tokio::net::TcpListener;

    fn spawn_signaling() -> SocketAddr {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
    fn per_peer_budget_divides_evenly() {
        assert_eq!(per_peer_budget(400_000, 3), 133_333);
        assert_eq!(per_peer_budget(120_000, 2), 60_000);
        assert_eq!(per_peer_budget(64_000, 0), 64_000); // no peers → full budget
    }

    #[test]
    fn mix_sums_and_saturates() {
        // Two half-scale streams sum to full scale, no clip.
        let a = vec![10_000i16, -10_000, 0];
        let b = vec![10_000i16, -10_000, 5];
        assert_eq!(mix(&[&a, &b]), vec![20_000, -20_000, 5]);

        // Overlap that would overflow i16 saturates at the rails.
        let hi = vec![30_000i16, -30_000];
        let hi2 = vec![30_000i16, -30_000];
        assert_eq!(mix(&[&hi, &hi2]), vec![i16::MAX, i16::MIN]);

        // Unequal lengths: the longer tail survives.
        let short = vec![1i16];
        let long = vec![1i16, 2, 3];
        assert_eq!(mix(&[&short, &long]), vec![2, 2, 3]);
        assert_eq!(mix(&[]), Vec::<i16>::new());
    }

    /// Four participants form a full mesh over the real signaling server and
    /// real UDP: every node ends with a live E2EE session to each of the other
    /// three, the topology is correct (each node sees exactly the other three
    /// static keys), and encrypted app data flows on every one of the six
    /// pairwise channels.
    #[test]
    fn four_peers_form_a_full_mesh_and_exchange_data() {
        let sig_addr = spawn_signaling();
        let code = client(sig_addr).create_room().unwrap();
        let group = 4;
        let ids = ["alice", "bob", "carol", "dave"];

        // Each participant runs on its own thread; collect (id, its pubkey, and
        // the set of peer pubkeys it saw) back on the main thread.
        let mut handles = Vec::new();
        for id in ids {
            let sig_addr = sig_addr;
            let code = code.clone();
            handles.push(thread::spawn(move || {
                let sig = client(sig_addr);
                let key = StaticKeypair::generate();
                let my_pub = key.public_key_bytes();
                let mut mesh =
                    establish_mesh(&sig, &code, id, &key, group, Duration::from_secs(20)).unwrap();

                // Send one datagram to every peer first (fire-and-forget, own
                // socket per pair), then read one from every peer — so no pair
                // deadlocks on ordering.
                for p in &mut mesh.peers {
                    p.session.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    p.session.send(format!("hello from {id}").as_bytes()).unwrap();
                }
                let mut got = 0;
                for p in &mut mesh.peers {
                    let msg = p.session.recv().unwrap();
                    assert!(msg.starts_with(b"hello from "));
                    got += 1;
                }

                let mut peer_keys: Vec<[u8; 32]> =
                    mesh.peers.iter().map(|p| p.session.remote_static_pubkey()).collect();
                peer_keys.sort();
                (id, my_pub, mesh.peers.len(), got, peer_keys)
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.join().unwrap());
        }

        // Every node connected to exactly the other three and exchanged data on
        // all three channels.
        for (id, _pub, n_peers, n_got, _keys) in &results {
            assert_eq!(*n_peers, group - 1, "{id} peer count");
            assert_eq!(*n_got, group - 1, "{id} received-from-peers count");
        }

        // Topology check: the set of peer keys each node saw equals the set of
        // the *other* nodes' own keys — a genuine full mesh, no missing edges.
        let all_keys: std::collections::BTreeSet<[u8; 32]> =
            results.iter().map(|(_, k, _, _, _)| *k).collect();
        for (id, my_pub, _, _, peer_keys) in &results {
            let mut expected: Vec<[u8; 32]> =
                all_keys.iter().copied().filter(|k| k != my_pub).collect();
            expected.sort();
            assert_eq!(peer_keys, &expected, "{id} did not mesh with exactly the other three");
        }
    }
}

//! Minimal STUN client for server-reflexive candidate gathering (RFC 5389).
//!
//! NAT traversal needs each peer to learn its *public* transport address —
//! the one a STUN server sees after the NAT rewrites the source. The eval
//! noted candidates were exchanged through signaling but never *gathered*
//! from STUN, so a peer behind NAT only ever published its private address.
//! This sends a Binding Request and parses the XOR-MAPPED-ADDRESS from the
//! response, yielding the reflexive candidate to publish alongside the local
//! one.
//!
//! Scope: a single Binding Request/Response over UDP (no ICE connectivity
//! checks, no TURN allocate — those live above this). Just enough to turn a
//! socket + STUN server into a public `SocketAddr`.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER_LEN: usize = 20;

/// A 96-bit STUN transaction id.
type TxId = [u8; 12];

/// Send a Binding Request from `sock` to `stun_server` and return the
/// server-reflexive address it reports, or `None` if the response is
/// malformed or lacks a (XOR-)MAPPED-ADDRESS.
///
/// `sock` must already be bound; the reflexive address corresponds to *this*
/// socket, so publish it as a candidate for the same socket used for media.
pub fn gather_reflexive(
    sock: &UdpSocket,
    stun_server: SocketAddr,
    timeout: Duration,
) -> io::Result<Option<SocketAddr>> {
    let tx = new_tx_id();
    sock.set_read_timeout(Some(timeout))?;
    sock.send_to(&encode_binding_request(&tx), stun_server)?;

    let mut buf = [0u8; 512];
    let (n, _from) = sock.recv_from(&mut buf)?;
    Ok(parse_binding_response(&buf[..n], &tx))
}

/// Build a 20-byte Binding Request with no attributes.
fn encode_binding_request(tx: &TxId) -> [u8; HEADER_LEN] {
    let mut msg = [0u8; HEADER_LEN];
    msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length
    msg[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg[8..20].copy_from_slice(tx);
    msg
}

/// Parse a Binding Success and extract the reflexive address. Accepts either
/// XOR-MAPPED-ADDRESS (preferred) or legacy MAPPED-ADDRESS. Returns `None`
/// unless the message is a success response matching `tx`.
fn parse_binding_response(msg: &[u8], tx: &TxId) -> Option<SocketAddr> {
    if msg.len() < HEADER_LEN {
        return None;
    }
    let msg_type = u16::from_be_bytes([msg[0], msg[1]]);
    if msg_type != BINDING_SUCCESS {
        return None;
    }
    let length = u16::from_be_bytes([msg[2], msg[3]]) as usize;
    if u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]) != MAGIC_COOKIE {
        return None;
    }
    if &msg[8..20] != tx {
        return None; // response to a different transaction
    }
    if HEADER_LEN + length > msg.len() {
        return None;
    }

    // Walk TLV attributes (4-byte aligned).
    let mut off = HEADER_LEN;
    let end = HEADER_LEN + length;
    let mut fallback = None;
    while off + 4 <= end {
        let attr_type = u16::from_be_bytes([msg[off], msg[off + 1]]);
        let attr_len = u16::from_be_bytes([msg[off + 2], msg[off + 3]]) as usize;
        let val_start = off + 4;
        let val_end = val_start + attr_len;
        if val_end > end {
            break;
        }
        let value = &msg[val_start..val_end];
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(a) = parse_addr(value, true) {
                    return Some(a); // prefer XOR form
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if fallback.is_none() {
                    fallback = parse_addr(value, false);
                }
            }
            _ => {}
        }
        // Advance with 4-byte padding.
        off = val_start + attr_len.div_ceil(4) * 4;
    }
    fallback
}

/// Parse a (XOR-)MAPPED-ADDRESS attribute value. IPv4 only.
fn parse_addr(value: &[u8], xored: bool) -> Option<SocketAddr> {
    // [reserved 1][family 1][port 2][address 4]  — 8 bytes for IPv4.
    if value.len() < 8 || value[1] != 0x01 {
        return None; // need IPv4 (family 0x01)
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let mut octets = [value[4], value[5], value[6], value[7]];
    if xored {
        port ^= (MAGIC_COOKIE >> 16) as u16;
        let cookie = MAGIC_COOKIE.to_be_bytes();
        for (o, c) in octets.iter_mut().zip(cookie.iter()) {
            *o ^= c;
        }
    }
    Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(octets), port)))
}

/// Generate a transaction id. Uniqueness (not unpredictability) is what the
/// client needs here to match a response to its request.
fn new_tx_id() -> TxId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut id = [0u8; 12];
    id.copy_from_slice(&nanos.to_le_bytes()[..12]);
    id
}

/// Build a Binding Success echoing `reflexive` as XOR-MAPPED-ADDRESS.
/// Test-only helper, shared with the session module's integration test.
#[cfg(test)]
pub(crate) fn encode_success_xor(tx: &[u8; 12], reflexive: SocketAddrV4) -> Vec<u8> {
    let xport = reflexive.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let mut xaddr = reflexive.ip().octets();
    for (o, c) in xaddr.iter_mut().zip(cookie.iter()) {
        *o ^= c;
    }
    let mut attr = Vec::new();
    attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    attr.extend_from_slice(&8u16.to_be_bytes());
    attr.extend_from_slice(&[0x00, 0x01]); // reserved, family IPv4
    attr.extend_from_slice(&xport.to_be_bytes());
    attr.extend_from_slice(&xaddr);

    let mut msg = Vec::new();
    msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    msg.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg.extend_from_slice(tx);
    msg.extend_from_slice(&attr);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn request_is_well_formed() {
        let tx = [7u8; 12];
        let req = encode_binding_request(&tx);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), MAGIC_COOKIE);
        assert_eq!(&req[8..20], &tx);
    }

    #[test]
    fn parses_xor_mapped_address() {
        let tx = [3u8; 12];
        let reflexive = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 5), 51234);
        let msg = encode_success_xor(&tx, reflexive);
        assert_eq!(parse_binding_response(&msg, &tx), Some(SocketAddr::V4(reflexive)));
    }

    #[test]
    fn rejects_wrong_transaction_or_type() {
        let tx = [1u8; 12];
        let other = [2u8; 12];
        let reflexive = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 9), 3478);
        let msg = encode_success_xor(&tx, reflexive);
        assert_eq!(parse_binding_response(&msg, &other), None, "tx mismatch rejected");

        let mut not_success = msg.clone();
        not_success[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
        assert_eq!(parse_binding_response(&not_success, &tx), None, "non-success rejected");
    }

    #[test]
    fn gather_reflexive_round_trips_against_mock_server() {
        // Mock STUN server: replies to one Binding Request with the client's
        // real source address as XOR-MAPPED-ADDRESS.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            // Echo the client's source addr (must be IPv4 on loopback).
            let from_v4 = match from {
                SocketAddr::V4(v4) => v4,
                _ => panic!("expected ipv4"),
            };
            let tx: TxId = buf[8..20].try_into().unwrap();
            assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), BINDING_REQUEST);
            let resp = encode_success_xor(&tx, from_v4);
            server.send_to(&resp, from).unwrap();
            let _ = n;
        });

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client.local_addr().unwrap();
        let reflexive =
            gather_reflexive(&client, server_addr, Duration::from_secs(3)).unwrap();
        // On loopback the "reflexive" address equals the client's own address.
        assert_eq!(reflexive, Some(client_addr));
        handle.join().unwrap();
    }
}

//! Feature 20 — X25519 ephemeral key exchange and HKDF session-key derivation.
//!
//! # Protocol
//!
//! Both peers generate an [`EphemeralKeypair`], exchange their public key bytes
//! out-of-band (e.g., via the Noise-IK handshake message), then each calls
//! [`EphemeralKeypair::derive_session_state`] with the peer's public key bytes.
//! The result is a [`SessionState`] whose [`TrafficKeys`] are consumed directly
//! by [`DatagramCipher`](crate::relay_guard::DatagramCipher).
//!
//! # Key derivation
//!
//! ```text
//! dh_output = X25519(local_secret, peer_public)        — 32 bytes
//! prk       = HKDF-Extract(salt=∅, IKM=dh_output)     — HMAC-SHA-256
//! okm       = HKDF-Expand(prk, info="lowband v1 traffic keys", L=64)
//!
//! traffic_keys.initiator_to_responder = okm[ 0..32]
//! traffic_keys.responder_to_initiator = okm[32..64]
//! ```
//!
//! The **initiator** role is assigned to whichever peer has the
//! lexicographically smaller ephemeral public key.  Both peers reach the same
//! assignment without additional coordination.
//!
//! # Example
//!
//! ```rust
//! use lowband_crypto::key_exchange::EphemeralKeypair;
//!
//! // Alice generates her keypair and shares her public key bytes.
//! let alice = EphemeralKeypair::generate();
//! let alice_pub = alice.public_key_bytes();
//!
//! // Bob generates his keypair and shares his public key bytes.
//! let bob = EphemeralKeypair::generate();
//! let bob_pub = bob.public_key_bytes();
//!
//! // Both derive the same session state independently.
//! let alice_state = alice.derive_session_state(bob_pub);
//! let bob_state   = bob.derive_session_state(alice_pub);
//!
//! assert_eq!(
//!     alice_state.traffic_keys.initiator_to_responder,
//!     bob_state.traffic_keys.initiator_to_responder,
//! );
//! assert_eq!(
//!     alice_state.traffic_keys.responder_to_initiator,
//!     bob_state.traffic_keys.responder_to_initiator,
//! );
//! ```

use hkdf::Hkdf;
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

const HKDF_INFO: &[u8] = b"lowband v1 traffic keys";
const KEY_LEN: usize = 32;

/// The two 32-byte ChaCha20-Poly1305 traffic keys derived from the X25519
/// shared secret via HKDF-SHA-256 (Feature 20).
///
/// Pass each key to [`DatagramCipher::new`](crate::relay_guard::DatagramCipher::new)
/// to encrypt datagrams in the corresponding direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficKeys {
    /// 32-byte key for initiator → responder datagrams.
    pub initiator_to_responder: [u8; KEY_LEN],
    /// 32-byte key for responder → initiator datagrams.
    pub responder_to_initiator: [u8; KEY_LEN],
}

impl TrafficKeys {
    /// The key this peer should use to **encrypt** outbound datagrams.
    ///
    /// `is_initiator` comes from [`SessionState::is_initiator`].
    pub fn send_key(&self, is_initiator: bool) -> &[u8; KEY_LEN] {
        if is_initiator { &self.initiator_to_responder } else { &self.responder_to_initiator }
    }

    /// The key this peer should use to **decrypt** inbound datagrams.
    ///
    /// `is_initiator` comes from [`SessionState::is_initiator`].
    pub fn recv_key(&self, is_initiator: bool) -> &[u8; KEY_LEN] {
        if is_initiator { &self.responder_to_initiator } else { &self.initiator_to_responder }
    }
}

/// Session state produced by a completed X25519 key exchange.
///
/// Both peers independently compute the same [`TrafficKeys`] from their
/// respective ephemeral secrets and each other's public key bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// HKDF-derived traffic keys ready for use with
    /// [`DatagramCipher`](crate::relay_guard::DatagramCipher).
    pub traffic_keys: TrafficKeys,
    /// The local ephemeral public key used in this exchange (32 bytes).
    pub local_public_key: [u8; KEY_LEN],
    /// The remote peer's ephemeral public key (32 bytes).
    pub peer_public_key: [u8; KEY_LEN],
    /// `true` when the local public key is lexicographically smaller than the
    /// peer's, meaning this peer acts as the **initiator**.
    pub is_initiator: bool,
}

/// An ephemeral X25519 keypair for a single session handshake.
///
/// The secret is consumed by [`derive_session_state`](Self::derive_session_state)
/// and cannot be reused, enforcing forward secrecy at the type level.
pub struct EphemeralKeypair {
    secret: EphemeralSecret,
    public: PublicKey,
}

impl EphemeralKeypair {
    /// Generate a fresh ephemeral keypair using the OS random-number generator.
    pub fn generate() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// The 32-byte ephemeral public key to share with the remote peer.
    pub fn public_key_bytes(&self) -> [u8; KEY_LEN] {
        *self.public.as_bytes()
    }

    /// Consume this keypair and derive a [`SessionState`] by performing X25519
    /// DH with `peer_public_key_bytes`, then expanding the shared secret via
    /// HKDF-SHA-256 into two 32-byte [`TrafficKeys`].
    ///
    /// `peer_public_key_bytes` must be the 32-byte value received from the
    /// remote peer during the handshake.
    pub fn derive_session_state(self, peer_public_key_bytes: [u8; KEY_LEN]) -> SessionState {
        let local_pub = *self.public.as_bytes();
        let peer_pub  = peer_public_key_bytes;

        let dh_output = self.secret.diffie_hellman(&PublicKey::from(peer_pub));

        let hk = Hkdf::<Sha256>::new(None, dh_output.as_bytes());
        let mut okm = [0u8; KEY_LEN * 2];
        hk.expand(HKDF_INFO, &mut okm)
            .expect("HKDF expand into 64 bytes is always valid for HMAC-SHA-256");

        let mut i2r = [0u8; KEY_LEN];
        let mut r2i = [0u8; KEY_LEN];
        i2r.copy_from_slice(&okm[..KEY_LEN]);
        r2i.copy_from_slice(&okm[KEY_LEN..]);

        SessionState {
            traffic_keys: TrafficKeys {
                initiator_to_responder: i2r,
                responder_to_initiator: r2i,
            },
            local_public_key: local_pub,
            peer_public_key:  peer_pub,
            is_initiator:     local_pub < peer_pub,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_guard::DatagramCipher;

    fn exchange() -> (SessionState, SessionState) {
        let alice = EphemeralKeypair::generate();
        let bob   = EphemeralKeypair::generate();
        let alice_pub = alice.public_key_bytes();
        let bob_pub   = bob.public_key_bytes();
        let a = alice.derive_session_state(bob_pub);
        let b = bob.derive_session_state(alice_pub);
        (a, b)
    }

    // ── Both peers derive the same traffic keys ───────────────────────────────

    #[test]
    fn both_peers_derive_same_initiator_to_responder_key() {
        let (a, b) = exchange();
        assert_eq!(
            a.traffic_keys.initiator_to_responder,
            b.traffic_keys.initiator_to_responder,
            "both peers must derive the identical i2r traffic key"
        );
    }

    #[test]
    fn both_peers_derive_same_responder_to_initiator_key() {
        let (a, b) = exchange();
        assert_eq!(
            a.traffic_keys.responder_to_initiator,
            b.traffic_keys.responder_to_initiator,
            "both peers must derive the identical r2i traffic key"
        );
    }

    // ── Exactly one peer is the initiator ────────────────────────────────────

    #[test]
    fn exactly_one_peer_is_initiator() {
        let (a, b) = exchange();
        assert_ne!(
            a.is_initiator, b.is_initiator,
            "the initiator role must be assigned to exactly one peer"
        );
    }

    // ── Public keys are echoed into session state ─────────────────────────────

    #[test]
    fn local_and_peer_keys_are_swapped_between_sides() {
        let alice = EphemeralKeypair::generate();
        let bob   = EphemeralKeypair::generate();
        let alice_pub = alice.public_key_bytes();
        let bob_pub   = bob.public_key_bytes();
        let a = alice.derive_session_state(bob_pub);
        let b = bob.derive_session_state(alice_pub);
        assert_eq!(a.local_public_key, b.peer_public_key);
        assert_eq!(b.local_public_key, a.peer_public_key);
    }

    // ── Two independent exchanges produce different traffic keys ─────────────

    #[test]
    fn independent_exchanges_produce_different_traffic_keys() {
        let (a1, _) = exchange();
        let (a2, _) = exchange();
        assert_ne!(
            a1.traffic_keys.initiator_to_responder,
            a2.traffic_keys.initiator_to_responder,
            "distinct key pairs must produce distinct traffic keys"
        );
    }

    // ── The two directional keys within a session differ ─────────────────────

    #[test]
    fn i2r_and_r2i_keys_are_distinct() {
        let (a, _) = exchange();
        assert_ne!(
            a.traffic_keys.initiator_to_responder,
            a.traffic_keys.responder_to_initiator,
            "the two traffic keys must be distinct — they cover opposite directions"
        );
    }

    // ── Traffic keys are 32 bytes ─────────────────────────────────────────────

    #[test]
    fn traffic_keys_are_32_bytes() {
        let (a, _) = exchange();
        assert_eq!(a.traffic_keys.initiator_to_responder.len(), 32);
        assert_eq!(a.traffic_keys.responder_to_initiator.len(), 32);
    }

    // ── send_key / recv_key helpers agree with the role ───────────────────────

    #[test]
    fn initiator_send_key_is_i2r() {
        let (a, b) = exchange();
        let initiator = if a.is_initiator { &a } else { &b };
        assert_eq!(
            initiator.traffic_keys.send_key(true),
            &initiator.traffic_keys.initiator_to_responder
        );
    }

    #[test]
    fn initiator_recv_key_is_r2i() {
        let (a, b) = exchange();
        let initiator = if a.is_initiator { &a } else { &b };
        assert_eq!(
            initiator.traffic_keys.recv_key(true),
            &initiator.traffic_keys.responder_to_initiator
        );
    }

    #[test]
    fn responder_send_key_is_r2i() {
        let (a, b) = exchange();
        let responder = if !a.is_initiator { &a } else { &b };
        assert_eq!(
            responder.traffic_keys.send_key(false),
            &responder.traffic_keys.responder_to_initiator
        );
    }

    #[test]
    fn responder_recv_key_is_i2r() {
        let (a, b) = exchange();
        let responder = if !a.is_initiator { &a } else { &b };
        assert_eq!(
            responder.traffic_keys.recv_key(false),
            &responder.traffic_keys.initiator_to_responder
        );
    }

    // ── Integration: derived keys feed DatagramCipher for a full round-trip ───

    #[test]
    fn initiator_encrypts_responder_decrypts_via_derived_keys() {
        let alice = EphemeralKeypair::generate();
        let bob   = EphemeralKeypair::generate();
        let alice_pub = alice.public_key_bytes();
        let bob_pub   = bob.public_key_bytes();
        let alice_state = alice.derive_session_state(bob_pub);
        let bob_state   = bob.derive_session_state(alice_pub);

        let alice_send = *alice_state.traffic_keys.send_key(alice_state.is_initiator);
        let bob_recv   = *bob_state.traffic_keys.recv_key(bob_state.is_initiator);

        let mut alice_cipher = DatagramCipher::new(alice_send);
        let bob_cipher       = DatagramCipher::new(bob_recv);

        let plaintext = b"secure media frame";
        let payload   = alice_cipher.seal(plaintext);
        let recovered = bob_cipher.open(&payload)
            .expect("Bob must decrypt Alice's datagram when both use matching derived keys");

        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn responder_encrypts_initiator_decrypts_via_derived_keys() {
        let alice = EphemeralKeypair::generate();
        let bob   = EphemeralKeypair::generate();
        let alice_pub = alice.public_key_bytes();
        let bob_pub   = bob.public_key_bytes();
        let alice_state = alice.derive_session_state(bob_pub);
        let bob_state   = bob.derive_session_state(alice_pub);

        let bob_send     = *bob_state.traffic_keys.send_key(bob_state.is_initiator);
        let alice_recv   = *alice_state.traffic_keys.recv_key(alice_state.is_initiator);

        let mut bob_cipher   = DatagramCipher::new(bob_send);
        let alice_cipher     = DatagramCipher::new(alice_recv);

        let plaintext = b"response media frame";
        let payload   = bob_cipher.seal(plaintext);
        let recovered = alice_cipher.open(&payload)
            .expect("Alice must decrypt Bob's datagram when both use matching derived keys");

        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn mismatched_keys_across_unrelated_exchanges_reject_decryption() {
        let (state_a, _) = exchange();
        let (state_b, _) = exchange();

        let key_from_exchange_a = state_a.traffic_keys.initiator_to_responder;
        let key_from_exchange_b = state_b.traffic_keys.initiator_to_responder;

        let mut encryptor = DatagramCipher::new(key_from_exchange_a);
        let decryptor     = DatagramCipher::new(key_from_exchange_b);

        let payload = encryptor.seal(b"media");
        assert!(
            decryptor.open(&payload).is_none(),
            "a key from a different exchange must not decrypt the ciphertext"
        );
    }

    // ── public_key_bytes is callable before derive_session_state ─────────────

    #[test]
    fn public_key_bytes_consistent_with_what_peer_sees() {
        let alice = EphemeralKeypair::generate();
        let bob   = EphemeralKeypair::generate();
        let alice_pub = alice.public_key_bytes();
        let bob_pub   = bob.public_key_bytes();
        let bob_state = bob.derive_session_state(alice_pub);
        // Bob's SessionState should record alice_pub as the peer key.
        assert_eq!(bob_state.peer_public_key, alice_pub);
        let _ = alice.derive_session_state(bob_pub); // consume alice
    }
}

//! Feature 19 — Noise-IK handshake keyed with `session_code`.
//!
//! Implements `Noise_IK_25519_ChaChaPoly_SHA256`, using the out-of-band
//! `session_code` as the handshake prologue.  Mixing the code into the
//! prologue binds both peers to the same signaling rendezvous channel, so any
//! mismatch is detected as AEAD decryption failure.
//!
//! # Pattern
//!
//! ```text
//! Noise_IK_25519_ChaChaPoly_SHA256
//! <- s
//! ...
//! -> e, es, s, ss
//! <- e, ee, se
//! ```
//!
//! The initiator already holds the responder's static public key, shared
//! out-of-band alongside the `session_code`.
//!
//! # Wire sizes
//!
//! | Message | Breakdown | Total |
//! |---------|-----------|-------|
//! | msg 1 (initiator → responder) | 32 B `e` + 48 B `enc(s)` + 16 B `enc(∅)` | 96 B |
//! | msg 2 (responder → initiator) | 32 B `e` + 16 B `enc(∅)` | 48 B |
//!
//! # Example
//!
//! ```rust
//! use lowband_crypto::noise_ik::{
//!     HandshakeResult, NoiseIkInitiator, NoiseIkResponder, StaticKeypair,
//! };
//!
//! let init_kp = StaticKeypair::generate();
//! let resp_kp = StaticKeypair::generate();
//! let session_code = "123456789";
//!
//! // Initiator creates message 1 knowing the responder's static pubkey.
//! let (initiator, msg1) = NoiseIkInitiator::new(
//!     &init_kp,
//!     resp_kp.public_key_bytes(),
//!     session_code,
//! );
//!
//! // Responder processes message 1 and builds message 2.
//! let responder = NoiseIkResponder::receive_message1(&resp_kp, session_code, &msg1)
//!     .expect("msg1 must verify");
//! let (resp_result, msg2) = responder.send_message2();
//!
//! // Initiator processes message 2 and obtains the final session state.
//! let init_result = initiator.receive_message2(&init_kp, &msg2).expect("msg2 must verify");
//!
//! assert_eq!(
//!     init_result.traffic_keys.initiator_to_responder,
//!     resp_result.traffic_keys.initiator_to_responder,
//! );
//! assert_eq!(init_result.transcript_hash, resp_result.transcript_hash);
//! ```

use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

use crate::key_exchange::TrafficKeys;

/// Protocol identifier per the Noise spec.  Exactly 32 bytes, so it is used
/// directly as the initial handshake hash without a further SHA-256 pass.
const PROTOCOL_NAME: &[u8; 32] = b"Noise_IK_25519_ChaChaPoly_SHA256";

/// Byte length of the first handshake message (initiator → responder).
pub const MSG1_LEN: usize = 96;
/// Byte length of the second handshake message (responder → initiator).
pub const MSG2_LEN: usize = 48;

// ── DH helpers ────────────────────────────────────────────────────────────────

/// Generate 32 random bytes suitable for use as an X25519 private scalar.
/// Clamping is performed by `x25519()` at use time per RFC 7748 §5.
fn gen_private_key() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Derive the X25519 public key from a private scalar.
fn pub_from_priv(priv_key: [u8; 32]) -> [u8; 32] {
    x25519(priv_key, X25519_BASEPOINT_BYTES)
}

/// Perform an X25519 Diffie-Hellman exchange.
fn dh(my_priv: [u8; 32], their_pub: [u8; 32]) -> [u8; 32] {
    x25519(my_priv, their_pub)
}

// ── Long-term identity keypair ─────────────────────────────────────────────────

/// Long-term X25519 static keypair for Noise-IK peer authentication.
///
/// Unlike the ephemeral keypair used in Feature 20, this keypair is retained
/// across sessions so returning peers can be recognised via
/// [`KnownPeerStore`](crate::known_peers::KnownPeerStore).
pub struct StaticKeypair {
    /// Raw private scalar bytes (clamped by `x25519()` at each DH call).
    secret: [u8; 32],
    public: [u8; 32],
}

impl StaticKeypair {
    /// Generate a fresh static keypair from the OS random-number generator.
    pub fn generate() -> Self {
        let secret = gen_private_key();
        let public = pub_from_priv(secret);
        Self { secret, public }
    }

    /// The 32-byte static public key to share with peers out-of-band.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.public
    }

    fn dh(&self, peer_pub: &[u8; 32]) -> [u8; 32] {
        dh(self.secret, *peer_pub)
    }
}

// ── Error ──────────────────────────────────────────────────────────────────────

/// Error returned when a Noise-IK handshake step fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// AEAD authentication tag did not verify — the message was tampered with,
    /// the `session_code` does not match, or the remote static public key is wrong.
    DecryptionFailed,
    /// The handshake message has the wrong byte length.
    BadMessageLength { expected: usize, got: usize },
}

impl core::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DecryptionFailed => f.write_str("Noise-IK decryption failed"),
            Self::BadMessageLength { expected, got } => {
                write!(f, "Noise-IK message length: expected {expected} B, got {got} B")
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

// ── Handshake result ───────────────────────────────────────────────────────────

/// Session keys and peer identity produced by a completed Noise-IK handshake.
///
/// Feed the fields into the rest of the security layer:
/// - `traffic_keys` → [`DatagramCipher`](crate::relay_guard::DatagramCipher)
/// - `transcript_hash` → [`ShortAuthString::derive`](crate::short_auth_string::ShortAuthString::derive)
/// - `remote_static_pubkey` → [`KnownPeerStore::upsert`](crate::known_peers::KnownPeerStore::upsert)
#[derive(Debug)]
pub struct HandshakeResult {
    /// Two 32-byte ChaCha20-Poly1305 keys for the two traffic directions.
    pub traffic_keys: TrafficKeys,
    /// SHA-256 handshake transcript hash, identical on both peers after a
    /// successful handshake.  Feed into `ShortAuthString::derive` for verbal
    /// MITM detection.
    pub transcript_hash: [u8; 32],
    /// Remote peer's X25519 static public key recovered from the handshake.
    /// Store in `KnownPeerStore` after completion.
    pub remote_static_pubkey: [u8; 32],
    /// `true` when this peer acted as the handshake initiator.
    pub is_initiator: bool,
}

// ── Internal symmetric state ───────────────────────────────────────────────────

#[derive(Debug)]
struct SymmetricState {
    /// Handshake hash — evolves with every token and becomes the transcript hash.
    h: [u8; 32],
    /// Chaining key — feeds into HKDF for each new DH-derived key.
    ck: [u8; 32],
    /// Current AEAD key (`None` until the first `mix_key` call).
    k: Option<[u8; 32]>,
    /// Nonce counter — reset to 0 by every `mix_key` call.
    n: u64,
}

impl SymmetricState {
    fn new() -> Self {
        // PROTOCOL_NAME is exactly 32 bytes, so it doubles as both h and ck.
        let h = *PROTOCOL_NAME;
        Self { h, ck: h, k: None, n: 0 }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Sha256::new();
        hasher.update(self.h);
        hasher.update(data);
        self.h = hasher.finalize().into();
    }

    /// HKDF-expand the chaining key with `ikm`, advancing ck and setting k.
    fn mix_key(&mut self, ikm: &[u8]) {
        let hk = Hkdf::<Sha256>::new(Some(&self.ck), ikm);
        let mut okm = [0u8; 64];
        hk.expand(b"", &mut okm).expect("HKDF expand 64 bytes is always valid");
        self.ck.copy_from_slice(&okm[..32]);
        let mut k = [0u8; 32];
        k.copy_from_slice(&okm[32..]);
        self.k = Some(k);
        self.n = 0;
    }

    /// AEAD-encrypt `plaintext` with AAD = current `h`, then mix the ciphertext
    /// into `h`.  If `k` is not yet set the plaintext is passed through unchanged.
    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ciphertext = if let Some(k) = self.k {
            let cipher = ChaCha20Poly1305::new_from_slice(&k)
                .expect("key is always 32 bytes");
            let nonce = nonce_from_counter(self.n);
            self.n += 1;
            cipher
                .encrypt(&nonce, Payload { msg: plaintext, aad: &self.h })
                .expect("ChaCha20-Poly1305 encrypt never fails for a valid key")
        } else {
            plaintext.to_vec()
        };
        self.mix_hash(&ciphertext);
        ciphertext
    }

    /// AEAD-decrypt `ciphertext` with AAD = current `h`, then mix the ciphertext
    /// (not plaintext) into `h`.  Returns `DecryptionFailed` on tag mismatch.
    fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        let plaintext = if let Some(k) = self.k {
            let cipher = ChaCha20Poly1305::new_from_slice(&k)
                .expect("key is always 32 bytes");
            let nonce = nonce_from_counter(self.n);
            self.n += 1;
            cipher
                .decrypt(&nonce, Payload { msg: ciphertext, aad: &self.h })
                .map_err(|_| HandshakeError::DecryptionFailed)?
        } else {
            ciphertext.to_vec()
        };
        // Per the Noise spec, always hash the ciphertext, not the plaintext.
        self.mix_hash(ciphertext);
        Ok(plaintext)
    }

    /// Derive final traffic keys from the chaining key via HKDF.
    fn split(&self) -> ([u8; 32], [u8; 32]) {
        let hk = Hkdf::<Sha256>::new(Some(&self.ck), b"");
        let mut okm = [0u8; 64];
        hk.expand(b"", &mut okm).expect("HKDF expand 64 bytes is always valid");
        let mut k1 = [0u8; 32];
        let mut k2 = [0u8; 32];
        k1.copy_from_slice(&okm[..32]);
        k2.copy_from_slice(&okm[32..]);
        (k1, k2)
    }
}

fn nonce_from_counter(n: u64) -> Nonce {
    // IETF ChaCha20 nonce: 4 zero bytes || little-endian 64-bit counter.
    let mut buf = [0u8; 12];
    buf[4..].copy_from_slice(&n.to_le_bytes());
    Nonce::from(buf)
}

// ── Initiator ─────────────────────────────────────────────────────────────────

/// Initiator half-state, holding the symmetric state between building message 1
/// and processing message 2.
///
/// Construct via [`NoiseIkInitiator::new`], which returns both the initiator
/// state and the serialised first message to send to the responder.
#[derive(Debug)]
pub struct NoiseIkInitiator {
    state: SymmetricState,
    /// Initiator's ephemeral private key — needed for `ee` in message 2.
    e_secret: [u8; 32],
    /// Responder's static public key — carried through to [`HandshakeResult`].
    remote_s_pub: [u8; 32],
}

impl NoiseIkInitiator {
    /// Create the initiator state and build the first handshake message.
    ///
    /// `local_static` is the initiator's long-term static keypair.
    /// `remote_static_pubkey` is the responder's 32-byte static public key,
    /// obtained out-of-band alongside `session_code`.
    ///
    /// Returns `(self, msg1)` where `msg1` is the 96-byte first message to
    /// send to the responder.
    pub fn new(
        local_static: &StaticKeypair,
        remote_static_pubkey: [u8; 32],
        session_code: &str,
    ) -> (Self, Vec<u8>) {
        let mut state = SymmetricState::new();

        // Prologue: binds the handshake to the out-of-band session code.
        state.mix_hash(session_code.as_bytes());
        // Pre-message: initiator knows the responder's static pubkey.
        state.mix_hash(&remote_static_pubkey);

        let e_secret = gen_private_key();
        let e_pub = pub_from_priv(e_secret);

        let mut msg = Vec::with_capacity(MSG1_LEN);

        // Token `e`: send initiator's ephemeral pubkey (cleartext).
        msg.extend_from_slice(&e_pub);
        state.mix_hash(&e_pub);

        // Token `es`: DH(init_e, resp_s) advances the chaining key.
        state.mix_key(&dh(e_secret, remote_static_pubkey));

        // Token `s`: send initiator's static pubkey, encrypted under current key.
        let enc_s = state.encrypt_and_hash(&local_static.public_key_bytes());
        msg.extend_from_slice(&enc_s);

        // Token `ss`: DH(init_s, resp_s) advances the chaining key again.
        state.mix_key(&local_static.dh(&remote_static_pubkey));

        // Empty payload authenticated with the current key.
        let enc_payload = state.encrypt_and_hash(b"");
        msg.extend_from_slice(&enc_payload);

        debug_assert_eq!(msg.len(), MSG1_LEN);
        (Self { state, e_secret, remote_s_pub: remote_static_pubkey }, msg)
    }

    /// Process the responder's second message and complete the handshake.
    ///
    /// `local_static` must be the same keypair passed to [`new`](Self::new).
    /// Returns a [`HandshakeResult`] on success or [`HandshakeError`] if the
    /// AEAD tag does not verify.
    pub fn receive_message2(
        mut self,
        local_static: &StaticKeypair,
        msg2: &[u8],
    ) -> Result<HandshakeResult, HandshakeError> {
        if msg2.len() != MSG2_LEN {
            return Err(HandshakeError::BadMessageLength {
                expected: MSG2_LEN,
                got: msg2.len(),
            });
        }

        let resp_e_pub: [u8; 32] = msg2[..32].try_into().unwrap();

        // Token `e`: receive responder's ephemeral pubkey.
        self.state.mix_hash(&resp_e_pub);

        // Token `ee`: DH(init_e, resp_e).
        self.state.mix_key(&dh(self.e_secret, resp_e_pub));

        // Token `se`: DH(init_s, resp_e) — initiator's static × responder's ephemeral.
        self.state.mix_key(&local_static.dh(&resp_e_pub));

        // Empty payload: verify AEAD tag.
        self.state.decrypt_and_hash(&msg2[32..])?;

        let (k1, k2) = self.state.split();
        Ok(HandshakeResult {
            traffic_keys: TrafficKeys {
                initiator_to_responder: k1,
                responder_to_initiator: k2,
            },
            transcript_hash: self.state.h,
            remote_static_pubkey: self.remote_s_pub,
            is_initiator: true,
        })
    }
}

// ── Responder ─────────────────────────────────────────────────────────────────

/// Responder half-state, held after processing message 1 and before building
/// message 2.
///
/// Construct via [`NoiseIkResponder::receive_message1`], then call
/// [`send_message2`](Self::send_message2) to complete the handshake.
#[derive(Debug)]
pub struct NoiseIkResponder {
    state: SymmetricState,
    /// Initiator's ephemeral public key — used for `ee` and `se` in message 2.
    init_e_pub: [u8; 32],
    /// Initiator's static public key — used for `se` in message 2, and carried
    /// through to [`HandshakeResult`].
    init_s_pub: [u8; 32],
}

impl NoiseIkResponder {
    /// Process the initiator's first handshake message.
    ///
    /// Returns the responder state on success, or [`HandshakeError`] if the
    /// message length is wrong or any AEAD tag fails to verify.
    pub fn receive_message1(
        local_static: &StaticKeypair,
        session_code: &str,
        msg1: &[u8],
    ) -> Result<Self, HandshakeError> {
        if msg1.len() != MSG1_LEN {
            return Err(HandshakeError::BadMessageLength {
                expected: MSG1_LEN,
                got: msg1.len(),
            });
        }

        let mut state = SymmetricState::new();

        // Prologue: must match what the initiator used.
        state.mix_hash(session_code.as_bytes());
        // Pre-message: responder mixes in its own static pubkey.
        state.mix_hash(&local_static.public_key_bytes());

        let init_e_pub: [u8; 32] = msg1[..32].try_into().unwrap();

        // Token `e`: receive initiator's ephemeral pubkey.
        state.mix_hash(&init_e_pub);

        // Token `es`: DH(resp_s, init_e) — responder's static × initiator's ephemeral.
        state.mix_key(&local_static.dh(&init_e_pub));

        // Token `s`: decrypt initiator's static pubkey.
        let init_s_raw = state.decrypt_and_hash(&msg1[32..80])?;
        let init_s_pub: [u8; 32] = init_s_raw
            .try_into()
            .map_err(|_| HandshakeError::DecryptionFailed)?;

        // Token `ss`: DH(resp_s, init_s) — both static keys.
        state.mix_key(&local_static.dh(&init_s_pub));

        // Empty payload: verify AEAD tag.
        state.decrypt_and_hash(&msg1[80..])?;

        Ok(Self { state, init_e_pub, init_s_pub })
    }

    /// Build the second handshake message and complete the handshake.
    ///
    /// Returns `(result, msg2)` where `msg2` is the 48-byte message to send to
    /// the initiator.  The [`HandshakeResult`] is available immediately — the
    /// responder does not need to wait for an acknowledgement.
    pub fn send_message2(mut self) -> (HandshakeResult, Vec<u8>) {
        let e_secret = gen_private_key();
        let e_pub = pub_from_priv(e_secret);

        let mut msg = Vec::with_capacity(MSG2_LEN);

        // Token `e`: send responder's ephemeral pubkey (cleartext).
        msg.extend_from_slice(&e_pub);
        self.state.mix_hash(&e_pub);

        // Token `ee`: DH(resp_e, init_e).
        self.state.mix_key(&dh(e_secret, self.init_e_pub));

        // Token `se`: DH(resp_e, init_s) — responder's ephemeral × initiator's static.
        self.state.mix_key(&dh(e_secret, self.init_s_pub));

        // Empty payload authenticated with the current key.
        let enc_payload = self.state.encrypt_and_hash(b"");
        msg.extend_from_slice(&enc_payload);

        debug_assert_eq!(msg.len(), MSG2_LEN);

        let (k1, k2) = self.state.split();
        let result = HandshakeResult {
            traffic_keys: TrafficKeys {
                initiator_to_responder: k1,
                responder_to_initiator: k2,
            },
            transcript_hash: self.state.h,
            remote_static_pubkey: self.init_s_pub,
            is_initiator: false,
        };
        (result, msg)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        known_peers::KnownPeerStore,
        relay_guard::DatagramCipher,
        short_auth_string::ShortAuthString,
    };

    fn handshake() -> (HandshakeResult, HandshakeResult) {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let code = "123456789";
        let (initiator, msg1) = NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), code);
        let responder = NoiseIkResponder::receive_message1(&resp_kp, code, &msg1).unwrap();
        let (resp_result, msg2) = responder.send_message2();
        let init_result = initiator.receive_message2(&init_kp, &msg2).unwrap();
        (init_result, resp_result)
    }

    // ── Traffic key agreement ─────────────────────────────────────────────────

    #[test]
    fn both_peers_derive_same_i2r_key() {
        let (a, b) = handshake();
        assert_eq!(
            a.traffic_keys.initiator_to_responder,
            b.traffic_keys.initiator_to_responder,
        );
    }

    #[test]
    fn both_peers_derive_same_r2i_key() {
        let (a, b) = handshake();
        assert_eq!(
            a.traffic_keys.responder_to_initiator,
            b.traffic_keys.responder_to_initiator,
        );
    }

    #[test]
    fn i2r_and_r2i_keys_are_distinct() {
        let (a, _) = handshake();
        assert_ne!(
            a.traffic_keys.initiator_to_responder,
            a.traffic_keys.responder_to_initiator,
        );
    }

    #[test]
    fn two_independent_handshakes_produce_different_keys() {
        let (a1, _) = handshake();
        let (a2, _) = handshake();
        assert_ne!(
            a1.traffic_keys.initiator_to_responder,
            a2.traffic_keys.initiator_to_responder,
        );
    }

    // ── Transcript hash ───────────────────────────────────────────────────────

    #[test]
    fn both_peers_derive_same_transcript_hash() {
        let (a, b) = handshake();
        assert_eq!(a.transcript_hash, b.transcript_hash);
    }

    #[test]
    fn two_independent_handshakes_produce_different_transcript_hashes() {
        let (a1, _) = handshake();
        let (a2, _) = handshake();
        assert_ne!(a1.transcript_hash, a2.transcript_hash);
    }

    // ── Role assignment ───────────────────────────────────────────────────────

    #[test]
    fn initiator_result_has_is_initiator_true() {
        let (a, _) = handshake();
        assert!(a.is_initiator);
    }

    #[test]
    fn responder_result_has_is_initiator_false() {
        let (_, b) = handshake();
        assert!(!b.is_initiator);
    }

    // ── Remote pubkey recovery ────────────────────────────────────────────────

    #[test]
    fn initiator_recovers_responder_static_pubkey() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let code = "000000001";
        let (initiator, msg1) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), code);
        let responder = NoiseIkResponder::receive_message1(&resp_kp, code, &msg1).unwrap();
        let (_, msg2) = responder.send_message2();
        let result = initiator.receive_message2(&init_kp, &msg2).unwrap();
        assert_eq!(result.remote_static_pubkey, resp_kp.public_key_bytes());
    }

    #[test]
    fn responder_recovers_initiator_static_pubkey() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let code = "000000002";
        let (_, msg1) = NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), code);
        let responder = NoiseIkResponder::receive_message1(&resp_kp, code, &msg1).unwrap();
        let (resp_result, _) = responder.send_message2();
        assert_eq!(resp_result.remote_static_pubkey, init_kp.public_key_bytes());
    }

    // ── Wire sizes ────────────────────────────────────────────────────────────

    #[test]
    fn msg1_length_is_96() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (_, msg1) = NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "abc");
        assert_eq!(msg1.len(), MSG1_LEN);
    }

    #[test]
    fn msg2_length_is_48() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (_, msg1) = NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "abc");
        let responder = NoiseIkResponder::receive_message1(&resp_kp, "abc", &msg1).unwrap();
        let (_, msg2) = responder.send_message2();
        assert_eq!(msg2.len(), MSG2_LEN);
    }

    // ── Failure modes ─────────────────────────────────────────────────────────

    #[test]
    fn wrong_remote_static_pubkey_fails_msg1() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let wrong_kp = StaticKeypair::generate();
        // Initiator uses wrong pubkey — the es/ss DH outputs won't match.
        let (_, msg1) = NoiseIkInitiator::new(&init_kp, wrong_kp.public_key_bytes(), "code");
        let err = NoiseIkResponder::receive_message1(&resp_kp, "code", &msg1).unwrap_err();
        assert_eq!(err, HandshakeError::DecryptionFailed);
    }

    #[test]
    fn mismatched_session_code_fails_msg1() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (_, msg1) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "correct-code");
        let err =
            NoiseIkResponder::receive_message1(&resp_kp, "wrong-code", &msg1).unwrap_err();
        assert_eq!(err, HandshakeError::DecryptionFailed);
    }

    #[test]
    fn tampered_msg1_fails() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (_, mut msg1) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "code");
        msg1[40] ^= 0xFF; // flip bits in the encrypted static-pubkey region
        let err = NoiseIkResponder::receive_message1(&resp_kp, "code", &msg1).unwrap_err();
        assert_eq!(err, HandshakeError::DecryptionFailed);
    }

    #[test]
    fn tampered_msg2_fails() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (initiator, msg1) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "code");
        let responder = NoiseIkResponder::receive_message1(&resp_kp, "code", &msg1).unwrap();
        let (_, mut msg2) = responder.send_message2();
        msg2[35] ^= 0xFF; // flip bits in the encrypted payload region
        let err = initiator.receive_message2(&init_kp, &msg2).unwrap_err();
        assert_eq!(err, HandshakeError::DecryptionFailed);
    }

    #[test]
    fn bad_msg1_length_returns_error() {
        let resp_kp = StaticKeypair::generate();
        let short = vec![0u8; 50];
        let err = NoiseIkResponder::receive_message1(&resp_kp, "code", &short).unwrap_err();
        assert_eq!(err, HandshakeError::BadMessageLength { expected: MSG1_LEN, got: 50 });
    }

    #[test]
    fn bad_msg2_length_returns_error() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let (initiator, _) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), "code");
        let short = vec![0u8; 20];
        let err = initiator.receive_message2(&init_kp, &short).unwrap_err();
        assert_eq!(err, HandshakeError::BadMessageLength { expected: MSG2_LEN, got: 20 });
    }

    // ── Integration: traffic keys feed DatagramCipher ─────────────────────────

    #[test]
    fn initiator_encrypts_responder_decrypts() {
        let (init_result, resp_result) = handshake();

        let send_key = *init_result.traffic_keys.send_key(init_result.is_initiator);
        let recv_key = *resp_result.traffic_keys.recv_key(resp_result.is_initiator);

        let mut sender = DatagramCipher::new(send_key);
        let receiver = DatagramCipher::new(recv_key);

        let plaintext = b"remote control frame";
        let payload = sender.seal(plaintext);
        let recovered = receiver.open(&payload).expect("must decrypt");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn responder_encrypts_initiator_decrypts() {
        let (init_result, resp_result) = handshake();

        let send_key = *resp_result.traffic_keys.send_key(resp_result.is_initiator);
        let recv_key = *init_result.traffic_keys.recv_key(init_result.is_initiator);

        let mut sender = DatagramCipher::new(send_key);
        let receiver = DatagramCipher::new(recv_key);

        let plaintext = b"screen share frame";
        let payload = sender.seal(plaintext);
        let recovered = receiver.open(&payload).expect("must decrypt");
        assert_eq!(recovered, plaintext);
    }

    // ── Integration: transcript hash feeds ShortAuthString ────────────────────

    #[test]
    fn both_peers_display_same_short_auth_string() {
        let (a, b) = handshake();
        let sas_a = ShortAuthString::derive(&a.transcript_hash);
        let sas_b = ShortAuthString::derive(&b.transcript_hash);
        assert_eq!(sas_a.to_string(), sas_b.to_string());
    }

    // ── Integration: remote pubkey feeds KnownPeerStore ───────────────────────

    #[test]
    fn remote_pubkey_stored_in_known_peer_store() {
        let init_kp = StaticKeypair::generate();
        let resp_kp = StaticKeypair::generate();
        let code = "555555555";
        let (initiator, msg1) =
            NoiseIkInitiator::new(&init_kp, resp_kp.public_key_bytes(), code);
        let responder = NoiseIkResponder::receive_message1(&resp_kp, code, &msg1).unwrap();
        let (resp_result, msg2) = responder.send_message2();
        let init_result = initiator.receive_message2(&init_kp, &msg2).unwrap();

        let mut store = KnownPeerStore::new();
        let now_ms = 1_000_000u64;

        let init_peer_id = store.upsert(init_result.remote_static_pubkey, now_ms);
        let resp_peer_id = store.upsert(resp_result.remote_static_pubkey, now_ms);

        assert_eq!(
            store.get(init_peer_id).unwrap().static_pubkey,
            resp_kp.public_key_bytes(),
        );
        assert_eq!(
            store.get(resp_peer_id).unwrap().static_pubkey,
            init_kp.public_key_bytes(),
        );
    }

    // ── StaticKeypair ─────────────────────────────────────────────────────────

    #[test]
    fn static_keypair_public_key_is_32_bytes() {
        let kp = StaticKeypair::generate();
        assert_eq!(kp.public_key_bytes().len(), 32);
    }

    #[test]
    fn two_generated_static_keypairs_have_different_public_keys() {
        let kp1 = StaticKeypair::generate();
        let kp2 = StaticKeypair::generate();
        assert_ne!(kp1.public_key_bytes(), kp2.public_key_bytes());
    }
}

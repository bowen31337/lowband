//! Features 21 & 25 — ChaCha20-Poly1305 datagram encryption and E2EE relay
//! transparency invariant.
//!
//! # Encryption (Feature 21)
//!
//! Every datagram is sealed under ChaCha20-Poly1305 (RFC 8439) using a
//! monotone 64-bit counter nonce:
//!
//! ```text
//! Wire layout per datagram:
//!   [0..12)   — 12-byte IETF ChaCha20 nonce  (4 zero bytes || counter LE64)
//!   [12..N+12)— ChaCha20 ciphertext           (same length as plaintext)
//!   [N+12..)  — Poly1305 AEAD tag             (16 bytes)
//! ```
//!
//! The counter increments with every call to [`DatagramCipher::seal`],
//! guaranteeing nonce uniqueness within one key epoch.
//! [`DatagramCipher::needs_rekey`] signals when the counter approaches 2^30
//! or the key has been active for 15 minutes (Feature 22).
//!
//! # E2EE relay invariant (Feature 25)
//!
//! Every datagram routed through the TURN relay must be encrypted before it
//! enters the relay framer.  This module enforces the invariant structurally
//! at the type level:
//!
//! 1. [`RelayPayload`] is a sealed newtype whose private field prevents
//!    external construction — the only way to obtain one is via
//!    [`DatagramCipher::seal`].
//! 2. [`E2eeRelayBridge::frame_for_relay`] accepts only `&RelayPayload`,
//!    not raw `&[u8]`, so the Rust borrow checker statically prevents
//!    plaintext from reaching the TURN ChannelData framer.
//! 3. The TURN server therefore forwards only ciphertext — it cannot read,
//!    modify, or inject session content.
//!
//! # Integration
//!
//! ```rust
//! use lowband_crypto::relay_guard::{DatagramCipher, E2eeRelayBridge};
//! use lowband_lbtp::turn_relay::TURN_DEFAULT_CHANNEL_NUMBER;
//!
//! let key = [0x42u8; 32]; // real key comes from HKDF (Feature 20)
//! let mut cipher = DatagramCipher::new(key);
//! let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);
//!
//! // Plaintext media frame.
//! let plaintext = b"raw audio frame";
//!
//! // Encrypt — the only way to produce a RelayPayload.
//! let payload = cipher.seal(plaintext);
//!
//! // Frame for the TURN relay — only accepts RelayPayload, not &[u8].
//! let channel_data = bridge.frame_for_relay(&payload)
//!     .expect("payload within relay size limit");
//! // → write channel_data to the UDP socket bound to the TURN server.
//! ```

use std::time::{Duration, Instant};

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use lowband_lbtp::turn_relay::{
    TurnChannelDataFramer, TURN_MAX_CHANNEL_NUMBER, TURN_MIN_CHANNEL_NUMBER,
};

/// Datagram count at which [`DatagramCipher::needs_rekey`] signals that the
/// traffic key must be replaced.  2^30 ≈ 1.07 billion datagrams.
pub const TRAFFIC_KEY_DATAGRAM_LIMIT: u64 = 1 << 30;

/// Maximum wall-clock age of a single traffic key.  A cipher whose key was
/// installed more than this long ago will signal [`DatagramCipher::needs_rekey`]
/// regardless of the datagram count.
pub const TRAFFIC_KEY_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// Overhead added to each plaintext datagram by [`DatagramCipher::seal`].
///
/// 12 bytes nonce (IETF ChaCha20 nonce prefix) + 16 bytes Poly1305 AEAD tag.
/// The constant reflects the final wire size accurately so callers can compute
/// the largest plaintext that fits within the LBTP 1 200-byte relay ceiling.
pub const RELAY_GUARD_OVERHEAD_BYTES: usize = 28;

/// Sealed ciphertext datagram ready for TURN relay forwarding (Feature 25).
///
/// The private `Vec<u8>` field is the sole constructor gate — from outside
/// this module, `RelayPayload` can only be obtained via
/// [`DatagramCipher::seal`].  This makes it structurally impossible to pass
/// a plaintext byte slice directly to [`E2eeRelayBridge::frame_for_relay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayPayload(Vec<u8>);

impl RelayPayload {
    /// Shared reference to the sealed ciphertext bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consume the payload and return the ciphertext bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Ciphertext length (plaintext length + [`RELAY_GUARD_OVERHEAD_BYTES`]).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the ciphertext is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Per-session datagram cipher — seals plaintext into [`RelayPayload`] using
/// ChaCha20-Poly1305 (Feature 21).
///
/// Each call to [`seal`](Self::seal) uses a unique 12-byte IETF nonce derived
/// from the monotone datagram counter, so nonces never repeat within a key
/// epoch.  [`needs_rekey`](Self::needs_rekey) signals when the epoch must end
/// (Feature 22).
#[derive(Debug)]
pub struct DatagramCipher {
    key: [u8; 32],
    counter: u64,
    born_at: Instant,
}

impl DatagramCipher {
    /// Create a cipher seeded with the 32-byte session `key`.
    ///
    /// The `key` is derived from HKDF traffic key derivation (Feature 20).
    pub fn new(key: [u8; 32]) -> Self {
        Self { key, counter: 0, born_at: Instant::now() }
    }

    /// Returns `true` when the caller must derive a fresh traffic key and call
    /// [`rotate_key`](Self::rotate_key) before sealing further datagrams.
    ///
    /// Either threshold independently triggers the signal:
    /// - **Datagram limit**: `counter` has reached [`TRAFFIC_KEY_DATAGRAM_LIMIT`]
    ///   (2^30 datagrams).
    /// - **Age limit**: the key has been active for at least
    ///   [`TRAFFIC_KEY_MAX_AGE`] (15 minutes).
    pub fn needs_rekey(&self) -> bool {
        self.counter >= TRAFFIC_KEY_DATAGRAM_LIMIT
            || self.born_at.elapsed() >= TRAFFIC_KEY_MAX_AGE
    }

    /// Install `new_key` as the active traffic key and reset both the datagram
    /// counter and the age clock.
    ///
    /// After this call [`needs_rekey`](Self::needs_rekey) returns `false` until
    /// one of the two thresholds is reached again with the new key.
    pub fn rotate_key(&mut self, new_key: [u8; 32]) {
        self.key = new_key;
        self.counter = 0;
        self.born_at = Instant::now();
    }

    /// Encrypt `plaintext` and return an opaque [`RelayPayload`] (Feature 21).
    ///
    /// Uses ChaCha20-Poly1305 (RFC 8439).  The 12-byte IETF nonce is built
    /// from the monotone datagram counter, ensuring uniqueness per key epoch.
    ///
    /// Wire layout: `[nonce 12 B][ciphertext N B][Poly1305 tag 16 B]`.
    pub fn seal(&mut self, plaintext: &[u8]) -> RelayPayload {
        let nonce_bytes = self.nonce_bytes();
        self.counter += 1;

        let aead = ChaCha20Poly1305::new_from_slice(&self.key)
            .expect("key is always 32 bytes");
        let nonce = Nonce::from(nonce_bytes);
        // encrypt() appends the 16-byte Poly1305 tag: returns [ciphertext || tag].
        let ciphertext_with_tag = aead
            .encrypt(&nonce, plaintext)
            .expect("ChaCha20-Poly1305 encrypt never fails for a valid key");

        let mut out = Vec::with_capacity(12 + ciphertext_with_tag.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext_with_tag);
        RelayPayload(out)
    }

    /// Decrypt a [`RelayPayload`] and return the plaintext, or `None` if the
    /// AEAD tag does not verify (tampered ciphertext, wrong key, or truncated
    /// wire data).
    ///
    /// Wire layout expected: `[nonce 12 B][ciphertext N B][Poly1305 tag 16 B]`.
    pub fn open(&self, payload: &RelayPayload) -> Option<Vec<u8>> {
        let bytes = payload.as_bytes();
        if bytes.len() < RELAY_GUARD_OVERHEAD_BYTES {
            return None;
        }
        let (nonce_bytes, ciphertext_with_tag) = bytes.split_at(12);

        let aead = ChaCha20Poly1305::new_from_slice(&self.key)
            .expect("key is always 32 bytes");
        let nonce = Nonce::try_from(nonce_bytes).ok()?;
        aead.decrypt(&nonce, ciphertext_with_tag).ok()
    }

    /// Decrypt raw datagram bytes received off the wire.
    ///
    /// The receive path reads bytes from a socket, not a [`RelayPayload`]
    /// (which can only be produced locally by [`seal`](Self::seal)).  This is
    /// the counterpart used by the live transport: it accepts the same
    /// `[nonce 12 B][ciphertext N B][tag 16 B]` layout `seal` produces and
    /// returns the plaintext, or `None` on any authentication failure.
    pub fn open_bytes(&self, wire: &[u8]) -> Option<Vec<u8>> {
        self.open(&RelayPayload(wire.to_vec()))
    }

    /// Number of datagrams sealed so far (equals the next nonce counter).
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Build the 12-byte IETF ChaCha20 nonce from the current counter.
    ///
    /// Layout: 4 zero bytes of padding, then the 8-byte little-endian counter.
    fn nonce_bytes(&self) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&self.counter.to_le_bytes());
        nonce
    }
}

/// Bridges E2EE-encrypted payloads to the TURN ChannelData framer.
///
/// Accepts only [`RelayPayload`] values — not raw `&[u8]` — so the Rust
/// type system statically guarantees that every datagram reaching the framer
/// originated from [`DatagramCipher::seal`].  The TURN server therefore
/// forwards only ciphertext it cannot interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E2eeRelayBridge {
    framer: TurnChannelDataFramer,
}

impl E2eeRelayBridge {
    /// Create a bridge using the given TURN `channel_number` (`0x4000–0x7FFF`).
    pub fn new(channel_number: u16) -> Self {
        debug_assert!(
            channel_number >= TURN_MIN_CHANNEL_NUMBER
                && channel_number <= TURN_MAX_CHANNEL_NUMBER,
            "channel_number {channel_number:#06x} outside valid TURN range \
             {TURN_MIN_CHANNEL_NUMBER:#06x}–{TURN_MAX_CHANNEL_NUMBER:#06x}"
        );
        Self {
            framer: TurnChannelDataFramer::new(channel_number),
        }
    }

    /// Encode `payload` as a TURN ChannelData message.
    ///
    /// Returns `Some(Vec<u8>)` — a 4-byte ChannelData header followed by the
    /// ciphertext bytes — or `None` when the payload exceeds the LBTP
    /// 1 200-byte ceiling.
    ///
    /// The only accepted argument type is `&RelayPayload`; a raw `&[u8]`
    /// is a compile-time error, enforcing the E2EE invariant at every call
    /// site.
    pub fn frame_for_relay(&self, payload: &RelayPayload) -> Option<Vec<u8>> {
        self.framer.encode(payload.as_bytes())
    }

    /// The TURN channel number this bridge encodes into.
    pub fn channel_number(&self) -> u16 {
        self.framer.channel_number()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_lbtp::turn_relay::{
        TURN_CHANNEL_HEADER_BYTES, TURN_DEFAULT_CHANNEL_NUMBER, TURN_MAX_PAYLOAD_BYTES,
    };

    const TEST_KEY: [u8; 32] = [0x5A; 32];

    // ── Construction invariant ─────────────────────────────────────────────────

    #[test]
    fn relay_payload_only_constructible_via_seal() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(b"media frame");
        assert!(!payload.is_empty(), "seal must produce a non-empty RelayPayload");
    }

    // ── DatagramCipher — seal ──────────────────────────────────────────────────

    #[test]
    fn seal_output_differs_from_plaintext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext = b"raw media plaintext";
        let payload = cipher.seal(plaintext);
        let ciphertext = payload.as_bytes();

        // ChaCha20 ciphertext body: skip 12-byte nonce prefix and 16-byte tag.
        let body = &ciphertext[12..ciphertext.len() - 16];
        assert_ne!(
            body, plaintext,
            "ciphertext body must differ from plaintext — TURN relay must not see raw media"
        );
    }

    #[test]
    fn seal_output_length_equals_plaintext_plus_overhead() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext: Vec<u8> = (0u8..64).collect();
        let payload = cipher.seal(&plaintext);
        assert_eq!(
            payload.len(),
            plaintext.len() + RELAY_GUARD_OVERHEAD_BYTES,
            "seal must add exactly RELAY_GUARD_OVERHEAD_BYTES ({RELAY_GUARD_OVERHEAD_BYTES} B)"
        );
    }

    #[test]
    fn seal_empty_plaintext_produces_overhead_only() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(&[]);
        assert_eq!(
            payload.len(),
            RELAY_GUARD_OVERHEAD_BYTES,
            "sealing empty plaintext must produce exactly the overhead bytes"
        );
    }

    #[test]
    fn seal_increments_counter() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        assert_eq!(cipher.counter(), 0);
        cipher.seal(b"a");
        assert_eq!(cipher.counter(), 1);
        cipher.seal(b"b");
        assert_eq!(cipher.counter(), 2);
    }

    #[test]
    fn successive_seals_of_same_plaintext_produce_different_ciphertext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext = b"identical plaintext";
        let p1 = cipher.seal(plaintext);
        let p2 = cipher.seal(plaintext);
        assert_ne!(
            p1.as_bytes(),
            p2.as_bytes(),
            "each seal must use a unique nonce so successive encryptions differ"
        );
    }

    #[test]
    fn nonce_prefix_changes_with_counter() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let p1 = cipher.seal(b"x");
        let p2 = cipher.seal(b"x");
        assert_ne!(
            &p1.as_bytes()[..12],
            &p2.as_bytes()[..12],
            "12-byte nonce prefix must change between consecutive datagrams"
        );
    }

    #[test]
    fn different_keys_produce_different_ciphertext() {
        let mut c1 = DatagramCipher::new([0x11u8; 32]);
        let mut c2 = DatagramCipher::new([0x22u8; 32]);
        let plaintext = b"same plaintext, different keys";
        let p1 = c1.seal(plaintext);
        let p2 = c2.seal(plaintext);
        assert_ne!(
            p1.as_bytes(),
            p2.as_bytes(),
            "different session keys must produce different ciphertext"
        );
    }

    // ── Feature 21: ChaCha20-Poly1305 AEAD correctness ────────────────────────

    #[test]
    fn open_roundtrip_recovers_plaintext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext = b"audio frame roundtrip";
        let payload = cipher.seal(plaintext);
        let recovered = cipher.open(&payload).expect("open must succeed for valid ciphertext");
        assert_eq!(recovered, plaintext, "decrypted plaintext must match original");
    }

    #[test]
    fn open_roundtrip_empty_plaintext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(&[]);
        let recovered = cipher.open(&payload).expect("open must succeed for empty plaintext");
        assert!(recovered.is_empty());
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(b"sensitive media");
        let mut bytes = payload.into_bytes();
        // Flip a bit in the ciphertext body (after the 12-byte nonce).
        bytes[13] ^= 0xFF;
        let tampered = RelayPayload(bytes);
        assert!(
            cipher.open(&tampered).is_none(),
            "open must return None when ciphertext is tampered (Poly1305 tag mismatch)"
        );
    }

    #[test]
    fn open_rejects_tampered_tag() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(b"video frame");
        let mut bytes = payload.into_bytes();
        // Corrupt the last byte of the Poly1305 tag.
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = RelayPayload(bytes);
        assert!(
            cipher.open(&tampered).is_none(),
            "open must return None when the AEAD tag is corrupted"
        );
    }

    #[test]
    fn open_rejects_wrong_key() {
        let mut sealer = DatagramCipher::new([0xAA; 32]);
        let payload = sealer.seal(b"screen tile");

        let opener = DatagramCipher::new([0xBB; 32]);
        assert!(
            opener.open(&payload).is_none(),
            "open must return None when the key does not match"
        );
    }

    #[test]
    fn open_rejects_truncated_payload() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(b"some data");
        // Truncate to fewer than RELAY_GUARD_OVERHEAD_BYTES.
        let truncated = RelayPayload(payload.as_bytes()[..10].to_vec());
        assert!(
            cipher.open(&truncated).is_none(),
            "open must return None for payloads shorter than overhead"
        );
    }

    #[test]
    fn seal_open_multiple_datagrams_each_verify() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let messages: &[&[u8]] = &[b"frame 0", b"frame 1", b"frame 2", b"frame 3"];
        let payloads: Vec<_> = messages.iter().map(|m| cipher.seal(m)).collect();
        for (payload, &msg) in payloads.iter().zip(messages.iter()) {
            let recovered = cipher.open(payload).expect("each datagram must decrypt");
            assert_eq!(recovered, msg);
        }
    }

    // ── E2EE invariant: relay sees only ciphertext ─────────────────────────────

    #[test]
    fn relay_framer_receives_ciphertext_not_plaintext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        let plaintext = b"camera frame data";
        let payload = cipher.seal(plaintext);
        let channel_data = bridge.frame_for_relay(&payload).unwrap();

        let relay_payload = &channel_data[TURN_CHANNEL_HEADER_BYTES..];

        assert_ne!(
            relay_payload,
            plaintext.as_slice(),
            "TURN relay must not see plaintext — relay payload must be ciphertext"
        );
        assert_eq!(
            relay_payload,
            payload.as_bytes(),
            "relay payload must be the exact ciphertext bytes returned by seal()"
        );
    }

    #[test]
    fn relay_framer_receives_ciphertext_for_audio_frame() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        let audio_frame: Vec<u8> = (0u8..80).collect();
        let payload = cipher.seal(&audio_frame);
        let channel_data = bridge.frame_for_relay(&payload).unwrap();

        let relay_bytes = &channel_data[TURN_CHANNEL_HEADER_BYTES..];
        assert_ne!(
            relay_bytes,
            audio_frame.as_slice(),
            "audio frame must be encrypted before reaching the TURN relay"
        );
    }

    #[test]
    fn relay_framer_receives_ciphertext_for_screen_frame() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        // 1 100-byte compressed screen tile; +28 overhead = 1 128 B ≤ 1 200 B ceiling.
        let screen_tile: Vec<u8> = (0u8..=255).cycle().take(1_100).collect();
        let payload = cipher.seal(&screen_tile);
        let channel_data = bridge.frame_for_relay(&payload).unwrap();

        let relay_bytes = &channel_data[TURN_CHANNEL_HEADER_BYTES..];
        assert_ne!(
            relay_bytes,
            screen_tile.as_slice(),
            "screen tile must be encrypted before reaching the TURN relay"
        );
    }

    // ── E2eeRelayBridge ────────────────────────────────────────────────────────

    #[test]
    fn bridge_uses_configured_channel_number() {
        let bridge = E2eeRelayBridge::new(0x5000);
        assert_eq!(bridge.channel_number(), 0x5000);
    }

    #[test]
    fn bridge_default_channel_number() {
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);
        assert_eq!(bridge.channel_number(), TURN_DEFAULT_CHANNEL_NUMBER);
    }

    #[test]
    fn bridge_frame_for_relay_returns_none_for_oversized_payload() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        let oversized = vec![0u8; TURN_MAX_PAYLOAD_BYTES - RELAY_GUARD_OVERHEAD_BYTES + 1];
        let payload = cipher.seal(&oversized);

        assert!(
            bridge.frame_for_relay(&payload).is_none(),
            "frame_for_relay must return None when ciphertext exceeds the 1 200-byte ceiling"
        );
    }

    #[test]
    fn bridge_frame_for_relay_accepts_max_sized_plaintext() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        // Largest plaintext that stays within the 1 200 B ceiling after sealing
        // (1 200 - 28 = 1 172 B).
        let max_plaintext_len = TURN_MAX_PAYLOAD_BYTES - RELAY_GUARD_OVERHEAD_BYTES;
        let plaintext = vec![0xABu8; max_plaintext_len];
        let payload = cipher.seal(&plaintext);

        assert!(
            bridge.frame_for_relay(&payload).is_some(),
            "max-size plaintext (ciphertext = {TURN_MAX_PAYLOAD_BYTES} B) must be accepted"
        );
    }

    // ── Wire format ────────────────────────────────────────────────────────────

    #[test]
    fn frame_for_relay_output_is_valid_turn_channel_data() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let bridge = E2eeRelayBridge::new(TURN_DEFAULT_CHANNEL_NUMBER);

        let plaintext = b"test media";
        let payload = cipher.seal(plaintext);
        let channel_data = bridge.frame_for_relay(&payload).unwrap();

        let channel = u16::from_be_bytes([channel_data[0], channel_data[1]]);
        let data_len = u16::from_be_bytes([channel_data[2], channel_data[3]]) as usize;

        assert_eq!(channel, TURN_DEFAULT_CHANNEL_NUMBER);
        assert_eq!(data_len, payload.len());
        assert_eq!(
            channel_data.len(),
            TURN_CHANNEL_HEADER_BYTES + payload.len(),
            "output must be 4-byte header + ciphertext"
        );
    }

    // ── RelayPayload accessors ─────────────────────────────────────────────────

    #[test]
    fn as_bytes_and_into_bytes_agree() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let payload = cipher.seal(b"hello");
        let via_ref = payload.as_bytes().to_vec();
        let via_own = payload.into_bytes();
        assert_eq!(via_ref, via_own);
    }

    #[test]
    fn len_and_is_empty_reflect_ciphertext_size() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext = b"audio";
        let payload = cipher.seal(plaintext);
        assert_eq!(payload.len(), plaintext.len() + RELAY_GUARD_OVERHEAD_BYTES);
        assert!(!payload.is_empty());
    }

    // ── Constants ──────────────────────────────────────────────────────────────

    #[test]
    fn relay_guard_overhead_is_12_byte_nonce_plus_16_byte_tag() {
        assert_eq!(
            RELAY_GUARD_OVERHEAD_BYTES, 28,
            "overhead must equal 12 B IETF ChaCha20 nonce + 16 B Poly1305 tag"
        );
    }

    // ── Feature 22 — traffic key invalidation ─────────────────────────────────

    #[test]
    fn needs_rekey_false_for_fresh_cipher() {
        let cipher = DatagramCipher::new(TEST_KEY);
        assert!(!cipher.needs_rekey(), "fresh cipher must not need rekeying");
    }

    #[test]
    fn needs_rekey_false_one_datagram_before_limit() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        cipher.counter = TRAFFIC_KEY_DATAGRAM_LIMIT - 1;
        assert!(
            !cipher.needs_rekey(),
            "cipher must not need rekey one datagram before the 2^30 limit"
        );
    }

    #[test]
    fn needs_rekey_true_at_datagram_limit() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        cipher.counter = TRAFFIC_KEY_DATAGRAM_LIMIT;
        assert!(
            cipher.needs_rekey(),
            "cipher must signal rekey when counter reaches 2^30"
        );
    }

    #[test]
    fn needs_rekey_true_when_key_is_15_minutes_old() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        cipher.born_at = std::time::Instant::now() - TRAFFIC_KEY_MAX_AGE;
        assert!(
            cipher.needs_rekey(),
            "cipher must signal rekey when the key has been active for 15 minutes"
        );
    }

    #[test]
    fn rotate_key_resets_counter_and_clears_rekey_signal() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        cipher.counter = TRAFFIC_KEY_DATAGRAM_LIMIT;
        assert!(cipher.needs_rekey(), "precondition: cipher needs rekey");

        cipher.rotate_key([0xBBu8; 32]);

        assert_eq!(cipher.counter(), 0, "counter must reset to 0 after rotate_key");
        assert!(
            !cipher.needs_rekey(),
            "cipher must not need rekey immediately after rotate_key"
        );
    }

    #[test]
    fn rotate_key_installs_new_key_material() {
        let mut cipher = DatagramCipher::new(TEST_KEY);
        let plaintext = b"probe datagram";
        let before = cipher.seal(plaintext);

        // rotate_key resets counter to 0, so same nonce position but different key.
        cipher.rotate_key([0xCCu8; 32]);
        let after = cipher.seal(plaintext);

        assert_ne!(
            before.as_bytes(),
            after.as_bytes(),
            "different keys must produce different ciphertext for the same plaintext and nonce"
        );
    }
}

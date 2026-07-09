//! Feature 23 — Peer static public-key persistence.
//!
//! After a successful Noise-IK handshake (Feature 19) the daemon extracts the
//! remote peer's 32-byte static X25519 public key and calls
//! [`KnownPeerStore::upsert`].  On first contact the store creates a new
//! [`KnownPeer`] entry and assigns it a monotone [`PeerId`]; on repeat contact
//! it advances `last_seen_at_ms` while leaving `first_seen_at_ms` unchanged.
//!
//! The in-memory types map directly to the `known_peers` SQLite table:
//!
//! | Column           | Rust field                    |
//! |------------------|-------------------------------|
//! | id               | `KnownPeer::id`               |
//! | static_pubkey    | `KnownPeer::static_pubkey`    |
//! | display_label    | `KnownPeer::display_label`    |
//! | first_seen_at    | `KnownPeer::first_seen_at_ms` |
//! | last_seen_at     | `KnownPeer::last_seen_at_ms`  |
//!
//! # Example
//!
//! ```
//! use lowband_crypto::known_peers::KnownPeerStore;
//!
//! let mut store = KnownPeerStore::new();
//!
//! let pubkey = [0x1a_u8; 32];
//!
//! // First contact — inserts a new entry.
//! let id = store.upsert(pubkey, 1_000);
//!
//! // Second contact — updates last_seen_at_ms, returns the same PeerId.
//! let id2 = store.upsert(pubkey, 5_000);
//! assert_eq!(id, id2);
//!
//! let peer = store.get(id).unwrap();
//! assert_eq!(peer.first_seen_at_ms, 1_000);
//! assert_eq!(peer.last_seen_at_ms,  5_000);
//! ```

/// Opaque identifier for a persisted peer, assigned on first contact.
///
/// Assigned by [`KnownPeerStore::upsert`] from a monotone counter that starts
/// at 1 (0 is reserved as a "not found" sentinel in the DB schema's foreign
/// key column).  Stable across the lifetime of the store; the SQLite layer maps
/// this to the `id UUID` column when flushing in-memory state to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub u64);

/// A single row in the `known_peers` store.
#[derive(Debug, Clone)]
pub struct KnownPeer {
    /// Stable identifier for this peer entry.
    pub id: PeerId,
    /// 32-byte X25519 static public key received during the Noise-IK handshake.
    pub static_pubkey: [u8; 32],
    /// Optional human-readable label set by the local user (max 120 bytes).
    ///
    /// Maps to `known_peers.display_label VARCHAR(120)`.
    pub display_label: Option<String>,
    /// Wall-clock milliseconds since the Unix epoch when this peer was first seen.
    pub first_seen_at_ms: u64,
    /// Wall-clock milliseconds since the Unix epoch when this peer was most recently seen.
    pub last_seen_at_ms: u64,
}

/// In-memory `known_peers` store.
///
/// Upserts peer static public keys on handshake completion and can serialise
/// the full table to JSON.  Each 32-byte key is unique; a second call with the
/// same key advances `last_seen_at_ms` rather than inserting a duplicate.
pub struct KnownPeerStore {
    peers: Vec<KnownPeer>,
    next_id: u64,
}

impl KnownPeerStore {
    /// Create an empty store.  The first assigned [`PeerId`] will be 1.
    pub fn new() -> Self {
        Self { peers: Vec::new(), next_id: 1 }
    }

    /// Persist a peer's static public key, creating or updating its entry.
    ///
    /// - **First contact** (`static_pubkey` not yet in the store): inserts a new
    ///   [`KnownPeer`] with `first_seen_at_ms = now_ms` and `last_seen_at_ms = now_ms`
    ///   and returns the freshly assigned [`PeerId`].
    /// - **Repeat contact**: advances `last_seen_at_ms` to `now_ms` (clamped so it
    ///   never regresses below the stored value) and returns the existing [`PeerId`].
    pub fn upsert(&mut self, static_pubkey: [u8; 32], now_ms: u64) -> PeerId {
        if let Some(i) = self.find_index(&static_pubkey) {
            if now_ms > self.peers[i].last_seen_at_ms {
                self.peers[i].last_seen_at_ms = now_ms;
            }
            self.peers[i].id
        } else {
            let id = PeerId(self.next_id);
            self.next_id += 1;
            self.peers.push(KnownPeer {
                id,
                static_pubkey,
                display_label: None,
                first_seen_at_ms: now_ms,
                last_seen_at_ms: now_ms,
            });
            id
        }
    }

    /// Look up a peer by [`PeerId`].
    pub fn get(&self, id: PeerId) -> Option<&KnownPeer> {
        self.peers.iter().find(|p| p.id == id)
    }

    /// Look up a peer by its static public key.
    pub fn find_by_key(&self, static_pubkey: &[u8; 32]) -> Option<&KnownPeer> {
        self.peers.iter().find(|p| &p.static_pubkey == static_pubkey)
    }

    /// Set or replace the display label for the given peer.
    ///
    /// Labels longer than 120 bytes are silently truncated to respect the
    /// `VARCHAR(120)` column limit in the SQLite schema.  Returns `true` when
    /// the peer was found and updated, `false` otherwise.
    pub fn set_label(&mut self, id: PeerId, label: &str) -> bool {
        let i = match self.peers.iter().position(|p| p.id == id) {
            Some(i) => i,
            None    => return false,
        };
        let truncated = truncate_to_120_bytes(label);
        self.peers[i].display_label = Some(truncated.to_owned());
        true
    }

    /// All persisted peers in insertion order.
    pub fn peers(&self) -> &[KnownPeer] {
        &self.peers
    }

    /// Serialise all peers to JSON for later export.
    ///
    /// Produces a JSON object with a `known_peers` array.  Each entry mirrors
    /// the `known_peers` table columns.  `static_pubkey` is encoded as 64
    /// lowercase hex digits.  `display_label` is a JSON string or `null`.
    pub fn export_json(&self) -> String {
        let mut buf = String::from("{\"known_peers\":[");
        for (i, p) in self.peers.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&peer_to_json(p));
        }
        buf.push_str("]}");
        buf
    }

    fn find_index(&self, static_pubkey: &[u8; 32]) -> Option<usize> {
        self.peers.iter().position(|p| &p.static_pubkey == static_pubkey)
    }
}

impl Default for KnownPeerStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate `s` to the longest UTF-8 prefix that fits in 120 bytes.
fn truncate_to_120_bytes(s: &str) -> &str {
    if s.len() <= 120 {
        return s;
    }
    // Walk back from byte 120 to find a valid UTF-8 char boundary.
    let mut end = 120;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── JSON serialisation ────────────────────────────────────────────────────────

fn peer_to_json(p: &KnownPeer) -> String {
    let hex: String = p.static_pubkey.iter().map(|b| format!("{b:02x}")).collect();
    let label_field = match &p.display_label {
        Some(l) => format!("\"{}\"", l.replace('"', "\\\"")),
        None    => "null".to_string(),
    };
    format!(
        "{{\"id\":{},\"static_pubkey\":\"{}\",\"display_label\":{},\
         \"first_seen_at_ms\":{},\"last_seen_at_ms\":{}}}",
        p.id.0, hex, label_field, p.first_seen_at_ms, p.last_seen_at_ms,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: [u8; 32] = [0x1a_u8; 32];
    const KEY_B: [u8; 32] = [0x2b_u8; 32];

    // ── KnownPeerStore::upsert — first contact ────────────────────────────────

    #[test]
    fn upsert_first_contact_inserts_new_peer() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 1_000);
        assert_eq!(store.peers().len(), 1);
    }

    #[test]
    fn upsert_first_contact_returns_nonzero_peer_id() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        assert_ne!(id.0, 0, "PeerId 0 is the reserved sentinel; assigned IDs must start at 1");
    }

    #[test]
    fn upsert_first_contact_stores_static_pubkey() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        assert_eq!(store.get(id).unwrap().static_pubkey, KEY_A);
    }

    #[test]
    fn upsert_first_contact_sets_first_and_last_seen() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        let peer = store.get(id).unwrap();
        assert_eq!(peer.first_seen_at_ms, 1_000);
        assert_eq!(peer.last_seen_at_ms,  1_000);
    }

    #[test]
    fn upsert_first_contact_leaves_display_label_none() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        assert!(store.get(id).unwrap().display_label.is_none());
    }

    // ── KnownPeerStore::upsert — repeat contact ───────────────────────────────

    #[test]
    fn upsert_repeat_contact_returns_same_peer_id() {
        let mut store = KnownPeerStore::new();
        let id1 = store.upsert(KEY_A, 1_000);
        let id2 = store.upsert(KEY_A, 5_000);
        assert_eq!(id1, id2);
    }

    #[test]
    fn upsert_repeat_contact_does_not_insert_duplicate() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 1_000);
        store.upsert(KEY_A, 5_000);
        assert_eq!(store.peers().len(), 1);
    }

    #[test]
    fn upsert_repeat_contact_advances_last_seen_at() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        store.upsert(KEY_A, 5_000);
        assert_eq!(store.get(id).unwrap().last_seen_at_ms, 5_000);
    }

    #[test]
    fn upsert_repeat_contact_does_not_change_first_seen_at() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        store.upsert(KEY_A, 5_000);
        assert_eq!(store.get(id).unwrap().first_seen_at_ms, 1_000);
    }

    #[test]
    fn upsert_repeat_contact_older_timestamp_does_not_regress_last_seen() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 5_000);
        store.upsert(KEY_A, 1_000); // earlier timestamp — must be ignored
        assert_eq!(store.get(id).unwrap().last_seen_at_ms, 5_000);
    }

    // ── Multiple distinct peers ───────────────────────────────────────────────

    #[test]
    fn two_distinct_keys_get_distinct_peer_ids() {
        let mut store = KnownPeerStore::new();
        let id_a = store.upsert(KEY_A, 1_000);
        let id_b = store.upsert(KEY_B, 2_000);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn two_distinct_keys_produce_two_entries() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 1_000);
        store.upsert(KEY_B, 2_000);
        assert_eq!(store.peers().len(), 2);
    }

    #[test]
    fn peer_ids_are_assigned_monotonically_from_one() {
        let mut store = KnownPeerStore::new();
        let id_a = store.upsert(KEY_A, 0);
        let id_b = store.upsert(KEY_B, 0);
        assert_eq!(id_a.0, 1);
        assert_eq!(id_b.0, 2);
    }

    // ── KnownPeerStore::find_by_key ───────────────────────────────────────────

    #[test]
    fn find_by_key_returns_peer_for_known_key() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 1_000);
        let found = store.find_by_key(&KEY_A).unwrap();
        assert_eq!(found.id, id);
    }

    #[test]
    fn find_by_key_returns_none_for_unknown_key() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 1_000);
        assert!(store.find_by_key(&KEY_B).is_none());
    }

    // ── KnownPeerStore::set_label ─────────────────────────────────────────────

    #[test]
    fn set_label_stores_display_label() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        assert!(store.set_label(id, "Alice's laptop"));
        assert_eq!(
            store.get(id).unwrap().display_label.as_deref(),
            Some("Alice's laptop")
        );
    }

    #[test]
    fn set_label_replaces_existing_label() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        store.set_label(id, "first");
        store.set_label(id, "second");
        assert_eq!(
            store.get(id).unwrap().display_label.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn set_label_unknown_peer_returns_false() {
        let mut store = KnownPeerStore::new();
        assert!(!store.set_label(PeerId(99), "ghost"));
    }

    #[test]
    fn set_label_truncates_at_120_bytes() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        let long_label: String = "x".repeat(200);
        store.set_label(id, &long_label);
        let stored = store.get(id).unwrap().display_label.as_deref().unwrap();
        assert!(stored.len() <= 120, "label must be truncated to 120 bytes");
    }

    #[test]
    fn set_label_120_byte_label_is_not_truncated() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        let exact: String = "y".repeat(120);
        store.set_label(id, &exact);
        assert_eq!(
            store.get(id).unwrap().display_label.as_deref().unwrap().len(),
            120
        );
    }

    // ── KnownPeerStore::export_json ───────────────────────────────────────────

    #[test]
    fn export_json_empty_store_returns_empty_array() {
        let store = KnownPeerStore::new();
        assert_eq!(store.export_json(), "{\"known_peers\":[]}");
    }

    #[test]
    fn export_json_contains_peer_id() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 0);
        let json = store.export_json();
        assert!(json.contains("\"id\":1"), "json: {json}");
    }

    #[test]
    fn export_json_static_pubkey_as_lowercase_hex() {
        let mut store = KnownPeerStore::new();
        store.upsert([0xab_u8; 32], 0);
        let json = store.export_json();
        let expected_hex = "ab".repeat(32);
        assert!(
            json.contains(&format!("\"static_pubkey\":\"{}\"", expected_hex)),
            "json: {json}"
        );
    }

    #[test]
    fn export_json_null_display_label_when_unset() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 0);
        let json = store.export_json();
        assert!(json.contains("\"display_label\":null"), "json: {json}");
    }

    #[test]
    fn export_json_display_label_quoted_when_set() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 0);
        store.set_label(id, "Bob's desktop");
        let json = store.export_json();
        assert!(json.contains("\"display_label\":\"Bob's desktop\""), "json: {json}");
    }

    #[test]
    fn export_json_first_and_last_seen_at_ms() {
        let mut store = KnownPeerStore::new();
        let _id = store.upsert(KEY_A, 1_000);
        store.upsert(KEY_A, 9_000);
        let json = store.export_json();
        assert!(json.contains("\"first_seen_at_ms\":1000"), "json: {json}");
        assert!(json.contains("\"last_seen_at_ms\":9000"),  "json: {json}");
    }

    #[test]
    fn export_json_all_schema_fields_present() {
        let mut store = KnownPeerStore::new();
        let id = store.upsert(KEY_A, 500);
        store.set_label(id, "test peer");
        let json = store.export_json();
        for field in &[
            "id", "static_pubkey", "display_label", "first_seen_at_ms", "last_seen_at_ms",
        ] {
            assert!(json.contains(field), "json missing '{field}': {json}");
        }
    }

    #[test]
    fn export_json_multiple_peers_all_present() {
        let mut store = KnownPeerStore::new();
        store.upsert(KEY_A, 1_000);
        store.upsert(KEY_B, 2_000);
        let json = store.export_json();
        assert!(json.contains("\"id\":1"), "json: {json}");
        assert!(json.contains("\"id\":2"), "json: {json}");
    }

    // ── Feature 23 acceptance: handshake → persist → re-contact ──────────────

    #[test]
    fn noise_ik_peer_key_persists_across_sessions() {
        // Simulates the daemon calling upsert() after each Noise-IK handshake.
        let mut store = KnownPeerStore::new();
        let peer_key = [0x42_u8; 32]; // static X25519 pubkey from Noise-IK

        // Session 1: first handshake with this peer at t=1000 ms.
        let id_first = store.upsert(peer_key, 1_000);

        // Session 2: same peer reconnects at t=60_000 ms.
        let id_second = store.upsert(peer_key, 60_000);

        // Same PeerId returned — the peer was recognised, not re-inserted.
        assert_eq!(id_first, id_second, "same key must resolve to the same PeerId");

        let peer = store.get(id_first).unwrap();
        assert_eq!(peer.static_pubkey,   peer_key);
        assert_eq!(peer.first_seen_at_ms, 1_000,  "first_seen must record the initial contact");
        assert_eq!(peer.last_seen_at_ms,  60_000, "last_seen must record the most recent contact");
        assert_eq!(store.peers().len(), 1, "exactly one entry for this key");
    }

    #[test]
    fn multiple_distinct_peers_stored_independently() {
        let mut store = KnownPeerStore::new();
        let key_alice = [0x01_u8; 32];
        let key_bob   = [0x02_u8; 32];
        let key_carol = [0x03_u8; 32];

        let id_alice = store.upsert(key_alice, 100);
        let id_bob   = store.upsert(key_bob,   200);
        let id_carol = store.upsert(key_carol, 300);

        assert_eq!(store.peers().len(), 3);
        assert_ne!(id_alice, id_bob);
        assert_ne!(id_bob,   id_carol);

        assert_eq!(store.get(id_alice).unwrap().static_pubkey, key_alice);
        assert_eq!(store.get(id_bob).unwrap().static_pubkey,   key_bob);
        assert_eq!(store.get(id_carol).unwrap().static_pubkey, key_carol);
    }
}

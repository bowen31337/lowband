//! Session-record persistence — Feature 32.
//!
//! The daemon creates a [`SessionRecord`] entry in the [`SessionRecordStore`]
//! when a peer LBTP session is established and closes it when the session ends.
//! Governor ticks update [`SessionRecord::total_bytes`] and
//! [`SessionRecord::peak_tier`] in place.  At any time the store can be
//! serialised to JSON via [`SessionRecordStore::export_json`] for later export
//! to the audit-export screen or offline analysis.
//!
//! # Schema alignment
//!
//! The in-memory types map directly to the `session_records` SQLite table:
//!
//! | Column        | Rust field                     |
//! |---------------|--------------------------------|
//! | connection_id | `SessionRecord::connection_id` |
//! | peer_key      | `SessionRecord::peer_key`      |
//! | started_at    | `SessionRecord::started_at_ms` |
//! | ended_at      | `SessionRecord::ended_at_ms`   |
//! | peak_tier     | `SessionRecord::peak_tier`     |
//! | total_bytes   | `SessionRecord::total_bytes`   |
//!
//! `peer_key` holds the remote peer's 32-byte static public key, which the
//! daemon maps to a `known_peers.id` UUID when persisting to SQLite.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::session_records::{SessionRecordStore, Tier};
//!
//! let mut store = SessionRecordStore::new();
//!
//! // Session opens (connection_id 42, anonymous peer, started at t=1000 ms).
//! store.open(42, None, 1_000);
//!
//! // Governor ticks update byte count and peak tier.
//! store.update_bytes(42, 4_800);
//! store.update_peak_tier(42, Tier::Constrained);
//! store.update_bytes(42, 9_600);
//! store.update_peak_tier(42, Tier::Comfortable);
//!
//! // Session ends at t=61 000 ms.
//! store.close(42, 61_000);
//!
//! let json = store.export_json();
//! assert!(json.contains("\"connection_id\":42"));
//! assert!(json.contains("\"peak_tier\":\"Comfortable\""));
//! ```

/// Quality tier reached during a session, ordered from lowest to highest.
///
/// The `PartialOrd`/`Ord` derive makes it easy to track the peak:
/// `Survival < Constrained < Comfortable < Full`.
/// [`SessionRecordStore::update_peak_tier`] only ever advances the stored
/// peak — it never retreats on a temporary downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Minimum viable: voice-only at 64 kbps or below.
    Survival,
    /// Functional: voice + coarse screen at 64–150 kbps.
    Constrained,
    /// Pleasant: voice + refined screen + limited camera at 150–400 kbps.
    Comfortable,
    /// Full quality: all streams at the highest gear.
    Full,
}

impl Tier {
    /// Return the canonical string label stored in `session_records.peak_tier`.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Survival    => "Survival",
            Tier::Constrained => "Constrained",
            Tier::Comfortable => "Comfortable",
            Tier::Full        => "Full",
        }
    }
}

/// A single row in the `session_records` store.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// LBTP `connection_id`, unique for the lifetime of a transport session.
    pub connection_id: u64,
    /// Static public key of the remote peer, if known.
    ///
    /// Maps to `peer_id` in the SQLite schema (a foreign key to `known_peers`).
    /// Encoded as lowercase hex in the JSON export.
    pub peer_key: Option<[u8; 32]>,
    /// Wall-clock milliseconds since the Unix epoch when the session was established.
    pub started_at_ms: u64,
    /// Wall-clock milliseconds since the Unix epoch when the session ended.
    ///
    /// `None` while the session is still live.
    pub ended_at_ms: Option<u64>,
    /// Highest quality tier reached at any point during the session.
    ///
    /// `None` before the first governor tier event arrives.
    pub peak_tier: Option<Tier>,
    /// Running total of bytes transferred across all streams, accumulated from
    /// governor `StreamBudget` ticks.
    pub total_bytes: u64,
}

/// In-memory `session_records` store.
///
/// Stores one [`SessionRecord`] per LBTP connection and can serialise the entire
/// collection to JSON for later export.  The store is append-only on open; close
/// and update operations mutate the matching record in place.
pub struct SessionRecordStore {
    records: Vec<SessionRecord>,
}

impl SessionRecordStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self { records: Vec::new() }
    }

    /// Open a new session record.
    ///
    /// Call this when a peer LBTP session is established.  `started_at_ms` is
    /// wall-clock milliseconds since the Unix epoch, supplied by the caller so
    /// the store remains deterministic and wall-clock-free.
    ///
    /// A duplicate `connection_id` is silently ignored — each LBTP session ID
    /// is unique within a daemon lifetime, so a duplicate signals a caller bug
    /// rather than a valid state transition.
    pub fn open(&mut self, connection_id: u64, peer_key: Option<[u8; 32]>, started_at_ms: u64) {
        if self.find_index(connection_id).is_none() {
            self.records.push(SessionRecord {
                connection_id,
                peer_key,
                started_at_ms,
                ended_at_ms: None,
                peak_tier: None,
                total_bytes: 0,
            });
        }
    }

    /// Seal the record for `connection_id` with its final `ended_at_ms`.
    ///
    /// No-op when `connection_id` is not found.  Returns `true` when the record
    /// was found and updated, `false` otherwise.
    pub fn close(&mut self, connection_id: u64, ended_at_ms: u64) -> bool {
        if let Some(i) = self.find_index(connection_id) {
            self.records[i].ended_at_ms = Some(ended_at_ms);
            true
        } else {
            false
        }
    }

    /// Overwrite the running byte total for `connection_id`.
    ///
    /// The caller (typically the governor) should pass a cumulative sum, not a
    /// per-tick delta.  No-op and returns `false` when `connection_id` is not
    /// found.
    pub fn update_bytes(&mut self, connection_id: u64, total_bytes: u64) -> bool {
        if let Some(i) = self.find_index(connection_id) {
            self.records[i].total_bytes = total_bytes;
            true
        } else {
            false
        }
    }

    /// Advance the peak tier for `connection_id`, never retreating.
    ///
    /// Sets `peak_tier` to `tier` only when `tier` is strictly higher than the
    /// current stored value (or when no value has been recorded yet).  A
    /// temporary congestion-driven downgrade therefore does not reduce the
    /// stored peak.  No-op and returns `false` when `connection_id` is not
    /// found.
    pub fn update_peak_tier(&mut self, connection_id: u64, tier: Tier) -> bool {
        if let Some(i) = self.find_index(connection_id) {
            let record = &mut self.records[i];
            let should_advance = record.peak_tier.map_or(true, |current| tier > current);
            if should_advance {
                record.peak_tier = Some(tier);
            }
            true
        } else {
            false
        }
    }

    /// Look up the record for `connection_id`.
    pub fn get(&self, connection_id: u64) -> Option<&SessionRecord> {
        self.find_index(connection_id).map(|i| &self.records[i])
    }

    /// All records in insertion order.
    pub fn records(&self) -> &[SessionRecord] {
        &self.records
    }

    /// Serialise all records to JSON for later export.
    ///
    /// Produces a JSON object with a `session_records` array.  Each entry uses
    /// the same field names as the SQLite schema.  `peer_key` is lowercase hex
    /// (64 chars) or `null`.  `ended_at_ms` and `peak_tier` are `null` while
    /// the session is still live.
    pub fn export_json(&self) -> String {
        let mut buf = String::from("{\"session_records\":[");
        for (i, r) in self.records.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&record_to_json(r));
        }
        buf.push_str("]}");
        buf
    }

    fn find_index(&self, connection_id: u64) -> Option<usize> {
        self.records.iter().position(|r| r.connection_id == connection_id)
    }
}

impl Default for SessionRecordStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── JSON serialisation ────────────────────────────────────────────────────────

fn record_to_json(r: &SessionRecord) -> String {
    let peer_key_field = match r.peer_key {
        Some(k) => {
            let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
            format!("\"{}\"", hex)
        }
        None => "null".to_string(),
    };
    let ended_field = match r.ended_at_ms {
        Some(ms) => ms.to_string(),
        None      => "null".to_string(),
    };
    let tier_field = match r.peak_tier {
        Some(t) => format!("\"{}\"", t.as_str()),
        None    => "null".to_string(),
    };
    format!(
        "{{\"connection_id\":{},\"peer_key\":{},\"started_at_ms\":{},\
         \"ended_at_ms\":{},\"peak_tier\":{},\"total_bytes\":{}}}",
        r.connection_id, peer_key_field, r.started_at_ms,
        ended_field, tier_field, r.total_bytes,
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tier ordering ─────────────────────────────────────────────────────────

    #[test]
    fn tier_ordering_survival_is_lowest() {
        assert!(Tier::Survival < Tier::Constrained);
        assert!(Tier::Survival < Tier::Comfortable);
        assert!(Tier::Survival < Tier::Full);
    }

    #[test]
    fn tier_ordering_full_is_highest() {
        assert!(Tier::Full > Tier::Comfortable);
        assert!(Tier::Full > Tier::Constrained);
        assert!(Tier::Full > Tier::Survival);
    }

    #[test]
    fn tier_ordering_constrained_between_survival_and_comfortable() {
        assert!(Tier::Constrained > Tier::Survival);
        assert!(Tier::Constrained < Tier::Comfortable);
    }

    #[test]
    fn tier_as_str_returns_expected_labels() {
        assert_eq!(Tier::Survival.as_str(),    "Survival");
        assert_eq!(Tier::Constrained.as_str(), "Constrained");
        assert_eq!(Tier::Comfortable.as_str(), "Comfortable");
        assert_eq!(Tier::Full.as_str(),        "Full");
    }

    // ── SessionRecordStore::open ──────────────────────────────────────────────

    #[test]
    fn open_inserts_a_new_record() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        assert_eq!(store.records().len(), 1);
        let r = store.get(1).unwrap();
        assert_eq!(r.connection_id, 1);
        assert_eq!(r.started_at_ms, 1_000);
        assert!(r.peer_key.is_none());
        assert!(r.ended_at_ms.is_none());
        assert!(r.peak_tier.is_none());
        assert_eq!(r.total_bytes, 0);
    }

    #[test]
    fn open_with_peer_key_stores_key() {
        let mut store = SessionRecordStore::new();
        let key = [0xab_u8; 32];
        store.open(7, Some(key), 500);
        assert_eq!(store.get(7).unwrap().peer_key, Some(key));
    }

    #[test]
    fn open_multiple_sessions_are_all_stored() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        store.open(2, None, 2_000);
        store.open(3, None, 3_000);
        assert_eq!(store.records().len(), 3);
    }

    #[test]
    fn open_duplicate_connection_id_is_ignored() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        store.open(1, Some([0xff_u8; 32]), 2_000); // duplicate — must not replace
        assert_eq!(store.records().len(), 1);
        assert_eq!(
            store.get(1).unwrap().started_at_ms,
            1_000,
            "original record must be preserved on duplicate open"
        );
    }

    // ── SessionRecordStore::close ─────────────────────────────────────────────

    #[test]
    fn close_sets_ended_at_ms() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        assert!(store.close(1, 61_000));
        assert_eq!(store.get(1).unwrap().ended_at_ms, Some(61_000));
    }

    #[test]
    fn close_unknown_connection_returns_false() {
        let mut store = SessionRecordStore::new();
        assert!(!store.close(99, 5_000));
    }

    #[test]
    fn record_ended_at_is_none_before_close() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        assert!(store.get(1).unwrap().ended_at_ms.is_none());
    }

    // ── SessionRecordStore::update_bytes ──────────────────────────────────────

    #[test]
    fn update_bytes_overwrites_running_total() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        assert!(store.update_bytes(1, 4_800));
        assert_eq!(store.get(1).unwrap().total_bytes, 4_800);
        assert!(store.update_bytes(1, 9_600));
        assert_eq!(store.get(1).unwrap().total_bytes, 9_600);
    }

    #[test]
    fn update_bytes_unknown_connection_returns_false() {
        let mut store = SessionRecordStore::new();
        assert!(!store.update_bytes(99, 1_000));
    }

    // ── SessionRecordStore::update_peak_tier ──────────────────────────────────

    #[test]
    fn update_peak_tier_sets_initial_tier() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        assert!(store.update_peak_tier(1, Tier::Constrained));
        assert_eq!(store.get(1).unwrap().peak_tier, Some(Tier::Constrained));
    }

    #[test]
    fn update_peak_tier_advances_to_higher_tier() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_peak_tier(1, Tier::Constrained);
        store.update_peak_tier(1, Tier::Comfortable);
        assert_eq!(store.get(1).unwrap().peak_tier, Some(Tier::Comfortable));
    }

    #[test]
    fn update_peak_tier_does_not_retreat_to_lower_tier() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_peak_tier(1, Tier::Full);
        store.update_peak_tier(1, Tier::Survival); // must be ignored
        assert_eq!(store.get(1).unwrap().peak_tier, Some(Tier::Full));
    }

    #[test]
    fn update_peak_tier_same_tier_is_idempotent() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_peak_tier(1, Tier::Constrained);
        store.update_peak_tier(1, Tier::Constrained);
        assert_eq!(store.get(1).unwrap().peak_tier, Some(Tier::Constrained));
    }

    #[test]
    fn update_peak_tier_unknown_connection_returns_false() {
        let mut store = SessionRecordStore::new();
        assert!(!store.update_peak_tier(99, Tier::Full));
    }

    // ── Multiple independent sessions ─────────────────────────────────────────

    #[test]
    fn multiple_sessions_are_updated_independently() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        store.open(2, None, 2_000);
        store.update_bytes(1, 1_000);
        store.update_bytes(2, 2_000);
        store.update_peak_tier(1, Tier::Survival);
        store.update_peak_tier(2, Tier::Full);
        store.close(1, 5_000);

        let r1 = store.get(1).unwrap();
        let r2 = store.get(2).unwrap();
        assert_eq!(r1.total_bytes, 1_000);
        assert_eq!(r2.total_bytes, 2_000);
        assert_eq!(r1.peak_tier, Some(Tier::Survival));
        assert_eq!(r2.peak_tier, Some(Tier::Full));
        assert_eq!(r1.ended_at_ms, Some(5_000));
        assert!(r2.ended_at_ms.is_none());
    }

    // ── export_json ───────────────────────────────────────────────────────────

    #[test]
    fn export_json_empty_store_returns_empty_array() {
        let store = SessionRecordStore::new();
        assert_eq!(store.export_json(), "{\"session_records\":[]}");
    }

    #[test]
    fn export_json_contains_connection_id() {
        let mut store = SessionRecordStore::new();
        store.open(42, None, 1_000);
        let json = store.export_json();
        assert!(json.contains("\"connection_id\":42"), "json: {json}");
    }

    #[test]
    fn export_json_null_peer_key_for_anonymous_session() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        let json = store.export_json();
        assert!(json.contains("\"peer_key\":null"), "json: {json}");
    }

    #[test]
    fn export_json_peer_key_encoded_as_lowercase_hex() {
        let mut store = SessionRecordStore::new();
        store.open(1, Some([0xab_u8; 32]), 0);
        let json = store.export_json();
        let expected_hex = "ab".repeat(32); // 32 bytes × 2 hex chars = 64-char string
        assert!(
            json.contains(&format!("\"peer_key\":\"{}\"", expected_hex)),
            "json: {json}"
        );
    }

    #[test]
    fn export_json_null_ended_for_live_session() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        let json = store.export_json();
        assert!(json.contains("\"ended_at_ms\":null"), "json: {json}");
    }

    #[test]
    fn export_json_ended_at_ms_after_close() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.close(1, 99_000);
        let json = store.export_json();
        assert!(json.contains("\"ended_at_ms\":99000"), "json: {json}");
    }

    #[test]
    fn export_json_null_peak_tier_before_first_governor_tick() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        let json = store.export_json();
        assert!(json.contains("\"peak_tier\":null"), "json: {json}");
    }

    #[test]
    fn export_json_peak_tier_string_after_update() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_peak_tier(1, Tier::Comfortable);
        let json = store.export_json();
        assert!(json.contains("\"peak_tier\":\"Comfortable\""), "json: {json}");
    }

    #[test]
    fn export_json_total_bytes_after_update() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_bytes(1, 15_000_000);
        let json = store.export_json();
        assert!(json.contains("\"total_bytes\":15000000"), "json: {json}");
    }

    #[test]
    fn export_json_contains_all_schema_fields() {
        let mut store = SessionRecordStore::new();
        store.open(1, Some([0x01_u8; 32]), 1_000);
        store.update_peak_tier(1, Tier::Full);
        store.update_bytes(1, 5_000_000);
        store.close(1, 61_000);
        let json = store.export_json();
        for field in &[
            "connection_id", "peer_key", "started_at_ms",
            "ended_at_ms", "peak_tier", "total_bytes",
        ] {
            assert!(json.contains(field), "json missing '{field}': {json}");
        }
    }

    #[test]
    fn export_json_multiple_records_are_all_present() {
        let mut store = SessionRecordStore::new();
        store.open(10, None, 100);
        store.open(20, None, 200);
        let json = store.export_json();
        assert!(json.contains("\"connection_id\":10"), "json: {json}");
        assert!(json.contains("\"connection_id\":20"), "json: {json}");
    }

    // ── Typical 30-minute constrained session (Feature 32 acceptance) ─────────

    #[test]
    fn constrained_30_min_session_persists_correctly() {
        let mut store = SessionRecordStore::new();
        // Session opens at t=0.
        store.open(99, None, 0);

        // Governor emits tier events; peak advances Survival → Constrained.
        store.update_peak_tier(99, Tier::Survival);
        store.update_peak_tier(99, Tier::Constrained);
        // Brief congestion dip back to Survival — peak must not retreat.
        store.update_peak_tier(99, Tier::Survival);

        // Accumulate ~15 MB over 30 minutes.
        store.update_bytes(99, 15_000_000);
        // Session ends at t = 30 min in ms.
        store.close(99, 30 * 60 * 1_000);

        let r = store.get(99).unwrap();
        assert_eq!(r.peak_tier,    Some(Tier::Constrained), "peak must not retreat on temporary dip");
        assert_eq!(r.total_bytes,  15_000_000);
        assert_eq!(r.ended_at_ms,  Some(1_800_000));

        let json = store.export_json();
        assert!(json.contains("\"peak_tier\":\"Constrained\""), "json: {json}");
        assert!(json.contains("\"total_bytes\":15000000"),      "json: {json}");
        assert!(json.contains("\"ended_at_ms\":1800000"),       "json: {json}");
    }
}

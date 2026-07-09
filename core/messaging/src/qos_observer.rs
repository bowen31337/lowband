//! Per-session QoS summary persistence — Feature 138.
//!
//! [`QosSessionObserver`] bridges governor events to the
//! [`SessionRecordStore`], accumulating quality metrics during a live
//! session and persisting them as each event arrives.  This ensures that
//! if a session ends uncleanly the store still holds the most recent
//! snapshot rather than an empty record.
//!
//! # Lifecycle
//!
//! ```text
//! observer.on_session_open(connection_id, peer_key, started_at_ms)
//!   ↓  (repeat at 10 Hz per governor tick)
//! observer.on_budget_tick(connection_id, …six stream bps allocations…)
//! observer.on_tier_event(connection_id, Tier::Constrained)
//!   ↓
//! observer.on_session_close(connection_id, ended_at_ms)
//! ```
//!
//! # Byte accounting
//!
//! Budget ticks arrive at 10 Hz (100 ms interval).  The byte estimate
//! per tick is `sum_of_stream_bps / 80` (÷ 8 bits, ÷ 10 Hz).  The
//! observer accumulates these into a running `total_bytes` and calls
//! [`SessionRecordStore::update_bytes`] after every tick so the record
//! is always current.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::qos_observer::QosSessionObserver;
//! use lowband_messaging::session_records::Tier;
//!
//! let mut obs = QosSessionObserver::new();
//!
//! obs.on_session_open(1, None, 0);
//! obs.on_tier_event(1, Tier::Constrained);
//! obs.on_budget_tick(1, 24_000, 8_000, 20_000, 12_000, 0, 0);
//! obs.on_session_close(1, 1_000);
//!
//! let record = obs.store().get(1).unwrap();
//! assert_eq!(record.peak_tier, Some(Tier::Constrained));
//! assert!(record.total_bytes > 0);
//! assert_eq!(record.ended_at_ms, Some(1_000));
//! ```

use crate::session_records::{SessionRecordStore, Tier};

/// Bridges governor events to the [`SessionRecordStore`].
///
/// Construct one observer per daemon instance.  Drive it with:
/// - [`on_session_open`](Self::on_session_open) when an LBTP session is
///   established.
/// - [`on_tier_event`](Self::on_tier_event) on each governor
///   `TierUpdate` event (10 Hz).
/// - [`on_budget_tick`](Self::on_budget_tick) on each governor
///   `StreamBudget` event (10 Hz).
/// - [`on_session_close`](Self::on_session_close) when the session ends.
///
/// Read results via [`store`](Self::store).
pub struct QosSessionObserver {
    store: SessionRecordStore,
}

impl QosSessionObserver {
    /// Create an observer with an empty session-record store.
    pub fn new() -> Self {
        Self { store: SessionRecordStore::new() }
    }

    /// Register a new session when an LBTP session is established.
    ///
    /// `started_at_ms` is wall-clock milliseconds since the Unix epoch,
    /// supplied by the caller so the observer remains testable without
    /// touching the system clock.  Duplicate `connection_id` values are
    /// silently ignored (same contract as
    /// [`SessionRecordStore::open`]).
    pub fn on_session_open(
        &mut self,
        connection_id: u64,
        peer_key: Option<[u8; 32]>,
        started_at_ms: u64,
    ) {
        self.store.open(connection_id, peer_key, started_at_ms);
    }

    /// Record a governor tier event, advancing the stored peak if higher.
    ///
    /// A temporary downgrade during congestion does **not** reduce the
    /// peak — only a strictly higher tier has any effect, matching
    /// [`SessionRecordStore::update_peak_tier`] semantics.
    pub fn on_tier_event(&mut self, connection_id: u64, tier: Tier) {
        self.store.update_peak_tier(connection_id, tier);
    }

    /// Accumulate the byte estimate for one 10 Hz governor `StreamBudget` tick.
    ///
    /// Each tick covers 100 ms, so bytes-per-tick = sum_of_bps / 80
    /// (÷ 8 bits, ÷ 10 ticks-per-second).  This value is added to the
    /// running `total_bytes` already in the store and written back via
    /// [`SessionRecordStore::update_bytes`].
    ///
    /// The six parameters mirror the six stream lanes in `StreamBudget`:
    /// `audio`, `input`, `screen_coarse`, `camera`, `screen_refinement`,
    /// and `xfer`.
    #[allow(clippy::too_many_arguments)]
    pub fn on_budget_tick(
        &mut self,
        connection_id: u64,
        audio_bps: u32,
        input_bps: u32,
        screen_coarse_bps: u32,
        camera_bps: u32,
        screen_refinement_bps: u32,
        xfer_bps: u32,
    ) {
        let total_bps = audio_bps as u64
            + input_bps as u64
            + screen_coarse_bps as u64
            + camera_bps as u64
            + screen_refinement_bps as u64
            + xfer_bps as u64;
        let bytes_this_tick = total_bps / 80;
        let current_total = self.store.get(connection_id).map_or(0, |r| r.total_bytes);
        self.store.update_bytes(connection_id, current_total + bytes_this_tick);
    }

    /// Seal the session record when the LBTP session ends.
    ///
    /// `ended_at_ms` is wall-clock milliseconds since the Unix epoch.
    /// After this call the record is complete and ready for JSON export
    /// via [`SessionRecordStore::export_json`].
    pub fn on_session_close(&mut self, connection_id: u64, ended_at_ms: u64) {
        self.store.close(connection_id, ended_at_ms);
    }

    /// Read-only access to the underlying [`SessionRecordStore`].
    pub fn store(&self) -> &SessionRecordStore {
        &self.store
    }
}

impl Default for QosSessionObserver {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── on_session_open ───────────────────────────────────────────────────────

    #[test]
    fn open_creates_record_with_correct_initial_fields() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(42, None, 5_000);
        let r = obs.store().get(42).unwrap();
        assert_eq!(r.connection_id, 42);
        assert_eq!(r.started_at_ms, 5_000);
        assert!(r.peer_key.is_none());
        assert!(r.ended_at_ms.is_none());
        assert!(r.peak_tier.is_none());
        assert_eq!(r.total_bytes, 0);
    }

    #[test]
    fn open_stores_peer_key() {
        let mut obs = QosSessionObserver::new();
        let key = [0xde_u8; 32];
        obs.on_session_open(1, Some(key), 0);
        assert_eq!(obs.store().get(1).unwrap().peer_key, Some(key));
    }

    #[test]
    fn open_duplicate_connection_id_is_ignored() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 1_000);
        obs.on_session_open(1, None, 9_999); // duplicate — must not overwrite
        assert_eq!(obs.store().get(1).unwrap().started_at_ms, 1_000);
    }

    // ── on_tier_event ─────────────────────────────────────────────────────────

    #[test]
    fn tier_event_sets_initial_peak() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_tier_event(1, Tier::Constrained);
        assert_eq!(obs.store().get(1).unwrap().peak_tier, Some(Tier::Constrained));
    }

    #[test]
    fn tier_event_advances_peak_to_higher_tier() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_tier_event(1, Tier::Constrained);
        obs.on_tier_event(1, Tier::Comfortable);
        assert_eq!(obs.store().get(1).unwrap().peak_tier, Some(Tier::Comfortable));
    }

    #[test]
    fn tier_event_does_not_retreat_on_temporary_downgrade() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_tier_event(1, Tier::Full);
        obs.on_tier_event(1, Tier::Survival); // congestion dip — must not change peak
        assert_eq!(obs.store().get(1).unwrap().peak_tier, Some(Tier::Full));
    }

    // ── on_budget_tick ────────────────────────────────────────────────────────

    #[test]
    fn budget_tick_64kbps_gives_800_bytes_per_tick() {
        // audio=24k + input=8k + screen_coarse=20k + camera=12k = 64_000 bps.
        // 64_000 / 80 = 800 bytes per 100 ms tick.
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_budget_tick(1, 24_000, 8_000, 20_000, 12_000, 0, 0);
        assert_eq!(obs.store().get(1).unwrap().total_bytes, 800);
    }

    #[test]
    fn budget_tick_accumulates_across_1800_ticks() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        for _ in 0..1_800 {
            obs.on_budget_tick(1, 24_000, 8_000, 20_000, 12_000, 0, 0);
        }
        assert_eq!(obs.store().get(1).unwrap().total_bytes, 800 * 1_800);
    }

    #[test]
    fn budget_tick_includes_screen_refinement_and_xfer() {
        // audio=24k + input=8k + screen_coarse=20k + camera=12k
        //   + refinement=8k + xfer=8k = 80_000 bps → 1_000 bytes/tick.
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_budget_tick(1, 24_000, 8_000, 20_000, 12_000, 8_000, 8_000);
        assert_eq!(obs.store().get(1).unwrap().total_bytes, 1_000);
    }

    #[test]
    fn zero_budget_tick_does_not_change_total_bytes() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_budget_tick(1, 0, 0, 0, 0, 0, 0);
        assert_eq!(obs.store().get(1).unwrap().total_bytes, 0);
    }

    // ── on_session_close ──────────────────────────────────────────────────────

    #[test]
    fn close_sets_ended_at_ms() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        obs.on_session_close(1, 61_000);
        assert_eq!(obs.store().get(1).unwrap().ended_at_ms, Some(61_000));
    }

    #[test]
    fn record_still_open_before_close() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 0);
        assert!(obs.store().get(1).unwrap().ended_at_ms.is_none());
    }

    // ── multiple concurrent sessions ──────────────────────────────────────────

    #[test]
    fn multiple_sessions_tracked_independently() {
        let mut obs = QosSessionObserver::new();
        obs.on_session_open(1, None, 1_000);
        obs.on_session_open(2, None, 2_000);

        obs.on_tier_event(1, Tier::Constrained);
        obs.on_tier_event(2, Tier::Full);

        // Session 1: 64 kbps → 800 bytes/tick
        obs.on_budget_tick(1, 24_000, 8_000, 20_000, 12_000, 0, 0);
        // Session 2: 100 kbps → 1_250 bytes/tick
        obs.on_budget_tick(2, 40_000, 10_000, 30_000, 20_000, 0, 0);

        obs.on_session_close(1, 5_000);

        let r1 = obs.store().get(1).unwrap();
        let r2 = obs.store().get(2).unwrap();

        assert_eq!(r1.peak_tier, Some(Tier::Constrained));
        assert_eq!(r2.peak_tier, Some(Tier::Full));
        assert_eq!(r1.total_bytes, 800);
        assert_eq!(r2.total_bytes, 1_250);
        assert_eq!(r1.ended_at_ms, Some(5_000));
        assert!(r2.ended_at_ms.is_none());
    }

    // ── Feature 138 acceptance: 30-min constrained session ───────────────────

    #[test]
    fn constrained_30_min_qos_summary_persists_correctly() {
        let mut obs = QosSessionObserver::new();

        // Session opens at t=0 ms.
        obs.on_session_open(99, None, 0);

        // Governor emits tier events; peak advances Survival → Constrained.
        obs.on_tier_event(99, Tier::Survival);
        obs.on_tier_event(99, Tier::Constrained);
        // Brief congestion dip — peak must not retreat.
        obs.on_tier_event(99, Tier::Survival);

        // 1 800 budget ticks (30 min at 10 Hz) at 64 kbps constrained split.
        for _ in 0..1_800 {
            obs.on_budget_tick(99, 24_000, 8_000, 20_000, 12_000, 0, 0);
        }

        // Session ends at t = 30 min.
        obs.on_session_close(99, 30 * 60 * 1_000);

        let r = obs.store().get(99).unwrap();
        assert_eq!(r.peak_tier,   Some(Tier::Constrained), "peak must not retreat on congestion dip");
        assert_eq!(r.total_bytes, 800 * 1_800);              // 1 440 000 bytes ≈ 1.44 MB
        assert_eq!(r.ended_at_ms, Some(1_800_000));

        let json = obs.store().export_json();
        assert!(json.contains("\"peak_tier\":\"Constrained\""), "json: {json}");
        assert!(json.contains("\"total_bytes\":1440000"),        "json: {json}");
        assert!(json.contains("\"ended_at_ms\":1800000"),        "json: {json}");
    }
}

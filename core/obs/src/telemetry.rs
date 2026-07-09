//! Aggregate QoS telemetry payload — Feature 137.
//!
//! [`QosTelemetryConfig`] gates sending behind an explicit opt-in flag.
//! [`QosTelemetryBatch`] holds the aggregate metrics built from the
//! [`SessionRecordStore`] and serialises to JSON.  The JSON payload
//! contains only numeric statistics; no audio, video, screen, or camera
//! data is ever included.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::session_records::SessionRecordStore;
//! use lowband_obs::telemetry::{QosTelemetryBatch, QosTelemetryConfig};
//!
//! let config = QosTelemetryConfig::new("http://telemetry.example.com/qos");
//! assert!(!config.enabled, "telemetry must be disabled by default");
//!
//! let config = config.with_opt_in();
//! assert!(config.enabled);
//!
//! let store = SessionRecordStore::new();
//! let batch = QosTelemetryBatch::from_store(&store);
//! let json = batch.to_json();
//! assert!(json.contains("\"session_count\":0"));
//! ```

use lowband_messaging::session_records::{SessionRecordStore, Tier};

/// Opt-in gate and endpoint configuration for QoS telemetry.
///
/// Construct with [`QosTelemetryConfig::new`]; telemetry is disabled by
/// default.  Call [`with_opt_in`](Self::with_opt_in) to enable it.
#[derive(Debug, Clone)]
pub struct QosTelemetryConfig {
    /// Whether the user has opted in to sending telemetry.
    ///
    /// `false` by default.  When `false`, [`crate::sender::send`] returns
    /// [`crate::sender::TelemetryError::Disabled`] immediately without
    /// opening any network connection.
    pub enabled: bool,
    /// HTTP endpoint that receives the POST request (e.g.
    /// `"http://telemetry.example.com/qos"`).
    pub endpoint: String,
}

impl QosTelemetryConfig {
    /// Create a config with telemetry **disabled** (the default).
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self { enabled: false, endpoint: endpoint.into() }
    }

    /// Enable telemetry (user opt-in).
    pub fn with_opt_in(mut self) -> Self {
        self.enabled = true;
        self
    }
}

/// Per-tier count of sessions within a [`QosTelemetryBatch`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TierCounts {
    /// Sessions whose peak tier was `Survival`.
    pub survival: u32,
    /// Sessions whose peak tier was `Constrained`.
    pub constrained: u32,
    /// Sessions whose peak tier was `Comfortable`.
    pub comfortable: u32,
    /// Sessions whose peak tier was `Full`.
    pub full: u32,
}

/// Aggregate-only QoS metrics batch — contains **no media content**.
///
/// Built from completed [`SessionRecord`](lowband_messaging::session_records::SessionRecord)
/// entries via [`QosTelemetryBatch::from_store`].  Serialised to JSON via
/// [`QosTelemetryBatch::to_json`]; the resulting object never contains
/// audio frames, video frames, screen captures, or camera images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QosTelemetryBatch {
    /// Number of completed sessions included in this batch.
    pub session_count: u32,
    /// Sum of `total_bytes` across all completed sessions.
    pub total_bytes_sum: u64,
    /// Sum of session durations (`ended_at_ms − started_at_ms`) in
    /// milliseconds.
    pub duration_sum_ms: u64,
    /// Number of sessions that reached each peak quality tier.
    pub peak_tier_counts: TierCounts,
}

impl QosTelemetryBatch {
    /// Build a batch from all **completed** sessions in `store`.
    ///
    /// Only sessions where `ended_at_ms` is `Some` contribute; live
    /// sessions whose byte totals are still accumulating are excluded.
    pub fn from_store(store: &SessionRecordStore) -> Self {
        let mut batch = Self {
            session_count: 0,
            total_bytes_sum: 0,
            duration_sum_ms: 0,
            peak_tier_counts: TierCounts::default(),
        };
        for record in store.records() {
            let Some(ended_at) = record.ended_at_ms else { continue };
            batch.session_count += 1;
            batch.total_bytes_sum += record.total_bytes;
            batch.duration_sum_ms += ended_at.saturating_sub(record.started_at_ms);
            match record.peak_tier {
                Some(Tier::Survival)    => batch.peak_tier_counts.survival    += 1,
                Some(Tier::Constrained) => batch.peak_tier_counts.constrained += 1,
                Some(Tier::Comfortable) => batch.peak_tier_counts.comfortable += 1,
                Some(Tier::Full)        => batch.peak_tier_counts.full        += 1,
                None => {}
            }
        }
        batch
    }

    /// Serialise the batch to a compact JSON string.
    ///
    /// The output schema is:
    /// ```json
    /// {
    ///   "schema_version": "1",
    ///   "session_count": <u32>,
    ///   "total_bytes_sum": <u64>,
    ///   "duration_sum_ms": <u64>,
    ///   "peak_tier_counts": {
    ///     "survival": <u32>,
    ///     "constrained": <u32>,
    ///     "comfortable": <u32>,
    ///     "full": <u32>
    ///   }
    /// }
    /// ```
    ///
    /// No audio, video, screen, or camera data appears in this output.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"schema_version\":\"1\",\"session_count\":{},\
             \"total_bytes_sum\":{},\"duration_sum_ms\":{},\
             \"peak_tier_counts\":{{\"survival\":{},\"constrained\":{},\
             \"comfortable\":{},\"full\":{}}}}}",
            self.session_count,
            self.total_bytes_sum,
            self.duration_sum_ms,
            self.peak_tier_counts.survival,
            self.peak_tier_counts.constrained,
            self.peak_tier_counts.comfortable,
            self.peak_tier_counts.full,
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_messaging::session_records::{SessionRecordStore, Tier};

    // ── QosTelemetryConfig ────────────────────────────────────────────────────

    #[test]
    fn config_is_disabled_by_default() {
        let cfg = QosTelemetryConfig::new("http://example.com/qos");
        assert!(!cfg.enabled, "telemetry must be opt-in — disabled by default");
    }

    #[test]
    fn config_with_opt_in_enables_telemetry() {
        let cfg = QosTelemetryConfig::new("http://example.com/qos").with_opt_in();
        assert!(cfg.enabled);
    }

    #[test]
    fn config_stores_endpoint() {
        let cfg = QosTelemetryConfig::new("http://telemetry.example.com:9000/v1/qos");
        assert_eq!(cfg.endpoint, "http://telemetry.example.com:9000/v1/qos");
    }

    // ── QosTelemetryBatch::from_store ─────────────────────────────────────────

    #[test]
    fn from_store_empty_store_gives_zero_counts() {
        let store = SessionRecordStore::new();
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count,   0);
        assert_eq!(batch.total_bytes_sum, 0);
        assert_eq!(batch.duration_sum_ms, 0);
        assert_eq!(batch.peak_tier_counts, TierCounts::default());
    }

    #[test]
    fn from_store_excludes_live_sessions() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        // Session 1 is still live (no close).
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count, 0, "live session must not appear in the batch");
    }

    #[test]
    fn from_store_includes_closed_sessions() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.close(1, 60_000);
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count, 1);
    }

    #[test]
    fn from_store_sums_total_bytes_across_sessions() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_bytes(1, 1_000_000);
        store.close(1, 60_000);
        store.open(2, None, 60_000);
        store.update_bytes(2, 2_000_000);
        store.close(2, 120_000);
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.total_bytes_sum, 3_000_000);
    }

    #[test]
    fn from_store_sums_duration_correctly() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 1_000);
        store.close(1, 61_000); // 60 000 ms
        store.open(2, None, 100_000);
        store.close(2, 280_000); // 180 000 ms
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.duration_sum_ms, 240_000);
    }

    #[test]
    fn from_store_counts_peak_tiers() {
        let mut store = SessionRecordStore::new();
        // Survival session
        store.open(1, None, 0);
        store.update_peak_tier(1, Tier::Survival);
        store.close(1, 10_000);
        // Two Constrained sessions
        store.open(2, None, 0);
        store.update_peak_tier(2, Tier::Constrained);
        store.close(2, 10_000);
        store.open(3, None, 0);
        store.update_peak_tier(3, Tier::Constrained);
        store.close(3, 10_000);
        // Full session
        store.open(4, None, 0);
        store.update_peak_tier(4, Tier::Full);
        store.close(4, 10_000);

        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.peak_tier_counts.survival,    1);
        assert_eq!(batch.peak_tier_counts.constrained, 2);
        assert_eq!(batch.peak_tier_counts.comfortable, 0);
        assert_eq!(batch.peak_tier_counts.full,        1);
    }

    #[test]
    fn from_store_session_with_no_tier_event_does_not_count_any_tier() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.close(1, 10_000);
        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count, 1);
        assert_eq!(batch.peak_tier_counts, TierCounts::default());
    }

    #[test]
    fn from_store_mixed_live_and_closed_sessions() {
        let mut store = SessionRecordStore::new();
        store.open(1, None, 0);
        store.update_bytes(1, 500_000);
        store.close(1, 30_000);
        store.open(2, None, 30_000); // still live
        store.update_bytes(2, 999_999);

        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count,   1, "only closed sessions counted");
        assert_eq!(batch.total_bytes_sum, 500_000, "live session bytes excluded");
    }

    // ── QosTelemetryBatch::to_json ────────────────────────────────────────────

    #[test]
    fn to_json_contains_required_aggregate_fields() {
        let batch = QosTelemetryBatch {
            session_count: 3,
            total_bytes_sum: 5_000_000,
            duration_sum_ms: 300_000,
            peak_tier_counts: TierCounts { survival: 0, constrained: 2, comfortable: 1, full: 0 },
        };
        let json = batch.to_json();
        assert!(json.contains("\"schema_version\":\"1\""), "json: {json}");
        assert!(json.contains("\"session_count\":3"),       "json: {json}");
        assert!(json.contains("\"total_bytes_sum\":5000000"), "json: {json}");
        assert!(json.contains("\"duration_sum_ms\":300000"),  "json: {json}");
        assert!(json.contains("\"peak_tier_counts\""),       "json: {json}");
        assert!(json.contains("\"constrained\":2"),          "json: {json}");
        assert!(json.contains("\"comfortable\":1"),          "json: {json}");
    }

    #[test]
    fn to_json_contains_no_media_content_fields() {
        let batch = QosTelemetryBatch {
            session_count: 1,
            total_bytes_sum: 1_000,
            duration_sum_ms: 60_000,
            peak_tier_counts: TierCounts::default(),
        };
        let json = batch.to_json();
        // None of these media-content keywords must appear.
        for forbidden in &["audio_frame", "video_frame", "screen_capture", "camera_image",
                           "pixel", "encoded_frame", "raw_audio", "pcm"] {
            assert!(
                !json.contains(forbidden),
                "telemetry payload must not contain media content (found '{forbidden}'): {json}"
            );
        }
    }

    #[test]
    fn to_json_peer_key_absent_confirming_anonymisation() {
        let batch = QosTelemetryBatch {
            session_count: 1,
            total_bytes_sum: 1_000,
            duration_sum_ms: 60_000,
            peak_tier_counts: TierCounts::default(),
        };
        let json = batch.to_json();
        assert!(!json.contains("peer_key"),      "peer_key must not appear in telemetry");
        assert!(!json.contains("connection_id"), "connection_id must not appear in telemetry");
    }

    #[test]
    fn to_json_roundtrip_zero_batch() {
        let batch = QosTelemetryBatch {
            session_count: 0,
            total_bytes_sum: 0,
            duration_sum_ms: 0,
            peak_tier_counts: TierCounts::default(),
        };
        let json = batch.to_json();
        assert!(json.contains("\"session_count\":0"), "json: {json}");
        assert!(json.contains("\"total_bytes_sum\":0"), "json: {json}");
    }

    // ── Feature 137 acceptance: 30-min constrained session ───────────────────

    #[test]
    fn constrained_30_min_session_produces_correct_batch() {
        let mut store = SessionRecordStore::new();
        store.open(99, None, 0);
        store.update_peak_tier(99, Tier::Constrained);
        // 1 440 000 bytes ≈ 1.44 MB (800 bytes × 1 800 ticks).
        store.update_bytes(99, 1_440_000);
        store.close(99, 1_800_000); // 30 min in ms

        let batch = QosTelemetryBatch::from_store(&store);
        assert_eq!(batch.session_count,                  1);
        assert_eq!(batch.total_bytes_sum,                1_440_000);
        assert_eq!(batch.duration_sum_ms,                1_800_000);
        assert_eq!(batch.peak_tier_counts.constrained,   1);
        assert_eq!(batch.peak_tier_counts.survival,      0);
        assert_eq!(batch.peak_tier_counts.comfortable,   0);
        assert_eq!(batch.peak_tier_counts.full,          0);

        let json = batch.to_json();
        assert!(json.contains("\"session_count\":1"),            "json: {json}");
        assert!(json.contains("\"total_bytes_sum\":1440000"),    "json: {json}");
        assert!(json.contains("\"duration_sum_ms\":1800000"),    "json: {json}");
        assert!(json.contains("\"constrained\":1"),              "json: {json}");
    }
}

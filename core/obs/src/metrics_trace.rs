//! Quality metrics time-series trace — Feature 132.
//!
//! [`MetricsTrace`] records a bounded ring-buffer of [`MetricsSample`]
//! observations emitted by the governor at 10 Hz.  This is the **traces**
//! component of the observability umbrella (Feature 132).  The complementary
//! components emitted from this module are:
//!
//! - **Metrics**: live quality-bar values via
//!   [`quality_bar_lag`](super::quality_bar_lag) (Feature 133) and
//!   [`quality_indicator`](super::quality_indicator) (Feature 134).
//! - **QoE probes**: per-frame perceptual quality via
//!   [`vmaf_sample`](super::vmaf_sample) (Feature 135) and
//!   [`ocr_probe`](super::ocr_probe) (Feature 136).
//!
//! # Ring-buffer design
//!
//! The governor ticks at 10 Hz.  A 30-minute session produces 18 000 samples.
//! The default capacity ([`METRICS_TRACE_CAPACITY`] = 1 800) holds the last
//! 3 minutes of quality history — enough to capture a full tier-transition
//! diagnostic window while keeping heap use below 100 KB per session.
//!
//! When full, the **oldest** sample is evicted so the trace always represents
//! the most recent observation window.
//!
//! # Usage
//!
//! ```
//! use lowband_obs::metrics_trace::{MetricsSample, MetricsTrace};
//! use lowband_platform::TierState;
//!
//! let mut trace = MetricsTrace::new();
//!
//! // On each governor tick (10 Hz):
//! trace.record(MetricsSample {
//!     session_ms: 0,
//!     tier:       TierState::Constrained,
//!     total_kbps: 64,
//!     rtt_ms:     85,
//!     loss_pct:   1.5,
//! });
//!
//! assert_eq!(trace.len(), 1);
//! assert_eq!(trace.last().unwrap().tier, TierState::Constrained);
//! ```

use std::collections::VecDeque;

use lowband_platform::TierState;

/// Default capacity of a [`MetricsTrace`] ring buffer.
///
/// 1 800 samples = 3 minutes at 10 Hz.  Each [`MetricsSample`] is ≤ 32 bytes,
/// so the default trace occupies at most ~57 KB — acceptable for a
/// constrained-tier endpoint.
pub const METRICS_TRACE_CAPACITY: usize = 1_800;

/// A single time-stamped quality-metrics observation from one governor tick.
///
/// The fields mirror the four quality-bar fields tracked by
/// [`QualityBarLag`](super::quality_bar_lag::QualityBarLag) with the addition
/// of a session-relative timestamp for time-series analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsSample {
    /// Milliseconds elapsed since session open.
    ///
    /// Supplied by the caller so the trace is testable without touching the
    /// system clock.
    pub session_ms: u64,
    /// Quality tier emitted by the governor for this tick.
    pub tier: TierState,
    /// Total outbound bitrate across all streams, in kbps (floor-divided from bps).
    pub total_kbps: u32,
    /// Round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// Packet-loss percentage in `[0.0, 100.0]`.
    pub loss_pct: f32,
}

/// Bounded ring-buffer trace of quality metrics emitted by the governor.
///
/// Construct one per session.  Push governor ticks with [`record`](Self::record).
/// The most recent [`capacity`](Self::capacity) samples are available in
/// chronological order via [`samples`](Self::samples).
///
/// When the buffer fills, the **oldest** sample is evicted to make room — the
/// trace always holds the most recent observation window.
pub struct MetricsTrace {
    capacity: usize,
    samples: VecDeque<MetricsSample>,
}

impl MetricsTrace {
    /// Create a trace with the default capacity ([`METRICS_TRACE_CAPACITY`]).
    pub fn new() -> Self {
        Self::with_capacity(METRICS_TRACE_CAPACITY)
    }

    /// Create a trace with a custom capacity.
    ///
    /// A capacity of `0` is valid; [`record`](Self::record) is then a no-op.
    pub fn with_capacity(capacity: usize) -> Self {
        let initial = capacity.min(64); // avoid large pre-allocation in tests
        Self { capacity, samples: VecDeque::with_capacity(initial) }
    }

    /// Record one governor quality-metrics sample.
    ///
    /// If the trace is at capacity, the oldest sample is evicted first.
    /// When capacity is `0` this is a no-op.
    pub fn record(&mut self, sample: MetricsSample) {
        if self.capacity == 0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Iterate over all samples in chronological order (oldest first).
    pub fn samples(&self) -> impl Iterator<Item = &MetricsSample> {
        self.samples.iter()
    }

    /// Number of samples currently in the trace.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// `true` when the trace holds no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Maximum number of samples before the oldest is evicted.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The most recently recorded sample, or `None` if the trace is empty.
    pub fn last(&self) -> Option<&MetricsSample> {
        self.samples.back()
    }
}

impl Default for MetricsTrace {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(session_ms: u64, tier: TierState, total_kbps: u32) -> MetricsSample {
        MetricsSample { session_ms, tier, total_kbps, rtt_ms: 80, loss_pct: 0.0 }
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_trace_is_empty() {
        let trace = MetricsTrace::new();
        assert!(trace.is_empty());
        assert_eq!(trace.len(), 0);
        assert!(trace.last().is_none());
    }

    #[test]
    fn default_capacity_is_metrics_trace_capacity() {
        assert_eq!(MetricsTrace::new().capacity(), METRICS_TRACE_CAPACITY);
    }

    #[test]
    fn with_capacity_sets_custom_capacity() {
        let trace = MetricsTrace::with_capacity(10);
        assert_eq!(trace.capacity(), 10);
    }

    #[test]
    fn default_and_new_are_equivalent() {
        let a = MetricsTrace::new();
        let b = MetricsTrace::default();
        assert_eq!(a.capacity(), b.capacity());
        assert_eq!(a.len(), b.len());
    }

    // ── record ────────────────────────────────────────────────────────────────

    #[test]
    fn record_one_sample_len_becomes_one() {
        let mut trace = MetricsTrace::new();
        trace.record(sample(0, TierState::Constrained, 64));
        assert_eq!(trace.len(), 1);
        assert!(!trace.is_empty());
    }

    #[test]
    fn record_two_samples_len_is_two() {
        let mut trace = MetricsTrace::new();
        trace.record(sample(0,   TierState::Constrained, 64));
        trace.record(sample(100, TierState::Comfortable, 128));
        assert_eq!(trace.len(), 2);
    }

    #[test]
    fn last_returns_most_recently_recorded_sample() {
        let mut trace = MetricsTrace::new();
        trace.record(sample(0,   TierState::Survival,    48));
        trace.record(sample(100, TierState::Constrained, 64));
        let last = trace.last().unwrap();
        assert_eq!(last.session_ms, 100);
        assert_eq!(last.tier, TierState::Constrained);
        assert_eq!(last.total_kbps, 64);
    }

    #[test]
    fn samples_iterator_is_chronological() {
        let mut trace = MetricsTrace::new();
        for ms in [0u64, 100, 200, 300] {
            trace.record(sample(ms, TierState::Comfortable, 128));
        }
        let times: Vec<u64> = trace.samples().map(|s| s.session_ms).collect();
        assert_eq!(times, vec![0, 100, 200, 300]);
    }

    // ── Ring-buffer eviction ──────────────────────────────────────────────────

    #[test]
    fn capacity_two_evicts_oldest_on_third_record() {
        let mut trace = MetricsTrace::with_capacity(2);
        trace.record(sample(0,   TierState::Survival,    48));
        trace.record(sample(100, TierState::Constrained, 64));
        trace.record(sample(200, TierState::Comfortable, 128)); // evicts t=0

        assert_eq!(trace.len(), 2);
        let times: Vec<u64> = trace.samples().map(|s| s.session_ms).collect();
        assert_eq!(times, vec![100, 200], "oldest must be evicted first");
    }

    #[test]
    fn capacity_one_always_holds_the_newest_sample() {
        let mut trace = MetricsTrace::with_capacity(1);
        trace.record(sample(0,   TierState::Survival,    48));
        trace.record(sample(100, TierState::Constrained, 64));
        trace.record(sample(200, TierState::Full,        200));
        assert_eq!(trace.len(), 1);
        assert_eq!(trace.last().unwrap().session_ms, 200);
        assert_eq!(trace.last().unwrap().tier, TierState::Full);
    }

    #[test]
    fn capacity_zero_record_is_noop() {
        let mut trace = MetricsTrace::with_capacity(0);
        trace.record(sample(0, TierState::Full, 200));
        assert!(trace.is_empty());
    }

    #[test]
    fn trace_never_exceeds_capacity() {
        let cap = 5;
        let mut trace = MetricsTrace::with_capacity(cap);
        for i in 0..20u64 {
            trace.record(sample(i * 100, TierState::Comfortable, 128));
        }
        assert_eq!(trace.len(), cap, "trace must not grow beyond capacity");
    }

    // ── MetricsSample fields ──────────────────────────────────────────────────

    #[test]
    fn sample_fields_are_recorded_verbatim() {
        let s = MetricsSample {
            session_ms: 5_000,
            tier:       TierState::Full,
            total_kbps: 300,
            rtt_ms:     12,
            loss_pct:   0.5,
        };
        let mut trace = MetricsTrace::new();
        trace.record(s.clone());
        let got = trace.last().unwrap();
        assert_eq!(got.session_ms,  5_000);
        assert_eq!(got.tier,        TierState::Full);
        assert_eq!(got.total_kbps,  300);
        assert_eq!(got.rtt_ms,      12);
        assert_eq!(got.loss_pct.to_bits(), 0.5f32.to_bits());
    }

    // ── Feature 132 acceptance: 30-minute session trace ──────────────────────

    #[test]
    fn feature_132_thirty_minute_session_trace_records_tier_transitions() {
        // Simulate a 30-minute (1 800 s) session at 10 Hz = 18 000 ticks.
        // The trace capacity (1 800) holds the last 3 minutes.
        // Tier progression: Constrained for the first 9 000 ticks, then
        // Comfortable for the remaining 9 000 ticks.

        let mut trace = MetricsTrace::new();
        assert_eq!(trace.capacity(), METRICS_TRACE_CAPACITY);

        for i in 0..18_000u64 {
            let tier = if i < 9_000 { TierState::Constrained } else { TierState::Comfortable };
            let kbps = if i < 9_000 { 64u32 } else { 128u32 };
            trace.record(MetricsSample {
                session_ms: i * 100, // 100 ms per tick
                tier,
                total_kbps: kbps,
                rtt_ms: 80,
                loss_pct: 0.0,
            });
        }

        // The ring buffer holds only the last 1 800 samples (ticks 16 200–17 999).
        assert_eq!(trace.len(), METRICS_TRACE_CAPACITY);

        // All retained samples are from the Comfortable tier (ticks 9 000+).
        let first = trace.samples().next().unwrap();
        assert_eq!(
            first.tier, TierState::Comfortable,
            "oldest retained sample must be from the Comfortable tier (ticks 9000–17999)"
        );
        assert_eq!(
            first.session_ms,
            (18_000 - METRICS_TRACE_CAPACITY as u64) * 100,
            "oldest retained sample must be tick {}", 18_000 - METRICS_TRACE_CAPACITY
        );

        // The most recent sample is the last tick of the session.
        let last = trace.last().unwrap();
        assert_eq!(last.session_ms, 17_999 * 100);
        assert_eq!(last.tier, TierState::Comfortable);
        assert_eq!(last.total_kbps, 128);
    }

    #[test]
    fn feature_132_samples_are_enumerable_for_tier_histogram() {
        // Verify that the samples() iterator can be used to compute a
        // per-tier histogram — the primary analytical use case.
        let mut trace = MetricsTrace::with_capacity(6);
        for _ in 0..3 { trace.record(sample(0, TierState::Constrained, 64)); }
        for _ in 0..2 { trace.record(sample(0, TierState::Comfortable, 128)); }
        trace.record(sample(0, TierState::Full, 200));

        let (mut constrained, mut comfortable, mut full) = (0u32, 0u32, 0u32);
        for s in trace.samples() {
            match s.tier {
                TierState::Constrained => constrained += 1,
                TierState::Comfortable => comfortable += 1,
                TierState::Full        => full        += 1,
                TierState::Survival    => {}
            }
        }
        assert_eq!(constrained, 3);
        assert_eq!(comfortable, 2);
        assert_eq!(full,        1);
    }
}

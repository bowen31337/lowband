//! NetEQ-style adaptive jitter buffer — Features 55 and 56.
//!
//! # Delay-distribution tracking (Feature 55)
//!
//! [`DelayHistogram`] records the inter-arrival delay variation of incoming
//! packets as a histogram over frame-unit buckets.  Each new observation
//! decays all existing buckets by [`HISTOGRAM_FORGET_FACTOR`] before adding
//! weight to the measured bucket, so recent network behaviour dominates.
//!
//! [`JitterBuffer`] calls [`JitterBuffer::observe_arrival_delay`] on every
//! received packet and updates its adaptive `target_level` to the 95th
//! percentile ([`TARGET_PERCENTILE`]) of the distribution.  This means the
//! buffer depth tracks actual jitter rather than a static guess: it grows
//! during a bursty period and shrinks when the path stabilises.
//!
//! # Playout convergence (Feature 56)
//!
//! When the jitter buffer diverges from its target level, the system
//! corrects by time-scaling the audio stream rather than inserting or
//! discarding frames, which would create audible gaps.
//!
//! | Buffer state                         | Action                              |
//! |--------------------------------------|-------------------------------------|
//! | `level > target + convergence_zone`  | `Accelerate` — drain faster         |
//! | `level < target − convergence_zone`  | `Decelerate` — fill faster          |
//! | within convergence zone              | `Normal` — no adjustment            |
//! | `level == 0`                         | `Conceal` — PLC, never a gap        |
//!
//! The scaling factor is proportional to the normalised deviation from the
//! target level, clamped to [`MAX_TIME_SCALE_RATE`] (15 %).  A larger deviation
//! produces a faster correction but never exceeds the perceptual quality
//! floor for speech intelligibility.
//!
//! # 15 % ceiling
//!
//! WSOLA and related time-scaling algorithms preserve speech intelligibility
//! up to roughly 20 % acceleration/deceleration.  The 15 % ceiling keeps the
//! system well inside this bound while still converging within a few hundred
//! milliseconds at a typical 20 ms frame rate.
//!
//! # Never silence
//!
//! The buffer returns [`PlayoutAction::Conceal`] — not silence — when empty.
//! The caller must apply the PLC chain (Feature 57) to generate a plausible
//! frame rather than emitting zeros.

// ── Delay-histogram constants ─────────────────────────────────────────────────

/// Number of buckets in the delay distribution histogram.
///
/// Each bucket i represents a delay of i frames (20 ms each), covering the
/// range 0–99 frames (0–1.98 s).  Delays exceeding this range are clamped
/// to bucket 99.
pub const HISTOGRAM_BUCKETS: usize = 100;

/// Per-observation exponential decay factor applied to all histogram buckets.
///
/// Before recording a new observation, every bucket is multiplied by this
/// factor so that older measurements contribute less weight over time.
/// The value 0.9931 yields a half-life of roughly 100 observations
/// (≈ 2 s at one packet per 20 ms frame), making the distribution
/// responsive to changing network conditions while staying stable under
/// steady-state jitter.
pub const HISTOGRAM_FORGET_FACTOR: f64 = 0.9931;

/// Percentile of the delay distribution that sets the adaptive target level.
///
/// The 95th percentile accommodates the vast majority of observed delays
/// while ignoring extreme outliers, so the buffer stalls on fewer than
/// 5 % of arriving packets in steady state.
pub const TARGET_PERCENTILE: f64 = 0.95;

// ── Playout-convergence constants ─────────────────────────────────────────────

/// Maximum time-scaling rate for playout convergence.
///
/// Audio is accelerated or decelerated by at most this fraction of the
/// nominal playback rate.  Effective rate lies in `[0.85, 1.15]`.
pub const MAX_TIME_SCALE_RATE: f64 = 0.15;

/// Frames within which the buffer is considered "at target" and no
/// time-scaling adjustment is applied.
///
/// One frame (20 ms) of dead band avoids perpetual micro-adjustments when
/// the buffer level oscillates around the target by a single frame.
pub const CONVERGENCE_ZONE_FRAMES: usize = 1;

// ── DelayHistogram ────────────────────────────────────────────────────────────

/// Histogram of observed inter-arrival delay variations, measured in 20 ms frames.
///
/// Tracks the empirical distribution of how many frames of extra delay each
/// incoming packet experienced relative to the nominal arrival cadence.  All
/// buckets are decayed by [`HISTOGRAM_FORGET_FACTOR`] on each observation so
/// that stale measurements fade out and recent behaviour dominates.
///
/// In steady state (many observations) the bucket weights sum to ≈ 1.0.
/// Before that, the total is < 1.0 and percentile queries are computed
/// against the actual sum, so the estimate is valid from the very first
/// observation.
///
/// # Usage
///
/// ```rust,no_run
/// use lowband_platform::jitter_buffer::{DelayHistogram, TARGET_PERCENTILE};
///
/// let mut hist = DelayHistogram::new();
/// hist.observe(3);   // packet was 3 frames late
/// let p95 = hist.percentile_frames(TARGET_PERCENTILE);
/// ```
#[derive(Debug, Clone)]
pub struct DelayHistogram {
    buckets: [f64; HISTOGRAM_BUCKETS],
}

impl Default for DelayHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayHistogram {
    /// Create an empty histogram with all buckets zeroed.
    pub fn new() -> Self {
        Self {
            buckets: [0.0; HISTOGRAM_BUCKETS],
        }
    }

    /// Record one observed inter-arrival delay and update the distribution.
    ///
    /// `delay_frames` is the measured extra delay for this packet in 20 ms
    /// frames.  Values exceeding `HISTOGRAM_BUCKETS − 1` are clamped to the
    /// last bucket.
    pub fn observe(&mut self, delay_frames: usize) {
        for b in self.buckets.iter_mut() {
            *b *= HISTOGRAM_FORGET_FACTOR;
        }
        let bucket = delay_frames.min(HISTOGRAM_BUCKETS - 1);
        self.buckets[bucket] += 1.0 - HISTOGRAM_FORGET_FACTOR;
    }

    /// Return the delay (in frames) at the given percentile of the distribution.
    ///
    /// Returns `0` when no observations have been made.
    pub fn percentile_frames(&self, percentile: f64) -> usize {
        let total: f64 = self.buckets.iter().sum();
        if total <= 0.0 {
            return 0;
        }
        let threshold = total * percentile.clamp(0.0, 1.0);
        let mut cumulative = 0.0;
        for (i, &w) in self.buckets.iter().enumerate() {
            cumulative += w;
            if cumulative >= threshold {
                return i;
            }
        }
        HISTOGRAM_BUCKETS - 1
    }

    /// Total accumulated weight across all buckets.
    ///
    /// Rises from 0 toward 1.0 as observations accumulate.
    pub fn total_weight(&self) -> f64 {
        self.buckets.iter().sum()
    }
}

// ── PlayoutAction ─────────────────────────────────────────────────────────────

/// Action the caller should apply to the next decoded audio frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayoutAction {
    /// Play at 1.0× (nominal rate) — buffer is within the convergence zone.
    Normal,
    /// Time-scale to play faster, draining an overfilled buffer.
    ///
    /// The effective playback rate is `1.0 + factor`.  `factor` is in
    /// `(0.0, MAX_TIME_SCALE_RATE]`.  The caller passes this factor to a
    /// WSOLA or OLA time-stretcher to produce the correct output duration.
    Accelerate(f64),
    /// Time-scale to play slower, filling an underfilled buffer.
    ///
    /// The effective playback rate is `1.0 - factor`.  `factor` is in
    /// `(0.0, MAX_TIME_SCALE_RATE]`.
    Decelerate(f64),
    /// Buffer is empty — apply PLC (Feature 57) instead of silence.
    Conceal,
}

/// NetEQ-style adaptive jitter buffer with delay-distribution tracking and
/// time-scaling playout convergence.
///
/// The buffer tracks frames enqueued and dequeued.  Each [`JitterBuffer::tick`]
/// call returns a [`PlayoutAction`] telling the caller how to schedule the
/// next output frame so the buffer converges to [`JitterBuffer::target_level`]
/// without audible gaps.
///
/// The target level is adaptive: call [`JitterBuffer::observe_arrival_delay`]
/// for each received packet with the measured inter-arrival delay in frames.
/// The buffer updates its internal [`DelayHistogram`] and raises or lowers the
/// target to the 95th percentile of observed delays (but never below the
/// `min_target_level` supplied at construction).
#[derive(Debug, Clone)]
pub struct JitterBuffer {
    /// Current adaptive target buffer depth (in frames).
    target_level: usize,
    /// Floor below which the adaptive target will not drop.
    min_target_level: usize,
    level: usize,
    convergence_zone: usize,
    histogram: DelayHistogram,
}

impl JitterBuffer {
    /// Create a new jitter buffer with the given target level.
    ///
    /// `target_level_frames` is the initial and minimum buffer depth in 20 ms
    /// frames (e.g. 4 frames = 80 ms at the constrained tier).  The adaptive
    /// target will never drop below this floor.
    /// `convergence_zone_frames` is the dead band; use [`CONVERGENCE_ZONE_FRAMES`].
    pub fn new(target_level_frames: usize, convergence_zone_frames: usize) -> Self {
        Self {
            target_level: target_level_frames,
            min_target_level: target_level_frames,
            level: 0,
            convergence_zone: convergence_zone_frames,
            histogram: DelayHistogram::new(),
        }
    }

    /// Return the current buffer level in frames.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Observe an incoming packet's inter-arrival delay and adapt the target level.
    ///
    /// `delay_frames` is the measured delay variation for this packet in 20 ms
    /// frames — typically derived as `(actual_arrival_tick − expected_arrival_tick)`
    /// on the receiver's playout clock.
    ///
    /// The internal [`DelayHistogram`] is updated and the adaptive target level
    /// is set to the 95th percentile of the distribution, floored at
    /// `min_target_level`.
    ///
    /// Call this once per arriving packet, before [`JitterBuffer::enqueue`].
    pub fn observe_arrival_delay(&mut self, delay_frames: usize) {
        self.histogram.observe(delay_frames);
        let p95 = self.histogram.percentile_frames(TARGET_PERCENTILE);
        self.target_level = p95.max(self.min_target_level);
    }

    /// Read-only access to the underlying delay distribution histogram.
    pub fn delay_histogram(&self) -> &DelayHistogram {
        &self.histogram
    }

    /// Enqueue a received frame into the buffer.
    ///
    /// Should be called for every frame the transport delivers, before
    /// [`JitterBuffer::tick`].
    pub fn enqueue(&mut self) {
        self.level += 1;
    }

    /// Dequeue one frame worth of playout and return the required action.
    ///
    /// Call once per 20 ms playout tick.  The returned [`PlayoutAction`]
    /// tells the caller:
    /// - how fast to play the dequeued frame, or
    /// - that the buffer is empty and PLC should be applied.
    ///
    /// When `Conceal` is returned, no frame is dequeued — the buffer remains
    /// empty and the caller must synthesise audio via the PLC chain.
    pub fn tick(&mut self) -> PlayoutAction {
        if self.level == 0 {
            return PlayoutAction::Conceal;
        }

        // Consume one frame.
        self.level -= 1;

        let deviation =
            (self.level as isize) - (self.target_level as isize);
        let abs_dev = deviation.unsigned_abs();

        if abs_dev <= self.convergence_zone {
            return PlayoutAction::Normal;
        }

        // Scale factor proportional to deviation, clamped to the 15 % ceiling.
        // Use target_level as the normalisation denominator; fall back to abs_dev
        // itself when target is zero to avoid division by zero.
        let denom = self.target_level.max(1) as f64;
        let raw_factor = (abs_dev as f64) / denom;
        let factor = raw_factor.min(MAX_TIME_SCALE_RATE);

        if deviation > 0 {
            // Buffer above target — accelerate to drain.
            PlayoutAction::Accelerate(factor)
        } else {
            // Buffer below target — decelerate to fill.
            PlayoutAction::Decelerate(factor)
        }
    }

    /// Target buffer level in frames.
    pub fn target_level(&self) -> usize {
        self.target_level
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_returns_conceal_not_silence() {
        let mut jb = JitterBuffer::new(4, CONVERGENCE_ZONE_FRAMES);
        assert_eq!(jb.tick(), PlayoutAction::Conceal);
    }

    #[test]
    fn buffer_at_target_returns_normal() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Fill to exactly target.
        for _ in 0..target {
            jb.enqueue();
        }
        // After dequeue, level = target - 1 which is within convergence_zone (1).
        // So Normal is expected.
        assert_eq!(jb.tick(), PlayoutAction::Normal);
    }

    #[test]
    fn overfilled_buffer_returns_accelerate() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Fill well above target.
        for _ in 0..(target * 2 + 4) {
            jb.enqueue();
        }
        let action = jb.tick();
        assert!(
            matches!(action, PlayoutAction::Accelerate(_)),
            "overfilled buffer must produce Accelerate, got {action:?}"
        );
    }

    #[test]
    fn underfilled_buffer_returns_decelerate() {
        let target = 8usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Enqueue only 2 frames — well below target of 8.
        jb.enqueue();
        jb.enqueue();
        let action = jb.tick();
        assert!(
            matches!(action, PlayoutAction::Decelerate(_)),
            "underfilled buffer must produce Decelerate, got {action:?}"
        );
    }

    #[test]
    fn acceleration_factor_is_capped_at_15_pct() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Load an extreme excess so the raw factor would exceed 15 %.
        for _ in 0..200 {
            jb.enqueue();
        }
        for _ in 0..100 {
            if let PlayoutAction::Accelerate(f) = jb.tick() {
                assert!(
                    f <= MAX_TIME_SCALE_RATE + 1e-9,
                    "Accelerate factor {f:.4} exceeds MAX_TIME_SCALE_RATE {MAX_TIME_SCALE_RATE}"
                );
            }
        }
    }

    #[test]
    fn deceleration_factor_is_capped_at_15_pct() {
        let target = 80usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Enqueue only 1 frame — maximum deviation below target.
        jb.enqueue();
        if let PlayoutAction::Decelerate(f) = jb.tick() {
            assert!(
                f <= MAX_TIME_SCALE_RATE + 1e-9,
                "Decelerate factor {f:.4} exceeds MAX_TIME_SCALE_RATE {MAX_TIME_SCALE_RATE}"
            );
        }
    }

    #[test]
    fn buffer_converges_from_excess_using_only_time_scaling() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Simulate a burst arrival: fill to 3× target.
        for _ in 0..(target * 3) {
            jb.enqueue();
        }

        let mut gaps = 0usize;
        let mut max_ticks = 500usize;

        // Drive the buffer with no new arrivals to simulate the drain.
        while jb.level() > target + CONVERGENCE_ZONE_FRAMES && max_ticks > 0 {
            match jb.tick() {
                PlayoutAction::Conceal => gaps += 1,
                PlayoutAction::Accelerate(f) => {
                    assert!(
                        f <= MAX_TIME_SCALE_RATE + 1e-9,
                        "convergence Accelerate factor {f:.4} exceeds 15%"
                    );
                }
                PlayoutAction::Normal | PlayoutAction::Decelerate(_) => {}
            }
            max_ticks -= 1;
        }

        // No concealment should be triggered while draining an overfilled buffer.
        assert_eq!(gaps, 0, "conceal emitted during drain — time-scaling must be used instead");
        assert!(max_ticks > 0, "buffer did not converge within the tick budget");
    }

    #[test]
    fn conceal_is_used_not_silence_when_buffer_empties() {
        let target = 2usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        jb.enqueue();
        jb.tick(); // drains the 1 frame

        // Buffer is now empty — must get Conceal, not Normal.
        assert_eq!(
            jb.tick(),
            PlayoutAction::Conceal,
            "empty buffer must signal Conceal, not silence"
        );
    }

    #[test]
    fn level_decrements_on_tick_not_on_conceal() {
        let mut jb = JitterBuffer::new(4, CONVERGENCE_ZONE_FRAMES);
        jb.enqueue();
        jb.enqueue();
        assert_eq!(jb.level(), 2);

        jb.tick();
        assert_eq!(jb.level(), 1, "tick must consume one frame");

        jb.tick(); // drains last frame
        let prev_level = jb.level();
        jb.tick(); // empty — Conceal
        assert_eq!(
            jb.level(),
            prev_level,
            "Conceal must not decrement level"
        );
    }

    // ── DelayHistogram tests ──────────────────────────────────────────────────

    #[test]
    fn histogram_empty_returns_zero_for_any_percentile() {
        let hist = DelayHistogram::new();
        assert_eq!(
            hist.percentile_frames(TARGET_PERCENTILE),
            0,
            "empty histogram must return 0 for any percentile query"
        );
        assert_eq!(hist.total_weight(), 0.0);
    }

    #[test]
    fn histogram_single_delay_value_tracks_distribution() {
        let mut hist = DelayHistogram::new();
        // Observe delay=5 many times so the histogram converges.
        for _ in 0..300 {
            hist.observe(5);
        }
        assert_eq!(
            hist.percentile_frames(TARGET_PERCENTILE),
            5,
            "after many observations at delay=5, p95 must return 5"
        );
    }

    #[test]
    fn histogram_bucket_clamped_for_extreme_delays() {
        let mut hist = DelayHistogram::new();
        // Observe an absurdly large delay — must be clamped to last bucket.
        hist.observe(usize::MAX);
        assert_eq!(
            hist.percentile_frames(TARGET_PERCENTILE),
            HISTOGRAM_BUCKETS - 1,
            "extreme delay must be clamped to last bucket"
        );
    }

    #[test]
    fn histogram_total_weight_converges_toward_one() {
        let mut hist = DelayHistogram::new();
        // After many observations the total weight approaches 1.0.
        for _ in 0..1000 {
            hist.observe(3);
        }
        let w = hist.total_weight();
        assert!(
            w > 0.99 && w <= 1.0 + 1e-9,
            "total weight must converge near 1.0 after many observations; got {w:.6}"
        );
    }

    #[test]
    fn histogram_forgets_old_distribution_when_delay_shifts() {
        let mut hist = DelayHistogram::new();
        // Establish a distribution around delay=3.
        for _ in 0..300 {
            hist.observe(3);
        }
        assert_eq!(hist.percentile_frames(TARGET_PERCENTILE), 3);

        // Shift to a higher delay — histogram should adapt.
        for _ in 0..300 {
            hist.observe(15);
        }
        let p95 = hist.percentile_frames(TARGET_PERCENTILE);
        assert!(
            p95 > 3,
            "after delay shift from 3 to 15, p95 must move above 3; got {p95}"
        );
    }

    #[test]
    fn histogram_p0_returns_first_occupied_bucket() {
        let mut hist = DelayHistogram::new();
        hist.observe(7);
        // 0th percentile — cumulative ≥ 0 immediately — should return 0.
        // (No observations before bucket 7, so we get 0 before any weight.)
        assert_eq!(hist.percentile_frames(0.0), 0);
    }

    // ── JitterBuffer adaptive target tests ───────────────────────────────────

    #[test]
    fn target_level_adapts_upward_from_observed_delays() {
        let min_target = 2usize;
        let mut jb = JitterBuffer::new(min_target, CONVERGENCE_ZONE_FRAMES);
        assert_eq!(jb.target_level(), min_target);

        // Observe consistent high delay — target must rise.
        for _ in 0..300 {
            jb.observe_arrival_delay(10);
        }
        assert!(
            jb.target_level() > min_target,
            "target level must rise above floor after observing high delays; got {}",
            jb.target_level()
        );
    }

    #[test]
    fn target_level_never_drops_below_min_target() {
        let min_target = 4usize;
        let mut jb = JitterBuffer::new(min_target, CONVERGENCE_ZONE_FRAMES);

        // Observe zero-delay packets — target must stay at floor.
        for _ in 0..500 {
            jb.observe_arrival_delay(0);
        }
        assert_eq!(
            jb.target_level(),
            min_target,
            "target level must not drop below min_target_level even with zero-delay observations"
        );
    }

    #[test]
    fn target_level_reflects_p95_of_delay_histogram() {
        let min_target = 1usize;
        let mut jb = JitterBuffer::new(min_target, CONVERGENCE_ZONE_FRAMES);

        // After enough observations at delay=8, p95 ≈ 8.
        for _ in 0..400 {
            jb.observe_arrival_delay(8);
        }
        assert_eq!(
            jb.target_level(),
            8,
            "target level must equal the p95 of observed delays"
        );
    }

    #[test]
    fn delay_histogram_accessor_returns_same_data_as_internal_state() {
        let min_target = 1usize;
        let mut jb = JitterBuffer::new(min_target, CONVERGENCE_ZONE_FRAMES);
        for _ in 0..200 {
            jb.observe_arrival_delay(6);
        }
        let p95_via_hist = jb.delay_histogram().percentile_frames(TARGET_PERCENTILE);
        assert_eq!(
            p95_via_hist,
            jb.target_level(),
            "delay_histogram accessor must agree with the adaptive target level"
        );
    }
}

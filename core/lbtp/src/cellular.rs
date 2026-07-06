//! Cellular-mode guard against 3G RAN-scheduler jitter — Feature 14.
//!
//! # Problem
//!
//! 3G (UMTS/HSPA) radio schedulers grant resources in discrete quanta.  When a
//! packet arrives between quanta it waits for the next allocation, producing a
//! bimodal one-way-delay (OWD) distribution: most packets experience a baseline
//! OWD while a fraction spike by 50–300 ms when they miss the scheduler slot.
//! These spikes are a property of the RAN, not of network congestion — they are
//! uncorrelated with the sender's transmit rate.  A delay-gradient controller
//! that interprets every spike as overuse will cut the rate every few seconds,
//! causing severe quality oscillation on 3G paths.
//!
//! # Detection — [`BimodalDetector`]
//!
//! The detector keeps a sliding window of recent OWD samples and classifies each
//! as a spike when it exceeds `SPIKE_THRESHOLD_FACTOR × baseline`, where the
//! baseline is an EMA that only updates on non-spike samples so it tracks the
//! "low" mode of the distribution rather than the mixture mean.
//!
//! A bimodal signature is declared when **all three** hold:
//!
//! 1. The window is full (`OWD_WINDOW_SIZE` samples seen).
//! 2. The spike fraction lies in `[MIN_BIMODAL_FRACTION, MAX_BIMODAL_FRACTION]`
//!    — too few spikes is a clean channel; too many is sustained high delay.
//! 3. The OWD spread EMA (`ALPHA_SPREAD` smoothed |owd − baseline|) meets
//!    `MIN_BIMODAL_SPREAD_US` — the two modes must be meaningfully separated.
//!
//! # State machine — [`CellularModeController`]
//!
//! The controller wraps the detector with hysteresis:
//!
//! - **Entry**: `CELLULAR_ENTRY_TICKS` consecutive ticks of bimodal evidence.
//! - **Exit** : `CELLULAR_EXIT_TICKS` consecutive ticks without bimodal evidence.
//!
//! Call [`observe_owd`](CellularModeController::observe_owd) for each OWD
//! measurement and [`tick`](CellularModeController::tick) once per 10 Hz
//! control interval.
//!
//! # Rate-control effects
//!
//! When active, the controller applies three SCReAM-inspired modifications:
//!
//! 1. **Widen γ**: [`gamma_multiplier`](CellularModeController::gamma_multiplier)
//!    returns `CELLULAR_GAMMA_MULTIPLIER`.  The caller multiplies the overuse
//!    threshold by this factor so transient RAN spikes do not declare overuse.
//!
//! 2. **Cap decrease frequency**: [`can_decrease`](CellularModeController::can_decrease)
//!    returns `false` until `CELLULAR_MIN_DECREASE_TICKS` have elapsed since the
//!    last allowed decrease.  Call [`record_decrease`](CellularModeController::record_decrease)
//!    when a rate cut is applied.
//!
//! 3. **Gate increases on trend**: [`can_increase`](CellularModeController::can_increase)
//!    requires the OWD trendline slope to be ≤ 0 (queue draining or neutral)
//!    before permitting a rate increase, preventing premature ramp-up while the
//!    scheduler is spiking.

use std::collections::VecDeque;

// ── Detection constants ────────────────────────────────────────────────────

/// Number of OWD samples held in the bimodal detection window.
pub const OWD_WINDOW_SIZE: usize = 30;

/// OWD spike multiplier: a sample is a spike when it exceeds this multiple of
/// the baseline EMA.  1.5 ≡ ≥ 1.5× the normal one-way delay.
///
/// 1.5 is chosen so that the spike-exclusion threshold (baseline × 1.5) is
/// crossed before the EMA stable-point when the spike fraction is ≤ ~40%.
/// 3G RAN scheduler spikes are typically 2–5× baseline, so genuine spikes
/// still clear this threshold with large margin.
pub const SPIKE_THRESHOLD_FACTOR: f64 = 1.5;

/// Minimum spike fraction (spikes / window) for bimodal classification.
/// Below this value the channel is clean — cellular mode does not apply.
pub const MIN_BIMODAL_FRACTION: f64 = 0.10;

/// Maximum spike fraction for bimodal classification.  Above this the channel
/// is in sustained high delay (congestion), not bimodal RAN jitter.
pub const MAX_BIMODAL_FRACTION: f64 = 0.45;

/// Minimum OWD spread (EMA of |owd − baseline|, microseconds) required
/// alongside a bimodal spike fraction.  Rejects false positives where OWDs are
/// tightly clustered near the spike threshold boundary.
///
/// 15 ms is well below the typical 50–300 ms RAN scheduling quantum seen on
/// 3G paths, so any genuine bimodal signature clears this floor easily.
pub const MIN_BIMODAL_SPREAD_US: f64 = 15_000.0;

// ── State-machine constants ────────────────────────────────────────────────

/// Consecutive ticks of bimodal evidence required to enter cellular mode.
/// At 10 Hz: 2 seconds of sustained evidence.
pub const CELLULAR_ENTRY_TICKS: u32 = 20;

/// Consecutive ticks without bimodal evidence required to exit cellular mode.
/// At 10 Hz: 5 seconds of sustained clean evidence.
pub const CELLULAR_EXIT_TICKS: u32 = 50;

// ── Rate-control modifier constants ───────────────────────────────────────

/// Multiplier applied to the delay-gradient overuse threshold γ in cellular
/// mode.  Widening γ by 2× prevents RAN spikes from triggering overuse.
pub const CELLULAR_GAMMA_MULTIPLIER: f64 = 2.0;

/// Minimum ticks between successive rate decreases in cellular mode.
/// At 10 Hz: 3 seconds minimum between decreases.
pub const CELLULAR_MIN_DECREASE_TICKS: u32 = 30;

// ── EMA smoothing factors ─────────────────────────────────────────────────

/// EMA α for the OWD baseline (~10-sample window).  Only non-spike samples
/// update this so it tracks the low mode of the bimodal distribution.
const ALPHA_BASELINE: f64 = 0.10;

/// EMA α for OWD spread (~5-sample window).  Faster than the baseline so the
/// spread estimate reacts to a new spike pattern within a few scheduling bursts.
const ALPHA_SPREAD: f64 = 0.20;

// ── BimodalDetector ────────────────────────────────────────────────────────

/// Online detector for bimodal one-way-delay patterns characteristic of 3G RAN
/// scheduling jitter.
///
/// One instance per active stream.  Feed OWD measurements in packet-arrival
/// order via [`observe`](Self::observe); query [`is_bimodal`](Self::is_bimodal)
/// to test whether the current window shows a cellular jitter signature.
#[derive(Debug)]
pub struct BimodalDetector {
    /// Sliding window of recent OWD measurements in microseconds.
    window: VecDeque<u32>,
    /// EMA of OWD for non-spike samples — tracks the "low" distribution mode.
    baseline_us: f64,
    /// EMA of |owd − baseline| across all samples — measures inter-mode spread.
    spread_us: f64,
    /// Cached bimodal verdict updated on each `observe`.
    bimodal: bool,
}

impl Default for BimodalDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl BimodalDetector {
    /// Create a new detector.  No samples seen; bimodal starts `false`.
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(OWD_WINDOW_SIZE + 1),
            baseline_us: 0.0,
            spread_us: 0.0,
            bimodal: false,
        }
    }

    /// Record an OWD sample in microseconds.
    ///
    /// Updates the baseline EMA, spread EMA, and the bimodal verdict.
    /// Must be called in packet-arrival order.
    pub fn observe(&mut self, owd_us: u32) {
        let owd = owd_us as f64;

        // Bootstrap: set the baseline directly from the first sample so the
        // spike threshold (2 × baseline) is meaningful from the very next
        // sample.  Without this, a spike as sample #1 would seed the EMA at
        // 10 % of the spike value, causing real BASE samples to be
        // misclassified as spikes indefinitely.
        if self.window.is_empty() {
            self.baseline_us = owd;
            self.window.push_back(owd_us);
            return;
        }

        // Maintain sliding window.
        self.window.push_back(owd_us);
        if self.window.len() > OWD_WINDOW_SIZE {
            self.window.pop_front();
        }

        // Update baseline EMA using only non-spike samples so it tracks the
        // "low" mode of the bimodal distribution rather than the mixture mean.
        let spike_thresh = self.baseline_us * SPIKE_THRESHOLD_FACTOR;
        if owd <= spike_thresh {
            self.baseline_us =
                (1.0 - ALPHA_BASELINE) * self.baseline_us + ALPHA_BASELINE * owd;
        }

        // Update spread EMA across all samples.
        let deviation = (owd - self.baseline_us).abs();
        self.spread_us = (1.0 - ALPHA_SPREAD) * self.spread_us + ALPHA_SPREAD * deviation;

        self.bimodal = self.evaluate();
    }

    /// `true` when the current window shows a bimodal OWD distribution.
    pub fn is_bimodal(&self) -> bool {
        self.bimodal
    }

    /// Current OWD baseline EMA in microseconds.
    pub fn baseline_us(&self) -> f64 {
        self.baseline_us
    }

    /// Current OWD spread EMA in microseconds.
    pub fn spread_us(&self) -> f64 {
        self.spread_us
    }

    fn evaluate(&self) -> bool {
        if self.window.len() < OWD_WINDOW_SIZE {
            return false;
        }

        let threshold = self.baseline_us * SPIKE_THRESHOLD_FACTOR;
        if threshold < 1.0 {
            return false;
        }

        let spike_count = self.window.iter().filter(|&&s| (s as f64) > threshold).count();
        let spike_fraction = spike_count as f64 / self.window.len() as f64;

        spike_fraction >= MIN_BIMODAL_FRACTION
            && spike_fraction <= MAX_BIMODAL_FRACTION
            && self.spread_us >= MIN_BIMODAL_SPREAD_US
    }
}

// ── CellularModeController ─────────────────────────────────────────────────

/// Detects 3G RAN-scheduler jitter and modifies rate-control behaviour to
/// resist it.
///
/// One instance per active session.  Integration sequence per control tick:
///
/// ```text
/// 1. controller.observe_owd(owd_us)  // for each incoming ACK group
/// 2. controller.tick()               // once per 10 Hz control interval
/// 3. Use controller.gamma_multiplier(), can_decrease(), can_increase()
///    in the congestion-control state machine.
/// ```
#[derive(Debug)]
pub struct CellularModeController {
    detector: BimodalDetector,
    /// Whether cellular mode is currently active.
    active: bool,
    /// Consecutive ticks of bimodal evidence (toward entry threshold).
    entry_counter: u32,
    /// Consecutive ticks of non-bimodal evidence (toward exit threshold).
    exit_counter: u32,
    /// Ticks elapsed since the last rate decrease was recorded.
    ticks_since_decrease: u32,
}

impl Default for CellularModeController {
    fn default() -> Self {
        Self::new()
    }
}

impl CellularModeController {
    /// Create a new controller.  Cellular mode starts inactive.
    pub fn new() -> Self {
        Self {
            detector: BimodalDetector::new(),
            active: false,
            entry_counter: 0,
            exit_counter: 0,
            // Pre-saturate so the first decrease is permitted immediately on
            // entry — the controller has not applied any decrease yet.
            ticks_since_decrease: CELLULAR_MIN_DECREASE_TICKS,
        }
    }

    /// Record an OWD sample in microseconds.
    ///
    /// Call once per incoming ACK-group report, in arrival order.
    pub fn observe_owd(&mut self, owd_us: u32) {
        self.detector.observe(owd_us);
    }

    /// Advance one 10 Hz control tick and update the cellular-mode state machine.
    ///
    /// Must be called exactly once per control interval, after all OWD
    /// observations for the interval have been fed via [`observe_owd`](Self::observe_owd).
    pub fn tick(&mut self) {
        self.ticks_since_decrease = self.ticks_since_decrease.saturating_add(1);

        if self.detector.is_bimodal() {
            self.entry_counter = self.entry_counter.saturating_add(1);
            self.exit_counter = 0;
        } else {
            self.exit_counter = self.exit_counter.saturating_add(1);
            self.entry_counter = 0;
        }

        if !self.active && self.entry_counter >= CELLULAR_ENTRY_TICKS {
            self.active = true;
        } else if self.active && self.exit_counter >= CELLULAR_EXIT_TICKS {
            self.active = false;
            // Reset so a new re-entry requires the full CELLULAR_ENTRY_TICKS.
            self.entry_counter = 0;
        }
    }

    /// Whether cellular mode is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Multiplier to apply to the overuse-detection threshold γ.
    ///
    /// Returns `CELLULAR_GAMMA_MULTIPLIER` when active, `1.0` otherwise.
    /// The congestion controller multiplies γ by this value before the
    /// overuse comparison so transient RAN spikes do not trigger rate cuts.
    pub fn gamma_multiplier(&self) -> f64 {
        if self.active { CELLULAR_GAMMA_MULTIPLIER } else { 1.0 }
    }

    /// Whether a rate decrease is permitted at this tick.
    ///
    /// In cellular mode, decreases are capped at one per
    /// `CELLULAR_MIN_DECREASE_TICKS` ticks to prevent spiral-down during
    /// scheduler-induced spikes.  Always returns `true` outside cellular mode.
    pub fn can_decrease(&self) -> bool {
        if !self.active {
            return true;
        }
        self.ticks_since_decrease >= CELLULAR_MIN_DECREASE_TICKS
    }

    /// Whether a rate increase is permitted given the current OWD trendline slope.
    ///
    /// `owd_trend` is the least-squares trendline slope over recent smoothed OWD
    /// samples (positive = delay growing, negative = draining, 0 = neutral).
    ///
    /// In cellular mode, increases require `owd_trend <= 0.0` — the queue must
    /// be draining or stable, not growing.  This prevents premature ramp-up
    /// while the RAN scheduler is actively spiking.  Always returns `true`
    /// outside cellular mode.
    pub fn can_increase(&self, owd_trend: f64) -> bool {
        if !self.active {
            return true;
        }
        owd_trend <= 0.0
    }

    /// Notify the controller that a rate decrease was applied.
    ///
    /// Resets the decrease-cap timer.  The next `can_decrease()` call will
    /// return `false` until `CELLULAR_MIN_DECREASE_TICKS` ticks have elapsed.
    pub fn record_decrease(&mut self) {
        self.ticks_since_decrease = 0;
    }

    /// Read-only access to the underlying `BimodalDetector`.
    pub fn detector(&self) -> &BimodalDetector {
        &self.detector
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Baseline OWD: 80 ms (typical 3G round-trip half).
    const BASE_OWD_US: u32 = 80_000;
    // Spike OWD: 250 ms — well above 2× baseline.
    const SPIKE_OWD_US: u32 = 250_000;
    // Clean OWD: slight jitter around baseline but never spiking.
    const CLEAN_OWD_US: u32 = 85_000;

    // ── BimodalDetector helpers ────────────────────────────────────────────

    /// Fill the detector with `total` samples where every `spike_every`-th
    /// packet is a spike.  `spike_every = 0` means no spikes.
    fn fill_detector(d: &mut BimodalDetector, total: usize, spike_every: usize) {
        for i in 0..total {
            let owd = if spike_every > 0 && i % spike_every == 0 {
                SPIKE_OWD_US
            } else {
                BASE_OWD_US
            };
            d.observe(owd);
        }
    }

    // ── BimodalDetector: window warmup ────────────────────────────────────

    #[test]
    fn bimodal_false_before_window_full() {
        let mut d = BimodalDetector::new();
        for _ in 0..(OWD_WINDOW_SIZE - 1) {
            d.observe(SPIKE_OWD_US); // all spikes — but window not full yet
        }
        assert!(
            !d.is_bimodal(),
            "must not classify as bimodal before OWD_WINDOW_SIZE samples"
        );
    }

    // ── BimodalDetector: clean channel ────────────────────────────────────

    #[test]
    fn bimodal_false_on_clean_channel() {
        let mut d = BimodalDetector::new();
        // Only baseline OWD — no spikes at all.
        for _ in 0..60 {
            d.observe(CLEAN_OWD_US);
        }
        assert!(
            !d.is_bimodal(),
            "clean channel with no spikes must not be classified as bimodal"
        );
    }

    // ── BimodalDetector: sustained high OWD ──────────────────────────────

    #[test]
    fn bimodal_false_when_all_samples_are_spikes() {
        let mut d = BimodalDetector::new();
        // All samples are spikes — spike fraction = 1.0 > MAX_BIMODAL_FRACTION.
        for _ in 0..60 {
            d.observe(SPIKE_OWD_US);
        }
        assert!(
            !d.is_bimodal(),
            "sustained high OWD (all spikes) must not be classified as bimodal"
        );
    }

    // ── BimodalDetector: genuine bimodal pattern ──────────────────────────

    #[test]
    fn bimodal_true_on_cellular_pattern_20pct_spikes() {
        let mut d = BimodalDetector::new();
        // 20% spike rate: every 5th packet is a spike.
        fill_detector(&mut d, 90, 5);
        assert!(
            d.is_bimodal(),
            "20% spike pattern with large spike/baseline ratio must be bimodal"
        );
    }

    #[test]
    fn bimodal_true_on_cellular_pattern_33pct_spikes() {
        let mut d = BimodalDetector::new();
        // 33% spike rate: every 3rd packet is a spike.
        fill_detector(&mut d, 90, 3);
        assert!(
            d.is_bimodal(),
            "33% spike pattern must be classified as bimodal"
        );
    }

    // ── BimodalDetector: spread gate ─────────────────────────────────────

    #[test]
    fn bimodal_false_when_spread_too_small() {
        let mut d = BimodalDetector::new();
        // OWD values are very close: base=80 ms, "spike"=85 ms (< 2×base),
        // so they never qualify as spikes under SPIKE_THRESHOLD_FACTOR.
        for i in 0..60 {
            let owd = if i % 5 == 0 { 85_000u32 } else { 80_000u32 };
            d.observe(owd);
        }
        assert!(
            !d.is_bimodal(),
            "small OWD variation must not trigger bimodal classification"
        );
    }

    // ── BimodalDetector: accessors ────────────────────────────────────────

    #[test]
    fn baseline_converges_toward_low_mode() {
        let mut d = BimodalDetector::new();
        // 20% spikes — baseline should converge toward BASE_OWD_US, not the mixture mean.
        fill_detector(&mut d, 200, 5);
        let baseline = d.baseline_us();
        // Acceptable range: within 50% of the true base OWD.
        assert!(
            baseline > BASE_OWD_US as f64 * 0.5 && baseline < BASE_OWD_US as f64 * 1.5,
            "baseline {baseline} should converge toward BASE_OWD_US ({BASE_OWD_US})"
        );
    }

    #[test]
    fn spread_nonzero_after_spikes() {
        let mut d = BimodalDetector::new();
        fill_detector(&mut d, 90, 5); // 20% spikes
        assert!(d.spread_us() > 0.0, "spread must be nonzero after spike observations");
    }

    // ── CellularModeController: initial state ────────────────────────────

    #[test]
    fn controller_not_active_initially() {
        let ctrl = CellularModeController::new();
        assert!(!ctrl.is_active());
    }

    #[test]
    fn gamma_multiplier_one_when_inactive() {
        let ctrl = CellularModeController::new();
        assert!(
            (ctrl.gamma_multiplier() - 1.0).abs() < f64::EPSILON,
            "gamma_multiplier must be 1.0 when cellular mode is inactive"
        );
    }

    #[test]
    fn can_decrease_true_when_inactive() {
        let ctrl = CellularModeController::new();
        assert!(ctrl.can_decrease(), "can_decrease must be true outside cellular mode");
    }

    #[test]
    fn can_increase_true_when_inactive_regardless_of_trend() {
        let ctrl = CellularModeController::new();
        assert!(ctrl.can_increase(1.0), "positive trend: must allow increase outside cellular mode");
        assert!(ctrl.can_increase(-1.0), "negative trend: must allow increase outside cellular mode");
    }

    // ── CellularModeController: entry ────────────────────────────────────

    fn make_bimodal_controller() -> CellularModeController {
        let mut ctrl = CellularModeController::new();
        // Warm the detector with a cellular pattern.
        for _ in 0..200 {
            for _ in 0..4 {
                ctrl.observe_owd(BASE_OWD_US);
            }
            ctrl.observe_owd(SPIKE_OWD_US); // 20% spikes
        }
        ctrl
    }

    #[test]
    fn not_active_before_entry_threshold() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..(CELLULAR_ENTRY_TICKS - 1) {
            ctrl.tick();
        }
        assert!(
            !ctrl.is_active(),
            "must not activate before CELLULAR_ENTRY_TICKS ticks"
        );
    }

    #[test]
    fn activates_after_entry_threshold() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(
            ctrl.is_active(),
            "must activate after CELLULAR_ENTRY_TICKS consecutive bimodal ticks"
        );
    }

    // ── CellularModeController: rate-control effects when active ─────────

    #[test]
    fn gamma_multiplier_widened_when_active() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());
        assert!(
            (ctrl.gamma_multiplier() - CELLULAR_GAMMA_MULTIPLIER).abs() < f64::EPSILON,
            "gamma_multiplier must equal CELLULAR_GAMMA_MULTIPLIER when active"
        );
    }

    #[test]
    fn can_decrease_true_immediately_after_entry() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());
        // No decrease recorded yet — ticks_since_decrease >= CELLULAR_MIN_DECREASE_TICKS.
        assert!(
            ctrl.can_decrease(),
            "can_decrease must be true when no decrease has been recorded yet"
        );
    }

    #[test]
    fn can_decrease_false_immediately_after_record_decrease() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        ctrl.record_decrease();
        assert!(
            !ctrl.can_decrease(),
            "can_decrease must be false immediately after record_decrease"
        );
    }

    #[test]
    fn can_decrease_true_after_cap_window_elapses() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        ctrl.record_decrease();
        // Tick through the cap window — each tick increments ticks_since_decrease.
        for _ in 0..CELLULAR_MIN_DECREASE_TICKS {
            ctrl.tick();
        }
        assert!(
            ctrl.can_decrease(),
            "can_decrease must be true after CELLULAR_MIN_DECREASE_TICKS ticks"
        );
    }

    #[test]
    fn can_decrease_suppressed_during_cap_window() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        ctrl.record_decrease();
        for tick in 0..(CELLULAR_MIN_DECREASE_TICKS - 1) {
            ctrl.tick();
            assert!(
                !ctrl.can_decrease(),
                "can_decrease must remain false at tick {tick} within cap window"
            );
        }
    }

    #[test]
    fn can_increase_gated_on_owd_trend_when_active() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        assert!(
            ctrl.can_increase(-1.0),
            "negative trend (queue draining) must allow increase in cellular mode"
        );
        assert!(
            ctrl.can_increase(0.0),
            "zero trend (neutral) must allow increase in cellular mode"
        );
        assert!(
            !ctrl.can_increase(0.1),
            "positive trend (queue growing) must block increase in cellular mode"
        );
        assert!(
            !ctrl.can_increase(100.0),
            "large positive trend must block increase in cellular mode"
        );
    }

    // ── CellularModeController: exit ─────────────────────────────────────

    #[test]
    fn exits_after_clean_ticks() {
        let mut ctrl = make_bimodal_controller();
        // Enter cellular mode.
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        // Switch to clean OWD — no more spikes.
        for _ in 0..200 {
            ctrl.observe_owd(CLEAN_OWD_US);
        }
        // Tick through the exit window.
        for _ in 0..CELLULAR_EXIT_TICKS {
            ctrl.tick();
        }
        assert!(
            !ctrl.is_active(),
            "must exit cellular mode after CELLULAR_EXIT_TICKS ticks of non-bimodal evidence"
        );
    }

    #[test]
    fn does_not_exit_before_exit_threshold() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        // Clean observations.
        for _ in 0..200 {
            ctrl.observe_owd(CLEAN_OWD_US);
        }
        for _ in 0..(CELLULAR_EXIT_TICKS - 1) {
            ctrl.tick();
        }
        assert!(
            ctrl.is_active(),
            "must not exit before CELLULAR_EXIT_TICKS clean ticks"
        );
    }

    #[test]
    fn gamma_multiplier_one_after_exit() {
        let mut ctrl = make_bimodal_controller();
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        for _ in 0..200 {
            ctrl.observe_owd(CLEAN_OWD_US);
        }
        for _ in 0..CELLULAR_EXIT_TICKS {
            ctrl.tick();
        }
        assert!(!ctrl.is_active());
        assert!(
            (ctrl.gamma_multiplier() - 1.0).abs() < f64::EPSILON,
            "gamma_multiplier must return 1.0 after exit from cellular mode"
        );
    }

    // ── CellularModeController: re-entry after exit ───────────────────────

    #[test]
    fn re_enters_after_bimodal_returns() {
        let mut ctrl = make_bimodal_controller();
        // Enter.
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(ctrl.is_active());

        // Exit.
        for _ in 0..200 {
            ctrl.observe_owd(CLEAN_OWD_US);
        }
        for _ in 0..CELLULAR_EXIT_TICKS {
            ctrl.tick();
        }
        assert!(!ctrl.is_active());

        // Reintroduce bimodal pattern.
        for _ in 0..200 {
            for _ in 0..4 {
                ctrl.observe_owd(BASE_OWD_US);
            }
            ctrl.observe_owd(SPIKE_OWD_US);
        }
        for _ in 0..CELLULAR_ENTRY_TICKS {
            ctrl.tick();
        }
        assert!(
            ctrl.is_active(),
            "must re-enter cellular mode when bimodal pattern returns"
        );
    }

    // ── Default / structural ──────────────────────────────────────────────

    #[test]
    fn default_equals_new_for_detector() {
        let a = BimodalDetector::new();
        let b = BimodalDetector::default();
        assert_eq!(a.is_bimodal(), b.is_bimodal());
        assert_eq!(a.baseline_us().to_bits(), b.baseline_us().to_bits());
    }

    #[test]
    fn default_equals_new_for_controller() {
        let a = CellularModeController::new();
        let b = CellularModeController::default();
        assert_eq!(a.is_active(), b.is_active());
        assert_eq!(
            a.gamma_multiplier().to_bits(),
            b.gamma_multiplier().to_bits()
        );
    }
}

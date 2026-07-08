//! Adaptive Reed-Solomon FEC sizing via Gilbert-Elliott burst model — Feature 16.
//!
//! The transport layer applies forward-error-correction overhead that adapts to
//! observed channel conditions.  A fixed FEC tax wastes bandwidth on clean links
//! and under-protects on lossy ones; this module derives a per-stream `fec_ratio`
//! from a fitted Gilbert-Elliott model so the repair overhead tracks actual loss
//! bursts rather than a worst-case constant.
//!
//! # Gilbert-Elliott model
//!
//! The channel alternates between two states driven by a first-order Markov chain:
//!
//! ```text
//! Good (G) ──a──► Bad (B)
//!           ◄──b──
//! ```
//!
//! | Symbol | Meaning |
//! |--------|---------|
//! | `a`    | G → B transition probability per packet |
//! | `b`    | B → G transition probability per packet |
//! | `p`    | Loss probability in good state (≈ 0) |
//! | `q`    | Loss probability in bad state (≈ 1) |
//!
//! This implementation uses the simplified *Gilbert* form (p = 0, q = 1), the
//! standard two-parameter model used in IETF FEC-sizing literature.  Under this
//! form every parameter is derivable from the observed mean loss rate `L` and
//! mean burst length `M`:
//!
//! ```text
//! b = 1/M
//! a = L·b / (1 − L)
//! steady-state loss  = a / (a + b) = L  ✓
//! ```
//!
//! # fec_ratio derivation
//!
//! `fec_ratio` is the fraction `r/k` where `r` repair symbols cover `k` source
//! symbols.  Two independent lower bounds are taken:
//!
//! 1. **Independent-loss baseline**: `L / (1 − L)` — minimum overhead if losses
//!    were Bernoulli-independent (classic FEC sizing).
//! 2. **Burst-coverage term**: `min(M, REF_BLOCK) / REF_BLOCK` — fraction of a
//!    reference block that one mean-length burst would erase.  Ensures sufficient
//!    repair overhead even when the mean loss rate is low but burst length is high.
//!
//! The result is clamped to [`MIN_FEC_RATIO`] … [`MAX_FEC_RATIO`].

/// Minimum FEC repair fraction — 5 % overhead on a clean channel.
pub const MIN_FEC_RATIO: f64 = 0.05;
/// Maximum FEC repair fraction — caps RS cost at 50 % of source symbols.
pub const MAX_FEC_RATIO: f64 = 0.50;

/// Reference block size used for burst-coverage normalisation (symbols).
///
/// A burst of length `mean_burst_len` erases `min(mean_burst_len, REF_BLOCK_SYMBOLS)`
/// symbols from a reference-sized block.  Using a fixed reference lets `fec_ratio`
/// remain dimensionless and independent of actual block size.
const REF_BLOCK_SYMBOLS: f64 = 32.0;

/// Minimum observations before [`GilbertElliottEstimator::params`] and a
/// non-floor [`fec_ratio`](GilbertElliottEstimator::fec_ratio) are returned.
///
/// Below this count the channel model is unreliable and callers receive the
/// conservative default ([`MIN_FEC_RATIO`]).
pub const MIN_OBS_FOR_ESTIMATE: u64 = 30;

/// Minimum completed run pairs before [`BurstStatsFitter::params`] produces an estimate.
///
/// One run pair is one good (received) run followed by one bad (lost) run.  Eight
/// pairs give stable mean estimates while staying responsive to channel changes.
pub const MIN_RUNS_FOR_ESTIMATE: u64 = 8;

/// Gilbert-Elliott two-state Markov model parameters for one stream.
///
/// Derived from the simplified Gilbert form: `p = 0` (no loss in good state),
/// `q = 1` (every bad-state packet is lost).  The full GE generalisation is
/// left as a future extension should per-class measurement infrastructure exist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GilbertElliottParams {
    /// G → B transition probability per packet.
    pub a: f64,
    /// B → G transition probability per packet (= 1 / mean_burst_length).
    pub b: f64,
    /// Loss probability in good state.  Always `0.0` in the current estimator.
    pub p: f64,
    /// Loss probability in bad state.  Always `1.0` in the current estimator.
    pub q: f64,
}

impl GilbertElliottParams {
    /// Steady-state packet loss rate: `π_B·q + π_G·p`.
    pub fn mean_loss_rate(&self) -> f64 {
        if (self.a + self.b) < f64::EPSILON {
            return 0.0;
        }
        let pi_b = self.a / (self.a + self.b);
        let pi_g = 1.0 - pi_b;
        pi_b * self.q + pi_g * self.p
    }

    /// Mean loss burst length in packets (= 1 / b).
    pub fn mean_burst_len(&self) -> f64 {
        if self.b > 0.0 { 1.0 / self.b } else { f64::INFINITY }
    }
}

/// Online Gilbert-Elliott burst-model estimator for one LBTP stream.
///
/// The transport event loop (or ACK processor) feeds packet outcomes through
/// [`observe`](Self::observe) in sequence.  The estimator maintains exponential
/// moving averages of the loss rate and burst length then exposes the fitted
/// parameters and the recommended [`fec_ratio`](Self::fec_ratio).
///
/// One instance must be created per active stream; create it via [`new`](Self::new)
/// and keep it alive for the session duration so the EMA warms up.
///
/// # Example
///
/// ```rust
/// use lowband_lbtp::fec::{GilbertElliottEstimator, MIN_FEC_RATIO};
///
/// let mut est = GilbertElliottEstimator::new();
///
/// // Observe 100 packets with 10 % independent loss.
/// for i in 0u32..100 {
///     est.observe(i % 10 != 0); // every 10th packet lost
/// }
///
/// // fec_ratio is at least MIN_FEC_RATIO.
/// assert!(est.fec_ratio() >= MIN_FEC_RATIO);
/// ```
#[derive(Debug)]
pub struct GilbertElliottEstimator {
    /// EMA of per-packet loss fraction (0 = received, 1 = lost).
    loss_rate: f64,
    /// EMA of loss run length, updated at the end of each loss burst.
    mean_burst_len: f64,
    /// Total observations fed to this estimator.
    n_obs: u64,

    // Loss-run state machine.
    in_loss_run: bool,
    current_run_len: u32,

    /// Per-packet EMA smoothing factor for `loss_rate`.
    alpha_loss: f64,
    /// Per-burst EMA smoothing factor for `mean_burst_len`.
    alpha_burst: f64,
}

impl Default for GilbertElliottEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl GilbertElliottEstimator {
    /// Create an estimator with default smoothing constants.
    ///
    /// `alpha_loss = 0.05` corresponds to a ~20-packet exponential window —
    /// slow enough to smooth single-packet glitches while reacting within a
    /// few hundred milliseconds at 50 pps.
    ///
    /// `alpha_burst = 0.25` adapts burst length over roughly 4 completed
    /// bursts, tracking changes in 3G burst statistics within seconds.
    pub fn new() -> Self {
        Self::with_alphas(0.05, 0.25)
    }

    /// Create an estimator with explicit EMA smoothing constants.
    ///
    /// Both values must lie strictly in `(0.0, 1.0)`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if either alpha is outside `(0, 1)`.
    pub fn with_alphas(alpha_loss: f64, alpha_burst: f64) -> Self {
        debug_assert!(
            alpha_loss > 0.0 && alpha_loss < 1.0,
            "alpha_loss must be in (0, 1)"
        );
        debug_assert!(
            alpha_burst > 0.0 && alpha_burst < 1.0,
            "alpha_burst must be in (0, 1)"
        );
        Self {
            loss_rate: 0.0,
            mean_burst_len: 1.0,
            n_obs: 0,
            in_loss_run: false,
            current_run_len: 0,
            alpha_loss,
            alpha_burst,
        }
    }

    /// Record one packet outcome.
    ///
    /// `received = true`  — packet arrived at the receiver.
    /// `received = false` — packet was lost (detected via ACK gap or timeout).
    ///
    /// Must be called in packet sequence order.  Calling with out-of-order
    /// observations will skew burst-length estimates.
    pub fn observe(&mut self, received: bool) {
        self.n_obs += 1;

        // Update loss-rate EMA: 0 on receive, 1 on loss.
        let loss_indicator = if received { 0.0 } else { 1.0 };
        self.loss_rate =
            (1.0 - self.alpha_loss) * self.loss_rate + self.alpha_loss * loss_indicator;

        // Update burst run-length state machine.
        if received {
            if self.in_loss_run && self.current_run_len > 0 {
                // Loss run just ended — update burst length EMA.
                let burst = self.current_run_len as f64;
                self.mean_burst_len = (1.0 - self.alpha_burst) * self.mean_burst_len
                    + self.alpha_burst * burst;
            }
            self.in_loss_run = false;
            self.current_run_len = 0;
        } else {
            self.in_loss_run = true;
            self.current_run_len = self.current_run_len.saturating_add(1);
        }
    }

    /// Derived Gilbert-Elliott parameters from current observations.
    ///
    /// Returns `None` until [`MIN_OBS_FOR_ESTIMATE`] packets have been
    /// observed; early estimates are unreliable and callers should fall back
    /// to the conservative [`MIN_FEC_RATIO`] default.
    ///
    /// Uses the simplified Gilbert form (p = 0, q = 1):
    ///
    /// ```text
    /// b = 1 / mean_burst_len
    /// a = L·b / (1 − L)
    /// ```
    pub fn params(&self) -> Option<GilbertElliottParams> {
        if self.n_obs < MIN_OBS_FOR_ESTIMATE {
            return None;
        }
        let l = self.loss_rate.clamp(0.0, 1.0 - f64::EPSILON);
        let b = 1.0 / self.mean_burst_len.max(1.0);
        let a = l * b / (1.0 - l).max(f64::EPSILON);
        Some(GilbertElliottParams { a, b, p: 0.0, q: 1.0 })
    }

    /// Recommended RS FEC repair ratio `r/k` for this stream.
    ///
    /// The caller computes `r = ceil(k * fec_ratio)` repair symbols for `k`
    /// source symbols.  Two lower bounds are combined:
    ///
    /// 1. `L / (1 − L)` — independent-loss baseline.
    /// 2. `min(M, REF_BLOCK) / REF_BLOCK` — burst-coverage fraction.
    ///
    /// Returns [`MIN_FEC_RATIO`] until [`MIN_OBS_FOR_ESTIMATE`] packets have
    /// been observed.
    pub fn fec_ratio(&self) -> f64 {
        if self.n_obs < MIN_OBS_FOR_ESTIMATE {
            return MIN_FEC_RATIO;
        }
        let loss = self.loss_rate.clamp(0.0, 0.99);
        let burst = self.mean_burst_len.max(1.0);

        let independent = loss / (1.0 - loss);
        let burst_coverage = burst.min(REF_BLOCK_SYMBOLS) / REF_BLOCK_SYMBOLS;

        independent.max(burst_coverage).clamp(MIN_FEC_RATIO, MAX_FEC_RATIO)
    }

    /// Current loss rate EMA in `[0, 1]`.
    pub fn loss_rate(&self) -> f64 {
        self.loss_rate
    }

    /// Current mean burst length EMA (≥ 1.0, in packets).
    pub fn mean_burst_len(&self) -> f64 {
        self.mean_burst_len
    }

    /// Total packet observations fed to this estimator.
    pub fn observation_count(&self) -> u64 {
        self.n_obs
    }
}

/// Batch Gilbert-Elliott parameter fitter from ACK-decoded run-length sequences.
///
/// Where [`GilbertElliottEstimator`] performs per-packet EMA, this fitter ingests
/// complete run-length sequences decoded from ACK frames and derives GE parameters
/// via method-of-moments on the empirical run-length distributions:
///
/// ```text
/// mean good run  M_g = E[received-run length]  →  a = 1 / M_g  (G → B per packet)
/// mean bad  run  M_b = E[lost-run length]       →  b = 1 / M_b  (B → G per packet)
/// steady-state loss  L = a / (a + b)
/// ```
///
/// The simplified Gilbert form is used throughout (`p = 0`, `q = 1`).
///
/// # Usage
///
/// Create one fitter per stream.  Each time an ACK frame is decoded, extract the
/// alternating good/bad run lengths and call [`ingest_ack_run_lengths`].  Query
/// [`fec_ratio`] to size the repair block.
///
/// ```rust
/// use lowband_lbtp::fec::{BurstStatsFitter, MIN_FEC_RATIO};
///
/// let mut fitter = BurstStatsFitter::new();
///
/// // Simulate ACK frame reporting 10 received then 2 lost, repeated.
/// let runs: Vec<(u32, u32)> = (0..20).map(|_| (10u32, 2u32)).collect();
/// fitter.ingest_ack_run_lengths(&runs);
///
/// assert!(fitter.fec_ratio() >= MIN_FEC_RATIO);
/// ```
///
/// [`ingest_ack_run_lengths`]: Self::ingest_ack_run_lengths
/// [`fec_ratio`]: Self::fec_ratio
#[derive(Debug, Default, Clone)]
pub struct BurstStatsFitter {
    good_run_sum: u64,
    good_run_count: u64,
    bad_run_sum: u64,
    bad_run_count: u64,
}

impl BurstStatsFitter {
    /// Create a new, empty fitter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a slice of `(good_len, bad_len)` run pairs decoded from one ACK frame.
    ///
    /// Each pair represents one complete channel cycle: `good_len` consecutive
    /// received packets followed by `bad_len` consecutive lost packets.  Zero-length
    /// fields are silently skipped so partial terminal runs (e.g., a trailing
    /// good-only run at the end of an ACK window) can be passed as `(n, 0)`.
    pub fn ingest_ack_run_lengths(&mut self, runs: &[(u32, u32)]) {
        for &(good, bad) in runs {
            if good > 0 {
                self.good_run_sum += u64::from(good);
                self.good_run_count += 1;
            }
            if bad > 0 {
                self.bad_run_sum += u64::from(bad);
                self.bad_run_count += 1;
            }
        }
    }

    /// Ingest a single good (received) run of `len` packets.
    pub fn observe_good_run(&mut self, len: u32) {
        if len > 0 {
            self.good_run_sum += u64::from(len);
            self.good_run_count += 1;
        }
    }

    /// Ingest a single bad (loss) run of `len` packets.
    pub fn observe_bad_run(&mut self, len: u32) {
        if len > 0 {
            self.bad_run_sum += u64::from(len);
            self.bad_run_count += 1;
        }
    }

    /// Number of complete (good, bad) run pairs observed so far.
    ///
    /// A pair requires both a good run and a bad run to have been recorded.
    pub fn run_pair_count(&self) -> u64 {
        self.good_run_count.min(self.bad_run_count)
    }

    /// Fitted Gilbert-Elliott parameters.
    ///
    /// Returns `None` until at least [`MIN_RUNS_FOR_ESTIMATE`] complete run pairs
    /// have been observed.
    ///
    /// Uses method-of-moments:
    ///
    /// ```text
    /// a = 1 / mean_good_run
    /// b = 1 / mean_bad_run
    /// p = 0, q = 1   (simplified Gilbert form)
    /// ```
    pub fn params(&self) -> Option<GilbertElliottParams> {
        if self.run_pair_count() < MIN_RUNS_FOR_ESTIMATE {
            return None;
        }
        let mean_good = self.good_run_sum as f64 / self.good_run_count as f64;
        let mean_bad = self.bad_run_sum as f64 / self.bad_run_count as f64;
        let a = 1.0 / mean_good.max(1.0);
        let b = 1.0 / mean_bad.max(1.0);
        Some(GilbertElliottParams { a, b, p: 0.0, q: 1.0 })
    }

    /// Recommended RS FEC repair ratio derived from the fitted GE parameters.
    ///
    /// Uses the same two-bound formula as [`GilbertElliottEstimator::fec_ratio`]:
    /// independent-loss baseline and burst-coverage fraction.  Returns
    /// [`MIN_FEC_RATIO`] until [`MIN_RUNS_FOR_ESTIMATE`] pairs have been observed.
    pub fn fec_ratio(&self) -> f64 {
        let Some(p) = self.params() else {
            return MIN_FEC_RATIO;
        };
        let loss = p.mean_loss_rate().clamp(0.0, 0.99);
        let burst = p.mean_burst_len().max(1.0);
        let independent = loss / (1.0 - loss);
        let burst_coverage = burst.min(REF_BLOCK_SYMBOLS) / REF_BLOCK_SYMBOLS;
        independent.max(burst_coverage).clamp(MIN_FEC_RATIO, MAX_FEC_RATIO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sanity helpers ────────────────────────────────────────────────────────

    fn observe_n(est: &mut GilbertElliottEstimator, total: u32, loss_every: u32) {
        for i in 0..total {
            est.observe(i % loss_every != 0 || loss_every == 0);
        }
    }

    /// Drive the estimator warm past MIN_OBS_FOR_ESTIMATE with no loss.
    fn warm_no_loss() -> GilbertElliottEstimator {
        let mut e = GilbertElliottEstimator::new();
        observe_n(&mut e, MIN_OBS_FOR_ESTIMATE as u32 + 10, u32::MAX);
        e
    }

    // ── Before MIN_OBS_FOR_ESTIMATE ───────────────────────────────────────────

    #[test]
    fn params_returns_none_before_min_obs() {
        let est = GilbertElliottEstimator::new();
        assert!(est.params().is_none());
    }

    #[test]
    fn fec_ratio_returns_min_before_min_obs() {
        let est = GilbertElliottEstimator::new();
        assert_eq!(est.fec_ratio(), MIN_FEC_RATIO);
    }

    #[test]
    fn params_returns_some_after_min_obs() {
        let est = warm_no_loss();
        assert!(est.params().is_some());
    }

    // ── Loss rate convergence ─────────────────────────────────────────────────

    #[test]
    fn loss_rate_zero_on_clean_channel() {
        let mut est = GilbertElliottEstimator::new();
        for _ in 0..200 {
            est.observe(true);
        }
        // Loss rate should be near-zero after many clean packets.
        assert!(
            est.loss_rate() < 0.01,
            "loss_rate {} should be near-zero on a clean channel",
            est.loss_rate()
        );
    }

    #[test]
    fn loss_rate_converges_toward_observed_fraction() {
        let mut est = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        // 10 % loss: every 10th packet lost.
        for _ in 0..5 {
            observe_n(&mut est, 100, 10);
        }
        let l = est.loss_rate();
        assert!(
            l > 0.05 && l < 0.20,
            "loss_rate {} should be near 0.10 for 10% channel",
            l
        );
    }

    #[test]
    fn loss_rate_high_on_lossy_channel() {
        let mut est = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        // 50 % loss
        for _ in 0..200 {
            est.observe(false);
            est.observe(true);
        }
        let l = est.loss_rate();
        assert!(l > 0.3, "loss_rate {} should be high on 50% channel", l);
    }

    // ── Burst length estimation ───────────────────────────────────────────────

    #[test]
    fn burst_len_stays_near_one_for_independent_loss() {
        // Single isolated losses (burst len = 1 each).
        let mut est = GilbertElliottEstimator::new();
        for _ in 0..200 {
            est.observe(true);
            est.observe(false); // every other packet
            est.observe(true);
        }
        let m = est.mean_burst_len();
        assert!(
            m < 2.5,
            "mean_burst_len {} should be near 1 for isolated single losses",
            m
        );
    }

    #[test]
    fn burst_len_grows_with_longer_bursts() {
        let mut est = GilbertElliottEstimator::with_alphas(0.05, 0.25);
        // Bursts of length 5.
        for _ in 0..30 {
            est.observe(true);
            est.observe(true);
            est.observe(true);
            for _ in 0..5 {
                est.observe(false);
            }
        }
        let m = est.mean_burst_len();
        assert!(
            m > 2.0,
            "mean_burst_len {} should be > 2 for bursts of length 5",
            m
        );
    }

    // ── GilbertElliottParams ──────────────────────────────────────────────────

    #[test]
    fn params_p_zero_q_one_simplified_model() {
        let est = warm_no_loss();
        let p = est.params().unwrap();
        assert_eq!(p.p, 0.0, "simplified model: p must be 0");
        assert_eq!(p.q, 1.0, "simplified model: q must be 1");
    }

    #[test]
    fn params_b_equals_reciprocal_of_burst_len() {
        let mut est = GilbertElliottEstimator::with_alphas(0.05, 0.25);
        // Drive bursts of length 4.
        for _ in 0..40 {
            est.observe(true);
            est.observe(true);
            for _ in 0..4 {
                est.observe(false);
            }
        }
        let p = est.params().unwrap();
        let expected_b = 1.0 / est.mean_burst_len();
        assert!(
            (p.b - expected_b).abs() < 1e-9,
            "b {} should equal 1/mean_burst_len {}",
            p.b,
            expected_b
        );
    }

    #[test]
    fn params_mean_loss_rate_matches_steady_state() {
        let mut est = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        // 20 % loss.
        for _ in 0..300 {
            if est.observation_count() % 5 == 0 {
                est.observe(false);
            } else {
                est.observe(true);
            }
        }
        if let Some(p) = est.params() {
            let computed = p.mean_loss_rate();
            let observed = est.loss_rate();
            // Both should be near 0.20; they are derived from the same EMA so
            // they should agree within floating-point tolerance.
            assert!(
                (computed - observed).abs() < 0.02,
                "params.mean_loss_rate() {} should be close to estimator loss_rate {}",
                computed,
                observed
            );
        }
    }

    #[test]
    fn params_mean_burst_len_matches_estimator() {
        let mut est = GilbertElliottEstimator::with_alphas(0.05, 0.25);
        // Bursts of length 3.
        for _ in 0..40 {
            est.observe(true);
            est.observe(true);
            est.observe(false);
            est.observe(false);
            est.observe(false);
        }
        if let Some(p) = est.params() {
            assert!(
                (p.mean_burst_len() - est.mean_burst_len()).abs() < 1e-9,
                "params.mean_burst_len() should match estimator.mean_burst_len()"
            );
        }
    }

    // ── fec_ratio ─────────────────────────────────────────────────────────────

    #[test]
    fn fec_ratio_at_least_min_on_clean_channel() {
        let est = warm_no_loss();
        assert!(
            est.fec_ratio() >= MIN_FEC_RATIO,
            "fec_ratio must not go below MIN_FEC_RATIO on a clean channel"
        );
    }

    #[test]
    fn fec_ratio_at_most_max() {
        let mut est = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        // Drive near-total loss.
        for _ in 0..500 {
            est.observe(false);
        }
        assert!(
            est.fec_ratio() <= MAX_FEC_RATIO,
            "fec_ratio must not exceed MAX_FEC_RATIO"
        );
    }

    #[test]
    fn fec_ratio_increases_with_loss_rate() {
        let mut low_loss = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        let mut high_loss = GilbertElliottEstimator::with_alphas(0.1, 0.25);

        // Low loss: 2 % (every 50th packet)
        for i in 0u32..300 {
            low_loss.observe(i % 50 != 0);
        }
        // High loss: 20 % (every 5th packet)
        for i in 0u32..300 {
            high_loss.observe(i % 5 != 0);
        }

        assert!(
            high_loss.fec_ratio() > low_loss.fec_ratio(),
            "higher loss rate ({}) should yield higher fec_ratio ({}) than low ({} @ {})",
            high_loss.loss_rate(),
            high_loss.fec_ratio(),
            low_loss.fec_ratio(),
            low_loss.loss_rate(),
        );
    }

    #[test]
    fn fec_ratio_increases_with_burst_length() {
        let mut short_burst = GilbertElliottEstimator::with_alphas(0.05, 0.25);
        let mut long_burst = GilbertElliottEstimator::with_alphas(0.05, 0.25);

        // Same average loss rate but different burst lengths.
        // Short bursts of length 1.
        for _ in 0..60 {
            short_burst.observe(true);
            short_burst.observe(true);
            short_burst.observe(true);
            short_burst.observe(false); // single loss
        }
        // Long bursts of length 8.
        for _ in 0..20 {
            long_burst.observe(true);
            long_burst.observe(true);
            long_burst.observe(true);
            long_burst.observe(true);
            for _ in 0..8 {
                long_burst.observe(false);
            }
            // Short gap to separate bursts.
            long_burst.observe(true);
            long_burst.observe(true);
        }

        assert!(
            long_burst.fec_ratio() >= short_burst.fec_ratio(),
            "longer bursts ({:.2}) should yield fec_ratio ({}) >= short bursts ({:.2}, {})",
            long_burst.mean_burst_len(),
            long_burst.fec_ratio(),
            short_burst.mean_burst_len(),
            short_burst.fec_ratio(),
        );
    }

    #[test]
    fn fec_ratio_clamped_to_max_on_sustained_loss() {
        let mut est = GilbertElliottEstimator::with_alphas(0.5, 0.5);
        // Sustained 90 % loss.
        for i in 0u32..200 {
            est.observe(i % 10 == 0);
        }
        assert_eq!(est.fec_ratio(), MAX_FEC_RATIO);
    }

    // ── observation_count ─────────────────────────────────────────────────────

    #[test]
    fn observation_count_increments_per_observe() {
        let mut est = GilbertElliottEstimator::new();
        for i in 1u64..=10 {
            est.observe(true);
            assert_eq!(est.observation_count(), i);
        }
    }

    // ── GilbertElliottParams helpers ──────────────────────────────────────────

    #[test]
    fn params_mean_burst_len_reciprocal_of_b() {
        let p = GilbertElliottParams { a: 0.01, b: 0.2, p: 0.0, q: 1.0 };
        assert!((p.mean_burst_len() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn params_mean_loss_rate_zero_when_a_zero() {
        // a=0 means the channel never enters the bad state.
        let p = GilbertElliottParams { a: 0.0, b: 0.5, p: 0.0, q: 1.0 };
        assert_eq!(p.mean_loss_rate(), 0.0);
    }

    #[test]
    fn params_mean_loss_rate_one_when_b_zero_a_nonzero() {
        // With a=1, b=0 the channel is always in the bad state.
        // a/(a+b) = 1/1 = 1.0, mean_loss = 1.0 * q = 1.0.
        let p = GilbertElliottParams { a: 1.0, b: 0.0, p: 0.0, q: 1.0 };
        // b=0 → pi_B = a/(a+0) but we guard against NaN via epsilon check.
        // mean_loss = 1.0 * 1.0 = 1.0.
        // Note: a+b = 1.0 so no division issue.
        assert!((p.mean_loss_rate() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn default_estimator_equals_new() {
        let a = GilbertElliottEstimator::default();
        let b = GilbertElliottEstimator::new();
        assert_eq!(a.loss_rate(), b.loss_rate());
        assert_eq!(a.mean_burst_len(), b.mean_burst_len());
        assert_eq!(a.observation_count(), b.observation_count());
    }

    // ── BurstStatsFitter ──────────────────────────────────────────────────────

    fn make_pairs(good: u32, bad: u32, n: usize) -> Vec<(u32, u32)> {
        vec![(good, bad); n]
    }

    #[test]
    fn fitter_params_none_before_min_runs() {
        let mut f = BurstStatsFitter::new();
        let pairs = make_pairs(10, 2, MIN_RUNS_FOR_ESTIMATE as usize - 1);
        f.ingest_ack_run_lengths(&pairs);
        assert!(f.params().is_none());
    }

    #[test]
    fn fitter_fec_ratio_min_before_min_runs() {
        let f = BurstStatsFitter::new();
        assert_eq!(f.fec_ratio(), MIN_FEC_RATIO);
    }

    #[test]
    fn fitter_params_some_after_min_runs() {
        let mut f = BurstStatsFitter::new();
        let pairs = make_pairs(10, 2, MIN_RUNS_FOR_ESTIMATE as usize);
        f.ingest_ack_run_lengths(&pairs);
        assert!(f.params().is_some());
    }

    #[test]
    fn fitter_a_b_from_known_mean_runs() {
        // Good runs of 10 → a = 1/10 = 0.1; bad runs of 4 → b = 1/4 = 0.25.
        let mut f = BurstStatsFitter::new();
        f.ingest_ack_run_lengths(&make_pairs(10, 4, 20));
        let p = f.params().unwrap();
        assert!(
            (p.a - 0.1).abs() < 1e-9,
            "a {} should be 0.1 for mean good run 10",
            p.a
        );
        assert!(
            (p.b - 0.25).abs() < 1e-9,
            "b {} should be 0.25 for mean bad run 4",
            p.b
        );
    }

    #[test]
    fn fitter_mean_loss_rate_from_run_lengths() {
        // Good runs of 8, bad runs of 2 → π_B = (1/8) / (1/8 + 1/2) = 0.2.
        let mut f = BurstStatsFitter::new();
        f.ingest_ack_run_lengths(&make_pairs(8, 2, 20));
        let p = f.params().unwrap();
        let expected_loss = 2.0_f64 / (8.0 + 2.0); // 0.2
        assert!(
            (p.mean_loss_rate() - expected_loss).abs() < 1e-9,
            "mean_loss_rate {} should be {} for 8/2 run ratio",
            p.mean_loss_rate(),
            expected_loss
        );
    }

    #[test]
    fn fitter_observe_good_bad_individual() {
        let mut f = BurstStatsFitter::new();
        for _ in 0..20 {
            f.observe_good_run(5);
            f.observe_bad_run(3);
        }
        let p = f.params().unwrap();
        assert!(
            (p.a - 0.2).abs() < 1e-9,
            "a {} should be 0.2 for mean good run 5",
            p.a
        );
        assert!(
            (p.b - 1.0 / 3.0).abs() < 1e-9,
            "b {} should be 1/3 for mean bad run 3",
            p.b
        );
    }

    #[test]
    fn fitter_run_pair_count_requires_both() {
        let mut f = BurstStatsFitter::new();
        for _ in 0..10 {
            f.observe_good_run(5);
        }
        // Only good runs ingested — pair count should be 0 (no bad runs yet).
        assert_eq!(f.run_pair_count(), 0);
    }

    #[test]
    fn fitter_zero_length_runs_skipped() {
        let mut f = BurstStatsFitter::new();
        f.ingest_ack_run_lengths(&[(0, 0); 20]);
        assert_eq!(f.run_pair_count(), 0);
        assert!(f.params().is_none());
    }

    #[test]
    fn fitter_partial_terminal_run_accepted() {
        // Last ACK window ends with a good run, no following loss run — (n, 0).
        let mut f = BurstStatsFitter::new();
        let mut pairs: Vec<(u32, u32)> = make_pairs(10, 2, 19);
        pairs.push((10, 0)); // terminal good-only run
        f.ingest_ack_run_lengths(&pairs);
        // 20 good runs, 19 bad runs → min pair count = 19 ≥ MIN_RUNS_FOR_ESTIMATE.
        assert!(f.params().is_some());
    }

    #[test]
    fn fitter_fec_ratio_bounded() {
        // Extreme: very short good runs, very long bad runs → high loss.
        let mut f = BurstStatsFitter::new();
        f.ingest_ack_run_lengths(&make_pairs(1, 30, 20));
        let r = f.fec_ratio();
        assert!(
            r >= MIN_FEC_RATIO && r <= MAX_FEC_RATIO,
            "fec_ratio {} out of [{}, {}]",
            r,
            MIN_FEC_RATIO,
            MAX_FEC_RATIO
        );
    }

    #[test]
    fn fitter_fec_ratio_higher_for_longer_bad_runs() {
        let mut short_bad = BurstStatsFitter::new();
        let mut long_bad = BurstStatsFitter::new();
        short_bad.ingest_ack_run_lengths(&make_pairs(10, 1, 20));
        long_bad.ingest_ack_run_lengths(&make_pairs(10, 8, 20));
        assert!(
            long_bad.fec_ratio() >= short_bad.fec_ratio(),
            "longer bad runs ({}) should give fec_ratio ({}) >= short bad ({}, {})",
            long_bad.params().unwrap().mean_burst_len(),
            long_bad.fec_ratio(),
            short_bad.fec_ratio(),
            short_bad.params().unwrap().mean_burst_len(),
        );
    }

    #[test]
    fn fitter_p_zero_q_one_simplified_model() {
        let mut f = BurstStatsFitter::new();
        f.ingest_ack_run_lengths(&make_pairs(10, 2, 20));
        let p = f.params().unwrap();
        assert_eq!(p.p, 0.0, "simplified model: p must be 0");
        assert_eq!(p.q, 1.0, "simplified model: q must be 1");
    }

    #[test]
    fn fitter_default_equals_new() {
        let a = BurstStatsFitter::default();
        let b = BurstStatsFitter::new();
        assert_eq!(a.run_pair_count(), b.run_pair_count());
        assert_eq!(a.fec_ratio(), b.fec_ratio());
    }
}

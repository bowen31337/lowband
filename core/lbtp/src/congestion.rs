//! Loss-backstop congestion controller — Feature 20.
//!
//! # Role
//!
//! The primary congestion signal is the delay gradient (OWD trend-line /
//! Kalman filter).  This module is the *backstop*: it acts only when sustained
//! packet loss exceeds [`LOSS_BACKSTOP_THRESHOLD`] and the estimator has seen
//! enough packets to produce a reliable signal.
//!
//! # Mechanism
//!
//! The transport event loop calls [`LossBackstop::check`] once per control
//! tick, passing the current send rate and the stream's
//! [`GilbertElliottEstimator`].  When all three conditions hold:
//!
//! 1. `estimator.observation_count() >= MIN_OBS_FOR_ESTIMATE`
//! 2. `estimator.loss_rate() > LOSS_BACKSTOP_THRESHOLD` (> 10 %)
//! 3. the internal cooldown timer has elapsed
//!
//! … the backstop returns `Some(new_rate)` — a multiplicative decrease of
//! `current_rate × LOSS_BACKSTOP_REDUCTION`, floored at
//! [`BACKSTOP_MIN_RATE_BPS`].  The caller must immediately apply the new rate
//! via [`Pacer::set_rate`].
//!
//! When none of the conditions are met, `check` returns `None` — the caller
//! leaves the rate unchanged.
//!
//! # Cooldown
//!
//! After each reduction the backstop suppresses further reductions for
//! [`BACKSTOP_COOLDOWN_TICKS`] ticks.  This prevents the rate from spiralling
//! down while the EMA catchs up to new link conditions.

use crate::fec::{GilbertElliottEstimator, MIN_OBS_FOR_ESTIMATE};

/// Loss fraction above which the backstop engages (10 %).
pub const LOSS_BACKSTOP_THRESHOLD: f64 = 0.10;

/// Multiplicative reduction applied to the send rate on each backstop trigger.
///
/// New rate = current × 0.85 (15 % cut).  A conservative decrease consistent
/// with the backstop's role as a last-resort control; the delay-gradient
/// controller handles routine bandwidth reduction.
pub const LOSS_BACKSTOP_REDUCTION: f64 = 0.85;

/// Hard floor on the backstop-controlled send rate.
///
/// 48 kbps matches the Survival tier minimum from the architecture spec (§8).
/// The backstop never pushes the rate below this value.
pub const BACKSTOP_MIN_RATE_BPS: f64 = 48_000.0;

/// Minimum number of ticks between successive backstop reductions.
///
/// 50 ticks at the nominal 10 Hz governor cadence ≈ 5 seconds of cooldown,
/// giving the EMA time to converge before the backstop considers another cut.
pub const BACKSTOP_COOLDOWN_TICKS: u32 = 50;

/// Loss-backstop congestion controller.
///
/// One instance per active stream.  Cheap to construct; zero heap allocation.
///
/// ## Usage
///
/// ```rust
/// use lowband_lbtp::congestion::LossBackstop;
/// use lowband_lbtp::fec::GilbertElliottEstimator;
/// use lowband_lbtp::pacer::Pacer;
///
/// let mut pacer = Pacer::new(500_000.0);
/// let mut backstop = LossBackstop::new();
/// let mut estimator = GilbertElliottEstimator::new();
///
/// // Feed packet observations into the estimator (transport event loop).
/// // ...
///
/// // Each control tick:
/// if let Some(new_rate) = backstop.check(pacer.rate_bps(), &estimator) {
///     pacer.set_rate(new_rate);
/// }
/// ```
#[derive(Debug)]
pub struct LossBackstop {
    /// Ticks remaining in the cooldown window.  Zero means the backstop may fire.
    cooldown_remaining: u32,
}

impl Default for LossBackstop {
    fn default() -> Self {
        Self::new()
    }
}

impl LossBackstop {
    /// Create a new backstop controller, ready to fire immediately.
    pub fn new() -> Self {
        Self { cooldown_remaining: 0 }
    }

    /// Evaluate loss conditions and optionally return a reduced send rate.
    ///
    /// Must be called once per control tick (10 Hz typical).
    ///
    /// Returns `Some(new_rate_bps)` when the backstop fires; the caller must
    /// apply the returned value via [`Pacer::set_rate`].  Returns `None` when
    /// conditions do not warrant a reduction or the cooldown is still active.
    ///
    /// # Arguments
    ///
    /// * `current_rate_bps` — the pacer's current send rate in bits per second.
    /// * `estimator` — the stream's loss estimator; the backstop reads
    ///   `observation_count()` and `loss_rate()`.
    pub fn check(
        &mut self,
        current_rate_bps: f64,
        estimator: &GilbertElliottEstimator,
    ) -> Option<f64> {
        // Advance cooldown timer regardless of other conditions.
        if self.cooldown_remaining > 0 {
            self.cooldown_remaining -= 1;
            return None;
        }

        // Require a warm estimator before acting.
        if estimator.observation_count() < MIN_OBS_FOR_ESTIMATE {
            return None;
        }

        // Only engage when sustained loss exceeds the threshold.
        if estimator.loss_rate() <= LOSS_BACKSTOP_THRESHOLD {
            return None;
        }

        let new_rate = (current_rate_bps * LOSS_BACKSTOP_REDUCTION).max(BACKSTOP_MIN_RATE_BPS);
        self.cooldown_remaining = BACKSTOP_COOLDOWN_TICKS;
        Some(new_rate)
    }

    /// Ticks remaining in the active cooldown window.
    ///
    /// Zero means the backstop is eligible to fire on the next `check` call
    /// (subject to loss and observation conditions).
    pub fn cooldown_remaining(&self) -> u32 {
        self.cooldown_remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{GilbertElliottEstimator, MIN_OBS_FOR_ESTIMATE};

    const RATE_500K: f64 = 500_000.0;

    /// Returns a warmed estimator with approximately `loss_fraction` packet loss.
    fn warm_estimator(loss_every: u32) -> GilbertElliottEstimator {
        let mut est = GilbertElliottEstimator::with_alphas(0.1, 0.25);
        let n = MIN_OBS_FOR_ESTIMATE as u32 * 10;
        for i in 0..n {
            est.observe(loss_every == 0 || i % loss_every != 0);
        }
        est
    }

    fn clean_estimator() -> GilbertElliottEstimator {
        warm_estimator(0) // no losses
    }

    fn lossy_estimator_15pct() -> GilbertElliottEstimator {
        // ~15 % loss: lose every 7th packet
        warm_estimator(7)
    }

    fn lossy_estimator_5pct() -> GilbertElliottEstimator {
        // ~5 % loss: lose every 20th packet (below threshold)
        warm_estimator(20)
    }

    // ── Preconditions: no fire when not yet warranted ─────────────────────

    #[test]
    fn no_reduction_when_estimator_cold() {
        let mut backstop = LossBackstop::new();
        let cold = GilbertElliottEstimator::new(); // 0 observations
        assert!(
            backstop.check(RATE_500K, &cold).is_none(),
            "must not fire when estimator has fewer than MIN_OBS_FOR_ESTIMATE observations"
        );
    }

    #[test]
    fn no_reduction_on_clean_channel() {
        let mut backstop = LossBackstop::new();
        let est = clean_estimator();
        assert!(
            backstop.check(RATE_500K, &est).is_none(),
            "must not fire on a clean channel"
        );
    }

    #[test]
    fn no_reduction_when_loss_below_threshold() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_5pct(); // ~5 % loss
        assert!(
            backstop.check(RATE_500K, &est).is_none(),
            "must not fire when loss is below the 10% threshold"
        );
    }

    // ── Trigger: fires when sustained loss exceeds threshold ─────────────

    #[test]
    fn fires_when_sustained_loss_exceeds_threshold() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        let result = backstop.check(RATE_500K, &est);
        assert!(result.is_some(), "must fire when sustained loss > 10%");
    }

    #[test]
    fn reduced_rate_is_current_times_reduction_factor() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        let new_rate = backstop.check(RATE_500K, &est).unwrap();
        let expected = RATE_500K * LOSS_BACKSTOP_REDUCTION;
        assert!(
            (new_rate - expected).abs() < 0.01,
            "rate {new_rate} should be current × LOSS_BACKSTOP_REDUCTION ({expected})"
        );
    }

    #[test]
    fn reduced_rate_never_below_minimum() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        // Drive rate down to just above the floor.
        let low_rate = BACKSTOP_MIN_RATE_BPS + 1.0;
        let new_rate = backstop.check(low_rate, &est).unwrap();
        assert!(
            new_rate >= BACKSTOP_MIN_RATE_BPS,
            "rate {new_rate} must not drop below BACKSTOP_MIN_RATE_BPS"
        );
    }

    #[test]
    fn reduced_rate_clamped_exactly_to_minimum_when_already_at_floor() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        let at_floor = BACKSTOP_MIN_RATE_BPS;
        let new_rate = backstop.check(at_floor, &est).unwrap();
        assert!(
            (new_rate - BACKSTOP_MIN_RATE_BPS).abs() < 0.01,
            "rate must stay at floor when already at minimum"
        );
    }

    // ── Cooldown: suppresses repeat reductions ────────────────────────────

    #[test]
    fn cooldown_set_after_trigger() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        backstop.check(RATE_500K, &est);
        assert_eq!(
            backstop.cooldown_remaining(),
            BACKSTOP_COOLDOWN_TICKS,
            "cooldown must be set to BACKSTOP_COOLDOWN_TICKS after firing"
        );
    }

    #[test]
    fn no_reduction_during_cooldown() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();

        // First check fires and starts cooldown.
        let first = backstop.check(RATE_500K, &est);
        assert!(first.is_some(), "first check must fire");

        // Every subsequent check during cooldown must return None.
        for tick in 0..BACKSTOP_COOLDOWN_TICKS {
            let result = backstop.check(RATE_500K, &est);
            assert!(
                result.is_none(),
                "check at cooldown tick {tick} must return None"
            );
        }
    }

    #[test]
    fn cooldown_decrements_each_tick() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        backstop.check(RATE_500K, &est); // trigger; cooldown = BACKSTOP_COOLDOWN_TICKS

        let clean = clean_estimator();
        for expected in (0..BACKSTOP_COOLDOWN_TICKS).rev() {
            backstop.check(RATE_500K, &clean); // tick without triggering
            assert_eq!(backstop.cooldown_remaining(), expected);
        }
    }

    #[test]
    fn fires_again_after_cooldown_expires() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();

        backstop.check(RATE_500K, &est); // trigger

        // Burn through the cooldown with clean-channel checks.
        let clean = clean_estimator();
        for _ in 0..BACKSTOP_COOLDOWN_TICKS {
            backstop.check(RATE_500K, &clean);
        }

        // Now the backstop is eligible again.
        let second = backstop.check(RATE_500K, &est);
        assert!(second.is_some(), "must fire again after cooldown expires");
    }

    #[test]
    fn cooldown_zero_at_construction() {
        let backstop = LossBackstop::new();
        assert_eq!(backstop.cooldown_remaining(), 0);
    }

    #[test]
    fn default_equals_new() {
        let a = LossBackstop::new();
        let b = LossBackstop::default();
        assert_eq!(a.cooldown_remaining(), b.cooldown_remaining());
    }

    // ── Rate progression under repeated backstop triggers ────────────────

    #[test]
    fn rate_decreases_monotonically_across_multiple_triggers() {
        let mut backstop = LossBackstop::new();
        let est = lossy_estimator_15pct();
        let clean = clean_estimator();

        let mut rate = RATE_500K;
        let mut prev = rate;

        for _ in 0..5 {
            if let Some(new_rate) = backstop.check(rate, &est) {
                assert!(
                    new_rate <= prev,
                    "rate must not increase after a backstop trigger: {prev} → {new_rate}"
                );
                prev = new_rate;
                rate = new_rate;
            }
            // Burn cooldown before the next trigger.
            for _ in 0..BACKSTOP_COOLDOWN_TICKS {
                backstop.check(rate, &clean);
            }
        }

        assert!(
            rate < RATE_500K,
            "rate must have dropped after multiple triggers"
        );
        assert!(
            rate >= BACKSTOP_MIN_RATE_BPS,
            "rate must not drop below minimum"
        );
    }
}

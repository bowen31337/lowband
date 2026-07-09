//! Fast tier downgrade on BWE collapse — Feature 70.
//!
//! When the bandwidth estimate falls below
//! [`DOWNGRADE_TRIGGER_RATIO`] × the current tier's floor the governor must
//! downgrade the session tier on the **next** 10 Hz tick.  At one tick per
//! 100 ms this keeps the response within the 200 ms SLA without any hold counter.
//!
//! # Contrast with the upgrade guard
//!
//! [`crate::tier_upgrade_guard::TierUpgradeGuard`] requires 50 consecutive
//! 10 Hz ticks of sustained headroom (5 seconds) before it permits an upgrade,
//! to prevent thrashing on transient spikes.  Downgrades must be the opposite:
//! **no hold period**, because a congested link can deteriorate fatally within
//! a few hundred milliseconds.
//!
//! # Tier floors and downgrade triggers
//!
//! | Tier        | Floor (`tier_floor_bps`)  | Trigger (0.8 × floor) |
//! |-------------|---------------------------|-----------------------|
//! | Full        | [`FULL_FLOOR_BPS`]        | 204 800 bps           |
//! | Comfortable | [`COMFORTABLE_FLOOR_BPS`] | 102 400 bps           |
//! | Constrained | [`CONSTRAINED_FLOOR_BPS`] |  51 200 bps           |
//! | Survival    | —  (bottom tier)          | never downgrades      |
//!
//! # Usage
//!
//! ```rust
//! use lowband_platform::tier_downgrade_guard::{
//!     TierDowngradeGuard, CONSTRAINED_FLOOR_BPS, DOWNGRADE_TRIGGER_RATIO,
//! };
//! use lowband_platform::TierState;
//!
//! let guard = TierDowngradeGuard::new();
//!
//! // BWE at the floor — no downgrade.
//! assert_eq!(
//!     guard.observe(TierState::Constrained, CONSTRAINED_FLOOR_BPS),
//!     TierState::Constrained,
//! );
//!
//! // BWE drops to 80% of floor (exactly at trigger) — still no downgrade.
//! let trigger = (CONSTRAINED_FLOOR_BPS as f64 * DOWNGRADE_TRIGGER_RATIO) as u32;
//! assert_eq!(
//!     guard.observe(TierState::Constrained, trigger),
//!     TierState::Constrained,
//! );
//!
//! // BWE drops one bps below trigger — immediate one-step downgrade.
//! assert_eq!(
//!     guard.observe(TierState::Constrained, trigger - 1),
//!     TierState::Survival,
//! );
//! ```

use crate::tier::TierState;

/// Fraction of the tier floor below which a downgrade is triggered immediately.
///
/// The 0.8 ratio (80 %) provides hysteresis: upgrades require BWE at or above
/// the full floor, while the downgrade does not fire until BWE has fallen to
/// below this fraction of the floor.  This prevents rapid oscillation at tier
/// boundaries where the BWE estimate fluctuates by a few percent.
pub const DOWNGRADE_TRIGGER_RATIO: f64 = 0.8;

/// Minimum BWE (bps) required to sustain [`TierState::Constrained`].
///
/// Below [`DOWNGRADE_TRIGGER_RATIO`] × this value (51 200 bps) the governor
/// must immediately downgrade to [`TierState::Survival`].
pub const CONSTRAINED_FLOOR_BPS: u32 = 64_000;

/// Minimum BWE (bps) required to sustain [`TierState::Comfortable`].
///
/// Below [`DOWNGRADE_TRIGGER_RATIO`] × this value (102 400 bps) the governor
/// must immediately downgrade to [`TierState::Constrained`].
pub const COMFORTABLE_FLOOR_BPS: u32 = 128_000;

/// Minimum BWE (bps) required to sustain [`TierState::Full`].
///
/// Below [`DOWNGRADE_TRIGGER_RATIO`] × this value (204 800 bps) the governor
/// must immediately downgrade to [`TierState::Comfortable`].
pub const FULL_FLOOR_BPS: u32 = 256_000;

/// Returns the downgrade trigger threshold (bps) for `tier`, or `None` at
/// [`TierState::Survival`] (no further downgrade is possible).
///
/// The threshold is [`DOWNGRADE_TRIGGER_RATIO`] × [`tier_floor_bps`], computed
/// with integer arithmetic as `floor * 4 / 5`.
///
/// | Tier        | Threshold |
/// |-------------|-----------|
/// | Full        | 204 800   |
/// | Comfortable | 102 400   |
/// | Constrained |  51 200   |
/// | Survival    | None      |
#[inline]
pub fn downgrade_trigger_bps(tier: TierState) -> Option<u32> {
    tier_floor_bps(tier).map(|floor| floor * 4 / 5)
}

/// Returns the BWE floor (bps) for `tier`, or `None` at [`TierState::Survival`]
/// (already at the bottom tier — no downgrade is possible).
#[inline]
pub fn tier_floor_bps(tier: TierState) -> Option<u32> {
    match tier {
        TierState::Survival    => None,
        TierState::Constrained => Some(CONSTRAINED_FLOOR_BPS),
        TierState::Comfortable => Some(COMFORTABLE_FLOOR_BPS),
        TierState::Full        => Some(FULL_FLOOR_BPS),
    }
}

/// Fast tier downgrade guard — Feature 70.
///
/// Stateless: each call to [`observe`](TierDowngradeGuard::observe) is
/// independent of prior calls.  The governor calls this once per 10 Hz tick;
/// a BWE below the trigger threshold produces an immediate one-step downgrade,
/// satisfying the 200 ms SLA.
///
/// One instance per active session.  Zero heap allocation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TierDowngradeGuard;

impl TierDowngradeGuard {
    /// Create a new downgrade guard.
    pub fn new() -> Self {
        Self
    }

    /// Evaluate one 10 Hz governor tick and return the tier to emit.
    ///
    /// # Arguments
    ///
    /// * `current_tier` — the tier the session is currently running at.
    /// * `bwe_bps` — the current bandwidth estimate in bits per second, as
    ///   reported by the congestion controller this tick.
    ///
    /// # Returns
    ///
    /// * `current_tier` when BWE is at or above
    ///   [`downgrade_trigger_bps`]`(current_tier)`.
    /// * One tier step below `current_tier` when BWE falls **strictly below**
    ///   the trigger.  Only one step is taken per tick regardless of how far
    ///   BWE has collapsed.
    /// * `TierState::Survival` (unchanged) when already at the bottom tier.
    pub fn observe(&self, current_tier: TierState, bwe_bps: u32) -> TierState {
        let Some(trigger) = downgrade_trigger_bps(current_tier) else {
            return current_tier;
        };

        if bwe_bps < trigger {
            tier_step_down(current_tier)
        } else {
            current_tier
        }
    }
}

/// Returns the tier one step below `tier`.
///
/// `Survival` has no lower tier, so it returns itself — callers that checked
/// `downgrade_trigger_bps` first will never reach this with `Survival`.
#[inline]
fn tier_step_down(tier: TierState) -> TierState {
    match tier {
        TierState::Full        => TierState::Comfortable,
        TierState::Comfortable => TierState::Constrained,
        TierState::Constrained => TierState::Survival,
        TierState::Survival    => TierState::Survival,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tier_floor_bps ────────────────────────────────────────────────────────

    #[test]
    fn survival_has_no_floor() {
        assert!(
            tier_floor_bps(TierState::Survival).is_none(),
            "Survival is the bottom tier — no floor means no downgrade"
        );
    }

    #[test]
    fn constrained_floor_is_64kbps() {
        assert_eq!(tier_floor_bps(TierState::Constrained), Some(CONSTRAINED_FLOOR_BPS));
        assert_eq!(CONSTRAINED_FLOOR_BPS, 64_000);
    }

    #[test]
    fn comfortable_floor_is_128kbps() {
        assert_eq!(tier_floor_bps(TierState::Comfortable), Some(COMFORTABLE_FLOOR_BPS));
        assert_eq!(COMFORTABLE_FLOOR_BPS, 128_000);
    }

    #[test]
    fn full_floor_is_256kbps() {
        assert_eq!(tier_floor_bps(TierState::Full), Some(FULL_FLOOR_BPS));
        assert_eq!(FULL_FLOOR_BPS, 256_000);
    }

    #[test]
    fn floors_are_strictly_increasing() {
        assert!(CONSTRAINED_FLOOR_BPS < COMFORTABLE_FLOOR_BPS);
        assert!(COMFORTABLE_FLOOR_BPS < FULL_FLOOR_BPS);
    }

    // ── downgrade_trigger_bps ─────────────────────────────────────────────────

    #[test]
    fn trigger_is_80_percent_of_floor() {
        // Verify trigger = floor × 4 / 5 for each tier.
        let cases = [
            (TierState::Constrained, CONSTRAINED_FLOOR_BPS),
            (TierState::Comfortable, COMFORTABLE_FLOOR_BPS),
            (TierState::Full,        FULL_FLOOR_BPS),
        ];
        for (tier, floor) in cases {
            let expected = floor * 4 / 5;
            assert_eq!(
                downgrade_trigger_bps(tier),
                Some(expected),
                "{tier:?}: trigger must be 80% of {floor} bps"
            );
        }
    }

    #[test]
    fn constrained_trigger_is_51200_bps() {
        assert_eq!(downgrade_trigger_bps(TierState::Constrained), Some(51_200));
    }

    #[test]
    fn comfortable_trigger_is_102400_bps() {
        assert_eq!(downgrade_trigger_bps(TierState::Comfortable), Some(102_400));
    }

    #[test]
    fn full_trigger_is_204800_bps() {
        assert_eq!(downgrade_trigger_bps(TierState::Full), Some(204_800));
    }

    #[test]
    fn survival_trigger_is_none() {
        assert!(downgrade_trigger_bps(TierState::Survival).is_none());
    }

    // ── observe: no downgrade above trigger ───────────────────────────────────

    #[test]
    fn no_downgrade_at_floor() {
        let guard = TierDowngradeGuard::new();
        for (tier, floor) in [
            (TierState::Constrained, CONSTRAINED_FLOOR_BPS),
            (TierState::Comfortable, COMFORTABLE_FLOOR_BPS),
            (TierState::Full,        FULL_FLOOR_BPS),
        ] {
            assert_eq!(
                guard.observe(tier, floor),
                tier,
                "{tier:?}: must not downgrade when BWE equals the floor"
            );
        }
    }

    #[test]
    fn no_downgrade_exactly_at_trigger() {
        // The trigger is a strict lower bound: BWE < trigger fires the downgrade.
        // BWE == trigger must NOT downgrade.
        let guard = TierDowngradeGuard::new();
        for tier in [TierState::Constrained, TierState::Comfortable, TierState::Full] {
            let trigger = downgrade_trigger_bps(tier).unwrap();
            assert_eq!(
                guard.observe(tier, trigger),
                tier,
                "{tier:?}: must not downgrade when BWE equals the trigger (strict <)"
            );
        }
    }

    #[test]
    fn no_downgrade_well_above_floor() {
        let guard = TierDowngradeGuard::new();
        assert_eq!(guard.observe(TierState::Constrained, 400_000), TierState::Constrained);
        assert_eq!(guard.observe(TierState::Comfortable,  400_000), TierState::Comfortable);
        assert_eq!(guard.observe(TierState::Full,         400_000), TierState::Full);
    }

    // ── observe: immediate downgrade below trigger ────────────────────────────

    #[test]
    fn downgrade_one_below_trigger() {
        let guard = TierDowngradeGuard::new();
        let cases = [
            (TierState::Constrained, TierState::Survival),
            (TierState::Comfortable, TierState::Constrained),
            (TierState::Full,        TierState::Comfortable),
        ];
        for (tier, expected) in cases {
            let trigger = downgrade_trigger_bps(tier).unwrap();
            let result = guard.observe(tier, trigger - 1);
            assert_eq!(
                result, expected,
                "{tier:?}: BWE one below trigger must produce immediate downgrade to {expected:?}"
            );
        }
    }

    #[test]
    fn downgrade_at_zero_bwe() {
        // Even a complete link failure produces a one-step downgrade, not a jump.
        let guard = TierDowngradeGuard::new();
        assert_eq!(guard.observe(TierState::Full,        0), TierState::Comfortable);
        assert_eq!(guard.observe(TierState::Comfortable, 0), TierState::Constrained);
        assert_eq!(guard.observe(TierState::Constrained, 0), TierState::Survival);
    }

    #[test]
    fn downgrade_is_exactly_one_step() {
        // Full → Comfortable (not Survival) even when BWE = 0.
        let guard = TierDowngradeGuard::new();
        let result = guard.observe(TierState::Full, 0);
        assert_eq!(
            result,
            TierState::Comfortable,
            "downgrade from Full must land at Comfortable, not skip tiers"
        );
    }

    // ── observe: Survival is the floor ───────────────────────────────────────

    #[test]
    fn survival_never_downgrades() {
        let guard = TierDowngradeGuard::new();
        for bwe in [0u32, 1, 100, 48_000, u32::MAX] {
            assert_eq!(
                guard.observe(TierState::Survival, bwe),
                TierState::Survival,
                "Survival is the bottom tier — must never downgrade (bwe={bwe})"
            );
        }
    }

    // ── Full upgrade ladder reversed: Full → Survival ─────────────────────────

    #[test]
    fn full_descent_requires_one_step_per_tick() {
        // Simulates three consecutive 10 Hz governor ticks at BWE = 0.
        // The guard is stateless so the caller drives the descent one step per tick.
        let guard = TierDowngradeGuard::new();
        let mut tier = TierState::Full;

        let expected_steps = [
            TierState::Comfortable,
            TierState::Constrained,
            TierState::Survival,
        ];

        for expected in expected_steps {
            tier = guard.observe(tier, 0);
            assert_eq!(tier, expected, "descent must step one tier at a time");
        }

        // Survival: no further descent.
        assert_eq!(guard.observe(tier, 0), TierState::Survival);
    }

    // ── Default equals new ────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        assert_eq!(TierDowngradeGuard::new(), TierDowngradeGuard::default());
    }

    // ── Boundary: BWE exactly between floor and trigger ──────────────────────

    #[test]
    fn bwe_between_trigger_and_floor_does_not_downgrade() {
        let guard = TierDowngradeGuard::new();
        // Mid-point between trigger (51 200) and floor (64 000) = 57 600.
        let mid_bwe = (CONSTRAINED_FLOOR_BPS + downgrade_trigger_bps(TierState::Constrained).unwrap()) / 2;
        assert_eq!(
            guard.observe(TierState::Constrained, mid_bwe),
            TierState::Constrained,
            "BWE between trigger and floor ({mid_bwe} bps) must not trigger a downgrade"
        );
    }
}

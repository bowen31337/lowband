//! Probe-validated tier upgrade guard — Feature 71.
//!
//! The governor must not upgrade the session tier on a momentary bandwidth
//! spike.  [`TierUpgradeGuard::observe`] gates tier upgrades behind a 5-second
//! hold: the BWE must sustain headroom above the next tier's threshold for
//! [`UPGRADE_HOLD_TICKS`] consecutive 10 Hz ticks before an upgrade is
//! permitted.
//!
//! Downgrades bypass this guard entirely — Feature 70 handles those with a
//! fast 200 ms path that does not go through this module.
//!
//! # Upgrade rules
//!
//! * **One step at a time**: Survival → Constrained → Comfortable → Full.
//!   Even if the BWE suddenly suggests `Full`, the guard only permits one step.
//! * **Counter resets on any gap**: a single tick where headroom drops below
//!   the next tier resets the counter to 0.
//! * **Counter resets after each granted upgrade**: the next step requires
//!   another full 5-second hold.
//!
//! # Usage
//!
//! ```rust
//! use lowband_platform::tier_upgrade_guard::TierUpgradeGuard;
//! use lowband_platform::TierState;
//!
//! let mut guard = TierUpgradeGuard::new();
//!
//! // 49 ticks of headroom — upgrade is still blocked.
//! for _ in 0..49 {
//!     let out = guard.observe(TierState::Survival, TierState::Constrained);
//!     assert_eq!(out, TierState::Survival);
//! }
//! // Tick 50: upgrade is permitted.
//! assert_eq!(
//!     guard.observe(TierState::Survival, TierState::Constrained),
//!     TierState::Constrained,
//! );
//! ```

use crate::tier::TierState;

/// Consecutive 10 Hz governor ticks that probe-validated headroom must be
/// sustained before a tier upgrade is permitted (Feature 71).
///
/// 50 ticks × 100 ms/tick = 5 seconds.
pub const UPGRADE_HOLD_TICKS: u32 = 50;

/// Guard that prevents tier upgrades until probe-validated headroom has been
/// sustained for [`UPGRADE_HOLD_TICKS`] consecutive governor ticks.
///
/// One instance per active session.  Zero heap allocation.
///
/// See module documentation for the upgrade vs. downgrade split and the
/// one-step-at-a-time rule.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TierUpgradeGuard {
    /// Consecutive ticks for which BWE headroom has been sufficient for an
    /// upgrade.  Resets to 0 whenever headroom drops or an upgrade is granted.
    hold_ticks: u32,
}

impl TierUpgradeGuard {
    /// Create a new guard with no accumulated hold time.
    pub fn new() -> Self {
        Self { hold_ticks: 0 }
    }

    /// Evaluate one 10 Hz governor tick and return the tier to emit.
    ///
    /// # Arguments
    ///
    /// * `current_tier` — the tier the session is currently running at.
    /// * `candidate_tier` — the tier the raw BWE would support this tick,
    ///   computed by the governor from the bandwidth estimate before applying
    ///   this guard.
    ///
    /// # Returns
    ///
    /// * `current_tier` while the hold count has not yet reached
    ///   [`UPGRADE_HOLD_TICKS`], or when headroom is insufficient.
    /// * One tier step above `current_tier` on the tick that completes the
    ///   5-second hold.
    ///
    /// Upgrades advance exactly one tier step.  The hold counter resets to 0
    /// after each permitted upgrade.
    pub fn observe(&mut self, current_tier: TierState, candidate_tier: TierState) -> TierState {
        let Some(next_tier) = tier_step_up(current_tier) else {
            // Already at Full — no upgrade possible.
            self.hold_ticks = 0;
            return current_tier;
        };

        if candidate_tier >= next_tier {
            self.hold_ticks += 1;
            if self.hold_ticks >= UPGRADE_HOLD_TICKS {
                self.hold_ticks = 0;
                return next_tier;
            }
        } else {
            // Headroom dropped; restart the hold.
            self.hold_ticks = 0;
        }

        current_tier
    }

    /// Consecutive ticks of sustained headroom accumulated toward the next
    /// upgrade.
    ///
    /// Returns 0 when the guard is idle (current tier is `Full`, headroom has
    /// never been observed, or the counter was just reset).  Resets to 0
    /// after a permitted upgrade.
    #[inline]
    pub fn hold_ticks(&self) -> u32 {
        self.hold_ticks
    }
}

/// Returns the tier one step above `tier`, or `None` when already at `Full`.
#[inline]
fn tier_step_up(tier: TierState) -> Option<TierState> {
    match tier {
        TierState::Survival    => Some(TierState::Constrained),
        TierState::Constrained => Some(TierState::Comfortable),
        TierState::Comfortable => Some(TierState::Full),
        TierState::Full        => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Upgrade blocked until UPGRADE_HOLD_TICKS ─────────────────────────────

    #[test]
    fn upgrade_blocked_for_49_ticks() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..49 {
            let out = guard.observe(TierState::Survival, TierState::Constrained);
            assert_eq!(
                out, TierState::Survival,
                "upgrade must be blocked until UPGRADE_HOLD_TICKS ticks have elapsed"
            );
        }
        assert_eq!(guard.hold_ticks(), 49);
    }

    #[test]
    fn upgrade_permitted_at_exactly_50_ticks() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..49 {
            guard.observe(TierState::Survival, TierState::Constrained);
        }
        let out = guard.observe(TierState::Survival, TierState::Constrained);
        assert_eq!(out, TierState::Constrained, "tier must step up on the 50th tick");
    }

    // ── Counter resets when headroom drops ────────────────────────────────────

    #[test]
    fn reset_when_headroom_drops() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..30 {
            guard.observe(TierState::Survival, TierState::Constrained);
        }
        // Headroom drops for one tick.
        guard.observe(TierState::Survival, TierState::Survival);
        assert_eq!(guard.hold_ticks(), 0, "counter must reset when headroom drops");
    }

    #[test]
    fn upgrade_requires_50_consecutive_ticks_after_reset() {
        let mut guard = TierUpgradeGuard::new();
        // Accumulate 40 ticks of headroom…
        for _ in 0..40 {
            guard.observe(TierState::Survival, TierState::Constrained);
        }
        // …then headroom collapses for one tick.
        guard.observe(TierState::Survival, TierState::Survival);
        // 49 ticks of headroom after the reset must still not upgrade.
        for i in 0..49 {
            let out = guard.observe(TierState::Survival, TierState::Constrained);
            assert_eq!(
                out, TierState::Survival,
                "tick {i} after reset must not upgrade"
            );
        }
        // The 50th tick relative to the reset should upgrade.
        let out = guard.observe(TierState::Survival, TierState::Constrained);
        assert_eq!(out, TierState::Constrained, "must upgrade at tick 50 after reset");
    }

    // ── One tier step at a time ───────────────────────────────────────────────

    #[test]
    fn upgrade_one_step_even_if_candidate_is_full() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..49 {
            guard.observe(TierState::Survival, TierState::Full);
        }
        let out = guard.observe(TierState::Survival, TierState::Full);
        assert_eq!(
            out, TierState::Constrained,
            "must advance exactly one tier step regardless of how high the candidate is"
        );
    }

    #[test]
    fn higher_candidate_satisfies_the_hold_for_one_step() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..49 {
            guard.observe(TierState::Constrained, TierState::Full);
        }
        let out = guard.observe(TierState::Constrained, TierState::Full);
        assert_eq!(
            out, TierState::Comfortable,
            "Full candidate must still produce a one-step upgrade to Comfortable"
        );
    }

    // ── Counter resets after a granted upgrade ────────────────────────────────

    #[test]
    fn counter_resets_after_upgrade_and_needs_50_more() {
        let mut guard = TierUpgradeGuard::new();
        // Complete the first upgrade (Survival → Constrained).
        for _ in 0..50 {
            guard.observe(TierState::Survival, TierState::Comfortable);
        }
        // Now at Constrained: 49 more ticks must still be blocked.
        for i in 0..49 {
            let out = guard.observe(TierState::Constrained, TierState::Comfortable);
            assert_eq!(
                out, TierState::Constrained,
                "tick {i} toward second upgrade must still be blocked"
            );
        }
        let out = guard.observe(TierState::Constrained, TierState::Comfortable);
        assert_eq!(out, TierState::Comfortable, "second upgrade must occur at tick 50");
    }

    // ── No upgrade when already at Full ──────────────────────────────────────

    #[test]
    fn no_upgrade_at_full() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..100 {
            let out = guard.observe(TierState::Full, TierState::Full);
            assert_eq!(out, TierState::Full, "no upgrade possible above Full");
        }
        assert_eq!(guard.hold_ticks(), 0, "hold counter must stay 0 when already at Full");
    }

    // ── hold_ticks accessor ───────────────────────────────────────────────────

    #[test]
    fn hold_ticks_zero_initially() {
        assert_eq!(TierUpgradeGuard::new().hold_ticks(), 0);
    }

    #[test]
    fn hold_ticks_increments_each_tick() {
        let mut guard = TierUpgradeGuard::new();
        for expected in 1..=10 {
            guard.observe(TierState::Survival, TierState::Constrained);
            assert_eq!(guard.hold_ticks(), expected);
        }
    }

    #[test]
    fn hold_ticks_zero_after_upgrade() {
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..50 {
            guard.observe(TierState::Survival, TierState::Constrained);
        }
        assert_eq!(guard.hold_ticks(), 0, "hold counter must reset to 0 after upgrade");
    }

    // ── Default equals new ────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        assert_eq!(
            TierUpgradeGuard::new().hold_ticks(),
            TierUpgradeGuard::default().hold_ticks()
        );
    }

    // ── Edge: no-headroom candidate never advances counter ───────────────────

    #[test]
    fn candidate_equal_to_current_does_not_advance_counter() {
        // candidate_tier == current_tier means no headroom for upgrade.
        let mut guard = TierUpgradeGuard::new();
        for _ in 0..100 {
            let out = guard.observe(TierState::Constrained, TierState::Constrained);
            assert_eq!(out, TierState::Constrained);
        }
        assert_eq!(guard.hold_ticks(), 0);
    }

    // ── Full upgrade ladder: Survival → Full takes 4 × 50 ticks ─────────────

    #[test]
    fn full_upgrade_ladder_requires_200_total_ticks() {
        let mut guard = TierUpgradeGuard::new();
        let mut tier = TierState::Survival;

        let steps = [
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ];

        for &expected_next in &steps {
            let mut upgraded = false;
            for _ in 0..50 {
                let out = guard.observe(tier, TierState::Full);
                if out != tier {
                    assert_eq!(out, expected_next, "unexpected tier step");
                    tier = out;
                    upgraded = true;
                    break;
                }
            }
            assert!(upgraded, "must have upgraded to {expected_next:?} within 50 ticks");
        }

        assert_eq!(tier, TierState::Full, "must have reached Full after three upgrades");
    }
}

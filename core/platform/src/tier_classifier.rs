//! Governor tier classification and emission — Feature 68.
//!
//! The governor emits a [`TierState`] once per 10 Hz interval.  That emission
//! is produced in three steps:
//!
//! 1. **BWE classification** — [`classify_bwe_tier`] maps the raw bandwidth
//!    estimate to one of the four tiers using the floor thresholds.
//!
//! 2. **Fast downgrade** (Feature 70) — if the BWE has fallen below
//!    0.8 × the current tier's floor, the tier steps down immediately on
//!    this tick.  No hold counter.
//!
//! 3. **Probe-validated upgrade** (Feature 71) — an upgrade is permitted only
//!    after [`UPGRADE_HOLD_TICKS`] consecutive 10 Hz ticks of sufficient
//!    headroom.
//!
//! [`GovernorTierEmitter`] wires these together into the single entry point
//! the governor calls once per tick.
//!
//! # Tier thresholds
//!
//! | BWE range (bps)                             | Emitted tier  |
//! |---------------------------------------------|---------------|
//! | ≥ [`FULL_FLOOR_BPS`] (256 kbps)             | Full          |
//! | ≥ [`COMFORTABLE_FLOOR_BPS`] (128 kbps)      | Comfortable   |
//! | ≥ [`CONSTRAINED_FLOOR_BPS`] (64 kbps)       | Constrained   |
//! | < [`CONSTRAINED_FLOOR_BPS`]                 | Survival      |
//!
//! # Usage
//!
//! ```rust
//! use lowband_platform::tier_classifier::{classify_bwe_tier, GovernorTierEmitter};
//! use lowband_platform::tier_downgrade_guard::CONSTRAINED_FLOOR_BPS;
//! use lowband_platform::TierState;
//!
//! // Raw BWE → candidate tier.
//! assert_eq!(classify_bwe_tier(400_000), TierState::Full);
//! assert_eq!(classify_bwe_tier(64_000),  TierState::Constrained);
//! assert_eq!(classify_bwe_tier(48_000),  TierState::Survival);
//!
//! // Combined emitter: downgrade is immediate; upgrade requires a 5-second hold.
//! let mut emitter = GovernorTierEmitter::new();
//! let mut tier = TierState::Constrained;
//!
//! // BWE well above Constrained floor — no upgrade yet (need 50 ticks).
//! for _ in 0..49 {
//!     tier = emitter.tick(tier, 200_000);
//!     assert_eq!(tier, TierState::Constrained);
//! }
//! // 50th tick: probe-validated headroom → upgrade to Comfortable.
//! tier = emitter.tick(tier, 200_000);
//! assert_eq!(tier, TierState::Comfortable);
//! ```

use crate::tier::TierState;
use crate::tier_downgrade_guard::{
    TierDowngradeGuard, COMFORTABLE_FLOOR_BPS, CONSTRAINED_FLOOR_BPS, FULL_FLOOR_BPS,
};
use crate::tier_upgrade_guard::TierUpgradeGuard;

// ── classify_bwe_tier ─────────────────────────────────────────────────────────

/// Map a raw bandwidth estimate to the highest [`TierState`] the link can sustain.
///
/// The mapping uses the tier floor thresholds from [`tier_downgrade_guard`]:
///
/// | BWE (bps)                            | Returned tier |
/// |--------------------------------------|---------------|
/// | ≥ [`FULL_FLOOR_BPS`] (256 kbps)      | Full          |
/// | ≥ [`COMFORTABLE_FLOOR_BPS`] (128 kbps)| Comfortable  |
/// | ≥ [`CONSTRAINED_FLOOR_BPS`] (64 kbps) | Constrained  |
/// | < 64 kbps                            | Survival      |
///
/// This is the *raw candidate* tier before the hysteretic guards are applied.
/// Callers that want the guarded emission should use [`GovernorTierEmitter::tick`].
#[inline]
pub fn classify_bwe_tier(bwe_bps: u32) -> TierState {
    if bwe_bps >= FULL_FLOOR_BPS {
        TierState::Full
    } else if bwe_bps >= COMFORTABLE_FLOOR_BPS {
        TierState::Comfortable
    } else if bwe_bps >= CONSTRAINED_FLOOR_BPS {
        TierState::Constrained
    } else {
        TierState::Survival
    }
}

// ── GovernorTierEmitter ───────────────────────────────────────────────────────

/// Governor tier emitter — Feature 68.
///
/// Produces the [`TierState`] emitted each 10 Hz governor interval by
/// combining the fast downgrade guard (Feature 70) with the probe-validated
/// upgrade guard (Feature 71).
///
/// One instance per active session.  The only heap-free mutable state is the
/// upgrade guard's hold counter.
///
/// # Tick semantics
///
/// Call [`tick`](GovernorTierEmitter::tick) exactly once per 10 Hz governor
/// interval, passing the tier that was emitted on the *previous* tick and the
/// current bandwidth estimate.  The returned tier is the value to emit this
/// interval and to pass as `current_tier` on the next tick.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GovernorTierEmitter {
    upgrade_guard: TierUpgradeGuard,
}

impl GovernorTierEmitter {
    /// Create a new emitter with no accumulated upgrade hold time.
    pub fn new() -> Self {
        Self { upgrade_guard: TierUpgradeGuard::new() }
    }

    /// Evaluate one 10 Hz governor tick and return the [`TierState`] to emit.
    ///
    /// # Arguments
    ///
    /// * `current_tier` — the tier emitted on the previous interval.  On the
    ///   first call, pass the initial tier (typically inferred from the opening
    ///   BWE sample via [`classify_bwe_tier`]).
    /// * `bwe_bps` — the bandwidth estimate (bps) reported by the congestion
    ///   controller this interval.
    ///
    /// # Returns
    ///
    /// The tier to emit this interval.  Feed this value back as `current_tier`
    /// on the next call.
    ///
    /// # Downgrade priority
    ///
    /// A downgrade (Feature 70) fires immediately: if
    /// `bwe_bps < 0.8 × tier_floor(current_tier)`, the returned tier is one
    /// step below `current_tier`, and the upgrade hold counter is reset so the
    /// new tier must hold for a fresh 5 seconds before any upgrade is considered.
    ///
    /// # Upgrade gating
    ///
    /// When no downgrade fires, the upgrade guard (Feature 71) checks whether
    /// the BWE candidate is sufficient for the next-higher tier and has been so
    /// for [`UPGRADE_HOLD_TICKS`](crate::tier_upgrade_guard::UPGRADE_HOLD_TICKS)
    /// consecutive ticks.  Only then does the returned tier step up.
    pub fn tick(&mut self, current_tier: TierState, bwe_bps: u32) -> TierState {
        let candidate = classify_bwe_tier(bwe_bps);

        // Fast downgrade (Feature 70) — stateless, fires immediately.
        let after_downgrade = TierDowngradeGuard::new().observe(current_tier, bwe_bps);

        if after_downgrade < current_tier {
            // A downgrade occurred: reset the upgrade hold so the demoted tier
            // must sustain headroom for a fresh 5-second window before any
            // upgrade is considered.
            self.upgrade_guard = TierUpgradeGuard::new();
            return after_downgrade;
        }

        // No downgrade: probe-validated upgrade (Feature 71).
        self.upgrade_guard.observe(current_tier, candidate)
    }

    /// The number of consecutive 10 Hz ticks of sufficient headroom accumulated
    /// toward the next upgrade.
    ///
    /// Delegates to [`TierUpgradeGuard::hold_ticks`].  Useful for diagnostics
    /// and tests.
    #[inline]
    pub fn upgrade_hold_ticks(&self) -> u32 {
        self.upgrade_guard.hold_ticks()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_bwe_tier ─────────────────────────────────────────────────────

    #[test]
    fn classify_survival_below_constrained_floor() {
        assert_eq!(classify_bwe_tier(0), TierState::Survival);
        assert_eq!(classify_bwe_tier(1), TierState::Survival);
        assert_eq!(classify_bwe_tier(CONSTRAINED_FLOOR_BPS - 1), TierState::Survival);
    }

    #[test]
    fn classify_constrained_at_and_above_floor() {
        assert_eq!(classify_bwe_tier(CONSTRAINED_FLOOR_BPS), TierState::Constrained);
        assert_eq!(classify_bwe_tier(CONSTRAINED_FLOOR_BPS + 1), TierState::Constrained);
        assert_eq!(classify_bwe_tier(COMFORTABLE_FLOOR_BPS - 1), TierState::Constrained);
    }

    #[test]
    fn classify_comfortable_at_and_above_floor() {
        assert_eq!(classify_bwe_tier(COMFORTABLE_FLOOR_BPS), TierState::Comfortable);
        assert_eq!(classify_bwe_tier(COMFORTABLE_FLOOR_BPS + 1), TierState::Comfortable);
        assert_eq!(classify_bwe_tier(FULL_FLOOR_BPS - 1), TierState::Comfortable);
    }

    #[test]
    fn classify_full_at_and_above_floor() {
        assert_eq!(classify_bwe_tier(FULL_FLOOR_BPS), TierState::Full);
        assert_eq!(classify_bwe_tier(FULL_FLOOR_BPS + 1), TierState::Full);
        assert_eq!(classify_bwe_tier(u32::MAX), TierState::Full);
    }

    #[test]
    fn classify_boundaries_are_exact_floor_values() {
        // Each floor is inclusive: bwe == floor → that tier.
        assert_eq!(classify_bwe_tier(CONSTRAINED_FLOOR_BPS), TierState::Constrained,
            "CONSTRAINED_FLOOR_BPS must classify as Constrained");
        assert_eq!(classify_bwe_tier(COMFORTABLE_FLOOR_BPS), TierState::Comfortable,
            "COMFORTABLE_FLOOR_BPS must classify as Comfortable");
        assert_eq!(classify_bwe_tier(FULL_FLOOR_BPS), TierState::Full,
            "FULL_FLOOR_BPS must classify as Full");
    }

    #[test]
    fn classify_is_monotone_with_bwe() {
        // Increasing BWE must not decrease the tier.
        let bwe_samples = [0u32, 32_000, 64_000, 100_000, 128_000, 200_000, 256_000, 400_000];
        let mut prev = classify_bwe_tier(bwe_samples[0]);
        for &bwe in &bwe_samples[1..] {
            let tier = classify_bwe_tier(bwe);
            assert!(
                tier >= prev,
                "classify_bwe_tier must be monotone: bwe={bwe} yielded {tier:?} < {prev:?}"
            );
            prev = tier;
        }
    }

    // ── GovernorTierEmitter — upgrade path ────────────────────────────────────

    #[test]
    fn upgrade_blocked_for_49_ticks() {
        let mut emitter = GovernorTierEmitter::new();
        let mut tier = TierState::Survival;
        for _ in 0..49 {
            tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
            assert_eq!(tier, TierState::Survival,
                "upgrade must be blocked until 50 ticks of headroom");
        }
        assert_eq!(emitter.upgrade_hold_ticks(), 49);
    }

    #[test]
    fn upgrade_granted_on_tick_50() {
        let mut emitter = GovernorTierEmitter::new();
        let mut tier = TierState::Survival;
        for _ in 0..49 {
            tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        }
        tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        assert_eq!(tier, TierState::Constrained, "upgrade must fire on the 50th tick");
    }

    #[test]
    fn upgrade_hold_counter_resets_when_headroom_drops() {
        let mut emitter = GovernorTierEmitter::new();
        let mut tier = TierState::Survival;
        for _ in 0..30 {
            tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        }
        // Headroom drops: BWE below Constrained floor.
        tier = emitter.tick(tier, 0);
        assert_eq!(emitter.upgrade_hold_ticks(), 0,
            "hold counter must reset when headroom drops");
        assert_eq!(tier, TierState::Survival);
    }

    // ── GovernorTierEmitter — downgrade path ──────────────────────────────────

    #[test]
    fn downgrade_fires_immediately_below_trigger() {
        let mut emitter = GovernorTierEmitter::new();
        let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5; // 51 200 bps
        let result = emitter.tick(TierState::Constrained, trigger - 1);
        assert_eq!(result, TierState::Survival,
            "BWE one below trigger must produce immediate downgrade");
    }

    #[test]
    fn downgrade_resets_upgrade_hold() {
        // Accumulate 30 ticks of headroom toward Comfortable…
        let mut emitter = GovernorTierEmitter::new();
        let mut tier = TierState::Constrained;
        for _ in 0..30 {
            tier = emitter.tick(tier, COMFORTABLE_FLOOR_BPS); // headroom for Comfortable
        }
        assert_eq!(emitter.upgrade_hold_ticks(), 30);

        // …then a downgrade fires.
        let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5;
        tier = emitter.tick(tier, trigger - 1);
        assert_eq!(tier, TierState::Survival, "downgrade must fire");
        assert_eq!(emitter.upgrade_hold_ticks(), 0,
            "downgrade must reset the upgrade hold counter");
    }

    #[test]
    fn downgrade_and_upgrade_never_fire_in_same_tick() {
        // Downgrade has priority: if both guards would fire, downgrade wins.
        let mut emitter = GovernorTierEmitter::new();
        // Accumulate full 50-tick hold for Constrained → Comfortable.
        let tier = TierState::Constrained;
        for _ in 0..49 {
            emitter.tick(tier, COMFORTABLE_FLOOR_BPS);
        }
        // On the 50th tick the upgrade would normally fire. But instead we
        // supply a BWE that also triggers a downgrade.
        let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5;
        let result = emitter.tick(tier, trigger - 1);
        assert_eq!(result, TierState::Survival,
            "downgrade must win over an upgrade that reached its hold count in the same tick");
    }

    // ── GovernorTierEmitter — hysteresis ──────────────────────────────────────

    #[test]
    fn tier_stable_in_hysteresis_zone() {
        // BWE between trigger and floor: no downgrade (above trigger),
        // no upgrade (below next-tier floor), tier is stable.
        let mut emitter = GovernorTierEmitter::new();
        let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5; // 51 200
        // bwe = 55 000: above trigger (51 200) but below floor (64 000).
        let bwe_hysteresis = trigger + 3_800; // 55 000
        let mut tier = TierState::Constrained;
        for _ in 0..100 {
            tier = emitter.tick(tier, bwe_hysteresis);
        }
        assert_eq!(tier, TierState::Constrained,
            "tier must be stable in the hysteresis zone for 100 ticks");
    }

    // ── All four tiers reachable ──────────────────────────────────────────────

    #[test]
    fn all_four_tiers_are_reachable_via_tick() {
        let mut emitter = GovernorTierEmitter::new();
        let mut tier = TierState::Survival;

        // Drive upgrade through all three steps (Survival → Full).
        let upgrade_bw = FULL_FLOOR_BPS + 10_000;
        loop {
            let next = emitter.tick(tier, upgrade_bw);
            if next == tier { break; } // 50-tick hold completed for this step
            tier = next;
        }
        // After the loop completes when the tier stops changing at Full.
        // Actually we may need more iterations. Let's be explicit:
        let mut emitter2 = GovernorTierEmitter::new();
        let mut tier2 = TierState::Survival;
        let mut reached = std::collections::HashSet::new();
        reached.insert(tier2);
        for _ in 0..200 {
            let next = emitter2.tick(tier2, upgrade_bw);
            tier2 = next;
            reached.insert(tier2);
        }
        assert!(reached.contains(&TierState::Survival));
        assert!(reached.contains(&TierState::Constrained));
        assert!(reached.contains(&TierState::Comfortable));
        assert!(reached.contains(&TierState::Full));
    }

    // ── Default == new ────────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        assert_eq!(GovernorTierEmitter::new(), GovernorTierEmitter::default());
    }
}

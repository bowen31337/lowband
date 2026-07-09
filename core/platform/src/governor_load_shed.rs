//! Governor load-shedding policy — Feature 318.
//!
//! When the bandwidth estimate collapses to the Survival tier the governor
//! freezes bulk_transfer headroom as the **first** action before adjusting
//! any other stream.
//!
//! # Shedding order at Survival
//!
//! | Priority | Stream               | Action                                  |
//! |----------|----------------------|-----------------------------------------|
//! | 1st shed | bulk_transfer (xfer) | Headroom → 0 (frozen, this module)      |
//! | 2nd shed | camera               | Allocator returns 0 at low bandwidth    |
//! | 3rd shed | screen refinement    | Allocator returns 0 at low bandwidth    |
//! | Never    | voice + input        | Always funded above floor               |
//!
//! Bulk_transfer is frozen **first** so that RaptorQ repair symbols queued in
//! the `BulkTransferScheduler` never compete with the voice + input + screen
//! coarse minimum at the 48 kbps survival floor.  Paused transfers resume from
//! their current position once the tier recovers to Constrained or above.

use crate::tier::TierState;

/// Effective bulk_transfer headroom to grant to the `BulkTransferScheduler`.
///
/// When `tier` is [`TierState::Survival`] returns `0` unconditionally —
/// bulk_transfer is the first stream frozen on a Survival-tier collapse,
/// regardless of any nominally available bandwidth.  Calling this before any
/// other stream adjustment implements the "frozen first" contract.
///
/// For all other tiers the `proposed_bps` is returned unchanged; the normal
/// governor allocation path ([`crate::gear_policy::allocate`]) determines how
/// much headroom the xfer channel may use.
///
/// # Usage
///
/// The governor calls this once per 10 Hz tick, after classifying the current
/// tier and before passing the result to `BulkTransferScheduler::set_headroom`:
///
/// ```ignore
/// let effective = bulk_xfer_headroom_bps(tier, budgets.xfer_bps);
/// scheduler.set_headroom(effective as usize / 10); // bps → bytes per 100 ms
/// ```
pub fn bulk_xfer_headroom_bps(tier: TierState, proposed_bps: u32) -> u32 {
    match tier {
        TierState::Survival => 0,
        _ => proposed_bps,
    }
}

/// Whether bulk_transfer must be frozen at `tier`.
///
/// Returns `true` only at [`TierState::Survival`].  Callers that need a
/// boolean guard (rather than the zero-vs-proposed byte value) can use this
/// directly.
#[inline]
pub fn bulk_xfer_frozen(tier: TierState) -> bool {
    tier == TierState::Survival
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── bulk_xfer_frozen ──────────────────────────────────────────────────────

    #[test]
    fn frozen_at_survival() {
        assert!(
            bulk_xfer_frozen(TierState::Survival),
            "bulk_transfer must be frozen at Survival tier"
        );
    }

    #[test]
    fn not_frozen_at_constrained() {
        assert!(!bulk_xfer_frozen(TierState::Constrained));
    }

    #[test]
    fn not_frozen_at_comfortable() {
        assert!(!bulk_xfer_frozen(TierState::Comfortable));
    }

    #[test]
    fn not_frozen_at_full() {
        assert!(!bulk_xfer_frozen(TierState::Full));
    }

    // ── bulk_xfer_headroom_bps ────────────────────────────────────────────────

    #[test]
    fn headroom_is_zero_at_survival_regardless_of_proposed() {
        for proposed in [0u32, 1, 1_000, 50_000, 400_000, u32::MAX] {
            assert_eq!(
                bulk_xfer_headroom_bps(TierState::Survival, proposed),
                0,
                "Feature 318: bulk_xfer must be frozen at Survival \
                 regardless of proposed headroom ({proposed} bps)"
            );
        }
    }

    #[test]
    fn headroom_passes_through_at_constrained() {
        assert_eq!(bulk_xfer_headroom_bps(TierState::Constrained, 10_000), 10_000);
        assert_eq!(bulk_xfer_headroom_bps(TierState::Constrained, 0), 0);
    }

    #[test]
    fn headroom_passes_through_at_comfortable() {
        assert_eq!(bulk_xfer_headroom_bps(TierState::Comfortable, 50_000), 50_000);
    }

    #[test]
    fn headroom_passes_through_at_full() {
        assert_eq!(bulk_xfer_headroom_bps(TierState::Full, 100_000), 100_000);
    }

    #[test]
    fn survival_freeze_is_unconditional() {
        // Even if the allocator produced a large xfer budget (e.g. at Critical
        // thermal where camera is off), the Survival-tier freeze must win.
        let large_budget = 200_000u32;
        assert_eq!(
            bulk_xfer_headroom_bps(TierState::Survival, large_budget),
            0,
            "Survival freeze must override even a large proposed headroom"
        );
    }
}

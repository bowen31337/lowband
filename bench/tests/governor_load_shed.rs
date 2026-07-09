//! Feature 318 — system sheds load with bulk_transfer frozen first when the
//! estimate collapses to survival tier.
//!
//! # What this test verifies
//!
//! 1. **Policy: Survival → headroom zero** — `bulk_xfer_headroom_bps` returns
//!    `0` at `TierState::Survival` regardless of the proposed headroom, and
//!    passes the proposed value unchanged at every higher tier.
//!
//! 2. **"Frozen first" order** — at Survival tier the bulk_transfer headroom is
//!    zeroed even when `allocate()` would leave non-zero leftover (e.g. Critical
//!    thermal with camera off).  No other stream is adjusted before xfer is
//!    frozen; this is tested by calling `bulk_xfer_headroom_bps` before any
//!    other stream adjustment and verifying xfer = 0.
//!
//! 3. **Scheduler integration** — when the governor applies the frozen headroom
//!    (`0` bytes) to a `BulkTransferScheduler` that holds queued frames, the
//!    scheduler returns `HeldForHeadroom`, not `SendAggregated`.  Queued frames
//!    are preserved — they drain once the tier recovers.
//!
//! 4. **Recovery** — after the tier rises to Constrained the governor grants
//!    non-zero headroom; queued frames drain normally.
//!
//! 5. **Voice floor preserved** — across the full Comfortable → Survival tier
//!    collapse, the voice allocation never drops below `AUDIO_FLOOR_BPS`.
//!
//! # Architecture contract
//!
//! From the spec (Feature 318): "System sheds load with bulk_transfer frozen
//! first when the estimate collapses to survival tier."  Bulk_transfer is the
//! lowest-priority data stream; freezing it first protects voice, input, and the
//! screen coarse lane without a keyframe burst or resync packet.

use lowband_platform::{
    bulk_xfer_frozen, bulk_xfer_headroom_bps,
    gear_policy::{allocate, GearConstraints, AUDIO_FLOOR_BPS},
    thermal::ThermalPressure,
    TierState,
};
use lowband_xfer::scheduler::{
    BulkTransferScheduler, PacerDemand, TickResult, XferFrame,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn no_demand() -> PacerDemand {
    PacerDemand { voice_pending: 0, input_pending: 0 }
}

fn make_frame(size: usize) -> XferFrame {
    XferFrame::new(vec![0u8; size], 1, 0)
}

// ── 1. Policy: survival → headroom zero ──────────────────────────────────────

#[test]
fn survival_tier_freezes_bulk_xfer_headroom() {
    // Any non-zero proposed headroom must be zeroed at Survival tier.
    for proposed in [1u32, 100, 10_000, 50_000, 400_000] {
        let effective = bulk_xfer_headroom_bps(TierState::Survival, proposed);
        assert_eq!(
            effective, 0,
            "Feature 318: bulk_transfer headroom must be 0 at Survival (proposed={proposed} bps)"
        );
    }
}

#[test]
fn bulk_xfer_frozen_returns_true_only_at_survival() {
    assert!(bulk_xfer_frozen(TierState::Survival));
    assert!(!bulk_xfer_frozen(TierState::Constrained));
    assert!(!bulk_xfer_frozen(TierState::Comfortable));
    assert!(!bulk_xfer_frozen(TierState::Full));
}

#[test]
fn non_survival_tiers_pass_headroom_through() {
    let proposed = 30_000u32;
    for tier in [TierState::Constrained, TierState::Comfortable, TierState::Full] {
        let effective = bulk_xfer_headroom_bps(tier, proposed);
        assert_eq!(
            effective, proposed,
            "headroom must be unchanged at {tier:?}"
        );
    }
}

// ── 2. "Frozen first" order ───────────────────────────────────────────────────

/// At Survival tier, even when allocate() returns a non-zero xfer_bps (because
/// camera is off and bandwidth remains), the freeze policy reduces it to zero
/// before any other stream adjustment is made.
#[test]
fn survival_freeze_wins_even_when_allocate_has_xfer_leftover() {
    // Critical thermal: camera is off, screen refinement is off.
    // At a moderate bandwidth the allocator may leave budget as xfer headroom.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Critical);
    let budgets = allocate(150_000, &constraints);

    // Confirm the precondition: allocate produces non-zero xfer at this BW.
    assert!(
        budgets.xfer_bps > 0,
        "precondition: allocate must leave non-zero xfer headroom at 150 kbps / Critical; \
         got {} bps",
        budgets.xfer_bps
    );

    // The governor classifies this as Survival and applies the freeze first.
    let effective = bulk_xfer_headroom_bps(TierState::Survival, budgets.xfer_bps);
    assert_eq!(
        effective, 0,
        "Feature 318: Survival-tier freeze must override the allocator's xfer_bps ({} bps)",
        budgets.xfer_bps
    );
}

// ── 3. Scheduler integration ──────────────────────────────────────────────────

/// When the governor applies a frozen headroom (0 bytes) to the scheduler, any
/// queued frames are held — not dropped.  The queue depth is unchanged.
#[test]
fn scheduler_held_for_headroom_when_survival_freeze_applied() {
    let mut sched = BulkTransferScheduler::new();
    let frame_size = 500usize;
    sched.enqueue(make_frame(frame_size));
    sched.enqueue(make_frame(frame_size));

    // Governor detects Survival tier and applies the frozen headroom.
    let effective = bulk_xfer_headroom_bps(TierState::Survival, 50_000);
    sched.set_headroom(effective as usize);

    // Scheduler must block on headroom, not send.
    let result = sched.tick(no_demand());
    assert!(
        matches!(result, TickResult::HeldForHeadroom),
        "scheduler must return HeldForHeadroom when Survival freeze is applied; got {result:?}"
    );

    // Frames must be preserved — not consumed during the freeze.
    assert_eq!(
        sched.queued_frames(),
        2,
        "queued frames must be preserved during Survival-tier freeze"
    );
}

/// The headroom balance after a survival-tier freeze remains zero — the
/// scheduler does not carry forward any pre-freeze credit.
#[test]
fn headroom_is_zero_after_survival_freeze() {
    let mut sched = BulkTransferScheduler::new();

    // Give the scheduler a large headroom then immediately freeze it.
    sched.set_headroom(100_000);
    let frozen = bulk_xfer_headroom_bps(TierState::Survival, 100_000);
    sched.set_headroom(frozen as usize);

    assert_eq!(
        sched.headroom_remaining(),
        0,
        "headroom_remaining must be zero after Survival freeze overwrites it"
    );
}

// ── 4. Recovery ───────────────────────────────────────────────────────────────

/// After the tier recovers to Constrained the governor grants non-zero headroom
/// and the scheduler drains queued frames normally.
#[test]
fn frames_drain_after_tier_recovers_from_survival() {
    let mut sched = BulkTransferScheduler::new();

    sched.enqueue(make_frame(400));
    sched.enqueue(make_frame(400));

    // Phase 1: Survival — headroom is frozen to 0 regardless of proposed budget.
    let proposed_bps = 30_000u32;
    let frozen = bulk_xfer_headroom_bps(TierState::Survival, proposed_bps);
    assert_eq!(frozen, 0);
    sched.set_headroom(frozen as usize);
    assert!(matches!(sched.tick(no_demand()), TickResult::HeldForHeadroom));
    assert_eq!(sched.queued_frames(), 2, "frames must wait during Survival freeze");

    // Phase 2: tier recovers to Constrained — governor passes through the same
    // proposed budget without freezing it.
    let recovered = bulk_xfer_headroom_bps(TierState::Constrained, proposed_bps);
    assert_eq!(recovered, proposed_bps, "Constrained must pass proposed headroom through");
    sched.set_headroom(recovered as usize);

    // Frames must drain (the scheduler may coalesce them into one datagram).
    let mut sent = 0usize;
    for _ in 0..10 {
        match sched.tick(no_demand()) {
            TickResult::SendAggregated(agg) => sent += agg.frames.len(),
            TickResult::Idle => break,
            TickResult::HeldForHeadroom | TickResult::HeldForPriority => {}
        }
    }
    assert_eq!(
        sent, 2,
        "both queued frames must drain after tier recovers from Survival"
    );
    assert_eq!(sched.queued_frames(), 0);
}

// ── 5. Voice floor preserved across tier collapse ─────────────────────────────

/// Across a Comfortable → Constrained → Survival bandwidth collapse, the
/// voice allocation must never drop below AUDIO_FLOOR_BPS.
#[test]
fn voice_floor_preserved_across_survival_collapse() {
    let tiers_and_bw: &[(u32, TierState)] = &[
        (400_000, TierState::Full),
        (150_000, TierState::Comfortable),
        (64_000,  TierState::Constrained),
        (48_000,  TierState::Survival),
        (12_000,  TierState::Survival),
    ];

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);

    for &(bw, tier) in tiers_and_bw {
        let budgets = allocate(bw, &constraints);

        // Voice floor invariant.
        assert!(
            budgets.audio_bps >= AUDIO_FLOOR_BPS,
            "voice floor violated at bw={bw} tier={tier:?}: got {} bps",
            budgets.audio_bps
        );

        // Bulk_transfer is frozen at Survival regardless of allocator output.
        let effective_xfer = bulk_xfer_headroom_bps(tier, budgets.xfer_bps);
        if tier == TierState::Survival {
            assert_eq!(
                effective_xfer, 0,
                "xfer must be frozen at Survival (bw={bw})"
            );
        }
    }
}

// ── 6. Survival is the only frozen tier ──────────────────────────────────────

/// Exhaustive check: Survival is the unique tier at which bulk_transfer is
/// frozen.  Constrained still allows bulk_transfer to use its allocated budget.
#[test]
fn exactly_one_tier_triggers_freeze() {
    let frozen_tiers: Vec<TierState> = [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ]
    .into_iter()
    .filter(|&t| bulk_xfer_frozen(t))
    .collect();

    assert_eq!(
        frozen_tiers,
        vec![TierState::Survival],
        "exactly one tier must freeze bulk_transfer: Survival (Feature 318)"
    );
}

// ── 7. Zero proposed is zero effective at all tiers ──────────────────────────

#[test]
fn zero_headroom_stays_zero_at_all_tiers() {
    for tier in [TierState::Survival, TierState::Constrained, TierState::Comfortable, TierState::Full] {
        assert_eq!(
            bulk_xfer_headroom_bps(tier, 0),
            0,
            "zero proposed headroom must produce zero effective at {tier:?}"
        );
    }
}

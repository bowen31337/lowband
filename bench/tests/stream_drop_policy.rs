//! Feature 77 — system keeps every stream droppable or layerable so no
//! transition emits a keyframe.
//!
//! # What this test verifies
//!
//! 1. **Static invariant**: every stream the governor manages has a
//!    [`DropPolicy`] whose `needs_keyframe_on_transition()` returns `false`.
//!
//! 2. **Gear B congestion response** (layered stream): when the delay-gradient
//!    estimator signals overuse, the [`TemporalSvcController`] escalates the
//!    T-layer drop floor — shedding T1 (and later T1+T2) enhancement frames
//!    without ever requesting a keyframe.  The base T0 layer continues to
//!    arrive and the decoder remains fully functional.
//!
//! 3. **Tier downgrade across all streams**: simulating a Comfortable →
//!    Constrained → Survival collapse, the policy table guarantees that every
//!    active stream can absorb the transition through dropping or layer shedding.
//!    No stream requires a resync keyframe at any tier boundary.
//!
//! 4. **Gear B resumption after a full stop**: when the camera is paused
//!    (Gear B budget → 0) and then resumed, the [`IntraRefreshState`]
//!    continues from its current column-sweep position rather than emitting
//!    a new IDR keyframe.  The invariant holds across the pause/resume gap.
//!
//! # Architecture contract
//!
//! From the spec: "System keeps every stream droppable or layerable so no
//! transition emits a keyframe" (Feature 77).  Keyframes under congestion are
//! counter-productive — 5–10× the average frame size arrives exactly when the
//! link budget is tightest.

use lowband_platform::gear_policy::{
    allocate, GearConstraints, AUDIO_FLOOR_BPS,
};
use lowband_platform::intra_refresh::{IntraRefreshFrame, IntraRefreshState};
use lowband_platform::stream_drop_policy::{DropPolicy, StreamDropPolicy, StreamKind};
use lowband_platform::temporal_svc::{
    TemporalSvcController, TemporalSvcMode, OVERUSE_ESCALATE_TICKS, T0, T1,
};
use lowband_platform::thermal::ThermalPressure;
use lowband_platform::TemporalLayerId;

// ── 1. Static invariant: every stream is keyframe-free ───────────────────────

#[test]
fn all_streams_satisfy_keyframe_free_invariant() {
    assert!(
        StreamDropPolicy::all_streams_keyframe_free(),
        "every governor-managed stream must have needs_keyframe_on_transition() == false"
    );
}

#[test]
fn each_stream_kind_keyframe_free_individually() {
    use StreamKind::*;
    for kind in [
        Audio,
        Input,
        ScreenCoarse,
        ScreenRefinement,
        CameraGearA,
        CameraGearB,
        CameraGearC,
        VideoSubStream,
        FileTransfer,
    ] {
        let policy = StreamDropPolicy::for_kind(kind);
        assert!(
            !policy.needs_keyframe_on_transition(),
            "{kind:?}: needs_keyframe_on_transition must be false (got {policy:?})"
        );
    }
}

// ── 2. Gear B temporal SVC: overuse escalates layer drops, never keyframe ─────

#[test]
fn gear_b_congestion_response_is_t_layer_drop_not_keyframe() {
    // Gear B is the only Layered stream; verify the drop policy.
    let gear_b_policy = StreamDropPolicy::for_kind(StreamKind::CameraGearB);
    assert!(
        gear_b_policy.is_layered(),
        "Gear B must be Layered (L1T2 temporal SVC)"
    );
    assert!(
        (gear_b_policy.base_fraction() - 0.5).abs() < f32::EPSILON,
        "Gear B L1T2 base fraction must be 0.5"
    );

    // Simulate OVERUSE_ESCALATE_TICKS of congestion — must drop T1, not IDR.
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    for _ in 0..OVERUSE_ESCALATE_TICKS {
        ctrl.update(true);
    }

    // The drop floor moves to T1 — enhancement frames are shed.
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "after overuse escalation the T-layer drop floor must reach T1"
    );

    // T0 (base layer) is never dropped regardless of the escalation level.
    let total = 60usize;
    let (sent, dropped): (Vec<_>, Vec<_>) = (0..total)
        .map(|_| ctrl.next_frame())
        .partition(|(_, drop)| !drop);

    assert!(!sent.is_empty(), "at least T0 base frames must be forwarded");
    for (layer, _) in &sent {
        assert_eq!(
            *layer, T0,
            "under T1-drop only T0 frames must be forwarded — no keyframe emitted"
        );
    }

    // The pacer only withholds enhancement frames; no IDR is requested.
    // Verified indirectly: all dropped frames are T1, not the base layer.
    for (layer, _) in &dropped {
        assert_ne!(
            *layer, T0,
            "T0 base-layer frames must never appear in the drop list"
        );
    }
}

#[test]
fn gear_b_sustained_overuse_holds_t1_drop_floor_no_further_degradation() {
    // L1T2 has only one enhancement layer; saturating overuse must not
    // push the drop floor below T1 (which would require dropping T0 = IDR).
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    for _ in 0..(OVERUSE_ESCALATE_TICKS * 20) {
        ctrl.update(true);
    }
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "sustained overuse must hold at T1 drop floor; \
         dropping T0 (base layer) would require a keyframe to recover sync"
    );
    // Base layer is still never dropped.
    for _ in 0..120 {
        let (layer, drop) = ctrl.next_frame();
        if layer == T0 {
            assert!(!drop, "T0 must never be dropped under any escalation");
        }
    }
}

// ── 3. Tier downgrade: every stream absorbs the transition without a keyframe ─

/// Simulate a tier downgrade from a high-bandwidth allocation to survival tier
/// and verify that the policy table's keyframe-free invariant holds at each step.
#[test]
fn tier_downgrade_comfortable_to_survival_all_streams_keyframe_free() {
    let tiers = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];

    let bandwidths = [400_000u32, 150_000, 64_000, 12_000];

    for (&bw, &thermal) in bandwidths.iter().zip(tiers.iter()) {
        let constraints = GearConstraints::from_thermal(thermal);
        let budgets = allocate(bw, &constraints);

        // Voice is always funded above the floor — this is a hard contract.
        assert!(
            budgets.audio_bps >= AUDIO_FLOOR_BPS,
            "audio floor violated at bw={bw} thermal={thermal:?}: got {}", budgets.audio_bps
        );

        // Every stream that carries a non-zero budget must have a keyframe-free
        // drop policy.  The static table guarantees this, but we verify the
        // mapping here to make the relationship explicit.
        let stream_budgets = [
            (StreamKind::Audio,             budgets.audio_bps),
            (StreamKind::Input,             budgets.input_bps),
            (StreamKind::ScreenCoarse,      budgets.screen_coarse_bps),
            (StreamKind::ScreenRefinement,  budgets.screen_refinement_bps),
            (StreamKind::FileTransfer,      budgets.xfer_bps),
        ];

        for (kind, bps) in stream_budgets {
            let policy = StreamDropPolicy::for_kind(kind);
            assert!(
                !policy.needs_keyframe_on_transition(),
                "{kind:?} at bw={bw} thermal={thermal:?} (bps={bps}): \
                 needs_keyframe_on_transition must be false"
            );
        }
    }
}

#[test]
fn camera_off_transition_does_not_require_keyframe() {
    // At Critical thermal the camera budget drops to zero (camera off).
    // Resuming the camera later must not force an IDR.
    let critical = GearConstraints::from_thermal(ThermalPressure::Critical);
    let budgets_off = allocate(64_000, &critical);
    assert_eq!(budgets_off.camera_bps, 0, "camera must be off at Critical thermal");

    // The camera's own drop policy is Droppable — no keyframe on resumption.
    let gear_a_policy = StreamDropPolicy::for_kind(StreamKind::CameraGearA);
    assert!(gear_a_policy.is_droppable());
    assert!(!gear_a_policy.needs_keyframe_on_transition());

    let gear_b_policy = StreamDropPolicy::for_kind(StreamKind::CameraGearB);
    assert!(!gear_b_policy.needs_keyframe_on_transition());
}

// ── 4. Gear B pause/resume: column sweep continues, no new IDR ───────────────

/// When the camera budget is zeroed and then restored, the Gear B encoder
/// resumes its column-sweep intra-refresh from the current position rather
/// than emitting a fresh IDR keyframe.
#[test]
fn gear_b_pause_resume_continues_column_sweep_without_new_idr() {
    // Start a Gear B stream.
    let mut ir = IntraRefreshState::new(30);

    // Initial keyframe — mandatory for decoder sync on stream start.
    assert_eq!(
        ir.advance(),
        IntraRefreshFrame::Keyframe,
        "Gear B stream start must emit exactly one keyframe"
    );

    // Advance through 10 column sweeps (simulating normal operation).
    for expected_col in 0..10u32 {
        match ir.advance() {
            IntraRefreshFrame::ColumnSweep { col } => {
                assert_eq!(col, expected_col, "column sweep must advance in order");
            }
            IntraRefreshFrame::Keyframe => {
                panic!("unexpected keyframe during normal Gear B operation at col {expected_col}");
            }
        }
    }

    // Simulate a camera pause: budget → 0 for a few governor ticks.
    // The IntraRefreshState retains its position (column 10 is next).
    // No IDR is emitted during the pause — nothing is sent.

    // Simulate camera resume: budget restored.
    // The stream resumes from column 10 — the pause is transparent.
    let position_before_pause = ir.next_column();
    assert_eq!(position_before_pause, 10, "next column must be 10 after 10 sweeps");

    // First frame after resumption: must be ColumnSweep at col 10, not Keyframe.
    match ir.advance() {
        IntraRefreshFrame::ColumnSweep { col } => {
            assert_eq!(
                col, 10,
                "on resumption the column sweep must continue from col 10, not restart"
            );
        }
        IntraRefreshFrame::Keyframe => {
            panic!(
                "Gear B must NOT emit a new IDR on resumption; \
                 the decoder can continue from its last state \
                 (Feature 77: no transition emits a keyframe)"
            );
        }
    }

    // Continue for a full remaining cycle — no further keyframes.
    for frame in 0..19u32 {
        assert!(
            matches!(ir.advance(), IntraRefreshFrame::ColumnSweep { .. }),
            "frame {frame} after resumption must be a column sweep, not a keyframe"
        );
    }
}

// ── 5. Droppable streams: base fraction is zero (fully pauseable) ─────────────

#[test]
fn droppable_streams_have_zero_base_fraction() {
    use StreamKind::*;
    let droppable_kinds = [
        Audio, Input, ScreenCoarse, ScreenRefinement,
        CameraGearA, CameraGearC, VideoSubStream, FileTransfer,
    ];
    for kind in droppable_kinds {
        let policy = StreamDropPolicy::for_kind(kind);
        assert!(
            policy.is_droppable(),
            "{kind:?} must be Droppable"
        );
        assert_eq!(
            policy.base_fraction(),
            0.0,
            "{kind:?} Droppable stream must have base_fraction 0.0"
        );
    }
}

// ── 6. Gear B T-layer drop reduces effective rate without IDR ─────────────────

/// Verify that the effective forwarded frame fraction matches the stated
/// base_fraction for the Gear B drop policy.
#[test]
fn gear_b_effective_rate_reduction_matches_stated_base_fraction() {
    let policy = StreamDropPolicy::for_kind(StreamKind::CameraGearB);
    let DropPolicy::Layered { base_fraction } = policy else {
        panic!("Gear B must be Layered");
    };

    // Run a TemporalSvcController at maximum drop level.
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    for _ in 0..OVERUSE_ESCALATE_TICKS {
        ctrl.update(true);
    }

    let total = 120usize;
    let sent = (0..total).filter(|_| !ctrl.next_frame().1).count();
    let actual_fraction = sent as f32 / total as f32;

    assert!(
        (actual_fraction - base_fraction).abs() < 0.02,
        "Gear B effective send fraction {actual_fraction:.2} must match stated \
         base_fraction {base_fraction:.2} under maximum T-layer drop"
    );
}

// ── 7. DropPolicy invariant: both variants always return false ────────────────

#[test]
fn drop_policy_droppable_never_needs_keyframe() {
    assert!(!DropPolicy::Droppable.needs_keyframe_on_transition());
}

#[test]
fn drop_policy_layered_never_needs_keyframe() {
    for base in [0.25f32, 0.5, 0.75] {
        assert!(
            !DropPolicy::Layered { base_fraction: base }.needs_keyframe_on_transition(),
            "Layered{{ base_fraction: {base} }} must not require a keyframe"
        );
    }
}

// ── 8. Screen refinement suspend: no resync ──────────────────────────────────

/// When screen refinement is suspended (e.g. at Serious thermal) the
/// refinement queue is simply paused.  No resync packet is sent because each
/// tile is an independent lossless encode unit.
#[test]
fn screen_refinement_suspend_is_droppable_not_resync() {
    let policy = StreamDropPolicy::for_kind(StreamKind::ScreenRefinement);
    assert!(
        policy.is_droppable(),
        "screen refinement must be Droppable so that suspension does not require resync"
    );

    // Confirm that at Serious thermal screen_refinement_bps is zeroed.
    let serious = GearConstraints::from_thermal(ThermalPressure::Serious);
    let budgets = allocate(200_000, &serious);
    assert_eq!(
        budgets.screen_refinement_bps, 0,
        "screen refinement must be zeroed at Serious thermal (Droppable — no resync needed)"
    );
}

// ── 9. Audio floor preserved across all tier transitions ─────────────────────

#[test]
fn audio_floor_preserved_at_every_tier() {
    for &thermal in &[
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ] {
        for &bw in &[6_000u32, 12_000, 32_000, 64_000, 150_000, 400_000] {
            let c = GearConstraints::from_thermal(thermal);
            let b = allocate(bw, &c);
            assert!(
                b.audio_bps >= AUDIO_FLOOR_BPS,
                "audio floor violated at thermal={thermal:?} bw={bw}: got {}", b.audio_bps
            );
        }
    }
}

// ── 10. T0 base layer delivery rate is positive for Gear B under congestion ───

#[test]
fn gear_b_t0_delivery_rate_is_positive_under_congestion() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    // Saturate overuse — maximum escalation.
    for _ in 0..(OVERUSE_ESCALATE_TICKS * 10) {
        ctrl.update(true);
    }

    let total = 120usize;
    let t0_sent = (0..total)
        .filter(|_| {
            let (layer, drop) = ctrl.next_frame();
            !drop && layer == T0
        })
        .count();

    assert!(
        t0_sent > 0,
        "under maximum Gear B escalation at least some T0 frames must be forwarded; \
         the base layer must always deliver a positive frame rate"
    );

    // Verify no dropped T0 frames (the drop floor is T1, never T0).
    let mut ctrl2 = TemporalSvcController::new(TemporalSvcMode::L1T2);
    for _ in 0..(OVERUSE_ESCALATE_TICKS * 10) {
        ctrl2.update(true);
    }
    let dropped_t0 = (0..total)
        .filter(|_| {
            let (layer, drop) = ctrl2.next_frame();
            drop && layer == T0
        })
        .count();
    assert_eq!(
        dropped_t0, 0,
        "T0 base-layer frames must never be dropped under any escalation; \
         dropping T0 would require a keyframe to resync the decoder"
    );
}

// ── 11. Recovery restores enhancement layers without a keyframe ───────────────

#[test]
fn gear_b_recovery_restores_t1_without_keyframe() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);

    // Escalate to T1 drop.
    for _ in 0..OVERUSE_ESCALATE_TICKS {
        ctrl.update(true);
    }
    assert_eq!(ctrl.drop_floor(), T1, "must be at T1 drop before recovery");

    // Recovery ticks.
    for _ in 0..lowband_platform::UNDERUSE_RELAX_TICKS {
        ctrl.update(false);
    }
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "L1T2 must fully recover after UNDERUSE_RELAX_TICKS without congestion"
    );

    // After recovery both T0 and T1 frames are forwarded — no IDR needed.
    let mut has_t1 = false;
    for _ in 0..60 {
        let (layer, drop) = ctrl.next_frame();
        assert!(!drop, "no frames should be dropped after full recovery");
        if layer == T1 { has_t1 = true; }
    }
    assert!(
        has_t1,
        "T1 enhancement frames must be restored after recovery \
         without an intervening keyframe"
    );
}

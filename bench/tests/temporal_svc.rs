//! Temporal SVC T-layer drop for congestion response — Feature §9.2.
//!
//! # Purpose
//!
//! Verifies that the Gear B (SVT-AV1) temporal-SVC controller assigns temporal
//! layer IDs to frames according to the L1T2 / L1T3 patterns and, when the
//! congestion signal triggers, sheds the highest T-layer without emitting an
//! IDR keyframe — a decoder-transparent rate reduction.
//!
//! # Why T-layer drops instead of keyframes
//!
//! A keyframe under congestion is counter-productive: it causes a bitrate spike
//! (5–10× average frame size) precisely when the link is already saturated,
//! worsening queue buildup.  Temporal-layer drops achieve the opposite: they
//! reduce the send rate (50 % for L1T2 T1-drop; 50 % / 75 % for L1T3) by
//! simply withholding enhancement-layer frames — frames the decoder never
//! depended on — while the base layer (T0) continues to arrive at its own
//! sustainable cadence.
//!
//! # Assertions
//!
//! 1. L1T2 pattern: frames alternate T0 / T1.
//! 2. L1T3 pattern: frames follow the T0, T2, T1, T2 period-4 sequence.
//! 3. T0 base-layer frames are **never** dropped regardless of escalation depth.
//! 4. L1T2: overuse triggers T1 drop after exactly `OVERUSE_ESCALATE_TICKS`.
//! 5. L1T3: first overuse burst drops T2; second drops T1+T2.
//! 6. Drop reduces the effective forwarded framerate by the expected fraction.
//! 7. Recovery: `UNDERUSE_RELAX_TICKS` of clear conditions restores a T-layer.
//! 8. L1T3 recovery is stepwise (T1+T2 → T2 → none) matching escalation depth.
//! 9. All frames forwarded under a drop policy are decodable independently
//!    (T2-drop: only T0+T1 sent; T1+T2-drop: only T0 sent).

use lowband_platform::gear_policy::GEAR_B_TARGET_FPS;
use lowband_platform::temporal_svc::{
    TemporalLayerAssigner, TemporalSvcController, TemporalSvcMode,
    OVERUSE_ESCALATE_TICKS, UNDERUSE_RELAX_TICKS, T0, T1, T2,
};
use lowband_platform::TemporalLayerId;

/// Simulated camera framerate used in rate-reduction assertions.
const FPS: u32 = GEAR_B_TARGET_FPS; // 30

/// Advance a controller through `overuse_ticks` overuse ticks.
fn apply_overuse(ctrl: &mut TemporalSvcController, overuse_ticks: u32) {
    for _ in 0..overuse_ticks {
        ctrl.update(true);
    }
}

/// Advance a controller through `ticks` non-overuse ticks.
fn apply_underuse(ctrl: &mut TemporalSvcController, ticks: u32) {
    for _ in 0..ticks {
        ctrl.update(false);
    }
}

// ── 1. L1T2 frame pattern ─────────────────────────────────────────────────────

#[test]
fn l1t2_frames_alternate_t0_t1() {
    let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
    let period = 2u64;
    let cycles = 10u64;
    for i in 0..(period * cycles) {
        let layer = a.next_layer();
        let expected = if i % 2 == 0 { T0 } else { T1 };
        assert_eq!(
            layer, expected,
            "L1T2 frame {i}: expected {expected:?}, got {layer:?}"
        );
    }
}

#[test]
fn l1t2_first_frame_is_always_t0() {
    let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
    assert_eq!(
        a.next_layer(),
        T0,
        "L1T2 stream must start with T0 so the decoder has a reference frame"
    );
}

// ── 2. L1T3 frame pattern ─────────────────────────────────────────────────────

#[test]
fn l1t3_frames_follow_t0_t2_t1_t2_period_four() {
    let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
    let expected_period = [T0, T2, T1, T2];
    let cycles = 8;
    for i in 0..(4 * cycles) {
        let layer = a.next_layer();
        let expected = expected_period[i % 4];
        assert_eq!(
            layer, expected,
            "L1T3 frame {i}: expected {expected:?}, got {layer:?}"
        );
    }
}

#[test]
fn l1t3_first_frame_is_always_t0() {
    let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
    assert_eq!(a.next_layer(), T0);
}

#[test]
fn l1t3_t0_appears_at_exact_quarter_rate() {
    let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
    let frames = 120u32;
    let t0_count = (0..frames).filter(|_| a.next_layer() == T0).count();
    assert_eq!(
        t0_count,
        (frames / 4) as usize,
        "L1T3 T0 must appear every 4th frame (quarter rate)"
    );
}

// ── 3. T0 base layer is never dropped ────────────────────────────────────────

#[test]
fn l1t2_t0_never_dropped_at_any_escalation_depth() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    // Saturate overuse far beyond any escalation level.
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 20);
    for _ in 0..(FPS * 5) {
        let (layer, drop) = ctrl.next_frame();
        if layer == T0 {
            assert!(!drop, "T0 must never be dropped under L1T2 congestion");
        }
    }
}

#[test]
fn l1t3_t0_never_dropped_at_any_escalation_depth() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 20);
    for _ in 0..(FPS * 5) {
        let (layer, drop) = ctrl.next_frame();
        if layer == T0 {
            assert!(!drop, "T0 must never be dropped under L1T3 congestion");
        }
    }
}

// ── 4. L1T2 overuse triggers T1 drop after exactly OVERUSE_ESCALATE_TICKS ───

#[test]
fn l1t2_no_drop_before_escalate_threshold() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS - 1);
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "L1T2 must not escalate before {OVERUSE_ESCALATE_TICKS} overuse ticks"
    );
}

#[test]
fn l1t2_drops_t1_exactly_at_escalate_threshold() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "L1T2 must drop T1 after {OVERUSE_ESCALATE_TICKS} overuse ticks"
    );
}

#[test]
fn l1t2_stays_at_t1_drop_under_sustained_overuse() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    // L1T2 only has one enhancement layer; further overuse cannot escalate higher.
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 10);
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "L1T2 drop floor must remain at T1 under sustained overuse (no higher layer to drop)"
    );
}

// ── 5. L1T3 two-step escalation ──────────────────────────────────────────────

#[test]
fn l1t3_first_escalation_drops_t2() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        T2,
        "L1T3 first escalation must drop T2 only (50 % rate reduction)"
    );
}

#[test]
fn l1t3_second_escalation_drops_t1_and_above() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS); // → T2 drop
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS); // → T1+T2 drop
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "L1T3 second escalation must drop T1 and above (75 % rate reduction)"
    );
}

#[test]
fn l1t3_stays_at_t1_drop_under_further_overuse() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 10);
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "L1T3 must not drop below T1 (base layer must always be forwarded)"
    );
}

// ── 6. Drop reduces effective framerate by the expected fraction ─────────────

#[test]
fn l1t2_t1_drop_halves_forwarded_frame_count() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);

    let total = FPS as usize * 4; // 4 seconds of frames
    let sent = (0..total).filter(|_| !ctrl.next_frame().1).count();

    let expected = total / 2;
    assert_eq!(
        sent, expected,
        "L1T2 T1-drop must forward exactly half the encoded frames ({expected}/{total})"
    );
}

#[test]
fn l1t3_t2_drop_halves_forwarded_frame_count() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS); // T2 drop only

    let total = FPS as usize * 4;
    let sent = (0..total).filter(|_| !ctrl.next_frame().1).count();

    let expected = total / 2; // T0 (25 %) + T1 (25 %) = 50 %
    assert_eq!(
        sent, expected,
        "L1T3 T2-drop must forward 50 % of frames ({expected}/{total})"
    );
}

#[test]
fn l1t3_t1t2_drop_quarters_forwarded_frame_count() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 2); // T1+T2 drop

    let total = (FPS as usize) * 4;
    let sent = (0..total).filter(|_| !ctrl.next_frame().1).count();

    let expected = total / 4; // only T0 (25 % of frames)
    assert_eq!(
        sent, expected,
        "L1T3 T1+T2-drop must forward 25 % of frames ({expected}/{total})"
    );
}

#[test]
fn active_frame_fraction_matches_actual_forwarded_ratio() {
    for mode in [TemporalSvcMode::L1T2, TemporalSvcMode::L1T3] {
        for escalations in 0u32..=2 {
            let mut ctrl = TemporalSvcController::new(mode);
            for _ in 0..escalations {
                apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
            }
            let stated_fraction = ctrl.active_frame_fraction();

            let total = FPS as usize * 8;
            let sent = (0..total).filter(|_| !ctrl.next_frame().1).count();
            let actual_fraction = sent as f64 / total as f64;

            assert!(
                (actual_fraction - stated_fraction).abs() < 0.01,
                "mode={mode:?} escalations={escalations}: \
                 active_frame_fraction()={stated_fraction:.2} but actual forwarded ratio={actual_fraction:.2}"
            );
        }
    }
}

// ── 7. Recovery after UNDERUSE_RELAX_TICKS ───────────────────────────────────

#[test]
fn l1t2_recovers_fully_after_relax_ticks() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    assert_eq!(ctrl.drop_floor(), T1);

    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "L1T2 must fully recover after {UNDERUSE_RELAX_TICKS} non-overuse ticks"
    );
}

#[test]
fn l1t2_no_recovery_before_relax_threshold() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS - 1);
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "L1T2 must not recover before {UNDERUSE_RELAX_TICKS} non-overuse ticks"
    );
}

// ── 8. L1T3 stepwise recovery ────────────────────────────────────────────────

#[test]
fn l1t3_stepwise_recovery_from_max_escalation() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    // Reach T1+T2 drop.
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 2);
    assert_eq!(ctrl.drop_floor(), T1, "must be at T1 drop before recovery");

    // Step 1: partial recovery to T2 drop.
    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        T2,
        "L1T3 first recovery step must restore T1 (drop T2 only)"
    );

    // Step 2: full recovery.
    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "L1T3 second recovery step must restore T2 (no drops)"
    );
}

#[test]
fn l1t3_single_escalation_recovers_in_one_step() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    // Only one escalation: T2 drop.
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    assert_eq!(ctrl.drop_floor(), T2);

    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS);
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "L1T3 single-step escalation must recover fully in one relax period"
    );
}

// ── 9. Forwarded frames are decodable subsets ─────────────────────────────────

#[test]
fn l1t2_t1_drop_only_forwards_t0() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    for _ in 0..(FPS * 3) {
        let (layer, drop) = ctrl.next_frame();
        if !drop {
            assert_eq!(
                layer, T0,
                "under L1T2 T1-drop every forwarded frame must be T0 \
                 (T1 frames have no decodable successors until the next T0)"
            );
        }
    }
}

#[test]
fn l1t3_t2_drop_only_forwards_t0_and_t1() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS); // T2 drop
    for _ in 0..(FPS * 3) {
        let (layer, drop) = ctrl.next_frame();
        if !drop {
            assert!(
                layer <= T1,
                "under L1T3 T2-drop only T0 and T1 frames may be forwarded; got {layer:?}"
            );
        }
    }
}

#[test]
fn l1t3_t1t2_drop_only_forwards_t0() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS * 2); // T1+T2 drop
    for _ in 0..(FPS * 3) {
        let (layer, drop) = ctrl.next_frame();
        if !drop {
            assert_eq!(
                layer, T0,
                "under L1T3 T1+T2-drop only T0 may be forwarded"
            );
        }
    }
}

// ── Escalation/relax interplay (counter reset) ────────────────────────────────

#[test]
fn overuse_counter_resets_when_non_overuse_tick_interrupts() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    // Feed ESCALATE-1 overuse ticks, then one clean tick (resets counter).
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS - 1);
    ctrl.update(false);
    // Another ESCALATE-1 ticks must not yet escalate.
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS - 1);
    assert_eq!(
        ctrl.drop_floor(),
        TemporalLayerId(u8::MAX),
        "overuse counter must reset when a non-overuse tick interrupts the sequence"
    );
}

#[test]
fn underuse_counter_resets_when_overuse_tick_interrupts() {
    let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
    apply_overuse(&mut ctrl, OVERUSE_ESCALATE_TICKS);
    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS - 1);
    ctrl.update(true); // reset underuse counter
    apply_underuse(&mut ctrl, UNDERUSE_RELAX_TICKS - 1);
    assert_eq!(
        ctrl.drop_floor(),
        T1,
        "underuse counter must reset when an overuse tick interrupts recovery"
    );
}

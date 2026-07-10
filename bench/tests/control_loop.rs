//! Feature 67 — System runs a single 10 Hz loop with control_loop inputs of
//! bandwidth, RTT, loss, and thermal state.
//!
//! # What this test verifies
//!
//! 1. **Loop inputs are consumed** — `bwe_bps`, `rtt_ms`, `loss_ppm`, and
//!    `thermal` all flow through the loop and appear in the output.
//!
//! 2. **Tier emitter is driven by BWE** — the loop produces tier state
//!    transitions that match the underlying `GovernorTierEmitter` contract:
//!    upgrades take 50 ticks, downgrades are immediate.
//!
//! 3. **RTT and loss are forwarded to the summary** — the `GovernorSummary`
//!    in the output mirrors the tick's `rtt_ms` and `loss_ppm` exactly so
//!    the peer-convergence path (Feature 73) receives fresh observables every
//!    100 ms.
//!
//! 4. **Thermal constraint is applied** — a `Critical` thermal input zeroes the
//!    camera budget while the audio floor remains intact.
//!
//! 5. **Audio floor is unconditional** — at every BWE and every thermal level
//!    `budgets.audio_bps` is always ≥ 6 000 bps.
//!
//! 6. **100 ms interval contract** — the loop object is stateless between ticks
//!    except for the tier-emitter hold counter; the test verifies that calling
//!    `tick()` exactly 50 times at the Constrained-floor BWE produces exactly
//!    one upgrade (on the 50th call), proving the 10 Hz / 5-second hold
//!    relationship is preserved.
//!
//! 7. **Full collapse-and-recovery** — a 400 kbps → 48 kbps collapse drives
//!    tier to Survival within 3 ticks; recovery at 64 kbps restores to
//!    Constrained after 50 ticks.

use lowband_platform::{
    ControlLoop, ControlLoopInput,
    gear_policy::AUDIO_FLOOR_BPS,
    thermal::ThermalPressure,
    TierState,
    CONSTRAINED_FLOOR_BPS, COMFORTABLE_FLOOR_BPS, FULL_FLOOR_BPS,
};

// ── helpers ───────────────────────────────────────────────────────────────────

fn input(bwe_bps: u32, rtt_ms: u32, loss_ppm: u32, thermal: ThermalPressure) -> ControlLoopInput {
    ControlLoopInput { bwe_bps, rtt_ms, loss_ppm, thermal }
}

fn nominal(bwe_bps: u32) -> ControlLoopInput {
    input(bwe_bps, 40, 0, ThermalPressure::Nominal)
}

// ── 1. Loop inputs are consumed ───────────────────────────────────────────────

#[test]
fn bwe_drives_tier_classification() {
    let mut gov = ControlLoop::new();

    // Below Constrained floor → Survival candidate (no upgrade yet at tick 1).
    let out = gov.tick(nominal(CONSTRAINED_FLOOR_BPS - 1));
    assert_eq!(out.tier, TierState::Survival,
        "BWE below Constrained floor must not upgrade from Survival on tick 1");

    // Reset and try Constrained floor — still Survival (need 50 ticks).
    let mut gov2 = ControlLoop::new();
    let out2 = gov2.tick(nominal(CONSTRAINED_FLOOR_BPS));
    assert_eq!(out2.tier, TierState::Survival,
        "single tick at Constrained floor must not yet produce upgrade");
}

#[test]
fn rtt_present_in_output_summary() {
    let mut gov = ControlLoop::new();
    let out = gov.tick(input(100_000, 123, 0, ThermalPressure::Nominal));
    assert_eq!(out.summary.rtt_ms, 123,
        "summary.rtt_ms must equal the input rtt_ms passed to tick()");
}

#[test]
fn loss_ppm_present_in_output_summary() {
    let mut gov = ControlLoop::new();
    let out = gov.tick(input(100_000, 40, 45_000, ThermalPressure::Nominal));
    assert_eq!(out.summary.loss_ppm, 45_000,
        "summary.loss_ppm must equal the input loss_ppm passed to tick()");
}

#[test]
fn thermal_critical_zeroes_camera_after_full_tier_reached() {
    let mut gov = ControlLoop::new();
    // Reach Full tier first so camera would ordinarily be funded.
    for _ in 0..160 {
        gov.tick(nominal(FULL_FLOOR_BPS + 50_000));
    }
    let out = gov.tick(input(FULL_FLOOR_BPS + 50_000, 20, 0, ThermalPressure::Critical));
    assert_eq!(out.budgets.camera_bps, 0,
        "Critical thermal must zero camera regardless of BWE");
}

// ── 2. Tier emitter is driven by BWE ─────────────────────────────────────────

#[test]
fn upgrade_from_survival_to_constrained_takes_exactly_50_ticks() {
    let mut gov = ControlLoop::new();

    for tick in 1u32..=50 {
        let out = gov.tick(nominal(CONSTRAINED_FLOOR_BPS));
        if tick < 50 {
            assert_eq!(out.tier, TierState::Survival,
                "tick {tick}: tier must remain Survival before 50-tick hold");
        } else {
            assert_eq!(out.tier, TierState::Constrained,
                "tick {tick}: tier must upgrade to Constrained on the 50th tick");
        }
    }
}

#[test]
fn downgrade_fires_on_the_very_next_tick() {
    let mut gov = ControlLoop::new();

    // Reach Constrained.
    for _ in 0..50 {
        gov.tick(nominal(CONSTRAINED_FLOOR_BPS));
    }
    assert_eq!(gov.current_tier(), TierState::Constrained);

    // One tick at BWE below the 0.8× trigger must step down immediately.
    let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5; // 51 200 bps
    let out = gov.tick(nominal(trigger - 1));
    assert_eq!(out.tier, TierState::Survival,
        "downgrade must be immediate — one tick is ≤ 100 ms");
}

// ── 3. RTT and loss forwarded to summary verbatim ────────────────────────────

#[test]
fn summary_mirrors_all_four_inputs() {
    let mut gov = ControlLoop::new();
    let inp = ControlLoopInput {
        bwe_bps:  80_000,
        rtt_ms:   200,
        loss_ppm: 30_000,
        thermal:  ThermalPressure::Nominal,
    };
    let out = gov.tick(inp);
    assert_eq!(out.summary.bwe_bps,  80_000);
    assert_eq!(out.summary.rtt_ms,   200);
    assert_eq!(out.summary.loss_ppm, 30_000);
    assert_eq!(out.summary.tier,     out.tier);
}

// ── 4. Thermal constraint applied ────────────────────────────────────────────

#[test]
fn fair_thermal_still_funds_camera_at_full_tier() {
    let mut gov = ControlLoop::new();
    for _ in 0..160 {
        gov.tick(nominal(FULL_FLOOR_BPS + 50_000));
    }
    let out = gov.tick(input(FULL_FLOOR_BPS + 50_000, 20, 0, ThermalPressure::Fair));
    assert!(out.budgets.camera_bps > 0,
        "camera must still be funded at Fair thermal (Gear B replaces Gear A, camera stays on)");
}

#[test]
fn serious_thermal_suspends_screen_refinement() {
    let mut gov = ControlLoop::new();
    for _ in 0..160 {
        gov.tick(nominal(FULL_FLOOR_BPS + 50_000));
    }
    let out = gov.tick(input(FULL_FLOOR_BPS + 50_000, 20, 0, ThermalPressure::Serious));
    assert_eq!(out.budgets.screen_refinement_bps, 0,
        "screen refinement must be suspended at Serious thermal pressure");
}

// ── 5. Audio floor is unconditional ──────────────────────────────────────────

#[test]
fn audio_floor_holds_at_zero_bwe_and_critical_thermal() {
    let mut gov = ControlLoop::new();
    let out = gov.tick(input(0, 500, 1_000_000, ThermalPressure::Critical));
    assert!(
        out.budgets.audio_bps >= AUDIO_FLOOR_BPS,
        "audio floor ({AUDIO_FLOOR_BPS} bps) must hold even at 0 bps BWE and Critical thermal; \
         got {} bps",
        out.budgets.audio_bps,
    );
}

#[test]
fn audio_floor_holds_across_all_thermal_levels() {
    let levels = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];
    for thermal in levels {
        let mut gov = ControlLoop::new();
        let out = gov.tick(input(48_000, 80, 5_000, thermal));
        assert!(
            out.budgets.audio_bps >= AUDIO_FLOOR_BPS,
            "audio floor must hold at {thermal:?} thermal; got {} bps",
            out.budgets.audio_bps,
        );
    }
}

// ── 6. 100 ms interval contract — hold counter ────────────────────────────────

#[test]
fn hold_counter_accumulates_across_ticks() {
    let mut gov = ControlLoop::new();
    for i in 1u32..=30 {
        gov.tick(nominal(CONSTRAINED_FLOOR_BPS));
        assert_eq!(gov.upgrade_hold_ticks(), i,
            "hold counter must equal the number of qualifying ticks so far");
    }
}

#[test]
fn hold_counter_resets_on_gap_tick() {
    let mut gov = ControlLoop::new();
    for _ in 0..30 {
        gov.tick(nominal(CONSTRAINED_FLOOR_BPS));
    }
    // One tick with BWE below the Constrained floor resets the counter.
    gov.tick(nominal(0));
    assert_eq!(gov.upgrade_hold_ticks(), 0,
        "hold counter must reset when BWE falls below the candidate tier floor");
}

// ── 7. Full collapse-and-recovery ────────────────────────────────────────────

#[test]
fn full_collapse_and_recovery_tier_sequence() {
    let mut gov = ControlLoop::new();

    // ── Phase 1: Reach Full tier ──────────────────────────────────────────
    let mut tier = TierState::Survival;
    for _ in 0..200 {
        tier = gov.tick(nominal(FULL_FLOOR_BPS + 50_000)).tier;
    }
    assert_eq!(tier, TierState::Full, "must reach Full after 200 ticks at 400 kbps");

    // ── Phase 2: Collapse to 48 kbps — 3 immediate downgrades ────────────
    let collapse_bwe = 48_000;
    let expected_descent = [
        TierState::Comfortable,
        TierState::Constrained,
        TierState::Survival,
    ];
    for (i, &expected) in expected_descent.iter().enumerate() {
        tier = gov.tick(nominal(collapse_bwe)).tier;
        assert_eq!(tier, expected,
            "collapse tick {}: expected {expected:?}, got {tier:?}", i + 1);
    }

    // ── Phase 3: Survival is the floor ───────────────────────────────────
    for _ in 0..10 {
        tier = gov.tick(nominal(collapse_bwe)).tier;
        assert_eq!(tier, TierState::Survival, "Survival must not descend further");
    }

    // ── Phase 4: Recovery to 64 kbps — 50-tick hold ──────────────────────
    for tick in 1u32..=50 {
        tier = gov.tick(nominal(CONSTRAINED_FLOOR_BPS)).tier;
        if tick < 50 {
            assert_eq!(tier, TierState::Survival,
                "tick {tick}: upgrade still blocked during 49-tick hold");
        } else {
            assert_eq!(tier, TierState::Constrained,
                "tick {tick}: upgrade must fire after 50-tick hold at 64 kbps");
        }
    }
}

// ── 8. current_tier() accessor ───────────────────────────────────────────────

#[test]
fn current_tier_matches_last_tick_output() {
    let mut gov = ControlLoop::new();

    for _ in 0..50 {
        gov.tick(nominal(COMFORTABLE_FLOOR_BPS));
    }
    let out = gov.tick(nominal(COMFORTABLE_FLOOR_BPS));

    assert_eq!(gov.current_tier(), out.tier,
        "current_tier() must equal the tier returned by the last tick()");
}

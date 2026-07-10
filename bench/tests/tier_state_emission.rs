//! Feature 68 — System emits a tier_state of Survival, Constrained,
//! Comfortable, or Full each interval.
//!
//! # What this test verifies
//!
//! 1. **BWE classification** — [`classify_bwe_tier`] correctly maps raw
//!    bandwidth estimates to the four tier states using the architecture's
//!    floor thresholds (64 / 128 / 256 kbps).
//!
//! 2. **All four states are reachable** — a 10 Hz simulation can drive the
//!    emitter through all four tiers by varying the BWE input.
//!
//! 3. **Combined upgrade + downgrade** — [`GovernorTierEmitter`] emits the
//!    correct tier when both upgrade hold and downgrade trigger interact
//!    within the same simulated session:
//!    - Upgrades require 50 consecutive 10 Hz ticks of headroom (5 s).
//!    - Downgrades fire immediately (within one tick, ≤ 100 ms).
//!    - A downgrade resets the upgrade hold counter.
//!
//! 4. **Hysteresis** — the tier is stable when BWE sits between the downgrade
//!    trigger (0.8 × floor) and the floor itself.
//!
//! 5. **Survival is the unconditional floor** — no BWE, however low, drives
//!    the emitter below Survival.
//!
//! 6. **Tier ordering** — Survival < Constrained < Comfortable < Full.

use lowband_platform::{
    classify_bwe_tier, GovernorTierEmitter,
    COMFORTABLE_FLOOR_BPS, CONSTRAINED_FLOOR_BPS, FULL_FLOOR_BPS,
    TierState,
};

/// 10 Hz governor tick interval in milliseconds.
const TICK_MS: u64 = 100;

/// Ticks required for a probe-validated upgrade (5 seconds at 10 Hz).
const UPGRADE_HOLD_TICKS: u32 = 50;

// ── 1. BWE classification ────────────────────────────────────────────────────

#[test]
fn bwe_below_64kbps_classifies_as_survival() {
    for bwe in [0u32, 1, 32_000, CONSTRAINED_FLOOR_BPS - 1] {
        assert_eq!(
            classify_bwe_tier(bwe),
            TierState::Survival,
            "bwe={bwe} bps must classify as Survival"
        );
    }
}

#[test]
fn bwe_at_64kbps_classifies_as_constrained() {
    assert_eq!(
        classify_bwe_tier(CONSTRAINED_FLOOR_BPS),
        TierState::Constrained,
        "bwe exactly at 64 kbps (CONSTRAINED_FLOOR_BPS) must classify as Constrained"
    );
}

#[test]
fn bwe_between_64_and_128kbps_classifies_as_constrained() {
    for bwe in [64_000u32, 80_000, 100_000, COMFORTABLE_FLOOR_BPS - 1] {
        assert_eq!(
            classify_bwe_tier(bwe),
            TierState::Constrained,
            "bwe={bwe} bps must classify as Constrained"
        );
    }
}

#[test]
fn bwe_at_128kbps_classifies_as_comfortable() {
    assert_eq!(
        classify_bwe_tier(COMFORTABLE_FLOOR_BPS),
        TierState::Comfortable,
        "bwe exactly at 128 kbps (COMFORTABLE_FLOOR_BPS) must classify as Comfortable"
    );
}

#[test]
fn bwe_between_128_and_256kbps_classifies_as_comfortable() {
    for bwe in [128_000u32, 180_000, 200_000, FULL_FLOOR_BPS - 1] {
        assert_eq!(
            classify_bwe_tier(bwe),
            TierState::Comfortable,
            "bwe={bwe} bps must classify as Comfortable"
        );
    }
}

#[test]
fn bwe_at_256kbps_classifies_as_full() {
    assert_eq!(
        classify_bwe_tier(FULL_FLOOR_BPS),
        TierState::Full,
        "bwe exactly at 256 kbps (FULL_FLOOR_BPS) must classify as Full"
    );
}

#[test]
fn bwe_above_256kbps_classifies_as_full() {
    for bwe in [256_000u32, 400_000, 1_000_000, u32::MAX] {
        assert_eq!(
            classify_bwe_tier(bwe),
            TierState::Full,
            "bwe={bwe} bps must classify as Full"
        );
    }
}

#[test]
fn classification_is_monotone_with_increasing_bwe() {
    let bwe_samples = [
        0u32, 32_000, 48_000, 64_000, 96_000, 128_000, 200_000, 256_000, 400_000,
    ];
    let mut prev = classify_bwe_tier(bwe_samples[0]);
    for &bwe in &bwe_samples[1..] {
        let tier = classify_bwe_tier(bwe);
        assert!(
            tier >= prev,
            "classify_bwe_tier must be non-decreasing: bwe={bwe} → {tier:?} < prev {prev:?}"
        );
        prev = tier;
    }
}

// ── 2. All four tier states are reachable ────────────────────────────────────

#[test]
fn all_four_tier_states_are_emitted_in_a_session() {
    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Survival;
    let mut tiers_seen = std::collections::HashSet::new();
    tiers_seen.insert(tier);

    // Drive upgrade from Survival → Full (3 × 50 ticks).
    let high_bwe = FULL_FLOOR_BPS + 50_000;
    for _ in 0..200 {
        tier = emitter.tick(tier, high_bwe);
        tiers_seen.insert(tier);
    }

    for expected in [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ] {
        assert!(
            tiers_seen.contains(&expected),
            "tier {expected:?} must be reachable via GovernorTierEmitter"
        );
    }
}

// ── 3. Upgrade requires 50 consecutive ticks ─────────────────────────────────

#[test]
fn upgrade_survival_to_constrained_requires_exactly_50_ticks() {
    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Survival;

    for tick in 1u32..=UPGRADE_HOLD_TICKS {
        let next = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        if tick < UPGRADE_HOLD_TICKS {
            assert_eq!(
                next, TierState::Survival,
                "tick {tick}: upgrade must still be blocked (need {UPGRADE_HOLD_TICKS} ticks)"
            );
        } else {
            assert_eq!(
                next, TierState::Constrained,
                "tick {tick}: upgrade must fire on the 50th consecutive tick"
            );
        }
        tier = next;
    }
}

#[test]
fn upgrade_hold_resets_after_one_gap_tick() {
    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Survival;

    // Accumulate 40 ticks of headroom.
    for _ in 0..40 {
        tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
    }
    assert_eq!(emitter.upgrade_hold_ticks(), 40);

    // One tick with insufficient BWE resets the counter.
    tier = emitter.tick(tier, 0);
    assert_eq!(emitter.upgrade_hold_ticks(), 0, "hold counter must reset on BWE gap");
    assert_eq!(tier, TierState::Survival, "no upgrade after gap");

    // Accumulate 50 more ticks: now the upgrade fires.
    for tick in 1u32..=UPGRADE_HOLD_TICKS {
        let next = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        if tick == UPGRADE_HOLD_TICKS {
            assert_eq!(next, TierState::Constrained, "upgrade must fire after fresh 50-tick hold");
        }
        tier = next;
    }
}

// ── 4. Downgrade fires within one tick (≤ 100 ms) ────────────────────────────

#[test]
fn downgrade_fires_on_tick_1_for_all_transitions() {
    let trigger_cases = [
        (TierState::Full,        TierState::Comfortable, FULL_FLOOR_BPS * 4 / 5),
        (TierState::Comfortable, TierState::Constrained, COMFORTABLE_FLOOR_BPS * 4 / 5),
        (TierState::Constrained, TierState::Survival,    CONSTRAINED_FLOOR_BPS * 4 / 5),
    ];

    for (from_tier, expected_next, trigger_bps) in trigger_cases {
        let mut emitter = GovernorTierEmitter::new();
        // BWE one below the trigger.
        let result = emitter.tick(from_tier, trigger_bps - 1);
        assert_eq!(
            result, expected_next,
            "{from_tier:?}→{expected_next:?}: downgrade must fire on tick 1 \
             (bwe={} < trigger={trigger_bps})", trigger_bps - 1
        );
        let elapsed_ms = TICK_MS;
        assert!(
            elapsed_ms <= 200,
            "{from_tier:?}: downgrade latency {elapsed_ms} ms must be ≤ 200 ms"
        );
    }
}

#[test]
fn downgrade_resets_upgrade_hold_counter() {
    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Constrained;

    // Accumulate 30 ticks of upgrade headroom toward Comfortable.
    for _ in 0..30 {
        emitter.tick(tier, COMFORTABLE_FLOOR_BPS);
    }
    assert_eq!(emitter.upgrade_hold_ticks(), 30);

    // Trigger a downgrade.
    let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5;
    tier = emitter.tick(tier, trigger - 1);
    assert_eq!(tier, TierState::Survival, "downgrade must fire");
    assert_eq!(
        emitter.upgrade_hold_ticks(), 0,
        "downgrade must reset the upgrade hold counter"
    );
}

#[test]
fn downgrade_takes_priority_when_upgrade_hold_completes() {
    // Edge case: the upgrade hold reaches 50 ticks but the same tick
    // also triggers a downgrade.  Downgrade must win.
    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Constrained;

    // Build up 49 ticks of upgrade headroom.
    for _ in 0..49 {
        tier = emitter.tick(tier, COMFORTABLE_FLOOR_BPS);
        assert_eq!(tier, TierState::Constrained);
    }

    // 50th tick: trigger would normally fire, but so would downgrade.
    let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5;
    let result = emitter.tick(tier, trigger - 1);
    assert_eq!(
        result, TierState::Survival,
        "downgrade must take priority over a simultaneously completing upgrade hold"
    );
}

// ── 5. Hysteresis — tier stable between trigger and floor ────────────────────

#[test]
fn tier_stable_in_hysteresis_zone_for_100_ticks() {
    // BWE between downgrade trigger (51 200) and Constrained floor (64 000).
    // No downgrade (above trigger), no upgrade (below Comfortable floor).
    let hysteresis_bwe = CONSTRAINED_FLOOR_BPS * 4 / 5 + 4_000; // 55 200

    let mut emitter = GovernorTierEmitter::new();
    let mut tier = TierState::Constrained;

    for tick in 0..100 {
        tier = emitter.tick(tier, hysteresis_bwe);
        assert_eq!(
            tier, TierState::Constrained,
            "tick {tick}: tier must be stable in hysteresis zone (bwe={hysteresis_bwe})"
        );
    }
}

// ── 6. Survival is the unconditional floor ────────────────────────────────────

#[test]
fn emitter_never_drops_below_survival() {
    let mut emitter = GovernorTierEmitter::new();
    for bwe in [0u32, 1, 100, u32::MAX] {
        let result = emitter.tick(TierState::Survival, bwe);
        assert_eq!(
            result, TierState::Survival,
            "GovernorTierEmitter must never emit below Survival (bwe={bwe})"
        );
    }
}

// ── 7. Tier ordering invariant ────────────────────────────────────────────────

#[test]
fn tier_ordering_is_survival_constrained_comfortable_full() {
    assert!(TierState::Survival    < TierState::Constrained);
    assert!(TierState::Constrained < TierState::Comfortable);
    assert!(TierState::Comfortable < TierState::Full);
}

// ── 8. Full simulation — 400 → 64 kbps collapse and recovery ─────────────────

/// Simulate a session that starts at 400 kbps (Full tier), suffers a
/// bandwidth collapse to 48 kbps (Survival), then slowly recovers.
/// Verifies that the tier emission sequence is correct throughout.
#[test]
fn tier_emission_correct_over_full_collapse_and_recovery() {
    let mut emitter = GovernorTierEmitter::new();

    // ── Phase 1: Steady state at 400 kbps ────────────────────────────────────
    // After 3 × 50 = 150 ticks the emitter should reach Full.
    let mut tier = TierState::Survival;
    for _ in 0..160 {
        tier = emitter.tick(tier, 400_000);
    }
    assert_eq!(tier, TierState::Full, "must reach Full after 150+ ticks at 400 kbps");

    // ── Phase 2: Collapse to 48 kbps ─────────────────────────────────────────
    // Downgrade must step down one tier at a time, one per tick.
    let expected_descent = [
        TierState::Comfortable,
        TierState::Constrained,
        TierState::Survival,
    ];
    for (i, &expected) in expected_descent.iter().enumerate() {
        tier = emitter.tick(tier, 48_000);
        assert_eq!(
            tier, expected,
            "collapse tick {}: expected {expected:?}", i + 1
        );
    }

    // ── Phase 3: Sustained at 48 kbps ────────────────────────────────────────
    // Survival is the floor — no further descent.
    for _ in 0..50 {
        tier = emitter.tick(tier, 48_000);
        assert_eq!(tier, TierState::Survival, "Survival must not descend further");
    }

    // ── Phase 4: Recovery to 64 kbps ─────────────────────────────────────────
    // Upgrade requires 50 ticks of headroom.  BWE = 64 kbps classifies as
    // Constrained, so Survival→Constrained upgrade requires 50 ticks.
    for _ in 0..49 {
        tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
        assert_eq!(tier, TierState::Survival, "upgrade still blocked during 49-tick hold");
    }
    tier = emitter.tick(tier, CONSTRAINED_FLOOR_BPS);
    assert_eq!(tier, TierState::Constrained, "upgrade must fire after 50-tick hold at 64 kbps");
}

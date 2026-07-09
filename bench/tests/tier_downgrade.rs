//! Feature 70 — system downgrades a tier within 200 ms when BWE falls below
//! 0.8 × the tier floor.
//!
//! # Scenario
//!
//! The session is running at some tier T.  At t = 0 the BWE collapses to just
//! below `0.8 × tier_floor_bps(T)`.  The governor runs at 10 Hz (one tick every
//! 100 ms).  On the very next governor tick after the BWE drops,
//! [`TierDowngradeGuard::observe`] must return the tier one step below T.
//!
//! In the worst case the collapse occurs immediately after a governor tick, so
//! the next tick fires 100 ms later — well inside the 200 ms SLA.  The tests
//! below verify this by driving a simulated 10 Hz governor loop and asserting
//! that the downgrade is emitted within two ticks (200 ms).
//!
//! # What is tested
//!
//! 1. **Immediate response** — downgrade emitted on the very first tick where
//!    BWE < trigger, not after a hold period.
//! 2. **200 ms SLA** — the downgrade is always emitted within two governor
//!    ticks regardless of which tier transitions.
//! 3. **One step at a time** — even when BWE collapses to 0, each tick advances
//!    only one tier step.
//! 4. **Stability above trigger** — no false downgrades while BWE stays at or
//!    above the trigger.
//! 5. **Survival is the floor** — the guard never downgrades below Survival.

use lowband_platform::{
    downgrade_trigger_bps, tier_floor_bps,
    TierDowngradeGuard,
    COMFORTABLE_FLOOR_BPS, CONSTRAINED_FLOOR_BPS, FULL_FLOOR_BPS,
    TierState,
};

/// 10 Hz governor tick interval in milliseconds.
const TICK_MS: u64 = 100;

/// Maximum elapsed time (ms) from BWE collapse to confirmed downgrade.
const MAX_DOWNGRADE_MS: u64 = 200;

/// Maximum governor ticks before a downgrade must be emitted.
const MAX_DOWNGRADE_TICKS: u64 = MAX_DOWNGRADE_MS / TICK_MS; // 2 ticks

// ── 1. Immediate response — downgrade on tick 1 ───────────────────────────────

/// Drive a simulated governor loop at 10 Hz, returning the tick index (1-based)
/// on which `guard.observe()` first emits a downgrade, or `None` if no downgrade
/// was emitted within `max_ticks`.
fn ticks_to_downgrade(
    guard: &TierDowngradeGuard,
    tier: TierState,
    bwe_bps: u32,
    max_ticks: u64,
) -> Option<u64> {
    let mut current = tier;
    for tick in 1..=max_ticks {
        let next = guard.observe(current, bwe_bps);
        if next != current {
            return Some(tick);
        }
        current = next;
    }
    None
}

#[test]
fn downgrade_emitted_on_tick_1_for_constrained_to_survival() {
    let guard = TierDowngradeGuard::new();
    let trigger = downgrade_trigger_bps(TierState::Constrained).unwrap();
    let bwe = trigger - 1;

    let tick = ticks_to_downgrade(&guard, TierState::Constrained, bwe, MAX_DOWNGRADE_TICKS + 1);
    assert_eq!(
        tick,
        Some(1),
        "Constrained→Survival: downgrade must fire on tick 1 (bwe={bwe} < trigger={trigger})"
    );
}

#[test]
fn downgrade_emitted_on_tick_1_for_comfortable_to_constrained() {
    let guard = TierDowngradeGuard::new();
    let trigger = downgrade_trigger_bps(TierState::Comfortable).unwrap();
    let bwe = trigger - 1;

    let tick = ticks_to_downgrade(&guard, TierState::Comfortable, bwe, MAX_DOWNGRADE_TICKS + 1);
    assert_eq!(
        tick,
        Some(1),
        "Comfortable→Constrained: downgrade must fire on tick 1 (bwe={bwe} < trigger={trigger})"
    );
}

#[test]
fn downgrade_emitted_on_tick_1_for_full_to_comfortable() {
    let guard = TierDowngradeGuard::new();
    let trigger = downgrade_trigger_bps(TierState::Full).unwrap();
    let bwe = trigger - 1;

    let tick = ticks_to_downgrade(&guard, TierState::Full, bwe, MAX_DOWNGRADE_TICKS + 1);
    assert_eq!(
        tick,
        Some(1),
        "Full→Comfortable: downgrade must fire on tick 1 (bwe={bwe} < trigger={trigger})"
    );
}

// ── 2. 200 ms SLA across all tier transitions ─────────────────────────────────

#[test]
fn downgrade_within_200ms_sla_for_all_tiers() {
    let guard = TierDowngradeGuard::new();

    // All three downgradeable tiers × a BWE well below the trigger.
    let cases = [
        (TierState::Full,        TierState::Comfortable, 0u32),
        (TierState::Comfortable, TierState::Constrained, 0u32),
        (TierState::Constrained, TierState::Survival,    0u32),
    ];

    for (tier, expected_next, bwe) in cases {
        let tick = ticks_to_downgrade(&guard, tier, bwe, MAX_DOWNGRADE_TICKS);
        assert!(
            tick.is_some(),
            "{tier:?}: downgrade to {expected_next:?} must occur within \
             {MAX_DOWNGRADE_TICKS} ticks ({MAX_DOWNGRADE_MS} ms) when BWE = {bwe}"
        );

        let elapsed_ms = tick.unwrap() * TICK_MS;
        assert!(
            elapsed_ms <= MAX_DOWNGRADE_MS,
            "{tier:?}: downgrade must occur within {MAX_DOWNGRADE_MS} ms; \
             fired at {elapsed_ms} ms"
        );
    }
}

// ── 3. One step at a time ─────────────────────────────────────────────────────

#[test]
fn single_tick_advances_exactly_one_step() {
    let guard = TierDowngradeGuard::new();

    // Full at BWE 0 must yield Comfortable, not Constrained or Survival.
    let result = guard.observe(TierState::Full, 0);
    assert_eq!(
        result,
        TierState::Comfortable,
        "single tick from Full at BWE=0 must step to Comfortable, not skip tiers"
    );
}

#[test]
fn full_to_survival_requires_three_ticks() {
    // Three consecutive 10 Hz ticks at BWE = 0 descend Full → Comfortable →
    // Constrained → Survival, one step per tick.
    let guard = TierDowngradeGuard::new();
    let mut tier = TierState::Full;

    for (tick, expected) in [(1, TierState::Comfortable), (2, TierState::Constrained), (3, TierState::Survival)] {
        tier = guard.observe(tier, 0);
        assert_eq!(
            tier, expected,
            "tick {tick}: expected {expected:?}, got {tier:?}"
        );
    }

    // Fourth tick: already at Survival — no further descent.
    let after = guard.observe(tier, 0);
    assert_eq!(
        after,
        TierState::Survival,
        "Survival must not descend further"
    );
}

// ── 4. Stability above trigger — no false downgrades ─────────────────────────

#[test]
fn no_false_downgrade_at_exactly_the_trigger() {
    let guard = TierDowngradeGuard::new();

    // Trigger is a strict lower bound: BWE equal to trigger must NOT downgrade.
    for tier in [TierState::Full, TierState::Comfortable, TierState::Constrained] {
        let trigger = downgrade_trigger_bps(tier).unwrap();
        let result = guard.observe(tier, trigger);
        assert_eq!(
            result, tier,
            "{tier:?}: BWE exactly at trigger ({trigger} bps) must not trigger downgrade"
        );
    }
}

#[test]
fn no_false_downgrade_at_floor() {
    let guard = TierDowngradeGuard::new();

    let cases = [
        (TierState::Constrained, CONSTRAINED_FLOOR_BPS),
        (TierState::Comfortable, COMFORTABLE_FLOOR_BPS),
        (TierState::Full,        FULL_FLOOR_BPS),
    ];

    for (tier, floor) in cases {
        let result = guard.observe(tier, floor);
        assert_eq!(
            result, tier,
            "{tier:?}: BWE at floor ({floor} bps) must not trigger downgrade"
        );
    }
}

#[test]
fn no_false_downgrade_over_100_ticks_at_high_bwe() {
    let guard = TierDowngradeGuard::new();

    // 100 ticks at 400 kbps — no tier should ever downgrade.
    for tier in [TierState::Full, TierState::Comfortable, TierState::Constrained] {
        let mut current = tier;
        for _ in 0..100 {
            current = guard.observe(current, 400_000);
        }
        assert_eq!(
            current, tier,
            "{tier:?}: tier must not change over 100 ticks at 400 kbps"
        );
    }
}

// ── 5. Survival is the floor ──────────────────────────────────────────────────

#[test]
fn survival_never_downgrades_at_any_bwe() {
    let guard = TierDowngradeGuard::new();
    for bwe in [0u32, 1, 48_000, 64_000, 400_000, u32::MAX] {
        assert_eq!(
            guard.observe(TierState::Survival, bwe),
            TierState::Survival,
            "Survival must not downgrade at any BWE; bwe={bwe}"
        );
    }
}

// ── 6. Tier floor constants are consistent with architecture ──────────────────

#[test]
fn tier_floor_bps_returns_expected_constants() {
    assert_eq!(tier_floor_bps(TierState::Survival),    None,                       "Survival has no floor");
    assert_eq!(tier_floor_bps(TierState::Constrained), Some(CONSTRAINED_FLOOR_BPS), "Constrained floor");
    assert_eq!(tier_floor_bps(TierState::Comfortable), Some(COMFORTABLE_FLOOR_BPS), "Comfortable floor");
    assert_eq!(tier_floor_bps(TierState::Full),        Some(FULL_FLOOR_BPS),        "Full floor");
}

#[test]
fn constrained_floor_is_64kbps() {
    assert_eq!(
        CONSTRAINED_FLOOR_BPS, 64_000,
        "Constrained floor must be 64 kbps — the architecture minimum viable session rate"
    );
}

#[test]
fn downgrade_trigger_is_80_percent_of_floor() {
    for tier in [TierState::Constrained, TierState::Comfortable, TierState::Full] {
        let floor   = tier_floor_bps(tier).unwrap();
        let trigger = downgrade_trigger_bps(tier).unwrap();
        let expected = floor * 4 / 5;
        assert_eq!(
            trigger, expected,
            "{tier:?}: trigger ({trigger} bps) must be 80% of floor ({floor} bps)"
        );
    }
}

//! Feature 130 — display resolution steps from 640×360 to 848×480 by governor budget.
//!
//! # Scenario
//!
//! The governor allocates the screen-coarse channel through [`allocate`] and
//! simultaneously selects a display resolution from the two-rung
//! [`RESOLUTION_LADDER`].  The ladder floor is 640×360 (always available) and
//! the ceiling is 848×480, reached when `screen_coarse_bps` ≥
//! [`SCREEN_UPGRADE_BPS`] (10 kbps).
//!
//! # Assertions
//!
//! 1. `select_resolution(0)` returns 640×360 (floor is always safe).
//! 2. `select_resolution(SCREEN_UPGRADE_BPS)` returns 848×480 (exact threshold).
//! 3. `select_resolution(SCREEN_UPGRADE_BPS - 1)` returns 640×360 (just below).
//! 4. `allocate(64_000, …)` produces `display_resolution == 848×480` because
//!    the screen-coarse budget reaches the 20 kbps cap at 64 kbps.
//! 5. `allocate(30_000, …)` produces `display_resolution == 640×360` because
//!    audio + input consume most of the link leaving < 10 kbps for screen.
//! 6. Resolution is monotone: it can only increase as budget rises.
//! 7. All four thermal levels at 64 kbps select 848×480 (voice floor met,
//!    screen still funded at a budget above the threshold).

use lowband_platform::gear_policy::{
    allocate, select_resolution, DisplayResolution, GearConstraints, RESOLUTION_LADDER,
    SCREEN_UPGRADE_BPS,
};
use lowband_platform::thermal::ThermalPressure;

const LOW: DisplayResolution = DisplayResolution { width: 640, height: 360 };
const HIGH: DisplayResolution = DisplayResolution { width: 848, height: 480 };

// ── 1. Floor rung ─────────────────────────────────────────────────────────────

#[test]
fn floor_rung_selected_at_zero_budget() {
    assert_eq!(
        select_resolution(0),
        LOW,
        "640×360 must be selected when screen-coarse budget is 0 bps"
    );
}

// ── 2. Exact threshold ────────────────────────────────────────────────────────

#[test]
fn high_rung_selected_at_exact_upgrade_threshold() {
    assert_eq!(
        select_resolution(SCREEN_UPGRADE_BPS),
        HIGH,
        "848×480 must be selected when budget equals SCREEN_UPGRADE_BPS ({} bps)",
        SCREEN_UPGRADE_BPS
    );
}

// ── 3. One bps below threshold ────────────────────────────────────────────────

#[test]
fn floor_rung_selected_one_bps_below_threshold() {
    assert_eq!(
        select_resolution(SCREEN_UPGRADE_BPS - 1),
        LOW,
        "640×360 must be held when budget is one bps below threshold"
    );
}

// ── 4. 64 kbps full-link allocation ──────────────────────────────────────────

#[test]
fn allocate_64kbps_nominal_selects_848x480() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(64_000, &c);
    assert!(
        b.screen_coarse_bps >= SCREEN_UPGRADE_BPS,
        "precondition: screen_coarse_bps={} must be ≥ {} at 64 kbps",
        b.screen_coarse_bps,
        SCREEN_UPGRADE_BPS
    );
    assert_eq!(
        b.display_resolution,
        HIGH,
        "governor must select 848×480 at 64 kbps (screen_coarse={} bps)",
        b.screen_coarse_bps
    );
}

// ── 5. Very tight link falls back to 640×360 ─────────────────────────────────

#[test]
fn allocate_30kbps_falls_back_to_640x360() {
    // At 30 kbps: audio (~24 kbps) + input (~3 kbps) leaves ~3 kbps for
    // screen-coarse — below SCREEN_UPGRADE_BPS.
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(30_000, &c);
    assert!(
        b.screen_coarse_bps < SCREEN_UPGRADE_BPS,
        "precondition: screen_coarse_bps={} must be below threshold at 30 kbps",
        b.screen_coarse_bps
    );
    assert_eq!(
        b.display_resolution,
        LOW,
        "governor must select 640×360 when screen budget < SCREEN_UPGRADE_BPS"
    );
}

// ── 6. Monotone stepping ──────────────────────────────────────────────────────

#[test]
fn resolution_steps_monotonically_with_budget() {
    fn pixels(r: DisplayResolution) -> u32 {
        r.width * r.height
    }
    let budgets = [0u32, 1_000, SCREEN_UPGRADE_BPS - 1, SCREEN_UPGRADE_BPS, 20_000, 100_000];
    let pixel_counts: Vec<u32> = budgets.iter().map(|&b| pixels(select_resolution(b))).collect();
    for i in 1..pixel_counts.len() {
        assert!(
            pixel_counts[i] >= pixel_counts[i - 1],
            "resolution must not shrink as budget rises: {}→{} pixels (budget {}→{} bps)",
            pixel_counts[i - 1],
            pixel_counts[i],
            budgets[i - 1],
            budgets[i]
        );
    }
}

// ── 7. All thermal levels at 64 kbps → 848×480 ───────────────────────────────

#[test]
fn all_thermal_levels_at_64kbps_select_848x480() {
    let levels = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];
    for level in levels {
        let c = GearConstraints::from_thermal(level);
        let b = allocate(64_000, &c);
        assert_eq!(
            b.display_resolution,
            HIGH,
            "848×480 must be selected at 64 kbps regardless of thermal level \
             (level={level:?}, screen_coarse={} bps)",
            b.screen_coarse_bps
        );
    }
}

// ── 8. Ladder constant sanity ─────────────────────────────────────────────────

#[test]
fn resolution_ladder_floor_has_zero_minimum() {
    let (floor_res, floor_min) = RESOLUTION_LADDER[0];
    assert_eq!(floor_min, 0, "ladder floor must have a zero minimum budget");
    assert_eq!(floor_res, LOW);
}

#[test]
fn resolution_ladder_ceiling_is_848x480() {
    let (ceiling_res, ceiling_min) = RESOLUTION_LADDER[RESOLUTION_LADDER.len() - 1];
    assert_eq!(ceiling_res, HIGH, "ladder ceiling must be 848×480");
    assert_eq!(ceiling_min, SCREEN_UPGRADE_BPS);
}

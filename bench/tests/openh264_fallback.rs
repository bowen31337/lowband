//! Feature 131 — openh264 fallback on legacy CPUs.
//!
//! # Purpose
//!
//! Verifies that when the startup AV1 encode capability probe returns
//! [`Av1EncodeCapability::Legacy`], the gear-selection policy substitutes
//! [`CameraGear::GearC`] (OpenH264) for every thermal level that would
//! otherwise produce Gear A or Gear B, while preserving the voice floor,
//! the screen-refinement rules, and the allocation priority order.
//!
//! # Fallback model
//!
//! On a capable CPU the thermal cascade is:
//!
//!   Nominal → GearA, Fair → GearB(11), Serious → GearB(12), Critical → Off
//!
//! On a legacy CPU the cascade becomes:
//!
//!   Nominal → GearC, Fair → GearC, Serious → GearC, Critical → Off
//!
//! Gear C (OpenH264) has lower CPU cost than SVT-AV1 at any preset, so it
//! does not drive the thermal runaway that Gear B would on such hardware.
//! The camera remains live at all non-Critical thermal levels, preserving the
//! session quality at the cost of reduced compression efficiency.
//!
//! # Assertions
//!
//! 1. Legacy profile → `GearC` at Nominal, Fair, and Serious thermal levels.
//! 2. Legacy profile → `Off` at Critical (same as capable).
//! 3. `camera_allowed()` is true under GearC; `neural_camera_allowed()` is false.
//! 4. Voice floor (6 kbps) is invariant across all thermal levels and bandwidths.
//! 5. Capable profile at Nominal still produces `GearA` — fallback is additive.
//! 6. Allocation under GearC funds the camera channel.

use lowband_platform::gear_policy::{
    allocate, Av1EncodeCapability, CameraGear, GearConstraints, AUDIO_FLOOR_BPS,
};
use lowband_platform::thermal::ThermalPressure;

// ── helpers ──────────────────────────────────────────────────────────────────

fn legacy(pressure: ThermalPressure) -> GearConstraints {
    GearConstraints::from_thermal_with_capability(pressure, Av1EncodeCapability::Legacy)
}

fn capable(pressure: ThermalPressure) -> GearConstraints {
    GearConstraints::from_thermal_with_capability(pressure, Av1EncodeCapability::Capable)
}

// ── 1. Gear selection on legacy CPU ──────────────────────────────────────────

#[test]
fn legacy_cpu_nominal_selects_gearc_not_geara() {
    let c = legacy(ThermalPressure::Nominal);
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearC,
        "Gear A (NPU/AV1 head) must be replaced by Gear C on legacy CPU at Nominal"
    );
}

#[test]
fn legacy_cpu_fair_selects_gearc_not_gearb() {
    let c = legacy(ThermalPressure::Fair);
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearC,
        "Gear B SVT-AV1 must be replaced by Gear C on legacy CPU at Fair"
    );
}

#[test]
fn legacy_cpu_serious_selects_gearc_not_gearb_preset12() {
    let c = legacy(ThermalPressure::Serious);
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearC,
        "Gear B preset-12 must be replaced by Gear C on legacy CPU at Serious"
    );
    // Screen refinement is still suspended at Serious — thermal rules are independent
    // of the AV1 capability flag.
    assert!(
        !c.screen_refinement_allowed,
        "screen refinement must be suspended at Serious regardless of AV1 capability"
    );
}

#[test]
fn legacy_cpu_critical_still_turns_camera_off() {
    let c = legacy(ThermalPressure::Critical);
    assert_eq!(
        c.max_camera_gear,
        CameraGear::Off,
        "camera must be Off at Critical regardless of CPU capability"
    );
}

// ── 2. camera_allowed / neural_camera_allowed under GearC ────────────────────

#[test]
fn gearc_is_camera_on_not_neural() {
    let c = legacy(ThermalPressure::Nominal);
    assert!(
        c.camera_allowed(),
        "GearC must be a camera-on state — openh264 stream is active"
    );
    assert!(
        !c.neural_camera_allowed(),
        "GearC is not the neural talking-head gear"
    );
}

// ── 3. Voice floor is invariant on legacy CPU ─────────────────────────────────

#[test]
fn voice_floor_invariant_on_legacy_cpu() {
    for level in [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ] {
        let c = legacy(level);
        assert_eq!(
            c.audio_floor_bps, AUDIO_FLOOR_BPS,
            "voice floor must be 6 kbps on legacy CPU at {level:?}"
        );
    }
}

// ── 4. Capable path unchanged ─────────────────────────────────────────────────

#[test]
fn capable_cpu_nominal_still_allows_geara() {
    let c = capable(ThermalPressure::Nominal);
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearA,
        "capable CPU at Nominal must still produce GearA — fallback must not affect it"
    );
    assert!(c.neural_camera_allowed());
    assert_eq!(c.av1_encode, Av1EncodeCapability::Capable);
}

// ── 5. av1_encode field carries the probe result ──────────────────────────────

#[test]
fn constraints_carry_av1_encode_capability() {
    assert_eq!(
        legacy(ThermalPressure::Nominal).av1_encode,
        Av1EncodeCapability::Legacy
    );
    assert_eq!(
        capable(ThermalPressure::Nominal).av1_encode,
        Av1EncodeCapability::Capable
    );
}

// ── 6. Allocation funds camera under GearC ────────────────────────────────────

#[test]
fn allocation_funds_camera_under_gearc() {
    // At 400 kbps on a legacy CPU the camera slot must receive bandwidth.
    let c = legacy(ThermalPressure::Nominal);
    let b = allocate(400_000, &c);
    assert!(
        b.camera_bps > 0,
        "camera must receive allocation under GearC at 400 kbps"
    );
    assert!(
        b.audio_bps >= AUDIO_FLOOR_BPS,
        "voice floor must be honoured before camera receives any allocation"
    );
}

#[test]
fn allocation_at_survival_floor_still_zeroes_camera() {
    // At 6 kbps (exactly the voice floor) camera must receive nothing —
    // same invariant as on a capable CPU.
    let c = legacy(ThermalPressure::Nominal);
    let b = allocate(AUDIO_FLOOR_BPS, &c);
    assert!(b.audio_bps >= AUDIO_FLOOR_BPS);
    assert_eq!(
        b.camera_bps, 0,
        "camera must not be funded when link is at the voice floor even on a legacy CPU"
    );
}

// ── 7. Legacy CPU degradation is still monotone ───────────────────────────────

#[test]
fn legacy_degradation_is_monotone() {
    // On a legacy CPU the gear can only stay the same or decrease as thermal
    // pressure rises.  GearC → GearC → GearC → Off is a valid monotone sequence.
    fn gear_rank(g: CameraGear) -> u8 {
        match g {
            CameraGear::GearA => 3,
            CameraGear::GearB { .. } => 2,
            CameraGear::GearC => 1,
            CameraGear::Off => 0,
        }
    }

    let levels = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];

    let ranks: Vec<u8> = levels
        .iter()
        .map(|&l| gear_rank(legacy(l).max_camera_gear))
        .collect();

    for i in 1..ranks.len() {
        assert!(
            ranks[i] <= ranks[i - 1],
            "legacy-CPU camera gear must not improve as thermal pressure rises: \
             {:?} → {:?}",
            levels[i - 1],
            levels[i]
        );
    }
}

// ── 8. Voice floor across all bandwidths on legacy CPU ───────────────────────

#[test]
fn voice_never_starved_on_legacy_cpu_across_bandwidths_and_thermals() {
    let bw_scenarios = [4_000u32, 6_000, 12_000, 32_000, 64_000, 150_000, 400_000];
    let levels = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];

    for &bw in &bw_scenarios {
        for &level in &levels {
            let c = legacy(level);
            let b = allocate(bw, &c);
            assert!(
                b.audio_bps >= AUDIO_FLOOR_BPS,
                "voice dropped below 6 kbps on legacy CPU at bw={bw} thermal={level:?}: \
                 got {}",
                b.audio_bps
            );
        }
    }
}

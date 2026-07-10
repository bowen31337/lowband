//! Feature 82 — system rejects the neural head_gear unless an NPU or spare
//! CPU is available.
//!
//! # Purpose
//!
//! Verifies the two-condition gate that guards the neural talking-head gear
//! (Gear A / [`CameraGear::GearA`]):
//!
//! 1. **NPU present** → Gear A always available, regardless of CPU load.
//! 2. **NPU absent + CPU < 50 %** → Gear A available via the CPU execution
//!    provider.
//! 3. **NPU absent + CPU ≥ 50 %** → Gear A rejected; Gear B selected instead.
//!
//! # Architecture contract
//!
//! `head_gear_available(has_npu, cpu_pct)` is the canonical gate function
//! ([`lowband_nn::head_gear_gate`]).  `GearConstraints::from_thermal_with_capability_cpu_and_npu`
//! incorporates this gate after applying the thermal and AV1-capability rules,
//! so callers always get a consistent `max_camera_gear` that reflects all
//! constraints simultaneously.
//!
//! # Scenarios covered
//!
//! | # | NPU | CPU %  | Thermal | AV1 cap | Expected max gear |
//! |---|-----|--------|---------|---------|-------------------|
//! | 1 | Yes | any    | Nominal | Capable | GearA             |
//! | 2 | No  | 0 %    | Nominal | Capable | GearA (spare CPU) |
//! | 3 | No  | 49.9 % | Nominal | Capable | GearA (spare CPU) |
//! | 4 | No  | 50 %   | Nominal | Capable | GearB (rejected)  |
//! | 5 | No  | 80 %   | Nominal | Capable | GearB preset 12   |
//! | 6 | No  | 60 %   | Fair    | Capable | GearB (thermal)   |
//! | 7 | Yes | 80 %   | Fair    | Capable | GearB (thermal)   |
//! | 8 | No  | 20 %   | Nominal | Legacy  | GearC (legacy CPU)|

use lowband_nn::head_gear_gate::{
    head_gear_available, HeadGearCapability, CPU_HEADROOM_THRESHOLD_PCT,
};
use lowband_platform::gear_policy::{Av1EncodeCapability, CameraGear, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

// ── Unit-level gate function tests ────────────────────────────────────────────

#[test]
fn gate_npu_present_always_available() {
    // When an NPU is confirmed, the head gear must be available at every CPU
    // load — the synthesis network runs on the hardware accelerator, not on CPU.
    for cpu_pct in [0.0_f64, 25.0, 49.9, 50.0, 75.0, 100.0] {
        assert_eq!(
            head_gear_available(true, cpu_pct),
            HeadGearCapability::Available,
            "NPU present must make head gear Available at any CPU load (cpu={cpu_pct}%)"
        );
    }
}

#[test]
fn gate_no_npu_spare_cpu_available() {
    // No NPU, but CPU usage is below the 50% threshold: spare CPU permits Gear A.
    assert_eq!(
        head_gear_available(false, 0.0),
        HeadGearCapability::Available,
        "no NPU + 0% CPU → Available"
    );
    assert_eq!(
        head_gear_available(false, 49.9),
        HeadGearCapability::Available,
        "no NPU + 49.9% CPU → Available (strictly below threshold)"
    );
}

#[test]
fn gate_no_npu_at_threshold_rejected() {
    // Exactly at the threshold: no spare headroom → rejected.
    assert_eq!(
        head_gear_available(false, CPU_HEADROOM_THRESHOLD_PCT),
        HeadGearCapability::Rejected,
        "no NPU + {}% CPU → Rejected (at threshold)",
        CPU_HEADROOM_THRESHOLD_PCT
    );
}

#[test]
fn gate_no_npu_above_threshold_rejected() {
    for cpu_pct in [50.0_f64, 60.0, 75.0, 90.0, 100.0] {
        assert_eq!(
            head_gear_available(false, cpu_pct),
            HeadGearCapability::Rejected,
            "no NPU + {cpu_pct}% CPU → Rejected (at or above threshold)"
        );
    }
}

#[test]
fn gate_threshold_is_50_pct() {
    assert_eq!(
        CPU_HEADROOM_THRESHOLD_PCT, 50.0,
        "threshold must be 50%: above this the synthesis network \
         on CPU would breach the 35% constrained-tier ceiling"
    );
}

// ── GearConstraints integration — Scenario 1 ──────────────────────────────────

#[test]
fn scenario_1_npu_present_any_cpu_nominal_gear_a() {
    // NPU present + Nominal thermal → Gear A must be selected at any CPU load.
    for cpu_pct in [0.0_f64, 25.0, 50.0, 80.0, 100.0] {
        let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Capable,
            cpu_pct,
            true, // has_npu
        );
        assert_eq!(
            c.max_camera_gear,
            CameraGear::GearA,
            "NPU present + Nominal thermal must yield GearA at cpu_pct={cpu_pct}"
        );
        assert!(
            c.neural_camera_allowed(),
            "neural_camera_allowed must be true when Gear A is selected"
        );
    }
}

// ── GearConstraints integration — Scenario 2 ──────────────────────────────────

#[test]
fn scenario_2_no_npu_zero_cpu_nominal_gear_a() {
    // No NPU + 0% CPU + Nominal thermal → spare CPU path → Gear A.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Capable,
        0.0,
        false,
    );
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearA,
        "no NPU + 0% CPU + Nominal thermal must yield GearA (spare CPU path)"
    );
}

// ── GearConstraints integration — Scenario 3 ──────────────────────────────────

#[test]
fn scenario_3_no_npu_49pct_cpu_nominal_gear_a() {
    // No NPU + 49.9% CPU + Nominal thermal → still spare CPU → Gear A.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Capable,
        49.9,
        false,
    );
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearA,
        "no NPU + 49.9% CPU + Nominal thermal must still yield GearA (spare CPU path)"
    );
}

// ── GearConstraints integration — Scenario 4 ──────────────────────────────────

#[test]
fn scenario_4_no_npu_50pct_cpu_nominal_gear_b() {
    // No NPU + 50% CPU (at threshold) + Nominal thermal → head gear rejected → Gear B.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Capable,
        CPU_HEADROOM_THRESHOLD_PCT,
        false,
    );
    assert!(
        matches!(c.max_camera_gear, CameraGear::GearB { .. }),
        "no NPU + 50% CPU (threshold) must reject GearA and yield GearB; \
         got {:?}",
        c.max_camera_gear
    );
    assert!(
        !c.neural_camera_allowed(),
        "neural_camera_allowed must be false when head gear is rejected"
    );
}

// ── GearConstraints integration — Scenario 5 ──────────────────────────────────

#[test]
fn scenario_5_no_npu_80pct_cpu_nominal_gear_b_preset_12() {
    // No NPU + 80% CPU + Nominal thermal → GearB at preset 12 (CPU is ≥ 75%).
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Capable,
        80.0,
        false,
    );
    assert!(
        matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 12 }),
        "no NPU + 80% CPU must yield GearB at preset 12; got {:?}",
        c.max_camera_gear
    );
}

// ── GearConstraints integration — Scenario 6 ──────────────────────────────────

#[test]
fn scenario_6_no_npu_high_cpu_fair_thermal_gear_b() {
    // Fair thermal already excludes GearA; the head-gear gate has no additional
    // effect.  Gear B must still be selected.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Fair,
        Av1EncodeCapability::Capable,
        60.0,
        false,
    );
    assert!(
        matches!(c.max_camera_gear, CameraGear::GearB { .. }),
        "Fair thermal (independent of NPU gate) must yield GearB; got {:?}",
        c.max_camera_gear
    );
}

// ── GearConstraints integration — Scenario 7 ──────────────────────────────────

#[test]
fn scenario_7_npu_present_fair_thermal_gear_b() {
    // Even with an NPU present, Fair thermal caps the gear to Gear B.
    // Feature 82 cannot override the thermal policy.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Fair,
        Av1EncodeCapability::Capable,
        80.0,
        true, // has_npu
    );
    assert!(
        matches!(c.max_camera_gear, CameraGear::GearB { .. }),
        "Fair thermal must cap gear to GearB even with NPU; got {:?}",
        c.max_camera_gear
    );
}

// ── GearConstraints integration — Scenario 8 ──────────────────────────────────

#[test]
fn scenario_8_no_npu_spare_cpu_legacy_av1_gear_c() {
    // Legacy CPU: AV1 capability overrides everything — GearC regardless of
    // NPU state or CPU load.  Feature 82 must not promote the gear above GearC
    // on a legacy CPU.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Legacy,
        20.0,
        false,
    );
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearC,
        "legacy CPU must yield GearC regardless of NPU state or spare CPU"
    );
}

// ── NPU present + legacy CPU ──────────────────────────────────────────────────

#[test]
fn npu_present_legacy_cpu_still_gear_c() {
    // NPU present satisfies Feature 82, but the legacy-CPU rule (Feature 131)
    // still caps the gear to Gear C.
    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
        ThermalPressure::Nominal,
        Av1EncodeCapability::Legacy,
        10.0,
        true, // has_npu
    );
    assert_eq!(
        c.max_camera_gear,
        CameraGear::GearC,
        "legacy CPU with NPU present must still yield GearC (AV1 cap takes precedence)"
    );
}

// ── Critical thermal is unaffected by NPU/CPU gate ───────────────────────────

#[test]
fn critical_thermal_forces_camera_off_regardless_of_npu() {
    for has_npu in [false, true] {
        let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
            ThermalPressure::Critical,
            Av1EncodeCapability::Capable,
            10.0,
            has_npu,
        );
        assert_eq!(
            c.max_camera_gear,
            CameraGear::Off,
            "Critical thermal must force camera Off regardless of NPU (has_npu={has_npu})"
        );
    }
}

// ── Exhaustive: Nominal thermal, no NPU, CPU sweep ───────────────────────────

#[test]
fn nominal_no_npu_cpu_sweep_boundary() {
    // Sweep CPU usage around the 50% boundary to verify the gate transitions
    // cleanly from Available (GearA) to Rejected (GearB) at exactly 50%.
    let sub_threshold = [0.0_f64, 10.0, 25.0, 30.0, 49.9, 49.99];
    let at_or_above = [50.0_f64, 50.01, 60.0, 75.0, 90.0, 100.0];

    for cpu_pct in sub_threshold {
        let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Capable,
            cpu_pct,
            false,
        );
        assert_eq!(
            c.max_camera_gear,
            CameraGear::GearA,
            "cpu_pct={cpu_pct} is below threshold — must yield GearA"
        );
    }

    for cpu_pct in at_or_above {
        let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Capable,
            cpu_pct,
            false,
        );
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { .. }),
            "cpu_pct={cpu_pct} is at or above threshold — must yield GearB; \
             got {:?}",
            c.max_camera_gear
        );
    }
}

// ── Audio floor invariant ─────────────────────────────────────────────────────

#[test]
fn audio_floor_invariant_holds_across_all_npu_and_cpu_combinations() {
    use lowband_platform::gear_policy::{allocate, AUDIO_FLOOR_BPS};

    let pressures = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];
    let npu_states = [false, true];
    let cpu_pcts = [0.0_f64, 49.9, 50.0, 80.0];
    let bandwidths = [4_000u32, 6_000, 32_000, 64_000, 150_000, 400_000];

    for &pressure in &pressures {
        for &has_npu in &npu_states {
            for &cpu_pct in &cpu_pcts {
                for &bw in &bandwidths {
                    let c = GearConstraints::from_thermal_with_capability_cpu_and_npu(
                        pressure,
                        Av1EncodeCapability::Capable,
                        cpu_pct,
                        has_npu,
                    );
                    let b = allocate(bw, &c);
                    assert!(
                        b.audio_bps >= AUDIO_FLOOR_BPS,
                        "audio floor violated: pressure={pressure:?} has_npu={has_npu} \
                         cpu_pct={cpu_pct} bw={bw}: got {} bps, need ≥ {AUDIO_FLOOR_BPS}",
                        b.audio_bps
                    );
                }
            }
        }
    }
}

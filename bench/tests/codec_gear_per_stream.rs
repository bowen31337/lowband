//! Feature 72 — system selects codec gears per stream with tier_budget allocations.
//!
//! # Purpose
//!
//! Verifies that [`select_codec_gears`] produces the correct per-stream codec
//! assignment for every combination of tier, bandwidth, thermal pressure, and
//! NPU capability that the governor can encounter in a real session.
//!
//! # Architecture contract
//!
//! The governor runs at 10 Hz.  Each tick it:
//!
//! 1. Classifies the BWE into a [`TierState`].
//! 2. Derives [`GearConstraints`] from thermal pressure.
//! 3. Calls [`allocate`] to obtain [`StreamBudgets`].
//! 4. Calls [`select_codec_gears`] to obtain [`TierCodecGears`] — the sole
//!    authoritative codec assignment distributed to all stream encoders.
//!
//! Key invariants:
//!
//! - NeuralVocoder activates **only** at `Survival` tier with `NpuCapability::Present`.
//! - `camera_gear == CameraGear::Off` whenever `camera_bps == 0`, even if
//!   thermals would otherwise permit a higher gear.
//! - `screen_refinement_enabled` requires **both** thermal permission **and** a
//!   non-zero `screen_refinement_bps` allocation.
//! - The audio floor (6 kbps) must be funded before any other stream at every
//!   tier, thermal level, and bandwidth.
//!
//! # Scenarios covered
//!
//! | # | Tier | BW kbps | Thermal | NPU | Expected audio gear | Expected camera gear |
//! |---|------|---------|---------|-----|---------------------|----------------------|
//! | 1 | Survival | 48 | Nominal | Present | NeuralVocoder | Off |
//! | 2 | Survival | 48 | Nominal | Absent  | OpusSilk/SilkWb | Off |
//! | 3 | Constrained | 150 | Nominal | Absent | OpusSilk/SilkHybridWb | GearA |
//! | 4 | Comfortable | 300 | Nominal | Absent | OpusSilk/HybridSwb | GearA |
//! | 5 | Full | 400 | Nominal | Absent | OpusSilk/CeltFb | GearA |
//! | 6 | Constrained | 150 | Fair | Absent | OpusSilk/SilkHybridWb | GearB |
//! | 7 | Full | 400 | Critical | Absent | OpusSilk/CeltFb | Off |
//! | 8 | Survival | 30 | Nominal | Present | NeuralVocoder | Off (budget) |

use lowband_platform::gear_policy::{allocate, CameraGear, GearConstraints, AUDIO_FLOOR_BPS};
use lowband_platform::neural_vocoder::{AudioGear, NpuCapability};
use lowband_platform::opus_encoder::{
    OpusMode, COMFORTABLE_AUDIO_BPS, CONSTRAINED_AUDIO_BPS, FULL_AUDIO_BPS,
    SURVIVAL_FALLBACK_AUDIO_BPS,
};
use lowband_platform::thermal::ThermalPressure;
use lowband_platform::tier::TierState;
use lowband_platform::tier_gears::select_codec_gears;

// ── Scenario 1: Survival + NPU → NeuralVocoder + camera Off ──────────────────

#[test]
fn survival_with_npu_activates_neural_vocoder_and_camera_off() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(48_000, &c);
    // Camera receives no bandwidth at 48 kbps (audio+input+screen exhaust it).
    assert_eq!(b.camera_bps, 0, "precondition: 48 kbps leaves no camera headroom");

    let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Present);

    assert!(
        matches!(g.audio_gear, AudioGear::NeuralVocoder { .. }),
        "Survival + NPU must activate NeuralVocoder; got {:?}", g.audio_gear
    );
    assert!(
        g.opus_settings.is_none(),
        "NeuralVocoder bypasses Opus; opus_settings must be None"
    );
    assert_eq!(
        g.camera_gear,
        CameraGear::Off,
        "camera_gear must be Off when camera_bps == 0"
    );
}

// ── Scenario 2: Survival + no NPU → OpusSilk/SilkWb + camera Off ─────────────

#[test]
fn survival_without_npu_selects_silk_wb() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(48_000, &c);

    let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Absent);

    assert_eq!(g.audio_gear, AudioGear::OpusSilk,
        "Survival without NPU must fall back to OpusSilk");
    let s = g.opus_settings.expect("OpusSilk must set opus_settings");
    assert_eq!(s.mode, OpusMode::SilkWb,
        "Survival Opus fallback must use pure SILK-WB");
    assert_eq!(s.target_bps, SURVIVAL_FALLBACK_AUDIO_BPS,
        "Survival Opus fallback must target {SURVIVAL_FALLBACK_AUDIO_BPS} bps");
    assert_eq!(g.camera_gear, CameraGear::Off,
        "camera must be Off when camera_bps == 0");
}

// ── Scenario 3: Constrained + Nominal thermal → SilkHybridWb + GearA ─────────

#[test]
fn constrained_nominal_thermal_selects_silk_hybrid_wb_and_gear_a() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(150_000, &c);
    assert!(b.camera_bps > 0, "precondition: 150 kbps must fund camera");

    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);

    let s = g.opus_settings.expect("OpusSilk must set opus_settings");
    assert_eq!(s.mode, OpusMode::SilkHybridWb,
        "Constrained tier must select SILK/hybrid WB");
    assert_eq!(s.target_bps, CONSTRAINED_AUDIO_BPS,
        "Constrained tier must target {CONSTRAINED_AUDIO_BPS} bps");
    assert_eq!(g.camera_gear, CameraGear::GearA,
        "Nominal thermal + non-zero camera budget must produce GearA");
}

// ── Scenario 4: Comfortable + Nominal thermal → HybridSwb + GearA ────────────

#[test]
fn comfortable_nominal_thermal_selects_hybrid_swb_and_gear_a() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(300_000, &c);

    let g = select_codec_gears(TierState::Comfortable, &b, &c, NpuCapability::Absent);

    let s = g.opus_settings.unwrap();
    assert_eq!(s.mode, OpusMode::HybridSwb, "Comfortable tier must use hybrid SWB");
    assert_eq!(s.target_bps, COMFORTABLE_AUDIO_BPS);
    assert_eq!(g.camera_gear, CameraGear::GearA);
}

// ── Scenario 5: Full + Nominal thermal → CeltFb + GearA ──────────────────────

#[test]
fn full_nominal_thermal_selects_celt_fb_and_gear_a() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(400_000, &c);

    let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);

    let s = g.opus_settings.unwrap();
    assert_eq!(s.mode, OpusMode::CeltFb, "Full tier must use CELT FB");
    assert_eq!(s.target_bps, FULL_AUDIO_BPS);
    assert_eq!(g.camera_gear, CameraGear::GearA);
}

// ── Scenario 6: Constrained + Fair thermal → SilkHybridWb + GearB ───────────

#[test]
fn constrained_fair_thermal_selects_gear_b() {
    let c = GearConstraints::from_thermal(ThermalPressure::Fair);
    let b = allocate(150_000, &c);
    assert!(b.camera_bps > 0, "precondition: 150 kbps must fund camera at Fair");

    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);

    assert!(
        matches!(g.camera_gear, CameraGear::GearB { .. }),
        "Fair thermal + funded camera must produce GearB; got {:?}", g.camera_gear
    );
    let s = g.opus_settings.unwrap();
    assert_eq!(s.mode, OpusMode::SilkHybridWb,
        "Tier selection is independent of thermal: Constrained → SilkHybridWb");
}

// ── Scenario 7: Full + Critical thermal → CeltFb + camera Off ────────────────

#[test]
fn full_critical_thermal_turns_camera_off() {
    let c = GearConstraints::from_thermal(ThermalPressure::Critical);
    // Even at 400 kbps the allocator zeroes camera_bps when camera_allowed() == false.
    let b = allocate(400_000, &c);
    assert_eq!(b.camera_bps, 0,
        "Critical thermal must zero camera_bps regardless of bandwidth");

    let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);

    assert_eq!(g.camera_gear, CameraGear::Off,
        "Critical thermal must force camera Off even at Full tier and 400 kbps");
    let s = g.opus_settings.unwrap();
    assert_eq!(s.mode, OpusMode::CeltFb, "audio mode is tier-driven, not thermal-driven");
}

// ── Scenario 8: Survival + tight link + NPU → NeuralVocoder + camera Off ─────

#[test]
fn survival_tight_link_with_npu_keeps_camera_off() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(30_000, &c); // even tighter than survival minimum
    assert_eq!(b.camera_bps, 0, "precondition: camera cannot be funded at 30 kbps");

    let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Present);

    assert!(matches!(g.audio_gear, AudioGear::NeuralVocoder { .. }),
        "NeuralVocoder must activate at Survival+NPU regardless of camera budget");
    assert_eq!(g.camera_gear, CameraGear::Off);
}

// ── Screen refinement gate ────────────────────────────────────────────────────

#[test]
fn screen_refinement_enabled_at_400kbps_nominal() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(400_000, &c);
    assert!(b.screen_refinement_bps > 0,
        "precondition: 400 kbps must leave refinement budget");

    let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);

    assert!(g.screen_refinement_enabled,
        "refinement must be enabled when thermal allows and budget > 0");
}

#[test]
fn screen_refinement_disabled_when_budget_exhausted() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(150_000, &c);
    assert_eq!(b.screen_refinement_bps, 0,
        "precondition: camera exhausts remaining budget at 150 kbps");

    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);

    assert!(!g.screen_refinement_enabled,
        "refinement must be disabled when screen_refinement_bps == 0");
}

#[test]
fn screen_refinement_disabled_at_serious_thermal() {
    let c = GearConstraints::from_thermal(ThermalPressure::Serious);
    let b = allocate(400_000, &c);

    let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);

    assert!(!g.screen_refinement_enabled,
        "Serious thermal must disable refinement regardless of budget");
}

// ── Strict-priority invariant: audio floor funded at every combination ────────

#[test]
fn audio_floor_funded_at_every_tier_thermal_and_bandwidth() {
    let tiers = [
        TierState::Survival, TierState::Constrained,
        TierState::Comfortable, TierState::Full,
    ];
    let thermals = [
        ThermalPressure::Nominal, ThermalPressure::Fair,
        ThermalPressure::Serious, ThermalPressure::Critical,
    ];
    let bandwidths = [4_000u32, 6_000, 32_000, 64_000, 150_000, 400_000];

    for &tier in &tiers {
        for &pressure in &thermals {
            for &bw in &bandwidths {
                let c = GearConstraints::from_thermal(pressure);
                let b = allocate(bw, &c);
                assert!(
                    b.audio_bps >= AUDIO_FLOOR_BPS,
                    "audio floor violated: tier={tier:?} pressure={pressure:?} bw={bw}: \
                     got {} bps, need ≥ {AUDIO_FLOOR_BPS}",
                    b.audio_bps
                );
            }
        }
    }
}

// ── NeuralVocoder exclusivity ─────────────────────────────────────────────────

#[test]
fn neural_vocoder_never_active_above_survival_tier() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(400_000, &c);

    for &tier in &[TierState::Constrained, TierState::Comfortable, TierState::Full] {
        let g = select_codec_gears(tier, &b, &c, NpuCapability::Present);
        assert_eq!(
            g.audio_gear, AudioGear::OpusSilk,
            "NeuralVocoder must not activate at {tier:?} (only Survival)"
        );
        assert!(g.opus_settings.is_some(),
            "opus_settings must be populated when OpusSilk is active (tier={tier:?})");
    }
}

// ── Budget fields carried faithfully into TierCodecGears ─────────────────────

#[test]
fn codec_gears_carry_display_resolution_from_budgets() {
    let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let b = allocate(64_000, &c);
    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
    assert_eq!(g.display_resolution, b.display_resolution,
        "display_resolution must be copied verbatim from StreamBudgets");
}

#[test]
fn codec_gears_carry_per_frame_byte_cap_from_budgets() {
    let c = GearConstraints::from_thermal(ThermalPressure::Fair);
    let b = allocate(200_000, &c);
    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
    assert_eq!(g.per_frame_byte_cap, b.per_frame_byte_cap,
        "per_frame_byte_cap must mirror StreamBudgets");
    assert!(g.per_frame_byte_cap > 0, "Gear B at 200 kbps must produce a non-zero cap");
}

#[test]
fn codec_gears_carry_roi_delta_qp_from_budgets() {
    let c = GearConstraints::from_thermal(ThermalPressure::Fair);
    let b = allocate(200_000, &c);
    let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
    assert_eq!(g.roi_delta_qp, b.roi_delta_qp,
        "roi_delta_qp must mirror StreamBudgets");
}

// ── Camera Off → per-frame cap and ROI delta must be zero ────────────────────

#[test]
fn camera_off_produces_zero_per_frame_cap_and_roi_delta() {
    let c = GearConstraints::from_thermal(ThermalPressure::Critical);
    let b = allocate(400_000, &c);
    assert_eq!(b.camera_bps, 0, "precondition");

    let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);

    assert_eq!(g.camera_gear, CameraGear::Off);
    assert_eq!(g.per_frame_byte_cap, 0,
        "per_frame_byte_cap must be zero when camera is Off");
    assert_eq!(g.roi_delta_qp, 0,
        "roi_delta_qp must be zero when camera is Off");
}

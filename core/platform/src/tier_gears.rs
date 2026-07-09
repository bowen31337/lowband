//! Per-stream codec gear selection from tier and budget allocations — Feature 72.
//!
//! The governor calls [`select_codec_gears`] once per 10 Hz interval after it
//! has determined the current [`TierState`] from the bandwidth estimate and
//! obtained [`StreamBudgets`] from [`crate::gear_policy::allocate`].  The
//! function resolves the codec gear for every stream in one place so that
//! downstream encoders receive a single authoritative assignment rather than
//! independently interpreting tier and thermal inputs.
//!
//! # Allocation → gear mapping
//!
//! | Stream | Gear driver |
//! |--------|-------------|
//! | Audio  | [`TierState`] + [`NpuCapability`]: NeuralVocoder at Survival+NPU, OpusSilk elsewhere |
//! | Camera | [`GearConstraints::max_camera_gear`] capped to [`CameraGear::Off`] when `camera_bps == 0` |
//! | Screen refinement | [`GearConstraints::screen_refinement_allowed`] **and** `screen_refinement_bps > 0` |
//! | Display resolution | carried from [`StreamBudgets::display_resolution`] |
//! | Per-frame cap      | carried from [`StreamBudgets::per_frame_byte_cap`] |
//! | ROI QP delta       | carried from [`StreamBudgets::roi_delta_qp`] |
//!
//! # Why camera_bps == 0 overrides thermal constraints
//!
//! [`crate::gear_policy::allocate`] zeroes `camera_bps` whenever there is not
//! enough headroom after audio, input, and screen have been funded (strict
//! priority).  In that case the camera encoder receives nothing regardless of
//! what the thermal policy allows.  Returning [`CameraGear::Off`] here makes
//! the "encoder gets no bits → encoder should not run" invariant explicit so
//! callers do not need to cross-check budget and gear independently.

use crate::gear_policy::{CameraGear, DisplayResolution, GearConstraints, StreamBudgets};
use crate::neural_vocoder::{audio_gear_from_tier_and_npu, AudioGear, NpuCapability};
use crate::opus_encoder::{opus_settings_from_tier, OpusTierSettings};
use crate::tier::TierState;

/// Per-stream codec gear assignments for one governor interval (Feature 72).
///
/// Produced by [`select_codec_gears`]; consumed by stream encoders that need to
/// know which codec to run and at what quality tier for the current interval.
///
/// All fields are derived from [`TierState`], [`StreamBudgets`],
/// [`GearConstraints`], and [`NpuCapability`] — none require additional inputs.
#[derive(Debug, Clone, Copy)]
pub struct TierCodecGears {
    /// Audio codec gear: [`AudioGear::NeuralVocoder`] at Survival tier when an
    /// NPU is present, [`AudioGear::OpusSilk`] in all other combinations.
    pub audio_gear: AudioGear,
    /// Opus encoder mode and target bitrate.  Present when
    /// `audio_gear == AudioGear::OpusSilk`; `None` when the neural vocoder
    /// is active (it bypasses Opus entirely).
    pub opus_settings: Option<OpusTierSettings>,
    /// Camera encoder gear after applying both thermal constraints and the
    /// budget check.  [`CameraGear::Off`] when `camera_bps == 0` even if
    /// thermals would permit a higher gear.
    pub camera_gear: CameraGear,
    /// Whether the screen build-to-lossless refinement lane may run this
    /// interval.  `false` when thermals suspend it **or** when
    /// `screen_refinement_bps == 0`.
    pub screen_refinement_enabled: bool,
    /// Display resolution selected from the two-rung resolution ladder.
    /// Mirrors [`StreamBudgets::display_resolution`].
    pub display_resolution: DisplayResolution,
    /// Per-frame byte ceiling for Gear B (SVT-AV1) output.  Zero for all
    /// other camera gears.  Mirrors [`StreamBudgets::per_frame_byte_cap`].
    pub per_frame_byte_cap: u32,
    /// ROI QP delta for face tiles in Gear B SVT-AV1 encoding.  Zero for
    /// all other camera gears.  Mirrors [`StreamBudgets::roi_delta_qp`].
    pub roi_delta_qp: i8,
}

/// Select per-stream codec gears from the current tier, allocated budgets, and
/// hardware capabilities.
///
/// This is the Feature 72 entry point.  The governor calls this once per 10 Hz
/// tick after:
///
/// 1. Classifying the link into a [`TierState`] from the BWE.
/// 2. Calling [`crate::gear_policy::GearConstraints::from_thermal`] for the
///    current thermal pressure.
/// 3. Calling [`crate::gear_policy::allocate`] with the BWE and constraints.
///
/// The returned [`TierCodecGears`] is the sole source of truth for which codec
/// each stream encoder should run during this interval.
pub fn select_codec_gears(
    tier: TierState,
    budgets: &StreamBudgets,
    constraints: &GearConstraints,
    npu: NpuCapability,
) -> TierCodecGears {
    let audio_gear = audio_gear_from_tier_and_npu(tier, npu);

    let opus_settings = match audio_gear {
        AudioGear::OpusSilk => Some(opus_settings_from_tier(tier)),
        AudioGear::NeuralVocoder { .. } => None,
    };

    // Camera: use the thermal-constrained gear, but the allocator's strict
    // priority may have left camera_bps == 0.  In that case the encoder must
    // not run even if the thermal policy would have allowed it.
    let camera_gear = if budgets.camera_bps == 0 {
        CameraGear::Off
    } else {
        constraints.max_camera_gear
    };

    // Screen refinement requires both thermal permission and a non-zero budget.
    let screen_refinement_enabled =
        constraints.screen_refinement_allowed && budgets.screen_refinement_bps > 0;

    TierCodecGears {
        audio_gear,
        opus_settings,
        camera_gear,
        screen_refinement_enabled,
        display_resolution: budgets.display_resolution,
        per_frame_byte_cap: budgets.per_frame_byte_cap,
        roi_delta_qp: budgets.roi_delta_qp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear_policy::{allocate, AUDIO_FLOOR_BPS};
    use crate::neural_vocoder::{NEURAL_VOCODER_HI_BPS, NEURAL_VOCODER_LO_BPS};
    use crate::opus_encoder::{
        COMFORTABLE_AUDIO_BPS, CONSTRAINED_AUDIO_BPS, FULL_AUDIO_BPS,
        SURVIVAL_FALLBACK_AUDIO_BPS,
    };
    use crate::thermal::ThermalPressure;

    fn nominal_constraints() -> GearConstraints {
        GearConstraints::from_thermal(ThermalPressure::Nominal)
    }

    // ── Audio gear selection ──────────────────────────────────────────────────

    #[test]
    fn survival_with_npu_selects_neural_vocoder() {
        let c = nominal_constraints();
        let b = allocate(64_000, &c);
        let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Present);
        assert!(
            matches!(g.audio_gear, AudioGear::NeuralVocoder { .. }),
            "Survival + NPU must activate NeuralVocoder; got {:?}", g.audio_gear
        );
        assert!(g.opus_settings.is_none(), "NeuralVocoder bypasses Opus; opus_settings must be None");
    }

    #[test]
    fn neural_vocoder_target_bps_within_bounds() {
        let c = nominal_constraints();
        let b = allocate(64_000, &c);
        let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Present);
        if let AudioGear::NeuralVocoder { target_bps } = g.audio_gear {
            assert!(
                target_bps >= NEURAL_VOCODER_LO_BPS && target_bps <= NEURAL_VOCODER_HI_BPS,
                "target_bps {target_bps} must be within [{NEURAL_VOCODER_LO_BPS}, {NEURAL_VOCODER_HI_BPS}]"
            );
        } else {
            panic!("expected NeuralVocoder");
        }
    }

    #[test]
    fn survival_without_npu_selects_opus_silk_wb() {
        let c = nominal_constraints();
        let b = allocate(48_000, &c);
        let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Absent);
        assert_eq!(g.audio_gear, AudioGear::OpusSilk);
        let s = g.opus_settings.expect("OpusSilk must populate opus_settings");
        assert_eq!(s.mode, crate::opus_encoder::OpusMode::SilkWb);
        assert_eq!(s.target_bps, SURVIVAL_FALLBACK_AUDIO_BPS);
    }

    #[test]
    fn constrained_tier_selects_silk_hybrid_wb() {
        let c = nominal_constraints();
        let b = allocate(150_000, &c);
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert_eq!(g.audio_gear, AudioGear::OpusSilk);
        let s = g.opus_settings.unwrap();
        assert_eq!(s.mode, crate::opus_encoder::OpusMode::SilkHybridWb);
        assert_eq!(s.target_bps, CONSTRAINED_AUDIO_BPS);
    }

    #[test]
    fn comfortable_tier_selects_hybrid_swb() {
        let c = nominal_constraints();
        let b = allocate(300_000, &c);
        let g = select_codec_gears(TierState::Comfortable, &b, &c, NpuCapability::Absent);
        let s = g.opus_settings.unwrap();
        assert_eq!(s.mode, crate::opus_encoder::OpusMode::HybridSwb);
        assert_eq!(s.target_bps, COMFORTABLE_AUDIO_BPS);
    }

    #[test]
    fn full_tier_selects_celt_fb() {
        let c = nominal_constraints();
        let b = allocate(400_000, &c);
        let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);
        let s = g.opus_settings.unwrap();
        assert_eq!(s.mode, crate::opus_encoder::OpusMode::CeltFb);
        assert_eq!(s.target_bps, FULL_AUDIO_BPS);
    }

    // ── Camera gear selection ─────────────────────────────────────────────────

    #[test]
    fn camera_off_when_budget_is_zero() {
        let c = nominal_constraints();
        // At 30 kbps audio+input consume all headroom; camera_bps == 0.
        let b = allocate(30_000, &c);
        assert_eq!(b.camera_bps, 0, "precondition: camera_bps must be 0 at 30 kbps");
        let g = select_codec_gears(TierState::Survival, &b, &c, NpuCapability::Absent);
        assert_eq!(g.camera_gear, CameraGear::Off,
            "camera_gear must be Off when budget allocator gives camera_bps == 0");
    }

    #[test]
    fn nominal_thermal_with_budget_selects_gear_a() {
        let c = nominal_constraints();
        let b = allocate(150_000, &c);
        assert!(b.camera_bps > 0, "precondition: camera must be funded at 150 kbps");
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert_eq!(g.camera_gear, CameraGear::GearA,
            "Nominal thermal + non-zero budget must yield GearA");
    }

    #[test]
    fn fair_thermal_with_budget_selects_gear_b() {
        let c = GearConstraints::from_thermal(ThermalPressure::Fair);
        let b = allocate(150_000, &c);
        assert!(b.camera_bps > 0, "precondition: camera must be funded at 150 kbps");
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert!(
            matches!(g.camera_gear, CameraGear::GearB { .. }),
            "Fair thermal + budget must yield GearB; got {:?}", g.camera_gear
        );
    }

    #[test]
    fn critical_thermal_forces_camera_off_regardless_of_bandwidth() {
        let c = GearConstraints::from_thermal(ThermalPressure::Critical);
        // At 400 kbps the budget is generous, but Critical thermal must still
        // produce camera Off (allocator zeroes camera_bps when camera_allowed() == false).
        let b = allocate(400_000, &c);
        assert_eq!(b.camera_bps, 0, "Critical thermal: allocator must zero camera_bps");
        let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);
        assert_eq!(g.camera_gear, CameraGear::Off,
            "Critical thermal must yield camera Off regardless of tier and bandwidth");
    }

    // ── Screen refinement ─────────────────────────────────────────────────────

    #[test]
    fn refinement_enabled_when_thermal_allows_and_budget_positive() {
        let c = nominal_constraints();
        // 400 kbps leaves headroom for refinement after all other streams.
        let b = allocate(400_000, &c);
        assert!(b.screen_refinement_bps > 0, "precondition: refinement budget must be > 0");
        let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);
        assert!(g.screen_refinement_enabled,
            "refinement must be enabled when thermal allows and budget is positive");
    }

    #[test]
    fn refinement_disabled_when_budget_is_zero() {
        let c = nominal_constraints();
        // 150 kbps: camera takes all remaining headroom; refinement budget == 0.
        let b = allocate(150_000, &c);
        assert_eq!(b.screen_refinement_bps, 0, "precondition: refinement budget must be 0");
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert!(!g.screen_refinement_enabled,
            "refinement must be disabled when screen_refinement_bps == 0");
    }

    #[test]
    fn refinement_disabled_at_serious_thermal_regardless_of_bandwidth() {
        let c = GearConstraints::from_thermal(ThermalPressure::Serious);
        let b = allocate(400_000, &c);
        let g = select_codec_gears(TierState::Full, &b, &c, NpuCapability::Absent);
        assert!(!g.screen_refinement_enabled,
            "Serious thermal must suspend screen refinement regardless of budget");
    }

    // ── Budget fields propagated faithfully ───────────────────────────────────

    #[test]
    fn display_resolution_mirrors_budgets() {
        let c = nominal_constraints();
        let b = allocate(64_000, &c);
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert_eq!(g.display_resolution, b.display_resolution,
            "display_resolution must be copied verbatim from StreamBudgets");
    }

    #[test]
    fn per_frame_byte_cap_mirrors_budgets() {
        let c = GearConstraints::from_thermal(ThermalPressure::Fair);
        let b = allocate(200_000, &c);
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert_eq!(g.per_frame_byte_cap, b.per_frame_byte_cap,
            "per_frame_byte_cap must be copied verbatim from StreamBudgets");
    }

    #[test]
    fn roi_delta_qp_mirrors_budgets() {
        let c = GearConstraints::from_thermal(ThermalPressure::Fair);
        let b = allocate(200_000, &c);
        let g = select_codec_gears(TierState::Constrained, &b, &c, NpuCapability::Absent);
        assert_eq!(g.roi_delta_qp, b.roi_delta_qp,
            "roi_delta_qp must be copied verbatim from StreamBudgets");
    }

    // ── Invariants across tier × bandwidth × thermal ──────────────────────────

    #[test]
    fn audio_floor_always_honoured() {
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
                        "audio floor violated at tier={tier:?} pressure={pressure:?} bw={bw}: \
                         got {} bps", b.audio_bps
                    );
                }
            }
        }
    }

    #[test]
    fn camera_off_when_camera_bps_is_zero_across_all_tiers() {
        // Regardless of tier or thermal, if the allocator sets camera_bps == 0
        // the gear selector must return CameraGear::Off.
        let c = nominal_constraints();
        let b = allocate(30_000, &c);
        assert_eq!(b.camera_bps, 0, "precondition");

        for &tier in &[TierState::Survival, TierState::Constrained,
                       TierState::Comfortable, TierState::Full] {
            let g = select_codec_gears(tier, &b, &c, NpuCapability::Absent);
            assert_eq!(
                g.camera_gear, CameraGear::Off,
                "camera_gear must be Off when camera_bps == 0 (tier={tier:?})"
            );
        }
    }

    #[test]
    fn neural_vocoder_only_at_survival_with_npu() {
        let c = nominal_constraints();
        let b = allocate(150_000, &c);

        for &tier in &[TierState::Constrained, TierState::Comfortable, TierState::Full] {
            let g = select_codec_gears(tier, &b, &c, NpuCapability::Present);
            assert_eq!(
                g.audio_gear, AudioGear::OpusSilk,
                "NeuralVocoder must not activate above Survival (tier={tier:?})"
            );
            assert!(g.opus_settings.is_some(),
                "opus_settings must be present when OpusSilk is active (tier={tier:?})");
        }
    }
}

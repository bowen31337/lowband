//! Neural-vocoder audio gear selection — Feature 54.
//!
//! When the governor declares Survival tier and a hardware NPU is detected,
//! the audio pipeline switches from Opus to a SoundStream-lineage neural
//! vocoder operating at 3.2–6 kbps.  This keeps a call intelligible on links
//! that can barely carry anything — 6 kbps of voice inside a 48 kbps budget
//! leaves 40 kbps for the screen.
//!
//! # Decision rule
//!
//! | Tier      | NPU      | Audio gear                   |
//! |-----------|----------|------------------------------|
//! | Survival  | Present  | NeuralVocoder (3.2–6 kbps)   |
//! | Survival  | Absent   | OpusSilk (fallback)          |
//! | Any other | any      | OpusSilk                     |
//!
//! At every non-Survival tier the governor keeps Opus (SILK / hybrid / CELT
//! as appropriate to the tier) regardless of NPU availability; the neural
//! vocoder is a survival-only optimisation.
//!
//! # NPU probe
//!
//! [`NpuCapability::probe`] detects whether a hardware neural accelerator is
//! present at startup.  The full ONNX Runtime execution-provider probe
//! (Feature 78) lives in the `nn` crate (Phase 4).  This module provides a
//! lightweight `cfg`-gated stub: on Apple Silicon (aarch64 macOS / iOS) the
//! Neural Engine is always present; on all other platforms the result is
//! conservatively [`Absent`](NpuCapability::Absent) until Phase 4 wires in
//! the full provider query.

use crate::tier::TierState;

/// Lower bound of the neural-vocoder target bitrate (bps).
///
/// SoundStream-lineage codecs maintain speech intelligibility down to 3.2 kbps;
/// the RVQ quantiser can drive the achieved rate to this floor under congestion.
pub const NEURAL_VOCODER_LO_BPS: u32 = 3_200;

/// Upper bound (and default target) of the neural-vocoder bitrate (bps).
///
/// Architecture §8.1: Survival (NPU/CPU-ok) targets 3.2–6 kbps.  The governor
/// passes this value as the encoder's rate cap; the quantiser drives the
/// achieved rate toward [`NEURAL_VOCODER_LO_BPS`] when the link is tighter.
pub const NEURAL_VOCODER_HI_BPS: u32 = 6_000;

/// Whether a hardware NPU (or GPU-backed neural inference path) is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpuCapability {
    /// A hardware neural accelerator was confirmed by the startup probe.
    /// The neural vocoder may be scheduled on it at Survival tier.
    Present,
    /// No hardware NPU detected.  The neural vocoder is not activated;
    /// the system falls back to Opus SILK-WB at 9–12 kbps.
    Absent,
}

impl NpuCapability {
    /// Probe for a hardware neural accelerator at startup.
    ///
    /// On Apple Silicon (aarch64 macOS / iOS) the Neural Engine is always
    /// present, so the probe returns [`Present`](Self::Present) immediately
    /// with no I/O or memory allocation.  On all other platforms this returns
    /// [`Absent`](Self::Absent) conservatively; the full ONNX Runtime
    /// execution-provider query (Feature 78, `nn::capability_probe`) will
    /// replace this stub in Phase 4.
    ///
    /// **Non-blocking** — does not load any model or allocate GPU/NPU memory.
    pub fn probe() -> Self {
        // Apple Silicon: Neural Engine is always present on aarch64 macOS (M1+)
        // and all modern iOS devices.  Report Present without any I/O.
        #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "ios")))]
        return Self::Present;

        // All other platforms: conservatively absent until the full ONNX
        // Runtime provider query (nn::capability_probe, Phase 4) is wired in.
        #[allow(unreachable_code)]
        Self::Absent
    }
}

/// Which audio codec the governor should use for a given tier and NPU state.
///
/// Rank (highest to lowest compression efficiency): `NeuralVocoder` > `OpusSilk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioGear {
    /// Neural vocoder (SoundStream-lineage, RVQ latents @ 50 Hz).
    ///
    /// Activated at Survival tier when [`NpuCapability::Present`].  The
    /// encoder targets `target_bps`; the RVQ quantiser can drive the achieved
    /// rate as low as [`NEURAL_VOCODER_LO_BPS`] under severe congestion.
    NeuralVocoder {
        /// Encoder rate cap in bps.  Always within
        /// `[NEURAL_VOCODER_LO_BPS, NEURAL_VOCODER_HI_BPS]`.
        target_bps: u32,
    },
    /// Opus SILK / hybrid codec.
    ///
    /// Used at all tiers except Survival-with-NPU.  The governor selects the
    /// Opus mode (SILK-WB at Survival fallback; SILK/hybrid at Constrained;
    /// hybrid SWB at Comfortable; CELT FB at Full) independently; this variant
    /// signals only that the neural vocoder is not active.
    OpusSilk,
}

/// Select the audio gear for the current session tier and NPU capability.
///
/// Activates the neural vocoder only at [`TierState::Survival`] with a
/// confirmed [`NpuCapability::Present`]; all other combinations retain Opus.
pub fn audio_gear_from_tier_and_npu(tier: TierState, npu: NpuCapability) -> AudioGear {
    if tier == TierState::Survival && npu == NpuCapability::Present {
        AudioGear::NeuralVocoder { target_bps: NEURAL_VOCODER_HI_BPS }
    } else {
        AudioGear::OpusSilk
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neural_vocoder_at_survival_with_npu() {
        let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Present);
        assert_eq!(
            gear,
            AudioGear::NeuralVocoder { target_bps: NEURAL_VOCODER_HI_BPS },
            "NeuralVocoder must be selected at Survival tier with NPU present"
        );
    }

    #[test]
    fn opus_silk_at_survival_without_npu() {
        let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Absent);
        assert_eq!(
            gear,
            AudioGear::OpusSilk,
            "OpusSilk fallback must be used at Survival tier when NPU is absent"
        );
    }

    #[test]
    fn opus_silk_at_constrained_with_npu() {
        let gear = audio_gear_from_tier_and_npu(TierState::Constrained, NpuCapability::Present);
        assert_eq!(
            gear, AudioGear::OpusSilk,
            "neural vocoder must not activate above Survival tier (Constrained)"
        );
    }

    #[test]
    fn opus_silk_at_comfortable_with_npu() {
        let gear = audio_gear_from_tier_and_npu(TierState::Comfortable, NpuCapability::Present);
        assert_eq!(
            gear, AudioGear::OpusSilk,
            "neural vocoder must not activate above Survival tier (Comfortable)"
        );
    }

    #[test]
    fn opus_silk_at_full_with_npu() {
        let gear = audio_gear_from_tier_and_npu(TierState::Full, NpuCapability::Present);
        assert_eq!(
            gear, AudioGear::OpusSilk,
            "neural vocoder must not activate above Survival tier (Full)"
        );
    }

    #[test]
    fn neural_vocoder_target_bps_within_bounds() {
        let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Present);
        if let AudioGear::NeuralVocoder { target_bps } = gear {
            assert!(
                target_bps >= NEURAL_VOCODER_LO_BPS && target_bps <= NEURAL_VOCODER_HI_BPS,
                "target_bps {target_bps} must be within [{NEURAL_VOCODER_LO_BPS}, {NEURAL_VOCODER_HI_BPS}]"
            );
        } else {
            panic!("expected NeuralVocoder gear at Survival+NPU");
        }
    }

    #[test]
    fn neural_vocoder_only_at_survival_tier() {
        // The vocoder must activate exactly at Survival and nowhere else,
        // regardless of NPU state.
        let tiers = [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ];
        for tier in tiers {
            let with_npu = audio_gear_from_tier_and_npu(tier, NpuCapability::Present);
            let without_npu = audio_gear_from_tier_and_npu(tier, NpuCapability::Absent);

            if tier == TierState::Survival {
                assert!(
                    matches!(with_npu, AudioGear::NeuralVocoder { .. }),
                    "NeuralVocoder must be selected at Survival+NPU, got {with_npu:?}"
                );
                assert_eq!(
                    without_npu, AudioGear::OpusSilk,
                    "OpusSilk must be selected at Survival without NPU"
                );
            } else {
                assert_eq!(
                    with_npu, AudioGear::OpusSilk,
                    "OpusSilk must be selected at {tier:?} even with NPU"
                );
                assert_eq!(
                    without_npu, AudioGear::OpusSilk,
                    "OpusSilk must be selected at {tier:?} without NPU"
                );
            }
        }
    }

    #[test]
    fn bitrate_constants_match_architecture_spec() {
        // Architecture §8.1: Survival (NPU/CPU-ok) — 3.2–6 kbps.
        assert_eq!(NEURAL_VOCODER_LO_BPS, 3_200, "lower bound must be 3.2 kbps per §8.1");
        assert_eq!(NEURAL_VOCODER_HI_BPS, 6_000, "upper bound must be 6 kbps per §8.1");
        assert!(
            NEURAL_VOCODER_LO_BPS < NEURAL_VOCODER_HI_BPS,
            "rate range must be non-empty"
        );
    }
}

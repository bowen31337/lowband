//! Opus 1.5 encoder mode and bitrate selection — Feature 48.
//!
//! Opus exposes three codec cores that cover the quality/bitrate ladder:
//!
//! | Mode | Bandwidth | Bitrate range | Best for |
//! |------|-----------|---------------|----------|
//! | SILK-WB | Wideband (0–8 kHz) | 6–20 kbps | Low-rate speech |
//! | SILK/hybrid WB | Wideband | 10–20 kbps | Speech with headroom |
//! | Hybrid SWB | Superwideband (0–12 kHz) | 16–32 kbps | Warm speech |
//! | CELT FB | Fullband (0–20 kHz) | 24–512 kbps | High fidelity |
//!
//! The governor selects an [`OpusTierSettings`] once per tier transition and
//! passes `mode` and `target_bps` to the Opus encoder via
//! `OPUS_SET_APPLICATION(OPUS_APPLICATION_VOIP)` + `OPUS_SET_BITRATE` +
//! `OPUS_SET_FORCE_MODE` (libopus private API, available since 1.1).  Frame
//! duration is handled independently by [`crate::opus_packetizer`].
//!
//! # Per-tier mapping
//!
//! | Tier | Mode | Target |
//! |------|------|--------|
//! | Survival (Opus fallback) | [`OpusMode::SilkWb`] | [`SURVIVAL_FALLBACK_AUDIO_BPS`] |
//! | **Constrained** | **[`OpusMode::SilkHybridWb`]** | **[`CONSTRAINED_AUDIO_BPS`]** |
//! | Comfortable | [`OpusMode::HybridSwb`] | [`COMFORTABLE_AUDIO_BPS`] |
//! | Full | [`OpusMode::CeltFb`] | [`FULL_AUDIO_BPS`] |
//!
//! Survival tier uses SILK-WB (not hybrid) because the 60 ms framing and
//! 9 kbps budget sit below the bitrate floor where the CELT layer of hybrid
//! mode contributes meaningful quality.  Hybrid mode becomes beneficial at
//! the Constrained tier (16 kbps), where it bridges SILK voice coding with a
//! thin CELT enhancement layer that improves the perceptual floor for sibilants
//! and fricatives without raising the bitrate.
//!
//! At Comfortable and Full tiers the link can sustain superwideband and
//! fullband content, so the mode steps up to `HybridSwb` and `CeltFb`
//! respectively.  Neural Vocoder at Survival is handled by
//! [`crate::neural_vocoder`] and is excluded from this module's scope.

use crate::tier::TierState;

// ── Bitrate constants ─────────────────────────────────────────────────────────

/// Target Opus bitrate at Survival-fallback tier (bps).
///
/// Used when no NPU is available and the neural vocoder cannot be activated.
/// At 9 kbps Opus SILK-WB produces intelligible speech at 60 ms framing;
/// quality degrades audibly below this floor.
pub const SURVIVAL_FALLBACK_AUDIO_BPS: u32 = 9_000;

/// Target Opus bitrate at Constrained tier (bps).
///
/// Architecture §7: "Constrained | Opus SILK/hybrid WB | 16 kbps".
/// 16 kbps provides comfortable wideband voice quality while leaving the
/// bulk of the 128–200 kbps tier budget for screen and control streams.
pub const CONSTRAINED_AUDIO_BPS: u32 = 16_000;

/// Target Opus bitrate at Comfortable tier (bps).
///
/// Hybrid SWB at 24 kbps opens the superwideband (0–12 kHz) path for
/// noticeably warmer voice reproduction on links that can sustain 300–500 kbps
/// total.
pub const COMFORTABLE_AUDIO_BPS: u32 = 24_000;

/// Target Opus bitrate at Full tier (bps).
///
/// CELT fullband at 32 kbps delivers near-transparent voice.  The encoder
/// may be raised up to 48 kbps for stereo content; this constant captures
/// the default mono target used for voice calls.
pub const FULL_AUDIO_BPS: u32 = 32_000;

// ── OpusMode ──────────────────────────────────────────────────────────────────

/// Opus codec core to apply at a given session tier.
///
/// Each variant corresponds to a distinct libopus forced mode
/// (`OPUS_SET_FORCE_MODE`).  The ordering reflects the quality/bitrate
/// ladder: `SilkWb < SilkHybridWb < HybridSwb < CeltFb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpusMode {
    /// Pure SILK wideband mode (0–8 kHz).
    ///
    /// Activated at Survival-fallback tier (Opus, no NPU).  The SILK encoder
    /// is optimal at bitrates below ≈ 12 kbps; forcing CELT components at
    /// these rates wastes bits on the high-frequency enhancement layer.
    SilkWb,

    /// SILK core with a thin CELT enhancement layer — wideband (0–8 kHz).
    ///
    /// Activated at **Constrained** tier (Feature 48).  At 16 kbps the CELT
    /// overlay improves perceptual quality for sibilants and fricatives
    /// (`/s/`, `/sh/`, `/f/`) that the SILK predictor renders dull.  The
    /// bandwidth stays wideband to keep the encoder complexity within the
    /// 35% CPU ceiling imposed at this tier (Feature 160).
    SilkHybridWb,

    /// Hybrid SILK + CELT with superwideband extension (0–12 kHz).
    ///
    /// Activated at Comfortable tier.  The superwideband extension adds
    /// 8–12 kHz presence that makes speech sound natural on headsets.
    HybridSwb,

    /// Pure CELT fullband mode (0–20 kHz).
    ///
    /// Activated at Full tier.  CELT delivers the highest fidelity and is
    /// the only mode suitable for music or wideband sound effects.  At ≥ 32
    /// kbps it is transparent for voice content.
    CeltFb,
}

// ── OpusTierSettings ─────────────────────────────────────────────────────────

/// Complete Opus encoder configuration for one session tier.
///
/// The governor applies these settings on every tier transition:
/// ```text
/// OPUS_SET_APPLICATION(OPUS_APPLICATION_VOIP)
/// OPUS_SET_BITRATE(target_bps)
/// OPUS_SET_FORCE_MODE(mode as i32)   // libopus private constant
/// ```
/// Frame duration comes from [`crate::opus_packetizer::frame_duration_ms_from_tier`]
/// and is applied separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusTierSettings {
    /// Opus codec core to force for this tier.
    pub mode: OpusMode,
    /// Encoder bitrate target in bits per second.
    pub target_bps: u32,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Return the Opus encoder settings for the Constrained tier.
///
/// This is the canonical entry point for Feature 48.  The Constrained tier
/// uses SILK hybrid wideband at [`CONSTRAINED_AUDIO_BPS`] (16 kbps), which
/// gives comfortable voice quality within the 35% CPU ceiling while keeping
/// the audio budget predictable for the governor's strict-priority allocator.
///
/// # Example
///
/// ```rust
/// use lowband_platform::opus_encoder::{
///     constrained_tier_settings, OpusMode, CONSTRAINED_AUDIO_BPS,
/// };
///
/// let s = constrained_tier_settings();
/// assert_eq!(s.mode, OpusMode::SilkHybridWb);
/// assert_eq!(s.target_bps, CONSTRAINED_AUDIO_BPS);
/// ```
pub fn constrained_tier_settings() -> OpusTierSettings {
    OpusTierSettings { mode: OpusMode::SilkHybridWb, target_bps: CONSTRAINED_AUDIO_BPS }
}

/// Select the Opus encoder settings for the current session tier.
///
/// At Survival tier the function returns SILK-WB settings for the Opus
/// fallback path.  Callers that have already selected the neural vocoder
/// (Feature 54) should not call this function for that tier; the neural
/// vocoder bypasses Opus entirely.
///
/// # Example
///
/// ```rust
/// use lowband_platform::opus_encoder::{opus_settings_from_tier, OpusMode};
/// use lowband_platform::tier::TierState;
///
/// let s = opus_settings_from_tier(TierState::Constrained);
/// assert_eq!(s.mode, OpusMode::SilkHybridWb);
/// ```
pub fn opus_settings_from_tier(tier: TierState) -> OpusTierSettings {
    match tier {
        TierState::Survival => {
            OpusTierSettings { mode: OpusMode::SilkWb, target_bps: SURVIVAL_FALLBACK_AUDIO_BPS }
        }
        TierState::Constrained => constrained_tier_settings(),
        TierState::Comfortable => {
            OpusTierSettings { mode: OpusMode::HybridSwb, target_bps: COMFORTABLE_AUDIO_BPS }
        }
        TierState::Full => {
            OpusTierSettings { mode: OpusMode::CeltFb, target_bps: FULL_AUDIO_BPS }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── constrained_tier_settings ─────────────────────────────────────────────

    #[test]
    fn constrained_settings_use_silk_hybrid_wb() {
        assert_eq!(
            constrained_tier_settings().mode,
            OpusMode::SilkHybridWb,
            "Constrained tier must select SILK/hybrid WB mode (Feature 48)"
        );
    }

    #[test]
    fn constrained_settings_target_16kbps() {
        assert_eq!(
            constrained_tier_settings().target_bps,
            CONSTRAINED_AUDIO_BPS,
            "Constrained tier must target {} bps",
            CONSTRAINED_AUDIO_BPS
        );
    }

    #[test]
    fn constrained_audio_bps_is_16000() {
        assert_eq!(CONSTRAINED_AUDIO_BPS, 16_000, "architecture §7 specifies 16 kbps at Constrained tier");
    }

    // ── opus_settings_from_tier ───────────────────────────────────────────────

    #[test]
    fn survival_tier_selects_silk_wb() {
        let s = opus_settings_from_tier(TierState::Survival);
        assert_eq!(s.mode, OpusMode::SilkWb, "Survival fallback must use pure SILK-WB");
        assert_eq!(
            s.target_bps,
            SURVIVAL_FALLBACK_AUDIO_BPS,
            "Survival fallback must target {SURVIVAL_FALLBACK_AUDIO_BPS} bps"
        );
    }

    #[test]
    fn constrained_tier_selects_silk_hybrid_wb() {
        let s = opus_settings_from_tier(TierState::Constrained);
        assert_eq!(s.mode, OpusMode::SilkHybridWb);
        assert_eq!(s.target_bps, CONSTRAINED_AUDIO_BPS);
    }

    #[test]
    fn comfortable_tier_selects_hybrid_swb() {
        let s = opus_settings_from_tier(TierState::Comfortable);
        assert_eq!(s.mode, OpusMode::HybridSwb, "Comfortable must use hybrid SWB");
        assert_eq!(s.target_bps, COMFORTABLE_AUDIO_BPS);
    }

    #[test]
    fn full_tier_selects_celt_fb() {
        let s = opus_settings_from_tier(TierState::Full);
        assert_eq!(s.mode, OpusMode::CeltFb, "Full tier must use CELT FB");
        assert_eq!(s.target_bps, FULL_AUDIO_BPS);
    }

    #[test]
    fn mode_ordering_follows_quality_ladder() {
        assert!(OpusMode::SilkWb < OpusMode::SilkHybridWb);
        assert!(OpusMode::SilkHybridWb < OpusMode::HybridSwb);
        assert!(OpusMode::HybridSwb < OpusMode::CeltFb);
    }

    #[test]
    fn bitrate_increases_with_tier() {
        assert!(SURVIVAL_FALLBACK_AUDIO_BPS < CONSTRAINED_AUDIO_BPS);
        assert!(CONSTRAINED_AUDIO_BPS < COMFORTABLE_AUDIO_BPS);
        assert!(COMFORTABLE_AUDIO_BPS < FULL_AUDIO_BPS);
    }

    #[test]
    fn opus_settings_from_tier_and_constrained_tier_settings_agree() {
        assert_eq!(
            opus_settings_from_tier(TierState::Constrained),
            constrained_tier_settings(),
            "constrained_tier_settings() must match opus_settings_from_tier(Constrained)"
        );
    }

    #[test]
    fn all_tiers_produce_valid_bitrates() {
        let tiers = [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ];
        for tier in tiers {
            let s = opus_settings_from_tier(tier);
            assert!(s.target_bps >= 6_000, "{tier:?}: target_bps must be ≥ audio floor (6 kbps)");
            assert!(s.target_bps <= 512_000, "{tier:?}: target_bps must be a sane Opus value");
        }
    }

    #[test]
    fn constrained_mode_is_not_pure_silk() {
        // Constrained uses the hybrid extension, not bare SILK.
        assert_ne!(
            constrained_tier_settings().mode,
            OpusMode::SilkWb,
            "Constrained must use hybrid mode, not bare SILK-WB"
        );
    }

    #[test]
    fn constrained_mode_is_not_celt() {
        // At Constrained tier CELT-only would be overkill and exceeds CPU budget.
        assert_ne!(
            constrained_tier_settings().mode,
            OpusMode::CeltFb,
            "Constrained must not use full CELT — too CPU-heavy for the 35% ceiling"
        );
    }
}

//! LowBand platform integration (`lowband-platform`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 128 | Denoise pre-filter: cleans sensor noise with denoise_prefilter before encode |
//! | 155 | Privilege escalation: per-platform elevation documented, explicit, never silent |
//! | 160 | CPU ceiling: caps a constrained-tier session to 35% on a 2015-class laptop |
//! | 161 | Thermal gear degradation: degrades encoder gears with thermal_pressure, never drops voice |
//! | 162 | CPU telemetry: cpu_telemetry drives SVT-AV1 Gear B preset selection (10–12) |
//!
//! ## Feature 160 — CPU ceiling
//!
//! The primary export is [`CpuCeiling`], a cooperative throttle that
//! work-loop owners call each tick.  When the session is in
//! [`TierState::Constrained`] and measured CPU exceeds
//! [`CONSTRAINED_CPU_CEILING_PCT`] (35%), [`CpuCeiling::throttle`] returns a
//! sleep duration that, when obeyed, keeps the rolling average at or below the
//! limit.
//!
//! ## Feature 161 — Thermal gear degradation
//!
//! The governor reads [`ThermalPressure`] at each 10 Hz tick via
//! [`ThermalMonitor::sample`] and calls [`GearConstraints::from_thermal`] to
//! obtain encoder constraints for the current thermal state.  The constraints
//! are then passed to [`allocate`] to produce per-stream bitrate budgets.
//!
//! The invariant enforced here is absolute: the voice stream receives at least
//! [`gear_policy::AUDIO_FLOOR_BPS`] (6 kbps) at every thermal level and at
//! every network bandwidth.

pub mod cpu_ceiling;
pub mod cpu_telemetry;
pub mod denoise_prefilter;
pub mod elevation;
pub mod fallback_detector;
pub mod gear_policy;
pub mod input_injection;
pub mod intra_refresh;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod keypoint_extractor;
pub mod screen_capture;
pub mod synthesis_network;
pub mod temporal_svc;
pub mod thermal;
pub mod tier;
pub mod uac;

pub use cpu_ceiling::{CpuCeiling, ThrottleAction};
pub use cpu_telemetry::CpuTelemetry;
pub use denoise_prefilter::DenoisePrefilter;
pub use elevation::{ElevationKind, ElevationOutcome, ElevationRequest, EscalationReason};
pub use fallback_detector::{
    FallbackDetector, FrameAnalysis, GuardrailDetector, GuardrailTrip,
    FORCE_GEAR_B_DEADLINE_MS, KEYPOINT_CONFIDENCE_THRESHOLD, NON_FACE_PIXEL_RATIO_THRESHOLD,
};
#[cfg(target_os = "windows")]
pub use elevation::WinElevationBridge;
pub use input_injection::{InputBroker, InputEvent, InjectionError, MouseButton};
pub use intra_refresh::{IntraRefreshFrame, IntraRefreshState};
pub use temporal_svc::{
    TemporalLayerAssigner, TemporalLayerId, TemporalSvcController, TemporalSvcMode,
    OVERUSE_ESCALATE_TICKS, UNDERUSE_RELAX_TICKS, T0, T1, T2,
};
pub use screen_capture::{CaptureError, CaptureFrame, DirtyRect, ScreenCaptureBroker};
pub use gear_policy::{
    allocate, gear_b_preset_from_cpu_pct, select_resolution, Av1EncodeCapability, CameraGear,
    DisplayResolution, GearConstraints, StreamBudgets, AUDIO_FLOOR_BPS,
    CPU_PRESET10_THRESHOLD_PCT, CPU_PRESET11_THRESHOLD_PCT, RESOLUTION_LADDER, SCREEN_UPGRADE_BPS,
};
pub use keypoint_extractor::{
    CameraFrame, ExtractionError, ExtractionResult, KeypointExtractor, KeypointExtractorConfig,
    EXPRESSION_DIM,
};
pub use synthesis_network::{
    ExpressionLatents, HeadPose, HeadResolution, Keypoint3D, MotionLatents,
    ReconstructedFrame, ReferenceFrame, SynthesisConfig, SynthesisError, SynthesisNetwork,
    HEAD_PX_MAX, HEAD_PX_MIN, KEYPOINT_COUNT,
};
pub use thermal::{ThermalMonitor, ThermalPressure};
pub use tier::TierState;

/// CPU ceiling percentage applied at the Constrained tier (Feature 160).
///
/// Baseline: 2015-class dual-core laptop (e.g. Core i5-5200U, 2 cores /
/// 4 threads, ~2.7 GHz boost).  35% total-CPU headroom is sufficient for
/// voice + screen + input while leaving the machine usable for other tasks.
pub const CONSTRAINED_CPU_CEILING_PCT: f64 = 35.0;

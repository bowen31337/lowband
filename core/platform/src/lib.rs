//! LowBand platform integration (`lowband-platform`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 44  | Mic capture: captures microphone audio at 48 kHz with sample_rate feeding the encode pipeline |
//! | 46  | Noise suppressor: RNNoise-class neural filtering with noise_suppression at ~0.1% CPU |
//! | 47  | AGC: applies automatic gain control ahead of detection with voice_activity gating |
//! | 51  | LBRR FEC: recovers isolated audio losses in-band with lbrr_fec redundancy |
//! | 52  | DRED sender: sends Opus 1.5 DRED neural redundancy to reconstruct multi-hundred-millisecond loss_bursts |
//! | 54  | DTX: silence costs near-zero bitrate with comfort_noise updates |
//! | 56  | Jitter buffer: converges playout under 15% with time_scaling instead of gaps |
//! | 57  | PLC chain: orders concealment — FEC decode, DRED, neural PLC, faded comfort noise |
//! | 58  | Opus LACE: enables LACE decoder enhancement when cpu_headroom is available |
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

pub mod agc;
pub mod cpu_ceiling;
pub mod cursor_sender;
pub mod mic_capture;
pub mod cpu_telemetry;
pub mod denoise_prefilter;
pub mod dred_sender;
pub mod dtx;
pub mod lbrr_fec;
pub mod elevation;
pub mod opus_encoder;
pub mod jitter_buffer;
pub mod opus_decoder;
pub mod plc_chain;
pub mod fallback_detector;
pub mod gear_policy;
pub mod input_channel_sender;
pub mod input_injection;
pub mod input_latency_budget;
pub mod intra_refresh;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod keypoint_extractor;
pub mod motion_encoder;
pub mod neural_vocoder;
pub mod noise_suppressor;
pub mod opus_packetizer;
pub mod reference_frame_sender;
pub mod screen_capture;
pub mod screen_encoder;
pub mod stream_drop_policy;
pub mod synthesis_network;
pub mod temporal_svc;
pub mod thermal;
pub mod tier;
pub mod uac;

pub use agc::{
    AgcProcessor, AgcStats,
    AGC_ENVELOPE_ATTACK, AGC_ENVELOPE_FLOOR, AGC_ENVELOPE_RELEASE,
    AGC_GAIN_DECREASE_COEFF, AGC_GAIN_INCREASE_COEFF,
    AGC_MAX_GAIN, AGC_MIN_GAIN, AGC_TARGET_RMS,
};
pub use cpu_ceiling::{CpuCeiling, ThrottleAction};
pub use cursor_sender::{
    CursorPositionSampler, CURSOR_CHANNEL_HZ, CURSOR_DELTA_BYTES, CURSOR_TICK_NS,
    decode_delta as decode_cursor_delta, encode_delta as encode_cursor_delta,
};
pub use cpu_telemetry::CpuTelemetry;
pub use denoise_prefilter::DenoisePrefilter;
pub use dred_sender::{
    dred_depth_from_burst_ms, dred_depth_from_ge_burst_packets, DredSender,
    DRED_BITS_PER_FRAME, DRED_FRAME_DURATION_MS,
    DRED_OVERHEAD_BPS_PER_FRAME, MAX_DRED_DEPTH_FRAMES, MIN_DRED_DEPTH_FRAMES,
};
pub use dtx::{
    DtxAction, DtxEncoder, DtxReceiver, DtxState,
    DTX_HANGOVER_FRAMES, DTX_SID_BYTES, DTX_SID_INTERVAL_FRAMES, DTX_SILENCE_BPS,
};
pub use elevation::{ElevationKind, ElevationOutcome, ElevationRequest, EscalationReason};
pub use fallback_detector::{
    FallbackDetector, FrameAnalysis, GuardrailDetector, GuardrailTrip,
    FORCE_GEAR_B_DEADLINE_MS, KEYPOINT_CONFIDENCE_THRESHOLD, NON_FACE_PIXEL_RATIO_THRESHOLD,
};
#[cfg(target_os = "windows")]
pub use elevation::WinElevationBridge;
pub use input_channel_sender::{
    decode_varint_i32 as input_decode_varint,
    encode_varint_i32 as input_encode_varint,
    InputChannelDecoder, InputChannelSender, MouseMoveCoalescer,
    INPUT_CHANNEL_ID, MAX_INPUT_FRAME_BYTES, MOUSE_COALESCE_TICK_NS, SCHEDULING_PRIORITY_RANK,
};
pub use input_injection::{InputBroker, InputEvent, InjectionError, MouseButton};
pub use input_latency_budget::{
    queuing_delay_ms as input_queuing_delay_ms,
    total_overhead_ms as input_to_photon_overhead_ms,
    within_sla as input_to_photon_within_sla,
    INPUT_PACER_TICK_MS, INPUT_TO_PHOTON_FIXED_OVERHEAD_MS,
    INPUT_TO_PHOTON_SLA_MS, MAX_BACKLOG_WITHIN_SLA,
};
pub use intra_refresh::{IntraRefreshFrame, IntraRefreshState};
pub use temporal_svc::{
    TemporalLayerAssigner, TemporalLayerId, TemporalSvcController, TemporalSvcMode,
    OVERUSE_ESCALATE_TICKS, UNDERUSE_RELAX_TICKS, T0, T1, T2,
};
pub use mic_capture::{
    MicCaptureBroker, MicCaptureError, MicFrame,
    MIC_CHANNELS, MIC_FRAME_MS, MIC_FRAME_SAMPLES, MIC_SAMPLE_RATE,
};
pub use screen_capture::{CaptureError, CaptureFrame, CursorShape, DirtyRect, ScreenCaptureBroker};
pub use screen_encoder::{
    classify_tile, RefinementQueue, TileClass, TileCoord, TileGrid, TileRect, VideoSubStream,
    EntropyPaletteDecoder, EntropyPaletteEncoder,
    PaletteDecodeError, PaletteEncodeError, PaletteTileDecoder, PaletteTileEncoder,
    LOSSLESS_BYTES_PER_PICTURE_TILE, PALETTE_COLOR_LIMIT, PIXEL_EXACT_DEADLINE_MS,
    TILE_BYTES, TILE_SIZE_PX, VIDEO_COLOR_LIMIT,
    BlitCommand, BlitResult, ScrollDetector,
    SCROLL_CONFIDENCE_THRESHOLD, SCROLL_MAX_SMALL_SHIFT, SCROLL_MIN_REGION_PX,
    merge_damage_rects, DAMAGE_MERGE_RATIO,
    IdleSuppressor, ScreenIdleAction, SCREEN_HEARTBEAT_NS,
    TileDiffDetector,
};
pub use gear_policy::{
    allocate, gear_b_preset_from_cpu_pct, select_resolution, Av1EncodeCapability, CameraGear,
    DisplayResolution, GearConstraints, StreamBudgets, AUDIO_FLOOR_BPS,
    CPU_PRESET10_THRESHOLD_PCT, CPU_PRESET11_THRESHOLD_PCT, RESOLUTION_LADDER, SCREEN_UPGRADE_BPS,
};
pub use keypoint_extractor::{
    CameraFrame, ExtractionError, ExtractionResult, KeypointExtractor, KeypointExtractorConfig,
    EXPRESSION_DIM,
};
pub use motion_encoder::{
    MotionCodecError, MotionDecoder, MotionEncoder, MOTION_BITRATE_HI_BPS,
    MOTION_BITRATE_LO_BPS, MOTION_TARGET_FPS,
};
pub use reference_frame_sender::{
    GearAReferenceDecoder, GearAReferenceEncoder, ReferenceCodecError, ReferenceFramePacket,
    HEADER_LEN as REFERENCE_FRAME_HEADER_LEN, TAG_REFERENCE_FRAME,
};
pub use synthesis_network::{
    ExpressionLatents, HeadPose, HeadResolution, Keypoint3D, MotionLatents,
    ReconstructedFrame, ReferenceFrame, SynthesisConfig, SynthesisError, SynthesisNetwork,
    HEAD_PX_MAX, HEAD_PX_MIN, KEYPOINT_COUNT,
};
pub use jitter_buffer::{
    JitterBuffer, PlayoutAction, CONVERGENCE_ZONE_FRAMES, MAX_TIME_SCALE_RATE,
};
pub use lbrr_fec::{
    LbrrDecoder, LbrrEncoder, LBRR_ENABLE_THRESHOLD, LBRR_FRAME_DURATION_MS, LBRR_OVERHEAD_BPS,
};
pub use noise_suppressor::{
    NoiseSuppressor, NsStats,
    NS_ENERGY_FLOOR, NS_FLOOR_ATTACK, NS_FLOOR_MAX_SNR, NS_FLOOR_RELEASE,
    NS_FRAME_MS, NS_FRAME_SAMPLES, NS_SAMPLE_RATE,
    NS_VAD_SMOOTH, NS_VAD_THRESHOLD,
};
pub use neural_vocoder::{
    audio_gear_from_tier_and_npu, AudioGear, NpuCapability,
    NEURAL_VOCODER_HI_BPS, NEURAL_VOCODER_LO_BPS,
};
pub use opus_decoder::{
    lace_mode_from_cpu_pct, LaceMode, LACE_CPU_OVERHEAD_PCT, LACE_HEADROOM_THRESHOLD_PCT,
};
pub use opus_encoder::{
    constrained_tier_settings, opus_settings_from_tier, OpusMode, OpusTierSettings,
    COMFORTABLE_AUDIO_BPS, CONSTRAINED_AUDIO_BPS, FULL_AUDIO_BPS, SURVIVAL_FALLBACK_AUDIO_BPS,
};
pub use opus_packetizer::{
    frame_duration_ms_from_tier, header_overhead_fraction, packets_per_second,
    DEFAULT_FRAME_MS, SURVIVAL_FRAME_MS,
};
pub use plc_chain::{
    PlcChain, PlcOutcome, PlcStage,
    COMFORT_NOISE_FADE_FRAMES, DRED_DEPTH_FRAMES, NEURAL_PLC_MAX_FRAMES,
};
pub use stream_drop_policy::{DropPolicy, StreamDropPolicy, StreamKind};
pub use thermal::{ThermalMonitor, ThermalPressure};
pub use tier::TierState;

/// CPU ceiling percentage applied at the Constrained tier (Feature 160).
///
/// Baseline: 2015-class dual-core laptop (e.g. Core i5-5200U, 2 cores /
/// 4 threads, ~2.7 GHz boost).  35% total-CPU headroom is sufficient for
/// voice + screen + input while leaving the machine usable for other tasks.
pub const CONSTRAINED_CPU_CEILING_PCT: f64 = 35.0;

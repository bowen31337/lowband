//! Observability — Features 133, 134, 135, 136, 137.
//!
//! | Module               | Feature | Description |
//! |----------------------|---------|-------------|
//! | [`quality_bar_lag`]  | 133 | Full quality-bar display-lag tracker: all four fields within 1 s |
//! | [`quality_indicator`]| 134 | Governor-to-display sync tracker: asserts display matches governor within 1 s |
//! | [`vmaf_sample`]      | 135 | VMAF-proxy probe: scores decoded camera frames for QoE |
//! | [`ocr_probe`]        | 136 | OCR-legibility probe: scores decoded screen frames |
//! | [`telemetry`]        | 137 | Opt-in aggregate-only QoS telemetry (no media content) |
//! | [`sender`]           | 137 | HTTP/1.1 telemetry batch sender |
//!
//! ## Feature 137 — opt-in telemetry
//!
//! Media content (audio frames, video frames, screen captures, camera images)
//! is never included in any telemetry payload — only numeric statistics
//! derived from session records are transmitted.  Telemetry is **disabled by
//! default**; the operator must call [`QosTelemetryConfig::with_opt_in`]
//! before any network connection is opened.

pub mod ocr_probe;
pub mod quality_bar_lag;
pub mod quality_indicator;
pub mod sender;
pub mod telemetry;
pub mod vmaf_sample;

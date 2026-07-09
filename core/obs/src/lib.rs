//! Observability — Features 136, 137.
//!
//! | Module | Feature | Description |
//! |--------|---------|-------------|
//! | [`ocr_probe`] | 136 | OCR-legibility probe: scores decoded screen frames |
//! | [`telemetry`] | 137 | Opt-in aggregate-only QoS telemetry (no media content) |
//! | [`sender`]    | 137 | HTTP/1.1 telemetry batch sender |
//!
//! ## Feature 137 — opt-in telemetry
//!
//! Media content (audio frames, video frames, screen captures, camera images)
//! is never included in any telemetry payload — only numeric statistics
//! derived from session records are transmitted.  Telemetry is **disabled by
//! default**; the operator must call [`QosTelemetryConfig::with_opt_in`]
//! before any network connection is opened.

pub mod ocr_probe;
pub mod sender;
pub mod telemetry;

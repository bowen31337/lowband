//! Observability — Feature 137.
//!
//! Provides opt-in aggregate-only QoS telemetry.  Media content (audio
//! frames, video frames, screen captures, camera images) is never included
//! in any telemetry payload — only numeric statistics derived from session
//! records are transmitted.
//!
//! Telemetry is **disabled by default**.  The operator must explicitly set
//! [`QosTelemetryConfig::enabled`] to `true` (or call
//! [`QosTelemetryConfig::with_opt_in`]) before any network connection is
//! attempted.

pub mod sender;
pub mod telemetry;

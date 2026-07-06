//! LowBand platform integration (`lowband-platform`).
//!
//! | Feature | Description |
//! |---------|-------------|
//! | 155 | Privilege escalation: per-platform elevation documented, explicit, never silent |
//! | 160 | CPU ceiling: caps a constrained-tier session to 35% on a 2015-class laptop |
//! | 161 | Thermal gear degradation: degrades encoder gears with thermal_pressure, never drops voice |
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
pub mod elevation;
pub mod gear_policy;
pub mod ipc;
pub mod thermal;
pub mod tier;
pub mod uac;

pub use cpu_ceiling::{CpuCeiling, ThrottleAction};
pub use elevation::{ElevationKind, ElevationOutcome, ElevationRequest, EscalationReason};
#[cfg(target_os = "windows")]
pub use elevation::WinElevationBridge;
pub use gear_policy::{
    allocate, CameraGear, GearConstraints, StreamBudgets, AUDIO_FLOOR_BPS,
};
pub use thermal::{ThermalMonitor, ThermalPressure};
pub use tier::TierState;

/// CPU ceiling percentage applied at the Constrained tier (Feature 160).
///
/// Baseline: 2015-class dual-core laptop (e.g. Core i5-5200U, 2 cores /
/// 4 threads, ~2.7 GHz boost).  35% total-CPU headroom is sufficient for
/// voice + screen + input while leaving the machine usable for other tasks.
pub const CONSTRAINED_CPU_CEILING_PCT: f64 = 35.0;

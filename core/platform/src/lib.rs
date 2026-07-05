//! LowBand platform integration crate.
//!
//! Implements Feature 161 of the LowBand architecture spec:
//!
//! > System degrades gears gracefully with thermal_pressure and never drops voice.
//!
//! # Overview
//!
//! The governor reads [`ThermalPressure`] at each 10 Hz tick via
//! [`ThermalMonitor::sample`] and calls [`GearConstraints::from_thermal`] to
//! obtain encoder constraints for the current thermal state.  The constraints
//! are then passed to [`allocate`] to produce per-stream bitrate budgets.
//!
//! The invariant enforced here is absolute: the voice stream receives at least
//! [`gear_policy::AUDIO_FLOOR_BPS`] (6 kbps) at every thermal level and at
//! every network bandwidth.  Camera gears are shed first; the neural Gear A
//! is the first to go, followed by SVT-AV1 at progressively faster (lower
//! quality, lower CPU) presets, and finally camera is disabled entirely at
//! the Critical level.  Screen refinement passes are suspended under Serious
//! or Critical pressure but the coarse lane continues so text remains legible.

pub mod gear_policy;
pub mod thermal;

pub use gear_policy::{
    allocate, CameraGear, GearConstraints, StreamBudgets, AUDIO_FLOOR_BPS,
};
pub use thermal::{ThermalMonitor, ThermalPressure};

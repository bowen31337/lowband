//! Neural head-gear availability gate — Feature 82.
//!
//! # Rule
//!
//! The neural talking-head codec (Gear A) runs the keypoint extractor and
//! synthesis network on every camera frame.  On devices without a hardware
//! neural accelerator this inference falls back to the CPU execution provider,
//! which is acceptable only when there is *spare* CPU headroom — i.e. the
//! process is not already near the constrained-tier ceiling.
//!
//! | NPU present | CPU usage       | Head gear |
//! |-------------|-----------------|-----------|
//! | Yes         | any             | Available |
//! | No          | < 50 %          | Available (CPU execution path) |
//! | No          | ≥ 50 %          | Rejected  |
//!
//! When rejected, the governor falls back to Gear B (SVT-AV1) or lower.
//!
//! # Threshold rationale
//!
//! [`CPU_HEADROOM_THRESHOLD_PCT`] is 50 %.  On a 2015-class dual-core (4
//! logical threads) a process at 50 % already consumes half the machine's
//! total CPU capacity.  Adding the synthesis-network and keypoint-extractor
//! workloads on CPU would push it past the 35 % constrained-tier ceiling
//! enforced by [`lowband_platform::CpuCeiling`].  Below 50 % there is
//! enough headroom that the neural inference fits within the budget without
//! causing throttle sleeps or audio glitches.

/// CPU usage percentage below which the neural head gear may run on the CPU
/// execution provider without exceeding the constrained-tier CPU ceiling.
///
/// At or above this threshold, CPU inference for the keypoint extractor and
/// synthesis network would compete with audio, screen, and input encoding and
/// push the process beyond the 35 % ceiling that the governor enforces at
/// Constrained and Survival tiers.
pub const CPU_HEADROOM_THRESHOLD_PCT: f64 = 50.0;

/// Whether the neural talking-head gear (Gear A) may be activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadGearCapability {
    /// Head gear may be activated: an NPU is present, or spare CPU is available.
    Available,
    /// Head gear is rejected: no NPU detected and CPU usage is at or above the
    /// spare-CPU threshold.  The governor must select Gear B or lower.
    Rejected,
}

/// Decide whether the neural talking-head gear (Gear A) is available.
///
/// Returns [`HeadGearCapability::Available`] when **either**:
/// - `has_npu` is `true` (a hardware neural accelerator is confirmed by
///   [`crate::capability_probe::probe`]), **or**
/// - `cpu_usage_pct < CPU_HEADROOM_THRESHOLD_PCT` (sufficient spare CPU for
///   the CPU execution-provider fallback path).
///
/// Returns [`HeadGearCapability::Rejected`] when no NPU is present **and**
/// `cpu_usage_pct >= CPU_HEADROOM_THRESHOLD_PCT`.
///
/// # Arguments
///
/// * `has_npu` — `true` when
///   [`crate::capability_probe::CapabilityProbeResult::has_neural_accelerator`]
///   is `true` (CoreML, NNAPI, or DirectML execution provider confirmed).
/// * `cpu_usage_pct` — process CPU usage as a percentage in `[0.0, 100.0]`,
///   sampled by the governor's telemetry each 10 Hz tick.
pub fn head_gear_available(has_npu: bool, cpu_usage_pct: f64) -> HeadGearCapability {
    if has_npu || cpu_usage_pct < CPU_HEADROOM_THRESHOLD_PCT {
        HeadGearCapability::Available
    } else {
        HeadGearCapability::Rejected
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npu_present_always_available_regardless_of_cpu() {
        for cpu_pct in [0.0_f64, 25.0, 49.9, 50.0, 75.0, 100.0] {
            assert_eq!(
                head_gear_available(true, cpu_pct),
                HeadGearCapability::Available,
                "NPU present must make head gear Available at any CPU load (cpu={cpu_pct}%)"
            );
        }
    }

    #[test]
    fn no_npu_spare_cpu_is_available() {
        assert_eq!(
            head_gear_available(false, 0.0),
            HeadGearCapability::Available,
            "no NPU + 0% CPU must be Available"
        );
        assert_eq!(
            head_gear_available(false, 49.9),
            HeadGearCapability::Available,
            "no NPU + 49.9% CPU must be Available (below 50% threshold)"
        );
    }

    #[test]
    fn no_npu_at_threshold_is_rejected() {
        assert_eq!(
            head_gear_available(false, CPU_HEADROOM_THRESHOLD_PCT),
            HeadGearCapability::Rejected,
            "no NPU + exactly {CPU_HEADROOM_THRESHOLD_PCT}% CPU must be Rejected"
        );
    }

    #[test]
    fn no_npu_above_threshold_is_rejected() {
        for cpu_pct in [50.0_f64, 60.0, 75.0, 90.0, 100.0] {
            assert_eq!(
                head_gear_available(false, cpu_pct),
                HeadGearCapability::Rejected,
                "no NPU + {cpu_pct}% CPU must be Rejected (at or above 50% threshold)"
            );
        }
    }

    #[test]
    fn threshold_constant_is_50_pct() {
        assert_eq!(
            CPU_HEADROOM_THRESHOLD_PCT, 50.0,
            "threshold must be 50%: above this the synthesis network pushes \
             a 2015-class dual-core past the constrained-tier CPU ceiling"
        );
    }

    #[test]
    fn boundary_strictly_below_threshold_is_available() {
        // Use a value that is meaningfully representable as less than 50.0 in f64.
        // f64::EPSILON (~2e-16) is smaller than the ulp of 50.0 (~7e-15) and
        // would round back to 50.0; use 49.999 instead.
        let just_below = 49.999_f64;
        assert!(
            just_below < CPU_HEADROOM_THRESHOLD_PCT,
            "precondition: {just_below} must be below {CPU_HEADROOM_THRESHOLD_PCT}"
        );
        assert_eq!(
            head_gear_available(false, just_below),
            HeadGearCapability::Available,
            "CPU usage strictly below threshold must be Available"
        );
    }

    #[test]
    fn capability_variants_are_distinct() {
        assert_ne!(HeadGearCapability::Available, HeadGearCapability::Rejected);
    }

    #[test]
    fn probe_is_pure_function_same_inputs_same_output() {
        // head_gear_available must be deterministic: same inputs → same output.
        let r1 = head_gear_available(false, 60.0);
        let r2 = head_gear_available(false, 60.0);
        assert_eq!(r1, r2);

        let r3 = head_gear_available(true, 80.0);
        let r4 = head_gear_available(true, 80.0);
        assert_eq!(r3, r4);
    }
}

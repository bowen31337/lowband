//! Opus decoder LACE enhancement gating — Feature 58.
//!
//! LACE (Learning-based Audio Codec Enhancement) is an optional neural
//! post-filter introduced in Opus 1.5 that improves decoded audio quality
//! at the cost of additional CPU.  The system enables it only when the
//! process has sufficient CPU headroom below the constrained-tier ceiling.
//!
//! # Decision rule
//!
//! At each governor tick the call site reads the current `cpu_usage_pct`
//! from [`CpuTelemetry`](crate::CpuTelemetry) and passes it to
//! [`lace_mode_from_cpu_pct`]:
//!
//! | CPU usage                                 | LACE mode |
//! |-------------------------------------------|-----------|
//! | < [`LACE_HEADROOM_THRESHOLD_PCT`] (27 %)  | Enabled   |
//! | ≥ 27 %                                    | Disabled  |
//!
//! # Threshold derivation
//!
//! The constrained-tier CPU ceiling is 35 % (see
//! [`crate::CONSTRAINED_CPU_CEILING_PCT`]).  LACE adds approximately
//! [`LACE_CPU_OVERHEAD_PCT`] (8 %) to process CPU load.  Enabling LACE
//! when `cpu_pct + overhead < ceiling` gives a safe threshold of
//! 35 − 8 = 27 %.  If the current load is already at 27 %, adding LACE
//! would push total usage to ≈ 35 % — exactly at the ceiling with no
//! margin.  Callers may apply additional hysteresis if needed, but the
//! policy itself uses this hard threshold.

/// Estimated process CPU overhead of Opus LACE decoder enhancement (%).
///
/// Derived from Opus 1.5 benchmarks: the LACE neural post-filter adds
/// roughly 8 percentage points of single-process CPU load on a 2015-class
/// dual-core at 48 kHz stereo decoding.
pub const LACE_CPU_OVERHEAD_PCT: f64 = 8.0;

/// CPU usage (%) at or above which LACE is disabled.
///
/// Set to `CONSTRAINED_CPU_CEILING_PCT` (35 %) minus
/// `LACE_CPU_OVERHEAD_PCT` (8 %) = 27 %.  Below this threshold the
/// process has headroom to absorb LACE's overhead without breaching the
/// constrained-tier ceiling.
pub const LACE_HEADROOM_THRESHOLD_PCT: f64 = 27.0;

/// Whether the Opus LACE decoder enhancement is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaceMode {
    /// LACE neural post-filter is active; decoded audio benefits from
    /// perceptual quality enhancement.
    Enabled,
    /// LACE is inactive to conserve CPU; the decoder runs the standard
    /// signal path.
    Disabled,
}

/// Select the LACE mode for the given process CPU usage.
///
/// Returns [`LaceMode::Enabled`] when `cpu_usage_pct` is below
/// [`LACE_HEADROOM_THRESHOLD_PCT`], giving the system headroom to absorb
/// the LACE overhead without breaching the constrained-tier CPU ceiling.
/// Returns [`LaceMode::Disabled`] at or above the threshold.
pub fn lace_mode_from_cpu_pct(cpu_usage_pct: f64) -> LaceMode {
    if cpu_usage_pct < LACE_HEADROOM_THRESHOLD_PCT {
        LaceMode::Enabled
    } else {
        LaceMode::Disabled
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CONSTRAINED_CPU_CEILING_PCT;

    #[test]
    fn lace_enabled_at_zero_cpu() {
        assert_eq!(lace_mode_from_cpu_pct(0.0), LaceMode::Enabled);
    }

    #[test]
    fn lace_enabled_just_below_threshold() {
        let just_below = LACE_HEADROOM_THRESHOLD_PCT - 0.001;
        assert_eq!(
            lace_mode_from_cpu_pct(just_below),
            LaceMode::Enabled,
            "LACE must be enabled at {just_below:.3}% (just below {LACE_HEADROOM_THRESHOLD_PCT}%)"
        );
    }

    #[test]
    fn lace_disabled_at_threshold() {
        assert_eq!(
            lace_mode_from_cpu_pct(LACE_HEADROOM_THRESHOLD_PCT),
            LaceMode::Disabled,
            "LACE must be disabled at exactly {LACE_HEADROOM_THRESHOLD_PCT}%"
        );
    }

    #[test]
    fn lace_disabled_above_threshold() {
        for pct in [28.0_f64, 35.0, 50.0, 75.0, 100.0] {
            assert_eq!(
                lace_mode_from_cpu_pct(pct),
                LaceMode::Disabled,
                "LACE must be disabled at {pct}% CPU"
            );
        }
    }

    #[test]
    fn threshold_leaves_headroom_below_ceiling() {
        // threshold + overhead must not exceed the constrained-tier ceiling.
        assert!(
            LACE_HEADROOM_THRESHOLD_PCT + LACE_CPU_OVERHEAD_PCT <= CONSTRAINED_CPU_CEILING_PCT,
            "threshold ({LACE_HEADROOM_THRESHOLD_PCT}%) + overhead ({LACE_CPU_OVERHEAD_PCT}%) \
             must not exceed the CPU ceiling ({CONSTRAINED_CPU_CEILING_PCT}%)"
        );
    }

    #[test]
    fn lace_mode_is_monotone_with_cpu_load() {
        // As CPU load rises, LACE must not re-enable after being disabled.
        let loads = [0.0_f64, 10.0, 20.0, 26.9, 27.0, 27.1, 35.0, 50.0, 100.0];
        let modes: Vec<LaceMode> = loads.iter().map(|&p| lace_mode_from_cpu_pct(p)).collect();

        let mut seen_disabled = false;
        for (i, &mode) in modes.iter().enumerate() {
            if mode == LaceMode::Disabled {
                seen_disabled = true;
            }
            if seen_disabled {
                assert_eq!(
                    mode,
                    LaceMode::Disabled,
                    "LACE must not re-enable after being disabled: at {:.1}% CPU (index {i})",
                    loads[i]
                );
            }
        }
    }
}

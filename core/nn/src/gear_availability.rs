//! Neural gear availability from capability_probe — Feature 79.
//!
//! At startup the neural runtime calls [`capability_probe::probe`] and passes
//! the result to [`GearAvailability::from_probe`] to decide which neural gear
//! slots are viable on this device.  The governor uses this result to build its
//! available-gear list before the first session tick so it never schedules a
//! gear the hardware cannot support.
//!
//! # Three neural gear slots
//!
//! | Slot          | Requires                                | Viable on CPU-only?              |
//! |---------------|-----------------------------------------|----------------------------------|
//! | NeuralVocoder | Hardware accelerator (NPU / GPU)        | No — 50 Hz × 8 ms/inf ≈ 400 % CPU |
//! | TalkingHead   | Any provider; runtime gate (Feature 82) | Yes — ~30 ms/frame at 25 fps     |
//! | NeuralPlc     | Any provider                            | Yes — 2.5 ms/inf at 50 Hz ≈ 12.5 % CPU |
//!
//! # Relationship to Features 82 and 83
//!
//! * **Feature 79** (this module) — startup-time slot existence from the probe.
//!   A slot that does not exist here can never activate.
//! * **Feature 82** (`head_gear_gate`) — runtime gate for TalkingHead based on
//!   current CPU utilisation.
//! * **Feature 83** (`neural_vocoder` in `lowband-platform`) — runtime
//!   activation of NeuralVocoder at Survival tier given NPU state and tier.

use crate::capability_probe::{probe as run_probe, CapabilityProbeResult};

/// Startup-time neural gear availability derived from [`CapabilityProbeResult`].
///
/// Produced by [`GearAvailability::from_probe`] after the startup capability
/// probe runs.  The governor passes this to its gear-selection logic before the
/// first 10 Hz tick; a `false` slot is permanently excluded — the governor will
/// not schedule that gear.  A `true` slot is *viable* on this hardware, but
/// whether it *activates* in a given tick is a separate per-tick decision
/// (Feature 82 for TalkingHead, Feature 83 for NeuralVocoder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GearAvailability {
    /// `true` when the neural vocoder gear slot exists on this device.
    ///
    /// Requires a hardware neural accelerator (CoreML, NNAPI, or DirectML).
    /// At 50 Hz the vocoder inference budget is 20 ms per frame; the CPU
    /// execution provider costs ~8 ms/inference on the 2015-class reference
    /// hardware, consuming ~400 % of one core with no margin for the rest of
    /// the pipeline.  The slot is excluded on CPU-only systems.
    pub neural_vocoder: bool,

    /// `true` when the neural talking-head gear (Gear A) slot exists.
    ///
    /// Always `true`: the keypoint extractor (~12 ms) and synthesis network
    /// (~18 ms) together fit within the 40 ms frame budget at 25 fps on the
    /// CPU execution provider given spare headroom.  The runtime gate
    /// (Feature 82 — `head_gear_gate`) enforces the headroom check; this
    /// startup flag never excludes the slot based on the probe alone.
    pub talking_head: bool,

    /// `true` when the neural PLC gear slot exists.
    ///
    /// Always `true`: inference takes ~2.5 ms at 50 Hz (~12.5 % of one CPU
    /// core), which fits within the constrained-tier CPU ceiling on any device
    /// regardless of execution provider.
    pub neural_plc: bool,
}

impl GearAvailability {
    /// Derive gear availability from a startup [`CapabilityProbeResult`].
    ///
    /// Call this once at startup immediately after [`capability_probe::probe`].
    /// The result is stable for the process lifetime — hardware capabilities
    /// do not change without a restart.
    pub fn from_probe(probe_result: &CapabilityProbeResult) -> Self {
        Self {
            // NeuralVocoder requires real-time hardware throughput; the CPU
            // execution provider cannot sustain 50 Hz inference within the
            // per-frame budget at constrained-tier CPU utilisation.
            neural_vocoder: probe_result.has_neural_accelerator,
            // TalkingHead: always a viable slot — Feature 82 (head_gear_gate)
            // enforces the spare-CPU check at runtime.
            talking_head: true,
            // NeuralPlc: lightweight enough for any execution provider.
            neural_plc: true,
        }
    }

    /// Run the startup capability probe and derive gear availability in one step.
    ///
    /// Equivalent to `GearAvailability::from_probe(&capability_probe::probe())`.
    /// **Non-blocking** — does not load any model or allocate GPU/NPU memory.
    pub fn probe() -> Self {
        Self::from_probe(&run_probe())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_probe::{CapabilityProbeResult, ExecutionProvider};

    fn hardware_result() -> CapabilityProbeResult {
        CapabilityProbeResult {
            provider: ExecutionProvider::CoreMl,
            has_neural_accelerator: true,
        }
    }

    fn cpu_result() -> CapabilityProbeResult {
        CapabilityProbeResult {
            provider: ExecutionProvider::Cpu,
            has_neural_accelerator: false,
        }
    }

    // ── Hardware accelerator present ──────────────────────────────────────────

    #[test]
    fn hardware_accelerator_enables_all_three_slots() {
        let av = GearAvailability::from_probe(&hardware_result());
        assert!(av.neural_vocoder, "hardware accelerator must enable neural_vocoder slot");
        assert!(av.talking_head, "talking_head must be true with hardware accelerator");
        assert!(av.neural_plc, "neural_plc must be true with hardware accelerator");
    }

    // ── CPU-only provider ─────────────────────────────────────────────────────

    #[test]
    fn cpu_only_excludes_neural_vocoder_slot() {
        let av = GearAvailability::from_probe(&cpu_result());
        assert!(
            !av.neural_vocoder,
            "neural_vocoder slot must be false on CPU-only: 50 Hz × 8 ms/inf ≈ 400 % CPU"
        );
    }

    #[test]
    fn cpu_only_keeps_talking_head_slot() {
        let av = GearAvailability::from_probe(&cpu_result());
        assert!(
            av.talking_head,
            "talking_head slot must remain true on CPU-only \
             (runtime gate applied by Feature 82 based on current CPU load)"
        );
    }

    #[test]
    fn cpu_only_keeps_neural_plc_slot() {
        let av = GearAvailability::from_probe(&cpu_result());
        assert!(
            av.neural_plc,
            "neural_plc slot must remain true on CPU-only (2.5 ms/inf ≈ 12.5 % CPU)"
        );
    }

    // ── All hardware providers enable neural_vocoder ──────────────────────────

    #[test]
    fn all_hardware_providers_enable_neural_vocoder() {
        let hardware_providers = [
            ExecutionProvider::CoreMl,
            ExecutionProvider::Nnapi,
            ExecutionProvider::DirectMl,
        ];
        for provider in hardware_providers {
            let result = CapabilityProbeResult {
                provider,
                has_neural_accelerator: true,
            };
            let av = GearAvailability::from_probe(&result);
            assert!(
                av.neural_vocoder,
                "{provider:?} must enable the neural_vocoder slot"
            );
        }
    }

    // ── neural_vocoder tracks has_neural_accelerator exactly ──────────────────

    #[test]
    fn neural_vocoder_equals_has_neural_accelerator() {
        for has_accel in [true, false] {
            let result = CapabilityProbeResult {
                provider: if has_accel {
                    ExecutionProvider::CoreMl
                } else {
                    ExecutionProvider::Cpu
                },
                has_neural_accelerator: has_accel,
            };
            let av = GearAvailability::from_probe(&result);
            assert_eq!(
                av.neural_vocoder,
                has_accel,
                "neural_vocoder must equal has_neural_accelerator ({has_accel})"
            );
        }
    }

    // ── talking_head and neural_plc are always true ───────────────────────────

    #[test]
    fn talking_head_always_true_for_every_provider() {
        let cases = [
            (ExecutionProvider::CoreMl, true),
            (ExecutionProvider::Nnapi, true),
            (ExecutionProvider::DirectMl, true),
            (ExecutionProvider::Cpu, false),
        ];
        for (provider, has_accel) in cases {
            let result = CapabilityProbeResult {
                provider,
                has_neural_accelerator: has_accel,
            };
            let av = GearAvailability::from_probe(&result);
            assert!(
                av.talking_head,
                "talking_head slot must be true for every provider ({provider:?})"
            );
        }
    }

    #[test]
    fn neural_plc_always_true_for_every_provider() {
        let cases = [
            (ExecutionProvider::CoreMl, true),
            (ExecutionProvider::Nnapi, true),
            (ExecutionProvider::DirectMl, true),
            (ExecutionProvider::Cpu, false),
        ];
        for (provider, has_accel) in cases {
            let result = CapabilityProbeResult {
                provider,
                has_neural_accelerator: has_accel,
            };
            let av = GearAvailability::from_probe(&result);
            assert!(
                av.neural_plc,
                "neural_plc slot must be true for every provider ({provider:?})"
            );
        }
    }

    // ── GearAvailability::probe() ─────────────────────────────────────────────

    #[test]
    fn probe_completes_without_panicking() {
        let av = GearAvailability::probe();
        let _ = av.neural_vocoder;
        let _ = av.talking_head;
        let _ = av.neural_plc;
    }

    #[test]
    fn probe_consistent_with_capability_probe() {
        use crate::capability_probe::probe as nn_probe;
        let nn_result = nn_probe();
        let av = GearAvailability::probe();
        assert_eq!(
            av.neural_vocoder,
            nn_result.has_neural_accelerator,
            "neural_vocoder must equal has_neural_accelerator from capability_probe"
        );
        assert!(av.talking_head, "talking_head must always be true");
        assert!(av.neural_plc, "neural_plc must always be true");
    }

    // ── PartialEq / Copy ──────────────────────────────────────────────────────

    #[test]
    fn equal_availability_structs_compare_equal() {
        let a = GearAvailability {
            neural_vocoder: false,
            talking_head: true,
            neural_plc: true,
        };
        let b = GearAvailability {
            neural_vocoder: false,
            talking_head: true,
            neural_plc: true,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn different_neural_vocoder_field_compares_not_equal() {
        let with_vocoder = GearAvailability {
            neural_vocoder: true,
            talking_head: true,
            neural_plc: true,
        };
        let without_vocoder = GearAvailability {
            neural_vocoder: false,
            talking_head: true,
            neural_plc: true,
        };
        assert_ne!(with_vocoder, without_vocoder);
    }

    #[test]
    fn copy_is_independent() {
        let a = GearAvailability {
            neural_vocoder: true,
            talking_head: true,
            neural_plc: true,
        };
        let b = a; // Copy
        assert_eq!(a, b);
    }
}

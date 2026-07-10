//! Feature 79 — system decides which gears exist with capability_probe results
//! at startup.
//!
//! # Purpose
//!
//! Verifies that [`GearAvailability::from_probe`] maps a
//! [`CapabilityProbeResult`] to the correct set of neural gear slots, and that
//! [`GearAvailability::probe`] is consistent with the live
//! `capability_probe::probe()` result on the current platform.
//!
//! # Architecture contract
//!
//! The neural runtime calls `capability_probe::probe()` once at startup and
//! passes the result to `GearAvailability::from_probe` to determine which of
//! the three neural gear slots exist on this device:
//!
//! | Slot          | Condition for existence                                    |
//! |---------------|------------------------------------------------------------|
//! | NeuralVocoder | `has_neural_accelerator == true` (hardware NPU / GPU)      |
//! | TalkingHead   | Always `true` (runtime gate applied by Feature 82)         |
//! | NeuralPlc     | Always `true` (lightweight, CPU-capable)                   |
//!
//! # Key invariants verified
//!
//! * `GearAvailability::probe()` never panics on any supported platform.
//! * `neural_vocoder` is `true` iff `has_neural_accelerator` is `true`.
//! * `talking_head` and `neural_plc` are `true` on every platform.
//! * `GearAvailability::probe()` result is consistent with `nn_probe()`.
//! * On CPU-only platforms, `neural_vocoder` is `false` and the other two
//!   slots remain `true`.
//! * When `neural_vocoder` is `false`, `NpuCapability::probe()` is `Absent`
//!   and the neural vocoder cannot activate at any tier (cross-feature check).
//!
//! # Scenarios covered
//!
//! | # | Provider  | `has_neural_accelerator` | `neural_vocoder` | `talking_head` | `neural_plc` |
//! |---|-----------|--------------------------|------------------|----------------|--------------|
//! | 1 | CoreML    | true                     | true             | true           | true         |
//! | 2 | NNAPI     | true                     | true             | true           | true         |
//! | 3 | DirectML  | true                     | true             | true           | true         |
//! | 4 | CPU       | false                    | false            | true           | true         |
//! | 5 | (current) | platform-dependent       | matches probe    | true           | true         |

use lowband_nn::capability_probe::{probe as nn_probe, CapabilityProbeResult, ExecutionProvider};
use lowband_nn::gear_availability::GearAvailability;

// ── probe() safety ────────────────────────────────────────────────────────────

#[test]
fn gear_availability_probe_does_not_panic() {
    // The startup probe must complete on any supported platform without panicking.
    let av = GearAvailability::probe();
    let _ = av.neural_vocoder;
    let _ = av.talking_head;
    let _ = av.neural_plc;
}

// ── Scenario 1: CoreML (macOS / iOS) ─────────────────────────────────────────

#[test]
fn coreml_provider_enables_all_three_slots() {
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::CoreMl,
        has_neural_accelerator: true,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        av.neural_vocoder,
        "CoreML (hardware accelerator) must enable the neural_vocoder slot"
    );
    assert!(
        av.talking_head,
        "CoreML must leave the talking_head slot available"
    );
    assert!(
        av.neural_plc,
        "CoreML must leave the neural_plc slot available"
    );
}

// ── Scenario 2: NNAPI (Android) ──────────────────────────────────────────────

#[test]
fn nnapi_provider_enables_all_three_slots() {
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Nnapi,
        has_neural_accelerator: true,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        av.neural_vocoder,
        "NNAPI (hardware accelerator) must enable the neural_vocoder slot"
    );
    assert!(av.talking_head, "NNAPI must leave the talking_head slot available");
    assert!(av.neural_plc, "NNAPI must leave the neural_plc slot available");
}

// ── Scenario 3: DirectML (Windows) ───────────────────────────────────────────

#[test]
fn directml_provider_enables_all_three_slots() {
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::DirectMl,
        has_neural_accelerator: true,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        av.neural_vocoder,
        "DirectML (hardware accelerator) must enable the neural_vocoder slot"
    );
    assert!(av.talking_head, "DirectML must leave the talking_head slot available");
    assert!(av.neural_plc, "DirectML must leave the neural_plc slot available");
}

// ── Scenario 4: CPU-only (Linux / other) ─────────────────────────────────────

#[test]
fn cpu_provider_excludes_neural_vocoder_slot() {
    // CPU-only: 50 Hz × 8 ms/inference ≈ 400 % of one CPU core.
    // The neural vocoder slot must not exist on CPU-only systems.
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        !av.neural_vocoder,
        "CPU provider must exclude the neural_vocoder slot: \
         real-time 50 Hz inference (8 ms/inf) consumes ≈ 400 % of one CPU core"
    );
}

#[test]
fn cpu_provider_keeps_talking_head_slot() {
    // Talking head at 25 fps: ~30 ms total (12 ms keypoint + 18 ms synthesis).
    // Feasible on CPU with spare headroom; Feature 82 enforces the runtime check.
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        av.talking_head,
        "CPU provider must keep the talking_head slot available: \
         ~30 ms/frame at 25 fps fits within the CPU budget given spare headroom \
         (Feature 82 applies the runtime CPU-load check)"
    );
}

#[test]
fn cpu_provider_keeps_neural_plc_slot() {
    // Neural PLC: ~2.5 ms/inference at 50 Hz ≈ 12.5 % of one CPU core.
    // Always within the constrained-tier CPU ceiling.
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(
        av.neural_plc,
        "CPU provider must keep the neural_plc slot available: \
         2.5 ms/inf at 50 Hz ≈ 12.5 % of one CPU core fits any execution provider"
    );
}

// ── Scenario 5: platform-consistent live probe ────────────────────────────────

#[test]
fn live_probe_neural_vocoder_consistent_with_nn_probe() {
    // GearAvailability::probe() must agree with capability_probe::probe().
    let nn_result = nn_probe();
    let av = GearAvailability::probe();
    assert_eq!(
        av.neural_vocoder,
        nn_result.has_neural_accelerator,
        "neural_vocoder slot must equal has_neural_accelerator from capability_probe \
         (provider: {:?})",
        nn_result.provider
    );
}

#[test]
fn live_probe_talking_head_always_available() {
    let av = GearAvailability::probe();
    assert!(
        av.talking_head,
        "talking_head slot must be true on every platform \
         (runtime gate is Feature 82, not the startup probe)"
    );
}

#[test]
fn live_probe_neural_plc_always_available() {
    let av = GearAvailability::probe();
    assert!(
        av.neural_plc,
        "neural_plc slot must be true on every platform \
         (2.5 ms/inf is within CPU budget on any device)"
    );
}

// ── neural_vocoder exactly tracks has_neural_accelerator ─────────────────────

#[test]
fn neural_vocoder_slot_is_true_iff_hardware_accelerator_present() {
    // Exhaustive check across all four providers.
    let cases = [
        (ExecutionProvider::CoreMl,  true,  true),
        (ExecutionProvider::Nnapi,   true,  true),
        (ExecutionProvider::DirectMl, true, true),
        (ExecutionProvider::Cpu,     false, false),
    ];
    for (provider, has_accel, expected_vocoder) in cases {
        let result = CapabilityProbeResult {
            provider,
            has_neural_accelerator: has_accel,
        };
        let av = GearAvailability::from_probe(&result);
        assert_eq!(
            av.neural_vocoder,
            expected_vocoder,
            "neural_vocoder slot for provider {provider:?} with \
             has_neural_accelerator={has_accel} must be {expected_vocoder}"
        );
    }
}

// ── Talking head and PLC are provider-independent ─────────────────────────────

#[test]
fn talking_head_slot_true_for_all_providers() {
    let cases = [
        (ExecutionProvider::CoreMl,  true),
        (ExecutionProvider::Nnapi,   true),
        (ExecutionProvider::DirectMl, true),
        (ExecutionProvider::Cpu,     false),
    ];
    for (provider, has_accel) in cases {
        let result = CapabilityProbeResult {
            provider,
            has_neural_accelerator: has_accel,
        };
        let av = GearAvailability::from_probe(&result);
        assert!(
            av.talking_head,
            "talking_head slot must be true for every execution provider ({provider:?})"
        );
    }
}

#[test]
fn neural_plc_slot_true_for_all_providers() {
    let cases = [
        (ExecutionProvider::CoreMl,  true),
        (ExecutionProvider::Nnapi,   true),
        (ExecutionProvider::DirectMl, true),
        (ExecutionProvider::Cpu,     false),
    ];
    for (provider, has_accel) in cases {
        let result = CapabilityProbeResult {
            provider,
            has_neural_accelerator: has_accel,
        };
        let av = GearAvailability::from_probe(&result);
        assert!(
            av.neural_plc,
            "neural_plc slot must be true for every execution provider ({provider:?})"
        );
    }
}

// ── Cross-feature: neural_vocoder slot absent → vocoder never activates ───────

#[test]
fn absent_neural_vocoder_slot_means_vocoder_never_activates() {
    // When the startup probe reports no hardware accelerator, the
    // neural_vocoder slot is absent.  This must be consistent with
    // NpuCapability::Absent — the audio gear selector (Feature 83) must never
    // activate the NeuralVocoder codec.
    use lowband_platform::neural_vocoder::{audio_gear_from_tier_and_npu, AudioGear, NpuCapability};
    use lowband_platform::tier::TierState;

    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(!av.neural_vocoder, "precondition: neural_vocoder slot must be absent on CPU");

    // With no hardware accelerator, NpuCapability is Absent, which means
    // audio_gear_from_tier_and_npu must return OpusSilk at every tier.
    let tiers = [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ];
    for tier in tiers {
        let gear = audio_gear_from_tier_and_npu(tier, NpuCapability::Absent);
        assert_eq!(
            gear,
            AudioGear::OpusSilk,
            "NeuralVocoder must not activate at {tier:?} when neural_vocoder slot is absent"
        );
    }
}

// ── Cross-feature: neural_vocoder slot present → vocoder can activate ─────────

#[test]
fn present_neural_vocoder_slot_means_vocoder_can_activate_at_survival() {
    // When the startup probe reports a hardware accelerator, the neural_vocoder
    // slot exists.  At Survival tier + NpuCapability::Present, the vocoder
    // must activate (Feature 83).
    use lowband_platform::neural_vocoder::{audio_gear_from_tier_and_npu, AudioGear, NpuCapability};
    use lowband_platform::tier::TierState;

    let result = CapabilityProbeResult {
        provider: ExecutionProvider::CoreMl,
        has_neural_accelerator: true,
    };
    let av = GearAvailability::from_probe(&result);
    assert!(av.neural_vocoder, "precondition: neural_vocoder slot must be present");

    let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Present);
    assert!(
        matches!(gear, AudioGear::NeuralVocoder { .. }),
        "NeuralVocoder must activate at Survival+NPU when the neural_vocoder slot exists; \
         got {gear:?}"
    );
}

// ── Cross-feature: GearAvailability consistent with live NpuCapability probe ──

#[test]
fn gear_availability_neural_vocoder_consistent_with_npu_capability_probe() {
    // GearAvailability::probe() and NpuCapability::probe() must agree on
    // whether a hardware accelerator is present.
    use lowband_platform::neural_vocoder::NpuCapability;

    let av = GearAvailability::probe();
    let npu = NpuCapability::probe();

    let npu_present = npu == NpuCapability::Present;
    assert_eq!(
        av.neural_vocoder,
        npu_present,
        "GearAvailability::neural_vocoder must equal (NpuCapability::probe() == Present); \
         got neural_vocoder={} but npu={npu:?}",
        av.neural_vocoder
    );
}

// ── GearAvailability::from_probe is pure (same input → same output) ───────────

#[test]
fn from_probe_is_pure_same_input_same_output() {
    let result = CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    };
    let a = GearAvailability::from_probe(&result);
    let b = GearAvailability::from_probe(&result);
    assert_eq!(a, b, "from_probe must be a pure function");
}

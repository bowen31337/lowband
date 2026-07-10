//! Feature 83 — system gates the neural vocoder with capability_probe results.
//!
//! # Purpose
//!
//! Verifies that `NpuCapability::probe()` is wired to
//! `lowband_nn::capability_probe::probe()` and that the vocoder gate behaves
//! correctly given the probe result on the current platform.
//!
//! # Architecture contract
//!
//! The gate has two layers:
//!
//! 1. **`capability_probe::probe()`** (`lowband-nn`) — queries available ONNX
//!    Runtime execution providers (CoreML / NNAPI / DirectML / CPU) and sets
//!    `has_neural_accelerator`.
//! 2. **`NpuCapability::probe()`** (`lowband-platform`) — converts the probe
//!    result into `Present` or `Absent` and passes it to the governor.
//! 3. **`audio_gear_from_tier_and_npu()`** — activates `NeuralVocoder` only
//!    at `Survival` tier with `NpuCapability::Present`; uses `OpusSilk` in all
//!    other combinations.
//!
//! # Invariants verified
//!
//! * `capability_probe::probe()` never panics on any supported platform.
//! * `capability_probe::probe().has_neural_accelerator` is `false` when the
//!   provider is `ExecutionProvider::Cpu`.
//! * `NpuCapability::probe()` is consistent with `capability_probe::probe()`.
//! * The neural vocoder does not activate when the probe reports CPU-only.
//! * The neural vocoder activates exactly when the probe reports a hardware
//!   accelerator and the tier is `Survival`.

use lowband_nn::capability_probe::{probe as nn_probe, ExecutionProvider};
use lowband_platform::neural_vocoder::{
    audio_gear_from_tier_and_npu, AudioGear, NpuCapability,
};
use lowband_platform::tier::TierState;

// ── Probe invariants ──────────────────────────────────────────────────────────

#[test]
fn capability_probe_completes_without_panicking() {
    let result = nn_probe();
    // Just calling the probe on the current platform must not panic.
    let _ = result.provider;
    let _ = result.has_neural_accelerator;
}

#[test]
fn cpu_provider_never_claims_neural_accelerator() {
    // The CPU execution provider must not claim a hardware neural accelerator:
    // it is the fallback used when no NPU, GPU, or platform accelerator is
    // present.  Any system that can only run CPU inference must not activate
    // the neural vocoder (which requires real-time NPU throughput).
    let result = nn_probe();
    if result.provider == ExecutionProvider::Cpu {
        assert!(
            !result.has_neural_accelerator,
            "CPU execution provider must not report has_neural_accelerator == true"
        );
    }
}

#[test]
fn hardware_provider_claims_neural_accelerator() {
    // Every hardware-backed provider (CoreML, NNAPI, DirectML) must report
    // has_neural_accelerator == true.  This validates the consistency of the
    // probe result on any platform where a hardware provider is detected.
    let result = nn_probe();
    match result.provider {
        ExecutionProvider::CoreMl
        | ExecutionProvider::Nnapi
        | ExecutionProvider::DirectMl => {
            assert!(
                result.has_neural_accelerator,
                "{:?} provider must report has_neural_accelerator == true",
                result.provider
            );
        }
        ExecutionProvider::Cpu => {
            // CPU-only: covered by the previous test.
        }
    }
}

#[test]
fn probe_result_provider_and_accelerator_flag_are_consistent() {
    // The provider and has_neural_accelerator flag must agree on every platform.
    let result = nn_probe();
    let expected_accelerator = result.provider != ExecutionProvider::Cpu;
    assert_eq!(
        result.has_neural_accelerator,
        expected_accelerator,
        "provider {:?}: has_neural_accelerator must be {} (CPU=false, hardware=true)",
        result.provider,
        expected_accelerator
    );
}

// ── NpuCapability wired to capability_probe ───────────────────────────────────

#[test]
fn npu_capability_probe_matches_nn_capability_probe() {
    // NpuCapability::probe() must return Present iff the nn capability probe
    // reports has_neural_accelerator == true.  This is the core wiring check
    // for Feature 83.
    let probe_result = nn_probe();
    let npu = NpuCapability::probe();

    if probe_result.has_neural_accelerator {
        assert_eq!(
            npu,
            NpuCapability::Present,
            "NpuCapability::probe() must return Present when capability_probe \
             reports has_neural_accelerator == true (provider: {:?})",
            probe_result.provider
        );
    } else {
        assert_eq!(
            npu,
            NpuCapability::Absent,
            "NpuCapability::probe() must return Absent when capability_probe \
             reports has_neural_accelerator == false (provider: {:?})",
            probe_result.provider
        );
    }
}

// ── Neural vocoder gate ───────────────────────────────────────────────────────

#[test]
fn neural_vocoder_gate_consistent_with_probe_at_survival() {
    // At Survival tier the audio gear must match the probe: NeuralVocoder when
    // a hardware accelerator is present, OpusSilk when it is absent.
    let probe_result = nn_probe();
    let npu = NpuCapability::probe();
    let gear = audio_gear_from_tier_and_npu(TierState::Survival, npu);

    if probe_result.has_neural_accelerator {
        assert!(
            matches!(gear, AudioGear::NeuralVocoder { .. }),
            "NeuralVocoder must activate at Survival when probe reports hardware \
             accelerator (provider: {:?}); got {gear:?}",
            probe_result.provider
        );
    } else {
        assert_eq!(
            gear,
            AudioGear::OpusSilk,
            "OpusSilk must be selected at Survival when probe reports CPU-only \
             (provider: {:?}); got {gear:?}",
            probe_result.provider
        );
    }
}

#[test]
fn neural_vocoder_absent_npu_always_selects_opus_silk() {
    // When the capability probe returns Absent (CPU-only path), the neural
    // vocoder must never activate — even at Survival tier.  This is the
    // canonical gate: no hardware accelerator → no neural vocoder.
    let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Absent);
    assert_eq!(
        gear,
        AudioGear::OpusSilk,
        "OpusSilk must be selected at Survival when NpuCapability is Absent \
         (capability_probe returned CPU-only)"
    );
}

#[test]
fn neural_vocoder_present_npu_activates_at_survival() {
    // When the capability probe returns Present (hardware accelerator confirmed),
    // the neural vocoder must activate at Survival tier.
    let gear = audio_gear_from_tier_and_npu(TierState::Survival, NpuCapability::Present);
    assert!(
        matches!(gear, AudioGear::NeuralVocoder { .. }),
        "NeuralVocoder must activate at Survival when NpuCapability is Present; \
         got {gear:?}"
    );
}

#[test]
fn neural_vocoder_never_activates_above_survival_regardless_of_probe() {
    // The neural vocoder is a Survival-only optimisation.  Even when the
    // capability probe reports a hardware accelerator, it must not activate
    // above Survival tier.
    let non_survival_tiers = [
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ];
    for &tier in &non_survival_tiers {
        let gear = audio_gear_from_tier_and_npu(tier, NpuCapability::Present);
        assert_eq!(
            gear,
            AudioGear::OpusSilk,
            "NeuralVocoder must not activate at {tier:?} tier \
             (capability_probe: Present); got {gear:?}"
        );
    }
}

// ── Probe × tier exhaustive cross-check ──────────────────────────────────────

#[test]
fn audio_gear_matches_probe_across_all_tiers() {
    // Exhaustively verify that audio gear selection is consistent with the
    // current platform's capability probe across all tier states.
    let probe_result = nn_probe();
    let npu = NpuCapability::probe();

    let all_tiers = [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ];

    for &tier in &all_tiers {
        let gear = audio_gear_from_tier_and_npu(tier, npu);
        let expects_vocoder =
            tier == TierState::Survival && probe_result.has_neural_accelerator;

        if expects_vocoder {
            assert!(
                matches!(gear, AudioGear::NeuralVocoder { .. }),
                "NeuralVocoder expected at {tier:?} with probe={:?}; got {gear:?}",
                probe_result.provider
            );
        } else {
            assert_eq!(
                gear,
                AudioGear::OpusSilk,
                "OpusSilk expected at {tier:?} with probe={:?}; got {gear:?}",
                probe_result.provider
            );
        }
    }
}

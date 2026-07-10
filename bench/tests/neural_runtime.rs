//! Feature 78 — System hosts models on ONNX Runtime with execution_providers
//! for CoreML, NNAPI, DirectML, and CPU.
//!
//! # Scenario
//!
//! The neural runtime must map each platform's execution provider to a correct
//! ONNX Runtime [`SessionOptions`] value, always include CPU as a fallback
//! provider for all non-CPU primaries, and expose a [`ModelRuntime`] that
//! creates sessions for every [`ModelId`] using the probe-selected provider.
//!
//! # Test structure
//!
//! **Part A — provider config mapping**: `SessionOptions::for_provider` maps
//! each [`ExecutionProvider`] to the correct [`ProviderConfig`] variant with
//! production-default field values.
//!
//! **Part B — CPU fallback invariant**: every non-CPU provider list ends with
//! a CPU entry so ONNX Runtime can always fall back to CPU-only inference when
//! the primary hardware provider is unavailable at runtime (e.g. DirectML on a
//! machine without a DX12 GPU).
//!
//! **Part C — ModelRuntime session factory**: `ModelRuntime::from_probe`
//! derives the provider from the startup capability probe result and
//! `session_options` produces matching options for every [`ModelId`].
//!
//! **Part D — open_session model/provider association**: the stub session
//! returned by `open_session` carries the correct `model_id` and `provider`
//! for every provider/model combination.
//!
//! # Provider × model matrix
//!
//! | Provider  | NoiseSuppressor | NeuralVocoder | NeuralPlc | KeypointExtractor | SynthesisNetwork |
//! |-----------|:---------------:|:-------------:|:---------:|:-----------------:|:----------------:|
//! | CoreML    | primary=CoreML  | primary=CoreML| …         | …                 | …                |
//! | NNAPI     | primary=NNAPI   | …             | …         | …                 | …                |
//! | DirectML  | primary=DirectML| …             | …         | …                 | …                |
//! | CPU       | primary=CPU     | …             | …         | …                 | …                |
//!
//! Every (provider, model) cell has `has_cpu_fallback == true` except when the
//! primary provider is CPU itself (where CPU is the only entry).

use lowband_nn::capability_probe::{CapabilityProbeResult, ExecutionProvider};
use lowband_nn::eval_card::ModelId;
use lowband_nn::runtime::{ModelRuntime, OnnxSession, ProviderConfig, SessionOptions};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn coreml_probe() -> CapabilityProbeResult {
    CapabilityProbeResult { provider: ExecutionProvider::CoreMl, has_neural_accelerator: true }
}
fn nnapi_probe() -> CapabilityProbeResult {
    CapabilityProbeResult { provider: ExecutionProvider::Nnapi, has_neural_accelerator: true }
}
fn directml_probe() -> CapabilityProbeResult {
    CapabilityProbeResult { provider: ExecutionProvider::DirectMl, has_neural_accelerator: true }
}
fn cpu_probe() -> CapabilityProbeResult {
    CapabilityProbeResult { provider: ExecutionProvider::Cpu, has_neural_accelerator: false }
}

// ── Part A: provider config mapping ──────────────────────────────────────────

#[test]
fn coreml_session_options_primary_provider_is_coreml_not_cpu_only() {
    // CoreML EP must route to Neural Engine / GPU, never forced to CPU.
    let opts = SessionOptions::for_provider(ExecutionProvider::CoreMl);
    assert!(
        matches!(opts.primary_provider(), ProviderConfig::CoreMl { use_cpu_only: false }),
        "CoreML SessionOptions must have use_cpu_only=false; got {:?}",
        opts.primary_provider()
    );
}

#[test]
fn nnapi_session_options_primary_provider_is_nnapi_default_flags() {
    // NNAPI flags=0 lets the runtime select the best accelerator automatically.
    let opts = SessionOptions::for_provider(ExecutionProvider::Nnapi);
    assert!(
        matches!(opts.primary_provider(), ProviderConfig::Nnapi { flags: 0 }),
        "NNAPI SessionOptions must have flags=0 (runtime default); got {:?}",
        opts.primary_provider()
    );
}

#[test]
fn directml_session_options_primary_provider_is_directml_device_zero() {
    // device_index=0 selects the primary GPU adapter.
    let opts = SessionOptions::for_provider(ExecutionProvider::DirectMl);
    assert!(
        matches!(opts.primary_provider(), ProviderConfig::DirectMl { device_index: 0 }),
        "DirectML SessionOptions must have device_index=0 (primary GPU); got {:?}",
        opts.primary_provider()
    );
}

#[test]
fn cpu_session_options_primary_provider_is_cpu_auto_threads() {
    // intra_op_threads=0 tells ORT to choose the optimal thread count.
    let opts = SessionOptions::for_provider(ExecutionProvider::Cpu);
    assert!(
        matches!(opts.primary_provider(), ProviderConfig::Cpu { intra_op_threads: 0 }),
        "CPU SessionOptions must have intra_op_threads=0 (auto); got {:?}",
        opts.primary_provider()
    );
}

#[test]
fn provider_config_roundtrip_returns_matching_execution_provider() {
    // ProviderConfig::provider() must return the ExecutionProvider that was
    // used to build it.
    let cases = [
        (ExecutionProvider::CoreMl,   ProviderConfig::CoreMl { use_cpu_only: false }),
        (ExecutionProvider::Nnapi,    ProviderConfig::Nnapi { flags: 0 }),
        (ExecutionProvider::DirectMl, ProviderConfig::DirectMl { device_index: 0 }),
        (ExecutionProvider::Cpu,      ProviderConfig::Cpu { intra_op_threads: 0 }),
    ];
    for (expected, cfg) in cases {
        assert_eq!(
            cfg.provider(), expected,
            "ProviderConfig built for {expected:?} must report provider()={expected:?}"
        );
    }
}

// ── Part B: CPU fallback invariant ───────────────────────────────────────────

#[test]
fn coreml_options_always_include_cpu_fallback() {
    let opts = SessionOptions::for_provider(ExecutionProvider::CoreMl);
    assert!(
        opts.has_cpu_fallback(),
        "CoreML SessionOptions must include a CPU fallback so sessions initialise \
         even when CoreML is unavailable at runtime"
    );
}

#[test]
fn nnapi_options_always_include_cpu_fallback() {
    let opts = SessionOptions::for_provider(ExecutionProvider::Nnapi);
    assert!(
        opts.has_cpu_fallback(),
        "NNAPI SessionOptions must include a CPU fallback"
    );
}

#[test]
fn directml_options_always_include_cpu_fallback() {
    let opts = SessionOptions::for_provider(ExecutionProvider::DirectMl);
    assert!(
        opts.has_cpu_fallback(),
        "DirectML SessionOptions must include a CPU fallback so sessions can \
         initialise on machines without a DX12 GPU"
    );
}

#[test]
fn cpu_options_have_no_separate_cpu_fallback_entry() {
    // CPU is the primary provider — adding a second CPU entry would be
    // redundant and waste ORT initialisation time.
    let opts = SessionOptions::for_provider(ExecutionProvider::Cpu);
    assert_eq!(
        opts.provider_count(),
        1,
        "CPU-primary SessionOptions must have exactly one provider entry (no duplicate fallback)"
    );
}

#[test]
fn non_cpu_providers_have_exactly_two_providers_primary_plus_cpu() {
    for ep in [
        ExecutionProvider::CoreMl,
        ExecutionProvider::Nnapi,
        ExecutionProvider::DirectMl,
    ] {
        let opts = SessionOptions::for_provider(ep);
        assert_eq!(
            opts.provider_count(),
            2,
            "{ep:?} SessionOptions must have 2 providers: primary + CPU fallback"
        );
        // Confirm the second entry is the CPU fallback, not another hardware EP.
        assert!(
            matches!(opts.providers[1], ProviderConfig::Cpu { .. }),
            "{ep:?} options: second provider must be CPU fallback, got {:?}",
            opts.providers[1]
        );
    }
}

#[test]
fn graph_optimisations_enabled_by_default_for_all_providers() {
    for ep in [
        ExecutionProvider::CoreMl,
        ExecutionProvider::Nnapi,
        ExecutionProvider::DirectMl,
        ExecutionProvider::Cpu,
    ] {
        let opts = SessionOptions::for_provider(ep);
        assert!(
            opts.enable_graph_opts,
            "graph optimisations must be enabled by default for {ep:?}"
        );
    }
}

// ── Part C: ModelRuntime session factory ─────────────────────────────────────

#[test]
fn model_runtime_from_coreml_probe_selects_coreml() {
    let rt = ModelRuntime::from_probe(&coreml_probe());
    assert_eq!(rt.provider, ExecutionProvider::CoreMl);
    assert!(rt.has_neural_accelerator);
}

#[test]
fn model_runtime_from_nnapi_probe_selects_nnapi() {
    let rt = ModelRuntime::from_probe(&nnapi_probe());
    assert_eq!(rt.provider, ExecutionProvider::Nnapi);
    assert!(rt.has_neural_accelerator);
}

#[test]
fn model_runtime_from_directml_probe_selects_directml() {
    let rt = ModelRuntime::from_probe(&directml_probe());
    assert_eq!(rt.provider, ExecutionProvider::DirectMl);
    assert!(rt.has_neural_accelerator);
}

#[test]
fn model_runtime_from_cpu_probe_selects_cpu_no_accelerator() {
    let rt = ModelRuntime::from_probe(&cpu_probe());
    assert_eq!(rt.provider, ExecutionProvider::Cpu);
    assert!(!rt.has_neural_accelerator);
}

#[test]
fn session_options_primary_provider_matches_runtime_provider_for_all_models() {
    // Every model must get the same primary provider as the runtime was built with.
    let cases = [
        (coreml_probe(),   ExecutionProvider::CoreMl),
        (nnapi_probe(),    ExecutionProvider::Nnapi),
        (directml_probe(), ExecutionProvider::DirectMl),
        (cpu_probe(),      ExecutionProvider::Cpu),
    ];
    for (probe, expected_ep) in cases {
        let rt = ModelRuntime::from_probe(&probe);
        for &id in ModelId::ALL {
            let opts = rt.session_options(id);
            assert_eq!(
                opts.primary_provider().provider(),
                expected_ep,
                "session_options({id:?}) primary provider must be {expected_ep:?} \
                 when runtime was built from {expected_ep:?} probe"
            );
        }
    }
}

#[test]
fn session_options_hardware_runtimes_have_cpu_fallback_for_all_models() {
    let hardware_probes = [coreml_probe(), nnapi_probe(), directml_probe()];
    for probe in hardware_probes {
        let rt = ModelRuntime::from_probe(&probe);
        for &id in ModelId::ALL {
            let opts = rt.session_options(id);
            assert!(
                opts.has_cpu_fallback(),
                "session_options({id:?}) on {:?} runtime must include CPU fallback",
                probe.provider
            );
        }
    }
}

#[test]
fn session_options_cpu_runtime_no_duplicate_cpu_entry_for_any_model() {
    let rt = ModelRuntime::from_probe(&cpu_probe());
    for &id in ModelId::ALL {
        let opts = rt.session_options(id);
        assert_eq!(
            opts.provider_count(),
            1,
            "CPU runtime session_options({id:?}) must have exactly one provider (no duplicate)"
        );
    }
}

// ── Part D: open_session model/provider association ───────────────────────────

#[test]
fn open_session_model_id_matches_requested_model_for_all_providers() {
    let probes = [coreml_probe(), nnapi_probe(), directml_probe(), cpu_probe()];
    for probe in probes {
        let rt = ModelRuntime::from_probe(&probe);
        for &id in ModelId::ALL {
            let session = rt.open_session(id);
            assert_eq!(
                session.model_id, id,
                "open_session({id:?}) on {:?} runtime must return session.model_id={id:?}",
                probe.provider
            );
        }
    }
}

#[test]
fn open_session_provider_matches_runtime_provider_for_all_models() {
    let cases = [
        (coreml_probe(),   ExecutionProvider::CoreMl),
        (nnapi_probe(),    ExecutionProvider::Nnapi),
        (directml_probe(), ExecutionProvider::DirectMl),
        (cpu_probe(),      ExecutionProvider::Cpu),
    ];
    for (probe, expected_ep) in cases {
        let rt = ModelRuntime::from_probe(&probe);
        for &id in ModelId::ALL {
            let session = rt.open_session(id);
            assert_eq!(
                session.provider, expected_ep,
                "open_session({id:?}).provider must be {expected_ep:?} for a runtime \
                 built from {expected_ep:?} probe"
            );
        }
    }
}

#[test]
fn open_session_does_not_panic_for_any_model_provider_combination() {
    // Completeness check — must not panic for any (ModelId, provider) pair.
    let probes = [coreml_probe(), nnapi_probe(), directml_probe(), cpu_probe()];
    for probe in probes {
        let rt = ModelRuntime::from_probe(&probe);
        for &id in ModelId::ALL {
            let _ = rt.open_session(id);
        }
    }
}

#[test]
fn onnx_session_stub_carries_model_id_and_provider() {
    let session = OnnxSession::new_stub(ModelId::SynthesisNetwork, ExecutionProvider::DirectMl);
    assert_eq!(session.model_id, ModelId::SynthesisNetwork);
    assert_eq!(session.provider, ExecutionProvider::DirectMl);
}

#[test]
fn model_runtime_is_cloneable_and_clone_selects_same_provider() {
    // ModelRuntime must be Clone so it can be shared with Arc without extra wrapping.
    let rt = ModelRuntime::from_probe(&nnapi_probe());
    let rt2 = rt.clone();
    let session = rt2.open_session(ModelId::NeuralVocoder);
    assert_eq!(session.provider, ExecutionProvider::Nnapi);
}

// ── Cross-feature: ModelRuntime → warm_pool session association ───────────────

#[test]
fn runtime_open_session_model_ids_cover_all_warm_pool_models() {
    // Every model tracked by the warm pool must be openable via ModelRuntime.
    use lowband_nn::warm_pool::GEAR_A_MODELS;

    let rt = ModelRuntime::from_probe(&coreml_probe());

    // Open a session for each Gear A model (the models the warm pool tracks first).
    for &id in GEAR_A_MODELS {
        let session = rt.open_session(id);
        assert_eq!(session.model_id, id, "Gear A model {id:?} must be openable via ModelRuntime");
    }
    // Open sessions for all other models in the registry.
    for &id in ModelId::ALL {
        let session = rt.open_session(id);
        assert_eq!(session.model_id, id, "model {id:?} must be openable via ModelRuntime");
    }
}

// ── Diagnostic output ─────────────────────────────────────────────────────────

#[test]
fn print_runtime_provider_summary() {
    // Emitted under `cargo test -- --nocapture` for CI diagnostics.
    let probes = [
        ("CoreML  (macOS/iOS)", coreml_probe()),
        ("NNAPI   (Android)",   nnapi_probe()),
        ("DirectML (Windows)",  directml_probe()),
        ("CPU     (Linux/other)", cpu_probe()),
    ];
    eprintln!("neural_runtime — provider × session-options summary:");
    for (label, probe) in probes {
        let rt = ModelRuntime::from_probe(&probe);
        let opts = rt.session_options(ModelId::NoiseSuppressor);
        eprintln!(
            "  {:24}  primary={:?}  providers={}  cpu_fallback={}  graph_opts={}",
            label,
            opts.primary_provider().provider(),
            opts.provider_count(),
            opts.has_cpu_fallback(),
            opts.enable_graph_opts,
        );
    }
}

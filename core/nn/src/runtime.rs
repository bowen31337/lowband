//! ONNX Runtime model hosting — Feature 78.
//!
//! This module provides the runtime layer that hosts ONNX models with the
//! correct execution provider for the current platform.  It bridges the startup
//! capability probe result ([`crate::capability_probe`]) to ONNX Runtime
//! session options, mapping each [`ExecutionProvider`] variant to its
//! platform-specific configuration.
//!
//! # Provider hierarchy
//!
//! ONNX Runtime tries execution providers in the order they appear in the
//! [`SessionOptions::providers`] list.  The first provider that successfully
//! initialises handles the inference; the remaining providers are fallbacks.
//! The canonical priority order for each platform:
//!
//! | Platform    | Primary    | Fallback |
//! |-------------|------------|----------|
//! | macOS / iOS | CoreML     | CPU      |
//! | Android     | NNAPI      | CPU      |
//! | Windows     | DirectML   | CPU      |
//! | Linux/other | CPU        | *(none)* |
//!
//! CPU is always present in the provider list as the last-resort fallback so
//! that sessions can initialise even when the primary hardware provider is
//! unavailable at runtime (e.g. DirectML on a machine without a DX12 GPU).
//!
//! # Session creation flow
//!
//! 1. At startup [`capability_probe::probe`] determines the primary provider.
//! 2. [`ModelRuntime::from_probe`] captures the probe result.
//! 3. For each model the background loader calls [`ModelRuntime::session_options`]
//!    to get a [`SessionOptions`] and then [`ModelRuntime::open_session`] to
//!    create an [`OnnxSession`].
//! 4. Every `open_session` call is wrapped in [`crate::model_watchdog::ModelWatchdog::run`]
//!    (Feature 81) to enforce the 50 ms per-inference deadline.

use crate::capability_probe::{CapabilityProbeResult, ExecutionProvider};
use crate::eval_card::ModelId;

// ── ProviderConfig ────────────────────────────────────────────────────────────

/// Platform-specific configuration for one ONNX Runtime execution provider.
///
/// Each variant carries only the options that differ from ONNX Runtime defaults.
/// Callers serialise this to the native `OrtSessionOptions` C API or the `ort`
/// Rust crate's session builder.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderConfig {
    /// CoreML execution provider (macOS 12+ / iOS 16+).
    ///
    /// Routes inference to the Neural Engine on Apple Silicon or the GPU on
    /// Intel Mac.  Ops not supported by CoreML fall back to CPU automatically
    /// inside ONNX Runtime — no manual fallback list needed.
    CoreMl {
        /// Force CPU-only execution inside the CoreML EP.  Always `false` in
        /// production; exposed for unit tests that verify CPU fallback paths.
        use_cpu_only: bool,
    },
    /// NNAPI execution provider (Android 8.1+).
    ///
    /// Hardware flags are left at ONNX Runtime defaults; the NNAPI layer
    /// selects the best available accelerator (NPU, DSP, or GPU) per op.
    Nnapi {
        /// NNAPI feature flags bitfield (0 = ONNX Runtime default selection).
        flags: u32,
    },
    /// DirectML execution provider (Windows; requires DirectX 12 GPU).
    ///
    /// Device index 0 selects the primary GPU.  If no DX12 device is found at
    /// session-creation time ONNX Runtime falls back to CPU automatically.
    DirectMl {
        /// DirectX 12 adapter index (0 = primary GPU).
        device_index: u32,
    },
    /// CPU execution provider — always available, used as the last-resort fallback.
    ///
    /// An intra-op thread count of 0 lets ONNX Runtime choose the optimal
    /// count based on logical CPU count.
    Cpu {
        /// Intra-op parallelism thread count (0 = ONNX Runtime auto).
        intra_op_threads: u32,
    },
}

impl ProviderConfig {
    /// Return which [`ExecutionProvider`] variant this config corresponds to.
    pub fn provider(&self) -> ExecutionProvider {
        match self {
            ProviderConfig::CoreMl { .. }   => ExecutionProvider::CoreMl,
            ProviderConfig::Nnapi { .. }    => ExecutionProvider::Nnapi,
            ProviderConfig::DirectMl { .. } => ExecutionProvider::DirectMl,
            ProviderConfig::Cpu { .. }      => ExecutionProvider::Cpu,
        }
    }
}

// ── SessionOptions ────────────────────────────────────────────────────────────

/// ONNX Runtime session options for one inference session.
///
/// The `providers` list is ordered by priority: ONNX Runtime tries each entry
/// in turn and uses the first provider that initialises successfully.  CPU is
/// always the last entry so sessions can always fall back to CPU-only inference.
///
/// Build with [`SessionOptions::for_provider`] or through [`ModelRuntime`].
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Execution providers in descending priority order (highest priority first,
    /// CPU last for non-CPU primary providers).
    pub providers: Vec<ProviderConfig>,
    /// Maximum number of parallel graph-optimisation threads (0 = ORT auto).
    pub graph_opt_threads: u32,
    /// Whether to apply ONNX Runtime's graph-level optimisation passes.
    pub enable_graph_opts: bool,
}

impl SessionOptions {
    /// Build session options that target `provider` as the primary backend,
    /// with CPU as the automatic fallback for all non-CPU providers.
    ///
    /// | `provider` | Primary config                                  | Fallback |
    /// |------------|--------------------------------------------------|----------|
    /// | `CoreMl`   | `CoreMl { use_cpu_only: false }`                | CPU      |
    /// | `Nnapi`    | `Nnapi { flags: 0 }`                            | CPU      |
    /// | `DirectMl` | `DirectMl { device_index: 0 }`                  | CPU      |
    /// | `Cpu`      | `Cpu { intra_op_threads: 0 }`                   | *(none)* |
    pub fn for_provider(provider: ExecutionProvider) -> Self {
        let primary = match provider {
            ExecutionProvider::CoreMl   => ProviderConfig::CoreMl { use_cpu_only: false },
            ExecutionProvider::Nnapi    => ProviderConfig::Nnapi { flags: 0 },
            ExecutionProvider::DirectMl => ProviderConfig::DirectMl { device_index: 0 },
            ExecutionProvider::Cpu      => ProviderConfig::Cpu { intra_op_threads: 0 },
        };

        // CPU is already the primary provider — no need for an extra fallback entry.
        let providers = if matches!(provider, ExecutionProvider::Cpu) {
            vec![primary]
        } else {
            vec![primary, ProviderConfig::Cpu { intra_op_threads: 0 }]
        };

        Self {
            providers,
            graph_opt_threads: 0,
            enable_graph_opts: true,
        }
    }

    /// Return the primary (highest-priority) provider config.
    pub fn primary_provider(&self) -> &ProviderConfig {
        &self.providers[0]
    }

    /// Return `true` if a CPU provider entry is anywhere in the provider list.
    ///
    /// CPU must always be present as a fallback so that sessions can initialise
    /// even when the primary hardware provider is unavailable at runtime.
    pub fn has_cpu_fallback(&self) -> bool {
        self.providers.iter().any(|p| matches!(p, ProviderConfig::Cpu { .. }))
    }

    /// Number of providers in this options set (primary + any fallbacks).
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }
}

// ── OnnxSession ───────────────────────────────────────────────────────────────

/// A handle to one loaded ONNX Runtime inference session.
///
/// In production this wraps the native `OrtSession*` pointer (managed via the
/// `ort` Rust crate).  This stub records which model was loaded and which
/// execution provider was selected — sufficient for the warm pool, watchdog,
/// and governor to track session state without linking to the ONNX binary.
///
/// `OnnxSession` is `Send` so it can be moved into the background loader
/// thread supervised by [`crate::model_watchdog::ModelWatchdog`].
#[derive(Debug)]
pub struct OnnxSession {
    /// Which model this session hosts.
    pub model_id: ModelId,
    /// The primary execution provider this session was created with.
    pub provider: ExecutionProvider,
}

impl OnnxSession {
    /// Create a stub session that records `model_id` and `provider`.
    ///
    /// In production this is replaced by a call to `OrtCreateSession`; the
    /// stub is used by the warm pool pre-loader and unit tests.
    pub fn new_stub(model_id: ModelId, provider: ExecutionProvider) -> Self {
        Self { model_id, provider }
    }
}

// ── ModelRuntime ──────────────────────────────────────────────────────────────

/// ONNX Runtime host for all neural models.
///
/// `ModelRuntime` owns the platform execution-provider selection derived from
/// the startup capability probe.  It produces [`SessionOptions`] for each
/// model that the session builder uses to create an [`OnnxSession`].
///
/// One `ModelRuntime` is constructed at daemon startup from the
/// [`CapabilityProbeResult`] and shared (via `Arc`) with the background model
/// loader and any governor code that needs to inspect the active provider.
///
/// ## Thread safety
///
/// `ModelRuntime` is `Clone + Send + Sync`; all fields are immutable after
/// construction, so it is safe to share across threads without locking.
#[derive(Debug, Clone)]
pub struct ModelRuntime {
    /// Primary execution provider selected at startup.
    pub provider: ExecutionProvider,
    /// Whether a hardware neural accelerator is confirmed available.
    pub has_neural_accelerator: bool,
}

impl ModelRuntime {
    /// Construct a `ModelRuntime` from the startup capability probe result.
    ///
    /// Call once at daemon start immediately after
    /// [`crate::capability_probe::probe`]:
    ///
    /// ```
    /// use lowband_nn::capability_probe::probe;
    /// use lowband_nn::runtime::ModelRuntime;
    ///
    /// let runtime = ModelRuntime::from_probe(&probe());
    /// ```
    pub fn from_probe(probe: &CapabilityProbeResult) -> Self {
        Self {
            provider: probe.provider,
            has_neural_accelerator: probe.has_neural_accelerator,
        }
    }

    /// Build [`SessionOptions`] for `model_id` using the probe-selected provider.
    ///
    /// All models use the same primary provider; per-model differences (input
    /// shape, batch size, dynamic axes) are encoded in the ONNX graph and do
    /// not affect the execution-provider configuration.
    pub fn session_options(&self, _model_id: ModelId) -> SessionOptions {
        SessionOptions::for_provider(self.provider)
    }

    /// Open a stub [`OnnxSession`] for `model_id`.
    ///
    /// In production this calls `OrtCreateSession` with the options returned by
    /// [`Self::session_options`].  The current stub implementation returns an
    /// `OnnxSession` immediately without I/O.
    ///
    /// **Must** be run through
    /// [`crate::model_watchdog::ModelWatchdog::run`] (Feature 81) to enforce
    /// the 50 ms per-inference deadline.
    pub fn open_session(&self, model_id: ModelId) -> OnnxSession {
        OnnxSession::new_stub(model_id, self.provider)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn coreml_probe() -> CapabilityProbeResult {
        CapabilityProbeResult { provider: ExecutionProvider::CoreMl,  has_neural_accelerator: true }
    }
    fn nnapi_probe() -> CapabilityProbeResult {
        CapabilityProbeResult { provider: ExecutionProvider::Nnapi,   has_neural_accelerator: true }
    }
    fn directml_probe() -> CapabilityProbeResult {
        CapabilityProbeResult { provider: ExecutionProvider::DirectMl, has_neural_accelerator: true }
    }
    fn cpu_probe() -> CapabilityProbeResult {
        CapabilityProbeResult { provider: ExecutionProvider::Cpu, has_neural_accelerator: false }
    }

    // ── ProviderConfig::provider() ────────────────────────────────────────────

    #[test]
    fn provider_config_coreml_returns_coreml_variant() {
        let cfg = ProviderConfig::CoreMl { use_cpu_only: false };
        assert_eq!(cfg.provider(), ExecutionProvider::CoreMl);
    }

    #[test]
    fn provider_config_nnapi_returns_nnapi_variant() {
        let cfg = ProviderConfig::Nnapi { flags: 0 };
        assert_eq!(cfg.provider(), ExecutionProvider::Nnapi);
    }

    #[test]
    fn provider_config_directml_returns_directml_variant() {
        let cfg = ProviderConfig::DirectMl { device_index: 0 };
        assert_eq!(cfg.provider(), ExecutionProvider::DirectMl);
    }

    #[test]
    fn provider_config_cpu_returns_cpu_variant() {
        let cfg = ProviderConfig::Cpu { intra_op_threads: 0 };
        assert_eq!(cfg.provider(), ExecutionProvider::Cpu);
    }

    // ── SessionOptions::for_provider primary config ───────────────────────────

    #[test]
    fn for_provider_coreml_primary_is_coreml_not_cpu_only() {
        let opts = SessionOptions::for_provider(ExecutionProvider::CoreMl);
        assert!(
            matches!(opts.primary_provider(), ProviderConfig::CoreMl { use_cpu_only: false }),
            "CoreML options must set use_cpu_only=false; got {:?}",
            opts.primary_provider()
        );
    }

    #[test]
    fn for_provider_nnapi_primary_is_nnapi_with_zero_flags() {
        let opts = SessionOptions::for_provider(ExecutionProvider::Nnapi);
        assert!(
            matches!(opts.primary_provider(), ProviderConfig::Nnapi { flags: 0 }),
            "NNAPI options must start with flags=0; got {:?}",
            opts.primary_provider()
        );
    }

    #[test]
    fn for_provider_directml_primary_is_directml_device_zero() {
        let opts = SessionOptions::for_provider(ExecutionProvider::DirectMl);
        assert!(
            matches!(opts.primary_provider(), ProviderConfig::DirectMl { device_index: 0 }),
            "DirectML options must target device_index=0; got {:?}",
            opts.primary_provider()
        );
    }

    #[test]
    fn for_provider_cpu_primary_is_cpu_auto_threads() {
        let opts = SessionOptions::for_provider(ExecutionProvider::Cpu);
        assert!(
            matches!(opts.primary_provider(), ProviderConfig::Cpu { intra_op_threads: 0 }),
            "CPU options must use intra_op_threads=0 (auto); got {:?}",
            opts.primary_provider()
        );
    }

    // ── CPU fallback invariant ────────────────────────────────────────────────

    #[test]
    fn coreml_options_have_cpu_fallback() {
        let opts = SessionOptions::for_provider(ExecutionProvider::CoreMl);
        assert!(
            opts.has_cpu_fallback(),
            "CoreML session options must include a CPU fallback provider"
        );
    }

    #[test]
    fn nnapi_options_have_cpu_fallback() {
        let opts = SessionOptions::for_provider(ExecutionProvider::Nnapi);
        assert!(
            opts.has_cpu_fallback(),
            "NNAPI session options must include a CPU fallback provider"
        );
    }

    #[test]
    fn directml_options_have_cpu_fallback() {
        let opts = SessionOptions::for_provider(ExecutionProvider::DirectMl);
        assert!(
            opts.has_cpu_fallback(),
            "DirectML session options must include a CPU fallback provider"
        );
    }

    #[test]
    fn cpu_options_has_exactly_one_provider() {
        // CPU is its own primary — no additional fallback entry is needed.
        let opts = SessionOptions::for_provider(ExecutionProvider::Cpu);
        assert_eq!(
            opts.provider_count(),
            1,
            "CPU-primary options must have exactly one provider entry (CPU itself)"
        );
    }

    #[test]
    fn non_cpu_options_have_exactly_two_providers() {
        // Primary hardware provider + CPU fallback.
        for provider in [
            ExecutionProvider::CoreMl,
            ExecutionProvider::Nnapi,
            ExecutionProvider::DirectMl,
        ] {
            let opts = SessionOptions::for_provider(provider);
            assert_eq!(
                opts.provider_count(),
                2,
                "{provider:?} options must have 2 providers (primary + CPU fallback)"
            );
        }
    }

    #[test]
    fn cpu_options_cpu_is_primary_not_fallback() {
        let opts = SessionOptions::for_provider(ExecutionProvider::Cpu);
        assert!(
            opts.has_cpu_fallback(),
            "CPU options must expose CPU via has_cpu_fallback (CPU is the primary provider)"
        );
        // The single provider entry must be the CPU primary, not a separate fallback.
        assert_eq!(opts.providers.len(), 1);
    }

    // ── graph_opt defaults ────────────────────────────────────────────────────

    #[test]
    fn graph_opts_enabled_by_default() {
        for provider in [
            ExecutionProvider::CoreMl,
            ExecutionProvider::Nnapi,
            ExecutionProvider::DirectMl,
            ExecutionProvider::Cpu,
        ] {
            let opts = SessionOptions::for_provider(provider);
            assert!(
                opts.enable_graph_opts,
                "graph optimisations must be enabled by default for {provider:?}"
            );
        }
    }

    #[test]
    fn graph_opt_threads_is_zero_by_default() {
        for provider in [
            ExecutionProvider::CoreMl,
            ExecutionProvider::Nnapi,
            ExecutionProvider::DirectMl,
            ExecutionProvider::Cpu,
        ] {
            let opts = SessionOptions::for_provider(provider);
            assert_eq!(
                opts.graph_opt_threads, 0,
                "graph_opt_threads must default to 0 (auto) for {provider:?}"
            );
        }
    }

    // ── ModelRuntime::from_probe ──────────────────────────────────────────────

    #[test]
    fn from_probe_captures_coreml_provider() {
        let rt = ModelRuntime::from_probe(&coreml_probe());
        assert_eq!(rt.provider, ExecutionProvider::CoreMl);
        assert!(rt.has_neural_accelerator);
    }

    #[test]
    fn from_probe_captures_nnapi_provider() {
        let rt = ModelRuntime::from_probe(&nnapi_probe());
        assert_eq!(rt.provider, ExecutionProvider::Nnapi);
        assert!(rt.has_neural_accelerator);
    }

    #[test]
    fn from_probe_captures_directml_provider() {
        let rt = ModelRuntime::from_probe(&directml_probe());
        assert_eq!(rt.provider, ExecutionProvider::DirectMl);
        assert!(rt.has_neural_accelerator);
    }

    #[test]
    fn from_probe_captures_cpu_provider() {
        let rt = ModelRuntime::from_probe(&cpu_probe());
        assert_eq!(rt.provider, ExecutionProvider::Cpu);
        assert!(!rt.has_neural_accelerator);
    }

    // ── ModelRuntime::session_options ─────────────────────────────────────────

    #[test]
    fn session_options_primary_matches_probe_provider_for_all_models() {
        let rt = ModelRuntime::from_probe(&coreml_probe());
        for &id in ModelId::ALL {
            let opts = rt.session_options(id);
            assert_eq!(
                opts.primary_provider().provider(),
                ExecutionProvider::CoreMl,
                "session_options({id:?}).primary_provider must be CoreML"
            );
        }
    }

    #[test]
    fn session_options_all_models_have_cpu_fallback_on_coreml() {
        let rt = ModelRuntime::from_probe(&coreml_probe());
        for &id in ModelId::ALL {
            let opts = rt.session_options(id);
            assert!(
                opts.has_cpu_fallback(),
                "session_options({id:?}) on CoreML must include a CPU fallback"
            );
        }
    }

    #[test]
    fn session_options_cpu_runtime_no_duplicate_cpu_fallback() {
        let rt = ModelRuntime::from_probe(&cpu_probe());
        for &id in ModelId::ALL {
            let opts = rt.session_options(id);
            assert_eq!(
                opts.provider_count(),
                1,
                "CPU-primary runtime must produce exactly one provider entry for {id:?}"
            );
        }
    }

    // ── ModelRuntime::open_session ────────────────────────────────────────────

    #[test]
    fn open_session_carries_correct_model_id() {
        let rt = ModelRuntime::from_probe(&coreml_probe());
        for &id in ModelId::ALL {
            let session = rt.open_session(id);
            assert_eq!(
                session.model_id, id,
                "open_session({id:?}).model_id must equal the requested ModelId"
            );
        }
    }

    #[test]
    fn open_session_carries_correct_provider() {
        let cases = [
            (coreml_probe(),   ExecutionProvider::CoreMl),
            (nnapi_probe(),    ExecutionProvider::Nnapi),
            (directml_probe(), ExecutionProvider::DirectMl),
            (cpu_probe(),      ExecutionProvider::Cpu),
        ];
        for (probe, expected_provider) in cases {
            let rt = ModelRuntime::from_probe(&probe);
            let session = rt.open_session(ModelId::NoiseSuppressor);
            assert_eq!(
                session.provider, expected_provider,
                "open_session on {expected_provider:?} runtime must produce a session \
                 with provider={expected_provider:?}"
            );
        }
    }

    #[test]
    fn open_session_does_not_panic_for_any_model_or_provider() {
        let probes = [coreml_probe(), nnapi_probe(), directml_probe(), cpu_probe()];
        for probe in probes {
            let rt = ModelRuntime::from_probe(&probe);
            for &id in ModelId::ALL {
                let _ = rt.open_session(id);
            }
        }
    }

    // ── ModelRuntime is Clone ─────────────────────────────────────────────────

    #[test]
    fn model_runtime_clone_preserves_provider() {
        let rt = ModelRuntime::from_probe(&directml_probe());
        let rt2 = rt.clone();
        assert_eq!(rt.provider, rt2.provider);
        assert_eq!(rt.has_neural_accelerator, rt2.has_neural_accelerator);
    }

    // ── OnnxSession stub ──────────────────────────────────────────────────────

    #[test]
    fn onnx_session_new_stub_sets_model_id_and_provider() {
        let session = OnnxSession::new_stub(ModelId::SynthesisNetwork, ExecutionProvider::CoreMl);
        assert_eq!(session.model_id, ModelId::SynthesisNetwork);
        assert_eq!(session.provider, ExecutionProvider::CoreMl);
    }
}

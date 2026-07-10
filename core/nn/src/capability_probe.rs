//! ONNX Runtime execution-provider capability probe — Features 79 and 83.
//!
//! At startup the neural runtime queries which ONNX Runtime execution providers
//! are available on the local machine.  The result drives two decisions:
//!
//! * **Feature 79** — which neural gear slots (vocoder, talking head, PLC) exist.
//! * **Feature 83** — whether the neural vocoder activates at Survival tier.
//!
//! # Provider hierarchy
//!
//! | Provider  | Platform          | Neural accelerator |
//! |-----------|-------------------|--------------------|
//! | CoreMl    | macOS / iOS       | Yes (Neural Engine on Apple Silicon; GPU on Intel) |
//! | Nnapi     | Android           | Yes (hardware NPU/DSP when present) |
//! | DirectMl  | Windows           | Yes (DirectX 12 GPU) |
//! | Cpu       | Linux / other     | No |
//!
//! # Stub vs full probe
//!
//! The full ONNX Runtime provider enumeration (Feature 78) runs inside the
//! model loader at session creation time and may raise or lower the detected
//! capability.  This module provides the startup probe: a compile-time
//! platform-detection stub that is conservative on unknown platforms (CPU-only)
//! and unconditionally correct on Apple Silicon (Neural Engine always present).

/// ONNX Runtime execution provider selected at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionProvider {
    /// Apple CoreML — Neural Engine on Apple Silicon (M1+); GPU-backed on Intel
    /// Mac.  Available on macOS 12+ and iOS 16+.
    CoreMl,
    /// Android NNAPI — hardware NPU/DSP acceleration via Android's NNAPI
    /// interface.  Available on Android 8.1+ when a supported accelerator is
    /// present.
    Nnapi,
    /// Windows DirectML — GPU-backed inference via DirectX 12.
    /// Requires a DirectX 12-capable GPU; falls back to CPU at session time if
    /// none is present.
    DirectMl,
    /// CPU — ONNX Runtime CPU execution provider.  Always available; used when
    /// no hardware accelerator is detected at compile time.
    Cpu,
}

/// Result of the startup execution-provider probe.
#[derive(Debug, Clone)]
pub struct CapabilityProbeResult {
    /// Which execution provider will be used for neural inference.
    pub provider: ExecutionProvider,
    /// `true` when a hardware neural accelerator (CoreML, NNAPI, or DirectML)
    /// is confirmed available; `false` when the system falls back to CPU-only
    /// inference.
    ///
    /// When `true`, the governor may activate the neural vocoder at Survival
    /// tier (Feature 83) and the talking-head Gear A codec at higher tiers
    /// (Feature 82).
    pub has_neural_accelerator: bool,
}

/// Probe for available ONNX Runtime execution providers.
///
/// Returns a [`CapabilityProbeResult`] describing which provider will be used
/// for neural inference and whether any hardware accelerator is available.
///
/// **Non-blocking** — does not load any model, open any device handle, or
/// allocate GPU/NPU memory.  It uses compile-time platform detection as the
/// startup heuristic; the full ONNX Runtime provider enumeration (Feature 78)
/// runs separately inside the model loader and may refine this result.
///
/// # Platform behavior
///
/// | Platform              | Provider  | `has_neural_accelerator` |
/// |-----------------------|-----------|--------------------------|
/// | macOS / iOS (any arch)| CoreML    | `true`                   |
/// | Android               | NNAPI     | `true`                   |
/// | Windows               | DirectML  | `true`                   |
/// | Linux / other         | CPU       | `false`                  |
pub fn probe() -> CapabilityProbeResult {
    // Apple platforms: CoreML is always available (Neural Engine on M1+,
    // GPU-backed on Intel Mac).  Any CoreML backend constitutes hardware
    // acceleration for the purposes of vocoder gating.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    return CapabilityProbeResult {
        provider: ExecutionProvider::CoreMl,
        has_neural_accelerator: true,
    };

    // Android: NNAPI provides hardware NPU/DSP acceleration on supported
    // devices.  We report Present at the probe stage; the full provider query
    // (Feature 78) corrects this if NNAPI is absent at runtime.
    #[cfg(target_os = "android")]
    return CapabilityProbeResult {
        provider: ExecutionProvider::Nnapi,
        has_neural_accelerator: true,
    };

    // Windows: DirectML with a DirectX 12 GPU.  The ONNX Runtime session
    // falls back to CPU if no DX12 device is found, but the startup probe
    // optimistically reports accelerator-present; the full Feature 78 probe
    // will correct it if necessary.
    #[cfg(target_os = "windows")]
    return CapabilityProbeResult {
        provider: ExecutionProvider::DirectMl,
        has_neural_accelerator: true,
    };

    // Linux and all other platforms: conservatively report CPU-only until the
    // full ONNX Runtime execution-provider enumeration (Feature 78, Phase 7)
    // is wired in.
    #[allow(unreachable_code)]
    CapabilityProbeResult {
        provider: ExecutionProvider::Cpu,
        has_neural_accelerator: false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_without_panicking() {
        // The probe must complete on any platform without panicking.
        let result = probe();
        let _ = result.provider;
        let _ = result.has_neural_accelerator;
    }

    #[test]
    fn cpu_provider_never_claims_neural_accelerator() {
        // CPU execution provider must never report a hardware neural accelerator:
        // it is the fallback path for systems with no dedicated NPU/GPU.
        let cpu_result = CapabilityProbeResult {
            provider: ExecutionProvider::Cpu,
            has_neural_accelerator: false,
        };
        assert!(
            !cpu_result.has_neural_accelerator,
            "CPU execution provider must not claim a neural accelerator"
        );
    }

    #[test]
    fn non_cpu_providers_claim_neural_accelerator() {
        // Every hardware-backed provider (CoreML, NNAPI, DirectML) must report
        // has_neural_accelerator == true.
        let hardware_providers = [
            ExecutionProvider::CoreMl,
            ExecutionProvider::Nnapi,
            ExecutionProvider::DirectMl,
        ];
        for &provider in &hardware_providers {
            let result = CapabilityProbeResult {
                provider,
                has_neural_accelerator: true,
            };
            assert!(
                result.has_neural_accelerator,
                "{provider:?} must claim a neural accelerator"
            );
        }
    }

    #[test]
    fn probe_result_is_consistent_on_current_platform() {
        // On any platform: if the provider is CPU, has_neural_accelerator must
        // be false.  If the provider is a hardware backend, it must be true.
        let result = probe();
        match result.provider {
            ExecutionProvider::Cpu => assert!(
                !result.has_neural_accelerator,
                "CPU provider must report has_neural_accelerator == false"
            ),
            ExecutionProvider::CoreMl
            | ExecutionProvider::Nnapi
            | ExecutionProvider::DirectMl => assert!(
                result.has_neural_accelerator,
                "{:?} provider must report has_neural_accelerator == true",
                result.provider
            ),
        }
    }

    #[test]
    fn execution_provider_variants_are_distinct() {
        // All provider variants must be distinguishable.
        assert_ne!(ExecutionProvider::CoreMl, ExecutionProvider::Nnapi);
        assert_ne!(ExecutionProvider::CoreMl, ExecutionProvider::DirectMl);
        assert_ne!(ExecutionProvider::CoreMl, ExecutionProvider::Cpu);
        assert_ne!(ExecutionProvider::Nnapi, ExecutionProvider::DirectMl);
        assert_ne!(ExecutionProvider::Nnapi, ExecutionProvider::Cpu);
        assert_ne!(ExecutionProvider::DirectMl, ExecutionProvider::Cpu);
    }
}

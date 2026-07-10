//! ONNX model versioning — Feature 84.
//!
//! Every model hosted by the neural runtime carries an [`EvalCard`] that
//! captures the version, ONNX opset, primary evaluation metric, and reference
//! inference latency.  The source-of-truth TOML files live in the workspace
//! `models/<name>/eval_card.toml`; the Rust constants here mirror those files
//! and are embedded at compile time for zero-cost runtime access.
//!
//! # Invariants
//!
//! * Every [`ModelId`] variant has a corresponding entry in [`MODEL_REGISTRY`].
//! * `sha256` is a 64-character lowercase hex string (placeholder until the
//!   ONNX binary is present; the capability probe rejects a model whose file
//!   digest does not match the registered value).
//! * `onnx_opset >= 17` — the minimum opset required by ONNX Runtime 1.16+.
//! * `inference_p50_ms > 0` — the measured p50 latency on the reference
//!   hardware (Intel Core i5-5200U, 2015-class, 1 thread).

// ── ModelId ───────────────────────────────────────────────────────────────────

/// Identifier for each ONNX model hosted by the neural runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelId {
    /// RNNoise-class neural noise suppressor (Feature 46).
    NoiseSuppressor,
    /// SoundStream-lineage neural vocoder at 3.2–6 kbps (Feature 54).
    NeuralVocoder,
    /// Neural packet-loss concealment (Feature 57).
    NeuralPlc,
    /// Implicit 3-D facial keypoint extractor for Gear A (Feature 118).
    KeypointExtractor,
    /// Receiver-side head warping and synthesis network for Gear A (Feature 120).
    SynthesisNetwork,
}

impl ModelId {
    /// All model identifiers in registry order.
    pub const ALL: &'static [ModelId] = &[
        ModelId::NoiseSuppressor,
        ModelId::NeuralVocoder,
        ModelId::NeuralPlc,
        ModelId::KeypointExtractor,
        ModelId::SynthesisNetwork,
    ];
}

// ── EvalCard ──────────────────────────────────────────────────────────────────

/// Evaluation card for one ONNX model.
///
/// Every field is `'static` so eval cards can be stored as compile-time
/// constants with no allocation.  The `raw_toml` field carries the full
/// TOML source embedded from `models/<name>/eval_card.toml` via
/// `include_str!`; external tooling can inspect or re-parse it without
/// linking to the binary.
#[derive(Debug, Clone, Copy)]
pub struct EvalCard {
    /// Unique model name — must match the directory under `models/`.
    pub name: &'static str,
    /// Semantic version string (e.g. `"1.0.0"`).
    pub version: &'static str,
    /// Lowercase hex SHA-256 digest of the `.onnx` file (64 characters).
    /// All-zero placeholder until the real model binary is committed.
    pub sha256: &'static str,
    /// ONNX opset the model was exported at.  Must be ≥ 17.
    pub onnx_opset: u16,
    /// Name of the primary evaluation metric (e.g. `"snr_improvement_db"`).
    pub primary_metric: &'static str,
    /// Numeric value of the primary metric.
    pub primary_value: f64,
    /// Dataset used for the primary metric measurement.
    pub metric_dataset: &'static str,
    /// Median single-inference latency in milliseconds on [`reference_hw`].
    pub inference_p50_ms: f64,
    /// Hardware description for the latency measurement.
    pub reference_hw: &'static str,
    /// Raw TOML source from `models/<name>/eval_card.toml`.
    pub raw_toml: &'static str,
}

// ── Per-model constants ───────────────────────────────────────────────────────

const NOISE_SUPPRESSOR: EvalCard = EvalCard {
    name: "noise_suppressor",
    version: "1.0.0",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    onnx_opset: 17,
    primary_metric: "snr_improvement_db",
    primary_value: 12.3,
    metric_dataset: "NOIZEUS-16k",
    inference_p50_ms: 0.15,
    reference_hw: "Intel Core i5-5200U @ 2.7 GHz (2015-class, 1 thread)",
    raw_toml: include_str!("../../../models/noise_suppressor/eval_card.toml"),
};

const NEURAL_VOCODER: EvalCard = EvalCard {
    name: "neural_vocoder",
    version: "1.0.0",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    onnx_opset: 17,
    primary_metric: "pesq_mos",
    primary_value: 3.4,
    metric_dataset: "ITU-T P.863 multilingual",
    inference_p50_ms: 8.0,
    reference_hw: "Intel Core i5-5200U @ 2.7 GHz (2015-class, 1 thread)",
    raw_toml: include_str!("../../../models/neural_vocoder/eval_card.toml"),
};

const NEURAL_PLC: EvalCard = EvalCard {
    name: "neural_plc",
    version: "1.0.0",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    onnx_opset: 17,
    primary_metric: "plcmos",
    primary_value: 3.8,
    metric_dataset: "DNS Challenge 2020",
    inference_p50_ms: 2.5,
    reference_hw: "Intel Core i5-5200U @ 2.7 GHz (2015-class, 1 thread)",
    raw_toml: include_str!("../../../models/neural_plc/eval_card.toml"),
};

const KEYPOINT_EXTRACTOR: EvalCard = EvalCard {
    name: "keypoint_extractor",
    version: "1.0.0",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    onnx_opset: 17,
    primary_metric: "nme_percentage",
    primary_value: 3.1,
    metric_dataset: "300W",
    inference_p50_ms: 12.0,
    reference_hw: "Intel Core i5-5200U @ 2.7 GHz (2015-class, 1 thread)",
    raw_toml: include_str!("../../../models/keypoint_extractor/eval_card.toml"),
};

const SYNTHESIS_NETWORK: EvalCard = EvalCard {
    name: "synthesis_network",
    version: "1.0.0",
    sha256: "0000000000000000000000000000000000000000000000000000000000000000",
    onnx_opset: 17,
    primary_metric: "fvd",
    primary_value: 45.2,
    metric_dataset: "VoxCeleb2",
    inference_p50_ms: 18.0,
    reference_hw: "Intel Core i5-5200U @ 2.7 GHz (2015-class, 1 thread)",
    raw_toml: include_str!("../../../models/synthesis_network/eval_card.toml"),
};

// ── Registry ──────────────────────────────────────────────────────────────────

/// All eval cards in [`ModelId::ALL`] order.
///
/// The index of each entry matches the discriminant order of [`ModelId::ALL`]:
/// `[0]` = `NoiseSuppressor`, `[1]` = `NeuralVocoder`, etc.
pub const MODEL_REGISTRY: &[EvalCard] = &[
    NOISE_SUPPRESSOR,
    NEURAL_VOCODER,
    NEURAL_PLC,
    KEYPOINT_EXTRACTOR,
    SYNTHESIS_NETWORK,
];

/// Return the [`EvalCard`] for a given [`ModelId`].
///
/// The lookup is O(1) by index; no allocation or I/O.
pub const fn eval_card(id: ModelId) -> &'static EvalCard {
    let idx = match id {
        ModelId::NoiseSuppressor => 0,
        ModelId::NeuralVocoder => 1,
        ModelId::NeuralPlc => 2,
        ModelId::KeypointExtractor => 3,
        ModelId::SynthesisNetwork => 4,
    };
    &MODEL_REGISTRY[idx]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn is_semver(s: &str) -> bool {
        let parts: Vec<&str> = s.split('.').collect();
        parts.len() == 3 && parts.iter().all(|p| p.parse::<u32>().is_ok())
    }

    fn is_hex64(s: &str) -> bool {
        s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn registry_length_matches_model_id_all() {
        assert_eq!(
            MODEL_REGISTRY.len(),
            ModelId::ALL.len(),
            "MODEL_REGISTRY must have one entry per ModelId variant"
        );
    }

    #[test]
    fn eval_card_name_matches_registry_order() {
        let expected = ["noise_suppressor", "neural_vocoder", "neural_plc",
                        "keypoint_extractor", "synthesis_network"];
        for (i, &id) in ModelId::ALL.iter().enumerate() {
            assert_eq!(
                eval_card(id).name,
                expected[i],
                "ModelId {:?} must map to name {:?}",
                id,
                expected[i]
            );
        }
    }

    #[test]
    fn all_versions_are_valid_semver() {
        for card in MODEL_REGISTRY {
            assert!(
                is_semver(card.version),
                "model {:?} version {:?} is not valid semver",
                card.name,
                card.version
            );
        }
    }

    #[test]
    fn all_sha256_are_64_hex_chars() {
        for card in MODEL_REGISTRY {
            assert!(
                is_hex64(card.sha256),
                "model {:?} sha256 {:?} must be 64 hex characters",
                card.name,
                card.sha256
            );
        }
    }

    #[test]
    fn all_onnx_opsets_at_least_17() {
        for card in MODEL_REGISTRY {
            assert!(
                card.onnx_opset >= 17,
                "model {:?} onnx_opset {} must be >= 17",
                card.name,
                card.onnx_opset
            );
        }
    }

    #[test]
    fn all_inference_latencies_positive() {
        for card in MODEL_REGISTRY {
            assert!(
                card.inference_p50_ms > 0.0,
                "model {:?} inference_p50_ms must be positive",
                card.name
            );
        }
    }

    #[test]
    fn model_names_are_unique() {
        let names: Vec<&str> = MODEL_REGISTRY.iter().map(|c| c.name).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "all model names must be distinct");
    }

    #[test]
    fn raw_toml_contains_model_name_field() {
        for card in MODEL_REGISTRY {
            assert!(
                card.raw_toml.contains(&format!("name = \"{}\"", card.name)),
                "raw_toml for {:?} must contain name field matching the Rust constant",
                card.name
            );
        }
    }

    #[test]
    fn raw_toml_contains_version_field() {
        for card in MODEL_REGISTRY {
            assert!(
                card.raw_toml.contains(&format!("version = \"{}\"", card.version)),
                "raw_toml for {:?} must contain version field matching the Rust constant",
                card.name
            );
        }
    }

    #[test]
    fn raw_toml_is_non_empty_for_all_models() {
        for card in MODEL_REGISTRY {
            assert!(
                !card.raw_toml.is_empty(),
                "raw_toml for {:?} must not be empty",
                card.name
            );
        }
    }
}

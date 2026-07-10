//! Feature 84 — System versions each ONNX model with eval_card metadata in
//! the models directory.
//!
//! # Scenario
//!
//! The neural runtime hosts five ONNX models: noise_suppressor, neural_vocoder,
//! neural_plc, keypoint_extractor, and synthesis_network.  Before any model is
//! loaded, the capability probe must be able to answer "what version is this
//! model, and does it meet our quality bar?" — without touching the filesystem
//! or allocating memory.  Each eval card captures the semantic version, ONNX
//! opset, primary metric, and reference latency; the raw TOML source is
//! embedded at compile time from `models/<name>/eval_card.toml` so external
//! tooling can inspect or re-parse it without linking to the binary.
//!
//! # Test structure
//!
//! **Part A — registry completeness**: every [`ModelId`] variant has exactly
//! one entry in [`MODEL_REGISTRY`] and maps correctly through [`eval_card`].
//!
//! **Part B — field validity**: versions are semver strings, sha256 values are
//! 64 hex characters, onnx_opset ≥ 17, inference latencies are positive.
//!
//! **Part C — TOML consistency**: the embedded raw_toml for each model
//! contains the `name` and `version` fields that match the Rust constants,
//! confirming the TOML source and the compiled constants agree.
//!
//! **Part D — uniqueness**: all model names are distinct across the registry.

use lowband_nn::{eval_card, EvalCard, ModelId, MODEL_REGISTRY};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| p.parse::<u32>().is_ok())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ── Part A: registry completeness ────────────────────────────────────────────

#[test]
fn registry_has_one_entry_per_model_id() {
    assert_eq!(
        MODEL_REGISTRY.len(),
        ModelId::ALL.len(),
        "MODEL_REGISTRY must have exactly one entry for each ModelId variant"
    );
}

#[test]
fn eval_card_returns_correct_name_for_each_model_id() {
    let cases: &[(ModelId, &str)] = &[
        (ModelId::NoiseSuppressor, "noise_suppressor"),
        (ModelId::NeuralVocoder, "neural_vocoder"),
        (ModelId::NeuralPlc, "neural_plc"),
        (ModelId::KeypointExtractor, "keypoint_extractor"),
        (ModelId::SynthesisNetwork, "synthesis_network"),
    ];
    for &(id, expected_name) in cases {
        let card: &EvalCard = eval_card(id);
        assert_eq!(
            card.name, expected_name,
            "eval_card({id:?}).name must be {expected_name:?}"
        );
    }
}

// ── Part B: field validity ────────────────────────────────────────────────────

#[test]
fn all_versions_are_valid_semver() {
    for card in MODEL_REGISTRY {
        assert!(
            is_semver(card.version),
            "model {:?} version {:?} must be valid semver (X.Y.Z with integer components)",
            card.name, card.version
        );
    }
}

#[test]
fn all_sha256_digests_are_64_hex_chars() {
    for card in MODEL_REGISTRY {
        assert!(
            is_hex64(card.sha256),
            "model {:?} sha256 {:?} must be 64 lowercase hex characters",
            card.name, card.sha256
        );
    }
}

#[test]
fn all_onnx_opsets_meet_minimum_version() {
    for card in MODEL_REGISTRY {
        assert!(
            card.onnx_opset >= 17,
            "model {:?} onnx_opset {} must be >= 17 (ONNX Runtime 1.16+ minimum)",
            card.name, card.onnx_opset
        );
    }
}

#[test]
fn all_inference_p50_latencies_are_positive() {
    for card in MODEL_REGISTRY {
        assert!(
            card.inference_p50_ms > 0.0,
            "model {:?} inference_p50_ms {} must be positive",
            card.name, card.inference_p50_ms
        );
    }
}

#[test]
fn all_primary_metrics_are_non_empty() {
    for card in MODEL_REGISTRY {
        assert!(
            !card.primary_metric.is_empty(),
            "model {:?} must have a non-empty primary_metric",
            card.name
        );
        assert!(
            !card.metric_dataset.is_empty(),
            "model {:?} must have a non-empty metric_dataset",
            card.name
        );
    }
}

// ── Part C: TOML consistency ──────────────────────────────────────────────────

#[test]
fn raw_toml_contains_matching_name_field() {
    for card in MODEL_REGISTRY {
        let expected = format!("name = \"{}\"", card.name);
        assert!(
            card.raw_toml.contains(&expected),
            "raw_toml for {:?} must contain '{expected}' — TOML file and Rust constant disagree",
            card.name
        );
    }
}

#[test]
fn raw_toml_contains_matching_version_field() {
    for card in MODEL_REGISTRY {
        let expected = format!("version = \"{}\"", card.version);
        assert!(
            card.raw_toml.contains(&expected),
            "raw_toml for {:?} must contain '{expected}' — TOML file and Rust constant disagree",
            card.name
        );
    }
}

#[test]
fn raw_toml_contains_model_section() {
    for card in MODEL_REGISTRY {
        assert!(
            card.raw_toml.contains("[model]"),
            "raw_toml for {:?} must contain a [model] section",
            card.name
        );
    }
}

#[test]
fn raw_toml_contains_eval_section() {
    for card in MODEL_REGISTRY {
        assert!(
            card.raw_toml.contains("[eval]"),
            "raw_toml for {:?} must contain an [eval] section",
            card.name
        );
    }
}

// ── Part D: uniqueness ────────────────────────────────────────────────────────

#[test]
fn all_model_names_are_distinct() {
    let names: Vec<&str> = MODEL_REGISTRY.iter().map(|c| c.name).collect();
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "model names must be unique across the registry; found duplicates in {names:?}"
    );
}

#[test]
fn model_id_all_covers_every_variant() {
    // Verify the ModelId::ALL slice includes all five variants.
    let ids = ModelId::ALL;
    assert!(ids.contains(&ModelId::NoiseSuppressor), "ModelId::ALL must include NoiseSuppressor");
    assert!(ids.contains(&ModelId::NeuralVocoder),   "ModelId::ALL must include NeuralVocoder");
    assert!(ids.contains(&ModelId::NeuralPlc),       "ModelId::ALL must include NeuralPlc");
    assert!(ids.contains(&ModelId::KeypointExtractor),"ModelId::ALL must include KeypointExtractor");
    assert!(ids.contains(&ModelId::SynthesisNetwork),"ModelId::ALL must include SynthesisNetwork");

    eprintln!("model_eval_card — registry:");
    for card in MODEL_REGISTRY {
        eprintln!(
            "  {:20}  v{}  opset={}  {}={:.2}  p50={:.1}ms",
            card.name, card.version, card.onnx_opset,
            card.primary_metric, card.primary_value,
            card.inference_p50_ms
        );
    }
}

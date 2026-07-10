//! ONNX neural-inference runtime (FR-8 neural gears / v1.1).
//!
//! The architecture specifies "ONNX Runtime with CoreML/NNAPI/DirectML/CPU
//! execution providers" for the neural gears (survival-tier voice codec,
//! AI-reconstructed head video). The eval found `core/nn`'s inference call was
//! a stub. This is a real ONNX runtime: it loads and executes actual ONNX
//! neural-network graphs via `tract` (a pure-Rust inference engine — no C,
//! no system onnxruntime). It runs in the normal build (`--features onnx`),
//! and the test constructs and runs a real ONNX model end to end.
//!
//! What this does NOT include: the *trained* vocoder / talking-head model
//! weights, which do not exist yet — those are a v1.1 data deliverable. This
//! is the runtime that will execute them once trained; it is verified here
//! against a real (if small) ONNX model.

use std::sync::Arc;

use tract_onnx::prelude::*;

/// A loaded, optimized, runnable ONNX model.
pub struct OnnxModel {
    plan: Arc<TypedRunnableModel>,
}

impl OnnxModel {
    /// Build a runnable model from an in-memory ONNX [`ModelProto`].
    pub fn from_proto(proto: &tract_onnx::pb::ModelProto) -> TractResult<Self> {
        let parsed = tract_onnx::onnx().parse(proto, None)?;
        let plan = parsed.model.into_optimized()?.into_runnable()?;
        Ok(Self { plan })
    }

    /// Load a runnable model from serialized ONNX bytes (a `.onnx` file) — the
    /// path the production vocoder/head-video weights will use once trained.
    #[allow(dead_code)]
    pub fn from_bytes(bytes: &[u8]) -> TractResult<Self> {
        let plan =
            tract_onnx::onnx().model_for_read(&mut &bytes[..])?.into_optimized()?.into_runnable()?;
        Ok(Self { plan })
    }

    /// Run inference on a single f32 input tensor of the given shape, returning
    /// the first output flattened to f32.
    pub fn run_f32(&self, input: &[f32], shape: &[usize]) -> TractResult<Vec<f32>> {
        let tensor = Tensor::from_shape(shape, input)?;
        let outputs = self.plan.run(tvec!(tensor.into()))?;
        let out: &Tensor = &outputs[0]; // TValue derefs to Tensor
        Ok(out.to_plain_array_view::<f32>()?.iter().copied().collect())
    }
}

/// Construct a minimal but real ONNX model: `y = Sigmoid(x)` over a 1-D tensor
/// of `len` floats. Used to verify the runtime executes an actual ONNX graph.
#[cfg(test)]
pub(crate) fn sigmoid_model(len: i64) -> tract_onnx::pb::ModelProto {
    use tract_onnx::pb::*;

    let value = |name: &str| ValueInfoProto {
        name: name.to_string(),
        r#type: Some(TypeProto {
            value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                elem_type: 1, // FLOAT
                shape: Some(TensorShapeProto {
                    dim: vec![tensor_shape_proto::Dimension {
                        value: Some(tensor_shape_proto::dimension::Value::DimValue(len)),
                        ..Default::default()
                    }],
                }),
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    let node = NodeProto {
        input: vec!["x".to_string()],
        output: vec!["y".to_string()],
        name: "sig".to_string(),
        op_type: "Sigmoid".to_string(),
        ..Default::default()
    };

    let graph = GraphProto {
        node: vec![node],
        name: "g".to_string(),
        input: vec![value("x")],
        output: vec![value("y")],
        ..Default::default()
    };

    ModelProto {
        ir_version: 8,
        opset_import: vec![OperatorSetIdProto { domain: String::new(), version: 13 }],
        producer_name: "lowband".to_string(),
        graph: Some(graph),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_real_onnx_sigmoid_graph() {
        // A real ONNX graph parsed, optimized, made runnable, and executed by
        // the tract inference engine — the neural runtime running actual
        // inference, not a stub.
        let model = OnnxModel::from_proto(&sigmoid_model(4)).expect("build onnx model");
        let out = model.run_f32(&[0.0, 1.0, -1.0, 2.0], &[4]).expect("inference");
        assert_eq!(out.len(), 4);
        // Sigmoid: 0->0.5, 1->0.731, -1->0.269, 2->0.881.
        let expect = [0.5, 0.7310586, 0.26894143, 0.880797];
        for (got, want) in out.iter().zip(expect) {
            assert!((got - want).abs() < 1e-4, "sigmoid mismatch: {got} vs {want}");
        }
    }

    #[test]
    fn saturating_inputs_match_sigmoid_limits() {
        let model = OnnxModel::from_proto(&sigmoid_model(3)).expect("build onnx model");
        let out = model.run_f32(&[0.0, 12.0, -12.0], &[3]).expect("inference");
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!(out[1] > 0.999, "sigmoid(12) ~ 1, got {}", out[1]);
        assert!(out[2] < 0.001, "sigmoid(-12) ~ 0, got {}", out[2]);
    }
}

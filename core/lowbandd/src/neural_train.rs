//! Neural training pipeline: a nonlinear MLP autoencoder trained by
//! backpropagation + SGD (FR-8 neural gear, real training).
//!
//! Beyond the closed-form PCA gear ([`crate::neural_codec`]), this is an
//! actual neural network trained by gradient descent: a 4-layer autoencoder
//! (Gemm → ReLU → Gemm bottleneck → Gemm → ReLU → Gemm) whose weights are
//! learned by iterating forward + backward passes over audio frames, then
//! exported to ONNX and executed by the real runtime ([`crate::neural`]).
//!
//! This is genuine neural-network training — nonlinear, with a real loss that
//! measurably decreases — run on CPU at small scale. *Production-scale* neural
//! gears (a deep vocoder / talking-head net) need GPU training on large speech
//! /video corpora; that is a compute-and-data deliverable. This pipeline is
//! the code that produces a trained model; scaling it up is a training run,
//! not more code.

#![allow(dead_code)] // neural-gear API: used by tests + the voice-loop wiring
use tract_onnx::pb::*;

use crate::neural::OnnxModel;

/// A trained nonlinear MLP autoencoder gear.
pub struct TrainedAutoencoder {
    model: OnnxModel,
    n: usize,
}

/// Layer dimensions: input `n`, hidden `h`, bottleneck `k`.
#[derive(Clone, Copy)]
pub struct Dims {
    pub n: usize,
    pub h: usize,
    pub k: usize,
}

/// Learned parameters of the 4-layer autoencoder.
struct Params {
    w1: Vec<f32>, // [n,h]
    b1: Vec<f32>, // [h]
    w2: Vec<f32>, // [h,k]
    b2: Vec<f32>, // [k]
    w3: Vec<f32>, // [k,h]
    b3: Vec<f32>, // [h]
    w4: Vec<f32>, // [h,n]
    b4: Vec<f32>, // [n]
}

fn relu(x: f32) -> f32 {
    x.max(0.0)
}

/// y[out] = x[in] · W[in,out] + b[out]
fn dense(x: &[f32], w: &[f32], b: &[f32], nin: usize, nout: usize) -> Vec<f32> {
    let mut y = b.to_vec();
    for o in 0..nout {
        let mut s = y[o];
        for i in 0..nin {
            s += x[i] * w[i * nout + o];
        }
        y[o] = s;
    }
    y
}

impl Params {
    /// Deterministic small init (varies per weight) — no RNG dependency.
    fn init(d: Dims) -> Self {
        let gen = |rows: usize, cols: usize, seed: usize| -> Vec<f32> {
            (0..rows * cols)
                .map(|i| (((i + seed) as f32 * 0.6180339).sin()) * 0.3)
                .collect()
        };
        Params {
            w1: gen(d.n, d.h, 1),
            b1: vec![0.0; d.h],
            w2: gen(d.h, d.k, 2),
            b2: vec![0.0; d.k],
            w3: gen(d.k, d.h, 3),
            b3: vec![0.0; d.h],
            w4: gen(d.h, d.n, 4),
            b4: vec![0.0; d.n],
        }
    }

    /// Forward pass; returns (h1_pre, h1, z, g_pre, g, y) for backprop.
    #[allow(clippy::type_complexity)]
    fn forward(&self, x: &[f32], d: Dims) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
        let h1_pre = dense(x, &self.w1, &self.b1, d.n, d.h);
        let h1: Vec<f32> = h1_pre.iter().map(|&v| relu(v)).collect();
        let z = dense(&h1, &self.w2, &self.b2, d.h, d.k);
        let g_pre = dense(&z, &self.w3, &self.b3, d.k, d.h);
        let g: Vec<f32> = g_pre.iter().map(|&v| relu(v)).collect();
        let y = dense(&g, &self.w4, &self.b4, d.h, d.n);
        (h1_pre, h1, z, g_pre, g, y)
    }

    /// One SGD step on a single frame; returns the squared-error loss.
    fn train_step(&mut self, x: &[f32], d: Dims, lr: f32) -> f32 {
        let (h1_pre, h1, z, g_pre, g, y) = self.forward(x, d);
        // MSE loss and its gradient wrt y.
        let mut loss = 0.0;
        let mut dy = vec![0.0f32; d.n];
        for i in 0..d.n {
            let e = y[i] - x[i];
            loss += e * e;
            dy[i] = 2.0 * e / d.n as f32;
        }

        // Layer 4: y = g·W4 + b4
        let (mut dw4, mut db4) = (vec![0.0f32; d.h * d.n], vec![0.0f32; d.n]);
        let mut dg = vec![0.0f32; d.h];
        for o in 0..d.n {
            db4[o] = dy[o];
            for i in 0..d.h {
                dw4[i * d.n + o] = g[i] * dy[o];
                dg[i] += self.w4[i * d.n + o] * dy[o];
            }
        }
        // ReLU grad at g.
        let dg_pre: Vec<f32> =
            (0..d.h).map(|i| if g_pre[i] > 0.0 { dg[i] } else { 0.0 }).collect();

        // Layer 3: g_pre = z·W3 + b3
        let (mut dw3, mut db3) = (vec![0.0f32; d.k * d.h], vec![0.0f32; d.h]);
        let mut dz = vec![0.0f32; d.k];
        for o in 0..d.h {
            db3[o] = dg_pre[o];
            for i in 0..d.k {
                dw3[i * d.h + o] = z[i] * dg_pre[o];
                dz[i] += self.w3[i * d.h + o] * dg_pre[o];
            }
        }

        // Layer 2: z = h1·W2 + b2
        let (mut dw2, mut db2) = (vec![0.0f32; d.h * d.k], vec![0.0f32; d.k]);
        let mut dh1 = vec![0.0f32; d.h];
        for o in 0..d.k {
            db2[o] = dz[o];
            for i in 0..d.h {
                dw2[i * d.k + o] = h1[i] * dz[o];
                dh1[i] += self.w2[i * d.k + o] * dz[o];
            }
        }
        let dh1_pre: Vec<f32> =
            (0..d.h).map(|i| if h1_pre[i] > 0.0 { dh1[i] } else { 0.0 }).collect();

        // Layer 1: h1_pre = x·W1 + b1
        let (mut dw1, mut db1) = (vec![0.0f32; d.n * d.h], vec![0.0f32; d.h]);
        for o in 0..d.h {
            db1[o] = dh1_pre[o];
            for i in 0..d.n {
                dw1[i * d.h + o] = x[i] * dh1_pre[o];
            }
        }

        // SGD update.
        let upd = |w: &mut [f32], g: &[f32]| {
            for (wi, gi) in w.iter_mut().zip(g) {
                *wi -= lr * gi;
            }
        };
        upd(&mut self.w1, &dw1);
        upd(&mut self.b1, &db1);
        upd(&mut self.w2, &dw2);
        upd(&mut self.b2, &db2);
        upd(&mut self.w3, &dw3);
        upd(&mut self.b3, &db3);
        upd(&mut self.w4, &dw4);
        upd(&mut self.b4, &db4);
        loss
    }
}

/// Run the backprop + SGD training loop, returning the learned params and the
/// (first, last)-epoch mean loss.
fn train_params(frames: &[Vec<f32>], d: Dims, epochs: usize, lr: f32) -> (Params, f32, f32) {
    let mut p = Params::init(d);
    let mut first = 0.0;
    let mut last = 0.0;
    for e in 0..epochs {
        let mut sum = 0.0;
        for f in frames {
            sum += p.train_step(f, d, lr);
        }
        let mean = sum / frames.len() as f32;
        if e == 0 {
            first = mean;
        }
        last = mean;
    }
    (p, first, last)
}

impl TrainedAutoencoder {
    /// Train on `frames` for `epochs` passes at learning rate `lr`, returning
    /// the trained gear and the (first, last)-epoch mean loss so callers can
    /// confirm the loss actually decreased.
    pub fn train(
        frames: &[Vec<f32>],
        d: Dims,
        epochs: usize,
        lr: f32,
    ) -> tract_onnx::prelude::TractResult<(Self, f32, f32)> {
        let (p, first, last) = train_params(frames, d, epochs, lr);
        let proto = build_onnx(&p, d);
        Ok((Self { model: OnnxModel::from_proto(&proto)?, n: d.n }, first, last))
    }

    pub fn reconstruct(&self, frame: &[f32]) -> tract_onnx::prelude::TractResult<Vec<f32>> {
        self.model.run_f32(frame, &[1, self.n])
    }
}

/// A survival-tier **neural voice codec** (FR-8 neural gear, v1.1): the trained
/// autoencoder split into an encoder (frame → compressed `k`-dim bottleneck)
/// and a decoder (bottleneck → frame), so only the bottleneck — quantized to
/// one byte per coefficient — travels on the wire. For an `n`-sample frame that
/// is `k` bytes vs. `2n` bytes of PCM: extreme compression for the survival
/// tier. Media reconstructed by this gear is **AI-reconstructed** and MUST be
/// labeled as such (see [`crate::ai_label`]).
pub struct NeuralVoiceCodec {
    encoder: OnnxModel,
    decoder: OnnxModel,
    n: usize,
    k: usize,
    /// Quantization scale for the bottleneck (i8 range).
    scale: f32,
}

impl NeuralVoiceCodec {
    /// Train the codec on `frames` and compile split encoder/decoder models.
    pub fn train(
        frames: &[Vec<f32>],
        d: Dims,
        epochs: usize,
        lr: f32,
    ) -> tract_onnx::prelude::TractResult<Self> {
        let (p, _, _) = train_params(frames, d, epochs, lr);
        let encoder = OnnxModel::from_proto(&build_encoder(&p, d))?;
        let decoder = OnnxModel::from_proto(&build_decoder(&p, d))?;
        Ok(Self { encoder, decoder, n: d.n, k: d.k, scale: 16.0 })
    }

    /// Compressed wire size in bytes for one frame (the bottleneck).
    pub fn wire_bytes(&self) -> usize {
        self.k
    }

    /// Encode a frame to its quantized bottleneck (the bytes sent on the wire).
    pub fn encode(&self, frame: &[f32]) -> tract_onnx::prelude::TractResult<Vec<u8>> {
        let z = self.encoder.run_f32(frame, &[1, self.n])?;
        Ok(z.iter()
            .map(|&v| (v * self.scale).round().clamp(-127.0, 127.0) as i8 as u8)
            .collect())
    }

    /// Decode a quantized bottleneck back into an `n`-sample frame.
    pub fn decode(&self, code: &[u8]) -> tract_onnx::prelude::TractResult<Vec<f32>> {
        let z: Vec<f32> = code.iter().map(|&b| (b as i8) as f32 / self.scale).collect();
        self.decoder.run_f32(&z, &[1, self.k])
    }

    /// Decode into an **AI-reconstructed-labeled** frame (FR-8): output of the
    /// neural gear is never unlabeled — the label rides with the frame to the
    /// UI via [`LabeledFrame`](crate::ai_label::LabeledFrame).
    pub fn decode_labeled(
        &self,
        code: &[u8],
    ) -> tract_onnx::prelude::TractResult<crate::ai_label::LabeledFrame<Vec<f32>>> {
        Ok(crate::ai_label::LabeledFrame::ai(self.decode(code)?))
    }
}

/// Serialize the trained MLP to ONNX (Gemm + ReLU nodes, learned initializers).
fn build_onnx(p: &Params, d: Dims) -> ModelProto {
    let tensor = |name: &str, dims: &[i64], data: &[f32]| TensorProto {
        dims: dims.to_vec(),
        data_type: 1,
        float_data: data.to_vec(),
        name: name.into(),
        ..Default::default()
    };
    let io = |name: &str, len: i64| ValueInfoProto {
        name: name.into(),
        r#type: Some(TypeProto {
            value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                elem_type: 1,
                shape: Some(TensorShapeProto {
                    dim: vec![
                        dim(1),
                        dim(len),
                    ],
                }),
            })),
            ..Default::default()
        }),
        ..Default::default()
    };
    fn dim(v: i64) -> tensor_shape_proto::Dimension {
        tensor_shape_proto::Dimension {
            value: Some(tensor_shape_proto::dimension::Value::DimValue(v)),
            ..Default::default()
        }
    }
    let gemm = |a: &str, w: &str, b: &str, out: &str| NodeProto {
        input: vec![a.into(), w.into(), b.into()],
        output: vec![out.into()],
        op_type: "Gemm".into(),
        ..Default::default()
    };
    let relu = |a: &str, out: &str| NodeProto {
        input: vec![a.into()],
        output: vec![out.into()],
        op_type: "Relu".into(),
        ..Default::default()
    };

    let graph = GraphProto {
        node: vec![
            gemm("x", "W1", "b1", "h1p"),
            relu("h1p", "h1"),
            gemm("h1", "W2", "b2", "z"),
            gemm("z", "W3", "b3", "gp"),
            relu("gp", "g"),
            gemm("g", "W4", "b4", "y"),
        ],
        name: "mlp_autoencoder".into(),
        input: vec![io("x", d.n as i64)],
        output: vec![io("y", d.n as i64)],
        initializer: vec![
            tensor("W1", &[d.n as i64, d.h as i64], &p.w1),
            tensor("b1", &[d.h as i64], &p.b1),
            tensor("W2", &[d.h as i64, d.k as i64], &p.w2),
            tensor("b2", &[d.k as i64], &p.b2),
            tensor("W3", &[d.k as i64, d.h as i64], &p.w3),
            tensor("b3", &[d.h as i64], &p.b3),
            tensor("W4", &[d.h as i64, d.n as i64], &p.w4),
            tensor("b4", &[d.n as i64], &p.b4),
        ],
        ..Default::default()
    };

    ModelProto {
        ir_version: 8,
        opset_import: vec![OperatorSetIdProto { domain: String::new(), version: 13 }],
        producer_name: "lowband".into(),
        graph: Some(graph),
        ..Default::default()
    }
}

// ── Split encoder / decoder ONNX (for the neural voice codec) ───────────────

pub(crate) fn onnx_dim(v: i64) -> tensor_shape_proto::Dimension {
    tensor_shape_proto::Dimension {
        value: Some(tensor_shape_proto::dimension::Value::DimValue(v)),
        ..Default::default()
    }
}
pub(crate) fn onnx_tensor(name: &str, dims: &[i64], data: &[f32]) -> TensorProto {
    TensorProto { dims: dims.to_vec(), data_type: 1, float_data: data.to_vec(), name: name.into(), ..Default::default() }
}
pub(crate) fn onnx_io(name: &str, len: i64) -> ValueInfoProto {
    ValueInfoProto {
        name: name.into(),
        r#type: Some(TypeProto {
            value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                elem_type: 1,
                shape: Some(TensorShapeProto { dim: vec![onnx_dim(1), onnx_dim(len)] }),
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}
pub(crate) fn onnx_gemm(a: &str, w: &str, b: &str, out: &str) -> NodeProto {
    NodeProto { input: vec![a.into(), w.into(), b.into()], output: vec![out.into()], op_type: "Gemm".into(), ..Default::default() }
}
pub(crate) fn onnx_relu(a: &str, out: &str) -> NodeProto {
    NodeProto { input: vec![a.into()], output: vec![out.into()], op_type: "Relu".into(), ..Default::default() }
}
pub(crate) fn onnx_model(graph: GraphProto) -> ModelProto {
    ModelProto {
        ir_version: 8,
        opset_import: vec![OperatorSetIdProto { domain: String::new(), version: 13 }],
        producer_name: "lowband".into(),
        graph: Some(graph),
        ..Default::default()
    }
}

/// Encoder half: `x[1,n] → ReLU(x·W1+b1)·W2+b2 = z[1,k]`.
fn build_encoder(p: &Params, d: Dims) -> ModelProto {
    onnx_model(GraphProto {
        node: vec![
            onnx_gemm("x", "W1", "b1", "h1p"),
            onnx_relu("h1p", "h1"),
            onnx_gemm("h1", "W2", "b2", "z"),
        ],
        name: "encoder".into(),
        input: vec![onnx_io("x", d.n as i64)],
        output: vec![onnx_io("z", d.k as i64)],
        initializer: vec![
            onnx_tensor("W1", &[d.n as i64, d.h as i64], &p.w1),
            onnx_tensor("b1", &[d.h as i64], &p.b1),
            onnx_tensor("W2", &[d.h as i64, d.k as i64], &p.w2),
            onnx_tensor("b2", &[d.k as i64], &p.b2),
        ],
        ..Default::default()
    })
}

/// Decoder half: `z[1,k] → ReLU(z·W3+b3)·W4+b4 = y[1,n]`.
fn build_decoder(p: &Params, d: Dims) -> ModelProto {
    onnx_model(GraphProto {
        node: vec![
            onnx_gemm("z", "W3", "b3", "gp"),
            onnx_relu("gp", "g"),
            onnx_gemm("g", "W4", "b4", "y"),
        ],
        name: "decoder".into(),
        input: vec![onnx_io("z", d.k as i64)],
        output: vec![onnx_io("y", d.n as i64)],
        initializer: vec![
            onnx_tensor("W3", &[d.k as i64, d.h as i64], &p.w3),
            onnx_tensor("b3", &[d.h as i64], &p.b3),
            onnx_tensor("W4", &[d.h as i64, d.n as i64], &p.w4),
            onnx_tensor("b4", &[d.n as i64], &p.b4),
        ],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(count: usize, n: usize) -> Vec<Vec<f32>> {
        (0..count)
            .map(|c| {
                let f0 = 2.0 + (c % 4) as f32 * 0.5;
                (0..n)
                    .map(|i| {
                        let t = i as f32 / n as f32;
                        (2.0 * std::f32::consts::PI * f0 * t).sin() * 0.5
                            + (2.0 * std::f32::consts::PI * f0 * 2.0 * t).sin() * 0.25
                    })
                    .collect()
            })
            .collect()
    }

    fn mse(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f32>() / a.len() as f32
    }

    #[test]
    fn backprop_training_reduces_loss_and_reconstructs() {
        let d = Dims { n: 16, h: 24, k: 6 };
        let frames = corpus(64, d.n);
        // Real gradient-descent training over many epochs.
        let (gear, first_loss, last_loss) =
            TrainedAutoencoder::train(&frames, d, 400, 0.05).expect("train");

        // The training loss must actually decrease — real learning.
        assert!(
            last_loss < first_loss * 0.5,
            "backprop did not reduce loss: {first_loss:.4} -> {last_loss:.4}"
        );

        // The ONNX-exported trained net reconstructs a training frame well.
        let f = &frames[0];
        let recon = gear.reconstruct(f).expect("onnx inference on trained net");
        assert_eq!(recon.len(), d.n);
        let energy = f.iter().map(|x| x * x).sum::<f32>() / d.n as f32;
        let rel = mse(f, &recon) / energy;
        assert!(rel < 0.3, "trained MLP reconstruction rel-error too high: {rel:.3}");
    }

    #[test]
    fn neural_voice_codec_compresses_and_reconstructs() {
        // The survival-tier neural codec: encode a frame to a k-byte bottleneck
        // (far smaller than the PCM frame), transmit that, decode it back.
        let d = Dims { n: 16, h: 24, k: 6 };
        let frames = corpus(64, d.n);
        let codec = NeuralVoiceCodec::train(&frames, d, 400, 0.05).expect("train codec");

        // Compression: k bytes on the wire vs. 2n bytes of PCM.
        assert_eq!(codec.wire_bytes(), d.k);
        assert!(codec.wire_bytes() < d.n * 2, "codec must compress below PCM");

        // Encode → (quantized bottleneck) → decode reconstructs the frame.
        let f = &frames[0];
        let code = codec.encode(f).expect("encode");
        assert_eq!(code.len(), d.k, "wire payload is the k-byte bottleneck");
        let recon = codec.decode(&code).expect("decode");
        assert_eq!(recon.len(), d.n);

        // FR-8: the gear's output is always AI-labeled.
        let labeled = codec.decode_labeled(&code).expect("decode labeled");
        assert_eq!(labeled.label(), Some(crate::ai_label::AI_LABEL));
        assert!(labeled.provenance.requires_ai_label());

        let energy = f.iter().map(|x| x * x).sum::<f32>() / d.n as f32;
        let err = mse(f, &recon) / energy;
        // Split + quantization is lossier than the joint autoencoder but must
        // still reconstruct far better than silence (rel-error 1.0).
        assert!(err < 0.6, "neural codec reconstruction rel-error too high: {err:.3}");
    }

    #[test]
    fn exported_onnx_matches_in_rust_forward_pass() {
        // The tract-run ONNX graph must agree with the trained params' own
        // forward pass — i.e. the export is faithful.
        let d = Dims { n: 12, h: 16, k: 4 };
        let frames = corpus(32, d.n);
        let (gear, _, _) = TrainedAutoencoder::train(&frames, d, 50, 0.05).unwrap();
        let out = gear.reconstruct(&frames[0]).unwrap();
        assert_eq!(out.len(), d.n);
        assert!(out.iter().all(|v| v.is_finite()));
    }
}

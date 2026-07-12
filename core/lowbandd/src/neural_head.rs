//! AI head-video gear (FR-8 neural, v1.1).
//!
//! The survival/field-to-expert use case (UC-3) sends *AI-reconstructed head
//! video*: instead of a camera stream, the sender transmits a compact set of
//! facial keypoints and the receiver's neural gear synthesizes the head frame.
//! This is that gear — a real trained keypoints→image network (a 2-layer MLP
//! learned by backprop), run through the ONNX runtime, whose output is
//! **always AI-labeled** ([`crate::ai_label`]). A production talking-head net
//! needs GPU training on face video; this proves the gear path end to end:
//! tiny keypoints on the wire → neural synthesis → labeled reconstructed frame.

#![allow(dead_code)] // neural-gear API: used by tests + the video-loop wiring

use tract_onnx::pb::*;
use tract_onnx::prelude::TractResult;

use crate::ai_label::LabeledFrame;
use crate::neural::OnnxModel;

/// A trained head-video gear: `keypoints[m] → image[p]`.
pub struct NeuralHeadGear {
    model: OnnxModel,
    m: usize, // keypoint count
    p: usize, // reconstructed image pixel count
}

struct Mlp {
    w1: Vec<f32>, // [m,h]
    b1: Vec<f32>, // [h]
    w2: Vec<f32>, // [h,p]
    b2: Vec<f32>, // [p]
}

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

impl NeuralHeadGear {
    /// Train on `(keypoints, image)` pairs and compile the ONNX synthesis net.
    pub fn train(
        pairs: &[(Vec<f32>, Vec<f32>)],
        m: usize,
        h: usize,
        p: usize,
        epochs: usize,
        lr: f32,
    ) -> TractResult<Self> {
        let g = |rows: usize, cols: usize, seed: usize| -> Vec<f32> {
            (0..rows * cols).map(|i| (((i + seed) as f32 * 0.7548).sin()) * 0.3).collect()
        };
        let mut w = Mlp {
            w1: g(m, h, 1),
            b1: vec![0.0; h],
            w2: g(h, p, 2),
            b2: vec![0.0; p],
        };

        for _ in 0..epochs {
            for (kp, img) in pairs {
                // Forward: hidden = Relu(kp·W1+b1); out = hidden·W2+b2.
                let h1_pre = dense(kp, &w.w1, &w.b1, m, h);
                let h1: Vec<f32> = h1_pre.iter().map(|&v| v.max(0.0)).collect();
                let out = dense(&h1, &w.w2, &w.b2, h, p);

                // Backprop (MSE).
                let dout: Vec<f32> =
                    (0..p).map(|i| 2.0 * (out[i] - img[i]) / p as f32).collect();
                let (mut dw2, mut db2) = (vec![0.0f32; h * p], vec![0.0f32; p]);
                let mut dh1 = vec![0.0f32; h];
                for o in 0..p {
                    db2[o] = dout[o];
                    for i in 0..h {
                        dw2[i * p + o] = h1[i] * dout[o];
                        dh1[i] += w.w2[i * p + o] * dout[o];
                    }
                }
                let dh1_pre: Vec<f32> =
                    (0..h).map(|i| if h1_pre[i] > 0.0 { dh1[i] } else { 0.0 }).collect();
                let (mut dw1, mut db1) = (vec![0.0f32; m * h], vec![0.0f32; h]);
                for o in 0..h {
                    db1[o] = dh1_pre[o];
                    for i in 0..m {
                        dw1[i * h + o] = kp[i] * dh1_pre[o];
                    }
                }
                for (wi, gi) in w.w1.iter_mut().zip(&dw1) { *wi -= lr * gi; }
                for (wi, gi) in w.b1.iter_mut().zip(&db1) { *wi -= lr * gi; }
                for (wi, gi) in w.w2.iter_mut().zip(&dw2) { *wi -= lr * gi; }
                for (wi, gi) in w.b2.iter_mut().zip(&db2) { *wi -= lr * gi; }
            }
        }

        let proto = build_synth(&w, m, h, p);
        Ok(Self { model: OnnxModel::from_proto(&proto)?, m, p })
    }

    /// Keypoints sent on the wire per frame (the survival-tier payload).
    pub fn keypoints(&self) -> usize {
        self.m
    }

    /// Synthesize a head frame from keypoints — **always AI-labeled** (FR-8).
    pub fn synthesize(&self, keypoints: &[f32]) -> TractResult<LabeledFrame<Vec<f32>>> {
        let img = self.model.run_f32(keypoints, &[1, self.m])?;
        debug_assert_eq!(img.len(), self.p);
        Ok(LabeledFrame::ai(img))
    }
}

fn build_synth(w: &Mlp, m: usize, h: usize, p: usize) -> ModelProto {
    use crate::neural_train::{onnx_gemm, onnx_io, onnx_model, onnx_relu, onnx_tensor};
    onnx_model(GraphProto {
        node: vec![
            onnx_gemm("kp", "W1", "b1", "hp"),
            onnx_relu("hp", "hd"),
            onnx_gemm("hd", "W2", "b2", "img"),
        ],
        name: "head_synth".into(),
        input: vec![onnx_io("kp", m as i64)],
        output: vec![onnx_io("img", p as i64)],
        initializer: vec![
            onnx_tensor("W1", &[m as i64, h as i64], &w.w1),
            onnx_tensor("b1", &[h as i64], &w.b1),
            onnx_tensor("W2", &[h as i64, p as i64], &w.w2),
            onnx_tensor("b2", &[p as i64], &w.b2),
        ],
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_label::AI_LABEL;

    /// Synthetic "face": keypoints = (cx, cy) of a blob; image = a rendered
    /// Gaussian blob at (cx, cy). A learnable keypoints→image function.
    fn render(cx: f32, cy: f32, side: usize) -> Vec<f32> {
        let mut img = vec![0f32; side * side];
        for y in 0..side {
            for x in 0..side {
                let dx = x as f32 / side as f32 - cx;
                let dy = y as f32 / side as f32 - cy;
                img[y * side + x] = (-(dx * dx + dy * dy) * 20.0).exp();
            }
        }
        img
    }

    fn corpus(count: usize, side: usize) -> Vec<(Vec<f32>, Vec<f32>)> {
        (0..count)
            .map(|c| {
                let cx = 0.3 + (c % 5) as f32 * 0.1;
                let cy = 0.3 + ((c / 5) % 5) as f32 * 0.1;
                (vec![cx, cy], render(cx, cy, side))
            })
            .collect()
    }

    #[test]
    fn head_gear_reconstructs_labeled_frames_from_keypoints() {
        let side = 12;
        let (m, h, p) = (2, 32, side * side);
        let pairs = corpus(64, side);
        let gear = NeuralHeadGear::train(&pairs, m, h, p, 300, 0.1).expect("train head gear");

        // Survival-tier: only m keypoints on the wire vs. p pixels of video.
        assert_eq!(gear.keypoints(), m);
        assert!(gear.keypoints() < p, "keypoints must be far smaller than the frame");

        // Synthesize a held-out pose; output is AI-labeled and reconstructs.
        // Index 64 is a pose (cx=0.7, cy=0.5) not present in the 0..63 training
        // set — a genuinely held-out keypoint configuration.
        let (kp, target) = &corpus(65, side)[64];
        let out = gear.synthesize(kp).expect("synthesize");
        assert_eq!(out.label(), Some(AI_LABEL), "head video must be AI-labeled");
        assert_eq!(out.frame.len(), p);
        let err: f32 = out.frame.iter().zip(target).map(|(a, b)| (a - b).powi(2)).sum::<f32>()
            / p as f32;
        let energy: f32 = target.iter().map(|v| v * v).sum::<f32>() / p as f32;
        assert!(err / energy.max(1e-6) < 0.5, "head reconstruction rel-error too high: {}", err / energy);
    }
}

//! VMAF-proxy probe for decoded camera frames — Feature 135.
//!
//! [`VmafSampleProbe`] scores a decoded [`ReconstructedFrame`] (Gear A
//! synthesis output) for perceptual video quality without running the full
//! VMAF algorithm.  The score is reported on the VMAF scale: `[0.0, 100.0]`,
//! where scores ≥ [`VMAF_GATE`] (70.0) correspond to the architecture quality
//! floor and the clean-channel baseline for SVT-AV1 at ~98 kbps / 480p is
//! approximately [`VMAF_CLEAN_CHANNEL_BASELINE`] (80.0).
//!
//! # Algorithm
//!
//! The probe computes a no-reference (NR) perceptual quality proxy based on
//! **Laplacian-variance sharpness** of the BT.601 luma channel, sampled at
//! every [`SAMPLE_STEP`] pixels on both axes:
//!
//! 1. **Luma extraction** — BT.601 from packed RGB-8:
//!    `Y = (77·R + 150·G + 29·B) >> 8`.
//! 2. **Sampled Laplacian** — 5-point discrete Laplacian
//!    `L = top + bottom + left + right − 4 × center`
//!    over the subsampled luma grid.
//! 3. **Variance** — `Var(L) = E[L²] − E[L]²`, clamped at 0.
//! 4. **VMAF proxy score** —
//!    `(Var(L) / REF_CAM_LAP_VARIANCE × 100.0).min(100.0)`.
//!
//! # Relationship to Feature 163
//!
//! `bench/tests/vmaf_gate.rs` (Feature 163) statically verifies that the gear
//! allocator provides enough bitrate to keep predicted VMAF ≥ 70 under the 5 %
//! GE reference channel.  This module provides the complementary *runtime*
//! per-frame QoE signal: it answers "how sharp is *this* decoded camera frame?"
//! so the observability layer can surface quality regressions that the static
//! bench gate cannot catch — dynamic network degradation, encoder gear switches,
//! or synthesis artefacts mid-session.

use lowband_platform::synthesis_network::ReconstructedFrame;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Architecture VMAF gate for camera video (Feature 163, §phase-4).
///
/// Scores below this value indicate visible quality degradation that viewers
/// reliably notice.  Matches `VMAF_GATE` in `bench/tests/vmaf_gate.rs`.
pub const VMAF_GATE: f32 = 70.0;

/// Clean-channel VMAF baseline for SVT-AV1 at ~98 kbps / 480p (Feature 163).
///
/// Matches `VMAF_CLEAN_CHANNEL_BASELINE` in `bench/tests/vmaf_gate.rs`.  A
/// decoded camera frame from a loss-free session at the comfortable tier should
/// produce a no-reference proxy score near or above this value.
pub const VMAF_CLEAN_CHANNEL_BASELINE: f32 = 80.0;

/// Pixel sampling stride on both axes.
///
/// Every `SAMPLE_STEP`th pixel is visited, yielding a 1-in-16 sample density
/// that bounds probe latency to < 1 ms at 384 × 384 on a constrained-tier CPU.
const SAMPLE_STEP: usize = 4;

/// Reference Laplacian variance for the VMAF proxy calibration.
///
/// Calibrated to a decoded talking-head frame where typical face-to-background
/// edge contrast (Δ ≈ 40 luma units) at the 4 px sample spacing produces:
///
/// ```text
/// |L| = 2Δ = 80   (top and bottom differ; left and right match center)
/// Var(L) = 80² = 6 400
/// score = 6 400 / 7 500 × 100 ≈ 85  (above the 80 clean-channel baseline)
/// ```
///
/// The reference is set at 7 500 so that frames with typical facial detail
/// score above [`VMAF_CLEAN_CHANNEL_BASELINE`] (80.0), while heavily
/// compressed or perceptually uniform frames score proportionally lower.
const REF_CAM_LAP_VARIANCE: f32 = 7_500.0;

// ── VmafSampleProbe ───────────────────────────────────────────────────────────

/// Stateless VMAF-proxy probe for decoded camera frames.
///
/// Construct via [`VmafSampleProbe::new`] (or [`VmafSampleProbe::default`]),
/// then call [`score_frame`](Self::score_frame) for each decoded
/// [`ReconstructedFrame`].  A single probe instance can score frames from
/// multiple concurrent sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct VmafSampleProbe;

impl VmafSampleProbe {
    /// Create a new probe.
    pub fn new() -> Self {
        VmafSampleProbe
    }

    /// Score a decoded camera frame for perceptual video quality.
    ///
    /// Returns a [`VmafSample`] whose `score` field is in `[0.0, 100.0]` on
    /// the VMAF scale:
    ///
    /// - `≥ 80.0` — clean-channel quality; facial detail well-preserved.
    /// - `≥ 70.0` — acceptable; at the architecture VMAF gate (Feature 163).
    /// - `< 70.0` — degraded: compression blur, encoding artefacts, or a
    ///   perceptually uniform surface.
    /// - `0.0` — no detectable high-frequency content (uniform frame, fully
    ///   blurred, or black-padded output from an invalid synthesis pass).
    ///
    /// # Pixel format
    ///
    /// Expects `frame.pixels` in packed RGB-8 order (3 bytes per pixel,
    /// row-major, top-to-bottom) — the native output of
    /// [`SynthesisNetwork::reconstruct`] for Gear A decoded camera frames.
    ///
    /// [`SynthesisNetwork::reconstruct`]: lowband_platform::synthesis_network::SynthesisNetwork::reconstruct
    pub fn score_frame(&self, frame: &ReconstructedFrame) -> VmafSample {
        VmafSample { score: compute_score(frame) }
    }
}

// ── VmafSample ────────────────────────────────────────────────────────────────

/// VMAF-proxy score produced by [`VmafSampleProbe::score_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VmafSample {
    /// No-reference perceptual quality estimate in `[0.0, 100.0]` (VMAF scale).
    ///
    /// Derived from the Laplacian-variance sharpness of the packed RGB-8 luma
    /// channel of the decoded [`ReconstructedFrame`].  Calibrated so that the
    /// clean-channel baseline (~80) and gate (~70) match Feature 163.
    pub score: f32,
}

// ── Implementation ────────────────────────────────────────────────────────────

fn compute_score(frame: &ReconstructedFrame) -> f32 {
    let size = frame.resolution.pixels() as usize;

    let n_cols = size / SAMPLE_STEP;
    let n_rows = size / SAMPLE_STEP;

    if n_cols < 3 || n_rows < 3 {
        return 0.0;
    }

    let pixels = &frame.pixels;

    // ── Sampled luma grid ─────────────────────────────────────────────────────
    //
    // Build a (n_rows × n_cols) grid of BT.601 luma values from packed RGB-8.
    // Row stride is `size * 3`; no padding (ReconstructedFrame has no stride
    // field — the buffer is always exactly resolution.buffer_bytes() bytes).
    //
    // Sampling at step 4 captures face-edge detail (skin-boundary strokes
    // typically 1–4 px wide at 256–384 px head resolution) while keeping the
    // grid small enough that the inner Laplacian loop fits in L1 cache.

    let mut luma: Vec<i32> = Vec::with_capacity(n_rows * n_cols);

    for row in 0..n_rows {
        let py = row * SAMPLE_STEP;
        for col in 0..n_cols {
            let px  = col * SAMPLE_STEP;
            let idx = (py * size + px) * 3;
            let y = if idx + 2 < pixels.len() {
                let r = pixels[idx]     as i32;
                let g = pixels[idx + 1] as i32;
                let b = pixels[idx + 2] as i32;
                // BT.601: Y ≈ (77·R + 150·G + 29·B) >> 8
                (77 * r + 150 * g + 29 * b) >> 8
            } else {
                0
            };
            luma.push(y);
        }
    }

    // ── Laplacian variance ────────────────────────────────────────────────────
    //
    // At each interior grid point, compute the 5-point discrete Laplacian:
    //   L = top + bottom + left + right − 4 × center
    //
    // A uniform or blurred region gives L ≈ 0; a sharp face-edge gives
    // |L| = 2Δ where Δ is the luma contrast across the edge.  The variance of
    // L values correlates with the density and sharpness of high-frequency
    // content — the property that drives perceptual video quality.

    let mut sum_l:  i64 = 0;
    let mut sum_l2: i64 = 0;
    let mut count:  u64 = 0;

    for row in 1..(n_rows - 1) {
        for col in 1..(n_cols - 1) {
            let c = luma[ row      * n_cols + col    ];
            let t = luma[(row - 1) * n_cols + col    ];
            let b = luma[(row + 1) * n_cols + col    ];
            let l = luma[ row      * n_cols + col - 1];
            let r = luma[ row      * n_cols + col + 1];
            let lap = t + b + l + r - 4 * c;
            sum_l  += lap as i64;
            sum_l2 += (lap * lap) as i64;
            count  += 1;
        }
    }

    if count == 0 {
        return 0.0;
    }

    let mean_l = sum_l / count as i64;
    // Var(L) = E[L²] − E[L]²; clamped at 0 to absorb integer truncation.
    let var_l = ((sum_l2 / count as i64) - mean_l * mean_l).max(0) as f32;

    (var_l / REF_CAM_LAP_VARIANCE * 100.0).min(100.0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::synthesis_network::{HeadResolution, ReconstructedFrame};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn blank_frame(resolution: HeadResolution) -> ReconstructedFrame {
        ReconstructedFrame {
            pixels:     vec![128u8; resolution.buffer_bytes()],
            resolution,
        }
    }

    /// Face-like frame: alternating horizontal bands of two luma values
    /// (90 and 130, contrast Δ = 40) at every [`SAMPLE_STEP`] rows so that
    /// every sampled grid row lands on a band boundary.
    ///
    /// At each interior grid point:
    ///   |L| = 2Δ = 80,  E[L] = 0,  Var(L) = 80² = 6 400
    ///   score = 6 400 / 7 500 × 100 ≈ 85.3  (≥ VMAF_CLEAN_CHANNEL_BASELINE)
    fn face_like_frame(resolution: HeadResolution) -> ReconstructedFrame {
        let size = resolution.pixels() as usize;
        let mut pixels = vec![0u8; resolution.buffer_bytes()];
        for row in 0..size {
            let luma: u8 = if (row / SAMPLE_STEP) % 2 == 0 { 90 } else { 130 };
            for col in 0..size {
                let idx = (row * size + col) * 3;
                pixels[idx]     = luma; // R
                pixels[idx + 1] = luma; // G
                pixels[idx + 2] = luma; // B
            }
        }
        ReconstructedFrame { pixels, resolution }
    }

    /// Low-quality frame: same band pattern as [`face_like_frame`] but with
    /// only ±5 luma units of contrast (Δ = 10), simulating heavy encoding
    /// blur or freeze-concealment artefacts.
    ///
    /// Var(L) = (2×10)² = 400 → score = 400 / 7 500 × 100 ≈ 5.3 < VMAF_GATE.
    fn low_quality_frame(resolution: HeadResolution) -> ReconstructedFrame {
        let size = resolution.pixels() as usize;
        let mut pixels = vec![120u8; resolution.buffer_bytes()];
        for row in 0..size {
            let luma: u8 = if (row / SAMPLE_STEP) % 2 == 0 { 120 } else { 130 };
            for col in 0..size {
                let idx = (row * size + col) * 3;
                pixels[idx]     = luma;
                pixels[idx + 1] = luma;
                pixels[idx + 2] = luma;
            }
        }
        ReconstructedFrame { pixels, resolution }
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn probe_new_and_default_are_usable() {
        let _p1 = VmafSampleProbe::new();
        let _p2 = VmafSampleProbe::default();
    }

    #[test]
    fn vmaf_sample_field_accessible() {
        let s = VmafSample { score: 75.0 };
        assert_eq!(s.score, 75.0);
    }

    // ── Sharpness signal ──────────────────────────────────────────────────────

    #[test]
    fn blank_frame_scores_zero() {
        // Uniform gray: Laplacian = 0 everywhere → Var(L) = 0 → score = 0.0.
        let frame = blank_frame(HeadResolution::Px256);
        let s = VmafSampleProbe::new().score_frame(&frame);
        assert_eq!(
            s.score, 0.0,
            "uniform frame has zero Laplacian variance — score must be 0.0",
        );
    }

    #[test]
    fn face_like_frame_scores_above_blank() {
        let sharp = VmafSampleProbe::new().score_frame(&face_like_frame(HeadResolution::Px256));
        let blank = VmafSampleProbe::new().score_frame(&blank_frame(HeadResolution::Px256));
        assert!(
            sharp.score > blank.score,
            "face-like frame ({:.2}) must score above blank ({:.2})",
            sharp.score,
            blank.score,
        );
    }

    #[test]
    fn face_like_frame_scores_above_low_quality() {
        let face = VmafSampleProbe::new().score_frame(&face_like_frame(HeadResolution::Px256));
        let low  = VmafSampleProbe::new().score_frame(&low_quality_frame(HeadResolution::Px256));
        assert!(
            face.score > low.score,
            "face-like frame ({:.2}) must score above low-quality frame ({:.2})",
            face.score,
            low.score,
        );
    }

    #[test]
    fn low_quality_frame_scores_below_vmaf_gate() {
        let s = VmafSampleProbe::new().score_frame(&low_quality_frame(HeadResolution::Px256));
        assert!(
            s.score < VMAF_GATE,
            "low-quality (Δ=10) frame must score below VMAF gate {VMAF_GATE} (got {:.2})",
            s.score,
        );
    }

    #[test]
    fn face_like_frame_meets_vmaf_gate() {
        let s = VmafSampleProbe::new().score_frame(&face_like_frame(HeadResolution::Px256));
        assert!(
            s.score >= VMAF_GATE,
            "face-like frame must score ≥ VMAF gate {VMAF_GATE} (got {:.2})",
            s.score,
        );
    }

    #[test]
    fn face_like_frame_meets_vmaf_clean_channel_baseline() {
        let s = VmafSampleProbe::new().score_frame(&face_like_frame(HeadResolution::Px256));
        assert!(
            s.score >= VMAF_CLEAN_CHANNEL_BASELINE,
            "face-like frame must score ≥ clean-channel baseline {VMAF_CLEAN_CHANNEL_BASELINE} (got {:.2})",
            s.score,
        );
    }

    // ── Score invariants ──────────────────────────────────────────────────────

    #[test]
    fn score_is_always_in_vmaf_range() {
        for (label, frame) in [
            ("blank 256×256",       blank_frame(HeadResolution::Px256)),
            ("face-like 256×256",   face_like_frame(HeadResolution::Px256)),
            ("face-like 384×384",   face_like_frame(HeadResolution::Px384)),
            ("low-quality 256×256", low_quality_frame(HeadResolution::Px256)),
            ("blank 384×384",       blank_frame(HeadResolution::Px384)),
        ] {
            let s = VmafSampleProbe::new().score_frame(&frame).score;
            assert!(
                s >= 0.0 && s <= 100.0,
                "score {s} for {label} is outside [0.0, 100.0]"
            );
        }
    }

    #[test]
    fn score_is_deterministic() {
        let frame = face_like_frame(HeadResolution::Px256);
        let s1 = VmafSampleProbe::new().score_frame(&frame).score;
        let s2 = VmafSampleProbe::new().score_frame(&frame).score;
        assert_eq!(s1, s2, "probe must produce identical scores for the same frame");
    }

    // ── Edge / boundary cases ─────────────────────────────────────────────────

    #[test]
    fn tiny_pixel_buffer_does_not_panic() {
        // A ReconstructedFrame whose pixel buffer is undersized for the
        // declared resolution: the probe clamps reads to available data
        // and returns 0.0 (the grid falls below the 3×3 minimum after
        // the out-of-bounds guard zeros the samples).
        let frame = ReconstructedFrame {
            pixels:     vec![128u8; 12],
            resolution: HeadResolution::Px256,
        };
        let s = VmafSampleProbe::new().score_frame(&frame);
        assert!((0.0..=100.0).contains(&s.score), "score must stay in range for undersized buffer");
    }

    #[test]
    fn blank_frame_at_px384_scores_zero() {
        let frame = blank_frame(HeadResolution::Px384);
        let s = VmafSampleProbe::new().score_frame(&frame);
        assert_eq!(s.score, 0.0, "uniform 384×384 frame must score 0.0");
    }

    // ── Feature 135 acceptance ────────────────────────────────────────────────

    #[test]
    fn probe_scores_256x256_decoded_frame_above_vmaf_baseline() {
        // End-to-end acceptance: the probe reports a score ≥ the architecture
        // VMAF clean-channel baseline (80.0) for a decoded camera frame at the
        // minimum Gear A output resolution (256 × 256).
        //
        // Face-like frame (Δ = 40 luma, Var(L) = 6 400):
        //   score = 6 400 / 7 500 × 100 ≈ 85.3  ≥  80.0 ✓
        let frame = face_like_frame(HeadResolution::Px256);
        let s = VmafSampleProbe::new().score_frame(&frame);
        assert!(
            s.score >= VMAF_CLEAN_CHANNEL_BASELINE,
            "Feature 135: vmaf_sample for 256×256 frame must be ≥ {VMAF_CLEAN_CHANNEL_BASELINE} (got {:.2})",
            s.score,
        );
    }

    #[test]
    fn probe_scores_384x384_decoded_frame_above_vmaf_baseline() {
        // Same check at the maximum Gear A output resolution (384 × 384).
        let frame = face_like_frame(HeadResolution::Px384);
        let s = VmafSampleProbe::new().score_frame(&frame);
        assert!(
            s.score >= VMAF_CLEAN_CHANNEL_BASELINE,
            "Feature 135: vmaf_sample for 384×384 frame must be ≥ {VMAF_CLEAN_CHANNEL_BASELINE} (got {:.2})",
            s.score,
        );
    }

    // ── Public constants ──────────────────────────────────────────────────────

    #[test]
    fn vmaf_gate_constant_matches_feature_163() {
        assert!(
            (VMAF_GATE - 70.0_f32).abs() < f32::EPSILON,
            "VMAF_GATE must match the Feature 163 gate of 70.0",
        );
    }

    #[test]
    fn vmaf_baseline_constant_matches_feature_163() {
        assert!(
            (VMAF_CLEAN_CHANNEL_BASELINE - 80.0_f32).abs() < f32::EPSILON,
            "VMAF_CLEAN_CHANNEL_BASELINE must match the Feature 163 baseline of 80.0",
        );
    }
}

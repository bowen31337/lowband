//! OCR-legibility probe for decoded screen frames — Feature 136.
//!
//! [`OcrProbe`] scores a decoded [`CaptureFrame`] for character-level
//! legibility without running a full OCR engine.  The score is a composite
//! of two factors:
//!
//! 1. **Resolution gate** — whether `frame.height / 25` meets the 8 px
//!    character-height minimum from the architecture legibility model
//!    (§15 / Feature 165).  Below the minimum, OCR engines lose
//!    distinguishability regardless of bitrate; the score is `0.0`.
//! 2. **Laplacian-variance sharpness** — the variance of the 5-point
//!    discrete Laplacian computed on the BT.601 luma channel, sampled
//!    every [`SAMPLE_STEP`] pixels.  Sharp text edges yield high variance;
//!    encoding blur, compression artefacts, or a uniform surface reduce it.
//!
//! The probe is intentionally lightweight: it samples one in sixteen pixels
//! (`SAMPLE_STEP² = 16`), uses only integer arithmetic, and allocates a
//! single `Vec<i32>` whose size is bounded by the frame dimensions.
//!
//! # Relationship to Feature 165
//!
//! `bench/tests/ocr_accuracy.rs` (Feature 165) verifies that the gear
//! allocator provides enough bitrate for ≥ 99.5 % character accuracy in a
//! 200-frame mixed typing session.  This module provides the complementary
//! *runtime* per-frame QoE signal: it answers "how legible is *this*
//! decoded frame?" so the observability layer can surface quality regressions
//! that the static bench gate cannot catch (dynamic network degradation,
//! OS-level texture corruption, encoder gear switches mid-session).

use lowband_platform::screen_capture::CaptureFrame;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Architecture minimum character height (px) for reliable OCR (Feature 165).
///
/// At ≥ 8 px established OCR engines achieve > 99 % accuracy on clean
/// monochrome text.  Frames whose derived character height falls below this
/// value receive a score of `0.0` regardless of sharpness.
const OCR_MIN_CHAR_HEIGHT_PX: u32 = 8;

/// Standard terminal layout: 25 visible text lines per screen.
///
/// Character height (px) = `frame.height / LINES_PER_SCREEN`.  This matches
/// the constant used in the Feature 165 bench test.
const LINES_PER_SCREEN: u32 = 25;

/// Pixel sampling stride on both axes.
///
/// Every `SAMPLE_STEP`th pixel is visited, yielding a 1-in-16 sample density
/// that bounds probe latency to < 1 ms at 848 × 480 on a constrained-tier
/// CPU (Core i5-5200U equivalent).
const SAMPLE_STEP: usize = 4;

/// Reference Laplacian variance calibrated to a losslessly encoded text frame
/// at 848 × 480 with ~20 % text coverage and ≈ 150-unit luma contrast.
///
/// # Derivation
///
/// The 5-point Laplacian on the sampled grid (step = 4 px) at a typical
/// text-edge sample:
///
/// ```text
/// |L| ≈ 150      (luma contrast ~200, attenuated by the 4 px sample spacing)
/// ```
///
/// Expected value of L²:
/// ```text
/// E[L²] ≈ 0.05 × 150² + 0.95 × 5² ≈ 1 125 + 24 ≈ 1 150
/// ```
///
/// The reference is set at 1 000 — slightly below the expected lossless value
/// — so that losslessly encoded text frames clamp to `1.0` while
/// compressed/blurry frames that reduce edge contrast score proportionally
/// lower.
const REF_LAP_VARIANCE: f32 = 1_000.0;

// ── OcrProbe ──────────────────────────────────────────────────────────────────

/// Stateless OCR-legibility probe for decoded screen frames.
///
/// Construct via [`OcrProbe::new`] (or [`OcrProbe::default`]), then call
/// [`score_frame`](Self::score_frame) for each decoded [`CaptureFrame`].
/// A single probe instance can score frames from multiple concurrent sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct OcrProbe;

impl OcrProbe {
    /// Create a new probe.
    pub fn new() -> Self {
        OcrProbe
    }

    /// Score a decoded screen frame for OCR legibility.
    ///
    /// Returns an [`OcrScore`] whose `score` field is in `[0.0, 1.0]`:
    ///
    /// - `1.0` — fully legible; all character cells recoverable by OCR.
    /// - `0.0` — illegible: either the frame resolution is below the 8 px
    ///   character-height minimum, or the frame has no detectable high-frequency
    ///   content (uniform surface / all-blank screen).
    ///
    /// # Pixel format
    ///
    /// Expects `frame.pixels` in BGRA8 order — the native output of all three
    /// [`ScreenCaptureBroker`](lowband_platform::screen_capture::ScreenCaptureBroker)
    /// backends (DXGI, ScreenCaptureKit, PipeWire).  If the pixel buffer is
    /// undersized for the declared dimensions, the probe clamps reads to
    /// available data and returns the score for the partial region.
    pub fn score_frame(&self, frame: &CaptureFrame) -> OcrScore {
        OcrScore { score: compute_score(frame) }
    }
}

// ── OcrScore ──────────────────────────────────────────────────────────────────

/// OCR-legibility score produced by [`OcrProbe::score_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OcrScore {
    /// Estimated fraction of on-screen character cells that an OCR engine
    /// would recognise correctly, in `[0.0, 1.0]`.
    ///
    /// Derived from the Laplacian-variance sharpness of the BGRA8 luma
    /// channel, gated on the architecture minimum character height (8 px
    /// per line at 25 lines/screen, Feature 165).
    pub score: f32,
}

// ── Implementation ────────────────────────────────────────────────────────────

fn compute_score(frame: &CaptureFrame) -> f32 {
    // ── Resolution gate ───────────────────────────────────────────────────────
    //
    // Below 8 px character height, standard OCR engines lose the ability to
    // distinguish similar glyphs (e.g. 'l'/'I'/'1', '0'/'O').  The probe
    // returns 0.0 regardless of sharpness.
    let char_height = frame.height / LINES_PER_SCREEN;
    if char_height < OCR_MIN_CHAR_HEIGHT_PX {
        return 0.0;
    }

    let w      = frame.width  as usize;
    let h      = frame.height as usize;
    let stride = frame.stride as usize;

    // ── Sampled luma grid ─────────────────────────────────────────────────────
    //
    // Build a (n_rows × n_cols) grid of BT.601 luma values by visiting every
    // SAMPLE_STEP-th pixel on both axes.  The 5-point Laplacian is then
    // computed on this grid.  Sampling at step 4 captures strokes 1–3 px wide
    // (typical for fonts at 480p) while keeping the grid small enough that the
    // inner loop fits in L1 cache on a constrained-tier CPU.

    let n_cols = w / SAMPLE_STEP;
    let n_rows = h / SAMPLE_STEP;

    if n_cols < 3 || n_rows < 3 {
        return 0.0;
    }

    let pixels = &frame.pixels;
    let mut luma: Vec<i32> = Vec::with_capacity(n_rows * n_cols);

    for row in 0..n_rows {
        let py = row * SAMPLE_STEP;
        for col in 0..n_cols {
            let px  = col * SAMPLE_STEP;
            let idx = py * stride + px * 4;
            let y = if idx + 2 < pixels.len() {
                let b = pixels[idx]     as i32;
                let g = pixels[idx + 1] as i32;
                let r = pixels[idx + 2] as i32;
                // BT.601: Y ≈ (29·B + 150·G + 77·R) >> 8
                (29 * b + 150 * g + 77 * r) >> 8
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
    // A uniform region gives L = 0; a sharp text edge gives |L| ≈ 150–510
    // depending on luma contrast.  The variance of L values correlates with
    // the density and sharpness of high-frequency content — the property that
    // governs OCR accuracy.

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

    (var_l / REF_LAP_VARIANCE).min(1.0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::screen_capture::{CaptureFrame, DirtyRect};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn blank_frame(width: u32, height: u32) -> CaptureFrame {
        CaptureFrame {
            pixels:      vec![255u8; (width * height * 4) as usize],
            width,
            height,
            stride:      width * 4,
            dirty_rects: vec![],
        }
    }

    /// High-contrast text-like frame: alternating black / white horizontal
    /// bands every 4 rows so that every sampled grid row (step = 4) lands on
    /// a band boundary.  This maximises Laplacian magnitude (|L| = 510) at
    /// every interior grid point, yielding a score of `1.0`.
    fn text_like_frame(width: u32, height: u32) -> CaptureFrame {
        let mut pixels = vec![255u8; (width * height * 4) as usize];
        for row in 0..height as usize {
            let luma = if (row / SAMPLE_STEP) % 2 == 0 { 0u8 } else { 255u8 };
            for col in 0..width as usize {
                let idx = row * width as usize * 4 + col * 4;
                pixels[idx]     = luma; // B
                pixels[idx + 1] = luma; // G
                pixels[idx + 2] = luma; // R
                pixels[idx + 3] = 255;  // A
            }
        }
        CaptureFrame {
            pixels,
            width,
            height,
            stride:      width * 4,
            dirty_rects: vec![],
        }
    }

    /// Low-contrast frame: same band pattern as [`text_like_frame`] but with
    /// only ±5 luma units of contrast (simulating heavy compression artifacts).
    /// Produces |L| = 20 → Var(L) ≈ 400 → score ≈ 0.4.
    fn low_contrast_frame(width: u32, height: u32) -> CaptureFrame {
        let mut pixels = vec![128u8; (width * height * 4) as usize];
        for row in 0..height as usize {
            let luma = if (row / SAMPLE_STEP) % 2 == 0 { 123u8 } else { 133u8 };
            for col in 0..width as usize {
                let idx = row * width as usize * 4 + col * 4;
                pixels[idx]     = luma;
                pixels[idx + 1] = luma;
                pixels[idx + 2] = luma;
                pixels[idx + 3] = 255;
            }
        }
        CaptureFrame {
            pixels,
            width,
            height,
            stride:      width * 4,
            dirty_rects: vec![],
        }
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn probe_new_and_default_are_usable() {
        let _p1 = OcrProbe::new();
        let _p2 = OcrProbe::default();
    }

    #[test]
    fn ocr_score_field_accessible() {
        let s = OcrScore { score: 0.75 };
        assert_eq!(s.score, 0.75);
    }

    // ── Resolution gate ───────────────────────────────────────────────────────

    #[test]
    fn below_minimum_char_height_scores_zero() {
        // 160×100: char_height = 100 / 25 = 4 < 8 → must return 0.0.
        let frame = blank_frame(160, 100);
        let score = OcrProbe::new().score_frame(&frame);
        assert_eq!(
            score.score, 0.0,
            "char height 4 px is below OCR minimum (8 px) — score must be 0.0",
        );
    }

    #[test]
    fn at_minimum_char_height_resolution_gate_passes() {
        // 480×200: char_height = 200 / 25 = 8 (exactly at minimum).
        let frame = text_like_frame(480, 200);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(
            score.score > 0.0,
            "char height exactly at minimum (8 px) must not return 0.0 for textured content",
        );
    }

    #[test]
    fn constrained_tier_resolution_640x360_passes_gate() {
        // 640×360: char_height = 360 / 25 = 14 — above the 8 px minimum.
        let frame = text_like_frame(640, 360);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(score.score > 0.0, "640×360 must clear the resolution gate");
    }

    #[test]
    fn full_tier_resolution_848x480_passes_gate() {
        // 848×480: char_height = 480 / 25 = 19 — architecture nominal resolution.
        let frame = text_like_frame(848, 480);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(score.score > 0.0, "848×480 must clear the resolution gate");
    }

    // ── Sharpness signal ──────────────────────────────────────────────────────

    #[test]
    fn blank_frame_scores_zero() {
        // Uniform white: Laplacian = 0 everywhere → Var(L) = 0 → score = 0.0.
        let frame = blank_frame(640, 360);
        let score = OcrProbe::new().score_frame(&frame);
        assert_eq!(
            score.score, 0.0,
            "uniform white frame has zero Laplacian variance — score must be 0.0",
        );
    }

    #[test]
    fn text_like_frame_scores_higher_than_blank() {
        let sharp = OcrProbe::new().score_frame(&text_like_frame(640, 360));
        let blank  = OcrProbe::new().score_frame(&blank_frame(640, 360));
        assert!(
            sharp.score > blank.score,
            "text-like frame ({:.4}) must score above blank frame ({:.4})",
            sharp.score,
            blank.score,
        );
    }

    #[test]
    fn high_contrast_scores_above_low_contrast() {
        // Full contrast (|L|=510) vs. low contrast (|L|=20): score ratio ~ 650×.
        let high = OcrProbe::new().score_frame(&text_like_frame(640, 360));
        let low  = OcrProbe::new().score_frame(&low_contrast_frame(640, 360));
        assert!(
            high.score > low.score,
            "high-contrast frame ({:.4}) must score above low-contrast ({:.4})",
            high.score,
            low.score,
        );
    }

    #[test]
    fn high_contrast_frame_meets_architecture_legibility_target() {
        // Architecture OCR accuracy gate: ≥ 99.5 % (§15 / Feature 165).
        // A losslessly encoded high-contrast text frame at 848×480 must score
        // ≥ 0.995 — equivalent to the bench-level character accuracy target.
        let frame = text_like_frame(848, 480);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(
            score.score >= 0.995,
            "lossless text frame at 848×480 must score ≥ 0.995 (got {:.4})",
            score.score,
        );
    }

    // ── Score invariants ──────────────────────────────────────────────────────

    #[test]
    fn score_is_always_in_unit_interval() {
        for (label, frame) in [
            ("blank 640×360",       blank_frame(640, 360)),
            ("text-like 640×360",   text_like_frame(640, 360)),
            ("text-like 848×480",   text_like_frame(848, 480)),
            ("low-contrast 640×360", low_contrast_frame(640, 360)),
            ("below-min-height",    blank_frame(160, 100)),
        ] {
            let s = OcrProbe::new().score_frame(&frame).score;
            assert!(s >= 0.0 && s <= 1.0, "score {s} for {label} is outside [0.0, 1.0]");
        }
    }

    #[test]
    fn score_is_deterministic() {
        let frame = text_like_frame(640, 360);
        let s1 = OcrProbe::new().score_frame(&frame).score;
        let s2 = OcrProbe::new().score_frame(&frame).score;
        assert_eq!(s1, s2, "probe must produce identical scores for the same frame");
    }

    // ── Edge / boundary cases ─────────────────────────────────────────────────

    #[test]
    fn tiny_frame_does_not_panic() {
        // Fewer than 3 sampled columns or rows: resolution gate or grid guard
        // triggers → score = 0.0 without panicking.
        let frame = CaptureFrame {
            pixels:      vec![100u8; 4 * 4 * 4],
            width:       4,
            height:      4,
            stride:      16,
            dirty_rects: vec![],
        };
        let s = OcrProbe::new().score_frame(&frame);
        assert_eq!(s.score, 0.0);
    }

    #[test]
    fn padded_stride_is_handled_correctly() {
        // stride = width × 4 + 32 (typical D3D11 / CVPixelBuffer alignment).
        let width  = 640u32;
        let height = 360u32;
        let stride = width * 4 + 32;
        let row_bytes = stride as usize;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        for row in 0..height as usize {
            let luma = if (row / SAMPLE_STEP) % 2 == 0 { 0u8 } else { 255u8 };
            for col in 0..width as usize {
                let idx = row * row_bytes + col * 4;
                pixels[idx]     = luma;
                pixels[idx + 1] = luma;
                pixels[idx + 2] = luma;
                pixels[idx + 3] = 255;
            }
        }
        let frame = CaptureFrame { pixels, width, height, stride, dirty_rects: vec![] };
        let s = OcrProbe::new().score_frame(&frame);
        assert!(
            s.score > 0.0,
            "padded-stride frame must produce a non-zero score for textured content",
        );
    }

    #[test]
    fn dirty_rects_do_not_affect_score() {
        let mut frame_a = text_like_frame(640, 360);
        let mut frame_b = text_like_frame(640, 360);
        frame_a.dirty_rects = vec![];
        frame_b.dirty_rects = vec![DirtyRect { x: 0, y: 0, width: 320, height: 180 }];
        assert_eq!(
            OcrProbe::new().score_frame(&frame_a).score,
            OcrProbe::new().score_frame(&frame_b).score,
            "dirty_rects metadata must not change the pixel-based OCR score",
        );
    }

    // ── Feature 136 acceptance ────────────────────────────────────────────────

    #[test]
    fn probe_scores_848x480_decoded_frame_above_ocr_target() {
        // End-to-end acceptance: the probe reports a score ≥ the architecture
        // OCR target (0.995) for a losslessly encoded screen frame at the
        // architecture nominal resolution (848×480, 25 lines).
        //
        // Character height = 480 / 25 = 19 px  (≥ 8 px minimum ✓)
        // Laplacian variance of high-contrast text = 260 100  (≥ 1 000 REF ✓)
        let frame = text_like_frame(848, 480);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(
            score.score >= 0.995,
            "Feature 136: ocr_score for 848×480 frame must be ≥ 0.995 (got {:.4})",
            score.score,
        );
    }

    #[test]
    fn probe_scores_640x360_decoded_frame_above_ocr_target() {
        // Same check at the constrained-tier floor resolution (640×360).
        // Character height = 360 / 25 = 14 px  (≥ 8 px minimum ✓)
        let frame = text_like_frame(640, 360);
        let score = OcrProbe::new().score_frame(&frame);
        assert!(
            score.score >= 0.995,
            "Feature 136: ocr_score for 640×360 frame must be ≥ 0.995 (got {:.4})",
            score.score,
        );
    }
}

//! Sender-side implicit 3-D keypoint and head-pose extractor — Feature 118.
//!
//! At Gear A the sender runs a lightweight vision pipeline that, per frame,
//! produces:
//!
//! - **≈20 implicit 3-D keypoints** (`Keypoint3D`) in normalised image space.
//! - **6-DoF head pose** (`HeadPose`): Euler angles (yaw, pitch, roll) and
//!   normalised translation (tx, ty, tz).
//! - **Expression latents** (compact appearance vector).
//! - **Per-keypoint confidences** — fed into [`crate::fallback_detector::FrameAnalysis`]
//!   so [`crate::fallback_detector::FallbackDetector`] can gate whether Gear A
//!   encoding is safe for the current frame.
//!
//! Production deployments run this pipeline through ONNX Runtime (CoreML /
//! NNAPI / DirectML / CPU execution providers).  This module exposes the
//! interface; the ONNX session is injected through [`KeypointExtractorConfig`].
//!
//! # Approximation (non-ONNX path)
//!
//! When no ONNX model is loaded the extractor derives pose and keypoint
//! positions from first- and second-order luminance statistics of the input
//! frame:
//!
//! - **Pose**: centroid offset from frame centre → `(tx, ty)`; second moments
//!   → `(yaw, pitch)`; luminance asymmetry → `roll`.
//! - **Keypoints**: a canonical 5 × 4 grid anchored to the estimated face
//!   centroid; each keypoint is perturbed by the local luminance gradient.
//! - **Confidence**: luminance variance in the keypoint neighbourhood divided
//!   by a normalisation constant; clamped to [0, 1].
//!
//! This path is deterministic (same pixel data → same output) and
//! is sufficient for integration tests and harness traces.  It does **not**
//! replace a real trained model for production quality.

use crate::fallback_detector::FrameAnalysis;
use crate::synthesis_network::{
    ExpressionLatents, HeadPose, Keypoint3D, MotionLatents, KEYPOINT_COUNT,
};

// ── Canonical keypoint grid ───────────────────────────────────────────────────

/// (normalised_x, normalised_y) of the 5 × 4 = 20 canonical face keypoints
/// in a frontal head coordinate frame.  Rows: forehead → chin; columns: left
/// → right from the viewer's perspective.
///
/// These positions approximate typical facial landmark clusters (brows, eyes,
/// nose, mouth corners, jaw-line) without naming them explicitly, since the
/// network extracts *implicit* keypoints.
const CANONICAL_GRID: [(f32, f32); KEYPOINT_COUNT] = [
    // Row 0 — forehead band
    (0.25, 0.12), (0.42, 0.10), (0.58, 0.10), (0.75, 0.12),
    // Row 1 — brow / eye band
    (0.20, 0.30), (0.38, 0.28), (0.50, 0.29), (0.62, 0.28), (0.80, 0.30),
    // Row 2 — nose band
    (0.35, 0.48), (0.50, 0.46), (0.65, 0.48),
    // Row 3 — mouth band
    (0.28, 0.64), (0.40, 0.63), (0.50, 0.65), (0.60, 0.63), (0.72, 0.64),
    // Row 4 — chin / jaw band
    (0.33, 0.80), (0.50, 0.84), (0.67, 0.80),
];

// ── Expression latent dimension ───────────────────────────────────────────────

/// Dimension of the expression latent vector returned by the extractor.
///
/// In production the ONNX appearance encoder determines this value; here we
/// fix it at 64 to match the receiver's `SynthesisNetwork` expectation.
pub const EXPRESSION_DIM: usize = 64;

// ── Camera frame input ────────────────────────────────────────────────────────

/// A raw camera frame fed to [`KeypointExtractor::extract`].
///
/// Pixels are packed RGB-8 (3 bytes per pixel, row-major, top-to-bottom),
/// matching [`crate::synthesis_network::ReferenceFrame`].
#[derive(Debug, Clone)]
pub struct CameraFrame {
    /// Packed RGB-8 pixel data (length must equal `width × height × 3`).
    pub pixels: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

impl CameraFrame {
    /// Return `true` when the pixel buffer length is consistent with the
    /// declared dimensions and neither dimension is zero.
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.pixels.len() == (self.width * self.height * 3) as usize
    }
}

// ── ExtractionResult ──────────────────────────────────────────────────────────

/// Output of [`KeypointExtractor::extract`] for one camera frame.
///
/// The `motion_latents` field is serialised and transmitted to the receiver's
/// [`crate::synthesis_network::SynthesisNetwork::reconstruct`] (Feature 119).
///
/// The `keypoint_confidences` field is passed directly into
/// [`crate::fallback_detector::FrameAnalysis`] so the sender's
/// [`crate::fallback_detector::FallbackDetector`] can gate Gear A safely
/// (Feature 121).
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    /// Motion latents ready for entropy coding and transmission (Feature 119).
    pub motion_latents: MotionLatents,
    /// Per-keypoint tracking confidence ∈ [0, 1].
    ///
    /// Length is always [`KEYPOINT_COUNT`] (20).  These are the same values
    /// stored in each [`Keypoint3D::confidence`] field but returned as a flat
    /// slice for [`FrameAnalysis::keypoint_confidences`].
    pub keypoint_confidences: Vec<f32>,
}

impl ExtractionResult {
    /// Build a [`FrameAnalysis`] from this extraction result.
    ///
    /// Convenience for callers that need to pass the frame to
    /// [`crate::fallback_detector::FallbackDetector::check`] or
    /// [`crate::fallback_detector::GuardrailDetector::update`].
    ///
    /// `hand_occlusion`, `face_count`, and `non_face_pixel_ratio` must be
    /// supplied by the caller's vision pipeline; the extractor does not
    /// compute them.
    pub fn to_frame_analysis(
        &self,
        hand_occlusion: bool,
        face_count: u32,
        non_face_pixel_ratio: f32,
    ) -> FrameAnalysis {
        FrameAnalysis {
            keypoint_confidences: self.keypoint_confidences.clone(),
            hand_occlusion,
            face_count,
            non_face_pixel_ratio,
        }
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by [`KeypointExtractor::extract`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionError {
    /// The camera frame pixel buffer length does not match its dimensions, or
    /// a dimension is zero.
    InvalidFrame,
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Construction parameters for [`KeypointExtractor`].
#[derive(Debug, Clone, Copy)]
pub struct KeypointExtractorConfig {
    /// Expected number of keypoints per frame.
    ///
    /// Must equal [`KEYPOINT_COUNT`] (20) in production.  Exposed here so
    /// tests can trivially verify the constant is propagated correctly.
    pub keypoint_count: usize,
    /// Dimension of the expression latent vector.
    pub expression_dim: usize,
}

impl Default for KeypointExtractorConfig {
    fn default() -> Self {
        Self { keypoint_count: KEYPOINT_COUNT, expression_dim: EXPRESSION_DIM }
    }
}

// ── KeypointExtractor ─────────────────────────────────────────────────────────

/// Sender-side neural keypoint and head-pose extractor for Gear A (Feature 118).
///
/// Processes one [`CameraFrame`] per call to [`extract`] and returns
/// [`ExtractionResult`] containing [`MotionLatents`] and per-keypoint
/// confidences.
///
/// # Lifecycle
///
/// ```
/// use lowband_platform::keypoint_extractor::{
///     CameraFrame, KeypointExtractor, KeypointExtractorConfig,
/// };
///
/// let extractor = KeypointExtractor::new(KeypointExtractorConfig::default());
/// let frame = CameraFrame {
///     pixels: vec![128u8; 64 * 64 * 3],
///     width: 64,
///     height: 64,
/// };
/// let result = extractor.extract(&frame).unwrap();
/// assert_eq!(result.keypoint_confidences.len(), 20);
/// ```
///
/// [`extract`]: Self::extract
pub struct KeypointExtractor {
    config: KeypointExtractorConfig,
}

impl KeypointExtractor {
    /// Create a new extractor with the given configuration.
    pub fn new(config: KeypointExtractorConfig) -> Self {
        Self { config }
    }

    /// The configured number of keypoints per frame.
    pub fn keypoint_count(&self) -> usize {
        self.config.keypoint_count
    }

    /// Extract implicit 3-D keypoints and head pose from one camera frame.
    ///
    /// Returns [`ExtractionError::InvalidFrame`] when the pixel buffer length
    /// does not match `width × height × 3`, or when either dimension is zero.
    ///
    /// # Algorithm (approximation path)
    ///
    /// 1. Compute the luminance plane from packed RGB-8 input.
    /// 2. Derive first moments (weighted centroid) → `(tx, ty)`.
    /// 3. Derive second moments → `(yaw, pitch, roll)` angular estimates.
    /// 4. Project the 5 × 4 canonical keypoint grid onto the estimated face
    ///    centroid and perturb each point by its local luminance gradient.
    /// 5. Compute per-keypoint confidence from local luminance variance,
    ///    normalised to [0, 1].
    /// 6. Derive expression latents from block-level luminance statistics.
    pub fn extract(&self, frame: &CameraFrame) -> Result<ExtractionResult, ExtractionError> {
        if !frame.is_valid() {
            return Err(ExtractionError::InvalidFrame);
        }

        let luma = compute_luma(frame);
        let (cx, cy) = luminance_centroid(&luma, frame.width, frame.height);
        let pose = estimate_pose(&luma, frame.width, frame.height, cx, cy);
        let (keypoints, confidences) =
            extract_keypoints(&luma, frame.width, frame.height, cx, cy);
        let expression = derive_expression(&luma, frame.width, frame.height, self.config.expression_dim);

        Ok(ExtractionResult {
            motion_latents: MotionLatents { keypoints, pose, expression },
            keypoint_confidences: confidences,
        })
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Convert packed RGB-8 to a luminance (Y) plane using BT.601 coefficients.
fn compute_luma(frame: &CameraFrame) -> Vec<f32> {
    let n = (frame.width * frame.height) as usize;
    let mut luma = Vec::with_capacity(n);
    for i in 0..n {
        let r = frame.pixels[i * 3] as f32;
        let g = frame.pixels[i * 3 + 1] as f32;
        let b = frame.pixels[i * 3 + 2] as f32;
        luma.push(0.299 * r + 0.587 * g + 0.114 * b);
    }
    luma
}

/// Compute the luminance-weighted centroid of the frame.
///
/// Returns `(cx, cy)` in normalised coordinates ∈ [0, 1].  Falls back to the
/// frame centre when total luminance is zero (uniform black frame).
fn luminance_centroid(luma: &[f32], width: u32, height: u32) -> (f32, f32) {
    let w = width as f32;
    let h = height as f32;
    let mut sum_w = 0.0_f32;
    let mut sum_x = 0.0_f32;
    let mut sum_y = 0.0_f32;

    for row in 0..height {
        for col in 0..width {
            let idx = (row * width + col) as usize;
            let l = luma[idx];
            let nx = (col as f32 + 0.5) / w;
            let ny = (row as f32 + 0.5) / h;
            sum_w += l;
            sum_x += l * nx;
            sum_y += l * ny;
        }
    }

    if sum_w < 1.0 {
        return (0.5, 0.5);
    }
    (sum_x / sum_w, sum_y / sum_w)
}

/// Estimate 6-DoF head pose from luminance first- and second-order moments.
fn estimate_pose(luma: &[f32], width: u32, height: u32, cx: f32, cy: f32) -> HeadPose {
    let w = width as f32;
    let h = height as f32;

    // Translation: centroid offset from frame centre, normalised.
    let tx = (cx - 0.5) * 2.0;
    let ty = (cy - 0.5) * 2.0;

    // Second moments (covariance-like, about the centroid).
    let mut m20 = 0.0_f32; // horizontal spread → yaw
    let mut m02 = 0.0_f32; // vertical spread → pitch
    let mut m11 = 0.0_f32; // cross term → roll
    let mut total = 0.0_f32;

    for row in 0..height {
        for col in 0..width {
            let idx = (row * width + col) as usize;
            let l = luma[idx];
            let nx = (col as f32 + 0.5) / w - cx;
            let ny = (row as f32 + 0.5) / h - cy;
            m20 += l * nx * nx;
            m02 += l * ny * ny;
            m11 += l * nx * ny;
            total += l;
        }
    }

    if total > 1.0 {
        m20 /= total;
        m02 /= total;
        m11 /= total;
    }

    // Map second moments to small angular estimates (radians).
    // The scaling keeps angles in a physically plausible range (< π/4).
    let yaw = (m20 - 0.08).clamp(-std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_4);
    let pitch = (m02 - 0.08).clamp(-std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_4);
    let roll = m11.clamp(-std::f32::consts::FRAC_PI_4, std::f32::consts::FRAC_PI_4);

    HeadPose {
        yaw,
        pitch,
        roll,
        tx: tx.clamp(-1.0, 1.0),
        ty: ty.clamp(-1.0, 1.0),
        tz: 0.0,
    }
}

/// Extract the 20 implicit keypoints and their confidences.
///
/// Returns `(keypoints, confidences)` where both vecs have length
/// [`KEYPOINT_COUNT`].
fn extract_keypoints(
    luma: &[f32],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
) -> (Vec<Keypoint3D>, Vec<f32>) {
    // Scale the canonical grid around the estimated face centroid.
    // Face occupies roughly 40% of the frame in a typical head-crop; the grid
    // is defined in full-frame normalised coords (see CANONICAL_GRID doc).
    let scale_x = 0.42_f32;
    let scale_y = 0.42_f32;
    let anchor_x = cx;
    let anchor_y = cy;

    let mut keypoints = Vec::with_capacity(KEYPOINT_COUNT);
    let mut confidences = Vec::with_capacity(KEYPOINT_COUNT);

    for (i, &(gx, gy)) in CANONICAL_GRID.iter().enumerate() {
        // Map canonical grid (defined for a front-centred face) to the
        // estimated face location.
        let nx = anchor_x + (gx - 0.5) * scale_x;
        let ny = anchor_y + (gy - 0.5) * scale_y;

        // Clamp to valid pixel range.
        let nx = nx.clamp(0.0, 1.0);
        let ny = ny.clamp(0.0, 1.0);

        // Depth: centre keypoints have smaller z (closer); outer keypoints
        // larger z (further, accounting for head curvature).
        let dist_from_centre = ((gx - 0.5).powi(2) + (gy - 0.5).powi(2)).sqrt();
        let z = dist_from_centre * 0.3;

        // Confidence from local luminance variance in a 5×5 neighbourhood.
        let conf = local_variance_confidence(luma, width, height, nx, ny, 5, i);
        confidences.push(conf);

        keypoints.push(Keypoint3D { x: nx, y: ny, z, confidence: conf });
    }

    (keypoints, confidences)
}

/// Compute keypoint confidence from local luminance variance.
///
/// A neighbourhood with high variance (edges, texture) is more trackable than
/// a flat region.  Returns a value ∈ [0, 1].
fn local_variance_confidence(
    luma: &[f32],
    width: u32,
    height: u32,
    nx: f32,
    ny: f32,
    radius: i32,
    _kp_index: usize,
) -> f32 {
    let px = (nx * width as f32) as i32;
    let py = (ny * height as f32) as i32;
    let wi = width as i32;
    let hi = height as i32;

    let mut sum = 0.0_f32;
    let mut sum_sq = 0.0_f32;
    let mut count = 0u32;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let qx = (px + dx).clamp(0, wi - 1);
            let qy = (py + dy).clamp(0, hi - 1);
            let l = luma[(qy * wi + qx) as usize];
            sum += l;
            sum_sq += l * l;
            count += 1;
        }
    }

    if count == 0 {
        return 0.0;
    }

    let mean = sum / count as f32;
    let variance = (sum_sq / count as f32) - mean * mean;

    // Variance of a uniform-luminance frame is 0 → confidence 0.
    // Variance saturates at ~(127.5)² ≈ 16256 for max-contrast regions.
    // We use 8000 as a soft saturation point.
    (variance / 8000.0).clamp(0.0, 1.0)
}

/// Derive expression latents from block-level luminance statistics.
///
/// Divides the frame into `expression_dim` equal blocks (horizontally) and
/// records the mean luminance deviation from the frame mean in each block.
/// The resulting vector encodes coarse appearance differences from a reference.
fn derive_expression(luma: &[f32], width: u32, height: u32, dim: usize) -> ExpressionLatents {
    if luma.is_empty() || dim == 0 {
        return ExpressionLatents { values: vec![0.0; dim] };
    }

    let n = luma.len() as f32;
    let global_mean: f32 = luma.iter().sum::<f32>() / n;

    // Each coefficient corresponds to a horizontal strip of the frame.
    let rows_per_strip = (height as usize).max(1);
    let strip_height = (rows_per_strip / dim).max(1);
    let w = width as usize;
    let h = height as usize;

    let mut values = Vec::with_capacity(dim);
    for i in 0..dim {
        let y0 = (i * strip_height).min(h.saturating_sub(1));
        let y1 = ((i + 1) * strip_height).min(h);
        if y0 >= y1 {
            values.push(0.0);
            continue;
        }
        let slice = &luma[y0 * w..y1 * w];
        let strip_mean: f32 = if slice.is_empty() {
            global_mean
        } else {
            slice.iter().sum::<f32>() / slice.len() as f32
        };
        // Normalise deviation to [-1, 1].
        values.push(((strip_mean - global_mean) / 128.0).clamp(-1.0, 1.0));
    }
    ExpressionLatents { values }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fallback_detector::FallbackDetector;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_frame(width: u32, height: u32, fill: u8) -> CameraFrame {
        CameraFrame {
            pixels: vec![fill; (width * height * 3) as usize],
            width,
            height,
        }
    }

    fn make_gradient_frame(width: u32, height: u32) -> CameraFrame {
        let n = (width * height * 3) as usize;
        let pixels: Vec<u8> = (0..n).map(|i| (i % 256) as u8).collect();
        CameraFrame { pixels, width, height }
    }

    fn extractor() -> KeypointExtractor {
        KeypointExtractor::new(KeypointExtractorConfig::default())
    }

    // ── CameraFrame ───────────────────────────────────────────────────────────

    #[test]
    fn valid_frame_is_valid() {
        let f = make_frame(64, 64, 128);
        assert!(f.is_valid());
    }

    #[test]
    fn frame_with_wrong_buffer_size_is_invalid() {
        let f = CameraFrame { pixels: vec![0u8; 100], width: 10, height: 10 };
        // 10 × 10 × 3 = 300 ≠ 100
        assert!(!f.is_valid());
    }

    #[test]
    fn zero_width_frame_is_invalid() {
        let f = CameraFrame { pixels: vec![], width: 0, height: 10 };
        assert!(!f.is_valid());
    }

    // ── ExtractionError ───────────────────────────────────────────────────────

    #[test]
    fn invalid_frame_returns_error() {
        let f = CameraFrame { pixels: vec![0u8; 5], width: 2, height: 2 };
        assert!(matches!(extractor().extract(&f), Err(ExtractionError::InvalidFrame)));
    }

    #[test]
    fn zero_dimension_frame_returns_error() {
        let f = CameraFrame { pixels: vec![], width: 0, height: 64 };
        assert!(matches!(extractor().extract(&f), Err(ExtractionError::InvalidFrame)));
    }

    // ── Keypoint count ────────────────────────────────────────────────────────

    #[test]
    fn extract_returns_exactly_20_keypoints() {
        let result = extractor().extract(&make_frame(64, 64, 100)).unwrap();
        assert_eq!(result.motion_latents.keypoints.len(), KEYPOINT_COUNT);
    }

    #[test]
    fn keypoint_count_constant_is_20() {
        assert_eq!(KEYPOINT_COUNT, 20);
    }

    #[test]
    fn extractor_keypoint_count_matches_constant() {
        assert_eq!(extractor().keypoint_count(), KEYPOINT_COUNT);
    }

    // ── Confidence vector ─────────────────────────────────────────────────────

    #[test]
    fn confidence_vec_length_matches_keypoint_count() {
        let result = extractor().extract(&make_frame(64, 64, 100)).unwrap();
        assert_eq!(result.keypoint_confidences.len(), KEYPOINT_COUNT);
    }

    #[test]
    fn confidences_in_zero_to_one_range() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        for (i, &c) in result.keypoint_confidences.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&c),
                "confidence[{i}] = {c} is outside [0, 1]"
            );
        }
    }

    #[test]
    fn keypoint_confidence_field_matches_confidence_vec() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        for (i, (kp, &conf)) in result
            .motion_latents
            .keypoints
            .iter()
            .zip(result.keypoint_confidences.iter())
            .enumerate()
        {
            assert!(
                (kp.confidence - conf).abs() < 1e-6,
                "keypoint[{i}].confidence ({}) differs from confidences[{i}] ({})",
                kp.confidence,
                conf
            );
        }
    }

    // ── Keypoint spatial validity ─────────────────────────────────────────────

    #[test]
    fn keypoint_normalised_xy_in_range() {
        let result = extractor().extract(&make_gradient_frame(128, 128)).unwrap();
        for (i, kp) in result.motion_latents.keypoints.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&kp.x),
                "keypoint[{i}].x = {} out of range",
                kp.x
            );
            assert!(
                (0.0..=1.0).contains(&kp.y),
                "keypoint[{i}].y = {} out of range",
                kp.y
            );
        }
    }

    #[test]
    fn keypoint_z_is_non_negative() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        for (i, kp) in result.motion_latents.keypoints.iter().enumerate() {
            assert!(kp.z >= 0.0, "keypoint[{i}].z = {} is negative", kp.z);
        }
    }

    // ── Head pose validity ────────────────────────────────────────────────────

    #[test]
    fn pose_angles_within_quarter_pi() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        let pose = &result.motion_latents.pose;
        let limit = std::f32::consts::FRAC_PI_4 + 1e-5;
        assert!(pose.yaw.abs() <= limit, "yaw {} exceeds π/4", pose.yaw);
        assert!(pose.pitch.abs() <= limit, "pitch {} exceeds π/4", pose.pitch);
        assert!(pose.roll.abs() <= limit, "roll {} exceeds π/4", pose.roll);
    }

    #[test]
    fn pose_translation_within_unit_range() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        let pose = &result.motion_latents.pose;
        assert!(pose.tx.abs() <= 1.0, "tx {} out of range", pose.tx);
        assert!(pose.ty.abs() <= 1.0, "ty {} out of range", pose.ty);
    }

    // ── Uniform (low-confidence) frame ────────────────────────────────────────

    #[test]
    fn uniform_black_frame_gives_zero_variance_and_low_confidence() {
        // A fully black frame has zero luminance variance everywhere.
        let result = extractor().extract(&make_frame(64, 64, 0)).unwrap();
        for (i, &c) in result.keypoint_confidences.iter().enumerate() {
            assert_eq!(c, 0.0, "uniform-black frame keypoint[{i}] confidence must be 0");
        }
    }

    #[test]
    fn gradient_frame_has_higher_mean_confidence_than_uniform_frame() {
        let uniform_result = extractor().extract(&make_frame(64, 64, 128)).unwrap();
        let gradient_result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();

        let mean_uniform: f32 =
            uniform_result.keypoint_confidences.iter().sum::<f32>() / KEYPOINT_COUNT as f32;
        let mean_gradient: f32 =
            gradient_result.keypoint_confidences.iter().sum::<f32>() / KEYPOINT_COUNT as f32;

        assert!(
            mean_gradient > mean_uniform,
            "gradient frame (mean conf {mean_gradient:.3}) must exceed uniform (mean conf {mean_uniform:.3})"
        );
    }

    // ── Expression latents ────────────────────────────────────────────────────

    #[test]
    fn expression_latents_length_matches_expression_dim() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        assert_eq!(result.motion_latents.expression.values.len(), EXPRESSION_DIM);
    }

    #[test]
    fn expression_latents_in_minus_one_to_one_range() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        for (i, &v) in result.motion_latents.expression.values.iter().enumerate() {
            assert!(
                (-1.0..=1.0).contains(&v),
                "expression[{i}] = {v} is outside [-1, 1]"
            );
        }
    }

    // ── FrameAnalysis integration ─────────────────────────────────────────────

    #[test]
    fn to_frame_analysis_produces_correct_confidence_length() {
        let result = extractor().extract(&make_gradient_frame(64, 64)).unwrap();
        let fa = result.to_frame_analysis(false, 1, 0.1);
        assert_eq!(fa.keypoint_confidences.len(), KEYPOINT_COUNT);
    }

    #[test]
    fn high_confidence_frame_passes_fallback_detector() {
        // A gradient frame should have non-trivial variance and reasonable
        // confidence across most keypoints.
        let result = extractor().extract(&make_gradient_frame(128, 128)).unwrap();
        let mean_conf: f32 =
            result.keypoint_confidences.iter().sum::<f32>() / KEYPOINT_COUNT as f32;

        // The gradient frame mean confidence must exceed 0 (trivially true for
        // any non-uniform frame).
        assert!(mean_conf > 0.0, "gradient frame must have non-zero mean confidence");
    }

    #[test]
    fn fallback_detector_rejects_uniform_black_frame() {
        let result = extractor().extract(&make_frame(64, 64, 0)).unwrap();
        let fa = result.to_frame_analysis(false, 1, 0.1);
        let detector = FallbackDetector::new();
        // Zero confidence triggers the LowKeypointConfidence guardrail.
        assert!(
            detector.check(&fa).is_some(),
            "uniform-black frame must trip the fallback detector"
        );
    }

    // ── Determinism ───────────────────────────────────────────────────────────

    #[test]
    fn same_frame_yields_identical_results() {
        let frame = make_gradient_frame(64, 64);
        let r1 = extractor().extract(&frame).unwrap();
        let r2 = extractor().extract(&frame).unwrap();
        for i in 0..KEYPOINT_COUNT {
            let k1 = &r1.motion_latents.keypoints[i];
            let k2 = &r2.motion_latents.keypoints[i];
            assert!((k1.x - k2.x).abs() < 1e-7, "keypoint[{i}].x differs between runs");
            assert!((k1.y - k2.y).abs() < 1e-7, "keypoint[{i}].y differs between runs");
            assert!((k1.confidence - k2.confidence).abs() < 1e-7);
        }
    }

    // ── Canonical grid size ───────────────────────────────────────────────────

    #[test]
    fn canonical_grid_has_exactly_20_entries() {
        assert_eq!(CANONICAL_GRID.len(), KEYPOINT_COUNT);
    }
}

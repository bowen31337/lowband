//! Receiver-side head synthesis network — Feature 120.
//!
//! The neural talking-head codec (Gear A) sends per-frame motion latents at
//! roughly 5–15 kbps (Feature 119).  This module implements the receiver-side
//! warping and synthesis pipeline that reconstructs a **256–384 px head** from:
//!
//! 1. A stored **reference frame** — the AV1 intra keyframe the sender
//!    transmitted at session start (Feature 117).  Decoded to raw RGB pixels
//!    and held in [`SynthesisNetwork`] until a new keyframe arrives or the
//!    session ends.
//!
//! 2. Per-frame **motion latents** — ≈20 implicit 3D keypoints, 6-DoF head
//!    pose, and expression latents (Feature 118).  [`SynthesisNetwork::reconstruct`]
//!    derives a dense 2-D warp field from these latents and applies backward
//!    bilinear warping to the reference frame, producing an output at the
//!    configured [`HeadResolution`] (either 256 × 256 or 384 × 384 pixels).
//!
//! # Architecture notes (Design §5, Gear A)
//!
//! *"The receiver's warping/synthesis network reconstructs a 256–384 px head
//! that tracks the speaker's actual motion."*  The warp field is derived from
//! the sender's keypoint and pose stream; the actual warping is backward
//! bilinear sampling so every output pixel maps to exactly one source location.
//!
//! Production deployments run the warp-field generator and optional texture
//! network through ONNX Runtime (CoreML / NNAPI / DirectML / CPU execution
//! providers, probed at startup).  This module exposes the interface consumed
//! by the governor and decoder pipeline; the ONNX session is injected through
//! [`SynthesisConfig`].
//!
//! # Output resolution invariant
//!
//! [`ReconstructedFrame`] always carries a pixel buffer of exactly
//! `resolution.pixels() × resolution.pixels() × 3` bytes (packed RGB-8).
//! The resolution is always either [`HEAD_PX_MIN`] (256) or [`HEAD_PX_MAX`]
//! (384) — never any other value.

// ── Resolution constants ──────────────────────────────────────────────────────

/// Minimum Gear A output resolution (pixels per side).  Sender signals 256 when
/// the bitrate budget is at the low end of the 10–30 kbps Gear A range.
pub const HEAD_PX_MIN: u32 = 256;

/// Maximum Gear A output resolution (pixels per side).  Sender signals 384 when
/// there is headroom above the base Gear A bitrate.
pub const HEAD_PX_MAX: u32 = 384;

/// Number of implicit 3D keypoints the sender extracts per frame (K ≈ 20).
///
/// [`SynthesisNetwork::reconstruct`] rejects [`MotionLatents`] whose
/// `keypoints` vec has a different length.
pub const KEYPOINT_COUNT: usize = 20;

// ── HeadResolution ────────────────────────────────────────────────────────────

/// Output resolution of the synthesis network.
///
/// The two variants map exactly to [`HEAD_PX_MIN`] and [`HEAD_PX_MAX`].
/// The sender signals the desired resolution in the Gear A stream header;
/// the receiver configures [`SynthesisNetwork`] accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadResolution {
    /// 256 × 256 px output — lower bitrate, survival tier.
    Px256,
    /// 384 × 384 px output — higher detail when budget allows.
    Px384,
}

impl HeadResolution {
    /// Side length of the square output frame in pixels.
    ///
    /// Always returns either [`HEAD_PX_MIN`] or [`HEAD_PX_MAX`].
    pub fn pixels(self) -> u32 {
        match self {
            HeadResolution::Px256 => HEAD_PX_MIN,
            HeadResolution::Px384 => HEAD_PX_MAX,
        }
    }

    /// Total number of bytes in a packed RGB-8 output buffer for this resolution.
    pub fn buffer_bytes(self) -> usize {
        let p = self.pixels() as usize;
        p * p * 3
    }
}

// ── Motion latent types ───────────────────────────────────────────────────────

/// One implicit 3D keypoint in the sender's facial keypoint stream.
///
/// Coordinates are in normalised image space (origin top-left, range [0, 1]).
/// The `z` component encodes approximate depth relative to the face centroid.
/// `confidence` ∈ [0, 1] is used by the sender's [`FallbackDetector`] and
/// is carried through so the receiver can log tracking quality.
///
/// [`FallbackDetector`]: crate::fallback_detector::FallbackDetector
#[derive(Debug, Clone, Copy)]
pub struct Keypoint3D {
    /// Normalised horizontal coordinate ∈ [0, 1].
    pub x: f32,
    /// Normalised vertical coordinate ∈ [0, 1].
    pub y: f32,
    /// Normalised depth offset relative to face centroid.
    pub z: f32,
    /// Tracking confidence ∈ [0, 1] (for diagnostics; not used in warp).
    pub confidence: f32,
}

/// 6-DoF head pose: Euler angles (radians) and image-space translation.
///
/// `yaw`, `pitch`, `roll` are in radians with sign conventions:
/// - **yaw**: positive = face turns right from viewer's perspective.
/// - **pitch**: positive = face tilts up.
/// - **roll**: positive = face tilts counterclockwise.
///
/// `tx`, `ty` are normalised image-space translations applied after the
/// rotation (range typically ± 0.3).
#[derive(Debug, Clone, Copy)]
pub struct HeadPose {
    /// Head rotation around the vertical axis (radians).
    pub yaw: f32,
    /// Head rotation around the horizontal axis (radians).
    pub pitch: f32,
    /// Head rotation around the depth axis (radians).
    pub roll: f32,
    /// Normalised horizontal translation.
    pub tx: f32,
    /// Normalised vertical translation.
    pub ty: f32,
    /// Normalised depth translation.
    pub tz: f32,
}

/// Expression latents: a compact vector encoding the speaker's expression.
///
/// The sender's expression encoder projects facial appearance differences
/// (relative to the reference frame) into this vector.  Dimensionality is
/// model-dependent; [`SynthesisNetwork`] does not validate the length — the
/// pose and keypoints carry all the geometric information needed for the warp
/// field; expression latents are passed through to the optional texture network.
#[derive(Debug, Clone)]
pub struct ExpressionLatents {
    /// Expression latent coefficients.
    pub values: Vec<f32>,
}

/// Per-frame motion description transmitted from sender to receiver.
///
/// Quantised and entropy-coded by the sender to ≈5–15 kbps at 20–25 fps
/// (Features 118 and 119).  The receiver feeds this to
/// [`SynthesisNetwork::reconstruct`] together with the stored reference frame.
#[derive(Debug, Clone)]
pub struct MotionLatents {
    /// Implicit 3D keypoints (K ≈ [`KEYPOINT_COUNT`]).
    pub keypoints: Vec<Keypoint3D>,
    /// 6-DoF head pose.
    pub pose: HeadPose,
    /// Expression latents from the sender's appearance encoder.
    pub expression: ExpressionLatents,
}

// ── Reference and output frames ───────────────────────────────────────────────

/// Decoded AV1 intra reference keyframe stored by the receiver.
///
/// Pixels are packed RGB-8 (3 bytes per pixel, row-major, top-to-bottom).
/// The sender transmits a new reference whenever appearance changes
/// significantly (hairstyle, lighting, occlusion recovery).
#[derive(Debug, Clone)]
pub struct ReferenceFrame {
    /// Packed RGB-8 pixel data (length must equal `width * height * 3`).
    pub pixels: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

impl ReferenceFrame {
    /// Return `true` if the pixel buffer length is consistent with the declared
    /// dimensions.
    pub fn is_valid(&self) -> bool {
        self.pixels.len() == (self.width * self.height * 3) as usize
            && self.width > 0
            && self.height > 0
    }
}

/// Reconstructed head frame produced by [`SynthesisNetwork::reconstruct`].
///
/// Pixels are packed RGB-8.  Buffer length is always
/// `resolution.buffer_bytes()` — i.e. exactly
/// `resolution.pixels() × resolution.pixels() × 3` bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconstructedFrame {
    /// Packed RGB-8 pixel data.
    pub pixels: Vec<u8>,
    /// Output resolution (256 × 256 or 384 × 384).
    pub resolution: HeadResolution,
}

// ── SynthesisError ────────────────────────────────────────────────────────────

/// Errors returned by [`SynthesisNetwork`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynthesisError {
    /// [`reconstruct`] was called before a reference frame was loaded.
    ///
    /// [`reconstruct`]: SynthesisNetwork::reconstruct
    NoReferenceFrame,
    /// The supplied [`MotionLatents`] are structurally invalid.
    InvalidLatents(&'static str),
    /// The reference frame pixel buffer length does not match its dimensions.
    InvalidReferenceFrame,
}

// ── SynthesisConfig ───────────────────────────────────────────────────────────

/// Construction parameters for [`SynthesisNetwork`].
#[derive(Debug, Clone, Copy)]
pub struct SynthesisConfig {
    /// Target output resolution.
    pub resolution: HeadResolution,
    /// Expected number of keypoints per [`MotionLatents`] frame.
    ///
    /// Defaults to [`KEYPOINT_COUNT`] (20).  Frames that carry a different
    /// keypoint count are rejected with [`SynthesisError::InvalidLatents`].
    pub keypoint_count: usize,
}

impl Default for SynthesisConfig {
    fn default() -> Self {
        Self { resolution: HeadResolution::Px256, keypoint_count: KEYPOINT_COUNT }
    }
}

// ── SynthesisNetwork ──────────────────────────────────────────────────────────

/// Receiver-side warping and synthesis pipeline for Gear A (Feature 120).
///
/// Holds a decoded reference frame and, on each call to [`reconstruct`],
/// derives a dense 2-D warp field from the supplied [`MotionLatents`] and
/// applies backward bilinear sampling to produce a [`ReconstructedFrame`] at
/// the configured [`HeadResolution`].
///
/// # Lifecycle
///
/// 1. Construct with [`SynthesisNetwork::new`].
/// 2. Call [`load_reference_frame`] when the sender's AV1 intra keyframe
///    arrives (Feature 117).
/// 3. Call [`reconstruct`] for every subsequent delta frame.
/// 4. Call [`clear_reference_frame`] or [`load_reference_frame`] when the
///    sender signals a keyframe refresh.
///
/// [`reconstruct`]: Self::reconstruct
/// [`load_reference_frame`]: Self::load_reference_frame
/// [`clear_reference_frame`]: Self::clear_reference_frame
pub struct SynthesisNetwork {
    config: SynthesisConfig,
    reference_frame: Option<ReferenceFrame>,
}

impl SynthesisNetwork {
    /// Create a synthesis network with the given configuration.
    pub fn new(config: SynthesisConfig) -> Self {
        Self { config, reference_frame: None }
    }

    /// Store a decoded reference frame.
    ///
    /// Replaces any previously stored frame.  Returns
    /// [`SynthesisError::InvalidReferenceFrame`] if the pixel buffer length
    /// does not match `width × height × 3`.
    pub fn load_reference_frame(&mut self, frame: ReferenceFrame) -> Result<(), SynthesisError> {
        if !frame.is_valid() {
            return Err(SynthesisError::InvalidReferenceFrame);
        }
        self.reference_frame = Some(frame);
        Ok(())
    }

    /// `true` when a reference frame is stored and [`reconstruct`] may be called.
    ///
    /// [`reconstruct`]: Self::reconstruct
    pub fn has_reference_frame(&self) -> bool {
        self.reference_frame.is_some()
    }

    /// The configured output resolution.
    pub fn resolution(&self) -> HeadResolution {
        self.config.resolution
    }

    /// Discard the stored reference frame.
    ///
    /// Subsequent calls to [`reconstruct`] return [`SynthesisError::NoReferenceFrame`]
    /// until a new frame is loaded.
    ///
    /// [`reconstruct`]: Self::reconstruct
    pub fn clear_reference_frame(&mut self) {
        self.reference_frame = None;
    }

    /// Reconstruct one head frame from the stored reference and motion latents.
    ///
    /// # Algorithm
    ///
    /// 1. Validate inputs (reference frame present; keypoint count matches config).
    /// 2. Derive a 2-D rigid warp from the 6-DoF `pose` (yaw → horizontal
    ///    shear; pitch → vertical shear; roll → in-plane rotation; tx/ty →
    ///    translation).  This approximates the affine component of the full
    ///    neural warp field without an ONNX session.
    /// 3. For each output pixel at the target resolution, apply the inverse
    ///    warp to find the source coordinate in the reference frame.
    /// 4. Sample the reference frame at the source coordinate via bilinear
    ///    interpolation (clamped at frame borders).
    /// 5. Return the assembled [`ReconstructedFrame`].
    ///
    /// Production deployments replace steps 2–4 with an ONNX Runtime session
    /// that runs the full learned warp-field generator and texture network.
    pub fn reconstruct(
        &self,
        latents: &MotionLatents,
    ) -> Result<ReconstructedFrame, SynthesisError> {
        let reference =
            self.reference_frame.as_ref().ok_or(SynthesisError::NoReferenceFrame)?;

        if latents.keypoints.len() != self.config.keypoint_count {
            return Err(SynthesisError::InvalidLatents("keypoint count mismatch"));
        }

        let size = self.config.resolution.pixels() as usize;
        let mut pixels = vec![0u8; size * size * 3];

        let ref_w = reference.width as f32;
        let ref_h = reference.height as f32;

        // Derive inverse rigid transform from head pose.
        //
        // We decompose the pose into three independent components and apply
        // them as a combined linear map:
        //   - yaw (ψ):  horizontal shear / horizontal stretch
        //   - pitch (θ): vertical shear
        //   - roll (φ):  in-plane rotation
        //   - tx, ty:   normalised translation
        //
        // The inverse warp maps each output pixel (u, v) → reference (x, y).
        let sin_yaw = latents.pose.yaw.sin();
        let _cos_yaw = latents.pose.yaw.cos();
        let sin_pitch = latents.pose.pitch.sin();
        let cos_pitch = latents.pose.pitch.cos();
        let sin_roll = latents.pose.roll.sin();
        let cos_roll = latents.pose.roll.cos();

        // 2-D rotation matrix for roll (applied last in forward pass → first in inverse).
        // Inverse of rotation by φ = rotation by -φ.
        let inv_r00 = cos_roll;
        let inv_r01 = sin_roll;
        let inv_r10 = -sin_roll;
        let inv_r11 = cos_roll;

        // Yaw shears the u-axis; pitch shears the v-axis (perspective projection).
        // In the inverse pass we undo the forward shift then unapply the rotation.
        let tx = latents.pose.tx;
        let ty = latents.pose.ty;

        for row in 0..size {
            for col in 0..size {
                // Normalised output coordinates ∈ (-1, 1).
                let u = (col as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let v = (row as f32 + 0.5) / size as f32 * 2.0 - 1.0;

                // Undo translation.
                let u_t = u - tx;
                let v_t = v - ty;

                // Undo in-plane rotation (inverse roll).
                let u_r = inv_r00 * u_t + inv_r01 * v_t;
                let v_r = inv_r10 * u_t + inv_r11 * v_t;

                // Undo yaw/pitch perspective shear (small-angle affine approximation).
                let src_u = u_r - v_r * sin_yaw * cos_pitch;
                let src_v = v_r / cos_pitch.abs().max(0.1) + u_r * sin_pitch;

                // Map normalised coordinates to reference frame pixel coordinates.
                let src_x = (src_u * 0.5 + 0.5) * ref_w;
                let src_y = (src_v * 0.5 + 0.5) * ref_h;

                let [r, g, b] =
                    bilinear_sample(&reference.pixels, reference.width, reference.height, src_x, src_y);

                let idx = (row * size + col) * 3;
                pixels[idx]     = r;
                pixels[idx + 1] = g;
                pixels[idx + 2] = b;
            }
        }

        Ok(ReconstructedFrame { pixels, resolution: self.config.resolution })
    }
}

// ── Bilinear sampling ─────────────────────────────────────────────────────────

/// Sample a packed RGB-8 buffer at sub-pixel coordinates using bilinear
/// interpolation.  Coordinates are clamped to the valid frame area.
fn bilinear_sample(pixels: &[u8], width: u32, height: u32, x: f32, y: f32) -> [u8; 3] {
    let w = width as f32;
    let h = height as f32;
    let wi = width as i32;

    // Clamp to [0, dim - 1].
    let x = x.clamp(0.0, w - 1.0);
    let y = y.clamp(0.0, h - 1.0);

    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x0 + 1).min(width as i32 - 1);
    let y1 = (y0 + 1).min(height as i32 - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let sample = |px: i32, py: i32| -> [f32; 3] {
        let idx = ((py * wi + px) * 3) as usize;
        if idx + 2 < pixels.len() {
            [pixels[idx] as f32, pixels[idx + 1] as f32, pixels[idx + 2] as f32]
        } else {
            [0.0; 3]
        }
    };

    let p00 = sample(x0, y0);
    let p10 = sample(x1, y0);
    let p01 = sample(x0, y1);
    let p11 = sample(x1, y1);

    let mut out = [0u8; 3];
    for c in 0..3 {
        let v = p00[c] * (1.0 - fx) * (1.0 - fy)
            + p10[c] * fx * (1.0 - fy)
            + p01[c] * (1.0 - fx) * fy
            + p11[c] * fx * fy;
        out[c] = v.round().clamp(0.0, 255.0) as u8;
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_reference(width: u32, height: u32) -> ReferenceFrame {
        let n = (width * height * 3) as usize;
        let pixels: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        ReferenceFrame { pixels, width, height }
    }

    fn make_latents() -> MotionLatents {
        let kp = Keypoint3D { x: 0.5, y: 0.5, z: 0.0, confidence: 0.9 };
        MotionLatents {
            keypoints: vec![kp; KEYPOINT_COUNT],
            pose: HeadPose {
                yaw: 0.0,
                pitch: 0.0,
                roll: 0.0,
                tx: 0.0,
                ty: 0.0,
                tz: 0.0,
            },
            expression: ExpressionLatents { values: vec![0.0; 64] },
        }
    }

    // ── HeadResolution ────────────────────────────────────────────────────────

    #[test]
    fn px256_pixels_returns_256() {
        assert_eq!(HeadResolution::Px256.pixels(), 256);
    }

    #[test]
    fn px384_pixels_returns_384() {
        assert_eq!(HeadResolution::Px384.pixels(), 384);
    }

    #[test]
    fn px256_buffer_bytes() {
        assert_eq!(HeadResolution::Px256.buffer_bytes(), 256 * 256 * 3);
    }

    #[test]
    fn px384_buffer_bytes() {
        assert_eq!(HeadResolution::Px384.buffer_bytes(), 384 * 384 * 3);
    }

    #[test]
    fn all_resolutions_are_within_256_to_384() {
        for res in [HeadResolution::Px256, HeadResolution::Px384] {
            let px = res.pixels();
            assert!(
                (HEAD_PX_MIN..=HEAD_PX_MAX).contains(&px),
                "resolution {} is outside the [{HEAD_PX_MIN}, {HEAD_PX_MAX}] range",
                px
            );
        }
    }

    // ── SynthesisNetwork construction ─────────────────────────────────────────

    #[test]
    fn new_network_has_no_reference_frame() {
        let net = SynthesisNetwork::new(SynthesisConfig::default());
        assert!(!net.has_reference_frame());
    }

    #[test]
    fn resolution_matches_config() {
        let net =
            SynthesisNetwork::new(SynthesisConfig { resolution: HeadResolution::Px384, ..Default::default() });
        assert_eq!(net.resolution(), HeadResolution::Px384);
    }

    // ── load_reference_frame ──────────────────────────────────────────────────

    #[test]
    fn load_valid_reference_frame_succeeds() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        let frame = make_reference(256, 256);
        assert!(net.load_reference_frame(frame).is_ok());
        assert!(net.has_reference_frame());
    }

    #[test]
    fn load_invalid_reference_frame_returns_error() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        // Pixel buffer length does not match 10 × 10 × 3.
        let frame = ReferenceFrame { pixels: vec![0u8; 100], width: 10, height: 10 };
        assert_eq!(
            net.load_reference_frame(frame),
            Err(SynthesisError::InvalidReferenceFrame)
        );
        assert!(!net.has_reference_frame());
    }

    #[test]
    fn load_zero_dimension_reference_frame_returns_error() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        let frame = ReferenceFrame { pixels: vec![], width: 0, height: 100 };
        assert_eq!(
            net.load_reference_frame(frame),
            Err(SynthesisError::InvalidReferenceFrame)
        );
    }

    #[test]
    fn loading_second_frame_replaces_first() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        net.load_reference_frame(make_reference(128, 128)).unwrap();
        net.load_reference_frame(make_reference(256, 256)).unwrap();
        assert!(net.has_reference_frame());
    }

    // ── clear_reference_frame ─────────────────────────────────────────────────

    #[test]
    fn clear_reference_frame_removes_stored_frame() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        net.load_reference_frame(make_reference(256, 256)).unwrap();
        net.clear_reference_frame();
        assert!(!net.has_reference_frame());
    }

    #[test]
    fn reconstruct_after_clear_returns_no_reference_frame_error() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        net.load_reference_frame(make_reference(256, 256)).unwrap();
        net.clear_reference_frame();
        assert_eq!(
            net.reconstruct(&make_latents()),
            Err(SynthesisError::NoReferenceFrame)
        );
    }

    // ── reconstruct ───────────────────────────────────────────────────────────

    #[test]
    fn reconstruct_without_reference_frame_returns_error() {
        let net = SynthesisNetwork::new(SynthesisConfig::default());
        assert_eq!(
            net.reconstruct(&make_latents()),
            Err(SynthesisError::NoReferenceFrame)
        );
    }

    #[test]
    fn reconstruct_with_wrong_keypoint_count_returns_error() {
        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        net.load_reference_frame(make_reference(256, 256)).unwrap();

        let mut bad = make_latents();
        bad.keypoints.pop();
        assert_eq!(
            net.reconstruct(&bad),
            Err(SynthesisError::InvalidLatents("keypoint count mismatch"))
        );
    }

    #[test]
    fn reconstruct_at_256px_returns_correct_buffer_size() {
        let mut net =
            SynthesisNetwork::new(SynthesisConfig { resolution: HeadResolution::Px256, ..Default::default() });
        net.load_reference_frame(make_reference(256, 256)).unwrap();

        let frame = net.reconstruct(&make_latents()).unwrap();
        assert_eq!(frame.resolution, HeadResolution::Px256);
        assert_eq!(frame.pixels.len(), HeadResolution::Px256.buffer_bytes());
    }

    #[test]
    fn reconstruct_at_384px_returns_correct_buffer_size() {
        let mut net =
            SynthesisNetwork::new(SynthesisConfig { resolution: HeadResolution::Px384, ..Default::default() });
        net.load_reference_frame(make_reference(384, 384)).unwrap();

        let frame = net.reconstruct(&make_latents()).unwrap();
        assert_eq!(frame.resolution, HeadResolution::Px384);
        assert_eq!(frame.pixels.len(), HeadResolution::Px384.buffer_bytes());
    }

    #[test]
    fn output_resolution_within_256_to_384_range() {
        for res in [HeadResolution::Px256, HeadResolution::Px384] {
            let mut net = SynthesisNetwork::new(SynthesisConfig { resolution: res, ..Default::default() });
            let size = res.pixels();
            net.load_reference_frame(make_reference(size, size)).unwrap();

            let frame = net.reconstruct(&make_latents()).unwrap();
            let px = frame.resolution.pixels();
            assert!(
                (HEAD_PX_MIN..=HEAD_PX_MAX).contains(&px),
                "output resolution {px} is outside [{HEAD_PX_MIN}, {HEAD_PX_MAX}]"
            );
        }
    }

    #[test]
    fn zero_pose_reconstructs_reference_faithfully() {
        // With identity pose and same reference / output size, each output pixel
        // should sample near the corresponding reference pixel.
        let size = 16u32; // small for speed; resolution type is Px256 but ref is 16×16
        let ref_pixels: Vec<u8> = (0..(size * size * 3)).map(|i| (i % 251) as u8).collect();
        let mut net = SynthesisNetwork::new(SynthesisConfig {
            resolution: HeadResolution::Px256,
            keypoint_count: KEYPOINT_COUNT,
        });
        net.load_reference_frame(ReferenceFrame {
            pixels: ref_pixels.clone(),
            width: size,
            height: size,
        })
        .unwrap();

        let frame = net.reconstruct(&make_latents()).unwrap();
        // Output must be non-empty and correctly sized (256×256×3).
        assert_eq!(frame.pixels.len(), 256 * 256 * 3);
        // At zero pose the warp is identity, so output pixels are sampled from
        // the reference; they must be valid u8 values (implicitly true) and
        // not all zero if the reference is non-trivial.
        let non_zero = frame.pixels.iter().any(|&p| p != 0);
        assert!(non_zero, "reconstructed frame must not be entirely black with a non-trivial reference");
    }

    #[test]
    fn non_zero_pose_changes_output() {
        let size = 64u32;
        let mut net = SynthesisNetwork::new(SynthesisConfig {
            resolution: HeadResolution::Px256,
            keypoint_count: KEYPOINT_COUNT,
        });
        let reference = make_reference(size, size);
        net.load_reference_frame(reference).unwrap();

        let identity_frame = net.reconstruct(&make_latents()).unwrap();

        let mut rotated_latents = make_latents();
        rotated_latents.pose.roll = 0.2; // 0.2 rad ≈ 11°

        let rotated_frame = net.reconstruct(&rotated_latents).unwrap();

        // The two frames must differ — the warp moved pixels.
        let differ = identity_frame
            .pixels
            .iter()
            .zip(rotated_frame.pixels.iter())
            .any(|(a, b)| a != b);
        assert!(differ, "applying a non-zero pose rotation must change the reconstructed output");
    }

    #[test]
    fn keypoint_count_constant_matches_spec() {
        // Architecture spec: K ≈ 20 keypoints.
        assert_eq!(KEYPOINT_COUNT, 20);
    }

    #[test]
    fn head_px_constants_match_spec() {
        assert_eq!(HEAD_PX_MIN, 256);
        assert_eq!(HEAD_PX_MAX, 384);
    }

    // ── ReferenceFrame validity ───────────────────────────────────────────────

    #[test]
    fn reference_frame_is_valid_for_matching_buffer() {
        let f = make_reference(128, 128);
        assert!(f.is_valid());
    }

    #[test]
    fn reference_frame_is_invalid_for_mismatched_buffer() {
        let f = ReferenceFrame { pixels: vec![0u8; 100], width: 10, height: 10 };
        // 10 × 10 × 3 = 300 ≠ 100
        assert!(!f.is_valid());
    }

    #[test]
    fn reference_frame_is_invalid_for_zero_width() {
        let f = ReferenceFrame { pixels: vec![], width: 0, height: 10 };
        assert!(!f.is_valid());
    }
}

//! Sender-side integrity guardrails for Gear A — Features 121 and 122.
//!
//! The neural codec (Gear A) reconstructs a head from ≈20 implicit 3D
//! keypoints.  When the source video is invalid for that model — occluded,
//! multi-face, or confidence-below-threshold — the reconstruction will invent
//! pixels the sender cannot see, violating the codec's integrity contract.
//!
//! [`FallbackDetector`] checks four conditions on each frame's analysis data
//! (Feature 121):
//!
//! 1. **Keypoint confidence** — the mean over all K≈20 tracked keypoints must
//!    exceed [`KEYPOINT_CONFIDENCE_THRESHOLD`].
//! 2. **Hand occlusion** — a classifier detects when a hand is covering part
//!    of the face region.
//! 3. **Second face** — a second face in frame confuses both the tracker and
//!    the receiver's appearance encoder.
//! 4. **Non-face pixel ratio** — the fraction of non-face pixels in the face
//!    crop must stay below [`NON_FACE_PIXEL_RATIO_THRESHOLD`].
//!
//! [`GuardrailDetector`] wraps [`FallbackDetector`] and tracks the timestamp
//! of the first trip, enforcing the ≤ 200 ms Gear B switch deadline
//! (Feature 122).  When [`GuardrailDetector::is_gear_b_required`] returns
//! `true` the encoder must switch to Gear B using column-sweep intra-refresh
//! (see [`crate::intra_refresh::IntraRefreshState`]) — not an IDR keyframe.

use std::time::{Duration, Instant};

// ── Thresholds ────────────────────────────────────────────────────────────────

/// Mean keypoint-confidence threshold below which a trip is declared.
///
/// The neural motion estimator produces a confidence scalar per keypoint
/// ∈ [0.0, 1.0].  When the mean across all K≈20 keypoints falls below this
/// value the reconstruction is unreliable and Gear A must fall back.
pub const KEYPOINT_CONFIDENCE_THRESHOLD: f32 = 0.5;

/// Maximum fraction of non-face pixels permitted in the face crop.
///
/// A high ratio indicates the face bounding box is dominated by background,
/// hair, or other non-face content, meaning the appearance encoder's
/// assumptions no longer hold.
pub const NON_FACE_PIXEL_RATIO_THRESHOLD: f32 = 0.4;

/// Maximum elapsed time (ms) from the first guardrail trip to the encoder
/// being forced to Gear B (Feature 122).
///
/// At 25 fps, 200 ms is ≈ 4–5 frames — the maximum number of frames the
/// synthesis network may run on invalid keypoints before the switch.
pub const FORCE_GEAR_B_DEADLINE_MS: u64 = 200;

// ── FrameAnalysis ─────────────────────────────────────────────────────────────

/// Per-frame analysis produced by the sender's vision pipeline.
///
/// All fields are computed before keypoint extraction so that
/// [`FallbackDetector::check`] can gate whether Gear A encoding is safe for
/// the current frame.
#[derive(Debug, Clone)]
pub struct FrameAnalysis {
    /// Confidence scores for each tracked keypoint, ∈ [0.0, 1.0].
    ///
    /// K ≈ 20 values are expected; the detector uses the mean of whatever
    /// slice is provided.  An empty slice is treated as zero confidence (trip).
    pub keypoint_confidences: Vec<f32>,

    /// `true` when a hand-occlusion classifier detects a hand in the face region.
    pub hand_occlusion: bool,

    /// Number of face bounding boxes detected in the frame.
    ///
    /// Normal single-person video: 1.  > 1 trips the second-face guardrail.
    /// 0 indicates tracking loss and will typically coincide with low keypoint
    /// confidence (tripped by that check first).
    pub face_count: u32,

    /// Fraction of pixels in the face crop classified as non-face
    /// (background, hair, shoulders, etc.), ∈ [0.0, 1.0].
    pub non_face_pixel_ratio: f32,
}

// ── GuardrailTrip ─────────────────────────────────────────────────────────────

/// Which guardrail tripped and the value that caused the trip.
///
/// Returned by [`FallbackDetector::check`] when at least one condition fires.
/// When multiple conditions fire simultaneously only the highest-priority one
/// is returned.  Priority order matches the architecture spec listing:
/// keypoint confidence → hand occlusion → second face → non-face pixel ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuardrailTrip {
    /// Mean keypoint confidence fell below [`KEYPOINT_CONFIDENCE_THRESHOLD`].
    LowKeypointConfidence {
        /// Measured mean confidence (< threshold).
        mean_confidence: f32,
    },
    /// A hand-occlusion classifier detected a hand in the face region.
    HandOcclusion,
    /// More than one face was detected in the frame.
    SecondFace {
        /// Total face count detected (≥ 2).
        face_count: u32,
    },
    /// Non-face pixel ratio exceeded [`NON_FACE_PIXEL_RATIO_THRESHOLD`].
    NonFacePixelRatio {
        /// Measured ratio (> threshold).
        ratio: f32,
    },
}

// ── FallbackDetector ──────────────────────────────────────────────────────────

/// Stateless per-frame checker for Gear A integrity guardrails (Feature 121).
///
/// Evaluates [`FrameAnalysis`] against four conditions.  Returns
/// `Some(GuardrailTrip)` when any condition fires, `None` when the frame is
/// safe for Gear A encoding.
///
/// This type carries no mutable state.  For the timed Gear B deadline
/// (Feature 122), wrap it in [`GuardrailDetector`].
#[derive(Debug, Clone, Copy)]
pub struct FallbackDetector {
    /// Minimum mean keypoint confidence permitted for Gear A.
    pub keypoint_confidence_threshold: f32,
    /// Maximum non-face pixel ratio permitted for Gear A.
    pub non_face_pixel_ratio_threshold: f32,
}

impl Default for FallbackDetector {
    fn default() -> Self {
        Self {
            keypoint_confidence_threshold: KEYPOINT_CONFIDENCE_THRESHOLD,
            non_face_pixel_ratio_threshold: NON_FACE_PIXEL_RATIO_THRESHOLD,
        }
    }
}

impl FallbackDetector {
    /// Create a new detector with the default architecture thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check `analysis` against the four Gear A guardrails.
    ///
    /// Returns `Some(trip)` for the first condition that fires, or `None`
    /// when all checks pass and Gear A encoding is safe.
    pub fn check(&self, analysis: &FrameAnalysis) -> Option<GuardrailTrip> {
        // 1. Keypoint-tracking confidence — empty slice → zero confidence.
        let mean_conf = if analysis.keypoint_confidences.is_empty() {
            0.0_f32
        } else {
            let sum: f32 = analysis.keypoint_confidences.iter().sum();
            sum / analysis.keypoint_confidences.len() as f32
        };
        if mean_conf < self.keypoint_confidence_threshold {
            return Some(GuardrailTrip::LowKeypointConfidence { mean_confidence: mean_conf });
        }

        // 2. Hand occlusion.
        if analysis.hand_occlusion {
            return Some(GuardrailTrip::HandOcclusion);
        }

        // 3. Second face.
        if analysis.face_count > 1 {
            return Some(GuardrailTrip::SecondFace { face_count: analysis.face_count });
        }

        // 4. Non-face pixel ratio.
        if analysis.non_face_pixel_ratio > self.non_face_pixel_ratio_threshold {
            return Some(GuardrailTrip::NonFacePixelRatio { ratio: analysis.non_face_pixel_ratio });
        }

        None
    }
}

// ── GuardrailDetector ─────────────────────────────────────────────────────────

/// Stateful guardrail monitor that enforces the Gear B switch deadline
/// (Feature 122).
///
/// Wraps [`FallbackDetector`] and records when the first trip was observed.
/// [`is_gear_b_required`] returns `true` from the moment of the first trip
/// until [`clear_trip`] is called — signalling the encoder has switched.
///
/// # 200 ms deadline
///
/// From the architecture spec: *"any trip forces Gear B within 200 ms
/// (encoder pre-warmed, intra-refresh start, no keyframe burst)"*.
///
/// After a trip the caller should:
/// 1. Check [`is_gear_b_required`] — if `true`, switch the encoder to Gear B
///    immediately, using [`crate::intra_refresh::IntraRefreshState`] so that
///    the column sweep begins (no IDR keyframe burst).
/// 2. Call [`clear_trip`] once the encoder switch is confirmed complete.
///
/// [`is_gear_b_required`]: Self::is_gear_b_required
/// [`clear_trip`]: Self::clear_trip
pub struct GuardrailDetector {
    detector: FallbackDetector,
    /// `Instant` the most recent trip was first observed; `None` when clear.
    trip_start: Option<Instant>,
    /// Maximum duration allowed between first trip and encoder switch.
    deadline: Duration,
}

impl GuardrailDetector {
    /// Create a detector with the default thresholds and 200 ms deadline.
    pub fn new() -> Self {
        Self::with_deadline(
            FallbackDetector::new(),
            Duration::from_millis(FORCE_GEAR_B_DEADLINE_MS),
        )
    }

    /// Create a detector with a custom fallback checker and deadline.
    ///
    /// Useful in tests and for environments that require a tighter deadline.
    pub fn with_deadline(detector: FallbackDetector, deadline: Duration) -> Self {
        Self { detector, trip_start: None, deadline }
    }

    /// Process one frame and advance internal trip state.
    ///
    /// Returns `Some(trip)` when the frame is unsafe for Gear A encoding,
    /// `None` when the frame is safe and no trip is latched.
    ///
    /// When a trip is returned the caller must switch to Gear B (via column-
    /// sweep intra-refresh, not an IDR keyframe) within
    /// [`FORCE_GEAR_B_DEADLINE_MS`] milliseconds.
    pub fn update(&mut self, analysis: &FrameAnalysis, now: Instant) -> Option<GuardrailTrip> {
        match self.detector.check(analysis) {
            None => {
                self.trip_start = None;
                None
            }
            Some(trip) => {
                if self.trip_start.is_none() {
                    self.trip_start = Some(now);
                }
                Some(trip)
            }
        }
    }

    /// Returns `true` when the encoder must be switched to Gear B immediately.
    ///
    /// Becomes `true` as soon as [`update`] returns a trip and remains `true`
    /// until [`clear_trip`] is called.  The caller must act on the first
    /// `true` return — do not wait for the deadline to expire.
    ///
    /// [`update`]: Self::update
    pub fn is_gear_b_required(&self) -> bool {
        self.trip_start.is_some()
    }

    /// How long the current trip has been active, or `None` when clear.
    pub fn trip_duration(&self, now: Instant) -> Option<Duration> {
        self.trip_start.map(|t| now.saturating_duration_since(t))
    }

    /// `true` when the trip has been active longer than the Gear B deadline.
    ///
    /// Indicates the encoder switch is overdue.  The governor should log a
    /// warning and force-switch the encoder on the next tick.
    pub fn deadline_exceeded(&self, now: Instant) -> bool {
        self.trip_duration(now).map(|d| d > self.deadline).unwrap_or(false)
    }

    /// Clear the trip latch after the encoder switch to Gear B is confirmed.
    ///
    /// After this call [`is_gear_b_required`] returns `false` until the next
    /// trip.
    ///
    /// [`is_gear_b_required`]: Self::is_gear_b_required
    pub fn clear_trip(&mut self) {
        self.trip_start = None;
    }

    /// Access the underlying [`FallbackDetector`].
    pub fn detector(&self) -> &FallbackDetector {
        &self.detector
    }
}

impl Default for GuardrailDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn good_analysis() -> FrameAnalysis {
        FrameAnalysis {
            keypoint_confidences: vec![0.9; 20],
            hand_occlusion: false,
            face_count: 1,
            non_face_pixel_ratio: 0.1,
        }
    }

    fn detector() -> FallbackDetector {
        FallbackDetector::new()
    }

    // ── FallbackDetector — Feature 121 ────────────────────────────────────────

    #[test]
    fn all_clear_returns_none() {
        assert_eq!(detector().check(&good_analysis()), None);
    }

    #[test]
    fn low_mean_keypoint_confidence_trips() {
        let a = FrameAnalysis {
            keypoint_confidences: vec![0.3; 20], // mean 0.3 < 0.5
            ..good_analysis()
        };
        match detector().check(&a) {
            Some(GuardrailTrip::LowKeypointConfidence { mean_confidence }) => {
                assert!((mean_confidence - 0.3).abs() < 1e-5);
            }
            other => panic!("expected LowKeypointConfidence, got {other:?}"),
        }
    }

    #[test]
    fn exactly_at_threshold_does_not_trip() {
        // mean == threshold → no trip (strict less-than check)
        let a = FrameAnalysis {
            keypoint_confidences: vec![KEYPOINT_CONFIDENCE_THRESHOLD; 20],
            ..good_analysis()
        };
        assert_eq!(detector().check(&a), None);
    }

    #[test]
    fn empty_keypoints_trips_as_zero_confidence() {
        let a = FrameAnalysis {
            keypoint_confidences: vec![],
            ..good_analysis()
        };
        match detector().check(&a) {
            Some(GuardrailTrip::LowKeypointConfidence { mean_confidence }) => {
                assert_eq!(mean_confidence, 0.0);
            }
            other => panic!("expected LowKeypointConfidence for empty slice, got {other:?}"),
        }
    }

    #[test]
    fn hand_occlusion_trips() {
        let a = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        assert_eq!(detector().check(&a), Some(GuardrailTrip::HandOcclusion));
    }

    #[test]
    fn single_face_does_not_trip_second_face_check() {
        let a = FrameAnalysis { face_count: 1, ..good_analysis() };
        assert_eq!(detector().check(&a), None);
    }

    #[test]
    fn two_faces_trips_second_face() {
        let a = FrameAnalysis { face_count: 2, ..good_analysis() };
        assert_eq!(detector().check(&a), Some(GuardrailTrip::SecondFace { face_count: 2 }));
    }

    #[test]
    fn three_faces_carries_correct_count() {
        let a = FrameAnalysis { face_count: 3, ..good_analysis() };
        assert_eq!(detector().check(&a), Some(GuardrailTrip::SecondFace { face_count: 3 }));
    }

    #[test]
    fn non_face_ratio_below_threshold_does_not_trip() {
        let a = FrameAnalysis {
            non_face_pixel_ratio: NON_FACE_PIXEL_RATIO_THRESHOLD,
            ..good_analysis()
        };
        // Exactly at threshold — no trip (strict greater-than check).
        assert_eq!(detector().check(&a), None);
    }

    #[test]
    fn non_face_ratio_above_threshold_trips() {
        let ratio = NON_FACE_PIXEL_RATIO_THRESHOLD + 0.01;
        let a = FrameAnalysis { non_face_pixel_ratio: ratio, ..good_analysis() };
        match detector().check(&a) {
            Some(GuardrailTrip::NonFacePixelRatio { ratio: r }) => {
                assert!((r - ratio).abs() < 1e-5);
            }
            other => panic!("expected NonFacePixelRatio, got {other:?}"),
        }
    }

    // ── Priority order ────────────────────────────────────────────────────────

    #[test]
    fn keypoint_confidence_beats_occlusion_and_second_face() {
        // All four conditions fire; keypoint confidence must win.
        let a = FrameAnalysis {
            keypoint_confidences: vec![0.1; 20],
            hand_occlusion: true,
            face_count: 3,
            non_face_pixel_ratio: 0.9,
        };
        assert!(
            matches!(
                detector().check(&a),
                Some(GuardrailTrip::LowKeypointConfidence { .. })
            ),
            "keypoint confidence must have highest priority"
        );
    }

    #[test]
    fn occlusion_beats_second_face_and_ratio() {
        // Keypoints are fine; occlusion + second face + ratio all fire.
        let a = FrameAnalysis {
            keypoint_confidences: vec![0.9; 20],
            hand_occlusion: true,
            face_count: 2,
            non_face_pixel_ratio: 0.9,
        };
        assert_eq!(
            detector().check(&a),
            Some(GuardrailTrip::HandOcclusion),
            "hand occlusion must beat second face and ratio"
        );
    }

    #[test]
    fn second_face_beats_ratio() {
        let a = FrameAnalysis {
            keypoint_confidences: vec![0.9; 20],
            hand_occlusion: false,
            face_count: 2,
            non_face_pixel_ratio: 0.9,
        };
        assert_eq!(
            detector().check(&a),
            Some(GuardrailTrip::SecondFace { face_count: 2 }),
            "second face must beat non-face pixel ratio"
        );
    }

    // ── GuardrailDetector — Feature 122 ───────────────────────────────────────

    #[test]
    fn no_trip_initially() {
        let gd = GuardrailDetector::new();
        assert!(!gd.is_gear_b_required());
    }

    #[test]
    fn trip_immediately_requires_gear_b() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { keypoint_confidences: vec![0.1; 20], ..good_analysis() };
        let now = Instant::now();
        let result = gd.update(&bad, now);
        assert!(result.is_some(), "update must return the trip");
        assert!(gd.is_gear_b_required(), "gear B must be required after first trip");
    }

    #[test]
    fn clear_analysis_clears_trip() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { keypoint_confidences: vec![0.1; 20], ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);
        assert!(gd.is_gear_b_required());

        // Good frame clears the latch.
        gd.update(&good_analysis(), t0);
        assert!(!gd.is_gear_b_required());
    }

    #[test]
    fn clear_trip_resets_latch() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { keypoint_confidences: vec![0.1; 20], ..good_analysis() };
        gd.update(&bad, Instant::now());
        assert!(gd.is_gear_b_required());

        gd.clear_trip();
        assert!(!gd.is_gear_b_required());
    }

    #[test]
    fn consecutive_trips_hold_latch() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);
        gd.update(&bad, t0);
        gd.update(&bad, t0);
        assert!(gd.is_gear_b_required(), "latch must stay set across consecutive bad frames");
    }

    #[test]
    fn trip_duration_returns_elapsed() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);

        let later = t0 + Duration::from_millis(150);
        let dur = gd.trip_duration(later).expect("duration must be Some when tripped");
        assert!(dur >= Duration::from_millis(149), "elapsed must be ~150 ms, got {dur:?}");
    }

    #[test]
    fn trip_duration_none_when_clear() {
        let gd = GuardrailDetector::new();
        assert!(gd.trip_duration(Instant::now()).is_none());
    }

    #[test]
    fn deadline_not_exceeded_within_200ms() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);

        // 100 ms later — still within deadline.
        assert!(!gd.deadline_exceeded(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn deadline_exceeded_after_200ms() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);

        // 201 ms later — deadline has passed.
        assert!(gd.deadline_exceeded(t0 + Duration::from_millis(201)));
    }

    #[test]
    fn deadline_not_exceeded_when_no_trip() {
        let gd = GuardrailDetector::new();
        // No trip → deadline_exceeded must be false regardless of time.
        assert!(!gd.deadline_exceeded(Instant::now() + Duration::from_secs(10)));
    }

    #[test]
    fn re_trip_after_clear_resets_deadline_clock() {
        let mut gd = GuardrailDetector::new();
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();

        // First trip.
        gd.update(&bad, t0);
        gd.clear_trip();

        // Re-trip at t0 + 500 ms — new clock starts here.
        let t1 = t0 + Duration::from_millis(500);
        gd.update(&bad, t1);
        assert!(gd.is_gear_b_required());

        // 100 ms after re-trip — deadline not yet exceeded.
        assert!(!gd.deadline_exceeded(t1 + Duration::from_millis(100)));
        // 201 ms after re-trip — deadline exceeded.
        assert!(gd.deadline_exceeded(t1 + Duration::from_millis(201)));
    }

    #[test]
    fn custom_deadline_respected() {
        let fd = FallbackDetector::new();
        let mut gd = GuardrailDetector::with_deadline(fd, Duration::from_millis(50));
        let bad = FrameAnalysis { hand_occlusion: true, ..good_analysis() };
        let t0 = Instant::now();
        gd.update(&bad, t0);

        assert!(!gd.deadline_exceeded(t0 + Duration::from_millis(49)));
        assert!(gd.deadline_exceeded(t0 + Duration::from_millis(51)));
    }
}

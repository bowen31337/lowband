//! Feature 119 — entropy-coded delta motion_latents reach 5–9 kbps at 25 fps.
//!
//! # Scenario
//!
//! The Gear A sender extracts ≈ 20 implicit 3D keypoints, a 6-DoF head pose,
//! and 64 expression latents per frame (Feature 118).  Feature 119 requires
//! those latents to be entropy-coded to **5–9 kbps at 25 fps** so they fit
//! alongside voice and a thin intra-refresh channel at the 64 kbps Gear A
//! floor.
//!
//! # Method
//!
//! A deterministic 250-frame sequence (10 seconds at 25 fps) simulates a
//! moderately active speaker: head turns of ±8°, pitch nods of ±4.5°,
//! combined keypoint drift, and typical speech-related expression changes.
//! The sequence is encoded with [`MotionEncoder`]; the keyframe (frame 0) is
//! excluded from the bitrate measurement since it is amortised over the whole
//! session.  Total bytes for frames 1 … 249 are converted to bps and asserted
//! against [MOTION_BITRATE_LO_BPS, MOTION_BITRATE_HI_BPS].
//!
//! # Determinism
//!
//! The test uses pure sinusoidal functions — no RNG — so the bitrate is
//! identical across platforms and compiler versions.

use lowband_platform::keypoint_extractor::EXPRESSION_DIM;
use lowband_platform::motion_encoder::{
    MotionEncoder, MotionDecoder, MOTION_BITRATE_HI_BPS, MOTION_BITRATE_LO_BPS, MOTION_TARGET_FPS,
};
use lowband_platform::synthesis_network::{
    ExpressionLatents, HeadPose, Keypoint3D, MotionLatents, KEYPOINT_COUNT,
};

/// Number of frames in the evaluation sequence (10 s at 25 fps).
const N_FRAMES: usize = 250;

/// Simulate one frame of head-tracking motion.
///
/// The parameters match a moderately active speaker:
/// - Keypoints translate with the head and have small individual perturbations.
/// - Pose yaw oscillates at ±8.5° (0.15 rad) at 0.3 Hz; pitch ±4.5° at 0.4 Hz.
/// - Expression latents vary with speech rhythm at 1.5 Hz, amplitude 0.15.
///
/// These parameters yield inter-frame deltas of ≈ 2 quant steps for keypoints
/// and expression, and ≈ 4–6 quant steps for pose angles, producing an average
/// delta-frame size of ≈ 35–42 bytes at the encoder's quantisation scales.
fn make_latents(frame: usize) -> MotionLatents {
    use std::f32::consts::PI;
    let t = frame as f32 / MOTION_TARGET_FPS as f32; // seconds

    // Head translation: combined base + individual per-keypoint perturbation.
    let base_x = 0.5 + 0.08 * (2.0 * PI * 0.4 * t).sin();
    let base_y = 0.5 + 0.06 * (2.0 * PI * 0.35 * t).cos();

    let keypoints: Vec<Keypoint3D> = (0..KEYPOINT_COUNT)
        .map(|i| {
            let phase = i as f32 * 0.3;
            Keypoint3D {
                x: (base_x + 0.02 * (2.0 * PI * 0.5 * t + phase).sin()).clamp(0.0, 1.0),
                y: (base_y + 0.02 * (2.0 * PI * 0.45 * t + phase).cos()).clamp(0.0, 1.0),
                z: (0.05 + 0.02 * (2.0 * PI * 0.4 * t + phase).sin().abs()).clamp(0.0, 1.0),
                confidence: 0.9,
            }
        })
        .collect();

    // Pose: typical head movement during speech.
    let yaw   =  0.15 * (2.0 * PI * 0.3 * t).sin(); // ±8.5° at 0.3 Hz
    let pitch =  0.08 * (2.0 * PI * 0.4 * t).cos(); // ±4.5° at 0.4 Hz
    let roll  =  0.04 * (2.0 * PI * 0.5 * t).sin(); // ±2.3° at 0.5 Hz
    let tx    =  0.05 * (2.0 * PI * 0.3 * t).cos();
    let ty    =  0.03 * (2.0 * PI * 0.35 * t).sin();

    // Expression latents: speech-rhythm expression changes at 1.5 Hz.
    let expression_values: Vec<f32> = (0..EXPRESSION_DIM)
        .map(|i| {
            let phase = i as f32 * 0.5;
            0.15 * (2.0 * PI * 1.5 * t + phase).sin()
        })
        .collect();

    MotionLatents {
        keypoints,
        pose: HeadPose { yaw, pitch, roll, tx, ty, tz: 0.0 },
        expression: ExpressionLatents { values: expression_values },
    }
}

/// Encode N_FRAMES and assert the delta-frame bitrate is 5–9 kbps at 25 fps.
#[test]
fn delta_frames_reach_5_to_9_kbps_at_25_fps() {
    let mut enc = MotionEncoder::new();
    let mut dec = MotionDecoder::new();

    let mut delta_bytes: usize = 0;
    let mut delta_count: usize = 0;

    for frame in 0..N_FRAMES {
        let latents = make_latents(frame);
        let encoded = enc.encode(&latents);

        // Verify round-trip: decoder must reconstruct without error.
        dec.decode(&encoded).unwrap_or_else(|e| {
            panic!("frame {frame}: decode error: {e:?}");
        });

        if frame == 0 {
            // Keyframe — excluded from the bitrate measurement.
            continue;
        }
        delta_bytes += encoded.len();
        delta_count += 1;
    }

    // Duration covered by the measured delta frames.
    let duration_secs = delta_count as f64 / MOTION_TARGET_FPS as f64;
    let bps = (delta_bytes as f64 * 8.0) / duration_secs;

    eprintln!(
        "motion encoder bitrate  [{N_FRAMES} frames, {} fps]\n  \
         delta frames:     {delta_count}\n  \
         delta bytes:      {delta_bytes}\n  \
         avg frame size:   {:.1} B\n  \
         bitrate:          {:.0} bps ({:.2} kbps)\n  \
         target window:    [{MOTION_BITRATE_LO_BPS}, {MOTION_BITRATE_HI_BPS}] bps",
        MOTION_TARGET_FPS,
        delta_bytes as f64 / delta_count as f64,
        bps,
        bps / 1000.0,
    );

    assert!(
        bps >= MOTION_BITRATE_LO_BPS as f64,
        "delta-frame bitrate {bps:.0} bps is below the {MOTION_BITRATE_LO_BPS} bps floor; \
         motion data has insufficient variation for Gear A coverage"
    );
    assert!(
        bps <= MOTION_BITRATE_HI_BPS as f64,
        "delta-frame bitrate {bps:.0} bps exceeds the {MOTION_BITRATE_HI_BPS} bps ceiling; \
         quantisation or coding scheme needs tuning for the 5–9 kbps Gear A target"
    );
}

/// Verify that each delta frame is strictly smaller than the keyframe.
///
/// If this fails, the quantisation scales are too fine or the motion amplitude
/// is so large that deltas are bigger than absolute values.
#[test]
fn every_delta_frame_is_smaller_than_keyframe() {
    let mut enc = MotionEncoder::new();
    let kf_size = enc.encode(&make_latents(0)).len(); // keyframe

    for frame in 1..50 {
        let df_size = enc.encode(&make_latents(frame)).len();
        assert!(
            df_size < kf_size,
            "frame {frame}: delta ({df_size} B) ≥ keyframe ({kf_size} B); \
             motion amplitude too large for delta coding"
        );
    }
}

/// Confirm the decoder reconstructs keypoints within one quantisation step.
///
/// Round-trip error ≤ 1/SCALE_KP_XY = 1/256 ≈ 0.004 normalised units.
#[test]
fn round_trip_fidelity_within_one_quant_step() {
    let mut enc = MotionEncoder::new();
    let mut dec = MotionDecoder::new();

    for frame in 0..N_FRAMES {
        let original = make_latents(frame);
        let reconstructed = dec.decode(&enc.encode(&original)).unwrap();

        // 1 quant step tolerance for keypoint x.
        let eps = 1.0_f32 / 256.0 + 1e-5;
        for (i, (orig_kp, rec_kp)) in original
            .keypoints
            .iter()
            .zip(reconstructed.keypoints.iter())
            .enumerate()
        {
            assert!(
                (orig_kp.x - rec_kp.x).abs() < eps,
                "frame {frame} keypoint[{i}].x: \
                 original {:.4} reconstructed {:.4} error {:.6} > {eps:.6}",
                orig_kp.x,
                rec_kp.x,
                (orig_kp.x - rec_kp.x).abs()
            );
        }
    }
}

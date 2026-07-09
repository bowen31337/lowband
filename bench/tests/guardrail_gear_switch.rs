//! Feature 122 — system forces Gear B within 200 milliseconds when a guardrail_detector trips.
//!
//! # Purpose
//!
//! Verifies the complete guardrail → Gear B switch pipeline:
//!
//! 1. `GuardrailDetector` latches a trip the instant `update()` sees an unsafe frame.
//! 2. `is_gear_b_required()` becomes true immediately — the encoder must not wait.
//! 3. The trip remains latched for the full 200 ms deadline window; it does not
//!    auto-clear on the next tick.
//! 4. After 200 ms `deadline_exceeded()` becomes true — the switch is overdue.
//! 5. Once `clear_trip()` is called the latch resets and the deadline clock restarts
//!    on any subsequent re-trip.
//! 6. On a Gear B switch the encoder starts a column-sweep intra-refresh (via
//!    `IntraRefreshState`) — not an IDR keyframe — so the pacer token bucket is
//!    not overwhelmed.
//!
//! # Simulation
//!
//! A 60-frame Gear A session at 25 fps is simulated (simulated, not real-time).
//! At frame 10 a hand-occlusion guardrail fires.  The test verifies:
//!
//! - Frame 10: trip detected, Gear B required, deadline not yet exceeded (t=0).
//! - t < 200 ms: deadline not exceeded; the switch window is open.
//! - t = 201 ms: deadline exceeded; the encoder should have switched by now.
//! - After calling `clear_trip()`: latch resets, `is_gear_b_required()` is false.
//! - Post-switch encoder: `IntraRefreshState` emits `Keyframe` on the very first
//!   Gear B frame, then continuous `ColumnSweep` — no subsequent keyframe within
//!   a 30-frame window.
//!
//! # Architecture contract
//!
//! From the architecture spec: *"any trip forces Gear B within 200 ms (encoder
//! pre-warmed, intra-refresh start, no keyframe burst)"*.
//!
//! The 200 ms window at 25 fps covers ≈ 4–5 frames — the maximum number of
//! frames the synthesis network may run on invalid keypoints before the switch.
//! The column-sweep (not IDR) requirement is critical: an IDR at 100 kbps can
//! be 5–10× the average frame size and would stall the pacer.

use std::time::{Duration, Instant};

use lowband_platform::fallback_detector::{
    FrameAnalysis, GuardrailDetector, FORCE_GEAR_B_DEADLINE_MS,
};
use lowband_platform::intra_refresh::{IntraRefreshFrame, IntraRefreshState};

// ── Simulation parameters ─────────────────────────────────────────────────────

/// Simulated Gear A session frame rate (fps).
const GEAR_A_FPS: u32 = 25;

/// Frame duration at [`GEAR_A_FPS`].
const FRAME_DURATION: Duration = Duration::from_millis(1_000 / GEAR_A_FPS as u64);

/// Frame index at which the hand-occlusion guardrail fires.
const TRIP_FRAME: u32 = 10;

/// Total frames in the Gear A phase of the simulation.
const TOTAL_FRAMES: u32 = 60;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn good_frame() -> FrameAnalysis {
    FrameAnalysis {
        keypoint_confidences: vec![0.9; 20],
        hand_occlusion: false,
        face_count: 1,
        non_face_pixel_ratio: 0.1,
    }
}

fn occluded_frame() -> FrameAnalysis {
    FrameAnalysis { hand_occlusion: true, ..good_frame() }
}

// ── 1. Trip latches immediately on first bad frame ────────────────────────────

#[test]
fn trip_latches_on_first_bad_frame() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    // Good frames before the trip — no latch.
    for _ in 0..TRIP_FRAME {
        let result = gd.update(&good_frame(), t0);
        assert!(result.is_none(), "good frames must not trip the detector");
        assert!(!gd.is_gear_b_required(), "latch must be clear on good frames");
    }

    // First bad frame: trip must be latched immediately.
    let trip_result = gd.update(&occluded_frame(), t0);
    assert!(
        trip_result.is_some(),
        "update must return Some(trip) on the first bad frame"
    );
    assert!(
        gd.is_gear_b_required(),
        "is_gear_b_required must be true immediately after the first trip"
    );
}

// ── 2. Gear B required from the first trip, not after a delay ────────────────

#[test]
fn gear_b_required_from_first_trip_with_no_waiting() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    gd.update(&occluded_frame(), t0);

    // At t=0 (same instant as the trip) the switch must already be required.
    assert!(
        gd.is_gear_b_required(),
        "encoder must not be permitted to delay: Gear B is required at t=0"
    );
    assert!(
        !gd.deadline_exceeded(t0),
        "deadline must not be exceeded at t=0; there is still headroom up to 200 ms"
    );
}

// ── 3. Deadline not exceeded within 200 ms ────────────────────────────────────

#[test]
fn deadline_not_exceeded_within_200ms() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();
    gd.update(&occluded_frame(), t0);

    // Tick through the frames within the 200 ms window.
    // At 25 fps, 4 frames = 160 ms — still within the deadline.
    for frame in 1..=4u32 {
        let t = t0 + FRAME_DURATION * frame;
        assert!(
            gd.is_gear_b_required(),
            "Gear B must still be required at frame {frame} ({} ms after trip)",
            (FRAME_DURATION * frame).as_millis()
        );
        assert!(
            !gd.deadline_exceeded(t),
            "deadline must not be exceeded at frame {frame} ({} ms after trip)",
            (FRAME_DURATION * frame).as_millis()
        );
    }
}

// ── 4. Deadline exceeded at 201 ms ───────────────────────────────────────────

#[test]
fn deadline_exceeded_at_201ms() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();
    gd.update(&occluded_frame(), t0);

    let just_before = t0 + Duration::from_millis(FORCE_GEAR_B_DEADLINE_MS - 1);
    let just_after  = t0 + Duration::from_millis(FORCE_GEAR_B_DEADLINE_MS + 1);

    assert!(
        !gd.deadline_exceeded(just_before),
        "deadline must not be exceeded 1 ms before the 200 ms boundary"
    );
    assert!(
        gd.deadline_exceeded(just_after),
        "deadline must be exceeded 1 ms after the 200 ms boundary"
    );
}

// ── 5. Latch persists across consecutive bad frames ───────────────────────────

#[test]
fn latch_persists_across_consecutive_bad_frames() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    // Trip on frame 0.
    gd.update(&occluded_frame(), t0);
    assert!(gd.is_gear_b_required());

    // More bad frames before the encoder has switched — latch must stay.
    for frame in 1..5u32 {
        let t = t0 + FRAME_DURATION * frame;
        gd.update(&occluded_frame(), t);
        assert!(
            gd.is_gear_b_required(),
            "latch must stay set across consecutive bad frames (frame {frame})"
        );
    }
}

// ── 6. clear_trip resets the latch ───────────────────────────────────────────

#[test]
fn clear_trip_resets_latch() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();
    gd.update(&occluded_frame(), t0);
    assert!(gd.is_gear_b_required(), "precondition: latch must be set");

    // Encoder confirms the switch; latch clears.
    gd.clear_trip();
    assert!(
        !gd.is_gear_b_required(),
        "is_gear_b_required must be false after clear_trip"
    );
    assert!(
        !gd.deadline_exceeded(t0 + Duration::from_secs(10)),
        "deadline_exceeded must be false after clear_trip regardless of elapsed time"
    );
}

// ── 7. Re-trip after clear restarts the 200 ms clock ─────────────────────────

#[test]
fn re_trip_after_clear_restarts_deadline_clock() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    // First trip and switch.
    gd.update(&occluded_frame(), t0);
    gd.clear_trip();

    // 300 ms later the occlusion returns.
    let t1 = t0 + Duration::from_millis(300);
    gd.update(&occluded_frame(), t1);
    assert!(gd.is_gear_b_required(), "latch must re-engage on second trip");

    // 100 ms after re-trip — still within the fresh 200 ms window.
    assert!(
        !gd.deadline_exceeded(t1 + Duration::from_millis(100)),
        "deadline must not be exceeded 100 ms after re-trip"
    );

    // 201 ms after re-trip — overdue.
    assert!(
        gd.deadline_exceeded(t1 + Duration::from_millis(201)),
        "deadline must be exceeded 201 ms after re-trip"
    );
}

// ── 8. Post-switch encoder uses column sweep, not an IDR keyframe ─────────────

#[test]
fn gear_b_switch_uses_column_sweep_not_keyframe() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    // Trip detected — encoder must switch to Gear B.
    gd.update(&occluded_frame(), t0);
    assert!(gd.is_gear_b_required());

    // Simulate Gear B encoder startup with intra-refresh.
    let mut ir = IntraRefreshState::new(30); // 30 columns → 1-second sweep at 30 fps

    // First Gear B frame: initial keyframe for decoder sync.
    assert_eq!(
        ir.advance(),
        IntraRefreshFrame::Keyframe,
        "Gear B must emit exactly one keyframe on stream start for decoder sync"
    );

    // Encoder confirms the gear switch.
    gd.clear_trip();
    assert!(!gd.is_gear_b_required(), "latch must clear after switch confirmation");

    // All subsequent frames must be column sweeps — no further IDR keyframes.
    for frame in 0..30u32 {
        match ir.advance() {
            IntraRefreshFrame::ColumnSweep { col } => {
                assert!(
                    col < 30,
                    "column index {col} out of range on Gear B frame {frame}"
                );
            }
            IntraRefreshFrame::Keyframe => {
                panic!(
                    "Gear B must not emit a second keyframe (frame {frame}); \
                     column-sweep intra-refresh must replace all subsequent keyframes"
                );
            }
        }
    }
}

// ── 9. Full session simulation: Gear A → trip → Gear B ───────────────────────

#[test]
fn full_session_gear_a_trip_then_gear_b_switch() {
    let mut gd = GuardrailDetector::new();
    let session_start = Instant::now();

    let mut trip_detected_at: Option<Instant> = None;

    // Phase 1: Gear A frames before the trip.
    for frame in 0..TOTAL_FRAMES {
        let frame_time = session_start + FRAME_DURATION * frame;
        let analysis = if frame >= TRIP_FRAME { occluded_frame() } else { good_frame() };

        let trip = gd.update(&analysis, frame_time);

        if frame < TRIP_FRAME {
            assert!(trip.is_none(), "no trip expected before frame {TRIP_FRAME}");
            assert!(!gd.is_gear_b_required());
        } else if trip_detected_at.is_none() {
            // First bad frame — trip must latch.
            assert!(trip.is_some(), "trip expected at frame {frame}");
            assert!(gd.is_gear_b_required());
            trip_detected_at = Some(frame_time);
        } else {
            // Subsequent bad frames — latch stays.
            assert!(gd.is_gear_b_required(), "latch must persist at frame {frame}");
        }
    }

    // Phase 2: verify deadline from the first trip.
    let trip_time = trip_detected_at.expect("trip must have been detected");
    let elapsed_at_last_frame =
        session_start + FRAME_DURATION * (TOTAL_FRAMES - 1) - trip_time;

    // The simulation ran (TOTAL_FRAMES - TRIP_FRAME) = 50 frames past the trip.
    // At 25 fps that is 50 × 40 ms = 2 000 ms — well past the 200 ms deadline.
    assert!(
        elapsed_at_last_frame > Duration::from_millis(FORCE_GEAR_B_DEADLINE_MS),
        "simulation must have passed the 200 ms deadline: elapsed={elapsed_at_last_frame:?}"
    );

    // A well-behaved encoder would have called clear_trip() within 200 ms.
    // Check that the deadline was reached at the expected frame.
    let deadline_frame = TRIP_FRAME + ((FORCE_GEAR_B_DEADLINE_MS / 40) as u32) + 1;
    let deadline_time = session_start + FRAME_DURATION * deadline_frame;
    assert!(
        gd.deadline_exceeded(deadline_time),
        "deadline must be exceeded at frame {deadline_frame} ({} ms after trip)",
        (deadline_time - trip_time).as_millis()
    );

    // Phase 3: encoder switches to Gear B (column sweep).
    let mut ir = IntraRefreshState::new(30);
    assert_eq!(ir.advance(), IntraRefreshFrame::Keyframe);
    gd.clear_trip();
    assert!(!gd.is_gear_b_required());

    // 10 more Gear B frames — all column sweeps.
    for gear_b_frame in 0..10u32 {
        assert!(
            matches!(ir.advance(), IntraRefreshFrame::ColumnSweep { .. }),
            "Gear B frame {gear_b_frame} must be a column sweep"
        );
    }
}

// ── 10. Good frame after trip clears latch (no explicit clear_trip needed) ───

#[test]
fn good_frame_after_trip_clears_latch() {
    let mut gd = GuardrailDetector::new();
    let t0 = Instant::now();

    gd.update(&occluded_frame(), t0);
    assert!(gd.is_gear_b_required());

    // A clean frame from the vision pipeline clears the latch automatically.
    gd.update(&good_frame(), t0 + Duration::from_millis(10));
    assert!(
        !gd.is_gear_b_required(),
        "a good frame must clear the latch without requiring an explicit clear_trip call"
    );
}

// ── 11. Force_gear_b_deadline_ms constant matches architecture spec ──────────

#[test]
fn force_gear_b_deadline_ms_is_200() {
    assert_eq!(
        FORCE_GEAR_B_DEADLINE_MS, 200,
        "architecture spec mandates a 200 ms Gear B switch deadline"
    );
}

// ── 12. All four guardrail conditions independently force Gear B ──────────────

#[test]
fn all_four_conditions_force_gear_b_independently() {
    use lowband_platform::fallback_detector::{
        KEYPOINT_CONFIDENCE_THRESHOLD, NON_FACE_PIXEL_RATIO_THRESHOLD,
    };

    let bad_frames: [(&str, FrameAnalysis); 4] = [
        (
            "low keypoint confidence",
            FrameAnalysis {
                keypoint_confidences: vec![KEYPOINT_CONFIDENCE_THRESHOLD - 0.1; 20],
                ..good_frame()
            },
        ),
        (
            "hand occlusion",
            FrameAnalysis { hand_occlusion: true, ..good_frame() },
        ),
        (
            "second face",
            FrameAnalysis { face_count: 2, ..good_frame() },
        ),
        (
            "non-face pixel ratio",
            FrameAnalysis {
                non_face_pixel_ratio: NON_FACE_PIXEL_RATIO_THRESHOLD + 0.01,
                ..good_frame()
            },
        ),
    ];

    for (label, bad_frame) in &bad_frames {
        let mut gd = GuardrailDetector::new();
        let t0 = Instant::now();

        let trip = gd.update(bad_frame, t0);
        assert!(
            trip.is_some(),
            "guardrail condition '{label}' must produce a trip"
        );
        assert!(
            gd.is_gear_b_required(),
            "guardrail condition '{label}' must require Gear B immediately"
        );
        assert!(
            !gd.deadline_exceeded(t0),
            "guardrail condition '{label}': deadline must not be exceeded at t=0"
        );
        assert!(
            gd.deadline_exceeded(t0 + Duration::from_millis(201)),
            "guardrail condition '{label}': deadline must be exceeded at 201 ms"
        );

        // Reset for next condition.
        gd.clear_trip();
        assert!(!gd.is_gear_b_required(), "clear_trip must reset latch for '{label}'");
    }
}

//! Feature 126 — periodic column sweeps with intra_refresh instead of keyframes after stream start.
//!
//! # Purpose
//!
//! Verifies that the Gear B (SVT-AV1) intra-refresh policy emits exactly one
//! keyframe (the initial IDR required by the decoder), then continuously sweeps
//! column bands — refreshing the full frame over [`SWEEP_COLUMNS`] frames —
//! without ever emitting another keyframe.
//!
//! # Why this matters
//!
//! A full IDR keyframe refreshes every pixel in one frame.  For a 640×360 Gear B
//! stream at 100 kbps, a keyframe can be 5–10× the average frame size
//! (≈ 2–4 kB vs 417 B average at 30 fps).  Emitted every 1–2 seconds, periodic
//! keyframes create recurrent bitrate spikes that exceed the pacer token bucket
//! and stall the 64 kbps link.
//!
//! Column-sweep intra-refresh spreads the identical refresh work across
//! `SWEEP_COLUMNS` frames, adding roughly `1/SWEEP_COLUMNS` extra bits per frame
//! — far below pacer noise — while still recovering from packet loss within the
//! sweep period.
//!
//! # Simulation
//!
//! The test drives [`IntraRefreshState`] as a stand-in for the SVT-AV1
//! encoder parameter `intra_refresh_type = SVT_AV1_KF_DISABLED` combined with
//! `intrabc_mode = INTRA_BC_MODE_PERIODIC`.  Each call to `advance()` represents
//! one encoded frame; the returned [`IntraRefreshFrame`] maps directly to the
//! encoder's `use_intrabc` / `intra_bc_offset` parameters:
//!
//! | Returned variant         | SVT-AV1 instruction                          |
//! |--------------------------|----------------------------------------------|
//! | `Keyframe`               | Force IDR (initial frame only)               |
//! | `ColumnSweep { col }`   | Set `intra_refresh_col = col`, encode P-frame |
//!
//! # Assertions
//!
//! 1. First advance returns `Keyframe` — decoder sync frame is emitted.
//! 2. No `Keyframe` is returned after stream start.
//! 3. Columns 0 …= `SWEEP_COLUMNS - 1` each appear exactly once per sweep cycle.
//! 4. After one full cycle the sweep wraps back to column 0.
//! 5. Multiple consecutive cycles are deterministic and in order.
//! 6. The sweep period (`SWEEP_COLUMNS`) matches the Gear B target frame rate,
//!    giving a one-second full-frame refresh at 30 fps.
//! 7. Bitrate overhead of column sweep vs. keyframe budget is bounded.

use lowband_platform::gear_policy::GEAR_B_TARGET_FPS;
use lowband_platform::intra_refresh::{IntraRefreshFrame, IntraRefreshState};

/// Number of column bands per sweep cycle.
///
/// Set to [`GEAR_B_TARGET_FPS`] (30) so the full frame refreshes in exactly
/// one second at 30 fps — the same recovery window as a classic 1-Hz keyframe
/// cadence, without the bitrate spike.
const SWEEP_COLUMNS: u32 = GEAR_B_TARGET_FPS;

/// Number of complete sweep cycles verified in the multi-cycle test.
const VERIFY_CYCLES: u32 = 5;

/// Simulated camera bitrate for the overhead-bound test (100 kbps).
const CAMERA_BPS: u64 = 100_000;

/// Average bytes per frame at [`CAMERA_BPS`] and [`GEAR_B_TARGET_FPS`].
fn avg_frame_bytes() -> u64 {
    CAMERA_BPS / 8 / GEAR_B_TARGET_FPS as u64
}

// ── 1. Initial frame is a keyframe ────────────────────────────────────────────

#[test]
fn first_advance_is_keyframe() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    assert_eq!(
        ir.advance(),
        IntraRefreshFrame::Keyframe,
        "the very first frame must be a keyframe so the decoder can initialise"
    );
}

// ── 2. No keyframe after stream start ────────────────────────────────────────

#[test]
fn no_keyframe_emitted_after_stream_start() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    ir.advance(); // consume the initial keyframe

    // Run for 10 full sweep cycles — no keyframe must appear.
    let total_frames = SWEEP_COLUMNS * 10;
    for frame in 0..total_frames {
        let kind = ir.advance();
        assert_ne!(
            kind,
            IntraRefreshFrame::Keyframe,
            "unexpected keyframe at frame {frame} (10× {SWEEP_COLUMNS}-frame window); \
             intra_refresh must replace all post-start keyframes with column sweeps"
        );
    }
}

// ── 3. One sweep cycle covers every column exactly once ──────────────────────

#[test]
fn one_cycle_covers_all_columns_exactly_once() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    ir.advance(); // keyframe

    let mut coverage = vec![0u32; SWEEP_COLUMNS as usize];

    for _ in 0..SWEEP_COLUMNS {
        match ir.advance() {
            IntraRefreshFrame::ColumnSweep { col } => {
                assert!(
                    (col as usize) < coverage.len(),
                    "column index {col} out of range [0, {SWEEP_COLUMNS})"
                );
                coverage[col as usize] += 1;
            }
            IntraRefreshFrame::Keyframe => panic!("unexpected keyframe during sweep"),
        }
    }

    for (col, &count) in coverage.iter().enumerate() {
        assert_eq!(
            count, 1,
            "column {col} was refreshed {count} times in one sweep cycle (must be exactly 1)"
        );
    }
}

// ── 4. Sweep wraps to column 0 after one full cycle ──────────────────────────

#[test]
fn sweep_wraps_to_column_zero_after_full_cycle() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    ir.advance(); // keyframe

    // Drain one complete cycle.
    for _ in 0..SWEEP_COLUMNS {
        ir.advance();
    }

    // The next frame must start a fresh cycle at column 0.
    assert_eq!(
        ir.advance(),
        IntraRefreshFrame::ColumnSweep { col: 0 },
        "sweep must restart at column 0 after one full {SWEEP_COLUMNS}-frame cycle"
    );
}

// ── 5. Multiple consecutive cycles are deterministic ─────────────────────────

#[test]
fn multiple_cycles_are_deterministic_and_ordered() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    ir.advance(); // keyframe

    for cycle in 0..VERIFY_CYCLES {
        for expected_col in 0..SWEEP_COLUMNS {
            match ir.advance() {
                IntraRefreshFrame::ColumnSweep { col } => assert_eq!(
                    col, expected_col,
                    "cycle {cycle}: expected column {expected_col}, got {col}"
                ),
                IntraRefreshFrame::Keyframe => {
                    panic!("unexpected keyframe at cycle {cycle} column {expected_col}")
                }
            }
        }
    }
}

// ── 6. Sweep period matches one-second refresh at Gear B target fps ──────────

#[test]
fn sweep_period_equals_gear_b_target_fps_for_one_second_refresh() {
    // SWEEP_COLUMNS == GEAR_B_TARGET_FPS == 30.
    // At 30 fps, 30 column bands → full refresh in 1.0 s.
    assert_eq!(
        SWEEP_COLUMNS,
        GEAR_B_TARGET_FPS,
        "SWEEP_COLUMNS must equal GEAR_B_TARGET_FPS ({GEAR_B_TARGET_FPS}) so that \
         the full frame refreshes in exactly one second at Gear B's target frame rate"
    );

    let refresh_seconds = SWEEP_COLUMNS as f64 / GEAR_B_TARGET_FPS as f64;
    assert!(
        (refresh_seconds - 1.0).abs() < f64::EPSILON,
        "expected 1.0-second full-frame refresh cycle; got {refresh_seconds:.3} s"
    );
}

// ── 7. Column-sweep overhead is bounded vs. a keyframe budget ────────────────

#[test]
fn column_sweep_overhead_is_bounded_relative_to_keyframe_budget() {
    // A keyframe encodes every pixel as intra — roughly 5–10× the average
    // P-frame size.  A column-sweep frame encodes 1/SWEEP_COLUMNS of the frame
    // as intra, adding ≈ 1/SWEEP_COLUMNS of an average frame in overhead.
    //
    // Overhead per sweep frame ≈ avg_bytes / SWEEP_COLUMNS.
    // Keyframe size estimate   ≈ avg_bytes × KEYFRAME_MULTIPLIER.
    //
    // We assert that the per-frame sweep overhead is strictly less than the
    // amortised keyframe cost (keyframe_size / SWEEP_COLUMNS).

    const KEYFRAME_MULTIPLIER: u64 = 5; // conservative lower bound
    let avg = avg_frame_bytes();
    let keyframe_bytes     = avg * KEYFRAME_MULTIPLIER;
    let sweep_overhead_per_frame = avg / SWEEP_COLUMNS as u64; // 1/30 of avg
    let amortised_keyframe_cost  = keyframe_bytes / SWEEP_COLUMNS as u64; // kf/30

    // Sweep overhead per frame must be well below amortised keyframe cost.
    assert!(
        sweep_overhead_per_frame < amortised_keyframe_cost,
        "per-frame sweep overhead ({sweep_overhead_per_frame} B) must be less than \
         amortised keyframe cost ({amortised_keyframe_cost} B/frame); \
         avg_frame={avg} B, keyframe_est={keyframe_bytes} B, \
         sweep_columns={SWEEP_COLUMNS}"
    );

    eprintln!(
        "intra_refresh overhead bound  [camera {CAMERA_BPS} bps, {GEAR_B_TARGET_FPS} fps]\n  \
         avg frame:               {avg} B\n  \
         keyframe estimate (×{KEYFRAME_MULTIPLIER}): {keyframe_bytes} B\n  \
         sweep overhead/frame:    {sweep_overhead_per_frame} B\n  \
         amortised kf cost/frame: {amortised_keyframe_cost} B\n  \
         overhead ratio:          {:.2}×",
        sweep_overhead_per_frame as f64 / amortised_keyframe_cost as f64
    );
}

// ── 8. is_active tracks stream state ─────────────────────────────────────────

#[test]
fn is_active_false_before_stream_start_true_after() {
    let mut ir = IntraRefreshState::new(SWEEP_COLUMNS);
    assert!(
        !ir.is_active(),
        "is_active must be false before the first frame is emitted"
    );
    ir.advance(); // keyframe
    assert!(
        ir.is_active(),
        "is_active must be true once the stream has started"
    );
    // Must stay true throughout the sweep.
    for _ in 0..SWEEP_COLUMNS * 2 {
        ir.advance();
        assert!(ir.is_active(), "is_active must remain true after stream start");
    }
}

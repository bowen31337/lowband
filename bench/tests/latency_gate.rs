//! Feature 166 — latency_gate checks over mouth-to-ear and input-to-photon distributions.
//!
//! # Success criteria (architecture §success_criteria)
//!
//! ```text
//! Input-to-photon <= RTT + 60 ms
//! Mouth-to-ear    <= RTT/2 + 100 ms
//! ```
//!
//! RTT is subtracted out so the gate applies to the *non-network* processing
//! pipeline.  The non-network overhead breaks down as:
//!
//! | Path            | Fixed overhead                                           | SLA   |
//! |-----------------|----------------------------------------------------------|-------|
//! | Mouth-to-ear    | Opus encode 5 ms + jitter buffer 20 ms + decode 5 ms    | 100 ms|
//! | Input-to-photon | Input encode 2 ms + screen decode 2 ms + render 16 ms   | 60 ms |
//!
//! The pacer queuing delay is the variable component under test.  This test
//! measures the queuing delay distribution across a simulated session and
//! asserts that p95 of (fixed overhead + queuing delay) stays within the SLA.
//!
//! # Session model
//!
//! A 10-second session (2 000 ticks at 5 ms/tick) at 64 kbps (constrained
//! tier) with two burst scenarios exercised per second:
//!
//! - **Camera keyframe** (every 200 ticks = 1 s): 35 × 35-byte frames injected
//!   at once on ch 5 (video-rt), building a ~1 250-byte backlog that takes
//!   ~153 ticks to drain at the 8 B/tick residual budget.
//!
//! - **Audio encoder jitter** (every 10 ticks): 3 audio frames arrive in one
//!   tick (3 × 15 B + input 5 B = 50 B > 40 B burst cap).  The third audio
//!   frame carries over, producing a 1-tick (5 ms) queuing delay in the
//!   following tick.  At a 10 % event rate this lifts p95 mouth-to-ear to
//!   35 ms, giving the gate a non-trivial distribution to check against.
//!
//! The priority order (`input(3) > cursor(2) > audio(1) > screen-rt(4) >
//! video-rt(5)`) ensures audio and input frames always drain before camera
//! frames regardless of backlog depth.
//!
//! # Why p95?
//!
//! p95 excludes extreme one-off OS scheduler jitter while catching systematic
//! latency regressions that affect the majority of keyframe transitions or
//! burst events.

use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

// ── Session parameters ────────────────────────────────────────────────────────

/// Pacing tick duration — matches the LBTP burst-cap window at 64 kbps.
const TICK_NS: u64 = 5_000_000; // 5 ms

/// Ticks per second.
const TICKS_PER_SEC: u64 = 1_000_000_000 / TICK_NS; // 200

/// Tick duration in milliseconds (for arithmetic).
const TICK_MS: u64 = TICK_NS / 1_000_000; // 5

/// Constrained-tier link rate.
const LINK_BPS: u32 = 64_000;

/// Total simulation ticks (10-second session).
const TOTAL_TICKS: u64 = 10 * TICKS_PER_SEC; // 2 000

/// Camera keyframe injection interval (every 1 second).
const KEYFRAME_INTERVAL_TICKS: u64 = TICKS_PER_SEC; // 200

/// Number of 35-byte fragments per simulated keyframe burst on ch 5 (video-rt).
///
/// 35 × 35 = 1 225 bytes.  At 64 kbps the residual camera budget after
/// audio + input + screen is ~8 B/tick, so the burst takes ~153 ticks to
/// drain — plenty of backlog to stress-test the priority invariant.
const KEYFRAME_FRAGMENTS: usize = 35;

/// Size (bytes) of each keyframe fragment.  Must be ≤ MAX_FRAME_DATA_BYTES.
const KEYFRAME_FRAG_BYTES: usize = 35;

/// Audio encoder jitter interval: every 10 ticks the encoder produces an extra
/// burst of audio frames in one tick, exercising the carry-over path.
const JITTER_INTERVAL_TICKS: u64 = 10;

// ── Overhead model ────────────────────────────────────────────────────────────

/// Fixed non-network overhead on the mouth-to-ear path (ms).
///
/// Opus encode ≈5 ms + jitter buffer ≈20 ms + Opus decode ≈5 ms = 30 ms.
const AUDIO_FIXED_OVERHEAD_MS: u64 = 30;

/// Fixed non-network overhead on the input-to-photon path (ms).
///
/// Input encode ≈2 ms + screen decode ≈2 ms + display render ≈16 ms = 20 ms.
const INPUT_FIXED_OVERHEAD_MS: u64 = 20;

// ── Gate thresholds ───────────────────────────────────────────────────────────

/// Architecture SLA for total mouth-to-ear non-network overhead (ms).
const MOUTH_TO_EAR_BUDGET_MS: u64 = 100;

/// Architecture SLA for total input-to-photon non-network overhead (ms).
const INPUT_TO_PHOTON_BUDGET_MS: u64 = 60;

/// Percentile applied to the distribution gate.
const GATE_PERCENTILE: f64 = 0.95;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Integer payload bytes per 5 ms tick at the given bit rate.
fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

/// p-th percentile of a sorted slice (p in [0.0, 1.0]).
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn latency_gate_mouth_to_ear_and_input_to_photon() {
    let ch_audio = ChannelId::new(1);
    let ch_input = ChannelId::new(3);
    let ch_screen_rt = ChannelId::new(4);
    let ch_camera = ChannelId::new(5);

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);

    let audio_bytes = bytes_per_tick(budgets.audio_bps);   // 15 B
    let input_bytes = bytes_per_tick(budgets.input_bps);   //  5 B
    let screen_bytes = bytes_per_tick(budgets.screen_coarse_bps); // 12 B
    let camera_bytes = bytes_per_tick(budgets.camera_bps); //  7 B

    let mut pacer = Pacer::new(LINK_BPS as f64);

    // Per-tick overhead samples.
    let mut mouth_to_ear_samples: Vec<u64> = Vec::with_capacity(TOTAL_TICKS as usize);
    let mut input_to_photon_samples: Vec<u64> = Vec::with_capacity(TOTAL_TICKS as usize);

    for tick in 0..TOTAL_TICKS {
        // ── Sample queuing backlog before this tick's frames arrive ───────────
        //
        // `audio_backlog` and `input_backlog` are frames from prior ticks that
        // haven't drained yet.  A new frame enqueued this tick must wait for
        // them, so queuing_delay = backlog_frames × TICK_MS.
        let audio_backlog = pacer.queued_frames(ch_audio) as u64;
        let input_backlog = pacer.queued_frames(ch_input) as u64;

        // ── Enqueue this tick's frames ────────────────────────────────────────

        // Periodic audio encoder jitter: every JITTER_INTERVAL_TICKS ticks the
        // encoder produces 3 back-to-back audio frames in one tick.  Three
        // frames × 15 B = 45 B; combined with input (5 B) that is 50 B, which
        // exceeds the 40 B burst cap.  The third frame cannot fit in this tick
        // and carries over, producing a 5 ms (one tick) queuing delay visible
        // in the next tick's `audio_backlog` measurement.
        let audio_frames = if tick > 0 && tick % JITTER_INTERVAL_TICKS == 0 { 3 } else { 1 };
        for _ in 0..audio_frames {
            if audio_bytes > 0 {
                pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; audio_bytes]));
            }
        }
        if input_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; input_bytes]));
        }
        if screen_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_screen_rt, vec![0u8; screen_bytes]));
        }

        // Camera keyframe burst: inject KEYFRAME_FRAGMENTS small frames at
        // once on the lowest-priority media channel.  Steady-state frames
        // also enqueued to model ongoing camera delta traffic.
        if tick % KEYFRAME_INTERVAL_TICKS == 0 {
            for _ in 0..KEYFRAME_FRAGMENTS {
                pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; KEYFRAME_FRAG_BYTES]));
            }
        } else if camera_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; camera_bytes]));
        }

        // ── Drain the tick ────────────────────────────────────────────────────
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}

        // ── Record overhead samples ───────────────────────────────────────────
        //
        // queuing_delay = backlog × TICK_MS: a frame waiting behind `backlog`
        // frames each of equal size and same channel drains one tick later per
        // frame ahead of it.
        let mouth_to_ear_ms = AUDIO_FIXED_OVERHEAD_MS + audio_backlog * TICK_MS;
        let input_to_photon_ms = INPUT_FIXED_OVERHEAD_MS + input_backlog * TICK_MS;

        mouth_to_ear_samples.push(mouth_to_ear_ms);
        input_to_photon_samples.push(input_to_photon_ms);
    }

    // ── Compute and gate the distributions ───────────────────────────────────

    mouth_to_ear_samples.sort_unstable();
    input_to_photon_samples.sort_unstable();

    let p95_mouth_to_ear = percentile(&mouth_to_ear_samples, GATE_PERCENTILE);
    let p95_input_to_photon = percentile(&input_to_photon_samples, GATE_PERCENTILE);

    let mouth_max = *mouth_to_ear_samples.last().unwrap_or(&0);
    let input_max = *input_to_photon_samples.last().unwrap_or(&0);

    eprintln!(
        "latency_gate — mouth_to_ear: p95={p95_mouth_to_ear} ms  max={mouth_max} ms  \
         [budget: {MOUTH_TO_EAR_BUDGET_MS} ms]"
    );
    eprintln!(
        "latency_gate — input_to_photon: p95={p95_input_to_photon} ms  max={input_max} ms  \
         [budget: {INPUT_TO_PHOTON_BUDGET_MS} ms]"
    );

    assert!(
        p95_mouth_to_ear <= MOUTH_TO_EAR_BUDGET_MS,
        "mouth-to-ear p95 {p95_mouth_to_ear} ms exceeds {MOUTH_TO_EAR_BUDGET_MS} ms SLA \
         (overhead = fixed {AUDIO_FIXED_OVERHEAD_MS} ms + pacer queuing)"
    );
    assert!(
        p95_input_to_photon <= INPUT_TO_PHOTON_BUDGET_MS,
        "input-to-photon p95 {p95_input_to_photon} ms exceeds {INPUT_TO_PHOTON_BUDGET_MS} ms SLA \
         (overhead = fixed {INPUT_FIXED_OVERHEAD_MS} ms + pacer queuing)"
    );
}

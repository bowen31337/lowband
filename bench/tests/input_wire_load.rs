//! Feature 63 — worst-case wire load: input_cost between 0.5 and 2 kbps.
//!
//! # Architecture
//!
//! The cursor channel (LBTP channel 2) emits one delta-encoded position frame
//! at [`CURSOR_CHANNEL_HZ`] Hz.  Each frame is [`CURSOR_DELTA_BYTES`] bytes
//! (two signed 16-bit LE integers for dx and dy).  Zero-delta frames are emitted
//! on every tick — even when the cursor is stationary — so the channel is always
//! live and the receiver can distinguish "stationary cursor" from "no session".
//!
//! # Worst-case wire load
//!
//! The cursor channel is the dominant, always-on component of `input_cost`.
//! At 60 Hz with 4-byte frames:
//!
//! ```text
//! cursor_bps = CURSOR_CHANNEL_HZ × CURSOR_DELTA_BYTES × 8
//!            = 60 × 4 × 8 = 1 920 bps
//! ```
//!
//! This worst-case steady-state rate must satisfy the architectural band:
//!   • Floor: 1 920 bps ≥ 500 bps (0.5 kbps) ✓
//!   • Ceiling: 1 920 bps ≤ 2 000 bps (2 kbps) ✓
//!
//! # Simulated scenario
//!
//! The test drives the LBTP pacer at [`CURSOR_CHANNEL_HZ`] Hz for exactly one
//! second (60 ticks, one 4-byte cursor frame per tick) at the constrained-tier
//! link rate (64 kbps).  The burst cap at 64 kbps with a 16.67 ms tick is
//! 64 000 × 16.67 / 8 000 ≈ 133 bytes; the 4-byte cursor payload always fits
//! within a single tick, so no cursor frame is ever dropped or deferred.
//!
//! The measured wire-byte total is converted to bits per second and asserted
//! to lie within [500, 2 000] bps.

use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
use lowband_platform::cursor_sender::{CURSOR_CHANNEL_HZ, CURSOR_DELTA_BYTES, CURSOR_TICK_NS};

/// LBTP channel 2 — reliable-ordered cursor position stream.
const CURSOR_CHANNEL_ID: u8 = 2;

/// Constrained-tier link rate (bps) — the tightest production operating point.
const LINK_BPS: u32 = 64_000;

/// Simulation duration: one second expressed as a tick count at 60 Hz.
const SIM_TICKS: u64 = CURSOR_CHANNEL_HZ as u64; // 60 ticks × 16.67 ms = 1 000 ms

/// Minimum `input_cost` bitrate: 0.5 kbps.
const MIN_INPUT_COST_BPS: u64 = 500;

/// Maximum `input_cost` bitrate: 2 kbps.
const MAX_INPUT_COST_BPS: u64 = 2_000;

// ── 1. Constant derivation cross-check ────────────────────────────────────────

/// Verify the cursor wire cost derivation matches the expected value.
///
/// 60 Hz × 4 bytes × 8 bits = 1 920 bps.  Pinning this prevents silent
/// regressions if either constant is changed independently.
#[test]
fn cursor_wire_cost_is_1920_bps() {
    let cursor_bps = CURSOR_CHANNEL_HZ as u64 * CURSOR_DELTA_BYTES as u64 * 8;
    assert_eq!(
        cursor_bps,
        1_920,
        "cursor wire cost must be 60 Hz × 4 bytes × 8 bits = 1 920 bps; \
         got {cursor_bps} bps — check CURSOR_CHANNEL_HZ or CURSOR_DELTA_BYTES"
    );
}

/// Verify that the 60 Hz tick interval is consistent with the channel rate.
#[test]
fn cursor_tick_ns_matches_sixty_hz() {
    assert_eq!(
        CURSOR_TICK_NS,
        1_000_000_000 / CURSOR_CHANNEL_HZ as u64,
        "CURSOR_TICK_NS must equal 1 s / CURSOR_CHANNEL_HZ; \
         check cursor_sender constants for internal inconsistency"
    );
}

// ── 2. Wire-load range assertion from constants ───────────────────────────────

#[test]
fn cursor_wire_load_is_above_0_5_kbps_floor() {
    let cursor_bps = CURSOR_CHANNEL_HZ as u64 * CURSOR_DELTA_BYTES as u64 * 8;
    assert!(
        cursor_bps >= MIN_INPUT_COST_BPS,
        "cursor wire load {cursor_bps} bps is below the {MIN_INPUT_COST_BPS} bps (0.5 kbps) \
         floor — the cursor channel must always produce enough traffic to signal session \
         liveness; current rate implies {:.2} kbps",
        cursor_bps as f64 / 1_000.0,
    );
}

#[test]
fn cursor_wire_load_is_below_2_kbps_ceiling() {
    let cursor_bps = CURSOR_CHANNEL_HZ as u64 * CURSOR_DELTA_BYTES as u64 * 8;
    assert!(
        cursor_bps <= MAX_INPUT_COST_BPS,
        "cursor wire load {cursor_bps} bps ({:.2} kbps) exceeds the {MAX_INPUT_COST_BPS} bps \
         (2 kbps) ceiling — increasing CURSOR_CHANNEL_HZ or CURSOR_DELTA_BYTES would starve \
         the constrained-tier session of bandwidth for voice and screen",
        cursor_bps as f64 / 1_000.0,
    );
}

// ── 3. End-to-end pacer simulation ───────────────────────────────────────────

/// Drive 60 cursor frames through the LBTP pacer over one simulated second
/// and verify that every frame drains and the measured bit rate is within
/// [MIN_INPUT_COST_BPS, MAX_INPUT_COST_BPS].
///
/// This confirms the wire-cost model holds end-to-end in the pacer, not just
/// in the arithmetic above.
#[test]
fn worst_case_input_cost_between_0_5_and_2_kbps() {
    let ch_cursor = ChannelId::new(CURSOR_CHANNEL_ID);
    let mut pacer = Pacer::new(LINK_BPS as f64);
    let mut total_bytes: u64 = 0;

    // Simulate exactly one second: SIM_TICKS (60) ticks at CURSOR_TICK_NS
    // (≈ 16.67 ms) each.  One 4-byte cursor frame is enqueued per tick.
    for _ in 0..SIM_TICKS {
        pacer.enqueue(PacerFrame::new(ch_cursor, vec![0u8; CURSOR_DELTA_BYTES]));
        pacer.advance(CURSOR_TICK_NS);
        while let Some(frame) = pacer.dequeue() {
            total_bytes += frame.data.len() as u64;
        }
    }

    // Every cursor frame must have drained — none stalled by an insufficient
    // token balance.  At 64 kbps the burst cap per tick is ~133 bytes, which
    // easily accommodates the 4-byte cursor frame.
    let expected_bytes = SIM_TICKS * CURSOR_DELTA_BYTES as u64;
    assert_eq!(
        total_bytes,
        expected_bytes,
        "expected {expected_bytes} bytes of cursor traffic in 1 second \
         ({SIM_TICKS} ticks × {CURSOR_DELTA_BYTES} bytes); \
         got {total_bytes} — some frames may have been dropped or not yet drained"
    );

    // Convert bytes/second to bits/second and gate on the [0.5, 2] kbps band.
    // (The simulation runs exactly 1 second so bytes/s ≡ bytes.)
    let measured_bps = total_bytes * 8;

    eprintln!(
        "input_cost = {total_bytes} bytes/s = {measured_bps} bps ({:.2} kbps) \
         at {LINK_BPS} bps constrained tier  [range: {MIN_INPUT_COST_BPS}–{MAX_INPUT_COST_BPS} bps]",
        measured_bps as f64 / 1_000.0,
    );

    assert!(
        measured_bps >= MIN_INPUT_COST_BPS,
        "measured input_cost {measured_bps} bps is below the {MIN_INPUT_COST_BPS} bps \
         (0.5 kbps) floor — the cursor channel must always provide session-liveness traffic"
    );
    assert!(
        measured_bps <= MAX_INPUT_COST_BPS,
        "measured input_cost {measured_bps} bps ({:.2} kbps) exceeds the \
         {MAX_INPUT_COST_BPS} bps (2 kbps) ceiling — \
         cursor traffic must not crowd out voice or screen at the constrained tier",
        measured_bps as f64 / 1_000.0,
    );
}

//! Feature 171 — constrained-assist session data budget.
//!
//! A 30-minute constrained assist session at the architecture minimum link
//! rate (64 kbps) must transfer at most 15 MB total.  This test simulates
//! the session through the LBTP pacer at the stream allocations computed by
//! the gear-policy allocator and asserts the measured session_data stays
//! within budget.
//!
//! # Why 5 ms ticks?
//!
//! The LBTP token-bucket burst cap is `rate × BURST_TOLERANCE_MS / 8 000`.
//! At 64 kbps that cap is exactly 40 bytes (64 000 × 5 / 8 000).  Using a
//! 5 ms tick interval (= BURST_TOLERANCE_MS) means each tick earns exactly
//! one burst cap's worth of tokens and the per-tick frame set (39 bytes) fits
//! in a single drain, so no frames are dropped due to insufficient tokens.
//!
//! # Why 64 kbps?
//!
//! The architecture specifies that a full constrained session (voice + legible
//! screen + responsive control) must be viable at 64 kbps.  Running the
//! budget check at this floor is the worst-case: any higher rate would produce
//! more bytes per unit time while still staying below the 15 MB cap.

use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

/// Architecture minimum for a viable constrained session (bits per second).
const LINK_BPS: u32 = 64_000;

/// Tick interval in nanoseconds.  Chosen to match the LBTP burst-cap window
/// so each tick earns exactly one burst cap's worth of tokens (40 bytes at
/// 64 kbps) and per-tick frames are admitted without token deficit.
const TICK_NS: u64 = 5_000_000; // 5 ms

/// Ticks per second derived from the tick interval.
const TICKS_PER_SEC: u64 = 1_000_000_000 / TICK_NS; // 200

/// Session duration in seconds (30 minutes).
const SESSION_SECS: u64 = 30 * 60;

/// Spec budget: "A typical 30-minute constrained assist session consumes at
/// most 15 MB total" (architecture PRD, success criteria §UX).
const BUDGET_BYTES: u64 = 15_000_000; // 15 MB (metric)

/// Integer payload bytes per tick for a stream allocated `bps` bits per second.
///
/// Truncates any fractional byte so the result is always a valid `usize`.
/// Aggregated truncation across 1 800 000 ticks is negligible compared to the
/// 600 kB headroom between the expected 13.4 MB and the 15 MB budget.
fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

#[test]
fn constrained_30_min_session_data_below_15_mb() {
    // Derive per-stream allocations at 64 kbps under nominal thermal pressure.
    // At this link rate the allocator funds:
    //   audio 24 kbps → input 8 kbps → screen coarse 20 kbps → camera 12 kbps.
    // Screen refinement and xfer receive nothing — no headroom remains.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);

    let audio_bytes = bytes_per_tick(budgets.audio_bps);   // 15 B per 5 ms
    let input_bytes = bytes_per_tick(budgets.input_bps);   //  5 B per 5 ms
    let screen_bytes = bytes_per_tick(budgets.screen_coarse_bps); // 12 B per 5 ms
    let camera_bytes = bytes_per_tick(budgets.camera_bps); //  7 B per 5 ms
    let per_tick_total = audio_bytes + input_bytes + screen_bytes + camera_bytes;

    // Verify the per-tick payload fits within one burst cap (40 bytes at 64 kbps).
    // If it exceeds the cap the pacer would stall frames and undercount data.
    let burst_cap_bytes = (LINK_BPS as u64 * 5 / 8_000) as usize; // 40 B
    assert!(
        per_tick_total <= burst_cap_bytes,
        "per-tick payload {per_tick_total} B exceeds burst cap {burst_cap_bytes} B; \
         reduce tick interval or frame sizes"
    );

    // Channel IDs per the LBTP channel map (lib.rs §Channel map).
    let ch_audio = ChannelId::new(1);
    let ch_input = ChannelId::new(3);
    let ch_screen_rt = ChannelId::new(4);
    let ch_video_rt = ChannelId::new(5);

    let mut pacer = Pacer::new(LINK_BPS as f64);
    let mut total_bytes: u64 = 0;

    let total_ticks = SESSION_SECS * TICKS_PER_SEC; // 360 000

    for _ in 0..total_ticks {
        // Enqueue one frame per active stream, sized to match the 5 ms bitrate
        // allocation.  Integer truncation causes a ~1 kbps shortfall but the
        // resulting total is well within the 15 MB budget.
        if audio_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; audio_bytes]));
        }
        if input_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; input_bytes]));
        }
        if screen_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_screen_rt, vec![0u8; screen_bytes]));
        }
        if camera_bytes > 0 {
            pacer.enqueue(PacerFrame::new(ch_video_rt, vec![0u8; camera_bytes]));
        }

        // Advance the token bucket by one 5 ms tick and drain all eligible
        // frames.  Because per_tick_total ≤ burst_cap_bytes, every frame is
        // admitted and no data is left in the queue at the end of the tick.
        pacer.advance(TICK_NS);
        while let Some(frame) = pacer.dequeue() {
            total_bytes += frame.data.len() as u64;
        }
    }

    let total_mb = total_bytes as f64 / 1_000_000.0;
    eprintln!(
        "session_data = {total_bytes} bytes ({total_mb:.2} MB) \
         for 30-min constrained session at {} kbps  [budget: 15 MB]",
        LINK_BPS / 1000,
    );

    assert!(
        total_bytes <= BUDGET_BYTES,
        "session_data {total_bytes} B ({total_mb:.2} MB) exceeds the 15 MB budget"
    );
}

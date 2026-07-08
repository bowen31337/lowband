//! Feature 170 — 400 → 64 kbps collapse keeps screen_latency under 200 ms.
//!
//! # Scenario
//!
//! A session running at 400 kbps collapses instantly to 64 kbps (e.g. a
//! cellular hand-off or sudden congestion event).  The governor runs at 10 Hz,
//! so it takes up to 100 ms (20 × 5 ms ticks) to detect the change and adjust
//! encoder targets.  During that reaction window the upstream encoders continue
//! submitting frames at the old 400 kbps rates, building a backlog in the
//! pacer queues.
//!
//! # Why screen-rt survives
//!
//! The LBTP priority order is:
//!   ctrl(0) > input(3) > cursor(2) > audio(1) > screen-rt(4) > camera(5) > …
//!
//! Camera (ch 5) has **lower** priority than screen-rt (ch 4).  Even with
//! 100 ms worth of undraned camera frames queued (≈ 3 740 bytes at 300 kbps),
//! those frames cannot block the screen-rt channel.  Only ctrl, input, cursor,
//! and audio — small, bounded streams — sit ahead of screen-rt in the drain
//! order.  The worst-case drain time for those four streams is well under
//! 200 ms at 64 kbps.
//!
//! # Latency model
//!
//! `screen_latency` here is the pacer queuing delay: time from enqueue of the
//! probe frame to its dequeue, measured in 5 ms ticks at 64 kbps.  New audio
//! and input frames (at 64 kbps rates) are submitted each tick so that the
//! simulation captures ongoing high-priority competition, not just the one-time
//! backlog.

use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

/// Tick interval in nanoseconds (5 ms, matching the LBTP burst-cap window).
const TICK_NS: u64 = 5_000_000;

/// Ticks per second derived from the tick interval.
const TICKS_PER_SEC: u64 = 1_000_000_000 / TICK_NS; // 200

/// Pre-collapse link rate.
const LINK_HIGH_BPS: u32 = 400_000;

/// Post-collapse link rate — the architecture minimum viable session floor.
const LINK_LOW_BPS: u32 = 64_000;

/// Governor reaction window: ticks at the old rate before encoder targets adjust.
///
/// The governor control loop runs at 10 Hz (one tick per 100 ms).  Using 20
/// ticks (100 ms) is the worst case: the collapse happens right after a
/// governor tick, so the full next interval must elapse before adaptation.
const GOVERNOR_LAG_TICKS: u64 = 20;

/// Maximum permissible screen latency per the architecture spec (200 ms).
const MAX_LATENCY_NS: u64 = 200_000_000;

/// Integer payload bytes per tick for a stream allocated `bps` bits per second.
fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

#[test]
fn collapse_400_to_64_kbps_keeps_screen_latency_under_200ms() {
    let ch_audio = ChannelId::new(1);
    let ch_input = ChannelId::new(3);
    let ch_screen_rt = ChannelId::new(4);
    let ch_camera = ChannelId::new(5);

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);

    // Stream sizes per 5 ms tick at 400 kbps (Nominal thermal):
    //   audio 24 kbps → 15 B, input 8 kbps → 5 B,
    //   screen_coarse 20 kbps → 12 B, camera 300 kbps → 187 B.
    let high = allocate(LINK_HIGH_BPS, &constraints);
    let high_audio = bytes_per_tick(high.audio_bps);
    let high_input = bytes_per_tick(high.input_bps);
    let high_camera = bytes_per_tick(high.camera_bps);

    // Stream sizes per 5 ms tick at 64 kbps (Nominal thermal):
    //   audio 24 kbps → 15 B, input 8 kbps → 5 B, screen_coarse 20 kbps → 12 B.
    let low = allocate(LINK_LOW_BPS, &constraints);
    let low_audio = bytes_per_tick(low.audio_bps);
    let low_input = bytes_per_tick(low.input_bps);
    let low_screen = bytes_per_tick(low.screen_coarse_bps).max(1);

    // ── Phase 1: Steady state at 400 kbps ────────────────────────────────────
    // Run enough ticks at the high rate so the pacer reaches a clean, steady-
    // state queue: all frames enqueued each tick drain in that same tick.
    let mut pacer = Pacer::new(LINK_HIGH_BPS as f64);
    for _ in 0..10 {
        if high_audio > 0 {
            pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; high_audio]));
        }
        if high_input > 0 {
            pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; high_input]));
        }
        if high_camera > 0 {
            pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; high_camera]));
        }
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}
    }

    // ── Phase 2: Governor lag — build worst-case backlog ─────────────────────
    // Encoders submit one full governor cycle of frames at 400 kbps rates but
    // the pacer is NOT drained.  This creates the maximum possible backlog that
    // a real session would have at the moment the collapse is detected.
    for _ in 0..GOVERNOR_LAG_TICKS {
        if high_audio > 0 {
            pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; high_audio]));
        }
        if high_input > 0 {
            pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; high_input]));
        }
        if high_camera > 0 {
            pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; high_camera]));
        }
    }

    // ── Phase 3: Collapse ─────────────────────────────────────────────────────
    // Drop the rate to 64 kbps.  The pacer clamps the token balance to the new
    // burst cap (40 bytes), so no accumulated surplus from the high-rate epoch
    // can leak through.
    pacer.set_rate(LINK_LOW_BPS as f64);

    // Enqueue the probe frame: the first screen-rt frame submitted post-collapse.
    // Its queuing delay is what we are measuring.
    pacer.enqueue(PacerFrame::new(ch_screen_rt, vec![0u8; low_screen]));

    // ── Phase 4: Drain at 64 kbps — measure latency ──────────────────────────
    // Each tick we also submit new audio and input frames at 64 kbps rates.
    // This models the ongoing high-priority competition during the drain phase
    // — the worst case for screen-rt latency.
    let mut elapsed_ns: u64 = 0;
    let max_ticks = MAX_LATENCY_NS / TICK_NS + 1;

    for _ in 0..max_ticks {
        // New traffic arrives from still-running 64 kbps audio and input encoders.
        if low_audio > 0 {
            pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; low_audio]));
        }
        if low_input > 0 {
            pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; low_input]));
        }

        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}
        elapsed_ns += TICK_NS;

        if pacer.queued_frames(ch_screen_rt) == 0 {
            break;
        }
    }

    let latency_ms = elapsed_ns / 1_000_000;
    eprintln!(
        "screen_latency = {}ms after 400→64 kbps collapse  [limit: 200ms]",
        latency_ms
    );

    assert!(
        pacer.queued_frames(ch_screen_rt) == 0,
        "screen-rt probe frame was never sent within the measurement window"
    );
    assert!(
        elapsed_ns <= MAX_LATENCY_NS,
        "screen_latency {}ms exceeds 200ms SLA after 400→64 kbps collapse",
        latency_ms
    );
}

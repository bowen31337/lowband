//! Feature 168 — stock WebRTC H.264 reference_client on identical traces.
//!
//! # Purpose
//!
//! Runs a stock WebRTC H.264 sender model (FIFO video queue, GCC-style token-bucket
//! pacing) and the LBTP priority pacer over the *same* network trace —
//! a 400 kbps → 64 kbps collapse with a 20-tick governor lag — and compares the
//! `screen_latency` of each system.
//!
//! # Trace description
//!
//! Both senders see the identical three-phase trace:
//!
//! 1. **Steady state** (10 ticks at 400 kbps) — all streams drain cleanly each tick.
//! 2. **Governor lag** (20 ticks) — encoders continue at 400 kbps rates while
//!    the pacer has not yet received a new rate target.  Neither sender drains the
//!    camera backlog that builds here.
//! 3. **Post-collapse drain** (64 kbps) — rate drops, token cap shrinks to 40 B,
//!    a screen probe frame is submitted, and we measure how many ticks elapse until
//!    it exits the send queue.
//!
//! # Why the systems diverge
//!
//! During the governor lag, the camera encoder (300 kbps) writes 187 B frames to
//! the send queue for 20 ticks, producing 3 740 B of queued camera data.  At the
//! 64 kbps floor the burst cap is 40 B per 5 ms tick; audio (15 B) and input (5 B)
//! consume 20 B of that, leaving at most 20 B per tick for video.
//!
//! | System                       | Video queue discipline  | Can skip 187 B camera frame? |
//! |------------------------------|-------------------------|------------------------------|
//! | LBTP (ch-priority pacer)     | per-channel priority    | Yes — screen-rt (ch 4) > camera (ch 5) |
//! | WebRTC H.264 reference (FIFO)| arrival-order FIFO      | No — head-of-line blocked    |
//!
//! The LBTP dequeuer skips camera (ch 5, 187 B) when tokens are insufficient and
//! drains the screen probe (ch 4, 12 B) in the first post-collapse tick.
//!
//! The reference FIFO can never drain a 187 B camera frame because the per-tick
//! video budget (20 B) is smaller than the frame and the token cap (40 B) prevents
//! accumulation across ticks.  The screen probe therefore remains blocked for the
//! entire measurement window.
//!
//! # Assertions
//!
//! 1. LBTP `screen_latency` ≤ 200 ms — channel-priority invariant holds.
//! 2. Reference `screen_latency` > LBTP `screen_latency` — camera head-of-line
//!    blocking is demonstrated.
//! 3. Reference `screen_latency` > 200 ms — the reference client fails the
//!    architecture SLA at the 64 kbps survival floor.

use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;
use std::collections::VecDeque;

/// Tick interval in nanoseconds (5 ms, matching the LBTP burst-cap window).
const TICK_NS: u64 = 5_000_000;

/// Ticks per second derived from tick interval.
const TICKS_PER_SEC: u64 = 1_000_000_000 / TICK_NS; // 200

/// Pre-collapse link rate.
const LINK_HIGH_BPS: u32 = 400_000;

/// Post-collapse link rate — the architecture minimum viable session floor.
const LINK_LOW_BPS: u32 = 64_000;

/// Governor reaction window: 20 ticks (100 ms worst case).
const GOVERNOR_LAG_TICKS: u64 = 20;

/// Architecture SLA for `screen_latency`.
const MAX_LATENCY_NS: u64 = 200_000_000; // 200 ms

/// Measurement window given to each sender: 4 × the SLA.
///
/// The reference client can never drain a 187 B camera frame within the token
/// budget, so it will exhaust this window.  Choosing 4× (800 ms) keeps the
/// test fast while clearly showing the reference misses the 200 ms SLA.
const MEASURE_WINDOW_TICKS: u64 = 4 * MAX_LATENCY_NS / TICK_NS; // 160 ticks = 800 ms

fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

// ── WebRTC H.264 reference sender model ──────────────────────────────────────

/// Simplified stock WebRTC H.264 send path.
///
/// Audio and input streams have a separate, always-admitted priority path that
/// mirrors WebRTC's practice of sending audio as a distinct SRTP stream at the
/// OS scheduler level.  Camera and screen-share share a single FIFO video queue
/// with GCC-style token-bucket pacing — no per-stream priority differentiation.
struct WebRtcReference {
    /// FIFO video queue: `(frame_size_bytes, is_screen_probe)`.
    video_fifo: VecDeque<(usize, bool)>,
    tokens: f64,
    rate_bps: f64,
}

impl WebRtcReference {
    fn new(rate_bps: f64) -> Self {
        Self { video_fifo: VecDeque::new(), tokens: 0.0, rate_bps }
    }

    fn burst_cap(&self) -> f64 {
        self.rate_bps * 5.0 / 8_000.0
    }

    fn set_rate(&mut self, rate_bps: f64) {
        self.rate_bps = rate_bps;
        // Clamp to the new burst cap on rate reduction (matches LBTP Pacer behaviour).
        self.tokens = self.tokens.min(self.burst_cap());
    }

    fn enqueue_camera(&mut self, size: usize) {
        if size > 0 {
            self.video_fifo.push_back((size, false));
        }
    }

    /// Append the screen probe frame *after* any queued camera frames.
    fn enqueue_screen_probe(&mut self, size: usize) {
        if size > 0 {
            self.video_fifo.push_back((size, true));
        }
    }

    fn advance(&mut self, elapsed_ns: u64) {
        let earned = self.rate_bps * elapsed_ns as f64 / 8_000_000_000.0;
        self.tokens = (self.tokens + earned).min(self.burst_cap());
    }

    /// Drain one tick: consume the audio+input priority budget first, then
    /// drain the video FIFO in arrival order until tokens are exhausted or
    /// the front frame is too large.
    ///
    /// Returns `true` if the screen probe was drained during this call.
    fn drain_tick(&mut self, priority_bytes: usize) -> bool {
        // Audio and input are admitted unconditionally (separate priority path).
        self.tokens = (self.tokens - priority_bytes as f64).max(0.0);

        let mut probe_drained = false;
        loop {
            match self.video_fifo.front() {
                Some(&(sz, is_probe)) if self.tokens >= sz as f64 => {
                    self.tokens -= sz as f64;
                    self.video_fifo.pop_front();
                    probe_drained |= is_probe;
                }
                _ => break,
            }
        }
        probe_drained
    }

}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn webrtc_h264_reference_client_on_identical_traces() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);

    // Pre-collapse stream sizes (400 kbps, Nominal thermal):
    //   audio 24 kbps → 15 B, input 8 kbps → 5 B, camera 300 kbps → 187 B.
    let high        = allocate(LINK_HIGH_BPS, &constraints);
    let high_audio  = bytes_per_tick(high.audio_bps);   // 15 B
    let high_input  = bytes_per_tick(high.input_bps);   //  5 B
    let high_camera = bytes_per_tick(high.camera_bps);  // 187 B

    // Post-collapse stream sizes (64 kbps, Nominal thermal):
    //   audio 24 kbps → 15 B, input 8 kbps → 5 B, screen coarse 20 kbps → 12 B.
    let low         = allocate(LINK_LOW_BPS, &constraints);
    let low_audio   = bytes_per_tick(low.audio_bps);             // 15 B
    let low_input   = bytes_per_tick(low.input_bps);             //  5 B
    let low_screen  = bytes_per_tick(low.screen_coarse_bps).max(1); // 12 B

    // Priority bytes per tick that audio+input consume before any video can drain.
    // Identical in both models; the sole difference is how video is queued.
    let low_priority_bytes = low_audio + low_input; // 20 B

    // ── LBTP path (channel-priority pacer) ───────────────────────────────────

    let ch_audio     = ChannelId::new(1);
    let ch_input     = ChannelId::new(3);
    let ch_screen_rt = ChannelId::new(4);
    let ch_camera    = ChannelId::new(5);

    let mut pacer = Pacer::new(LINK_HIGH_BPS as f64);

    // Phase 1: Steady state — 10 ticks at 400 kbps, drained each tick.
    for _ in 0..10 {
        if high_audio  > 0 { pacer.enqueue(PacerFrame::new(ch_audio,  vec![0u8; high_audio])); }
        if high_input  > 0 { pacer.enqueue(PacerFrame::new(ch_input,  vec![0u8; high_input])); }
        if high_camera > 0 { pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; high_camera])); }
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}
    }

    // Phase 2: Governor lag — 20 ticks of backlog building, no drain.
    for _ in 0..GOVERNOR_LAG_TICKS {
        if high_audio  > 0 { pacer.enqueue(PacerFrame::new(ch_audio,  vec![0u8; high_audio])); }
        if high_input  > 0 { pacer.enqueue(PacerFrame::new(ch_input,  vec![0u8; high_input])); }
        if high_camera > 0 { pacer.enqueue(PacerFrame::new(ch_camera, vec![0u8; high_camera])); }
    }

    // Phase 3: Collapse — rate drops to 64 kbps; screen probe enqueued.
    pacer.set_rate(LINK_LOW_BPS as f64);
    pacer.enqueue(PacerFrame::new(ch_screen_rt, vec![0u8; low_screen]));

    // Phase 4: Drain at 64 kbps — measure ticks until the probe clears.
    // The LBTP dequeuer skips the 187 B camera frames (token budget 20 B < 187 B)
    // and drains the 12 B screen probe in the first tick (12 B ≤ 20 B remaining).
    let mut lbtp_elapsed_ns: u64 = 0;
    for _ in 0..MEASURE_WINDOW_TICKS {
        if low_audio > 0 { pacer.enqueue(PacerFrame::new(ch_audio, vec![0u8; low_audio])); }
        if low_input > 0 { pacer.enqueue(PacerFrame::new(ch_input, vec![0u8; low_input])); }
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}
        lbtp_elapsed_ns += TICK_NS;
        if pacer.queued_frames(ch_screen_rt) == 0 { break; }
    }

    // ── Reference path (WebRTC H.264 FIFO video queue) ───────────────────────

    let mut reference = WebRtcReference::new(LINK_HIGH_BPS as f64);

    // Phase 1: Steady state — camera frames drain each tick (burst cap 250 B > 187 B).
    let high_priority = high_audio + high_input;
    for _ in 0..10 {
        reference.enqueue_camera(high_camera);
        reference.advance(TICK_NS);
        reference.drain_tick(high_priority);
    }

    // Phase 2: Governor lag — camera backlog builds in the FIFO, no drain.
    for _ in 0..GOVERNOR_LAG_TICKS {
        reference.enqueue_camera(high_camera);
    }
    // FIFO now holds GOVERNOR_LAG_TICKS × high_camera bytes of 187 B camera frames.

    // Phase 3: Collapse — rate drops to 64 kbps; screen probe appended after backlog.
    reference.set_rate(LINK_LOW_BPS as f64);
    reference.enqueue_screen_probe(low_screen);

    // Phase 4: Drain at 64 kbps — measure ticks until the probe clears.
    // The per-tick video budget is 20 B (= 40 B burst cap − 20 B priority).
    // The FIFO front is a 187 B camera frame, which exceeds the 20 B budget and
    // can never drain (token cap at 64 kbps is 40 B, so accumulation is bounded).
    // The screen probe therefore remains blocked for the entire window.
    let mut ref_elapsed_ns: u64 = 0;
    let mut ref_probe_drained = false;
    for _ in 0..MEASURE_WINDOW_TICKS {
        reference.advance(TICK_NS);
        ref_elapsed_ns += TICK_NS;
        if reference.drain_tick(low_priority_bytes) {
            ref_probe_drained = true;
            break;
        }
    }

    let lbtp_ms = lbtp_elapsed_ns / 1_000_000;
    let ref_ms  = ref_elapsed_ns  / 1_000_000;

    eprintln!(
        "screen_latency on identical 400→64 kbps trace  [SLA: ≤200 ms]\n  \
         LBTP (channel-priority pacer):    {lbtp_ms} ms\n  \
         WebRTC H.264 reference (FIFO):    {ref_ms} ms  \
         (probe_drained={ref_probe_drained})\n  \
         camera_backlog: {} × {high_camera} B = {} B at 64 kbps burst cap {} B",
        GOVERNOR_LAG_TICKS,
        GOVERNOR_LAG_TICKS as usize * high_camera,
        (LINK_LOW_BPS as f64 * 5.0 / 8_000.0) as usize,
    );

    // 1. LBTP must meet the architecture screen-latency SLA.
    assert!(
        lbtp_elapsed_ns <= MAX_LATENCY_NS,
        "LBTP screen_latency {lbtp_ms} ms exceeds 200 ms SLA after 400→64 kbps collapse; \
         channel-priority invariant (screen-rt ch4 > camera ch5) must hold"
    );

    // 2. Reference is slower: camera FIFO head-of-line blocking is demonstrated.
    assert!(
        ref_elapsed_ns > lbtp_elapsed_ns,
        "reference screen_latency {ref_ms} ms must exceed LBTP {lbtp_ms} ms; \
         camera FIFO blocking not reproduced — check governor lag or frame sizes"
    );

    // 3. Reference fails the SLA: camera backlog cannot be cleared fast enough.
    assert!(
        ref_elapsed_ns > MAX_LATENCY_NS,
        "reference screen_latency {ref_ms} ms must exceed 200 ms SLA; \
         the 187 B camera frames must permanently block the FIFO at the 64 kbps floor"
    );
}

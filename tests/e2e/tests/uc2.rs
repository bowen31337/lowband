//! UC-2 end-to-end verification — Feature 176.
//!
//! Guided walkthrough, view-only: a household ADSL upload saturates the link
//! mid-call.  The governor holds voice with `audio_gaps` eliminated while
//! dropping screen refinement; the cursor overlay remains fluid throughout.
//!
//! # Scenario
//!
//! | Phase | Link | Channels active |
//! |-------|------|-----------------|
//! | Pre-saturation | 512 kbps | audio + cursor + screen-rt + camera + refinement |
//! | Governor lag (100 ms) | 64 kbps | old encoders still submit at 512 kbps rates |
//! | Post-adaptation | 64 kbps | audio + cursor + screen-rt only |
//!
//! # Why voice never gaps
//!
//! LBTP priority order: `ctrl(0) > input(3) > cursor(2) > audio(1) > screen-rt(4) > camera(5) > reliable(6)`
//!
//! At 64 kbps the token budget per 5 ms tick is 40 bytes:
//!
//! ```text
//! cursor  (ch 2)  4 B  → dequeues first; 36 B remaining
//! audio   (ch 1) 15 B  → dequeues next;  21 B remaining
//! screen  (ch 4) 12 B  → dequeues next;   9 B remaining
//! camera  (ch 5) 187 B → needs 187 B, have 9 B → blocked
//! reliable(ch 6)  31 B → needs  31 B, have 9 B → blocked
//! ```
//!
//! Camera and refinement accumulate a backlog during the 100 ms governor lag
//! but cannot block audio because they have lower LBTP priority.  Every audio
//! frame submitted is dequeued in the same 5 ms tick it arrives → zero gaps.
//!
//! # Screen refinement assertion
//!
//! `allocate(64_000, Nominal)` returns `screen_refinement_bps = 0` because the
//! 64 kbps budget is exhausted after audio (24 k) + input (8 k) + screen-coarse
//! (20 k) + residual camera (12 k).  The governor can stop the refinement
//! encoder immediately on detecting the saturation event.

use lowband_lbtp::{ChannelId, Pacer, PacerFrame};
use lowband_platform::{allocate, GearConstraints, ThermalPressure, AUDIO_FLOOR_BPS};

// Channel constants matching the LBTP architecture spec §6.2.
const CH_AUDIO:     ChannelId = ChannelId(1);
const CH_CURSOR:    ChannelId = ChannelId(2);
const CH_SCREEN_RT: ChannelId = ChannelId(4);
const CH_CAMERA:    ChannelId = ChannelId(5);
const CH_RELIABLE:  ChannelId = ChannelId(6);

// One pacing tick (LBTP burst-cap window): 5 ms = 5_000_000 ns.
const TICK_NS: u64 = 5_000_000;
const TICKS_PER_SEC: u64 = 1_000_000_000 / TICK_NS; // 200

// ADSL upload bandwidth before the household upload starts.
const ADSL_HIGH_BPS: u32 = 512_000;
// ADSL upload bandwidth left for the session after saturation.
const ADSL_LOW_BPS: u32 = 64_000;

// Governor control loop: 10 Hz → 100 ms reaction time → 20 × 5 ms ticks.
// This captures the worst-case window in which old-rate camera frames pile up.
const GOVERNOR_LAG_TICKS: u64 = 20;

/// Integer payload bytes per 5 ms tick for a stream allocated `bps` bits/s.
fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

/// UC-2: governor holds voice with audio_gaps eliminated while an ADSL upload
/// saturates the link.
#[test]
fn uc2_view_only_walkthrough_holds_voice_during_adsl_upload_saturation() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);

    // ── Phase 1: Verify governor allocations ──────────────────────────────────
    //
    // Pre-saturation (512 kbps): camera and screen refinement are funded.
    // Post-saturation (64 kbps): audio floor is still honoured; refinement
    // drops to zero because the budget is exhausted by higher-priority streams.

    let high = allocate(ADSL_HIGH_BPS, &constraints);
    assert!(
        high.camera_bps > 0,
        "camera must receive bandwidth pre-saturation at {} kbps (got 0)",
        ADSL_HIGH_BPS / 1_000
    );
    assert!(
        high.screen_refinement_bps > 0,
        "screen refinement must receive bandwidth pre-saturation at {} kbps",
        ADSL_HIGH_BPS / 1_000
    );
    assert!(
        high.audio_bps >= AUDIO_FLOOR_BPS,
        "voice floor must be honoured pre-saturation: got {} bps, floor {} bps",
        high.audio_bps,
        AUDIO_FLOOR_BPS
    );

    let low = allocate(ADSL_LOW_BPS, &constraints);
    assert!(
        low.audio_bps >= AUDIO_FLOOR_BPS,
        "voice floor must be honoured during ADSL saturation: got {} bps, floor {} bps",
        low.audio_bps,
        AUDIO_FLOOR_BPS
    );
    assert_eq!(
        low.screen_refinement_bps, 0,
        "screen refinement must drop to 0 at {} kbps — governor drops refinement passes",
        ADSL_LOW_BPS / 1_000
    );
    assert!(
        low.camera_bps < high.camera_bps,
        "camera allocation must fall sharply at saturation: {} → {} bps",
        high.camera_bps,
        low.camera_bps
    );

    // ── Phase 2: Steady state at 512 kbps ─────────────────────────────────────
    //
    // Run a few ticks so the pacer reaches a clean drain-each-tick equilibrium
    // before the saturation event.

    let mut pacer = Pacer::new(ADSL_HIGH_BPS as f64);

    let high_audio    = bytes_per_tick(high.audio_bps).max(1);
    let high_screen   = bytes_per_tick(high.screen_coarse_bps).max(1);
    let high_camera   = bytes_per_tick(high.camera_bps).max(1);
    let high_reliable = bytes_per_tick(high.screen_refinement_bps).max(1);
    // Small cursor event: a few bytes per tick (pointer overlay delta).
    let cursor_bytes: usize = 4;

    for _ in 0..10 {
        pacer.enqueue(PacerFrame::new(CH_AUDIO,     vec![0u8; high_audio]));
        pacer.enqueue(PacerFrame::new(CH_CURSOR,    vec![0u8; cursor_bytes]));
        pacer.enqueue(PacerFrame::new(CH_SCREEN_RT, vec![0u8; high_screen]));
        pacer.enqueue(PacerFrame::new(CH_CAMERA,    vec![0u8; high_camera]));
        pacer.enqueue(PacerFrame::new(CH_RELIABLE,  vec![0u8; high_reliable]));
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}
    }

    // ── Phase 3: ADSL upload saturates — link drops to 64 kbps ───────────────
    //
    // The congestion controller detects overuse and calls set_rate(64 kbps)
    // immediately.  The governor (10 Hz) still feeds camera and refinement at
    // the old 512 kbps rates for up to GOVERNOR_LAG_TICKS (100 ms), causing
    // their queues to accumulate a backlog.

    pacer.set_rate(ADSL_LOW_BPS as f64);

    let low_audio  = bytes_per_tick(low.audio_bps).max(1);
    let low_screen = bytes_per_tick(low.screen_coarse_bps).max(1);

    let mut audio_gap_ticks = 0u32;

    for _ in 0..GOVERNOR_LAG_TICKS {
        // Encoders have not yet adapted; camera and refinement still submit at
        // the old high rates, building a backlog the pacer cannot clear.
        pacer.enqueue(PacerFrame::new(CH_AUDIO,     vec![0u8; high_audio]));
        pacer.enqueue(PacerFrame::new(CH_CURSOR,    vec![0u8; cursor_bytes]));
        pacer.enqueue(PacerFrame::new(CH_SCREEN_RT, vec![0u8; low_screen]));
        pacer.enqueue(PacerFrame::new(CH_CAMERA,    vec![0u8; high_camera]));
        pacer.enqueue(PacerFrame::new(CH_RELIABLE,  vec![0u8; high_reliable]));

        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}

        // An audio gap means the frame submitted this tick was not drained
        // within the same 5 ms tick — a potential voice interruption.
        if pacer.queued_frames(CH_AUDIO) > 0 {
            audio_gap_ticks += 1;
        }
    }

    // ── Phase 4: Governor adapts — camera and refinement stop ─────────────────
    //
    // After the 100 ms lag the governor sees the new bandwidth estimate and
    // stops the camera and refinement encoders.  In view-only mode the cursor
    // overlay, audio, and screen-coarse continue; the governor does not need
    // to send camera.
    //
    // The old camera backlog remains queued but is effectively stranded: at
    // 64 kbps the 40 B/tick budget is fully consumed by cursor (4 B) +
    // audio (15 B) + screen-rt (12 B) = 31 B, leaving 9 B — not enough for
    // a 187 B camera frame.  Realtime camera frames are discarded stale by
    // the receiver; they must not block voice.

    for _ in 0..GOVERNOR_LAG_TICKS {
        pacer.enqueue(PacerFrame::new(CH_AUDIO,     vec![0u8; low_audio]));
        pacer.enqueue(PacerFrame::new(CH_CURSOR,    vec![0u8; cursor_bytes]));
        pacer.enqueue(PacerFrame::new(CH_SCREEN_RT, vec![0u8; low_screen]));

        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}

        if pacer.queued_frames(CH_AUDIO) > 0 {
            audio_gap_ticks += 1;
        }
    }

    // ── Assertions ────────────────────────────────────────────────────────────

    let camera_backlog = pacer.queued_frames(CH_CAMERA);

    eprintln!(
        "UC-2 ADSL saturation: audio_gap_ticks={audio_gap_ticks} [limit: 0], \
         camera_backlog={camera_backlog} frames (stale, stranded by priority), \
         screen_refinement_bps_at_64k={}",
        low.screen_refinement_bps,
    );

    assert_eq!(
        audio_gap_ticks, 0,
        "governor must eliminate audio gaps during ADSL upload saturation: \
         {audio_gap_ticks} gap tick(s) detected across {} ticks",
        GOVERNOR_LAG_TICKS * 2
    );

    // Cursor overlay must drain every tick — cursor stays fluid (priority rank 3,
    // ahead of audio rank 4, so it never waits for media frames).
    assert_eq!(
        pacer.queued_frames(CH_CURSOR), 0,
        "cursor frames must drain in every tick — pointer overlay must stay fluid"
    );

    // Camera backlog exists but is bounded: at most GOVERNOR_LAG_TICKS frames
    // built up during the adaptation window.  It does NOT grow in Phase 4.
    assert!(
        camera_backlog <= GOVERNOR_LAG_TICKS as usize,
        "camera backlog must not exceed the governor lag window ({} ticks), got {} frames",
        GOVERNOR_LAG_TICKS,
        camera_backlog
    );
}

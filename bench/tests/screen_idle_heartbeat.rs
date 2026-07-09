//! Feature 87 — System sends zero bytes for a static screen beyond a 1 Hz heartbeat.
//!
//! # Scenario
//!
//! A remote desktop session often spends significant time showing an unchanged
//! screen (user reading, waiting for a build, etc.).  Continuously re-encoding
//! and transmitting identical frame data wastes bandwidth on a metered or
//! low-rate link.
//!
//! With idle suppression enabled:
//! - Static ticks (no dirty rects) produce zero content bytes.
//! - Exactly one heartbeat packet fires per [`SCREEN_HEARTBEAT_NS`] (1 s) of
//!   uninterrupted static content so the remote can distinguish "idle" from
//!   "connection lost".
//! - Any dirty tick resets the idle accumulator and resumes normal encoding.
//!
//! # Test structure
//!
//! **Part A — static screen produces zero bytes**: 10 s of static content at
//! 50 Hz confirms exactly 10 heartbeats and 490 suppress events.
//!
//! **Part B — heartbeat fires at exactly 1 Hz**: drive 1 s of ticks and confirm
//! a single heartbeat at the 1-second boundary.
//!
//! **Part C — dirty frame resets idle timer**: dirty frame at 0.9 s of idle
//! prevents the 1 s heartbeat; heartbeat fires 1 s after the dirty tick.
//!
//! **Part D — Send on dirty**: dirty ticks always return `Send` regardless of
//! accumulated idle time.

use lowband_platform::{IdleSuppressor, ScreenIdleAction, SCREEN_HEARTBEAT_NS};

/// Tick interval in nanoseconds at 50 Hz (20 ms per tick).
///
/// 50 Hz gives exactly 1_000_000_000 / 20_000_000 = 50 ticks per second so
/// heartbeat boundaries land cleanly on tick boundaries.
const TICK_NS: u64 = 20_000_000; // 20 ms

/// Ticks per second at [`TICK_NS`].
const TICKS_PER_SEC: u64 = SCREEN_HEARTBEAT_NS / TICK_NS; // 50

// ── Part A: static screen produces zero bytes ─────────────────────────────────

#[test]
fn static_screen_produces_exactly_one_heartbeat_per_second() {
    let total_secs: u64 = 10;
    let total_ticks = total_secs * TICKS_PER_SEC;

    let mut idle = IdleSuppressor::new();
    let mut heartbeat_count: u64 = 0;
    let mut suppress_count: u64 = 0;
    let mut send_count: u64 = 0;

    for _ in 0..total_ticks {
        match idle.observe(false, TICK_NS) {
            ScreenIdleAction::Heartbeat => heartbeat_count += 1,
            ScreenIdleAction::Suppress  => suppress_count  += 1,
            ScreenIdleAction::Send      => send_count      += 1,
        }
    }

    assert_eq!(
        send_count, 0,
        "no dirty ticks → zero Send actions; got {send_count}"
    );
    assert_eq!(
        heartbeat_count, total_secs,
        "static screen must produce exactly {total_secs} heartbeats in {total_secs} s; \
         got {heartbeat_count}"
    );
    assert_eq!(
        suppress_count,
        total_ticks - total_secs,
        "all non-heartbeat ticks must be Suppress; \
         expected {}, got {suppress_count}",
        total_ticks - total_secs
    );

    eprintln!(
        "screen_idle: {total_ticks} ticks  heartbeats={heartbeat_count}  \
         suppress={suppress_count}  send={send_count}"
    );
}

// ── Part B: heartbeat fires at exactly 1 Hz ───────────────────────────────────

#[test]
fn heartbeat_fires_at_one_second_boundary() {
    let mut idle = IdleSuppressor::new();

    // Advance ticks_per_sec - 1 ticks: all must be Suppress.
    for i in 0..(TICKS_PER_SEC - 1) {
        let action = idle.observe(false, TICK_NS);
        assert_eq!(
            action,
            ScreenIdleAction::Suppress,
            "tick {i}: must be Suppress before 1 s elapses"
        );
    }

    // The next tick crosses the 1 s boundary → Heartbeat.
    let action = idle.observe(false, TICK_NS);
    assert_eq!(
        action,
        ScreenIdleAction::Heartbeat,
        "tick {} (= 1 s): must be Heartbeat",
        TICKS_PER_SEC - 1
    );
}

#[test]
fn second_heartbeat_fires_one_second_after_first() {
    let mut idle = IdleSuppressor::new();

    // Advance to first heartbeat.
    for _ in 0..TICKS_PER_SEC {
        idle.observe(false, TICK_NS);
    }

    // Now advance ticks_per_sec - 1 more ticks: all must be Suppress.
    for i in 0..(TICKS_PER_SEC - 1) {
        let action = idle.observe(false, TICK_NS);
        assert_eq!(
            action,
            ScreenIdleAction::Suppress,
            "inter-heartbeat tick {i}: must be Suppress"
        );
    }

    // Exactly one tick later: second Heartbeat.
    let action = idle.observe(false, TICK_NS);
    assert_eq!(
        action,
        ScreenIdleAction::Heartbeat,
        "second heartbeat must fire exactly {TICKS_PER_SEC} ticks after the first"
    );
}

// ── Part C: dirty frame resets idle timer ─────────────────────────────────────

#[test]
fn dirty_frame_at_0_9s_prevents_heartbeat_and_restarts_timer() {
    let mut idle = IdleSuppressor::new();

    // Advance 0.9 s (45 of 50 ticks) with no dirty rects.
    for _ in 0..(TICKS_PER_SEC * 9 / 10) {
        let a = idle.observe(false, TICK_NS);
        assert_eq!(a, ScreenIdleAction::Suppress);
    }

    // Dirty tick resets the accumulator; heartbeat must NOT fire here.
    let action = idle.observe(true, TICK_NS);
    assert_eq!(
        action,
        ScreenIdleAction::Send,
        "dirty tick must return Send even at 0.9 s of idle"
    );

    // Next TICKS_PER_SEC - 1 static ticks: still Suppress (timer reset by dirty).
    for i in 0..(TICKS_PER_SEC - 1) {
        let a = idle.observe(false, TICK_NS);
        assert_eq!(
            a,
            ScreenIdleAction::Suppress,
            "post-dirty tick {i}: idle timer was reset; must be Suppress"
        );
    }

    // One more tick → heartbeat fires 1 s after the dirty tick.
    let action = idle.observe(false, TICK_NS);
    assert_eq!(
        action,
        ScreenIdleAction::Heartbeat,
        "heartbeat must fire exactly 1 s after dirty tick reset the timer"
    );
}

#[test]
fn idle_timer_is_zero_immediately_after_dirty_tick() {
    let mut idle = IdleSuppressor::new();
    // Build up some idle time.
    for _ in 0..20 {
        idle.observe(false, TICK_NS);
    }
    let before = idle.idle_ns();
    assert!(before > 0, "idle_ns should be nonzero after 20 static ticks");

    // Dirty tick must reset it.
    idle.observe(true, TICK_NS);
    assert_eq!(
        idle.idle_ns(),
        0,
        "dirty tick must reset idle_ns to zero; got {}",
        idle.idle_ns()
    );
}

// ── Part D: Send on dirty ─────────────────────────────────────────────────────

#[test]
fn dirty_ticks_always_return_send() {
    let mut idle = IdleSuppressor::new();

    // Even with no accumulated idle time, dirty must return Send.
    for _ in 0..50 {
        let action = idle.observe(true, TICK_NS);
        assert_eq!(
            action,
            ScreenIdleAction::Send,
            "dirty tick must always return Send"
        );
    }
}

#[test]
fn send_resumes_immediately_after_long_idle() {
    let mut idle = IdleSuppressor::new();

    // 5 s of static — accumulates heartbeats.
    for _ in 0..(5 * TICKS_PER_SEC) {
        idle.observe(false, TICK_NS);
    }

    // Dirty frame must return Send right away.
    let action = idle.observe(true, TICK_NS);
    assert_eq!(
        action,
        ScreenIdleAction::Send,
        "dirty tick after 5 s of idle must return Send immediately"
    );
}

#[test]
fn mixed_sequence_has_zero_content_bytes_during_static_runs() {
    // A 20-second session: first 5 s active, then 10 s static, then 5 s active.
    // Verify zero Send events during the static middle.
    let active_secs = 5u64;
    let static_secs = 10u64;

    let mut idle = IdleSuppressor::new();
    let mut static_send = 0usize;

    // Active phase.
    for _ in 0..(active_secs * TICKS_PER_SEC) {
        idle.observe(true, TICK_NS);
    }

    // Static phase — count any erroneous Send events.
    for _ in 0..(static_secs * TICKS_PER_SEC) {
        if idle.observe(false, TICK_NS) == ScreenIdleAction::Send {
            static_send += 1;
        }
    }

    assert_eq!(
        static_send, 0,
        "static phase must produce zero Send events; got {static_send}"
    );

    // Active phase resumes.
    let action = idle.observe(true, TICK_NS);
    assert_eq!(action, ScreenIdleAction::Send);
}

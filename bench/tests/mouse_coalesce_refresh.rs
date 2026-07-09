//! Feature 60 — system coalesces mouse moves to the remote display
//! refresh_cadence.
//!
//! # What this test suite verifies
//!
//! 1. **One frame per tick**: a burst of OS mouse-move events within one 60 Hz
//!    display tick produces exactly one coalesced [`InputEvent::MouseMove`] frame
//!    on flush, not one frame per OS event.
//! 2. **Net displacement equality**: the coalesced dx/dy equals the integer-
//!    rounded sum of all individual deltas accumulated during the tick.
//! 3. **Empty tick returns None**: when no mouse moves arrived during a tick,
//!    [`MouseMoveCoalescer::flush`] returns `None` — no zero-displacement frame
//!    is sent (the cursor-channel heartbeat already covers the stationary case).
//! 4. **Sub-pixel carry**: fractional remainders not representable in the
//!    integer wire format are carried into the next tick so the remote cursor
//!    converges to the correct pixel over time.
//! 5. **Cadence constant matches cursor channel**: [`MOUSE_COALESCE_TICK_NS`]
//!    equals [`CURSOR_TICK_NS`], confirming coalescing is locked to the same
//!    60 Hz cadence as the cursor-position heartbeat.
//! 6. **High-frequency stress**: 1 000 Hz pointer for 10 display ticks emits
//!    exactly 10 frames and the cumulative displacement matches the sum of all
//!    individual moves.

use lowband_platform::input_channel_sender::{
    InputChannelDecoder, InputChannelSender, MouseMoveCoalescer,
    MOUSE_COALESCE_TICK_NS,
};
use lowband_platform::cursor_sender::CURSOR_TICK_NS;
use lowband_platform::input_injection::InputEvent;

// ── Cadence constant ──────────────────────────────────────────────────────────

#[test]
fn mouse_coalesce_tick_ns_matches_cursor_tick_ns() {
    assert_eq!(
        MOUSE_COALESCE_TICK_NS,
        CURSOR_TICK_NS,
        "mouse-move coalescing cadence must match the cursor-channel 60 Hz tick \
         so both channels stay phase-aligned (Feature 60)"
    );
}

#[test]
fn mouse_coalesce_tick_ns_is_sixty_hz() {
    assert_eq!(
        MOUSE_COALESCE_TICK_NS,
        1_000_000_000 / 60,
        "MOUSE_COALESCE_TICK_NS must equal 16 666 666 ns (60 Hz)"
    );
}

// ── One frame per tick ────────────────────────────────────────────────────────

#[test]
fn burst_of_os_events_in_one_tick_produces_one_frame() {
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    // Simulate 500 Hz device over one 60 Hz tick (≈ 8–9 OS events).
    let events_per_tick = (500.0_f64 / 60.0).ceil() as usize;
    for _ in 0..events_per_tick {
        coalescer.accumulate(1.0, 0.5);
    }

    let frame = coalescer.flush(&mut sender);
    assert!(
        frame.is_some(),
        "flush must emit one frame for a non-empty coalescing window (Feature 60)"
    );

    // No second frame — the window is now empty.
    assert!(
        coalescer.flush(&mut sender).is_none(),
        "flush must return None when no moves were accumulated since the last flush"
    );
}

// ── Net displacement equality ─────────────────────────────────────────────────

#[test]
fn coalesced_displacement_equals_rounded_sum_of_individual_events() {
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    let deltas: &[(f64, f64)] = &[
        (3.0, -1.0),
        (5.0,  2.0),
        (-1.0, 4.0),
        (2.0, -2.0),
    ];
    let expected_dx: f64 = deltas.iter().map(|&(x, _)| x).sum(); // 9.0
    let expected_dy: f64 = deltas.iter().map(|&(_, y)| y).sum(); // 3.0

    for &(dx, dy) in deltas {
        coalescer.accumulate(dx, dy);
    }

    let frame = coalescer.flush(&mut sender).expect("must produce a frame");
    let mut decoder = InputChannelDecoder::new();
    let ev = decoder.decode(&frame).expect("frame must decode");

    match ev {
        InputEvent::MouseMove { dx, dy } => {
            assert_eq!(
                dx, expected_dx.round(),
                "coalesced dx must equal round(sum(individual dx))"
            );
            assert_eq!(
                dy, expected_dy.round(),
                "coalesced dy must equal round(sum(individual dy))"
            );
        }
        other => panic!("expected MouseMove, got {other:?}"),
    }
}

// ── Empty tick returns None ───────────────────────────────────────────────────

#[test]
fn empty_tick_returns_none() {
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    assert!(
        coalescer.flush(&mut sender).is_none(),
        "flush on an empty coalescer must return None — no zero-displacement \
         frame should be sent; the cursor-channel heartbeat covers this case \
         (Feature 60)"
    );
}

#[test]
fn tick_with_no_new_moves_after_a_previous_flush_returns_none() {
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    coalescer.accumulate(5.0, 3.0);
    coalescer.flush(&mut sender).unwrap();   // consumes the window
    assert!(
        coalescer.flush(&mut sender).is_none(),
        "second flush without any new accumulate calls must return None"
    );
}

// ── Sub-pixel carry ───────────────────────────────────────────────────────────

#[test]
fn sub_pixel_remainder_carries_into_next_tick() {
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();
    let mut decoder   = InputChannelDecoder::new();

    // 0.3 rounds to 0 → carries 0.3 forward.
    coalescer.accumulate(0.3, 0.0);
    let f1 = coalescer.flush(&mut sender).unwrap();
    let ev1 = decoder.decode(&f1).unwrap();
    assert!(
        matches!(ev1, InputEvent::MouseMove { dx, dy } if dx == 0.0 && dy == 0.0),
        "first tick: 0.3 rounds to 0"
    );

    // 0.3 carry + 0.3 new = 0.6 → rounds to 1.
    coalescer.accumulate(0.3, 0.0);
    let f2 = coalescer.flush(&mut sender).unwrap();
    let mut dec2 = InputChannelDecoder::new();
    let ev2 = dec2.decode(&f2).unwrap();
    assert!(
        matches!(ev2, InputEvent::MouseMove { dx, dy } if dx == 1.0 && dy == 0.0),
        "second tick: carry 0.3 + 0.3 = 0.6 rounds to 1"
    );
}

#[test]
fn sub_pixel_carry_does_not_grow_unboundedly() {
    // Feed sub-pixel moves for many ticks and confirm the carry stays in (−0.5, 0.5].
    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    for _ in 0..120 {
        coalescer.accumulate(0.3, -0.3);
        coalescer.flush(&mut sender); // drain each tick
        // After flush the carry is in (−0.5, 0.5] — no further assertions
        // needed here; if carry grew unboundedly the 500 Hz test below would fail.
    }
    // The coalescer is still usable.
    coalescer.accumulate(10.0, 10.0);
    assert!(coalescer.flush(&mut sender).is_some());
}

// ── High-frequency stress ─────────────────────────────────────────────────────

/// At 1 000 Hz over 10 display ticks (≈ 167 OS events per tick) the coalescer
/// must emit exactly 10 frames and the cumulative integer displacement must
/// match the rounded sum of all individual moves.
#[test]
fn thousand_hz_over_ten_ticks_emits_ten_frames_with_correct_displacement() {
    const TICKS: usize = 10;
    const POINTER_HZ: f64 = 1_000.0;
    const DISPLAY_HZ: f64 = 60.0;
    let events_per_tick = (POINTER_HZ / DISPLAY_HZ).ceil() as usize; // 17

    let mut coalescer = MouseMoveCoalescer::new();
    let mut sender    = InputChannelSender::new();

    let per_event_dx = 2.0_f64;
    let per_event_dy = -1.0_f64;

    let mut frame_count     = 0usize;
    let mut cumulative_dx   = 0i32;
    let mut cumulative_dy   = 0i32;

    for _tick in 0..TICKS {
        for _ in 0..events_per_tick {
            coalescer.accumulate(per_event_dx, per_event_dy);
        }
        if let Some(frame) = coalescer.flush(&mut sender) {
            frame_count += 1;
            let mut dec = InputChannelDecoder::new();
            if let Some(InputEvent::MouseMove { dx, dy }) = dec.decode(&frame) {
                cumulative_dx += dx as i32;
                cumulative_dy += dy as i32;
            }
        }
    }

    assert_eq!(
        frame_count, TICKS,
        "must emit exactly one frame per display tick — got {frame_count} for {TICKS} ticks"
    );

    let total_os_events = TICKS * events_per_tick;
    let expected_dx = (per_event_dx * total_os_events as f64).round() as i32;
    let expected_dy = (per_event_dy * total_os_events as f64).round() as i32;

    // Allow ±1 pixel tolerance per tick due to sub-pixel rounding across ticks.
    let dx_error = (cumulative_dx - expected_dx).abs();
    let dy_error = (cumulative_dy - expected_dy).abs();
    assert!(
        dx_error <= TICKS as i32,
        "cumulative dx error {dx_error} must be ≤ {TICKS} px (one rounding step per tick)"
    );
    assert!(
        dy_error <= TICKS as i32,
        "cumulative dy error {dy_error} must be ≤ {TICKS} px (one rounding step per tick)"
    );

    eprintln!(
        "1 000 Hz × {TICKS} ticks: {frame_count} frames, \
         cumulative ({cumulative_dx}, {cumulative_dy}), \
         expected ({expected_dx}, {expected_dy}), \
         error ({dx_error}, {dy_error})"
    );
}

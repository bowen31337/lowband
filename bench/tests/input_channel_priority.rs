//! Feature 61 — system carries input events on the reliable-ordered channel
//! with scheduling_priority at the top.
//!
//! # What this test suite verifies
//!
//! 1. **Channel identity**: input events go on LBTP channel 3
//!    ([`INPUT_CHANNEL_ID`]).
//! 2. **Delivery class**: channel 3 is `ReliableOrdered` — keyboard and button
//!    events arrive exactly once, in sender order, with retransmission.
//! 3. **Scheduling priority at the top**: among all application data channels
//!    (audio ch 1, cursor ch 2, screen-rt ch 4, video-rt ch 5, bulk ch 6,
//!    xfer ch 7, probes ch 8) input holds the first position in
//!    `PRIORITY_ORDER`.  Only the transport-level ctrl/ACK channel (ch 0)
//!    takes precedence.
//! 4. **Varint delta coding** (Feature 59): the encoder produces compact
//!    2-byte frames for typical keystrokes (same or adjacent keycodes) and
//!    correct round-trips for all event types across a full session sequence.
//! 5. **End-to-end pacer integration**: encoded input frames enqueued on
//!    channel 3 drain before audio, screen-rt, and video-rt frames at every
//!    pacing tick, confirming the scheduling guarantee at the transport layer.

use lowband_lbtp::pacer::{
    ChannelId, DeliveryClass, CHANNEL_DELIVERY_CLASS, PRIORITY_ORDER,
    Pacer, PacerFrame,
};
use lowband_platform::input_channel_sender::{
    InputChannelDecoder, InputChannelSender,
    INPUT_CHANNEL_ID, SCHEDULING_PRIORITY_RANK, MAX_INPUT_FRAME_BYTES,
};
use lowband_platform::input_injection::{InputEvent, MouseButton};

// ── Channel-3 identity and delivery class ────────────────────────────────────

/// Channel 3 must be the input event channel by convention.
#[test]
fn input_channel_id_is_three() {
    assert_eq!(
        INPUT_CHANNEL_ID, 3,
        "input events must travel on LBTP channel 3 (Feature 61)"
    );
}

/// Channel 3 must carry the `ReliableOrdered` delivery class.
///
/// `ReliableOrdered` guarantees that every input frame is retransmitted until
/// acknowledged and delivered in the order it was sent.  This is essential for
/// keyboard events (key-down before key-up) and click sequences.
#[test]
fn channel_3_is_reliable_ordered() {
    assert_eq!(
        CHANNEL_DELIVERY_CLASS[INPUT_CHANNEL_ID as usize],
        DeliveryClass::ReliableOrdered,
        "LBTP channel 3 must be ReliableOrdered so input events survive packet \
         loss and arrive in order (Feature 61)"
    );
}

/// Verify that `ChannelId(3).delivery_class()` agrees with the table.
#[test]
fn channel_id_3_delivery_class_method_agrees() {
    assert_eq!(
        ChannelId::new(INPUT_CHANNEL_ID).delivery_class(),
        DeliveryClass::ReliableOrdered,
    );
}

// ── Scheduling priority at the top ───────────────────────────────────────────

/// Channel 3 must appear at [`SCHEDULING_PRIORITY_RANK`] (index 1) in
/// `PRIORITY_ORDER`, making it second only to ctrl/ACK (channel 0).
#[test]
fn input_channel_is_at_scheduling_priority_rank() {
    assert_eq!(
        PRIORITY_ORDER[SCHEDULING_PRIORITY_RANK],
        INPUT_CHANNEL_ID,
        "channel 3 (input) must be at index {SCHEDULING_PRIORITY_RANK} in \
         PRIORITY_ORDER — second only to ctrl/ACK (Feature 61)"
    );
}

/// Channel 3 must appear exactly once in `PRIORITY_ORDER`.
#[test]
fn input_channel_appears_exactly_once_in_priority_order() {
    let count = PRIORITY_ORDER.iter().filter(|&&c| c == INPUT_CHANNEL_ID).count();
    assert_eq!(count, 1, "channel {INPUT_CHANNEL_ID} must appear exactly once in PRIORITY_ORDER");
}

/// Input (ch 3) must rank above audio (ch 1) in the priority order.
#[test]
fn input_channel_beats_audio_in_priority_order() {
    let input_rank = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
    let audio_rank = PRIORITY_ORDER.iter().position(|&c| c == 1).unwrap();
    assert!(
        input_rank < audio_rank,
        "input channel (rank {input_rank}) must beat audio channel (rank {audio_rank}) \
         in PRIORITY_ORDER (Feature 61: scheduling_priority at the top)"
    );
}

/// Input (ch 3) must rank above cursor (ch 2).
#[test]
fn input_channel_beats_cursor_in_priority_order() {
    let input_rank  = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
    let cursor_rank = PRIORITY_ORDER.iter().position(|&c| c == 2).unwrap();
    assert!(
        input_rank < cursor_rank,
        "input (rank {input_rank}) must beat cursor (rank {cursor_rank})"
    );
}

/// Input (ch 3) must rank above screen-rt (ch 4).
#[test]
fn input_channel_beats_screen_rt_in_priority_order() {
    let input_rank  = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
    let screen_rank = PRIORITY_ORDER.iter().position(|&c| c == 4).unwrap();
    assert!(
        input_rank < screen_rank,
        "input (rank {input_rank}) must beat screen-rt (rank {screen_rank})"
    );
}

/// Input (ch 3) must rank above video-rt (ch 5).
#[test]
fn input_channel_beats_video_rt_in_priority_order() {
    let input_rank = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
    let video_rank = PRIORITY_ORDER.iter().position(|&c| c == 5).unwrap();
    assert!(
        input_rank < video_rank,
        "input (rank {input_rank}) must beat video-rt (rank {video_rank})"
    );
}

/// Input (ch 3) must rank above all bulk/xfer/probe channels (6, 7, 8).
#[test]
fn input_channel_beats_all_lower_channels() {
    let input_rank = PRIORITY_ORDER.iter().position(|&c| c == INPUT_CHANNEL_ID).unwrap();
    for &lower_ch in &[6u8, 7, 8] {
        let lower_rank = PRIORITY_ORDER.iter().position(|&c| c == lower_ch).unwrap();
        assert!(
            input_rank < lower_rank,
            "input (rank {input_rank}) must beat channel {lower_ch} (rank {lower_rank})"
        );
    }
}

// ── Pacer integration: input drains before media ──────────────────────────────

/// When an input frame and an audio frame are both queued, the input frame
/// drains first regardless of enqueue order.
#[test]
fn input_frame_drains_before_audio_in_pacer() {
    let mut pacer = Pacer::new(1_000_000.0); // large rate — no token starvation
    pacer.advance(1_000_000_000); // advance 1 s worth of tokens

    // Enqueue audio first, then input — arrival order must not matter.
    pacer.enqueue(PacerFrame::new(ChannelId::new(1), vec![0u8; 80])); // audio
    pacer.enqueue(PacerFrame::new(ChannelId::new(INPUT_CHANNEL_ID), vec![0u8; 32])); // input

    let first = pacer.dequeue().expect("a frame should be available");
    assert_eq!(
        first.channel.0,
        INPUT_CHANNEL_ID,
        "input frame must drain before audio frame — scheduling_priority at the top \
         (Feature 61)"
    );

    let second = pacer.dequeue().expect("audio frame should follow");
    assert_eq!(second.channel.0, 1, "audio frame must be the second to drain");
}

/// When input, screen-rt, and video-rt frames are queued together, input drains
/// first, followed by screen-rt and then video-rt (per PRIORITY_ORDER).
#[test]
fn input_drains_before_screen_and_video_in_pacer() {
    // Use 10 Mbps so the burst cap (10_000_000 × 5 / 8_000 = 6 250 bytes) easily
    // covers all three frames.
    let mut pacer = Pacer::new(10_000_000.0);
    pacer.advance(1_000_000_000);

    pacer.enqueue(PacerFrame::new(ChannelId::new(5), vec![0u8; 400])); // video-rt
    pacer.enqueue(PacerFrame::new(ChannelId::new(4), vec![0u8; 300])); // screen-rt
    pacer.enqueue(PacerFrame::new(ChannelId::new(INPUT_CHANNEL_ID), vec![0u8; 6])); // input

    let first = pacer.dequeue().unwrap();
    assert_eq!(first.channel.0, INPUT_CHANNEL_ID, "input must drain first");

    let second = pacer.dequeue().unwrap();
    assert_eq!(second.channel.0, 4, "screen-rt must drain second");

    let third = pacer.dequeue().unwrap();
    assert_eq!(third.channel.0, 5, "video-rt must drain third");
}

/// Verify that `drain_tick` — which coalesces multiple frames into one datagram
/// — places the input frame before any media frames.
#[test]
fn drain_tick_orders_input_before_audio_and_video() {
    let mut pacer = Pacer::new(1_000_000.0);
    pacer.advance(1_000_000_000);

    pacer.enqueue(PacerFrame::new(ChannelId::new(1), vec![0u8; 20]));  // audio
    pacer.enqueue(PacerFrame::new(ChannelId::new(5), vec![0u8; 20]));  // video-rt
    pacer.enqueue(PacerFrame::new(ChannelId::new(INPUT_CHANNEL_ID), vec![0u8; 6])); // input

    let agg = pacer.drain_tick().expect("should produce an aggregated datagram");
    assert_eq!(
        agg.frames[0].channel.0,
        INPUT_CHANNEL_ID,
        "input must be the first frame in the aggregated datagram (Feature 61)"
    );
}

// ── Varint delta coding (Feature 59) ─────────────────────────────────────────

/// A `KeyPress` on the first event (delta from 0) should encode the keycode
/// directly as a varint.
#[test]
fn first_key_press_encodes_keycode_as_delta_from_zero() {
    let mut sender = InputChannelSender::new();
    let bytes = sender.encode(InputEvent::KeyPress { keycode: 10 }).unwrap();
    // delta = 10 − 0 = 10; 10 < 64 → 1-byte varint (0x0A)
    assert_eq!(bytes.len(), 2, "discriminant + 1-byte varint for small keycode");
    assert_eq!(bytes[1], 10);
}

/// Typing the same key twice: the second event has delta = 0 → 1-byte varint.
#[test]
fn same_key_twice_produces_zero_delta() {
    let mut sender = InputChannelSender::new();
    sender.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap();
    let bytes = sender.encode(InputEvent::KeyRelease { keycode: 0x41 }).unwrap();
    assert_eq!(bytes[1], 0x00, "repeated keycode → delta=0");
    assert_eq!(bytes.len(), 2);
}

/// Adjacent keys on a standard keyboard (e.g., A=0x41, B=0x42) produce a
/// delta of ±1 that encodes in 1 byte.
#[test]
fn adjacent_keys_encode_in_one_byte_varint() {
    let mut sender = InputChannelSender::new();
    sender.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap(); // A
    let bytes = sender.encode(InputEvent::KeyPress { keycode: 0x42 }).unwrap(); // B
    assert_eq!(bytes.len(), 2, "delta=1 must encode in 1-byte varint");
}

/// A mouse move with small deltas encodes in 3 bytes
/// (1 discriminant + 1 varint dx + 1 varint dy).
#[test]
fn small_mouse_move_encodes_in_three_bytes() {
    let mut sender = InputChannelSender::new();
    let bytes = sender.encode(InputEvent::MouseMove { dx: 5.0, dy: -3.0 }).unwrap();
    assert_eq!(bytes.len(), 3, "small MouseMove must fit in 3 bytes");
}

/// A mouse button event always encodes in exactly 2 bytes.
#[test]
fn mouse_button_encodes_in_two_bytes() {
    let mut sender = InputChannelSender::new();
    let p = sender.encode(InputEvent::MouseButtonPress { button: MouseButton::Left }).unwrap();
    let r = sender.encode(InputEvent::MouseButtonRelease { button: MouseButton::Right }).unwrap();
    assert_eq!(p.len(), 2);
    assert_eq!(r.len(), 2);
}

// ── End-to-end round-trip through the pacer ──────────────────────────────────

/// Encode a realistic session fragment — modifier key, letter key, click, mouse
/// move — through `InputChannelSender`, wrap each in a `PacerFrame` on channel
/// 3, enqueue into the pacer alongside audio and screen-rt frames, and verify:
/// 1. All input frames drain before the audio frame.
/// 2. The decoded event sequence is identical to the original.
#[test]
fn end_to_end_input_sequence_drains_before_media() {
    let events: &[InputEvent] = &[
        InputEvent::KeyPress   { keycode: 0x11 }, // Ctrl
        InputEvent::KeyPress   { keycode: 0x43 }, // C
        InputEvent::KeyRelease { keycode: 0x43 },
        InputEvent::KeyRelease { keycode: 0x11 },
        InputEvent::MouseMove  { dx: 8.0, dy: -4.0 },
        InputEvent::MouseButtonPress   { button: MouseButton::Left },
        InputEvent::MouseButtonRelease { button: MouseButton::Left },
    ];

    let mut sender  = InputChannelSender::new();
    let mut decoder = InputChannelDecoder::new();
    let mut pacer   = Pacer::new(1_000_000.0);
    pacer.advance(1_000_000_000);

    // Enqueue an audio frame that should lose the priority race.
    pacer.enqueue(PacerFrame::new(ChannelId::new(1), vec![0u8; 80]));

    // Encode each event and enqueue as a channel-3 frame.
    let mut encoded_payloads: Vec<Vec<u8>> = Vec::new();
    for &ev in events {
        let bytes = sender.encode(ev).expect("encode must succeed");
        assert!(bytes.len() <= MAX_INPUT_FRAME_BYTES, "frame exceeds MTU limit");
        encoded_payloads.push(bytes.clone());
        pacer.enqueue(PacerFrame::new(ChannelId::new(INPUT_CHANNEL_ID), bytes));
    }

    // Drain the pacer; collect channel assignments in order.
    let mut channel_drain_order: Vec<u8> = Vec::new();
    let mut decoded_events: Vec<InputEvent> = Vec::new();

    while let Some(frame) = pacer.dequeue() {
        channel_drain_order.push(frame.channel.0);
        if frame.channel.0 == INPUT_CHANNEL_ID {
            if let Some(ev) = decoder.decode(&frame.data) {
                decoded_events.push(ev);
            }
        }
    }

    // All input frames (ch 3) must appear before the audio frame (ch 1).
    let last_input_pos = channel_drain_order
        .iter()
        .rposition(|&c| c == INPUT_CHANNEL_ID)
        .expect("at least one input frame must have drained");
    let audio_pos = channel_drain_order
        .iter()
        .position(|&c| c == 1)
        .expect("audio frame must have drained");

    assert!(
        last_input_pos < audio_pos,
        "all input frames must drain before the audio frame — \
         scheduling_priority at the top (Feature 61); \
         drain order was: {:?}",
        channel_drain_order
    );

    // Decoded events must match the originals.
    assert_eq!(decoded_events.len(), events.len(), "all events must decode successfully");
    for (original, decoded) in events.iter().zip(decoded_events.iter()) {
        match (original, decoded) {
            (InputEvent::KeyPress   { keycode: a }, InputEvent::KeyPress   { keycode: b }) => assert_eq!(a, b),
            (InputEvent::KeyRelease { keycode: a }, InputEvent::KeyRelease { keycode: b }) => assert_eq!(a, b),
            (InputEvent::MouseMove  { dx: ax, dy: ay }, InputEvent::MouseMove { dx: bx, dy: by }) => {
                assert_eq!(ax, bx);
                assert_eq!(ay, by);
            }
            (InputEvent::MouseButtonPress   { button: a }, InputEvent::MouseButtonPress   { button: b }) => assert_eq!(a, b),
            (InputEvent::MouseButtonRelease { button: a }, InputEvent::MouseButtonRelease { button: b }) => assert_eq!(a, b),
            _ => panic!("event type mismatch: original={original:?}, decoded={decoded:?}"),
        }
    }
}

// ── Wire-cost: input events are negligible on the constrained link ────────────

/// A burst of 10 typical input events (6 keyboard + 4 mouse) must total fewer
/// than 100 bytes on the wire — negligible next to the 2 kbps input_cost ceiling
/// documented in Feature 63 (1 920 bps steady-state from cursor alone).
///
/// This confirms that varint delta coding keeps event overhead well within budget.
#[test]
fn ten_typical_events_fit_in_100_bytes() {
    let events: &[InputEvent] = &[
        InputEvent::KeyPress   { keycode: 0x41 },      // A press
        InputEvent::KeyRelease { keycode: 0x41 },
        InputEvent::KeyPress   { keycode: 0x42 },      // B press (Δ=1 from A)
        InputEvent::KeyRelease { keycode: 0x42 },
        InputEvent::MouseMove  { dx: 5.0, dy: -3.0 },
        InputEvent::MouseMove  { dx: 10.0, dy: 0.0 },
        InputEvent::MouseButtonPress   { button: MouseButton::Left },
        InputEvent::MouseButtonRelease { button: MouseButton::Left },
        InputEvent::KeyPress   { keycode: 0x0D },      // Enter
        InputEvent::KeyRelease { keycode: 0x0D },
    ];

    let mut sender = InputChannelSender::new();
    let total_bytes: usize = events
        .iter()
        .map(|&ev| sender.encode(ev).unwrap().len())
        .sum();

    assert!(
        total_bytes < 100,
        "10 typical input events must fit in fewer than 100 wire bytes; \
         got {total_bytes} bytes — varint delta coding is not achieving \
         expected compression"
    );

    eprintln!(
        "10 typical input events = {total_bytes} bytes on the wire \
         (varint delta coded, Feature 59)"
    );
}

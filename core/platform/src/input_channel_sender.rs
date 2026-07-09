//! Input event encoder/decoder for the reliable-ordered input channel —
//! Features 59, 60, and 61.
//!
//! # Feature 59 — varint delta coding
//!
//! Input events are encoded compactly:
//! - A 1-byte discriminant identifies the event type.
//! - **Keyboard**: the keycode is delta-encoded relative to the last transmitted
//!   keycode, then stored as a signed LEB128 varint.  Typical keystroke sequences
//!   (adjacent keys, modifier pairs) produce deltas in `−64..=63`, encoding in
//!   **1 byte** rather than the 4 bytes a bare `u32` would require.
//! - **Mouse move**: `(dx, dy)` are signed pixel deltas, already first-order so
//!   no further differencing is applied.  Values in `−64..=63` encode in 1 byte.
//! - **Mouse button**: absolute button index (1 byte), no delta.
//!
//! # Wire format
//!
//! | Byte(s) | Field        | Encoding               |
//! |---------|--------------|------------------------|
//! | 1       | discriminant | fixed u8 (table below) |
//! | 1–5     | payload      | signed LEB128 or u8    |
//!
//! Discriminant table:
//!
//! | Value  | Event                | Payload                            |
//! |--------|----------------------|------------------------------------|
//! | `0x01` | `KeyPress`           | signed LEB128 keycode delta        |
//! | `0x02` | `KeyRelease`         | signed LEB128 keycode delta        |
//! | `0x10` | `MouseMove`          | signed LEB128 dx, signed LEB128 dy |
//! | `0x20` | `MouseButtonPress`   | 1 byte: 0=Left, 1=Right, 2=Middle  |
//! | `0x21` | `MouseButtonRelease` | 1 byte: 0=Left, 1=Right, 2=Middle  |
//!
//! Worst-case frame size:
//! - `KeyPress`/`KeyRelease`: 1 + 5 = 6 bytes (32-bit keycode delta, extreme case)
//! - `MouseMove`: 1 + 5 + 5 = 11 bytes (extreme dx and dy)
//! - `MouseButtonPress`/`Release`: 1 + 1 = 2 bytes
//!
//! All are well within the 1 179-byte LBTP frame data limit.
//!
//! # Feature 61 — reliable-ordered channel with top scheduling priority
//!
//! Encoded frames are placed on **LBTP channel 3** (the reliable-ordered input
//! event channel).  The LBTP pacer's `PRIORITY_ORDER` positions channel 3 at
//! index [`SCHEDULING_PRIORITY_RANK`] — second only to ctrl/ACK (channel 0),
//! which carries transport-level protocol frames rather than application data.
//! Among all application data channels (audio ch 1, cursor ch 2, screen-rt ch
//! 4, video-rt ch 5, bulk ch 6, xfer ch 7, probes ch 8) input events hold the
//! **top** scheduling slot: every input frame drains before any media frame.
//!
//! The delivery class of channel 3 is `ReliableOrdered`, ensuring that keyboard
//! and button events arrive exactly once, in sender order, across retransmissions.
//!
//! # Usage
//!
//! ```
//! use lowband_platform::input_channel_sender::{
//!     InputChannelSender, INPUT_CHANNEL_ID,
//! };
//! use lowband_platform::input_injection::{InputEvent, MouseButton};
//!
//! let mut sender = InputChannelSender::new();
//!
//! // Encode 'A' key-press into bytes for LBTP channel 3.
//! let bytes = sender.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap();
//! // Hand `bytes` to the LBTP pacer as a PacerFrame on channel 3.
//! assert!(bytes.len() <= lowband_platform::input_channel_sender::MAX_INPUT_FRAME_BYTES);
//! ```

use crate::input_injection::{InputEvent, MouseButton};

// ── Mouse-move coalescing cadence (Feature 60) ───────────────────────────────

/// Nanosecond interval for coalescing mouse-move events to the display
/// refresh cadence.
///
/// Matches `CURSOR_TICK_NS` from `cursor_sender` (1 000 000 000 / 60 ≈ 16.67 ms).
/// The session loop calls [`MouseMoveCoalescer::flush`] once per this interval.
pub const MOUSE_COALESCE_TICK_NS: u64 = 1_000_000_000 / 60;

// ── Channel constants ─────────────────────────────────────────────────────────

/// LBTP channel number for the reliable-ordered input event stream.
///
/// The LBTP architecture spec §6.2 reserves channel 3 for input events with
/// delivery class `ReliableOrdered` and scheduling priority second only to
/// ctrl/ACK (channel 0).
pub const INPUT_CHANNEL_ID: u8 = 3;

/// Index of channel 3 within `PRIORITY_ORDER` (from `lowband_lbtp::pacer`).
///
/// `PRIORITY_ORDER = [0, 3, 2, 1, 4, 5, 6, 7, 8]` — channel 3 sits at index 1,
/// giving it second-highest scheduling priority across all 9 channels.  Among
/// application data channels it is **first**: every input frame drains before
/// audio, screen-rt, video-rt, bulk, xfer, or probe frames.
pub const SCHEDULING_PRIORITY_RANK: usize = 1;

/// Maximum bytes a single encoded input event may occupy, matching the LBTP
/// per-frame data limit: `1 200 − 19 (AEAD envelope) − 2 (frame header) = 1 179`.
///
/// In practice, input frames are 2–11 bytes and never approach this ceiling.
pub const MAX_INPUT_FRAME_BYTES: usize = 1_179;

// ── Discriminant constants ────────────────────────────────────────────────────

const DISC_KEY_PRESS:      u8 = 0x01;
const DISC_KEY_RELEASE:    u8 = 0x02;
const DISC_MOUSE_MOVE:     u8 = 0x10;
const DISC_BUTTON_PRESS:   u8 = 0x20;
const DISC_BUTTON_RELEASE: u8 = 0x21;

// ── Button wire-byte constants ────────────────────────────────────────────────

const WIRE_LEFT:   u8 = 0;
const WIRE_RIGHT:  u8 = 1;
const WIRE_MIDDLE: u8 = 2;

fn button_to_wire(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left   => WIRE_LEFT,
        MouseButton::Right  => WIRE_RIGHT,
        MouseButton::Middle => WIRE_MIDDLE,
    }
}

fn wire_to_button(byte: u8) -> Option<MouseButton> {
    match byte {
        WIRE_LEFT   => Some(MouseButton::Left),
        WIRE_RIGHT  => Some(MouseButton::Right),
        WIRE_MIDDLE => Some(MouseButton::Middle),
        _           => None,
    }
}

// ── Signed LEB128 varint encoding (Feature 59) ───────────────────────────────

/// Encode a signed 32-bit integer as a signed LEB128 varint into `buf`.
///
/// Byte counts by range:
/// - `−64..=63` → **1 byte** (covers most mouse deltas and keycode offsets)
/// - `−8 192..=8 191` → 2 bytes
/// - `−1 048 576..=1 048 575` → 3 bytes
/// - `−134 217 728..=134 217 727` → 4 bytes
/// - all other `i32` values → 5 bytes
pub fn encode_varint_i32(mut value: i32, buf: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7; // arithmetic right shift — sign extends
        let finished = (value == 0 && (byte & 0x40) == 0)
            || (value == -1 && (byte & 0x40) != 0);
        if finished {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Decode a signed LEB128 varint from `bytes`.
///
/// Returns `(value, bytes_consumed)` on success, or `None` if `bytes` is
/// empty or the varint extends beyond the end of the slice.
pub fn decode_varint_i32(bytes: &[u8]) -> Option<(i32, usize)> {
    let mut result: i32 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        let bits = (byte & 0x7f) as i32;
        result |= bits << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            // Sign-extend: if the sign bit of the last group is set and we
            // haven't filled all 32 bits, set the upper bits to 1.
            if shift < 32 && (byte & 0x40) != 0 {
                result |= -(1i32 << shift);
            }
            return Some((result, i + 1));
        }
        if shift >= 35 {
            // A valid i32 varint requires at most ⌈32/7⌉ = 5 groups (35 bits).
            return None;
        }
    }
    None // truncated
}

// ── InputChannelSender ────────────────────────────────────────────────────────

/// Stateful input event encoder for LBTP channel 3.
///
/// Maintains the last transmitted keycode to apply delta coding (Feature 59).
/// Construct one `InputChannelSender` per session; call [`encode`](Self::encode)
/// on each [`InputEvent`] to obtain the compact frame bytes, then wrap them in
/// a `PacerFrame` on [`INPUT_CHANNEL_ID`] and submit to the LBTP pacer.
///
/// # Scheduling priority
///
/// By placing frames on channel 3 the caller benefits automatically from the
/// pacer's `PRIORITY_ORDER` — input frames drain before every media frame
/// (audio, screen-rt, video-rt) at every pacing tick (Feature 61).
///
/// # Example
///
/// ```
/// use lowband_platform::input_channel_sender::InputChannelSender;
/// use lowband_platform::input_injection::InputEvent;
///
/// let mut sender = InputChannelSender::new();
///
/// // Encode 'A' (VK_A / KEY_A / CGKeyCode 0).
/// let b = sender.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap();
/// assert_eq!(b[0], 0x01);      // KeyPress discriminant
/// assert!(b.len() <= 6);       // varint delta ≤ 5 bytes
/// ```
#[derive(Debug, Default)]
pub struct InputChannelSender {
    /// Keycode of the last transmitted keyboard event; used for delta coding.
    last_keycode: u32,
}

impl InputChannelSender {
    /// Create a new sender with initial keycode state at zero.
    pub fn new() -> Self {
        Self { last_keycode: 0 }
    }

    /// Encode `event` into the compact varint-delta wire format.
    ///
    /// Returns the frame bytes, or `None` if the encoded length would exceed
    /// [`MAX_INPUT_FRAME_BYTES`] (which cannot happen for any valid input event
    /// — the maximum is 11 bytes).
    ///
    /// For `KeyPress` and `KeyRelease`, the sender updates its `last_keycode`
    /// so the next keyboard event is delta-coded relative to this one.
    pub fn encode(&mut self, event: InputEvent) -> Option<Vec<u8>> {
        let mut buf = Vec::with_capacity(11);
        match event {
            InputEvent::KeyPress { keycode } => {
                buf.push(DISC_KEY_PRESS);
                let delta = keycode as i32 - self.last_keycode as i32;
                encode_varint_i32(delta, &mut buf);
                self.last_keycode = keycode;
            }
            InputEvent::KeyRelease { keycode } => {
                buf.push(DISC_KEY_RELEASE);
                let delta = keycode as i32 - self.last_keycode as i32;
                encode_varint_i32(delta, &mut buf);
                self.last_keycode = keycode;
            }
            InputEvent::MouseMove { dx, dy } => {
                buf.push(DISC_MOUSE_MOVE);
                encode_varint_i32(dx.round() as i32, &mut buf);
                encode_varint_i32(dy.round() as i32, &mut buf);
            }
            InputEvent::MouseButtonPress { button } => {
                buf.push(DISC_BUTTON_PRESS);
                buf.push(button_to_wire(button));
            }
            InputEvent::MouseButtonRelease { button } => {
                buf.push(DISC_BUTTON_RELEASE);
                buf.push(button_to_wire(button));
            }
        }
        if buf.len() <= MAX_INPUT_FRAME_BYTES { Some(buf) } else { None }
    }
}

// ── InputChannelDecoder ───────────────────────────────────────────────────────

/// Stateful decoder for frames received on LBTP channel 3.
///
/// Maintains the `last_keycode` mirror to reconstruct keycode deltas in order.
/// Construct one `InputChannelDecoder` per session on the receiver side and call
/// [`decode`](Self::decode) for each in-order channel-3 frame.
///
/// The `ReliableOrdered` delivery class of channel 3 guarantees that frames
/// arrive exactly once and in the order they were sent, so the decoder's
/// keycode state stays in sync with the sender's state automatically.
#[derive(Debug, Default)]
pub struct InputChannelDecoder {
    last_keycode: u32,
}

impl InputChannelDecoder {
    /// Create a new decoder with initial keycode state at zero.
    pub fn new() -> Self {
        Self { last_keycode: 0 }
    }

    /// Decode the bytes of one channel-3 frame into an [`InputEvent`].
    ///
    /// Returns `None` if the bytes are malformed, truncated, or contain an
    /// unknown discriminant.
    pub fn decode(&mut self, bytes: &[u8]) -> Option<InputEvent> {
        let (&disc, rest) = bytes.split_first()?;
        match disc {
            DISC_KEY_PRESS => {
                let (delta, _) = decode_varint_i32(rest)?;
                let keycode = (self.last_keycode as i32).checked_add(delta)? as u32;
                self.last_keycode = keycode;
                Some(InputEvent::KeyPress { keycode })
            }
            DISC_KEY_RELEASE => {
                let (delta, _) = decode_varint_i32(rest)?;
                let keycode = (self.last_keycode as i32).checked_add(delta)? as u32;
                self.last_keycode = keycode;
                Some(InputEvent::KeyRelease { keycode })
            }
            DISC_MOUSE_MOVE => {
                let (dx, consumed) = decode_varint_i32(rest)?;
                let (dy, _) = decode_varint_i32(&rest[consumed..])?;
                Some(InputEvent::MouseMove { dx: dx as f64, dy: dy as f64 })
            }
            DISC_BUTTON_PRESS => {
                let button = wire_to_button(*rest.first()?)?;
                Some(InputEvent::MouseButtonPress { button })
            }
            DISC_BUTTON_RELEASE => {
                let button = wire_to_button(*rest.first()?)?;
                Some(InputEvent::MouseButtonRelease { button })
            }
            _ => None,
        }
    }
}

// ── MouseMoveCoalescer (Feature 60) ──────────────────────────────────────────

/// Coalesces mouse-move events to the remote display refresh cadence (60 Hz).
///
/// High-precision pointer devices generate 200–1 000 `MouseMove` events per
/// second, but the remote display refreshes at only 60 Hz.  Sending every
/// individual event wastes bandwidth and adds LBTP framing overhead with no
/// perceptible benefit.
///
/// `MouseMoveCoalescer` accumulates all `(dx, dy)` deltas that arrive within
/// one display tick and emits exactly one coalesced [`InputEvent::MouseMove`]
/// frame when [`flush`](Self::flush) is called.  Sub-pixel remainders are
/// carried forward so the remote cursor converges to the correct pixel even at
/// very slow pointer speeds.
///
/// # Usage
///
/// ```
/// use lowband_platform::input_channel_sender::{
///     InputChannelSender, MouseMoveCoalescer,
/// };
///
/// let mut coalescer = MouseMoveCoalescer::new();
/// let mut sender    = InputChannelSender::new();
///
/// // OS events at 500 Hz — accumulate each one.
/// coalescer.accumulate(2.0, -1.0);
/// coalescer.accumulate(3.0,  1.5);
/// coalescer.accumulate(1.0, -0.5);
///
/// // Once per 60 Hz display tick, flush one coalesced frame.
/// let frame = coalescer.flush(&mut sender);
/// assert!(frame.is_some()); // moves were accumulated
///
/// // A flush with no new moves returns None.
/// let empty = coalescer.flush(&mut sender);
/// assert!(empty.is_none());
/// ```
#[derive(Debug, Default)]
pub struct MouseMoveCoalescer {
    /// Accumulated fractional dx since the last flush (includes sub-pixel carry).
    pending_dx: f64,
    /// Accumulated fractional dy since the last flush (includes sub-pixel carry).
    pending_dy: f64,
    /// True when at least one `accumulate` call has been made since the last flush.
    has_pending: bool,
}

impl MouseMoveCoalescer {
    /// Create a new coalescer with no pending movement.
    pub fn new() -> Self {
        Self { pending_dx: 0.0, pending_dy: 0.0, has_pending: false }
    }

    /// Accumulate a mouse-move delta into the current coalescing window.
    ///
    /// Call this for every `MouseMove` OS event regardless of arrival rate.
    /// Deltas are summed in `f64` to preserve sub-pixel precision until
    /// [`flush`](Self::flush) rounds and transmits the net displacement.
    pub fn accumulate(&mut self, dx: f64, dy: f64) {
        self.pending_dx += dx;
        self.pending_dy += dy;
        self.has_pending = true;
    }

    /// Emit one coalesced [`InputEvent::MouseMove`] frame for this display tick.
    ///
    /// Returns `None` when no moves were accumulated since the last call — the
    /// caller must not transmit a frame in that case.  The 60 Hz cursor-position
    /// heartbeat on LBTP channel 2 already signals "cursor is stationary"
    /// independently of the input event channel.
    ///
    /// The integer `(dx, dy)` written to the wire are `round(pending_dx)` and
    /// `round(pending_dy)`.  The sub-pixel remainder is carried forward so the
    /// remote cursor position converges correctly even at slow pointer speeds.
    ///
    /// # Returns
    ///
    /// `Some(bytes)` ready to hand to the LBTP pacer on [`INPUT_CHANNEL_ID`],
    /// or `None` if the coalescing window was empty.
    pub fn flush(&mut self, sender: &mut InputChannelSender) -> Option<Vec<u8>> {
        if !self.has_pending {
            return None;
        }
        let int_dx = self.pending_dx.round();
        let int_dy = self.pending_dy.round();
        // Carry the sub-pixel remainder into the next tick.
        self.pending_dx -= int_dx;
        self.pending_dy -= int_dy;
        self.has_pending = false;
        sender.encode(InputEvent::MouseMove { dx: int_dx, dy: int_dy })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Varint encode/decode round-trips ────────────────────────────────────

    fn roundtrip(value: i32) -> i32 {
        let mut buf = Vec::new();
        encode_varint_i32(value, &mut buf);
        let (decoded, consumed) = decode_varint_i32(&buf).unwrap();
        assert_eq!(consumed, buf.len(), "must consume all bytes");
        decoded
    }

    #[test]
    fn varint_zero() {
        assert_eq!(roundtrip(0), 0);
    }

    #[test]
    fn varint_positive_small() {
        for v in 1..=63 {
            assert_eq!(roundtrip(v), v);
        }
    }

    #[test]
    fn varint_negative_small() {
        for v in -64..=-1 {
            assert_eq!(roundtrip(v), v);
        }
    }

    #[test]
    fn varint_boundary_values() {
        for &v in &[i32::MIN, i32::MAX, -1, 0, 1, 64, -65, 8191, -8192] {
            assert_eq!(roundtrip(v), v, "roundtrip failed for {v}");
        }
    }

    #[test]
    fn varint_size_small_values_is_one_byte() {
        // Values in −64..=63 must encode in exactly 1 byte.
        for v in -64..=63_i32 {
            let mut buf = Vec::new();
            encode_varint_i32(v, &mut buf);
            assert_eq!(buf.len(), 1, "value {v} must encode in 1 byte, got {}", buf.len());
        }
    }

    #[test]
    fn varint_size_two_bytes_for_moderate_values() {
        // 64 is the first value needing 2 bytes.
        let mut buf = Vec::new();
        encode_varint_i32(64, &mut buf);
        assert_eq!(buf.len(), 2);
        let mut buf2 = Vec::new();
        encode_varint_i32(-65, &mut buf2);
        assert_eq!(buf2.len(), 2);
    }

    #[test]
    fn varint_decode_truncated_returns_none() {
        assert!(decode_varint_i32(&[]).is_none());
        // Multi-byte varint with no terminator.
        assert!(decode_varint_i32(&[0x80]).is_none());
        assert!(decode_varint_i32(&[0x80, 0x80]).is_none());
    }

    // ── InputChannelSender — encoding ───────────────────────────────────────

    #[test]
    fn key_press_discriminant_is_0x01() {
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap();
        assert_eq!(bytes[0], 0x01);
    }

    #[test]
    fn key_release_discriminant_is_0x02() {
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::KeyRelease { keycode: 0x41 }).unwrap();
        assert_eq!(bytes[0], 0x02);
    }

    #[test]
    fn mouse_move_discriminant_is_0x10() {
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::MouseMove { dx: 5.0, dy: -3.0 }).unwrap();
        assert_eq!(bytes[0], 0x10);
    }

    #[test]
    fn mouse_button_press_discriminant_is_0x20() {
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::MouseButtonPress { button: MouseButton::Left }).unwrap();
        assert_eq!(bytes[0], 0x20);
    }

    #[test]
    fn mouse_button_release_discriminant_is_0x21() {
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::MouseButtonRelease { button: MouseButton::Right }).unwrap();
        assert_eq!(bytes[0], 0x21);
    }

    #[test]
    fn mouse_button_wire_bytes_are_0_1_2() {
        let mut s = InputChannelSender::new();
        let l = s.encode(InputEvent::MouseButtonPress { button: MouseButton::Left }).unwrap();
        let r = s.encode(InputEvent::MouseButtonPress { button: MouseButton::Right }).unwrap();
        let m = s.encode(InputEvent::MouseButtonPress { button: MouseButton::Middle }).unwrap();
        assert_eq!(l[1], 0); // Left
        assert_eq!(r[1], 1); // Right
        assert_eq!(m[1], 2); // Middle
    }

    // ── Delta coding: same keycode encodes as delta = 0 ─────────────────────

    #[test]
    fn repeat_keycode_encodes_zero_delta() {
        let mut s = InputChannelSender::new();
        s.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap(); // sends Δ=65
        let bytes = s.encode(InputEvent::KeyRelease { keycode: 0x41 }).unwrap(); // Δ=0
        // delta=0 encodes as single byte 0x00
        assert_eq!(bytes[1], 0x00, "repeated keycode must produce delta=0 (1 byte)");
        assert_eq!(bytes.len(), 2); // discriminant + 1 varint byte
    }

    #[test]
    fn adjacent_keycodes_encode_small_delta() {
        // Typing "AB" (0x41, 0x42): second key delta = 1.
        let mut s = InputChannelSender::new();
        s.encode(InputEvent::KeyPress { keycode: 0x41 }).unwrap();
        let bytes = s.encode(InputEvent::KeyPress { keycode: 0x42 }).unwrap();
        assert_eq!(bytes.len(), 2, "delta=1 must encode in 1 byte varint");
        assert_eq!(bytes[1], 0x01);
    }

    #[test]
    fn first_key_event_delta_equals_keycode() {
        let mut s = InputChannelSender::new(); // last_keycode = 0
        let bytes = s.encode(InputEvent::KeyPress { keycode: 10 }).unwrap();
        // delta = 10 - 0 = 10, encodes in 1 byte (10 < 64)
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[1], 10);
    }

    // ── InputChannelDecoder — round-trip ────────────────────────────────────

    fn round_trip(events: &[InputEvent]) -> Vec<InputEvent> {
        let mut sender = InputChannelSender::new();
        let mut decoder = InputChannelDecoder::new();
        events
            .iter()
            .map(|&ev| {
                let bytes = sender.encode(ev).expect("encode must succeed");
                decoder.decode(&bytes).expect("decode must succeed")
            })
            .collect()
    }

    fn key_press(keycode: u32) -> InputEvent {
        InputEvent::KeyPress { keycode }
    }
    fn key_release(keycode: u32) -> InputEvent {
        InputEvent::KeyRelease { keycode }
    }
    fn mouse_move(dx: f64, dy: f64) -> InputEvent {
        InputEvent::MouseMove { dx, dy }
    }
    fn btn_press(button: MouseButton) -> InputEvent {
        InputEvent::MouseButtonPress { button }
    }
    fn btn_release(button: MouseButton) -> InputEvent {
        InputEvent::MouseButtonRelease { button }
    }

    fn events_eq(a: InputEvent, b: InputEvent) -> bool {
        match (a, b) {
            (InputEvent::KeyPress { keycode: ka }, InputEvent::KeyPress { keycode: kb }) => ka == kb,
            (InputEvent::KeyRelease { keycode: ka }, InputEvent::KeyRelease { keycode: kb }) => ka == kb,
            (InputEvent::MouseMove { dx: ax, dy: ay }, InputEvent::MouseMove { dx: bx, dy: by }) => {
                ax == bx && ay == by
            }
            (InputEvent::MouseButtonPress { button: ba }, InputEvent::MouseButtonPress { button: bb }) => {
                ba == bb
            }
            (InputEvent::MouseButtonRelease { button: ba }, InputEvent::MouseButtonRelease { button: bb }) => {
                ba == bb
            }
            _ => false,
        }
    }

    #[test]
    fn round_trip_key_press() {
        let events = [key_press(0x41)];
        let decoded = round_trip(&events);
        assert!(events_eq(decoded[0], events[0]));
    }

    #[test]
    fn round_trip_key_release() {
        let events = [key_release(0x0D)];
        let decoded = round_trip(&events);
        assert!(events_eq(decoded[0], events[0]));
    }

    #[test]
    fn round_trip_mouse_move_positive() {
        let events = [mouse_move(10.0, 20.0)];
        let decoded = round_trip(&events);
        assert!(events_eq(decoded[0], events[0]));
    }

    #[test]
    fn round_trip_mouse_move_negative() {
        let events = [mouse_move(-5.0, -15.0)];
        let decoded = round_trip(&events);
        assert!(events_eq(decoded[0], events[0]));
    }

    #[test]
    fn round_trip_mouse_move_zero() {
        let events = [mouse_move(0.0, 0.0)];
        let decoded = round_trip(&events);
        assert!(events_eq(decoded[0], events[0]));
    }

    #[test]
    fn round_trip_mouse_button_all_variants() {
        let events = [
            btn_press(MouseButton::Left),
            btn_press(MouseButton::Right),
            btn_press(MouseButton::Middle),
            btn_release(MouseButton::Left),
            btn_release(MouseButton::Right),
            btn_release(MouseButton::Middle),
        ];
        let decoded = round_trip(&events);
        for (a, b) in events.iter().zip(decoded.iter()) {
            assert!(events_eq(*a, *b));
        }
    }

    #[test]
    fn round_trip_sequence_ctrl_a() {
        // CTRL+A sequence: Press Ctrl (0x11), Press A (0x41), Release A, Release Ctrl.
        let events = [
            key_press(0x11),  // Ctrl
            key_press(0x41),  // A
            key_release(0x41),
            key_release(0x11),
        ];
        let decoded = round_trip(&events);
        for (a, b) in events.iter().zip(decoded.iter()) {
            assert!(events_eq(*a, *b));
        }
    }

    #[test]
    fn round_trip_keyboard_sentence() {
        // Typing "HELLO" — verifies that the delta state accumulates correctly
        // across a sequence of distinct keycodes.
        let keycodes = [0x48u32, 0x45, 0x4C, 0x4C, 0x4F]; // H E L L O (Windows VK codes)
        let events: Vec<InputEvent> = keycodes.iter().map(|&k| key_press(k)).collect();
        let decoded = round_trip(&events);
        for (a, b) in events.iter().zip(decoded.iter()) {
            assert!(events_eq(*a, *b));
        }
    }

    #[test]
    fn round_trip_mixed_event_types() {
        let events = [
            key_press(0x41),
            mouse_move(5.0, -3.0),
            btn_press(MouseButton::Left),
            btn_release(MouseButton::Left),
            key_release(0x41),
        ];
        let decoded = round_trip(&events);
        for (a, b) in events.iter().zip(decoded.iter()) {
            assert!(events_eq(*a, *b));
        }
    }

    // ── Frame size bounds ────────────────────────────────────────────────────

    #[test]
    fn all_event_types_fit_within_max_frame_bytes() {
        let mut s = InputChannelSender::new();
        let events = [
            key_press(u32::MAX),
            key_release(0),
            mouse_move(f64::from(i32::MAX), f64::from(i32::MIN)),
            btn_press(MouseButton::Middle),
            btn_release(MouseButton::Middle),
        ];
        for ev in events {
            let bytes = s.encode(ev).expect("every valid event must encode successfully");
            assert!(
                bytes.len() <= MAX_INPUT_FRAME_BYTES,
                "event encoded to {} bytes, exceeding MAX_INPUT_FRAME_BYTES ({})",
                bytes.len(),
                MAX_INPUT_FRAME_BYTES
            );
        }
    }

    #[test]
    fn mouse_button_frame_is_exactly_two_bytes() {
        let mut s = InputChannelSender::new();
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let p = s.encode(InputEvent::MouseButtonPress { button }).unwrap();
            let r = s.encode(InputEvent::MouseButtonRelease { button }).unwrap();
            assert_eq!(p.len(), 2, "MouseButtonPress must encode in exactly 2 bytes");
            assert_eq!(r.len(), 2, "MouseButtonRelease must encode in exactly 2 bytes");
        }
    }

    #[test]
    fn zero_mouse_move_is_three_bytes() {
        // discriminant(1) + varint(0)(1) + varint(0)(1) = 3 bytes
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::MouseMove { dx: 0.0, dy: 0.0 }).unwrap();
        assert_eq!(bytes.len(), 3);
    }

    #[test]
    fn small_mouse_move_is_three_bytes() {
        // Both dx and dy in −64..=63 → each encodes in 1 byte varint → 1+1+1 = 3.
        let mut s = InputChannelSender::new();
        let bytes = s.encode(InputEvent::MouseMove { dx: 10.0, dy: -5.0 }).unwrap();
        assert_eq!(bytes.len(), 3);
    }

    // ── Decoder: malformed input returns None ────────────────────────────────

    #[test]
    fn decode_empty_bytes_returns_none() {
        let mut d = InputChannelDecoder::new();
        assert!(d.decode(&[]).is_none());
    }

    #[test]
    fn decode_unknown_discriminant_returns_none() {
        let mut d = InputChannelDecoder::new();
        assert!(d.decode(&[0xFF, 0x00]).is_none());
    }

    #[test]
    fn decode_truncated_key_press_returns_none() {
        // discriminant only, no varint payload
        let mut d = InputChannelDecoder::new();
        assert!(d.decode(&[DISC_KEY_PRESS]).is_none());
    }

    #[test]
    fn decode_truncated_mouse_move_returns_none() {
        // discriminant + one varint but missing dy
        let mut d = InputChannelDecoder::new();
        let mut buf = vec![DISC_MOUSE_MOVE];
        encode_varint_i32(10, &mut buf); // only dx — no dy
        assert!(d.decode(&buf).is_none());
    }

    #[test]
    fn decode_button_with_invalid_index_returns_none() {
        let mut d = InputChannelDecoder::new();
        assert!(d.decode(&[DISC_BUTTON_PRESS, 0xFF]).is_none());
    }

    // ── Channel 3 constant assertions ────────────────────────────────────────

    #[test]
    fn input_channel_id_is_three() {
        assert_eq!(INPUT_CHANNEL_ID, 3);
    }

    #[test]
    fn scheduling_priority_rank_is_one() {
        // Channel 3 must be at index 1 in PRIORITY_ORDER (second only to ctrl/ACK).
        assert_eq!(SCHEDULING_PRIORITY_RANK, 1);
    }

    #[test]
    fn max_input_frame_bytes_matches_lbtp_mtu_formula() {
        // 1 200 − 19 (AEAD envelope) − 2 (frame header) = 1 179
        assert_eq!(MAX_INPUT_FRAME_BYTES, 1_200 - 19 - 2);
    }

    // ── MouseMoveCoalescer (Feature 60) ──────────────────────────────────────

    #[test]
    fn coalesce_tick_ns_is_sixty_hz() {
        assert_eq!(MOUSE_COALESCE_TICK_NS, 1_000_000_000 / 60);
    }

    #[test]
    fn flush_with_no_accumulation_returns_none() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        assert!(coalescer.flush(&mut sender).is_none());
    }

    #[test]
    fn flush_after_second_empty_window_returns_none() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        coalescer.accumulate(5.0, 3.0);
        coalescer.flush(&mut sender).unwrap(); // first tick — consumes
        assert!(coalescer.flush(&mut sender).is_none()); // second tick — empty
    }

    #[test]
    fn single_accumulate_then_flush_produces_frame() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        coalescer.accumulate(10.0, -5.0);
        let frame = coalescer.flush(&mut sender).unwrap();
        assert_eq!(frame[0], DISC_MOUSE_MOVE);

        let mut decoder = InputChannelDecoder::new();
        let ev = decoder.decode(&frame).unwrap();
        assert!(matches!(ev, InputEvent::MouseMove { dx, dy } if dx == 10.0 && dy == -5.0));
    }

    #[test]
    fn multiple_accumulations_sum_into_one_frame() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        coalescer.accumulate(3.0, 1.0);
        coalescer.accumulate(4.0, 2.0);
        coalescer.accumulate(1.0, -6.0);
        let frame = coalescer.flush(&mut sender).unwrap();

        let mut decoder = InputChannelDecoder::new();
        let ev = decoder.decode(&frame).unwrap();
        assert!(matches!(ev, InputEvent::MouseMove { dx, dy } if dx == 8.0 && dy == -3.0));
    }

    #[test]
    fn sub_pixel_remainder_carries_forward() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();

        // 0.4 rounds to 0 — remainder 0.4 carries forward.
        coalescer.accumulate(0.4, 0.0);
        let frame1 = coalescer.flush(&mut sender).unwrap(); // has_pending=true, emits 0
        let mut decoder = InputChannelDecoder::new();
        let ev1 = decoder.decode(&frame1).unwrap();
        assert!(matches!(ev1, InputEvent::MouseMove { dx, dy } if dx == 0.0 && dy == 0.0));

        // Next tick: 0.4 carry + 0.4 new = 0.8, rounds to 1.
        coalescer.accumulate(0.4, 0.0);
        let frame2 = coalescer.flush(&mut sender).unwrap();
        let mut decoder2 = InputChannelDecoder::new();
        let ev2 = decoder2.decode(&frame2).unwrap();
        assert!(matches!(ev2, InputEvent::MouseMove { dx, dy } if dx == 1.0 && dy == 0.0));
    }

    #[test]
    fn coalescer_default_matches_new() {
        let mut a = MouseMoveCoalescer::new();
        let mut b = MouseMoveCoalescer::default();
        let mut sender_a = InputChannelSender::new();
        let mut sender_b = InputChannelSender::new();
        a.accumulate(5.0, 5.0);
        b.accumulate(5.0, 5.0);
        assert_eq!(a.flush(&mut sender_a), b.flush(&mut sender_b));
    }

    #[test]
    fn coalesced_frame_has_mouse_move_discriminant() {
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        coalescer.accumulate(1.0, 1.0);
        let frame = coalescer.flush(&mut sender).unwrap();
        assert_eq!(frame[0], DISC_MOUSE_MOVE, "coalesced frame must carry MouseMove discriminant");
    }

    #[test]
    fn five_hundred_hz_burst_produces_one_frame_per_tick() {
        // Simulate 500 Hz pointer for one 60 Hz tick ≈ 8 events, then flush once.
        let events_per_tick = (500.0_f64 / 60.0).ceil() as usize; // ≈ 9
        let mut coalescer = MouseMoveCoalescer::new();
        let mut sender    = InputChannelSender::new();
        for _ in 0..events_per_tick {
            coalescer.accumulate(2.0, 1.0);
        }
        let frame = coalescer.flush(&mut sender);
        assert!(frame.is_some(), "must emit exactly one frame for the full tick");
        assert!(coalescer.flush(&mut sender).is_none(), "no second frame after flush");
    }
}

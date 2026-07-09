//! Cursor-position sampler — 60 Hz delta encoding for the cursor channel.
//!
//! Reads the OS cursor position and encodes signed (dx, dy) position deltas
//! for the reliable-ordered cursor channel (LBTP channel 2).
//!
//! # Wire format
//!
//! | Bytes | Field | Type   | Description                              |
//! |-------|-------|--------|------------------------------------------|
//! | 0–1   | dx    | i16 LE | Horizontal delta since the previous tick |
//! | 2–3   | dy    | i16 LE | Vertical delta since the previous tick   |
//!
//! 4 bytes total.  Signed 16-bit covers ±32 767 pixels per tick — more than
//! any display needs.  Movements larger than ±32 767 px per tick are saturated
//! (clamped) rather than wrapped so the receiver's accumulated position never
//! jumps discontinuously.
//!
//! A zero-delta frame is emitted on every tick regardless of whether the
//! cursor moved; this keeps the channel rhythm regular and lets the receiver
//! distinguish "cursor is stationary" from "no session".
//!
//! # Platform support
//!
//! | Platform | API                                        |
//! |----------|--------------------------------------------|
//! | macOS    | `CGEventCreate` + `CGEventGetLocation`     |
//! | Windows  | `GetCursorPos`                             |
//! | Linux    | Not implemented — returns `(0, 0)` always  |

/// Sampling rate of the cursor channel (frames per second).
pub const CURSOR_CHANNEL_HZ: u32 = 60;

/// Number of bytes in one encoded cursor-channel delta frame.
pub const CURSOR_DELTA_BYTES: usize = 4;

/// Nanosecond interval between cursor-channel ticks at [`CURSOR_CHANNEL_HZ`].
///
/// 1 000 000 000 ns / 60 = 16 666 666 ns ≈ 16.67 ms.
pub const CURSOR_TICK_NS: u64 = 1_000_000_000 / CURSOR_CHANNEL_HZ as u64;

/// Samples the OS cursor position and encodes (dx, dy) deltas for the
/// reliable-ordered cursor channel at [`CURSOR_CHANNEL_HZ`] Hz.
///
/// Construct one `CursorPositionSampler` per session.  On each 60 Hz tick
/// call [`sample`](Self::sample) and transmit the returned 4-byte frame on the
/// cursor channel (LBTP channel 2, reliable-ordered).
///
/// # Example
///
/// ```
/// use lowband_platform::cursor_sender::CursorPositionSampler;
///
/// let mut sampler = CursorPositionSampler::new();
/// // Called every ~16.67 ms by the session loop.
/// let frame: [u8; 4] = sampler.sample();
/// // Transmit `frame` on the cursor channel.
/// ```
pub struct CursorPositionSampler {
    last_x: i32,
    last_y: i32,
}

impl CursorPositionSampler {
    /// Create a new sampler anchored at the origin `(0, 0)`.
    ///
    /// The first [`sample`](Self::sample) call returns the full current OS
    /// cursor position as a delta from the origin.
    pub fn new() -> Self {
        Self { last_x: 0, last_y: 0 }
    }

    /// Read the current OS cursor position, compute the (dx, dy) delta from
    /// the last sampled position, update the stored position, and return the
    /// 4-byte encoded cursor-channel frame.
    pub fn sample(&mut self) -> [u8; CURSOR_DELTA_BYTES] {
        let (x, y) = platform::read_cursor_pos();
        self.advance(x, y)
    }

    /// Compute the delta from the last position to `(x, y)`, update the
    /// stored position, and return the encoded frame.
    fn advance(&mut self, x: i32, y: i32) -> [u8; CURSOR_DELTA_BYTES] {
        let dx = (x - self.last_x).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let dy = (y - self.last_y).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.last_x = x;
        self.last_y = y;
        encode_delta(dx, dy)
    }
}

impl Default for CursorPositionSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a (dx, dy) cursor delta into the 4-byte wire format (i16 LE each).
#[inline]
pub fn encode_delta(dx: i16, dy: i16) -> [u8; CURSOR_DELTA_BYTES] {
    let mut buf = [0u8; CURSOR_DELTA_BYTES];
    buf[0..2].copy_from_slice(&dx.to_le_bytes());
    buf[2..4].copy_from_slice(&dy.to_le_bytes());
    buf
}

/// Decode the 4-byte wire format back to (dx, dy).
///
/// The receiver passes the decoded values to `PointerOverlay::apply_delta` as
/// `(dx as f64, dy as f64)`.
#[inline]
pub fn decode_delta(frame: [u8; CURSOR_DELTA_BYTES]) -> (i16, i16) {
    (
        i16::from_le_bytes([frame[0], frame[1]]),
        i16::from_le_bytes([frame[2], frame[3]]),
    )
}

// ── Platform backends ─────────────────────────────────────────────────────────

mod platform {
    /// Read the current cursor position in screen pixels.
    pub fn read_cursor_pos() -> (i32, i32) {
        inner::read_cursor_pos()
    }

    #[cfg(target_os = "macos")]
    mod inner {
        use std::ffi::c_void;

        #[repr(C)]
        struct CGPoint {
            x: f64,
            y: f64,
        }

        extern "C" {
            fn CGEventCreate(source: *mut c_void) -> *mut c_void;
            fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
            fn CFRelease(cf: *mut c_void);
        }

        pub fn read_cursor_pos() -> (i32, i32) {
            // SAFETY: CGEventCreate(NULL) always succeeds; NULL source is valid.
            let ev = unsafe { CGEventCreate(std::ptr::null_mut()) };
            if ev.is_null() {
                return (0, 0);
            }
            let pos = unsafe { CGEventGetLocation(ev) };
            // SAFETY: ev is a valid, non-null CGEventRef returned above.
            unsafe { CFRelease(ev) };
            (pos.x.round() as i32, pos.y.round() as i32)
        }
    }

    #[cfg(target_os = "windows")]
    mod inner {
        #[repr(C)]
        struct POINT {
            x: i32,
            y: i32,
        }

        extern "system" {
            fn GetCursorPos(lpPoint: *mut POINT) -> i32;
        }

        pub fn read_cursor_pos() -> (i32, i32) {
            let mut pt = POINT { x: 0, y: 0 };
            // SAFETY: `pt` is a valid, aligned POINT on the stack.
            unsafe { GetCursorPos(&mut pt) };
            (pt.x, pt.y)
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    mod inner {
        pub fn read_cursor_pos() -> (i32, i32) {
            (0, 0)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn cursor_channel_hz_is_sixty() {
        assert_eq!(CURSOR_CHANNEL_HZ, 60);
    }

    #[test]
    fn cursor_delta_bytes_is_four() {
        assert_eq!(CURSOR_DELTA_BYTES, 4);
    }

    #[test]
    fn cursor_tick_ns_matches_sixty_hz() {
        assert_eq!(CURSOR_TICK_NS, 1_000_000_000 / 60);
    }

    // ── encode_delta / decode_delta round-trips ───────────────────────────────

    #[test]
    fn encode_decode_zero_delta() {
        let frame = encode_delta(0, 0);
        assert_eq!(frame, [0u8; 4]);
        assert_eq!(decode_delta(frame), (0, 0));
    }

    #[test]
    fn encode_decode_positive_delta() {
        let frame = encode_delta(100, 200);
        assert_eq!(decode_delta(frame), (100, 200));
    }

    #[test]
    fn encode_decode_negative_delta() {
        let frame = encode_delta(-50, -75);
        assert_eq!(decode_delta(frame), (-50, -75));
    }

    #[test]
    fn encode_decode_max_values() {
        let frame = encode_delta(i16::MAX, i16::MAX);
        assert_eq!(decode_delta(frame), (i16::MAX, i16::MAX));
    }

    #[test]
    fn encode_decode_min_values() {
        let frame = encode_delta(i16::MIN, i16::MIN);
        assert_eq!(decode_delta(frame), (i16::MIN, i16::MIN));
    }

    #[test]
    fn encode_is_little_endian() {
        // dx = 0x0102 → bytes [0x02, 0x01]; dy = 0x0304 → bytes [0x04, 0x03].
        let frame = encode_delta(0x0102, 0x0304);
        assert_eq!(frame, [0x02, 0x01, 0x04, 0x03]);
    }

    #[test]
    fn decode_little_endian_bytes() {
        let frame = [0x02u8, 0x01, 0x04, 0x03];
        assert_eq!(decode_delta(frame), (0x0102, 0x0304));
    }

    // ── CursorPositionSampler ─────────────────────────────────────────────────

    #[test]
    fn sampler_first_advance_is_delta_from_origin() {
        let mut sampler = CursorPositionSampler::new();
        let frame = sampler.advance(320, 240);
        assert_eq!(decode_delta(frame), (320, 240));
    }

    #[test]
    fn sampler_consecutive_advances_track_delta() {
        let mut sampler = CursorPositionSampler::new();
        sampler.advance(100, 50);
        let frame = sampler.advance(130, 45);
        assert_eq!(decode_delta(frame), (30, -5));
    }

    #[test]
    fn sampler_stationary_cursor_emits_zero_delta() {
        let mut sampler = CursorPositionSampler::new();
        sampler.advance(200, 100);
        let frame = sampler.advance(200, 100);
        assert_eq!(decode_delta(frame), (0, 0));
    }

    #[test]
    fn sampler_large_movement_saturates_at_i16_max() {
        let mut sampler = CursorPositionSampler::new();
        // Jump larger than i16::MAX in one tick.
        let far: i32 = i16::MAX as i32 + 1000;
        let frame = sampler.advance(far, 0);
        assert_eq!(decode_delta(frame), (i16::MAX, 0));
    }

    #[test]
    fn sampler_large_negative_movement_saturates_at_i16_min() {
        let mut sampler = CursorPositionSampler::new();
        sampler.advance(i16::MAX as i32, 0);
        // Jump further left than i16::MIN can represent.
        let far_left: i32 = i16::MAX as i32 - (i32::MAX / 2);
        let frame = sampler.advance(far_left, 0);
        assert_eq!(decode_delta(frame), (i16::MIN, 0));
    }

    #[test]
    fn sampler_default_matches_new() {
        let mut a = CursorPositionSampler::new();
        let mut b = CursorPositionSampler::default();
        assert_eq!(a.advance(10, 20), b.advance(10, 20));
    }

    #[test]
    fn sample_returns_four_bytes() {
        let mut sampler = CursorPositionSampler::new();
        let frame = sampler.sample();
        assert_eq!(frame.len(), 4);
    }

    #[test]
    fn sample_frame_decodes_to_valid_i16_pair() {
        let mut sampler = CursorPositionSampler::new();
        let frame = sampler.sample();
        let (dx, dy) = decode_delta(frame);
        // OS cursor coordinates must fit in i32; clamped to i16 before encode.
        let _ = (dx, dy); // confirmed the decode succeeded without panic
    }

    // ── 60 Hz stress: 60 consecutive ticks accumulate correctly ──────────────

    #[test]
    fn sixty_advances_at_one_pixel_per_tick_reach_expected_position() {
        let mut sampler = CursorPositionSampler::new();
        let mut total_dx: i32 = 0;
        let mut total_dy: i32 = 0;

        for tick in 0..60i32 {
            let x = tick + 1;
            let y = tick / 2 + 1;
            let frame = sampler.advance(x, y);
            let (dx, dy) = decode_delta(frame);
            total_dx += dx as i32;
            total_dy += dy as i32;
        }

        assert_eq!(total_dx, 60, "sum of dx deltas must equal final x position");
        assert_eq!(total_dy, 30, "sum of dy deltas must equal final y position");
    }
}

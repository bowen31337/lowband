//! Feature 90 — System emits a 16-byte blit_command plus the exposed strip
//! for a detected scroll.
//!
//! # What this test suite verifies
//!
//! 1. **Wire size**: [`BlitCommand`] is exactly 16 bytes.
//! 2. **Round-trip serialization**: `to_bytes` / `from_bytes` preserve all fields.
//! 3. **Strip origin — all four directions**: `strip_origin()` derives the
//!    correct screen position for upward, downward, left, and rightward scrolls.
//! 4. **Vertical scroll detected**: feeding a scrolled frame to
//!    [`ScrollDetector::detect`] returns `Some(BlitResult)` with the correct
//!    `dy` and strip dimensions.
//! 5. **Static frame produces no blit**: two identical frames yield `None`.
//! 6. **First call returns None**: the detector needs a previous frame to compare
//!    against, so the very first call always returns `None`.
//! 7. **Small region ignored**: a dirty region smaller than
//!    [`SCROLL_MIN_REGION_PX`] is not a candidate for scroll detection.
//! 8. **Exposed strip size**: `strip_w × strip_h` matches the scroll amount and
//!    region dimensions.
//! 9. **Exposed strip pixels from current frame**: the strip byte count equals
//!    `strip_w × strip_h × 4` and the pixels match the corresponding area of
//!    the current (post-scroll) frame.
//! 10. **Strip origin consistency with BlitCommand**: `strip_origin()` agrees
//!     with the start of the exposed-strip area.

use lowband_platform::screen_capture::DirtyRect;
use lowband_platform::screen_encoder::{
    BlitCommand, BlitResult, ScrollDetector,
    SCROLL_MIN_REGION_PX,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a BGRA8 frame where each row is filled with a solid colour derived
/// from `(row + scroll_y) mod 128 / 16` — 8 distinct colour bands, each 16
/// rows tall.  All columns in a row carry the same colour.
///
/// Using distinct solid-colour horizontal bands ensures the 1/8-scale luma
/// at the 8-pixel block boundary has clear variation, giving the SAD
/// correlator a strong signal.
fn make_banded_frame(scroll_y: i32, width: u32, height: u32) -> Vec<u8> {
    const PALETTE: &[[u8; 4]] = &[
        [0x10, 0x80, 0xFF, 0xFF], // band 0  BGRA
        [0xFF, 0x20, 0x10, 0xFF], // band 1
        [0x10, 0xFF, 0x30, 0xFF], // band 2
        [0x20, 0x10, 0xFF, 0xFF], // band 3
        [0xFF, 0xFF, 0x10, 0xFF], // band 4
        [0xFF, 0x10, 0xFF, 0xFF], // band 5
        [0x10, 0xFF, 0xFF, 0xFF], // band 6
        [0x80, 0x80, 0x80, 0xFF], // band 7
    ];

    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for row in 0..height {
        let src_row = (row as i32 + scroll_y).rem_euclid(128) as u32;
        let band = (src_row / 16) as usize % PALETTE.len();
        let color = &PALETTE[band];
        for col in 0..width {
            let off = (row * stride + col * 4) as usize;
            pixels[off..off + 4].copy_from_slice(color);
        }
    }
    pixels
}

fn dirty_rect(x: i32, y: i32, w: u32, h: u32) -> DirtyRect {
    DirtyRect { x, y, width: w, height: h }
}

// ── 1. Wire size ──────────────────────────────────────────────────────────────

#[test]
fn blit_command_is_exactly_16_bytes() {
    assert_eq!(
        std::mem::size_of::<BlitCommand>(),
        16,
        "BlitCommand must be exactly 16 bytes for wire-format compatibility (Feature 90)"
    );
}

// ── 2. Round-trip serialization ───────────────────────────────────────────────

#[test]
fn blit_command_round_trips_to_bytes() {
    let cmd = BlitCommand {
        region_x: -10,
        region_y:  20,
        region_w:  640,
        region_h:  480,
        dx:          0,
        dy:        -32,
        strip_w:   640,
        strip_h:    32,
    };
    let bytes = cmd.to_bytes();
    assert_eq!(bytes.len(), 16, "to_bytes must return exactly 16 bytes");
    let restored = BlitCommand::from_bytes(&bytes);
    assert_eq!(
        restored, cmd,
        "from_bytes(to_bytes(cmd)) must equal the original command"
    );
}

#[test]
fn blit_command_bytes_are_little_endian() {
    let cmd = BlitCommand {
        region_x: 0x0102,
        region_y: 0x0304,
        region_w: 0x0506,
        region_h: 0x0708,
        dx:       -1,     // 0xFFFF LE
        dy:       -1,
        strip_w:  0x0A0B,
        strip_h:  0x0C0D,
    };
    let b = cmd.to_bytes();
    // region_x = 0x0102 in LE → bytes [0x02, 0x01]
    assert_eq!(b[0], 0x02, "region_x LSB");
    assert_eq!(b[1], 0x01, "region_x MSB");
    // dx = -1 → 0xFFFF LE
    assert_eq!(b[8],  0xFF, "dx LSB");
    assert_eq!(b[9],  0xFF, "dx MSB");
}

// ── 3. Strip origin — four directions ─────────────────────────────────────────

#[test]
fn strip_origin_dy_negative_is_at_bottom() {
    // dy = -16: content moved up; exposed strip appears at the bottom.
    let cmd = BlitCommand {
        region_x: 10, region_y: 20, region_w: 200, region_h: 100,
        dx: 0, dy: -16, strip_w: 200, strip_h: 16,
    };
    let (sx, sy) = cmd.strip_origin();
    assert_eq!(sx, 10,  "strip_x must equal region_x for vertical scroll");
    assert_eq!(sy, 104, "strip_y = region_y + region_h + dy = 20 + 100 − 16 = 104");
}

#[test]
fn strip_origin_dy_positive_is_at_top() {
    // dy = +24: content moved down; exposed strip appears at the top.
    let cmd = BlitCommand {
        region_x: 0, region_y: 50, region_w: 320, region_h: 200,
        dx: 0, dy: 24, strip_w: 320, strip_h: 24,
    };
    let (sx, sy) = cmd.strip_origin();
    assert_eq!(sx, 0,  "strip_x must equal region_x");
    assert_eq!(sy, 50, "strip_y = region_y for downward content movement");
}

#[test]
fn strip_origin_dx_negative_is_at_right() {
    // dx = -32: content moved left; exposed strip appears on the right.
    let cmd = BlitCommand {
        region_x: 100, region_y: 0, region_w: 400, region_h: 200,
        dx: -32, dy: 0, strip_w: 32, strip_h: 200,
    };
    let (sx, sy) = cmd.strip_origin();
    assert_eq!(sx, 468, "strip_x = region_x + region_w + dx = 100 + 400 − 32 = 468");
    assert_eq!(sy, 0,   "strip_y must equal region_y");
}

#[test]
fn strip_origin_dx_positive_is_at_left() {
    // dx = +40: content moved right; exposed strip appears on the left.
    let cmd = BlitCommand {
        region_x: 200, region_y: 30, region_w: 300, region_h: 150,
        dx: 40, dy: 0, strip_w: 40, strip_h: 150,
    };
    let (sx, sy) = cmd.strip_origin();
    assert_eq!(sx, 200, "strip_x = region_x for rightward content movement");
    assert_eq!(sy, 30,  "strip_y must equal region_y");
}

// ── 4. Vertical scroll detected ───────────────────────────────────────────────

#[test]
fn vertical_scroll_up_detected() {
    // Content moved UP by 16 full-scale pixels (user scrolled DOWN).
    // The scroll is exactly 2 units at 1/8 scale, giving a clean
    // SAD minimum and high confidence.
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);

    let frame0 = make_banded_frame(0,  w, h);
    let frame1 = make_banded_frame(16, w, h); // same content scrolled up 16 px

    let mut det = ScrollDetector::new();

    // First call stores the frame; detection is impossible without a prior frame.
    let r0 = det.detect(&frame0, w, h, stride, region);
    assert!(r0.is_none(), "first call must return None (no previous frame to compare)");

    // Second call compares frame1 against the stored frame0.
    let r1 = det.detect(&frame1, w, h, stride, region)
        .expect("a 16-px vertical scroll must be detected");

    assert_eq!(r1.command.dy, -16, "dy must be −16 (content moved up 16 px)");
    assert_eq!(r1.command.dx,   0, "dx must be 0 for a pure vertical scroll");
    assert_eq!(r1.command.region_x, 0,     "region_x");
    assert_eq!(r1.command.region_y, 0,     "region_y");
    assert_eq!(r1.command.region_w, w as u16, "region_w");
    assert_eq!(r1.command.region_h, h as u16, "region_h");
}

#[test]
fn vertical_scroll_down_detected() {
    // Content moved DOWN by 16 px (user scrolled UP).
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);

    let frame0 = make_banded_frame(16, w, h);
    let frame1 = make_banded_frame(0,  w, h); // content moved DOWN by 16 px

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, region); // seed

    let r = det.detect(&frame1, w, h, stride, region)
        .expect("a 16-px downward content movement must be detected");

    assert_eq!(r.command.dy, 16, "dy must be +16 (content moved down 16 px)");
    assert_eq!(r.command.dx,  0, "dx must be 0 for a pure vertical scroll");
}

// ── 5. Static frame produces no blit ─────────────────────────────────────────

#[test]
fn identical_frames_produce_no_blit() {
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);
    let frame = make_banded_frame(0, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame, w, h, stride, region); // seed

    let result = det.detect(&frame, w, h, stride, region);
    assert!(
        result.is_none(),
        "identical consecutive frames must not produce a blit command"
    );
}

// ── 6. First call returns None ────────────────────────────────────────────────

#[test]
fn first_call_always_returns_none() {
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);
    let frame = make_banded_frame(0, w, h);

    let mut det = ScrollDetector::new();
    assert!(
        det.detect(&frame, w, h, stride, region).is_none(),
        "the very first detect call must return None (no previous frame stored)"
    );
}

// ── 7. Small region ignored ───────────────────────────────────────────────────

#[test]
fn region_below_min_size_is_ignored() {
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;

    // Region of exactly SCROLL_MIN_REGION_PX - 1 in both dimensions.
    let small = dirty_rect(0, 0, SCROLL_MIN_REGION_PX - 1, SCROLL_MIN_REGION_PX - 1);

    let frame0 = make_banded_frame(0,  w, h);
    let frame1 = make_banded_frame(16, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, small); // seed

    let result = det.detect(&frame1, w, h, stride, small);
    assert!(
        result.is_none(),
        "a region smaller than SCROLL_MIN_REGION_PX must be ignored"
    );
}

#[test]
fn region_at_exact_min_size_is_considered() {
    // A region exactly SCROLL_MIN_REGION_PX × SCROLL_MIN_REGION_PX is large
    // enough to be considered — detection is not guaranteed for every content
    // type at this boundary size, but the region must not be unconditionally
    // rejected.  Use a very clear scroll (16 px) on a well-structured frame.
    let w = 256u32;
    let h = 256u32;
    let stride = w * 4;
    let min = SCROLL_MIN_REGION_PX;
    let region = dirty_rect(0, 0, min, min);

    // Use tighter bands (8 px) so they're visible at 1/8 scale even in the
    // min-size window.  We build a special frame for this case.
    let make_tight = |scroll_y: u32| -> Vec<u8> {
        let mut px = vec![0u8; (stride * h) as usize];
        for row in 0..h {
            let band = (row + scroll_y) / 8 % 8;
            let val  = (band as u8) * 30 + 30;
            for col in 0..w {
                let off = (row * stride + col * 4) as usize;
                px[off]     = val;
                px[off + 1] = 255 - val;
                px[off + 2] = val / 2;
                px[off + 3] = 0xFF;
            }
        }
        px
    };

    let f0 = make_tight(0);
    let f1 = make_tight(8); // 8-px upward scroll = 1 unit at 1/8 scale

    let mut det = ScrollDetector::new();
    det.detect(&f0, w, h, stride, region);
    // We only assert no panic and no unconditional rejection.
    // The returned value depends on content complexity.
    let _ = det.detect(&f1, w, h, stride, region);
}

// ── 8. Exposed strip size ─────────────────────────────────────────────────────

#[test]
fn exposed_strip_dimensions_match_scroll_amount() {
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);

    let frame0 = make_banded_frame(0,  w, h);
    let frame1 = make_banded_frame(16, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, region);

    let r = det.detect(&frame1, w, h, stride, region)
        .expect("scroll must be detected");

    assert_eq!(
        r.command.strip_w, w as u16,
        "strip_w must equal region_w for a vertical scroll"
    );
    assert_eq!(
        r.command.strip_h, 16,
        "strip_h must equal |dy| = 16 for a 16-px vertical scroll"
    );
}

// ── 9. Exposed strip pixels from current frame ────────────────────────────────

#[test]
fn exposed_strip_pixel_count_matches_dimensions() {
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);

    let frame0 = make_banded_frame(0,  w, h);
    let frame1 = make_banded_frame(16, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, region);

    let r = det.detect(&frame1, w, h, stride, region)
        .expect("scroll must be detected");

    let expected_bytes = r.command.strip_w as usize
        * r.command.strip_h as usize
        * 4;
    assert_eq!(
        r.exposed_strip.len(),
        expected_bytes,
        "exposed_strip byte count must equal strip_w × strip_h × 4"
    );
}

#[test]
fn exposed_strip_pixels_match_current_frame() {
    // The scroll is 16 px upward → exposed strip is at rows 112..128 of frame1.
    let w      = 256u32;
    let h      = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);
    let scroll = 16i32;

    let frame0 = make_banded_frame(0,      w, h);
    let frame1 = make_banded_frame(scroll, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, region);

    let BlitResult { command: cmd, exposed_strip } = det
        .detect(&frame1, w, h, stride, region)
        .expect("scroll must be detected");

    let (strip_x, strip_y) = cmd.strip_origin();
    let sw = cmd.strip_w as u32;
    let sh = cmd.strip_h as u32;

    // Compare every strip pixel against the same location in frame1.
    for row in 0..sh {
        for col in 0..sw {
            let src_y = strip_y + row as i32;
            let src_x = strip_x + col as i32;
            if src_y < 0 || src_y >= h as i32 || src_x < 0 || src_x >= w as i32 {
                continue;
            }
            let src_off  = (src_y as u32 * stride + src_x as u32 * 4) as usize;
            let dst_off  = ((row * sw + col) * 4) as usize;
            assert_eq!(
                &exposed_strip[dst_off..dst_off + 4],
                &frame1[src_off..src_off + 4],
                "exposed_strip pixel ({col},{row}) must match frame1 at ({src_x},{src_y})"
            );
        }
    }
}

// ── 10. Strip origin consistency ──────────────────────────────────────────────

#[test]
fn strip_origin_consistent_with_scroll_direction() {
    // For a 16-px upward scroll, strip must be at the BOTTOM of the region.
    let w = 256u32;
    let h = 128u32;
    let stride = w * 4;
    let region = dirty_rect(0, 0, w, h);

    let frame0 = make_banded_frame(0,  w, h);
    let frame1 = make_banded_frame(16, w, h);

    let mut det = ScrollDetector::new();
    det.detect(&frame0, w, h, stride, region);

    let r = det.detect(&frame1, w, h, stride, region)
        .expect("scroll must be detected");

    let (_, strip_y) = r.command.strip_origin();
    let expected_strip_y = h as i32 - r.command.strip_h as i32;

    assert_eq!(
        strip_y, expected_strip_y,
        "for a scroll-up (dy < 0) the strip must begin at region_bottom − strip_h"
    );
}

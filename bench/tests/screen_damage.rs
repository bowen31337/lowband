//! Feature 85 — System acquires screen damage from DXGI, ScreenCaptureKit,
//! or PipeWire with dirty_rects events.
//!
//! # What this test suite verifies
//!
//! 1. **DirtyRect fields** — `x`, `y`, `width`, `height` are present and
//!    accessible; the type is `Copy`.
//! 2. **CaptureFrame carries dirty_rects** — the `dirty_rects: Vec<DirtyRect>`
//!    field is part of the public API.
//! 3. **Empty dirty_rects is valid** — a frame with no damage metadata is
//!    representable (full-frame fallback path, covered by Feature 86).
//! 4. **Multiple dirty rects** — a frame may carry more than one damage region.
//! 5. **DirtyRect PartialEq** — two equal rects compare equal; two different
//!    rects do not.
//! 6. **Negative origin is valid** — virtual desktops can place monitors at
//!    negative coordinates; `x` and `y` are `i32`.
//! 7. **Linux open(-1) returns Unavailable** — the PipeWire backend fails
//!    cleanly when no portal fd is provided; this is the behaviour CI relies on.
//! 8. **request_grant never panics** — the elevation path must be panic-free
//!    in any environment.
//! 9. **CaptureError variants are distinguishable** — each variant has a
//!    non-empty `Display` string (regression guard).
//! 10. **DirtyRect Debug is implemented** — required for test assertion output.

use lowband_platform::{CaptureError, CaptureFrame, DirtyRect, ScreenCaptureBroker};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rect(x: i32, y: i32, w: u32, h: u32) -> DirtyRect {
    DirtyRect { x, y, width: w, height: h }
}

fn frame_with_rects(dirty_rects: Vec<DirtyRect>) -> CaptureFrame {
    CaptureFrame {
        pixels:      vec![0u8; 4],
        width:       1,
        height:      1,
        stride:      4,
        dirty_rects,
        cursor_shape: None,
    }
}

// ── 1. DirtyRect fields and Copy ─────────────────────────────────────────────

#[test]
fn dirty_rect_fields_accessible() {
    let r = rect(10, 20, 100, 50);
    assert_eq!(r.x,      10);
    assert_eq!(r.y,      20);
    assert_eq!(r.width,  100);
    assert_eq!(r.height, 50);
}

#[test]
fn dirty_rect_is_copy() {
    let a = rect(0, 0, 32, 32);
    let b = a; // copy
    let c = a; // still valid
    assert_eq!(b, c);
}

// ── 2. CaptureFrame carries dirty_rects ──────────────────────────────────────

#[test]
fn capture_frame_carries_dirty_rects() {
    let rects = vec![rect(0, 0, 100, 100), rect(200, 150, 64, 32)];
    let frame = frame_with_rects(rects.clone());
    assert_eq!(frame.dirty_rects.len(), 2);
    assert_eq!(frame.dirty_rects[0], rects[0]);
    assert_eq!(frame.dirty_rects[1], rects[1]);
}

// ── 3. Empty dirty_rects is valid ────────────────────────────────────────────

#[test]
fn capture_frame_with_empty_dirty_rects_is_valid() {
    let frame = frame_with_rects(vec![]);
    assert!(frame.dirty_rects.is_empty());
}

// ── 4. Multiple dirty rects ───────────────────────────────────────────────────

#[test]
fn capture_frame_multiple_dirty_rects() {
    let n = 16usize;
    let rects: Vec<DirtyRect> = (0..n)
        .map(|i| rect(i as i32 * 64, 0, 64, 64))
        .collect();
    let frame = frame_with_rects(rects);
    assert_eq!(frame.dirty_rects.len(), n);
    for (i, r) in frame.dirty_rects.iter().enumerate() {
        assert_eq!(r.x, i as i32 * 64, "rect {i} wrong x");
        assert_eq!(r.width, 64);
    }
}

// ── 5. DirtyRect PartialEq ───────────────────────────────────────────────────

#[test]
fn dirty_rect_eq() {
    let a = rect(0, 0, 32, 32);
    let b = rect(0, 0, 32, 32);
    let c = rect(1, 0, 32, 32);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn dirty_rect_ne_on_each_field() {
    let base = rect(5, 10, 20, 30);
    assert_ne!(base, rect(6, 10, 20, 30), "x differs");
    assert_ne!(base, rect(5, 11, 20, 30), "y differs");
    assert_ne!(base, rect(5, 10, 21, 30), "width differs");
    assert_ne!(base, rect(5, 10, 20, 31), "height differs");
}

// ── 6. Negative origin ───────────────────────────────────────────────────────

#[test]
fn dirty_rect_negative_origin() {
    let r = rect(-100, -200, 1920, 1080);
    assert_eq!(r.x, -100);
    assert_eq!(r.y, -200);
    let frame = frame_with_rects(vec![r]);
    assert_eq!(frame.dirty_rects[0].x, -100);
}

// ── 7. Linux open(-1) returns Unavailable ────────────────────────────────────

#[cfg(target_os = "linux")]
#[test]
fn linux_open_negative_fd_is_unavailable() {
    match ScreenCaptureBroker::open(-1) {
        Err(CaptureError::Unavailable) => {}
        other => panic!("expected Unavailable on Linux with fd=-1, got {:?}", other.err()),
    }
}

// ── 8. request_grant never panics ────────────────────────────────────────────

#[test]
fn request_grant_does_not_panic() {
    let _ = ScreenCaptureBroker::request_grant();
}

// ── 9. CaptureError Display ──────────────────────────────────────────────────

#[test]
fn capture_error_display_nonempty() {
    for err in [
        CaptureError::NotGranted,
        CaptureError::OsRejected,
        CaptureError::Unavailable,
        CaptureError::NoNewFrame,
    ] {
        let s = err.to_string();
        assert!(!s.is_empty(), "CaptureError::{err:?} Display is empty");
    }
}

// ── 10. DirtyRect Debug ───────────────────────────────────────────────────────

#[test]
fn dirty_rect_debug_is_implemented() {
    let r = rect(1, 2, 3, 4);
    let s = format!("{r:?}");
    assert!(!s.is_empty());
    assert!(s.contains('1') || s.contains("DirtyRect"));
}

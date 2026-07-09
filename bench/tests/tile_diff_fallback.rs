//! Feature 86 — System falls back to an xxHash3 comparison with tile_diff
//! where OS damage events are unavailable.
//!
//! # Problem
//!
//! Some capture backends (ScreenCaptureKit without damage callbacks, certain
//! PipeWire setups, headless environments) deliver frames with an empty
//! `dirty_rects` list — the OS cannot tell us what changed.  Without damage
//! metadata every frame would need to be fully re-encoded, wasting bandwidth
//! on a metered link.
//!
//! # Solution
//!
//! [`TileDiffDetector`] hashes each 32×32 tile with xxHash3-64 and compares
//! the digest to the previous frame.  Only tiles whose digest changes are
//! returned as dirty rectangles.  The rest of the pipeline is unchanged.
//!
//! # Test structure
//!
//! **A — first-call semantics**: all tiles are dirty on the first call because
//! no prior frame exists (stored digests are zero, which is not a valid
//! xxHash3 output for any real tile).
//!
//! **B — static screen**: identical consecutive frames yield zero dirty tiles.
//!
//! **C — single-tile change**: modifying exactly one tile reports exactly that
//! tile; unchanged tiles are suppressed.
//!
//! **D — multi-tile change**: N non-overlapping changed tiles produce exactly N
//! dirty rects.
//!
//! **E — reset**: after `reset()`, the next `detect()` returns all tiles dirty
//! regardless of what was seen before.
//!
//! **F — coordinate alignment**: every returned dirty rect's origin is a
//! multiple of `TILE_SIZE_PX` and its dimensions equal `TILE_SIZE_PX × TILE_SIZE_PX`.
//!
//! **G — update_hashes**: calling `update_hashes` on a frame (OS-supplied dirty
//! rects path) keeps the stored state consistent so the next damage-less frame
//! compares correctly.
//!
//! **H — repeated change detection**: a tile that reverts to its original
//! content causes two transitions — dirty when it changes, dirty again when
//! it reverts.

use lowband_platform::{TileDiffDetector, TILE_SIZE_PX};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a flat BGRA8 frame of `w × h` pixels all filled with `fill`.
fn solid_frame(w: u32, h: u32, fill: u8) -> Vec<u8> {
    vec![fill; (w * h * 4) as usize]
}

/// Modify one 32×32 tile at `(tile_col, tile_row)` in `pixels` by XOR-ing a
/// single byte at the tile's first pixel.  Returns the modified frame.
fn flip_tile(mut pixels: Vec<u8>, w: u32, tile_col: u32, tile_row: u32) -> Vec<u8> {
    let stride = w * 4;
    let off    = (tile_row * TILE_SIZE_PX * stride + tile_col * TILE_SIZE_PX * 4) as usize;
    pixels[off] ^= 0xFF;
    pixels
}

// ── A: First-call semantics ───────────────────────────────────────────────────

#[test]
fn first_detect_returns_all_tiles_dirty() {
    // 96×64 → ceil(96/32)×ceil(64/32) = 3×2 = 6 tiles.
    let (w, h) = (96u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame   = solid_frame(w, h, 0xAA);
    let dirty   = det.detect(&frame, w * 4);
    assert_eq!(
        dirty.len(), 6,
        "first detect must return all 6 tiles as dirty; got {}",
        dirty.len()
    );
}

#[test]
fn first_detect_dirty_count_matches_tile_grid() {
    // 64×32 → 2×1 = 2 tiles.
    let (w, h) = (64u32, 32u32);
    let mut det = TileDiffDetector::new(w, h);
    assert_eq!(det.cols(), 2, "tile columns must be ceil(64/32)=2");
    assert_eq!(det.rows(), 1, "tile rows must be ceil(32/32)=1");
    let dirty = det.detect(&solid_frame(w, h, 0), w * 4);
    assert_eq!(dirty.len(), 2);
}

// ── B: Static screen — identical consecutive frames ───────────────────────────

#[test]
fn identical_consecutive_frames_produce_zero_dirty_tiles() {
    let (w, h) = (64u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame   = solid_frame(w, h, 0x55);
    det.detect(&frame, w * 4); // first call — primes the hashes
    let dirty = det.detect(&frame, w * 4);
    assert!(
        dirty.is_empty(),
        "second detect with the same frame must yield zero dirty tiles; got {}",
        dirty.len()
    );
}

#[test]
fn three_static_frames_always_empty_after_first() {
    let (w, h) = (64u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame   = solid_frame(w, h, 0x77);
    det.detect(&frame, w * 4);
    for tick in 0..3 {
        let dirty = det.detect(&frame, w * 4);
        assert!(
            dirty.is_empty(),
            "tick {tick}: static frame must produce zero dirty tiles"
        );
    }
}

// ── C: Single-tile change ─────────────────────────────────────────────────────

#[test]
fn single_tile_change_returns_exactly_one_dirty_rect() {
    let (w, h) = (64u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame0 = solid_frame(w, h, 0x00);
    det.detect(&frame0, stride);

    let frame1 = flip_tile(frame0, w, 1, 0); // tile (col=1, row=0): x=32, y=0
    let dirty  = det.detect(&frame1, stride);

    assert_eq!(dirty.len(), 1, "only one tile changed; expected one dirty rect");
    assert_eq!(dirty[0].x,      32, "dirty rect x must be col=1 → x=32");
    assert_eq!(dirty[0].y,       0, "dirty rect y must be row=0 → y=0");
    assert_eq!(dirty[0].width,  TILE_SIZE_PX);
    assert_eq!(dirty[0].height, TILE_SIZE_PX);
}

#[test]
fn changing_corner_tile_reports_correct_origin() {
    let (w, h) = (64u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame0 = solid_frame(w, h, 0x00);
    det.detect(&frame0, stride);

    // Change bottom-right tile (col=1, row=1): x=32, y=32.
    let frame1 = flip_tile(frame0, w, 1, 1);
    let dirty  = det.detect(&frame1, stride);

    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].x, 32, "bottom-right tile: x must be 32");
    assert_eq!(dirty[0].y, 32, "bottom-right tile: y must be 32");
}

// ── D: Multi-tile change ──────────────────────────────────────────────────────

#[test]
fn changing_two_tiles_returns_exactly_two_dirty_rects() {
    let (w, h) = (96u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame0 = solid_frame(w, h, 0x00);
    det.detect(&frame0, stride);

    let frame1 = flip_tile(frame0,  w, 0, 0); // tile (0,0)
    let frame1 = flip_tile(frame1,  w, 2, 1); // tile (2,1)
    let dirty  = det.detect(&frame1, stride);

    assert_eq!(dirty.len(), 2, "two tiles changed; expected exactly two dirty rects");

    let xs: Vec<i32> = dirty.iter().map(|r| r.x).collect();
    let ys: Vec<i32> = dirty.iter().map(|r| r.y).collect();
    assert!(xs.contains(&0),  "dirty rects must include x=0 (tile col=0)");
    assert!(xs.contains(&64), "dirty rects must include x=64 (tile col=2)");
    assert!(ys.contains(&0),  "dirty rects must include y=0 (tile row=0)");
    assert!(ys.contains(&32), "dirty rects must include y=32 (tile row=1)");
}

#[test]
fn changing_all_tiles_returns_full_grid_count() {
    let (w, h) = (64u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame0  = solid_frame(w, h, 0x00);
    det.detect(&frame0, w * 4);

    // Flip every pixel — guarantees all tiles change.
    let frame1: Vec<u8> = frame0.iter().map(|b| b ^ 0xFF).collect();
    let dirty = det.detect(&frame1, w * 4);

    assert_eq!(
        dirty.len(), 4,
        "all 4 tiles changed; expected 4 dirty rects, got {}",
        dirty.len()
    );
}

// ── E: Reset ──────────────────────────────────────────────────────────────────

#[test]
fn reset_restores_all_dirty_on_next_detect() {
    let (w, h) = (64u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame   = solid_frame(w, h, 0xCC);

    det.detect(&frame, w * 4); // primes hashes
    assert!(det.detect(&frame, w * 4).is_empty(), "pre-reset: no dirty tiles");

    det.reset();
    let dirty = det.detect(&frame, w * 4);
    assert_eq!(
        dirty.len(), 4,
        "after reset all tiles must be dirty again; got {}",
        dirty.len()
    );
}

// ── F: Coordinate alignment ───────────────────────────────────────────────────

#[test]
fn dirty_rects_are_tile_aligned_and_tile_sized() {
    let (w, h) = (128u32, 96u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame0 = solid_frame(w, h, 0x00);
    det.detect(&frame0, stride);

    // Change three scattered tiles.
    let frame1 = flip_tile(frame0, w, 0, 0);
    let frame1 = flip_tile(frame1, w, 3, 1);
    let frame1 = flip_tile(frame1, w, 1, 2);
    let dirty  = det.detect(&frame1, stride);

    assert_eq!(dirty.len(), 3);
    for r in &dirty {
        assert_eq!(
            r.x % TILE_SIZE_PX as i32, 0,
            "dirty rect x={} must be a multiple of TILE_SIZE_PX={}",
            r.x, TILE_SIZE_PX
        );
        assert_eq!(
            r.y % TILE_SIZE_PX as i32, 0,
            "dirty rect y={} must be a multiple of TILE_SIZE_PX={}",
            r.y, TILE_SIZE_PX
        );
        assert_eq!(r.width,  TILE_SIZE_PX, "dirty rect width must equal TILE_SIZE_PX");
        assert_eq!(r.height, TILE_SIZE_PX, "dirty rect height must equal TILE_SIZE_PX");
    }
}

// ── G: update_hashes keeps state consistent ────────────────────────────────────

#[test]
fn update_hashes_followed_by_same_frame_yields_no_dirty() {
    let (w, h) = (64u32, 64u32);
    let mut det = TileDiffDetector::new(w, h);
    let frame   = solid_frame(w, h, 0x99);

    // Simulate: OS supplies dirty rects this frame, so we call update_hashes
    // instead of detect to keep the stored state in sync.
    det.update_hashes(&frame, w * 4);

    let dirty = det.detect(&frame, w * 4);
    assert!(
        dirty.is_empty(),
        "after update_hashes with frame A, detecting frame A must yield no dirty tiles"
    );
}

#[test]
fn update_hashes_then_changed_frame_yields_changed_tiles() {
    let (w, h) = (64u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame0 = solid_frame(w, h, 0x00);
    det.update_hashes(&frame0, stride); // OS-supplied dirty rects path

    let frame1 = flip_tile(frame0, w, 0, 1); // change tile (0,1)
    let dirty  = det.detect(&frame1, stride);
    assert_eq!(
        dirty.len(), 1,
        "only tile (0,1) changed after update_hashes; expected 1 dirty rect"
    );
    assert_eq!(dirty[0].x,  0,              "tile col=0 → x=0");
    assert_eq!(dirty[0].y,  TILE_SIZE_PX as i32, "tile row=1 → y=32");
}

// ── H: Repeated change detection — revert cycle ───────────────────────────────

#[test]
fn tile_revert_to_original_is_detected_as_dirty() {
    let (w, h) = (64u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame_a = solid_frame(w, h, 0x11); // original
    let frame_b = flip_tile(frame_a.clone(), w, 0, 0); // changed

    det.detect(&frame_a, stride); // prime with frame_a

    // Change to frame_b → tile (0,0) is dirty.
    let dirty1 = det.detect(&frame_b, stride);
    assert_eq!(dirty1.len(), 1, "frame_a→frame_b: tile (0,0) must be dirty");

    // Revert to frame_a → tile (0,0) must be dirty again.
    let dirty2 = det.detect(&frame_a, stride);
    assert_eq!(
        dirty2.len(), 1,
        "frame_b→frame_a (revert): tile (0,0) must be dirty again; got {}",
        dirty2.len()
    );
    assert_eq!(dirty2[0].x, 0);
    assert_eq!(dirty2[0].y, 0);
}

#[test]
fn stable_tiles_remain_suppressed_across_revert_cycle() {
    let (w, h) = (64u32, 64u32);
    let stride  = w * 4;
    let mut det = TileDiffDetector::new(w, h);

    let frame_a = solid_frame(w, h, 0x22);
    let frame_b = flip_tile(frame_a.clone(), w, 1, 1); // only tile (1,1) changes

    det.detect(&frame_a, stride);

    det.detect(&frame_b, stride); // tile (1,1) dirty
    let dirty_revert = det.detect(&frame_a, stride); // tile (1,1) dirty again

    // Only tile (1,1) should appear; tiles (0,0), (1,0), (0,1) are unchanged.
    assert_eq!(
        dirty_revert.len(), 1,
        "revert cycle must dirty only the reverted tile, not stable neighbours"
    );
    assert_eq!(dirty_revert[0].x, 32, "reverted tile col=1 → x=32");
    assert_eq!(dirty_revert[0].y, 32, "reverted tile row=1 → y=32");
}

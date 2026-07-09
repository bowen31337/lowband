//! Feature 96 — VIDEO region confined to Gear-B AV1 encode with sub_stream isolation.
//!
//! # Purpose
//!
//! Verifies that tiles classified as VIDEO are:
//!
//! 1. Correctly classified (> [`VIDEO_COLOR_LIMIT`] distinct colours → VIDEO).
//! 2. Completely isolated from the two-pass encode pipeline — they must never
//!    enter the [`RefinementQueue`] and `needs_refinement()` must return `false`.
//! 3. Accepted by [`VideoSubStream::push`] which merges them into a growing
//!    bounding [`TileRect`].
//! 4. Returned as a pixel-accurate encode region by
//!    [`VideoSubStream::take_dirty_region`], which clears the dirty flag.
//! 5. Correctly computed in pixel coordinates (tile_coord × TILE_SIZE_PX).
//!
//! # Isolation contract (Feature 96)
//!
//! A VIDEO tile never enters the lossless-refinement path.  The sub-stream
//! bounding rect is the only data structure that tracks VIDEO regions; the
//! [`RefinementQueue`] must stay empty after VIDEO-only damage events.
//!
//! # Bounding-rect semantics
//!
//! [`VideoSubStream`] maintains the *union* bounding rectangle of all pushed
//! tiles.  When a new frame starts the bounding rect is **not** cleared between
//! frames; the caller resets it on scene cuts via [`VideoSubStream::reset`].

use lowband_platform::screen_encoder::{
    classify_tile, RefinementQueue, TileClass, TileCoord, TileRect, VideoSubStream,
    TILE_SIZE_PX, VIDEO_COLOR_LIMIT,
};

// ── Helper: build a tile with `n` distinct BGRA8 colours ─────────────────────

fn tile_with_n_colours(n: usize) -> Vec<u8> {
    assert!(n <= 4096, "classify_tile fingerprint space is 12 bits (4096 entries)");
    let mut px = vec![0u8; (TILE_SIZE_PX * TILE_SIZE_PX * 4) as usize];
    for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
        let c = i % n;
        // Spread colours across all three nibble slots of the 12-bit fingerprint
        // idx = (r_nibble << 8) | (g_nibble << 4) | b_nibble so that each c in
        // 0..n maps to a unique idx regardless of whether n > 256.
        let b_nibble = (c % 16) as u8;
        let g_nibble = ((c / 16) % 16) as u8;
        let r_nibble = ((c / 256) % 16) as u8;
        chunk[0] = b_nibble << 4; // B
        chunk[1] = g_nibble << 4; // G
        chunk[2] = r_nibble << 4; // R
        chunk[3] = 0xFF;           // A
    }
    px
}

// ── 1. Classification: > VIDEO_COLOR_LIMIT colours → TileClass::Video ────────

#[test]
fn tile_with_257_colours_is_video() {
    let px = tile_with_n_colours(VIDEO_COLOR_LIMIT + 1);
    let class = classify_tile(&px);
    assert_eq!(
        class,
        TileClass::Video,
        "a tile with {} distinct colours must be classified VIDEO; got {class:?}",
        VIDEO_COLOR_LIMIT + 1,
    );
}

#[test]
fn tile_at_exact_video_limit_is_video() {
    // VIDEO_COLOR_LIMIT + 1 colours crosses the threshold; VIDEO_COLOR_LIMIT
    // itself is within PICTURE range.
    let px_over  = tile_with_n_colours(VIDEO_COLOR_LIMIT + 1);
    let px_exact = tile_with_n_colours(VIDEO_COLOR_LIMIT);
    assert_eq!(classify_tile(&px_over),  TileClass::Video,   "over limit must be Video");
    assert_eq!(classify_tile(&px_exact), TileClass::Picture, "at limit must be Picture");
}

#[test]
fn video_class_saliency_is_zero() {
    assert_eq!(
        TileClass::Video.saliency(),
        0,
        "VIDEO saliency must be 0 (lowest priority — it never enters the refinement queue)"
    );
}

// ── 2. Isolation: VIDEO must not enter the refinement queue ──────────────────

#[test]
fn video_tile_does_not_need_refinement() {
    assert!(
        !TileClass::Video.needs_refinement(),
        "TileClass::Video must return needs_refinement() = false; \
         it is isolated in the Gear-B sub-stream, not the two-pass pipeline"
    );
}

#[test]
fn video_coarse_pass_is_not_lossless() {
    // VIDEO tiles do not go through the palette/lossless coarse pass.
    assert!(
        !TileClass::Video.coarse_is_lossless(),
        "VIDEO tiles are encoded by Gear-B AV1, not the lossless palette path"
    );
}

#[test]
fn refinement_queue_stays_empty_after_video_only_damage() {
    let mut q = RefinementQueue::new();

    // Simulate a damage event: several tiles all classified as VIDEO.
    // The caller must NOT push VIDEO tiles into the refinement queue.
    for col in 0..4u32 {
        let class = TileClass::Video;
        if class.needs_refinement() {
            q.push(TileCoord { col, row: 0 }, class);
        }
    }

    assert!(
        q.is_empty(),
        "the refinement queue must remain empty after a VIDEO-only damage event; \
         VIDEO tiles are isolated in the Gear-B sub-stream"
    );
}

// ── 3. VideoSubStream: dirty flag and single-tile region ─────────────────────

#[test]
fn new_sub_stream_is_not_dirty() {
    let vs = VideoSubStream::new();
    assert!(!vs.is_dirty(), "a new VideoSubStream must not be dirty");
}

#[test]
fn new_sub_stream_is_empty() {
    let vs = VideoSubStream::new();
    assert!(vs.is_empty(), "a new VideoSubStream must be empty");
}

#[test]
fn push_marks_sub_stream_dirty() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 0, row: 0 });
    assert!(vs.is_dirty(), "sub-stream must be dirty after push");
}

#[test]
fn take_dirty_region_clears_dirty_flag() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 2, row: 3 });
    assert!(vs.take_dirty_region().is_some(), "must return Some on first take");
    assert!(
        !vs.is_dirty(),
        "dirty flag must be cleared after take_dirty_region"
    );
}

#[test]
fn take_dirty_region_returns_none_when_not_dirty() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 0, row: 0 });
    vs.take_dirty_region(); // clears dirty
    assert!(
        vs.take_dirty_region().is_none(),
        "second take_dirty_region without an intervening push must return None"
    );
}

#[test]
fn single_tile_region_has_size_one() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 5, row: 7 });
    let rect = vs.take_dirty_region().expect("must be Some after push");
    assert_eq!(rect.cols(), 1, "single-tile region must have cols() == 1");
    assert_eq!(rect.rows(), 1, "single-tile region must have rows() == 1");
}

// ── 4. VideoSubStream: bounding-rect growth ──────────────────────────────────

#[test]
fn bounding_rect_grows_to_cover_all_pushed_tiles() {
    let mut vs = VideoSubStream::new();
    let tiles = [
        TileCoord { col: 3, row: 1 },
        TileCoord { col: 7, row: 5 },
        TileCoord { col: 1, row: 9 },
    ];
    for &t in &tiles {
        vs.push(t);
    }
    let rect = vs.take_dirty_region().expect("must be Some");
    assert_eq!(rect.col_min, 1, "col_min must be the leftmost column pushed");
    assert_eq!(rect.col_max, 7, "col_max must be the rightmost column pushed");
    assert_eq!(rect.row_min, 1, "row_min must be the topmost row pushed");
    assert_eq!(rect.row_max, 9, "row_max must be the bottommost row pushed");
}

#[test]
fn bounding_rect_is_union_of_pushed_tiles() {
    let mut vs = VideoSubStream::new();
    // Push tiles in non-monotone order to stress the min/max logic.
    vs.push(TileCoord { col: 10, row: 10 });
    vs.push(TileCoord { col:  0, row:  0 });
    vs.push(TileCoord { col:  5, row: 15 });
    let rect = vs.take_dirty_region().expect("must be Some");
    assert_eq!(rect, TileRect { col_min: 0, row_min: 0, col_max: 10, row_max: 15 });
}

#[test]
fn push_after_take_extends_same_region() {
    // The bounding rect persists across take_dirty_region; it is only cleared
    // by reset().  A second batch of pushes merges with the existing region.
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 2, row: 2 });
    vs.take_dirty_region();

    // Second damage event: tile outside previous region.
    vs.push(TileCoord { col: 8, row: 1 });
    let rect = vs.take_dirty_region().expect("must be Some");
    // Union of (2,2) and (8,1)
    assert_eq!(rect.col_min, 2);
    assert_eq!(rect.col_max, 8);
    assert_eq!(rect.row_min, 1);
    assert_eq!(rect.row_max, 2);
}

// ── 5. VideoSubStream: pixel-accurate encode region ──────────────────────────

#[test]
fn pixel_coordinates_match_tile_size() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 3, row: 2 });
    vs.push(TileCoord { col: 5, row: 4 });
    let rect = vs.take_dirty_region().expect("must be Some");

    assert_eq!(
        rect.x_px(),
        3 * TILE_SIZE_PX,
        "x_px must be col_min × TILE_SIZE_PX"
    );
    assert_eq!(
        rect.y_px(),
        2 * TILE_SIZE_PX,
        "y_px must be row_min × TILE_SIZE_PX"
    );
    assert_eq!(
        rect.width_px(),
        (5 - 3 + 1) * TILE_SIZE_PX,
        "width_px must be cols() × TILE_SIZE_PX"
    );
    assert_eq!(
        rect.height_px(),
        (4 - 2 + 1) * TILE_SIZE_PX,
        "height_px must be rows() × TILE_SIZE_PX"
    );
}

#[test]
fn single_tile_pixel_region_is_tile_size_px_square() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 0, row: 0 });
    let rect = vs.take_dirty_region().expect("must be Some");
    assert_eq!(rect.width_px(),  TILE_SIZE_PX, "single tile must be TILE_SIZE_PX wide");
    assert_eq!(rect.height_px(), TILE_SIZE_PX, "single tile must be TILE_SIZE_PX tall");
}

// ── 6. VideoSubStream: reset ──────────────────────────────────────────────────

#[test]
fn reset_clears_region_and_dirty_flag() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 4, row: 4 });
    vs.reset();
    assert!(!vs.is_dirty(), "dirty must be false after reset");
    assert!(vs.is_empty(), "region must be None after reset");
    assert!(
        vs.take_dirty_region().is_none(),
        "take_dirty_region must return None after reset"
    );
}

#[test]
fn push_after_reset_starts_fresh_region() {
    let mut vs = VideoSubStream::new();
    vs.push(TileCoord { col: 10, row: 10 });
    vs.reset();
    vs.push(TileCoord { col: 1, row: 2 });
    let rect = vs.take_dirty_region().expect("must be Some");
    // Region must be exactly the post-reset tile, not a union with pre-reset tile.
    assert_eq!(rect, TileRect { col_min: 1, row_min: 2, col_max: 1, row_max: 2 });
}

// ── 7. End-to-end: classify → route → sub_stream → pixel region ──────────────

#[test]
fn video_tiles_routed_to_sub_stream_not_refinement_queue() {
    let mut vs = VideoSubStream::new();
    let mut q  = RefinementQueue::new();

    // Build a mixed damage event: some VIDEO, some PICTURE tiles.
    let video_px = tile_with_n_colours(VIDEO_COLOR_LIMIT + 1);

    let damage: &[(TileCoord, &[u8])] = &[
        (TileCoord { col: 0, row: 0 }, &video_px),
        (TileCoord { col: 1, row: 0 }, &video_px),
    ];

    for &(coord, px) in damage {
        let class = classify_tile(px);
        assert_eq!(class, TileClass::Video);
        if class.needs_refinement() {
            q.push(coord, class);
        } else if class == TileClass::Video {
            vs.push(coord);
        }
    }

    // Refinement queue stays empty — VIDEO tiles never enter it.
    assert!(
        q.is_empty(),
        "refinement queue must be empty; VIDEO tiles belong in the sub-stream"
    );

    // Sub-stream accumulated a bounding rect covering both tiles.
    let rect = vs.take_dirty_region().expect("sub-stream must be dirty");
    assert_eq!(rect.col_min, 0);
    assert_eq!(rect.col_max, 1);
    assert_eq!(rect.row_min, 0);
    assert_eq!(rect.row_max, 0);
    assert_eq!(rect.width_px(), 2 * TILE_SIZE_PX);
}

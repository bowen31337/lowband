//! Feature 99 — first_pass text legibility and pixel-exact within about one second.
//!
//! # Purpose
//!
//! Verifies the two-pass guarantees of the screen codec:
//!
//! 1. **Text legibility in the first pass**: TEXT and FLAT tiles are encoded
//!    losslessly in the coarse pass using palette_index coding (Features 93–94).
//!    A viewer reading a stack trace or log sees correct characters in the very
//!    first frame — before the refinement pass completes — because the coarse
//!    encode is already bit-exact for palette-compatible content.
//!
//! 2. **Pixel-exact within about one second**: Given the `screen_refinement_bps`
//!    budget allocated by the gear policy, all PICTURE tiles enqueued during the
//!    coarse pass drain to lossless quality within
//!    [`PIXEL_EXACT_DEADLINE_MS`](lowband_platform::screen_encoder::PIXEL_EXACT_DEADLINE_MS)
//!    (1 000 ms) of the damage event.
//!
//! # Scenario
//!
//! Comfortable-tier session at 400 kbps (voice + screen + camera):
//!
//! | Stream          | Budget   | Derivation                                      |
//! |-----------------|----------|-------------------------------------------------|
//! | Voice           | 24 kbps  | audio floor                                     |
//! | Input / cursor  |  8 kbps  | architecture minimum                            |
//! | Screen coarse   | 20 kbps  | capped by gear policy                           |
//! | Camera (Gear A) |300 kbps  | min(remaining=348k, 300k)                       |
//! | Screen refine   | 48 kbps  | remaining 48k ≤ 50k cap                         |
//!
//! Simulated damage event: 60 dirty tiles from a moderate scroll or UI update
//! on an 848×480 display.  80% are TEXT/FLAT (lossless in coarse pass), 20%
//! are PICTURE (12 tiles needing refinement).
//!
//! Refinement time = 12 tiles × 400 bytes/tile × 8 bits/byte / 48 000 bps
//!                 = 800 ms ≤ 1 000 ms deadline.
//!
//! # Assertions
//!
//! 1. TEXT and FLAT tiles are immediately pixel-exact (`coarse_is_lossless()`).
//! 2. PICTURE tiles require refinement (`needs_refinement()`).
//! 3. The refinement queue orders by saliency (TEXT before PICTURE).
//! 4. Refinement of all PICTURE tiles in the damage event completes within
//!    [`PIXEL_EXACT_DEADLINE_MS`] ms at the gear-policy screen_refinement_bps.
//! 5. TEXT and FLAT tiles do not need refinement (no bandwidth wasted).
//! 6. Tile classification correctly maps colour counts to tile classes.
//! 7. The tile grid covers the full 848×480 display.

use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::screen_encoder::{
    classify_tile, RefinementQueue, TileClass, TileCoord,
    LOSSLESS_BYTES_PER_PICTURE_TILE, PIXEL_EXACT_DEADLINE_MS, TILE_BYTES, TILE_SIZE_PX,
};
use lowband_platform::thermal::ThermalPressure;

// ── Link budget ───────────────────────────────────────────────────────────────

/// Comfortable-tier link rate for this scenario (bps).
const LINK_BPS: u32 = 400_000;

// ── Display geometry ──────────────────────────────────────────────────────────

const SCREEN_W: u32 = 848;
const SCREEN_H: u32 = 480;

const TILE_COLS: u32 = (SCREEN_W + TILE_SIZE_PX - 1) / TILE_SIZE_PX; // 27
const TILE_ROWS: u32 = (SCREEN_H + TILE_SIZE_PX - 1) / TILE_SIZE_PX; // 15

// ── Damage scenario ───────────────────────────────────────────────────────────

/// Total dirty tiles in the damage event.
const DIRTY_TILES: u32 = 60;

/// TEXT/FLAT tiles in the damage event (lossless in the coarse pass).
const TEXT_TILES: u32 = 48; // 80% of 60

/// PICTURE tiles in the damage event (need lossless refinement).
const PICTURE_TILES: u32 = DIRTY_TILES - TEXT_TILES; // 12

// ── 1. TEXT tile is lossless in the first pass ────────────────────────────────

#[test]
fn text_tile_is_lossless_in_first_pass() {
    // 32×32 BGRA8 tile with 2 distinct colours: white background, black strokes.
    let mut pixels = vec![0xFFu8; TILE_BYTES];
    for i in 0..1024usize {
        if i % 16 == 0 {
            let off = i * 4;
            pixels[off]     = 0x00; // B
            pixels[off + 1] = 0x00; // G
            pixels[off + 2] = 0x00; // R
            pixels[off + 3] = 0xFF; // A
        }
    }

    let class = classify_tile(&pixels);
    assert!(
        class == TileClass::Text || class == TileClass::Flat,
        "2-colour tile must classify as Text or Flat; got {class:?}"
    );
    assert!(
        class.coarse_is_lossless(),
        "TEXT / FLAT tiles must be lossless in the coarse first_pass; \
         a technician must see correct characters in frame 1 before refinement"
    );
    assert!(
        !class.needs_refinement(),
        "TEXT / FLAT tiles must not enter the refinement queue — they are \
         already pixel-exact and queuing them wastes refinement bandwidth"
    );
}

// ── 2. PICTURE tile requires refinement ──────────────────────────────────────

#[test]
fn picture_tile_requires_lossless_refinement() {
    // 30 distinct colours via 5 × 6 (R>>4, G>>4) pairs.
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let c = i % 30;
        chunk[0] = 0x00;
        chunk[1] = ((c % 6) as u8) << 4;
        chunk[2] = ((c / 6) as u8) << 4;
        chunk[3] = 0xFF;
    }

    let class = classify_tile(&pixels);
    assert_eq!(class, TileClass::Picture, "30-colour tile must classify as Picture");
    assert!(
        !class.coarse_is_lossless(),
        "Picture tiles are encoded lossily in the coarse pass; \
         pixel-exact quality arrives in the refinement pass"
    );
    assert!(
        class.needs_refinement(),
        "Picture tile must be enqueued for lossless refinement after the coarse pass"
    );
}

// ── 3. Refinement queue orders by saliency ───────────────────────────────────

#[test]
fn refinement_queue_orders_text_before_picture() {
    let mut q = RefinementQueue::new();

    let picture_coord = TileCoord { col: 0, row: 0 };
    let text_coord    = TileCoord { col: 1, row: 0 };

    q.push(picture_coord, TileClass::Picture); // inserted first, lower saliency
    q.push(text_coord,    TileClass::Text);    // inserted second, higher saliency

    let (coord, class) = q.pop().expect("queue has two entries");
    assert_eq!(class, TileClass::Text,    "Text (saliency=3) must be dequeued before Picture (saliency=1)");
    assert_eq!(coord, text_coord);

    let (coord, class) = q.pop().expect("queue still has one entry");
    assert_eq!(class, TileClass::Picture);
    assert_eq!(coord, picture_coord);

    assert!(q.is_empty());
}

// ── 4. Pixel-exact within about one second ────────────────────────────────────

#[test]
fn pixel_exact_within_one_second_at_comfortable_tier() {
    // Derive per-stream budgets at 400 kbps under nominal thermal pressure.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);
    let refinement_bps = budgets.screen_refinement_bps;

    assert!(
        refinement_bps > 0,
        "screen_refinement_bps must be > 0 at {LINK_BPS} bps (comfortable tier); \
         without refinement budget the pixel-exact guarantee cannot be met"
    );

    // Enqueue all PICTURE tiles from the damage event.
    let mut q = RefinementQueue::new();
    for i in 0..PICTURE_TILES {
        q.push(
            TileCoord { col: i % TILE_COLS, row: (i / TILE_COLS) % TILE_ROWS },
            TileClass::Picture,
        );
    }
    assert_eq!(q.len(), PICTURE_TILES as usize);

    // Compute worst-case refinement time: send all tiles at the refinement rate.
    let total_bits   = PICTURE_TILES as u64 * LOSSLESS_BYTES_PER_PICTURE_TILE * 8;
    let refine_ms    = total_bits * 1_000 / refinement_bps as u64;

    eprintln!(
        "pixel_exact  link={LINK_BPS} bps  refine_bps={refinement_bps}  \
         picture_tiles={PICTURE_TILES}  bytes/tile={LOSSLESS_BYTES_PER_PICTURE_TILE}\n  \
         total_bits={total_bits}  refine_time={refine_ms} ms  \
         deadline={PIXEL_EXACT_DEADLINE_MS} ms"
    );

    assert!(
        refine_ms <= PIXEL_EXACT_DEADLINE_MS,
        "{PICTURE_TILES} PICTURE tiles × {LOSSLESS_BYTES_PER_PICTURE_TILE} bytes/tile = \
         {total_bits} bits; at {refinement_bps} bps refinement that is {refine_ms} ms, \
         which exceeds the {PIXEL_EXACT_DEADLINE_MS} ms pixel-exact deadline"
    );

    // Drain the queue — all tiles must be consumed.
    while q.pop().is_some() {}
    assert!(q.is_empty(), "refinement queue must be empty after all tiles are processed");
}

// ── 5. TEXT and FLAT tiles do not consume refinement bandwidth ────────────────

#[test]
fn text_flat_video_tiles_skip_refinement() {
    assert!(
        !TileClass::Text.needs_refinement(),
        "Text tiles are already lossless in the coarse pass"
    );
    assert!(
        !TileClass::Flat.needs_refinement(),
        "Flat tiles are already lossless in the coarse pass"
    );
    assert!(
        !TileClass::Video.needs_refinement(),
        "Video tiles use the Gear-B AV1 sub-stream, not the refinement queue"
    );
}

// ── 6. Tile classification thresholds ────────────────────────────────────────

#[test]
fn classify_tile_maps_colour_counts_to_classes() {
    // 1 colour → Text (≤ 4)
    let mono = vec![0x80u8; TILE_BYTES];
    assert_eq!(classify_tile(&mono), TileClass::Text);

    // 8 distinct colours → Flat (5–16)
    let mut flat_px = vec![0u8; TILE_BYTES];
    for (i, chunk) in flat_px.chunks_exact_mut(4).enumerate() {
        let c = (i % 8) as u8;
        chunk[0] = 0x00;
        chunk[1] = 0x00;
        chunk[2] = c << 4; // top 4 bits differ: 0x00, 0x10, 0x20, …, 0x70
        chunk[3] = 0xFF;
    }
    assert_eq!(classify_tile(&flat_px), TileClass::Flat);

    // 30 distinct colours → Picture (17–256)
    let mut pic_px = vec![0u8; TILE_BYTES];
    for (i, chunk) in pic_px.chunks_exact_mut(4).enumerate() {
        let c = i % 30;
        chunk[0] = 0x00;
        chunk[1] = ((c % 6) as u8) << 4;
        chunk[2] = ((c / 6) as u8) << 4;
        chunk[3] = 0xFF;
    }
    assert_eq!(classify_tile(&pic_px), TileClass::Picture);
}

// ── 7. Tile grid covers the full 848×480 display ─────────────────────────────

#[test]
fn tile_grid_covers_full_848x480_display() {
    assert_eq!(TILE_COLS, 27, "ceil(848 / 32) = 27 columns");
    assert_eq!(TILE_ROWS, 15, "ceil(480 / 32) = 15 rows");

    let total = TILE_COLS * TILE_ROWS;
    assert_eq!(total, 405, "27 × 15 = 405 tiles cover the 848×480 display");

    // Every pixel in the 848×480 display maps to at least one tile.
    for y in 0..SCREEN_H {
        for x in 0..SCREEN_W {
            let col = x / TILE_SIZE_PX;
            let row = y / TILE_SIZE_PX;
            assert!(col < TILE_COLS && row < TILE_ROWS, "pixel ({x},{y}) outside tile grid");
        }
    }
}

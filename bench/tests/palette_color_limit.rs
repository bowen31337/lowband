//! Feature 95 — tile escape to PICTURE coding with color_count over 16 distinct colours.
//!
//! # Purpose
//!
//! Verifies that `classify_tile` applies the palette-limit escape correctly:
//! tiles with ≤ [`PALETTE_COLOR_LIMIT`] (16) distinct colour fingerprints receive
//! lossless palette coding (TEXT or FLAT), while tiles whose fingerprint count
//! exceeds 16 escape to PICTURE coding — a fast lossy AV1 coarse pass followed
//! by a deferred lossless refinement via the `priority_queue`.
//!
//! Colour distinctness is measured by a 12-bit fingerprint
//! `(R >> 4) << 8 | (G >> 4) << 4 | (B >> 4)`.  Only changes in the upper 4 bits
//! of each channel register a new entry in the 4 096-bit seen-set; two BGRA8 pixels
//! that differ only in the lower 4 bits of any channel hash to the same fingerprint
//! and are treated as one distinct colour.
//!
//! # Assertions
//!
//! 1. A tile with exactly 16 distinct colour fingerprints classifies as Flat.
//! 2. A tile with 17 distinct colour fingerprints escapes to Picture.
//! 3. The Flat tile (≤ 16 colours) is lossless in the coarse pass.
//! 4. The Picture tile (> 16 colours) is not lossless in the coarse pass.
//! 5. The Flat tile does not need refinement.
//! 6. The Picture tile needs lossless refinement after the coarse pass.
//! 7. Sub-nibble variation (colours differing only in the lower 4 bits) does not
//!    register as a new distinct colour — the tile stays within the palette limit.
//! 8. Introducing one new fingerprint-distinct colour beyond 16 is sufficient to
//!    trigger the PICTURE escape; all 16 original fingerprints remain present.

use lowband_platform::screen_encoder::{
    classify_tile, TileClass, PALETTE_COLOR_LIMIT, TILE_BYTES,
};

// ── Helper ────────────────────────────────────────────────────────────────────

/// Build a BGRA8 tile whose pixels cycle through exactly `n` fingerprint-distinct
/// colours.  Each colour `c` maps to a unique 12-bit fingerprint
/// `(r_nibble, g_nibble, b_nibble)` derived from `c` so that no two values of
/// `c` in `0..n` collide.  `n` must be ≤ 4 096 (the fingerprint space).
fn tile_with_n_colours(n: usize) -> Vec<u8> {
    assert!(n >= 1 && n <= 4096, "colour count must be in 1..=4096");
    let mut px = vec![0u8; TILE_BYTES];
    for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
        let c = i % n;
        let b_nibble = (c % 16) as u8;
        let g_nibble = ((c / 16) % 16) as u8;
        let r_nibble = ((c / 256) % 16) as u8;
        chunk[0] = b_nibble << 4; // B: upper nibble carries the fingerprint bit
        chunk[1] = g_nibble << 4; // G
        chunk[2] = r_nibble << 4; // R
        chunk[3] = 0xFF;
    }
    px
}

// ── 1. Exactly 16 distinct colours → Flat ────────────────────────────────────

#[test]
fn exactly_palette_limit_colours_classifies_as_flat() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT);
    let class = classify_tile(&px);
    assert_eq!(
        class,
        TileClass::Flat,
        "a tile with exactly {PALETTE_COLOR_LIMIT} distinct colour fingerprints \
         must classify as Flat — it sits at the palette limit and must not escape \
         to PICTURE; got {class:?}"
    );
}

// ── 2. One colour over the limit → Picture ────────────────────────────────────

#[test]
fn one_colour_over_palette_limit_escapes_to_picture() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT + 1);
    let class = classify_tile(&px);
    assert_eq!(
        class,
        TileClass::Picture,
        "a tile with {} distinct colour fingerprints must escape to PICTURE — \
         color_count ({}) exceeds PALETTE_COLOR_LIMIT ({}); got {class:?}",
        PALETTE_COLOR_LIMIT + 1,
        PALETTE_COLOR_LIMIT + 1,
        PALETTE_COLOR_LIMIT,
    );
}

// ── 3 & 4. Coarse-pass lossless property ─────────────────────────────────────

#[test]
fn flat_at_palette_limit_is_lossless_in_coarse_pass() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT);
    let class = classify_tile(&px);
    assert!(
        class.coarse_is_lossless(),
        "a Flat tile (≤ {PALETTE_COLOR_LIMIT} colours) must be lossless in the \
         coarse pass via palette_index coding; got {class:?} with \
         coarse_is_lossless={}",
        class.coarse_is_lossless(),
    );
}

#[test]
fn picture_escape_is_not_lossless_in_coarse_pass() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT + 1);
    let class = classify_tile(&px);
    assert!(
        !class.coarse_is_lossless(),
        "a Picture tile (> {PALETTE_COLOR_LIMIT} colours) must not be lossless \
         in the coarse pass — it uses fast lossy AV1; pixel-exact quality \
         arrives in the refinement pass; got {class:?} with \
         coarse_is_lossless={}",
        class.coarse_is_lossless(),
    );
}

// ── 5 & 6. Refinement-queue property ─────────────────────────────────────────

#[test]
fn flat_at_palette_limit_does_not_need_refinement() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT);
    let class = classify_tile(&px);
    assert!(
        !class.needs_refinement(),
        "a Flat tile (≤ {PALETTE_COLOR_LIMIT} colours) must not enter the \
         refinement queue — it is already pixel-exact in the coarse pass; \
         enqueueing it would waste idle refinement bandwidth; \
         got {class:?} with needs_refinement={}",
        class.needs_refinement(),
    );
}

#[test]
fn picture_escape_needs_lossless_refinement() {
    let px = tile_with_n_colours(PALETTE_COLOR_LIMIT + 1);
    let class = classify_tile(&px);
    assert!(
        class.needs_refinement(),
        "a Picture tile (> {PALETTE_COLOR_LIMIT} colours) must be enqueued for \
         lossless refinement after the coarse pass so the viewer receives \
         pixel-exact quality within the PIXEL_EXACT_DEADLINE_MS window; \
         got {class:?} with needs_refinement={}",
        class.needs_refinement(),
    );
}

// ── 7. Sub-nibble variation does not cross the palette limit ──────────────────

#[test]
fn sub_nibble_variation_stays_within_palette_limit() {
    // A tile with PALETTE_COLOR_LIMIT fingerprint-distinct colours but many
    // more full-depth BGRA8 colours: each of the 16 base fingerprints appears
    // in pixels where the lower 4 bits of B vary (0x00, 0x01, …, 0x0F).
    // The fingerprint computation strips those lower bits, so all 16 sub-nibble
    // variants of a base colour collapse onto the same seen-set entry.
    let mut px = vec![0u8; TILE_BYTES];
    for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
        let base = i % PALETTE_COLOR_LIMIT;          // 16 fingerprint-distinct bases
        let sub  = ((i / PALETTE_COLOR_LIMIT) & 0xF) as u8; // 0..15 sub-nibble variation
        // Upper nibble of B carries the fingerprint; lower nibble carries sub-variation.
        // The fingerprint is (R>>4, G>>4, B>>4), so only the upper nibble matters.
        chunk[0] = ((base % 16) as u8) << 4 | sub;  // B: fingerprint nibble | sub
        chunk[1] = 0x00;                              // G
        chunk[2] = 0x00;                              // R
        chunk[3] = 0xFF;
    }
    let class = classify_tile(&px);
    assert_eq!(
        class,
        TileClass::Flat,
        "sub-nibble variation within each of the {PALETTE_COLOR_LIMIT} base \
         fingerprints must not register as additional distinct colours; \
         the tile must still classify as Flat, not Picture; got {class:?}"
    );
}

// ── 8. Adding a 17th fingerprint triggers the PICTURE escape ─────────────────

#[test]
fn adding_seventeenth_fingerprint_triggers_picture_escape() {
    // A tile at the palette boundary (16 colours) is Flat.
    let mut px = tile_with_n_colours(PALETTE_COLOR_LIMIT);
    assert_eq!(
        classify_tile(&px),
        TileClass::Flat,
        "precondition: tile with {PALETTE_COLOR_LIMIT} colours must be Flat"
    );

    // Overwrite the first pixel with a 17th fingerprint-distinct colour.
    // tile_with_n_colours(16) uses fingerprints (b_nibble=c%16, g=0, r=0) for c in 0..16.
    // Introduce a new fingerprint: b=0, g=0x10 (g_nibble=1) — not present in the original 16.
    let first_pixel = 0; // byte offset of pixel 0
    px[first_pixel]     = 0x00; // B nibble = 0
    px[first_pixel + 1] = 0x10; // G nibble = 1 — new fingerprint (0, 1, 0)
    px[first_pixel + 2] = 0x00; // R nibble = 0
    px[first_pixel + 3] = 0xFF;

    let class = classify_tile(&px);
    assert_eq!(
        class,
        TileClass::Picture,
        "introducing a 17th fingerprint-distinct colour must trigger the \
         PICTURE escape: color_count ({}) now exceeds PALETTE_COLOR_LIMIT ({}); \
         got {class:?}",
        PALETTE_COLOR_LIMIT + 1,
        PALETTE_COLOR_LIMIT,
    );
}

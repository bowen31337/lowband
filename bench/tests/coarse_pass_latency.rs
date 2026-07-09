//! Feature 97 — coarse_pass latency: viewer sees the change within 50 ms.
//!
//! # Purpose
//!
//! Verifies that the coarse pass satisfies its 50 ms transmission deadline
//! (Feature 97) across the tile classes produced by the screen encoder.
//!
//! TEXT tiles use AV1 `palette_index` coding (Features 93–94) and achieve a
//! compact on-wire size of roughly [`COARSE_BYTES_PER_PALETTE_TILE`] bytes
//! each.  PICTURE tiles use a fast lossy AV1 preset and achieve roughly
//! [`COARSE_BYTES_PER_PICTURE_TILE_COARSE`] bytes each.  Both are smaller
//! than the lossless refinement target ([`LOSSLESS_BYTES_PER_PICTURE_TILE`]
//! = 400 B), ensuring the coarse pass fits within the 50 ms window at the
//! `screen_coarse_bps` allocated by the gear policy.
//!
//! # Scenario
//!
//! Constrained-tier session at 64 kbps (minimum link rate that sustains a
//! useful screen remote session):
//!
//! | Stream          | Budget   | Derivation                                    |
//! |-----------------|----------|-----------------------------------------------|
//! | Voice           | 24 kbps  | audio floor                                   |
//! | Input / cursor  |  8 kbps  | architecture minimum                          |
//! | Screen coarse   | 20 kbps  | capped by gear policy                         |
//! | Camera          |  0 kbps  | no remaining budget                           |
//! | Screen refine   |  0 kbps  | no remaining budget                           |
//!
//! In the 50 ms coarse-pass window, 20 kbps delivers:
//!
//!   20 000 bps × 0.050 s / 8 bits/byte = 125 bytes
//!
//! # Assertions
//!
//! 1. Four TEXT tiles (single typed glyph) = 4 × 30 B = 120 B ≤ 125 B:
//!    fits the 50 ms window at constrained tier.
//! 2. One PICTURE tile (icon update) = 75 B ≤ 125 B:
//!    fits the 50 ms window at constrained tier.
//! 3. Fast lossy coarse bytes (75 B) < lossless refinement bytes (400 B):
//!    the refinement pass improves on a first-impression encode.
//! 4. Palette coarse bytes (30 B) ≤ picture coarse bytes (75 B):
//!    simpler tiles produce smaller coarse output.

use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::screen_encoder::{
    COARSE_BYTES_PER_PALETTE_TILE, COARSE_BYTES_PER_PICTURE_TILE_COARSE,
    COARSE_PASS_DEADLINE_MS, LOSSLESS_BYTES_PER_PICTURE_TILE,
};
use lowband_platform::thermal::ThermalPressure;

// ── Link budget ───────────────────────────────────────────────────────────────

/// Constrained-tier link rate (bps).
const LINK_BPS: u32 = 64_000;

// ── Damage scenarios ──────────────────────────────────────────────────────────

/// Tiles dirtied by a single typed character on a 32 px/tile terminal grid.
///
/// A glyph is typically 8–16 px wide × 16 px tall and touches a 2×2 block of
/// 32 px tiles, yielding 4 dirty TEXT tiles per keystroke.
const GLYPH_TILES: u64 = 4;

// ── 1. Four TEXT tiles (single typed glyph) fit within 50 ms ─────────────────

#[test]
fn text_glyph_coarse_pass_within_50ms_at_constrained_tier() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);
    let screen_coarse_bps = budgets.screen_coarse_bps as u64;

    assert!(
        screen_coarse_bps > 0,
        "screen_coarse_bps must be > 0 at {LINK_BPS} bps; \
         without a coarse budget the 50 ms guarantee cannot be met"
    );

    // Total bits for the coarse pass of a single-glyph damage event.
    let total_bits = GLYPH_TILES * COARSE_BYTES_PER_PALETTE_TILE * 8;
    let coarse_ms  = total_bits * 1_000 / screen_coarse_bps;

    eprintln!(
        "coarse_pass  link={LINK_BPS} bps  screen_coarse_bps={screen_coarse_bps}  \
         glyph_tiles={GLYPH_TILES}  bytes/tile={COARSE_BYTES_PER_PALETTE_TILE}\n  \
         total_bits={total_bits}  coarse_time={coarse_ms} ms  \
         deadline={COARSE_PASS_DEADLINE_MS} ms"
    );

    assert!(
        coarse_ms <= COARSE_PASS_DEADLINE_MS,
        "{GLYPH_TILES} TEXT tiles × {COARSE_BYTES_PER_PALETTE_TILE} bytes/tile = \
         {total_bits} bits; at {screen_coarse_bps} bps that is {coarse_ms} ms, \
         which exceeds the {COARSE_PASS_DEADLINE_MS} ms coarse-pass deadline — \
         a typed character must be visible within 50 ms of the damage event"
    );
}

// ── 2. One PICTURE tile (icon update) fits within 50 ms ──────────────────────

#[test]
fn picture_tile_coarse_pass_within_50ms_at_constrained_tier() {
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);
    let screen_coarse_bps = budgets.screen_coarse_bps as u64;

    let total_bits = COARSE_BYTES_PER_PICTURE_TILE_COARSE * 8;
    let coarse_ms  = total_bits * 1_000 / screen_coarse_bps;

    eprintln!(
        "coarse_pass picture  link={LINK_BPS} bps  \
         screen_coarse_bps={screen_coarse_bps}  \
         bytes/tile={COARSE_BYTES_PER_PICTURE_TILE_COARSE}  \
         coarse_time={coarse_ms} ms  deadline={COARSE_PASS_DEADLINE_MS} ms"
    );

    assert!(
        coarse_ms <= COARSE_PASS_DEADLINE_MS,
        "1 PICTURE tile × {COARSE_BYTES_PER_PICTURE_TILE_COARSE} bytes = \
         {total_bits} bits; at {screen_coarse_bps} bps that is {coarse_ms} ms, \
         which exceeds the {COARSE_PASS_DEADLINE_MS} ms coarse-pass deadline — \
         an icon change must be visible within 50 ms of the damage event"
    );
}

// ── 3. Fast lossy coarse < lossless refinement ────────────────────────────────

#[test]
fn coarse_picture_tile_smaller_than_lossless_refinement() {
    assert!(
        COARSE_BYTES_PER_PICTURE_TILE_COARSE < LOSSLESS_BYTES_PER_PICTURE_TILE,
        "fast lossy coarse ({COARSE_BYTES_PER_PICTURE_TILE_COARSE} B) must be smaller \
         than lossless refinement ({LOSSLESS_BYTES_PER_PICTURE_TILE} B): \
         the coarse pass provides a quick first impression; \
         the refinement pass improves to pixel-exact quality within ~1 second"
    );
}

// ── 4. Palette coarse ≤ picture coarse ────────────────────────────────────────

#[test]
fn palette_coarse_tiles_no_larger_than_picture_coarse() {
    assert!(
        COARSE_BYTES_PER_PALETTE_TILE <= COARSE_BYTES_PER_PICTURE_TILE_COARSE,
        "palette coarse ({COARSE_BYTES_PER_PALETTE_TILE} B) must not exceed \
         picture coarse ({COARSE_BYTES_PER_PICTURE_TILE_COARSE} B): \
         TEXT and FLAT tiles have fewer colours and simpler index planes; \
         they must produce smaller coarse output than complex PICTURE tiles"
    );
}

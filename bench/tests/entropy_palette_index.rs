//! Feature 94 — System entropy-codes the palette losslessly with index_map
//! context modeling from left and above.
//!
//! # Purpose
//!
//! Verifies the [`EntropyPaletteEncoder`] / [`EntropyPaletteDecoder`] pair
//! (Feature 94), which replaces the raw bit-packed index stream of Feature 93
//! with a context-adaptive hit/miss code derived from the left and above
//! neighbour palette indices.
//!
//! ## Context model
//!
//! For each pixel at (row, col) in raster order the encoder builds a
//! **Context Colour Order** (CCO):
//!
//! * CCO[0] = left neighbour index  (or 0 when col == 0)
//! * CCO[1] = above neighbour index (omitted when equal to left)
//! * CCO[2..] = remaining indices in natural order
//!
//! The actual palette index rank `k` in the CCO is then coded:
//!
//! | k | bits                                       |
//! |---|--------------------------------------------|
//! | 0 | "0" (1 bit — context hit)                   |
//! | k | "1" + (k−1) in `bits_per_index(n−1)` bits   |
//!
//! For n == 2 every pixel costs exactly 1 bit regardless of k (same as raw).
//! For n ≥ 3 context hits (rank 0) save bits relative to raw bit-packing
//! whenever the hit-rate exceeds a palette-size-dependent threshold.
//!
//! ## Wire format
//!
//! The header is identical to [`PaletteTileEncoder`]:
//!
//! ```text
//! byte  0            : n_colors (1..=16)
//! bytes 1..(1+n*3)   : palette — n × [B, G, R] (full 4:4:4 chroma)
//! bytes (1+n*3)..    : entropy-coded CCO-rank stream; final byte zero-padded
//! ```
//!
//! # Assertions
//!
//! 1.  Single-colour TEXT tile (1 colour) round-trips losslessly.
//! 2.  Two-colour TEXT tile round-trips losslessly.
//! 3.  Four-colour TEXT tile round-trips losslessly.
//! 4.  Eight-colour FLAT tile round-trips losslessly.
//! 5.  Sixteen-colour FLAT tile round-trips losslessly.
//! 6.  Full 4:4:4 chroma: all RGB channels preserved exactly after entropy round-trip.
//! 7.  Entropy encoder rejects 17-colour tile as TooManyColors.
//! 8.  Entropy encoder rejects wrong-length input as InvalidLength.
//! 9.  Entropy decoder rejects empty input as Truncated.
//! 10. Entropy decoder rejects n_colors = 0 as InvalidPaletteSize.
//! 11. Entropy decoder rejects n_colors > 16 as InvalidPaletteSize.
//! 12. Single-colour tile produces minimum bitstream: 4 bytes (no index stream).
//! 13. Wire format: n_colors header and palette table identical to PaletteTileEncoder.
//! 14. Left-context compression: n=8 horizontal-run tile is substantially smaller than raw.
//! 15. Left-context compression: n=16 horizontal-run tile is substantially smaller than raw.
//! 16. Two-colour tile: entropy-coded index stream is exactly 128 bytes (1 bit/pixel,
//!     same as raw; n=2 gives no compression gain or regression).
//! 17. Context model correctness: two-phase tile (top half / bottom half split) round-trips.
//! 18. Entropy decoder rejects a truncated palette table as Truncated.
//! 19. Entropy decoder rejects a truncated entropy index stream as Truncated.
//! 20. Adversarial 16-colour cycling tile (zero context hits) round-trips correctly.

use lowband_platform::screen_encoder::{
    EntropyPaletteDecoder, EntropyPaletteEncoder,
    PaletteDecodeError, PaletteEncodeError, PaletteTileEncoder,
    PALETTE_COLOR_LIMIT, TILE_BYTES, TILE_SIZE_PX,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a BGRA8 32×32 tile whose pixels cycle through exactly `n_colors`
/// distinct colours, each colour identified by a unique (B, G, R) triplet.
fn tile_with_n_exact_colours(n_colors: usize) -> Vec<u8> {
    assert!(n_colors >= 1 && n_colors <= 256);
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let c = i % n_colors;
        chunk[0] = c as u8;
        chunk[1] = (c / 16) as u8;
        chunk[2] = 0x00;
        chunk[3] = 0xFF;
    }
    pixels
}

/// Build a tile with `n_colors` colours arranged in horizontal runs: each row
/// is filled entirely with one colour, cycling through the `n_colors` palette
/// entries.  The left-context hit rate is ≈ (TILE_SIZE_PX − 1) / TILE_SIZE_PX
/// (≈ 97 %) because every pixel except the first column of each colour group
/// has a left neighbour with the same index.
fn tile_with_horizontal_runs(n_colors: usize) -> Vec<u8> {
    assert!(n_colors >= 1 && n_colors <= TILE_SIZE_PX as usize);
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let row = i / TILE_SIZE_PX as usize;
        let color = (row % n_colors) as u8;
        chunk[0] = color << 4;
        chunk[1] = 0x00;
        chunk[2] = 0x00;
        chunk[3] = 0xFF;
    }
    pixels
}

/// Assert BGR channels of `decoded` match `original` pixel-for-pixel.
/// Alpha in the decoded output is expected to be `0xFF`.
fn assert_round_trip(original: &[u8], decoded: &[u8], label: &str) {
    assert_eq!(decoded.len(), TILE_BYTES, "{label}: decoded length mismatch");
    for (i, (d, o)) in decoded.chunks_exact(4).zip(original.chunks_exact(4)).enumerate() {
        assert_eq!(d[0], o[0], "{label}: pixel {i} B mismatch");
        assert_eq!(d[1], o[1], "{label}: pixel {i} G mismatch");
        assert_eq!(d[2], o[2], "{label}: pixel {i} R mismatch");
        assert_eq!(d[3], 0xFF, "{label}: pixel {i} alpha must be 0xFF");
    }
}

// ── 1. Single-colour TEXT tile round-trips ────────────────────────────────────

#[test]
fn single_colour_tile_round_trips() {
    let pixels  = tile_with_n_exact_colours(1);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "single-colour");
}

// ── 2. Two-colour TEXT tile round-trips ──────────────────────────────────────

#[test]
fn two_colour_tile_round_trips() {
    let pixels  = tile_with_n_exact_colours(2);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "two-colour");
}

// ── 3. Four-colour TEXT tile round-trips ─────────────────────────────────────

#[test]
fn four_colour_tile_round_trips() {
    let pixels  = tile_with_n_exact_colours(4);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "four-colour");
}

// ── 4. Eight-colour FLAT tile round-trips ────────────────────────────────────

#[test]
fn eight_colour_flat_tile_round_trips() {
    let pixels  = tile_with_n_exact_colours(8);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "eight-colour-flat");
}

// ── 5. Sixteen-colour FLAT tile round-trips ──────────────────────────────────

#[test]
fn sixteen_colour_flat_tile_round_trips() {
    let pixels  = tile_with_n_exact_colours(PALETTE_COLOR_LIMIT);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "sixteen-colour-flat");
}

// ── 6. Full 4:4:4 chroma preserved ───────────────────────────────────────────

#[test]
fn full_444_chroma_all_channels_preserved() {
    let palette = [
        [0x11u8, 0x22, 0x33],
        [0x44,   0x55, 0x66],
        [0x77,   0x88, 0x99],
        [0xAA,   0xBB, 0xCC],
        [0xDD,   0xEE, 0xFF],
        [0x10,   0x20, 0x30],
        [0x40,   0x50, 0x60],
        [0x70,   0x80, 0x90],
    ];
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let [b, g, r] = palette[i % 8];
        chunk[0] = b; chunk[1] = g; chunk[2] = r; chunk[3] = 0xFF;
    }
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    for (i, chunk) in decoded.chunks_exact(4).enumerate() {
        let expected = palette[i % 8];
        assert_eq!(chunk[0], expected[0], "pixel {i}: B channel mismatch");
        assert_eq!(chunk[1], expected[1], "pixel {i}: G channel mismatch");
        assert_eq!(chunk[2], expected[2], "pixel {i}: R channel mismatch");
        assert_eq!(chunk[3], 0xFF,        "pixel {i}: alpha must be 0xFF");
    }
}

// ── 7. Encoder rejects 17 distinct colours ───────────────────────────────────

#[test]
fn encoder_rejects_seventeen_colours() {
    let pixels = tile_with_n_exact_colours(PALETTE_COLOR_LIMIT + 1);
    match EntropyPaletteEncoder::encode(&pixels) {
        Err(PaletteEncodeError::TooManyColors { found }) => {
            assert!(
                found > PALETTE_COLOR_LIMIT,
                "found={found} must exceed PALETTE_COLOR_LIMIT={PALETTE_COLOR_LIMIT}"
            );
        }
        other => panic!("expected TooManyColors, got {other:?}"),
    }
}

// ── 8. Encoder rejects wrong-length input ────────────────────────────────────

#[test]
fn encoder_rejects_wrong_length_input() {
    let short = vec![0u8; TILE_BYTES - 1];
    match EntropyPaletteEncoder::encode(&short) {
        Err(PaletteEncodeError::InvalidLength { got }) => {
            assert_eq!(got, TILE_BYTES - 1);
        }
        other => panic!("expected InvalidLength for short input, got {other:?}"),
    }
    let long = vec![0u8; TILE_BYTES + 4];
    match EntropyPaletteEncoder::encode(&long) {
        Err(PaletteEncodeError::InvalidLength { got }) => {
            assert_eq!(got, TILE_BYTES + 4);
        }
        other => panic!("expected InvalidLength for long input, got {other:?}"),
    }
}

// ── 9. Decoder rejects empty input ───────────────────────────────────────────

#[test]
fn decoder_rejects_empty_input() {
    assert_eq!(
        EntropyPaletteDecoder::decode(&[]),
        Err(PaletteDecodeError::Truncated),
        "empty input must produce Truncated"
    );
}

// ── 10. Decoder rejects n_colors = 0 ─────────────────────────────────────────

#[test]
fn decoder_rejects_zero_palette_size() {
    assert_eq!(
        EntropyPaletteDecoder::decode(&[0, 0, 0, 0]),
        Err(PaletteDecodeError::InvalidPaletteSize { got: 0 }),
        "n_colors=0 must produce InvalidPaletteSize"
    );
}

// ── 11. Decoder rejects n_colors > 16 ────────────────────────────────────────

#[test]
fn decoder_rejects_oversized_palette_size() {
    let bad = PALETTE_COLOR_LIMIT as u8 + 1;
    assert_eq!(
        EntropyPaletteDecoder::decode(&[bad]),
        Err(PaletteDecodeError::InvalidPaletteSize { got: bad }),
        "n_colors={bad} must produce InvalidPaletteSize"
    );
}

// ── 12. Single-colour tile: minimum bitstream ─────────────────────────────────

#[test]
fn single_colour_tile_produces_minimum_bitstream() {
    // n=1: 1 (n_colors) + 3 (palette) = 4 bytes, no index stream.
    let pixels  = tile_with_n_exact_colours(1);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    assert_eq!(
        encoded.len(), 4,
        "single-colour entropy bitstream must be 4 bytes \
         (1 header + 3 palette; no index stream); got {}",
        encoded.len()
    );
    assert_eq!(encoded[0], 1, "n_colors header must be 1");
}

// ── 13. Wire format: header identical to PaletteTileEncoder ──────────────────

#[test]
fn wire_format_header_identical_to_raw_encoder() {
    // For any n, the entropy encoder and the raw encoder share the same
    // header layout: byte 0 = n_colors, bytes 1..1+n*3 = palette in BGR order.
    for n in [1usize, 2, 4, 8, PALETTE_COLOR_LIMIT] {
        let pixels     = tile_with_n_exact_colours(n);
        let raw_enc    = PaletteTileEncoder::encode(&pixels).unwrap();
        let entropy_enc = EntropyPaletteEncoder::encode(&pixels).unwrap();

        let header_len = 1 + n * 3;
        assert_eq!(
            entropy_enc[0], n as u8,
            "n={n}: entropy n_colors header must equal {n}"
        );
        assert_eq!(
            entropy_enc[..header_len], raw_enc[..header_len],
            "n={n}: entropy and raw headers must be identical (n_colors + palette)"
        );
    }
}

// ── 14. Left-context compression: n=8 horizontal-run tile ────────────────────

#[test]
fn horizontal_run_tile_n8_entropy_smaller_than_raw() {
    // 8-colour tile, 4 rows per colour.  After the first column of each new
    // colour group every pixel sees left == actual → rank 0 → 1-bit hit.
    // Hit rate ≈ 31/32 ≈ 97 %.  Entropy output must be substantially smaller
    // than raw bit-packed (3 bits/pixel × 1024 = 384 bytes index stream).
    let pixels  = tile_with_horizontal_runs(8);
    let raw     = PaletteTileEncoder::encode(&pixels).unwrap();
    let entropy = EntropyPaletteEncoder::encode(&pixels).unwrap();

    assert!(
        entropy.len() < raw.len(),
        "n=8 horizontal-run tile: entropy ({} B) must be smaller than raw ({} B)",
        entropy.len(), raw.len()
    );

    // Sanity-check round-trip correctness.
    let decoded = EntropyPaletteDecoder::decode(&entropy).unwrap();
    assert_round_trip(&pixels, &decoded, "n=8 horizontal-run");
}

// ── 15. Left-context compression: n=16 horizontal-run tile ───────────────────

#[test]
fn horizontal_run_tile_n16_entropy_smaller_than_raw() {
    // 16-colour tile, 2 rows per colour.  Same argument as n=8 but with a
    // larger palette; raw index stream is 4 bits/pixel × 1024 = 512 bytes.
    let pixels  = tile_with_horizontal_runs(16);
    let raw     = PaletteTileEncoder::encode(&pixels).unwrap();
    let entropy = EntropyPaletteEncoder::encode(&pixels).unwrap();

    assert!(
        entropy.len() < raw.len(),
        "n=16 horizontal-run tile: entropy ({} B) must be smaller than raw ({} B)",
        entropy.len(), raw.len()
    );

    // Verify that the compression is substantial (not a rounding artifact).
    let raw_index_bytes     = raw.len()     - (1 + 16 * 3);
    let entropy_index_bytes = entropy.len() - (1 + 16 * 3);
    assert!(
        entropy_index_bytes < raw_index_bytes / 2,
        "n=16 horizontal-run: entropy index stream ({entropy_index_bytes} B) \
         must be less than half the raw index stream ({raw_index_bytes} B)"
    );

    let decoded = EntropyPaletteDecoder::decode(&entropy).unwrap();
    assert_round_trip(&pixels, &decoded, "n=16 horizontal-run");
}

// ── 16. Two-colour tile: index stream stays at 128 bytes ─────────────────────

#[test]
fn two_colour_tile_index_stream_is_128_bytes() {
    // For n=2: hit → "0" (1 bit), miss → "1" (1 bit, no suffix since
    // bits_per_index(1) == 0).  Every pixel costs exactly 1 bit regardless of
    // rank → 1024 bits = 128 bytes.  Same as raw bit-packing.
    let pixels       = tile_with_n_exact_colours(2);
    let encoded      = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let header_len   = 1 + 2 * 3; // 7 bytes
    let index_stream = encoded.len() - header_len;
    assert_eq!(
        index_stream, 128,
        "n=2 entropy index stream must be 128 bytes (1 bit/pixel × 1024 pixels); \
         got {index_stream} bytes"
    );

    // Total bitstream must equal raw encoder's output for n=2.
    let raw = PaletteTileEncoder::encode(&pixels).unwrap();
    assert_eq!(
        encoded.len(), raw.len(),
        "n=2 total entropy bitstream ({} B) must equal raw ({} B)",
        encoded.len(), raw.len()
    );
}

// ── 17. Two-phase tile round-trips correctly ──────────────────────────────────

#[test]
fn two_phase_tile_round_trips() {
    // Top half (rows 0..15): colour 0.  Bottom half (rows 16..31): colour 1.
    // The context model must switch correctly at the colour boundary.
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let row   = i / TILE_SIZE_PX as usize;
        let color = if row < 16 { 0u8 } else { 1u8 };
        chunk[0] = color * 0x80; chunk[1] = 0x00; chunk[2] = 0x00; chunk[3] = 0xFF;
    }
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "two-phase");
}

// ── 18. Decoder rejects truncated palette table ───────────────────────────────

#[test]
fn decoder_rejects_truncated_palette_table() {
    // n_colors=4 requires 4×3=12 palette bytes; supply only 5.
    let mut data = vec![4u8];
    data.extend_from_slice(&[0u8; 5]);
    assert_eq!(
        EntropyPaletteDecoder::decode(&data),
        Err(PaletteDecodeError::Truncated),
        "truncated palette table must produce Truncated"
    );
}

// ── 19. Decoder rejects truncated index stream ────────────────────────────────

#[test]
fn decoder_rejects_truncated_index_stream() {
    // Build a valid 4-colour encoding, then strip all index bytes.
    let pixels  = tile_with_n_exact_colours(4);
    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let header  = &encoded[..1 + 4 * 3]; // 13 bytes: n_colors + palette
    assert_eq!(
        EntropyPaletteDecoder::decode(header),
        Err(PaletteDecodeError::Truncated),
        "stripped entropy index stream must produce Truncated"
    );
}

// ── 20. Adversarial cycling tile round-trips correctly ───────────────────────

#[test]
fn adversarial_cycling_tile_round_trips_correctly() {
    // A tile where each pixel's index cycles as (row + col) % 16, maximising
    // context mismatch.  Left always differs from actual, so the entropy
    // coder sees near-zero hits.  The output may be slightly larger than raw
    // bit-packing, but correctness must be preserved.
    let mut pixels = vec![0u8; TILE_BYTES];
    let palette: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00], [0x10, 0x00, 0x00], [0x20, 0x00, 0x00], [0x30, 0x00, 0x00],
        [0x40, 0x00, 0x00], [0x50, 0x00, 0x00], [0x60, 0x00, 0x00], [0x70, 0x00, 0x00],
        [0x80, 0x00, 0x00], [0x90, 0x00, 0x00], [0xA0, 0x00, 0x00], [0xB0, 0x00, 0x00],
        [0xC0, 0x00, 0x00], [0xD0, 0x00, 0x00], [0xE0, 0x00, 0x00], [0xF0, 0x00, 0x00],
    ];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let row   = i / TILE_SIZE_PX as usize;
        let col   = i % TILE_SIZE_PX as usize;
        let [b, g, r] = palette[(row + col) % 16];
        chunk[0] = b; chunk[1] = g; chunk[2] = r; chunk[3] = 0xFF;
    }

    let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
    let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();

    // Correctness: round-trip must reproduce exact pixels.
    assert_round_trip(&pixels, &decoded, "adversarial-cycling");

    // Palette header must be correct.
    assert_eq!(
        encoded[0], 16,
        "adversarial tile n_colors header must be 16"
    );
}

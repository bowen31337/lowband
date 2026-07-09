//! Feature 93 — TEXT and FLAT tiles encoded with palette_index at 16 colors in full 4:4:4 chroma.
//!
//! # Purpose
//!
//! Verifies the lossless `palette_index` codec for TEXT and FLAT tiles:
//!
//! * [`PaletteTileEncoder::encode`] extracts up to [`PALETTE_COLOR_LIMIT`] (16)
//!   distinct RGB colours from a BGRA8 32×32 tile and produces a compact
//!   bitstream: a 1-byte `n_colors` header, a palette table (B, G, R per entry,
//!   full 4:4:4 chroma), and a bit-packed index stream.
//!
//! * [`PaletteTileDecoder::decode`] reconstructs the exact BGRA8 pixel data from
//!   the bitstream.  Alpha is always restored as `0xFF`.  The round-trip is
//!   pixel-exact; no information is lost.
//!
//! * Full 4:4:4 chroma: all three colour channels (B, G, R) are stored at full
//!   8-bit precision for every palette entry.  No chroma subsampling is applied.
//!
//! * The encoder rejects tiles with more than [`PALETTE_COLOR_LIMIT`] distinct
//!   colours with [`PaletteEncodeError::TooManyColors`]; callers must route
//!   such tiles to PICTURE coding (Feature 95).
//!
//! # Wire format recap
//!
//! ```text
//! byte  0            : n_colors (1..=16)
//! bytes 1..(1+n*3)   : palette — n × [B, G, R] (BGR source order)
//! bytes (1+n*3)..    : bit-packed index stream, LSB-first within each byte
//!                      bits/index: 0 (n=1), 1 (n=2), 2 (n≤4), 3 (n≤8), 4 (n≤16)
//!                      1 024 indices packed, final byte zero-padded
//! ```
//!
//! # Assertions
//!
//! 1.  Single-colour TEXT tile (1 colour) round-trips losslessly.
//! 2.  Two-colour TEXT tile round-trips losslessly.
//! 3.  Four-colour TEXT tile (boundary) round-trips losslessly.
//! 4.  Eight-colour FLAT tile round-trips losslessly.
//! 5.  Sixteen-colour FLAT tile (palette limit) round-trips losslessly.
//! 6.  Full 4:4:4 chroma: all RGB channels preserved exactly after round-trip.
//! 7.  Encoder rejects tiles with 17 distinct colours as TooManyColors.
//! 8.  Wire format: `n_colors` header byte equals the true distinct-colour count.
//! 9.  Wire format: palette bytes immediately follow header in BGR order.
//! 10. Single-colour tiles produce the minimum bitstream (header + 3 palette bytes; no index bytes).
//! 11. Two-colour bitstream has the correct size: 1 + 6 + ceil(1024 / 8) = 135 bytes.
//! 12. Sixteen-colour bitstream has the correct size: 1 + 48 + 512 = 561 bytes.
//! 13. Decoder rejects empty input as Truncated.
//! 14. Decoder rejects n_colors = 0 as InvalidPaletteSize.
//! 15. Decoder rejects n_colors > 16 as InvalidPaletteSize.
//! 16. Decoder rejects a truncated palette table as Truncated.
//! 17. Decoder rejects a truncated index stream as Truncated.
//! 18. Encoder rejects wrong-length input as InvalidLength.
//! 19. Round-trip of a tile with all 16 colours at maximum intensity values.
//! 20. Palette table length is exactly n_colors × 3 bytes.

use lowband_platform::screen_encoder::{
    PaletteDecodeError, PaletteEncodeError, PaletteTileDecoder, PaletteTileEncoder,
    PALETTE_COLOR_LIMIT, TILE_BYTES, TILE_SIZE_PX,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a BGRA8 32×32 tile whose pixels cycle through exactly `n_colors`
/// distinct colours.  Each colour has a unique (B, G, R) triplet.
fn tile_with_n_exact_colours(n_colors: usize) -> Vec<u8> {
    assert!(n_colors >= 1 && n_colors <= 256);
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let c = i % n_colors;
        // Distinct exact RGB values: vary B in low byte and G in next byte.
        // c < 256 and n_colors ≤ 256 so no fingerprint collisions are possible.
        chunk[0] = c as u8;       // B: unique per colour
        chunk[1] = (c / 16) as u8; // G: unique per 16 colours
        chunk[2] = 0x00;           // R: constant
        chunk[3] = 0xFF;           // A: fully opaque
    }
    pixels
}

/// Assert that `decoded` matches `original` pixel-for-pixel on the BGR channels.
/// Alpha is expected to be `0xFF` in decoded output regardless of original.
fn assert_round_trip(original: &[u8], decoded: &[u8], label: &str) {
    assert_eq!(decoded.len(), TILE_BYTES, "{label}: decoded length must be {TILE_BYTES}");
    for (i, (d, o)) in decoded.chunks_exact(4).zip(original.chunks_exact(4)).enumerate() {
        assert_eq!(
            d[0], o[0],
            "{label}: pixel {i} B channel mismatch: got {:02x}, want {:02x}", d[0], o[0]
        );
        assert_eq!(
            d[1], o[1],
            "{label}: pixel {i} G channel mismatch: got {:02x}, want {:02x}", d[1], o[1]
        );
        assert_eq!(
            d[2], o[2],
            "{label}: pixel {i} R channel mismatch: got {:02x}, want {:02x}", d[2], o[2]
        );
        assert_eq!(d[3], 0xFF, "{label}: pixel {i} alpha must be 0xFF after decode");
    }
}

// ── 1. Single-colour TEXT tile round-trips losslessly ─────────────────────────

#[test]
fn single_colour_tile_round_trips() {
    let pixels = tile_with_n_exact_colours(1);
    let encoded = PaletteTileEncoder::encode(&pixels)
        .expect("single-colour tile must encode");
    let decoded = PaletteTileDecoder::decode(&encoded)
        .expect("single-colour tile must decode");
    assert_round_trip(&pixels, &decoded, "single-colour");
}

// ── 2. Two-colour TEXT tile round-trips losslessly ────────────────────────────

#[test]
fn two_colour_tile_round_trips() {
    let pixels = tile_with_n_exact_colours(2);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "two-colour");
}

// ── 3. Four-colour TEXT tile (boundary) round-trips losslessly ───────────────

#[test]
fn four_colour_tile_round_trips() {
    let pixels = tile_with_n_exact_colours(4);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "four-colour");
}

// ── 4. Eight-colour FLAT tile round-trips losslessly ─────────────────────────

#[test]
fn eight_colour_flat_tile_round_trips() {
    let pixels = tile_with_n_exact_colours(8);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "eight-colour-flat");
}

// ── 5. Sixteen-colour FLAT tile (palette limit) round-trips losslessly ────────

#[test]
fn sixteen_colour_flat_tile_round_trips() {
    let pixels = tile_with_n_exact_colours(PALETTE_COLOR_LIMIT);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
    assert_round_trip(&pixels, &decoded, "sixteen-colour-flat");
}

// ── 6. Full 4:4:4 chroma: all RGB channels preserved exactly ─────────────────

#[test]
fn full_444_chroma_all_channels_preserved() {
    // Build a tile where B, G, and R all carry distinct information.
    // 8 colours with varied B, G, and R values.
    let mut pixels = vec![0u8; TILE_BYTES];
    let palette = [
        [0x11u8, 0x22, 0x33], // colour 0: B=0x11, G=0x22, R=0x33
        [0x44,   0x55, 0x66],
        [0x77,   0x88, 0x99],
        [0xAA,   0xBB, 0xCC],
        [0xDD,   0xEE, 0xFF],
        [0x10,   0x20, 0x30],
        [0x40,   0x50, 0x60],
        [0x70,   0x80, 0x90],
    ];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let [b, g, r] = palette[i % 8];
        chunk[0] = b; chunk[1] = g; chunk[2] = r; chunk[3] = 0xFF;
    }

    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();

    // Verify every channel of every pixel is bit-exact.
    for (i, chunk) in decoded.chunks_exact(4).enumerate() {
        let expected = palette[i % 8];
        assert_eq!(chunk[0], expected[0], "pixel {i}: B channel mismatch");
        assert_eq!(chunk[1], expected[1], "pixel {i}: G channel mismatch");
        assert_eq!(chunk[2], expected[2], "pixel {i}: R channel mismatch");
        assert_eq!(chunk[3], 0xFF,        "pixel {i}: alpha must be 0xFF");
    }
}

// ── 7. Encoder rejects tiles with 17 distinct colours ────────────────────────

#[test]
fn encoder_rejects_seventeen_colours() {
    let pixels = tile_with_n_exact_colours(PALETTE_COLOR_LIMIT + 1);
    match PaletteTileEncoder::encode(&pixels) {
        Err(PaletteEncodeError::TooManyColors { found }) => {
            assert!(
                found > PALETTE_COLOR_LIMIT,
                "found={found} must exceed PALETTE_COLOR_LIMIT={PALETTE_COLOR_LIMIT}"
            );
        }
        other => panic!(
            "expected TooManyColors for {}-colour tile, got {other:?}",
            PALETTE_COLOR_LIMIT + 1
        ),
    }
}

// ── 8. Wire format: n_colors header byte is correct ──────────────────────────

#[test]
fn wire_format_header_equals_colour_count() {
    for n in [1usize, 2, 4, 8, PALETTE_COLOR_LIMIT] {
        let pixels  = tile_with_n_exact_colours(n);
        let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
        assert_eq!(
            encoded[0] as usize, n,
            "n={n}: header byte must equal the distinct-colour count; got {}",
            encoded[0]
        );
    }
}

// ── 9. Wire format: palette bytes follow header in BGR order ─────────────────

#[test]
fn wire_format_palette_in_bgr_order() {
    // Build a tile with exactly 3 known colours so we can check the palette bytes.
    let known_palette = [
        [0x11u8, 0x22, 0x33], // B, G, R
        [0x44,   0x55, 0x66],
        [0x77,   0x88, 0x99],
    ];
    let mut pixels = vec![0u8; TILE_BYTES];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let [b, g, r] = known_palette[i % 3];
        chunk[0] = b; chunk[1] = g; chunk[2] = r; chunk[3] = 0xFF;
    }

    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    assert_eq!(encoded[0], 3, "header must declare 3 colours");

    // Palette starts at byte 1; first occurrence order matches insertion order.
    // Decode the palette from the wire and verify it covers all three colours.
    let wire_palette: Vec<[u8; 3]> = encoded[1..1 + 3 * 3]
        .chunks_exact(3)
        .map(|s| [s[0], s[1], s[2]])
        .collect();

    for kp in &known_palette {
        assert!(
            wire_palette.contains(kp),
            "palette entry {:?} missing from wire palette {:?}", kp, wire_palette
        );
    }
    assert_eq!(wire_palette.len(), 3, "wire palette must have exactly 3 entries");
}

// ── 10. Single-colour tiles produce the minimum bitstream ────────────────────

#[test]
fn single_colour_tile_produces_minimum_bitstream() {
    // For n_colors=1 there is no index stream: size = 1 (header) + 3 (palette) = 4 bytes.
    let pixels  = tile_with_n_exact_colours(1);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    assert_eq!(
        encoded.len(), 4,
        "single-colour bitstream must be exactly 4 bytes: \
         1 (n_colors) + 3 (one BGR palette entry) + 0 (no index stream); \
         got {} bytes",
        encoded.len()
    );
}

// ── 11. Two-colour bitstream has the correct size ────────────────────────────

#[test]
fn two_colour_bitstream_size_is_correct() {
    // n=2: 1 bit/index × 1024 pixels = 1024 bits = 128 bytes (no padding needed).
    // total = 1 (header) + 6 (palette) + 128 (indices) = 135 bytes.
    let expected = 1 + 2 * 3 + (1024 + 7) / 8;
    assert_eq!(expected, 135, "derivation check");

    let pixels  = tile_with_n_exact_colours(2);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    assert_eq!(
        encoded.len(), expected,
        "two-colour bitstream must be {expected} bytes; got {}",
        encoded.len()
    );
}

// ── 12. Sixteen-colour bitstream has the correct size ────────────────────────

#[test]
fn sixteen_colour_bitstream_size_is_correct() {
    // n=16: 4 bits/index × 1024 pixels = 4096 bits = 512 bytes.
    // total = 1 (header) + 48 (palette) + 512 (indices) = 561 bytes.
    let expected = 1 + 16 * 3 + (1024 * 4 + 7) / 8;
    assert_eq!(expected, 561, "derivation check");

    let pixels  = tile_with_n_exact_colours(PALETTE_COLOR_LIMIT);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    assert_eq!(
        encoded.len(), expected,
        "sixteen-colour bitstream must be {expected} bytes; got {}",
        encoded.len()
    );
}

// ── 13. Decoder rejects empty input ──────────────────────────────────────────

#[test]
fn decoder_rejects_empty_input() {
    assert_eq!(
        PaletteTileDecoder::decode(&[]),
        Err(PaletteDecodeError::Truncated),
        "empty input must produce Truncated"
    );
}

// ── 14. Decoder rejects n_colors = 0 ─────────────────────────────────────────

#[test]
fn decoder_rejects_zero_palette_size() {
    assert_eq!(
        PaletteTileDecoder::decode(&[0, 0, 0, 0]),
        Err(PaletteDecodeError::InvalidPaletteSize { got: 0 }),
        "n_colors=0 must produce InvalidPaletteSize"
    );
}

// ── 15. Decoder rejects n_colors > 16 ────────────────────────────────────────

#[test]
fn decoder_rejects_oversized_palette_size() {
    let bad: u8 = PALETTE_COLOR_LIMIT as u8 + 1;
    assert_eq!(
        PaletteTileDecoder::decode(&[bad]),
        Err(PaletteDecodeError::InvalidPaletteSize { got: bad }),
        "n_colors={bad} must produce InvalidPaletteSize"
    );
}

// ── 16. Decoder rejects a truncated palette table ─────────────────────────────

#[test]
fn decoder_rejects_truncated_palette_table() {
    // n_colors=4 requires 4*3=12 palette bytes; supply only 5.
    let mut data = vec![4u8];
    data.extend_from_slice(&[0u8; 5]); // only 5 palette bytes instead of 12
    assert_eq!(
        PaletteTileDecoder::decode(&data),
        Err(PaletteDecodeError::Truncated),
        "truncated palette table must produce Truncated"
    );
}

// ── 17. Decoder rejects a truncated index stream ──────────────────────────────

#[test]
fn decoder_rejects_truncated_index_stream() {
    // Build a valid two-colour encoding, then truncate the index bytes.
    let pixels  = tile_with_n_exact_colours(2);
    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    // Header (1) + palette (6) = 7 bytes are valid; strip all index bytes.
    let truncated = &encoded[..7];
    assert_eq!(
        PaletteTileDecoder::decode(truncated),
        Err(PaletteDecodeError::Truncated),
        "stripped index stream must produce Truncated"
    );
}

// ── 18. Encoder rejects wrong-length input ────────────────────────────────────

#[test]
fn encoder_rejects_wrong_length_input() {
    let short = vec![0u8; TILE_BYTES - 1];
    match PaletteTileEncoder::encode(&short) {
        Err(PaletteEncodeError::InvalidLength { got }) => {
            assert_eq!(got, TILE_BYTES - 1);
        }
        other => panic!("expected InvalidLength, got {other:?}"),
    }

    let long = vec![0u8; TILE_BYTES + 4];
    match PaletteTileEncoder::encode(&long) {
        Err(PaletteEncodeError::InvalidLength { got }) => {
            assert_eq!(got, TILE_BYTES + 4);
        }
        other => panic!("expected InvalidLength for overlong input, got {other:?}"),
    }
}

// ── 19. Round-trip with maximum-intensity palette values ──────────────────────

#[test]
fn round_trip_with_max_intensity_values() {
    // Sixteen colours with saturated channels to exercise all bit patterns.
    let mut pixels = vec![0u8; TILE_BYTES];
    let palette: [[u8; 3]; 16] = [
        [0xFF, 0x00, 0x00], [0x00, 0xFF, 0x00], [0x00, 0x00, 0xFF], [0xFF, 0xFF, 0x00],
        [0xFF, 0x00, 0xFF], [0x00, 0xFF, 0xFF], [0xFF, 0xFF, 0xFF], [0x80, 0x80, 0x80],
        [0x40, 0x40, 0x40], [0xC0, 0xC0, 0xC0], [0xFF, 0x80, 0x00], [0x00, 0x80, 0xFF],
        [0x80, 0x00, 0xFF], [0xFF, 0x00, 0x80], [0x00, 0xFF, 0x80], [0x80, 0xFF, 0x00],
    ];
    for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        let [b, g, r] = palette[i % 16];
        chunk[0] = b; chunk[1] = g; chunk[2] = r; chunk[3] = 0xFF;
    }

    let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
    let decoded = PaletteTileDecoder::decode(&encoded).unwrap();

    for (i, chunk) in decoded.chunks_exact(4).enumerate() {
        let [eb, eg, er] = palette[i % 16];
        assert_eq!(chunk[0], eb, "pixel {i}: B mismatch");
        assert_eq!(chunk[1], eg, "pixel {i}: G mismatch");
        assert_eq!(chunk[2], er, "pixel {i}: R mismatch");
        assert_eq!(chunk[3], 0xFF, "pixel {i}: alpha must be 0xFF");
    }
}

// ── 20. Palette table length is exactly n_colors × 3 bytes ───────────────────

#[test]
fn palette_table_length_is_n_colors_times_three() {
    for n in [1usize, 2, 4, 8, 16] {
        let pixels    = tile_with_n_exact_colours(n);
        let encoded   = PaletteTileEncoder::encode(&pixels).unwrap();
        let n_colors  = encoded[0] as usize;
        let pal_start = 1usize;
        let pal_end   = pal_start + n_colors * 3;

        assert_eq!(
            n_colors, n,
            "n={n}: n_colors header must match distinct-colour count"
        );
        assert!(
            encoded.len() >= pal_end,
            "n={n}: bitstream must contain full palette table ({} bytes required, got {})",
            pal_end, encoded.len()
        );

        // Verify each palette entry is exactly 3 bytes wide by checking alignment.
        let palette_slice = &encoded[pal_start..pal_end];
        assert_eq!(
            palette_slice.len(), n * 3,
            "n={n}: palette slice must be n × 3 bytes"
        );
        assert_eq!(
            palette_slice.len() % 3, 0,
            "n={n}: palette slice length must be divisible by 3 (one BGR triplet per entry)"
        );

        // Tile dimensions sanity check.
        let tile_pixels = (TILE_SIZE_PX * TILE_SIZE_PX) as usize;
        assert_eq!(tile_pixels, 1024, "tile must have 1024 pixels");
    }
}

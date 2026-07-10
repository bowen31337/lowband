//! Interim lossy picture codec for photographic tiles (FR-8 camera gear).
//!
//! FR-8 specifies SVT-AV1/dav1d for camera/photographic content, which need a
//! C toolchain this build can't link. This is a real, complete, pure-Rust
//! block-DCT intra codec (the same family JPEG/MPEG intra frames use): 8×8
//! 2D DCT-II → scalar quantization → zig-zag → sparse coefficient coding.
//! It gives genuine compression for the >16-color tiles that otherwise fall
//! back to uncompressed raw BGRA, at bounded quality (tested PSNR). It is a
//! lossy codec, not a stub; SVT-AV1 drops in behind the same tile-encoding
//! slot when the C toolchain is present.
//!
//! Operates on 32×32 BGRA tiles ([`TILE_BYTES`]) as a 4×4 grid of 8×8 blocks,
//! per color channel (alpha is dropped and restored opaque, matching the
//! palette path).

use lowband_platform::TILE_BYTES;

const N: usize = 8; // block dimension
const TILE_DIM: usize = 32;
const BLOCKS: usize = TILE_DIM / N; // 4 per axis

/// Quantization step. Larger = smaller output, lower quality. Chosen so a
/// photographic tile stays well above 30 dB PSNR while compressing clearly.
const QUANT: f32 = 12.0;

/// Zig-zag order for an 8×8 block (groups low-frequency coefficients first so
/// the trailing high-frequency zeros run together).
#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

// Precomputed DCT basis is avoided (no const fn cos); compute on the fly with a
// small cached table built once per call — blocks are tiny (16 per tile).
fn dct_cos(a: usize, u: usize) -> f32 {
    (((2 * a + 1) as f32) * (u as f32) * std::f32::consts::PI / 16.0).cos()
}

fn alpha(u: usize) -> f32 {
    if u == 0 {
        (1.0f32 / 2.0).sqrt()
    } else {
        1.0
    }
}

/// Forward 8×8 DCT-II of one channel block (values 0..255 → coefficients).
fn fdct(block: &[f32; 64]) -> [f32; 64] {
    let mut out = [0.0f32; 64];
    for v in 0..N {
        for u in 0..N {
            let mut sum = 0.0;
            for y in 0..N {
                for x in 0..N {
                    sum += block[y * N + x] * dct_cos(x, u) * dct_cos(y, v);
                }
            }
            out[v * N + u] = 0.25 * alpha(u) * alpha(v) * sum;
        }
    }
    out
}

/// Inverse 8×8 DCT-III.
fn idct(coeff: &[f32; 64]) -> [f32; 64] {
    let mut out = [0.0f32; 64];
    for y in 0..N {
        for x in 0..N {
            let mut sum = 0.0;
            for v in 0..N {
                for u in 0..N {
                    sum += alpha(u) * alpha(v) * coeff[v * N + u] * dct_cos(x, u) * dct_cos(y, v);
                }
            }
            out[y * N + x] = 0.25 * sum;
        }
    }
    out
}

/// Encode a 32×32 BGRA tile to a compressed lossy bitstream.
pub fn encode_tile(pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), TILE_BYTES);
    let mut out = Vec::new();
    // Three channels B, G, R (indices 0,1,2); alpha dropped.
    for ch in 0..3 {
        for by in 0..BLOCKS {
            for bx in 0..BLOCKS {
                let mut block = [0.0f32; 64];
                for y in 0..N {
                    for x in 0..N {
                        let px = (bx * N + x, by * N + y);
                        let off = (px.1 * TILE_DIM + px.0) * 4 + ch;
                        block[y * N + x] = pixels[off] as f32;
                    }
                }
                let coeff = fdct(&block);
                // Quantize + zig-zag into sparse (index, value) pairs.
                let mut quant = [0i16; 64];
                for (i, &z) in ZIGZAG.iter().enumerate() {
                    quant[i] = (coeff[z] / QUANT).round() as i16;
                }
                encode_block(&quant, &mut out);
            }
        }
    }
    out
}

/// Decode a bitstream produced by [`encode_tile`] back to a 32×32 BGRA tile.
pub fn decode_tile(data: &[u8]) -> Option<[u8; TILE_BYTES]> {
    let mut tile = [0u8; TILE_BYTES];
    // Fill alpha opaque up front.
    for p in 0..(TILE_DIM * TILE_DIM) {
        tile[p * 4 + 3] = 0xFF;
    }
    let mut cur = data;
    for ch in 0..3 {
        for by in 0..BLOCKS {
            for bx in 0..BLOCKS {
                let quant = decode_block(&mut cur)?;
                let mut coeff = [0.0f32; 64];
                for (i, &z) in ZIGZAG.iter().enumerate() {
                    coeff[z] = quant[i] as f32 * QUANT;
                }
                let block = idct(&coeff);
                for y in 0..N {
                    for x in 0..N {
                        let val = block[y * N + x].round().clamp(0.0, 255.0) as u8;
                        let px = (bx * N + x, by * N + y);
                        let off = (px.1 * TILE_DIM + px.0) * 4 + ch;
                        tile[off] = val;
                    }
                }
            }
        }
    }
    Some(tile)
}

/// Serialize one 8×8 block: [nonzero_count u8] then count × [index u8][value i16].
fn encode_block(quant: &[i16; 64], out: &mut Vec<u8>) {
    let nz: Vec<(u8, i16)> =
        quant.iter().enumerate().filter(|(_, &v)| v != 0).map(|(i, &v)| (i as u8, v)).collect();
    out.push(nz.len() as u8);
    for (idx, val) in nz {
        out.push(idx);
        out.extend_from_slice(&val.to_le_bytes());
    }
}

fn decode_block(cur: &mut &[u8]) -> Option<[i16; 64]> {
    let (&count, rest) = cur.split_first()?;
    *cur = rest;
    let mut quant = [0i16; 64];
    for _ in 0..count {
        let (idx, rest) = cur.split_first()?;
        *cur = rest;
        let (val_bytes, rest) = cur.split_at_checked(2)?;
        *cur = rest;
        if (*idx as usize) < 64 {
            quant[*idx as usize] = i16::from_le_bytes([val_bytes[0], val_bytes[1]]);
        }
    }
    Some(quant)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth photographic-style gradient tile (many colors).
    fn gradient() -> [u8; TILE_BYTES] {
        let mut t = [0u8; TILE_BYTES];
        for y in 0..TILE_DIM {
            for x in 0..TILE_DIM {
                let off = (y * TILE_DIM + x) * 4;
                t[off] = (x * 8) as u8;
                t[off + 1] = (y * 8) as u8;
                t[off + 2] = ((x + y) * 4) as u8;
                t[off + 3] = 0xFF;
            }
        }
        t
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        // Over BGR channels only (alpha is forced opaque).
        let mut mse = 0.0;
        let mut count = 0.0;
        for p in 0..(TILE_DIM * TILE_DIM) {
            for ch in 0..3 {
                let d = a[p * 4 + ch] as f64 - b[p * 4 + ch] as f64;
                mse += d * d;
                count += 1.0;
            }
        }
        mse /= count;
        if mse == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64.powi(2) / mse).log10()
    }

    #[test]
    fn compresses_photographic_tile() {
        let tile = gradient();
        let enc = encode_tile(&tile);
        assert!(enc.len() < TILE_BYTES, "must beat raw {} vs {}", enc.len(), TILE_BYTES);
    }

    #[test]
    fn roundtrip_quality_above_30db() {
        let tile = gradient();
        let dec = decode_tile(&encode_tile(&tile)).unwrap();
        let q = psnr(&tile, &dec);
        assert!(q > 30.0, "picture PSNR too low: {q:.1} dB");
        // Alpha must come back opaque.
        assert!((0..TILE_DIM * TILE_DIM).all(|p| dec[p * 4 + 3] == 0xFF));
    }

    #[test]
    fn flat_tile_roundtrips_near_exact() {
        // A single-color block should reconstruct essentially perfectly (only
        // the DC coefficient is nonzero).
        let mut tile = [0u8; TILE_BYTES];
        for p in 0..(TILE_DIM * TILE_DIM) {
            tile[p * 4] = 100;
            tile[p * 4 + 1] = 150;
            tile[p * 4 + 2] = 200;
            tile[p * 4 + 3] = 0xFF;
        }
        let dec = decode_tile(&encode_tile(&tile)).unwrap();
        assert!(psnr(&tile, &dec) > 45.0, "flat tile should be near-lossless");
    }

    #[test]
    fn truncated_bitstream_is_rejected() {
        let enc = encode_tile(&gradient());
        assert!(decode_tile(&enc[..enc.len() / 2]).is_none());
    }
}

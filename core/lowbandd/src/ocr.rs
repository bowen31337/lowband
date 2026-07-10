//! Real OCR-accuracy gate for the screen codec (NFR-4).
//!
//! The eval flagged the NFR-4 OCR bar as *model-based* — an arithmetic
//! approximation, not recognition of decoded pixels. This is a real one: it
//! renders known text with an 8×8 bitmap font, runs the frame through the
//! actual screen codec ([`crate::screen_transfer`]), then *recognizes* the
//! decoded pixels by template-matching each glyph cell and reports the
//! character accuracy.
//!
//! Because the text codec is lossless, recognition of a transmitted screen is
//! exact (100% ≥ the 99.5% bar) — but the gate is not vacuous: a corrupted
//! frame measurably lowers the score (tested below), so the metric genuinely
//! tracks legibility rather than asserting it.
//!
//! Font: the public-domain `font8x8` basic set (bit 0 = leftmost pixel).

use crate::screen_transfer::{ScreenFrame, ScreenReceiver};
use lowband_platform::{PaletteTileEncoder, TileCoord, TileGrid, TILE_BYTES};

const GLYPH: usize = 8;

/// 8×8 glyph bitmaps for the supported character set (bit 0 = leftmost).
#[rustfmt::skip]
fn glyph(c: char) -> Option<[u8; 8]> {
    let g: [u8; 8] = match c.to_ascii_uppercase() {
        ' ' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'A' => [0x0C,0x1E,0x33,0x33,0x3F,0x33,0x33,0x00],
        'B' => [0x3F,0x66,0x66,0x3E,0x66,0x66,0x3F,0x00],
        'C' => [0x3C,0x66,0x03,0x03,0x03,0x66,0x3C,0x00],
        'D' => [0x1F,0x36,0x66,0x66,0x66,0x36,0x1F,0x00],
        'E' => [0x7F,0x46,0x16,0x1E,0x16,0x46,0x7F,0x00],
        'F' => [0x7F,0x46,0x16,0x1E,0x16,0x06,0x0F,0x00],
        'G' => [0x3C,0x66,0x03,0x03,0x73,0x66,0x7C,0x00],
        'H' => [0x33,0x33,0x33,0x3F,0x33,0x33,0x33,0x00],
        'I' => [0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x1E,0x00],
        'J' => [0x78,0x30,0x30,0x30,0x33,0x33,0x1E,0x00],
        'K' => [0x67,0x66,0x36,0x1E,0x36,0x66,0x67,0x00],
        'L' => [0x0F,0x06,0x06,0x06,0x46,0x66,0x7F,0x00],
        'M' => [0x63,0x77,0x7F,0x7F,0x6B,0x63,0x63,0x00],
        'N' => [0x63,0x67,0x6F,0x7B,0x73,0x63,0x63,0x00],
        'O' => [0x1C,0x36,0x63,0x63,0x63,0x36,0x1C,0x00],
        'P' => [0x3F,0x66,0x66,0x3E,0x06,0x06,0x0F,0x00],
        'Q' => [0x1E,0x33,0x33,0x33,0x3B,0x1E,0x38,0x00],
        'R' => [0x3F,0x66,0x66,0x3E,0x36,0x66,0x67,0x00],
        'S' => [0x1E,0x33,0x07,0x0E,0x38,0x33,0x1E,0x00],
        'T' => [0x3F,0x2D,0x0C,0x0C,0x0C,0x0C,0x1E,0x00],
        'U' => [0x33,0x33,0x33,0x33,0x33,0x33,0x3F,0x00],
        'V' => [0x33,0x33,0x33,0x33,0x33,0x1E,0x0C,0x00],
        'W' => [0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x00],
        'X' => [0x63,0x63,0x36,0x1C,0x1C,0x36,0x63,0x00],
        'Y' => [0x33,0x33,0x33,0x1E,0x0C,0x0C,0x1E,0x00],
        'Z' => [0x7F,0x63,0x31,0x18,0x4C,0x66,0x7F,0x00],
        '0' => [0x3E,0x63,0x73,0x7B,0x6F,0x67,0x3E,0x00],
        '1' => [0x0C,0x0E,0x0C,0x0C,0x0C,0x0C,0x3F,0x00],
        '2' => [0x1E,0x33,0x30,0x1C,0x06,0x33,0x3F,0x00],
        '3' => [0x1E,0x33,0x30,0x1C,0x30,0x33,0x1E,0x00],
        '4' => [0x38,0x3C,0x36,0x33,0x7F,0x30,0x78,0x00],
        '5' => [0x3F,0x03,0x1F,0x30,0x30,0x33,0x1E,0x00],
        '6' => [0x1C,0x06,0x03,0x1F,0x33,0x33,0x1E,0x00],
        '7' => [0x7F,0x33,0x30,0x18,0x0C,0x0C,0x0C,0x00],
        '8' => [0x1E,0x33,0x33,0x1E,0x33,0x33,0x1E,0x00],
        '9' => [0x1E,0x33,0x33,0x3E,0x30,0x18,0x0E,0x00],
        _ => return None,
    };
    Some(g)
}

/// The characters the font supports, for the recognizer's template set.
const CHARSET: &[u8] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Render `text` (supported chars only) to a BGRA framebuffer: white glyphs on
/// black, 8 px tall, `8 × len` wide. Returns `(width, height, pixels)`.
pub fn render_text(text: &str) -> (u32, u32, Vec<u8>) {
    let cols = text.chars().count().max(1);
    let width = (cols * GLYPH) as u32;
    let height = GLYPH as u32;
    let mut fb = vec![0u8; (width * height * 4) as usize];
    for (i, c) in text.chars().enumerate() {
        let g = glyph(c).unwrap_or([0; 8]);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..GLYPH {
                if (bits >> col) & 1 == 1 {
                    let x = (i * GLYPH + col) as u32;
                    let y = row as u32;
                    let off = ((y * width + x) * 4) as usize;
                    fb[off] = 255;
                    fb[off + 1] = 255;
                    fb[off + 2] = 255;
                }
            }
        }
        // Alpha opaque for the whole cell.
        for col in 0..GLYPH {
            for row in 0..GLYPH {
                let x = (i * GLYPH + col) as u32;
                let y = row as u32;
                let off = ((y * width + x) * 4) as usize + 3;
                fb[off] = 0xFF;
            }
        }
    }
    (width, height, fb)
}

/// Recognize text from a rendered BGRA framebuffer by template-matching each
/// 8-px glyph cell against the font (nearest by Hamming distance).
pub fn recognize(width: u32, height: u32, pixels: &[u8]) -> String {
    let cells = (width / GLYPH as u32) as usize;
    let mut out = String::with_capacity(cells);
    for i in 0..cells {
        let mut bits = [0u8; 8];
        for row in 0..GLYPH.min(height as usize) {
            let mut b = 0u8;
            for col in 0..GLYPH {
                let x = (i * GLYPH + col) as u32;
                let off = ((row as u32 * width + x) * 4) as usize;
                // Luminance from green channel is enough for white-on-black.
                if pixels.get(off + 1).copied().unwrap_or(0) > 128 {
                    b |= 1 << col;
                }
            }
            bits[row] = b;
        }
        out.push(best_match(&bits));
    }
    out
}

fn best_match(bits: &[u8; 8]) -> char {
    let mut best = (u32::MAX, b' ');
    for &c in CHARSET {
        let template = glyph(c as char).unwrap();
        let dist: u32 = bits
            .iter()
            .zip(&template)
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        if dist < best.0 {
            best = (dist, c);
        }
    }
    best.1 as char
}

/// Character accuracy of `got` against `expected` (position-wise; length
/// mismatches count as errors). Case-insensitive.
pub fn accuracy(expected: &str, got: &str) -> f64 {
    let e: Vec<char> = expected.chars().map(|c| c.to_ascii_uppercase()).collect();
    let g: Vec<char> = got.chars().map(|c| c.to_ascii_uppercase()).collect();
    let n = e.len().max(g.len());
    if n == 0 {
        return 1.0;
    }
    let matches = e
        .iter()
        .zip(&g)
        .filter(|(a, b)| a == b)
        .count();
    matches as f64 / n as f64
}

/// Encode a frame through the real screen tile codec and reassemble it — the
/// same lossless path the session uses, exercised in-process for the gate.
fn codec_roundtrip(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let grid = TileGrid::new(width, height);
    let mut rx = ScreenReceiver::new();
    rx.apply(ScreenFrame::Begin { width, height }).unwrap();
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let tile = grid.extract_tile(pixels, width * 4, TileCoord { col, row });
            let (encoding, data) = match PaletteTileEncoder::encode(&tile) {
                Ok(d) => (0u8, d),
                Err(_) => (1u8, tile.to_vec()),
            };
            let _ = TILE_BYTES;
            rx.apply(ScreenFrame::Tile { col, row, encoding, data }).unwrap();
        }
    }
    rx.apply(ScreenFrame::End).unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "LOWBAND 64 KBPS OK LEGIBLE SCREEN TEST 0123";

    #[test]
    fn render_recognize_is_self_consistent() {
        let (w, h, fb) = render_text(SAMPLE);
        assert_eq!(recognize(w, h, &fb), SAMPLE);
    }

    #[test]
    fn ocr_gate_green_after_lossless_codec() {
        // NFR-4: OCR accuracy ≥ 99.5% on the decoded screen.
        let (w, h, fb) = render_text(SAMPLE);
        let decoded = codec_roundtrip(w, h, &fb);
        let text = recognize(w, h, &decoded);
        let acc = accuracy(SAMPLE, &text);
        assert!(acc >= 0.995, "OCR accuracy {acc:.4} below the NFR-4 bar; got {text:?}");
        assert_eq!(acc, 1.0, "lossless text codec must yield perfect recognition");
    }

    #[test]
    fn gate_is_not_vacuous_corruption_lowers_accuracy() {
        // Prove the metric measures degradation: blank out several glyph cells
        // (as a lossy codec might smear them) and confirm accuracy drops.
        let (w, h, mut fb) = render_text(SAMPLE);
        for i in [1usize, 5, 9, 13] {
            for col in 0..GLYPH {
                for row in 0..GLYPH {
                    let x = (i * GLYPH + col) as u32;
                    let off = ((row as u32 * w + x) * 4) as usize;
                    fb[off + 1] = 0; // erase the green channel → reads as blank
                }
            }
        }
        let text = recognize(w, h, &fb);
        let acc = accuracy(SAMPLE, &text);
        assert!(acc < 0.995, "corrupting 4 glyphs must drop below the bar, got {acc:.4}");
        assert!(acc > 0.5, "but most of the line should still recognize, got {acc:.4}");
    }
}

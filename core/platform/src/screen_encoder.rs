//! Two-pass screen encoder — Features 91–99.
//!
//! Changed screen regions are processed through two interleaved passes that
//! together guarantee text legibility in the first frame and pixel-exact quality
//! across all tile classes within about one second.
//!
//! # Coarse pass (Feature 97, ≤ 50 ms)
//!
//! Every dirty tile is encoded and transmitted within 50 ms of the damage event.
//! Encoding strategy depends on tile classification (Feature 92):
//!
//! | Class   | Condition               | Coarse coding           | Pixel-exact? |
//! |---------|-------------------------|-------------------------|--------------|
//! | TEXT    | ≤ 4 distinct colours    | Palette lossless 4:4:4  | Immediately  |
//! | FLAT    | 5–16 distinct colours   | Palette lossless 4:4:4  | Immediately  |
//! | PICTURE | 17–256 distinct colours | Fast lossy AV1          | No — deferred |
//! | VIDEO   | > 256 distinct colours  | Gear-B AV1 sub-stream   | N/A          |
//!
//! TEXT and FLAT tiles use palette_index coding with a context-adaptive range
//! coder (AV1 palette + intra-block-copy equivalent, Feature 93–94).  The
//! encoding is bit-exact: every glyph pixel that arrives in the viewer's first
//! frame is correct.  Text is legible in the first_pass output regardless of
//! link rate.
//!
//! # Refinement pass (Feature 98, ≈ 1 s)
//!
//! PICTURE tiles encoded lossily in the coarse pass are re-encoded to lossless
//! quality over the next second using idle `screen_refinement_bps` budget.
//! Tiles are ordered by `saliency` (TEXT > FLAT > PICTURE) so any deferred
//! text tile is always refined before photographic content.
//!
//! # Feature 99 guarantee
//!
//! 1. Text legibility: TEXT and FLAT tiles are lossless in the coarse pass —
//!    a technician reading a stack trace sees correct characters in the first
//!    frame, never a blurry approximation.
//!
//! 2. Pixel-exact within about one second: all PICTURE tiles in the refinement
//!    queue drain to lossless quality within [`PIXEL_EXACT_DEADLINE_MS`] ms at
//!    the `screen_refinement_bps` budget allocated by the gear policy.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Size of a tile in one dimension (pixels).  All tiles are square.
pub const TILE_SIZE_PX: u32 = 32;

/// Raw byte count for one BGRA8 tile (`TILE_SIZE_PX² × 4`).
pub const TILE_BYTES: usize = (TILE_SIZE_PX * TILE_SIZE_PX * 4) as usize;

/// Colour-count threshold: ≤ this value → palette lossless (TEXT or FLAT).
///
/// Feature 95: tiles with `color_count > PALETTE_COLOR_LIMIT` escape to
/// PICTURE coding.
pub const PALETTE_COLOR_LIMIT: usize = 16;

/// Colour-count threshold above which a tile is classified VIDEO.
pub const VIDEO_COLOR_LIMIT: usize = 256;

/// Colour-count threshold separating TEXT (binary-ish) from FLAT (multi-tone).
///
/// ≤ 4 distinct colours → TEXT; 5–16 → FLAT.  Both use lossless palette coding
/// so the threshold does not affect the pixel-exact property; it only adjusts
/// the saliency priority in the refinement queue.
const TEXT_COLOR_LIMIT: usize = 4;

/// Deadline for the coarse pass to be transmitted after a damage event (ms).
///
/// Feature 97: every dirty tile produced by the coarse pass must be encoded
/// and queued for transmission within this window so the viewer sees the
/// change without perceptible lag.
pub const COARSE_PASS_DEADLINE_MS: u64 = 50;

/// Conservative byte estimate for one TEXT tile in the coarse pass.
///
/// TEXT tiles (≤4 distinct colours) use AV1 `palette_index` coding
/// (Features 93–94).  For a 32×32 tile with 1–4 colours:
///
///   palette table : 4 entries × 3 bytes (RGB)             = 12 bytes
///   index plane   : 1 024 pixels × 2 bits / ~20× entropy  ≈ 13 bytes
///   per-tile framing and header overhead                   ≈  5 bytes
///   ──────────────────────────────────────────────────────────────────
///   conservative total                                     ≈ 30 bytes
///
/// Actual text tiles compress tighter when most pixels share a single
/// background colour; 30 bytes is the safe upper bound used for timing.
pub const COARSE_BYTES_PER_PALETTE_TILE: u64 = 30;

/// Conservative byte estimate for one PICTURE tile in the fast lossy coarse pass.
///
/// PICTURE tiles (17–256 colours) are encoded with a fast lossy AV1 preset
/// in the coarse pass — favouring low encode latency over quality.  The
/// coarse image is substantially smaller than the
/// [`LOSSLESS_BYTES_PER_PICTURE_TILE`] (400 B) refinement target:
///
///   32 × 32 pixels × 0.3 bits/px (fast preset, high QP) / 8 ≈ 39 bytes
///   per-tile header and OBU framing overhead               ≈ 36 bytes
///   ──────────────────────────────────────────────────────────────────
///   conservative total                                     ≈ 75 bytes
///
/// The lossy coarse encode provides a recognisable first impression;
/// the refinement pass restores pixel-exact quality within ~1 second.
pub const COARSE_BYTES_PER_PICTURE_TILE_COARSE: u64 = 75;

/// Target deadline for PICTURE tiles to reach pixel-exact quality (ms).
pub const PIXEL_EXACT_DEADLINE_MS: u64 = 1_000;

/// Conservative lossless byte estimate per PICTURE tile at 32×32 pixels.
///
/// Derivation — AV1 lossless encode of a BGRA8 32×32 region with 50–200
/// distinct colours.  Context-adaptive intra coding at this palette complexity
/// achieves roughly 0.5 bits/pixel on screen content:
///
///   32 × 32 pixels × 0.5 bits/px / 8 bits/byte = 64 bytes
///
/// Adding per-tile header overhead and rounding conservatively gives 400 bytes.
/// The conservatism ensures the timing model does not overstate capacity.
pub const LOSSLESS_BYTES_PER_PICTURE_TILE: u64 = 400;

// ── TileClass ─────────────────────────────────────────────────────────────────

/// Tile classification from colour-count analysis (Feature 92).
///
/// Classification runs in < 1 µs per 32×32 tile via a stack-allocated 4096-bit
/// array indexed by a 12-bit colour fingerprint (top 4 bits per RGB channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileClass {
    /// ≤ 4 distinct colours; text glyphs, cursors, and simple UI elements.
    ///
    /// Encoded losslessly with `palette_index` in full 4:4:4 chroma
    /// (Features 93–94).  Pixel-exact in the coarse first_pass.
    Text,

    /// 5–16 distinct colours; solid fills, gradients, and multi-tone UI regions.
    ///
    /// Same lossless palette coding as [`Text`](Self::Text).
    Flat,

    /// 17–256 distinct colours; icons, images, and syntax-highlighted regions
    /// that exceed the 16-colour palette limit.
    ///
    /// Fast lossy AV1 in the coarse pass; re-encoded to lossless via the
    /// `priority_queue` within [`PIXEL_EXACT_DEADLINE_MS`] ms.
    Picture,

    /// > 256 distinct colours or high-motion video region.
    ///
    /// Confined to a Gear-B AV1 sub-stream (Feature 96); excluded from the
    /// two-pass refinement pipeline.
    Video,
}

impl TileClass {
    /// Saliency priority for the refinement queue.
    ///
    /// Higher value → dequeued sooner.  TEXT and FLAT tiles are prioritised over
    /// PICTURE so any extremely rare deferred text tile is always refined before
    /// photographic content.
    pub fn saliency(self) -> u8 {
        match self {
            Self::Text    => 3,
            Self::Flat    => 2,
            Self::Picture => 1,
            Self::Video   => 0,
        }
    }

    /// Whether the coarse-pass encoding for this class is bit-exact (lossless).
    ///
    /// `true` for TEXT and FLAT (palette_index coding, Features 93–94).
    /// `false` for PICTURE (fast lossy AV1 in the coarse pass).
    /// `false` for VIDEO (Gear-B AV1 sub-stream, not in the two-pass pipeline).
    pub fn coarse_is_lossless(self) -> bool {
        matches!(self, Self::Text | Self::Flat)
    }

    /// Whether this tile class is enqueued for lossless refinement after the
    /// coarse pass.
    ///
    /// Only PICTURE tiles need refinement; TEXT and FLAT are already lossless
    /// in the coarse pass, and VIDEO uses the Gear-B sub-stream.
    pub fn needs_refinement(self) -> bool {
        self == Self::Picture
    }
}

// ── classify_tile ─────────────────────────────────────────────────────────────

/// Classify a BGRA8 tile by counting distinct colours (Feature 92).
///
/// Uses a 4096-bit stack-allocated bitset indexed by a 12-bit fingerprint
/// `(R >> 4) << 8 | (G >> 4) << 4 | (B >> 4)`.  Alpha is ignored.
/// Runs in < 1 µs for a 32×32 tile.
///
/// `pixels` must be BGRA8 with exactly [`TILE_BYTES`] bytes.
///
/// # Panics (debug only)
///
/// Panics in debug builds when `pixels.len() != TILE_BYTES`.
pub fn classify_tile(pixels: &[u8]) -> TileClass {
    debug_assert_eq!(
        pixels.len(),
        TILE_BYTES,
        "classify_tile: expected {TILE_BYTES} bytes, got {}",
        pixels.len()
    );

    let mut seen = [0u64; 64]; // 64 × 64-bit words = 4 096 bits
    let mut count = 0usize;

    for chunk in pixels.chunks_exact(4) {
        let b   = (chunk[0] >> 4) as usize;
        let g   = (chunk[1] >> 4) as usize;
        let r   = (chunk[2] >> 4) as usize;
        let idx = (r << 8) | (g << 4) | b;
        let word = idx >> 6;
        let bit  = 1u64 << (idx & 63);
        if seen[word] & bit == 0 {
            seen[word] |= bit;
            count += 1;
            if count > VIDEO_COLOR_LIMIT {
                return TileClass::Video;
            }
        }
    }

    match count {
        c if c <= TEXT_COLOR_LIMIT    => TileClass::Text,
        c if c <= PALETTE_COLOR_LIMIT => TileClass::Flat,
        _                             => TileClass::Picture,
    }
}

// ── TileCoord ─────────────────────────────────────────────────────────────────

/// Column and row position of a tile in the tile grid (Feature 91).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    /// Zero-based column index.
    pub col: u32,
    /// Zero-based row index.
    pub row: u32,
}

// ── RefinementQueue ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueEntry {
    saliency: u8,
    seq:      u64, // monotonic insertion counter; lower → inserted earlier
    coord:    TileCoord,
    class:    TileClass,
}

impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: higher saliency first.
        // Tiebreak: lower seq first (FIFO within a saliency tier).
        self.saliency
            .cmp(&other.saliency)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority queue for lossless refinement of PICTURE tiles (Feature 98).
///
/// Tiles are dequeued in decreasing saliency order (TEXT first, then FLAT,
/// then PICTURE).  Within a saliency tier, tiles are dequeued in FIFO order
/// so older damage is resolved before newer damage of equal priority.
///
/// Only [`TileClass::Picture`] tiles normally enter this queue; TEXT and FLAT
/// tiles are already lossless in the coarse pass and do not require refinement.
pub struct RefinementQueue {
    heap: BinaryHeap<QueueEntry>,
    seq:  u64,
}

impl RefinementQueue {
    /// Create an empty refinement queue.
    pub fn new() -> Self {
        Self { heap: BinaryHeap::new(), seq: 0 }
    }

    /// Enqueue `coord` for lossless refinement with `class`-derived saliency.
    pub fn push(&mut self, coord: TileCoord, class: TileClass) {
        self.heap.push(QueueEntry {
            saliency: class.saliency(),
            seq: self.seq,
            coord,
            class,
        });
        self.seq += 1;
    }

    /// Remove and return the highest-priority pending tile.
    ///
    /// Returns `None` when the queue is empty and all dirty tiles have been
    /// refined to lossless quality.
    pub fn pop(&mut self) -> Option<(TileCoord, TileClass)> {
        self.heap.pop().map(|e| (e.coord, e.class))
    }

    /// Number of tiles pending lossless refinement.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// `true` when all pending tiles have been refined.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

impl Default for RefinementQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_colour_tile_is_text() {
        let mut px = vec![0xFFu8; TILE_BYTES];
        // Add black pixels at every 16th position.
        for i in 0..1024usize {
            if i % 16 == 0 {
                let off = i * 4;
                px[off]     = 0x00;
                px[off + 1] = 0x00;
                px[off + 2] = 0x00;
                px[off + 3] = 0xFF;
            }
        }
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Text);
        assert!(class.coarse_is_lossless());
        assert!(!class.needs_refinement());
    }

    #[test]
    fn thirty_colour_tile_is_picture() {
        let mut px = vec![0u8; TILE_BYTES];
        for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
            let c = i % 30;
            // 5 × 6 distinct (R>>4, G>>4) pairs — all 30 map to unique fingerprints.
            chunk[0] = 0x00;
            chunk[1] = ((c % 6) as u8) << 4;
            chunk[2] = ((c / 6) as u8) << 4;
            chunk[3] = 0xFF;
        }
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Picture);
        assert!(!class.coarse_is_lossless());
        assert!(class.needs_refinement());
    }

    #[test]
    fn refinement_queue_prioritises_text_over_picture() {
        let mut q = RefinementQueue::new();
        q.push(TileCoord { col: 0, row: 0 }, TileClass::Picture);
        q.push(TileCoord { col: 1, row: 0 }, TileClass::Text);
        let (_, first)  = q.pop().unwrap();
        let (_, second) = q.pop().unwrap();
        assert_eq!(first,  TileClass::Text,    "Text saliency=3 must come before Picture saliency=1");
        assert_eq!(second, TileClass::Picture);
        assert!(q.is_empty());
    }

    #[test]
    fn refinement_queue_fifo_within_same_saliency() {
        let mut q = RefinementQueue::new();
        let a = TileCoord { col: 0, row: 0 };
        let b = TileCoord { col: 1, row: 0 };
        let c = TileCoord { col: 2, row: 0 };
        q.push(a, TileClass::Picture);
        q.push(b, TileClass::Picture);
        q.push(c, TileClass::Picture);
        assert_eq!(q.pop().unwrap().0, a, "oldest tile must come out first");
        assert_eq!(q.pop().unwrap().0, b);
        assert_eq!(q.pop().unwrap().0, c);
    }
}

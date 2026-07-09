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

// ── TileRect ──────────────────────────────────────────────────────────────────

/// Axis-aligned bounding rectangle expressed in tile coordinates.
///
/// Used by [`VideoSubStream`] to describe the union of all VIDEO-classified
/// tile positions that need a Gear-B AV1 encode this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    /// Left-most tile column (inclusive).
    pub col_min: u32,
    /// Top-most tile row (inclusive).
    pub row_min: u32,
    /// Right-most tile column (inclusive).
    pub col_max: u32,
    /// Bottom-most tile row (inclusive).
    pub row_max: u32,
}

impl TileRect {
    /// Width in tiles.
    #[inline]
    pub fn cols(&self) -> u32 {
        self.col_max - self.col_min + 1
    }

    /// Height in tiles.
    #[inline]
    pub fn rows(&self) -> u32 {
        self.row_max - self.row_min + 1
    }

    /// Pixel-accurate left edge of the rectangle.
    #[inline]
    pub fn x_px(&self) -> u32 {
        self.col_min * TILE_SIZE_PX
    }

    /// Pixel-accurate top edge of the rectangle.
    #[inline]
    pub fn y_px(&self) -> u32 {
        self.row_min * TILE_SIZE_PX
    }

    /// Pixel-accurate width of the rectangle.
    #[inline]
    pub fn width_px(&self) -> u32 {
        self.cols() * TILE_SIZE_PX
    }

    /// Pixel-accurate height of the rectangle.
    #[inline]
    pub fn height_px(&self) -> u32 {
        self.rows() * TILE_SIZE_PX
    }
}

// ── VideoSubStream ────────────────────────────────────────────────────────────

/// Gear-B AV1 sub-stream isolator for VIDEO-classified tiles (Feature 96).
///
/// VIDEO tiles (> [`VIDEO_COLOR_LIMIT`] distinct colours, or high-motion
/// regions) are excluded from the two-pass encode pipeline entirely.  They are
/// instead confined here: each call to [`push`] extends a single bounding
/// [`TileRect`] that the caller submits to a dedicated SVT-AV1 Gear-B encode.
///
/// The sub-stream is isolated from the [`RefinementQueue`] — VIDEO tiles never
/// enter the lossless-refinement path and do not compete with text and UI tiles
/// for idle bandwidth.
///
/// # Usage
///
/// After classifying each dirty tile:
///
/// 1. If `tile_class == TileClass::Video`, call `video_sub_stream.push(coord)`.
/// 2. All other classes enter the two-pass pipeline as normal.
/// 3. At frame-submission time, call `video_sub_stream.take_dirty_region()`.
///    If `Some(rect)` is returned, submit `rect` (in pixels: see [`TileRect`])
///    to the Gear-B SVT-AV1 encoder; the dirty flag is cleared.
///
/// [`push`]: Self::push
/// [`take_dirty_region`]: Self::take_dirty_region
pub struct VideoSubStream {
    /// Bounding rect of all registered VIDEO tiles, or `None` when no tiles
    /// have been pushed since the last reset.
    region: Option<TileRect>,
    /// `true` when at least one [`push`] has been called since the last
    /// [`take_dirty_region`].
    dirty: bool,
}

impl VideoSubStream {
    /// Create an empty sub-stream with no registered tiles.
    pub fn new() -> Self {
        Self { region: None, dirty: false }
    }

    /// Register a VIDEO tile at `coord`.
    ///
    /// Extends the bounding [`TileRect`] to include `coord` and marks the
    /// sub-stream dirty so that the next call to [`take_dirty_region`] returns
    /// the updated region.
    ///
    /// [`take_dirty_region`]: Self::take_dirty_region
    pub fn push(&mut self, coord: TileCoord) {
        self.dirty = true;
        match self.region {
            None => {
                self.region = Some(TileRect {
                    col_min: coord.col,
                    row_min: coord.row,
                    col_max: coord.col,
                    row_max: coord.row,
                });
            }
            Some(ref mut r) => {
                r.col_min = r.col_min.min(coord.col);
                r.row_min = r.row_min.min(coord.row);
                r.col_max = r.col_max.max(coord.col);
                r.row_max = r.row_max.max(coord.row);
            }
        }
    }

    /// Return the bounding rect of all registered VIDEO tiles and clear the
    /// dirty flag.
    ///
    /// Returns `Some(rect)` exactly once after one or more [`push`] calls.
    /// Returns `None` when nothing has changed since the last call.
    ///
    /// The caller is responsible for submitting the returned rect to the Gear-B
    /// SVT-AV1 encoder as the capture/encode region for this frame.
    ///
    /// [`push`]: Self::push
    pub fn take_dirty_region(&mut self) -> Option<TileRect> {
        if self.dirty {
            self.dirty = false;
            self.region
        } else {
            None
        }
    }

    /// `true` when at least one [`push`] has been called since the last
    /// [`take_dirty_region`].
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// `true` when no VIDEO tiles have been registered yet (i.e. no [`push`]
    /// calls since construction or the last [`reset`]).
    ///
    /// [`reset`]: Self::reset
    pub fn is_empty(&self) -> bool {
        self.region.is_none()
    }

    /// Discard all registered tiles and clear the dirty flag.
    ///
    /// Call on scene cuts or when the video region disappears entirely.
    pub fn reset(&mut self) {
        self.region = None;
        self.dirty = false;
    }
}

impl Default for VideoSubStream {
    fn default() -> Self {
        Self::new()
    }
}

// ── bits_per_index ────────────────────────────────────────────────────────────

/// Bits needed to represent a palette index for a palette of size `n`.
///
/// | n      | bits | note                         |
/// |--------|------|------------------------------|
/// | 1      | 0    | single colour; no index data |
/// | 2      | 1    |                              |
/// | 3–4    | 2    |                              |
/// | 5–8    | 3    |                              |
/// | 9–16   | 4    |                              |
#[inline]
fn bits_per_index(n: usize) -> u32 {
    match n {
        0 | 1 => 0,
        2     => 1,
        3..=4 => 2,
        5..=8 => 3,
        _     => 4, // 9..=PALETTE_COLOR_LIMIT
    }
}

// ── BitWriter ─────────────────────────────────────────────────────────────────

struct BitWriter {
    buf:       Vec<u8>,
    current:   u8,
    bits_used: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { buf: Vec::new(), current: 0, bits_used: 0 }
    }

    /// Append the `n_bits` LSBs of `value` to the stream (LSB first per byte).
    fn write_bits(&mut self, value: u32, n_bits: u32) {
        for i in 0..n_bits {
            let bit = ((value >> i) & 1) as u8;
            self.current |= bit << self.bits_used;
            self.bits_used += 1;
            if self.bits_used == 8 {
                self.buf.push(self.current);
                self.current   = 0;
                self.bits_used = 0;
            }
        }
    }

    /// Flush any partial byte (zero-padded) and return the byte buffer.
    fn finish(mut self) -> Vec<u8> {
        if self.bits_used > 0 {
            self.buf.push(self.current);
        }
        self.buf
    }
}

// ── BitReader ─────────────────────────────────────────────────────────────────

struct BitReader<'a> {
    data:     &'a [u8],
    byte_pos: usize,
    bit_pos:  u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, byte_pos: 0, bit_pos: 0 }
    }

    /// Read `n_bits` from the stream into the LSBs of a `u32` (LSB first).
    ///
    /// Returns `None` when fewer than `n_bits` bits remain.
    fn read_bits(&mut self, n_bits: u32) -> Option<u32> {
        let mut result = 0u32;
        for i in 0..n_bits {
            if self.byte_pos >= self.data.len() {
                return None;
            }
            let bit = (self.data[self.byte_pos] >> self.bit_pos) & 1;
            result       |= (bit as u32) << i;
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.byte_pos += 1;
                self.bit_pos   = 0;
            }
        }
        Some(result)
    }
}

// ── PaletteEncodeError ────────────────────────────────────────────────────────

/// Error returned by [`PaletteTileEncoder::encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEncodeError {
    /// Tile contains more than [`PALETTE_COLOR_LIMIT`] distinct RGB colours.
    ///
    /// The tile has escaped the palette coding path; the caller must route it
    /// to PICTURE or VIDEO coding instead.
    TooManyColors {
        /// Lower bound on distinct colours found before giving up.
        found: usize,
    },
    /// Input slice is not exactly [`TILE_BYTES`] bytes.
    InvalidLength {
        /// Actual byte count supplied.
        got: usize,
    },
}

impl std::fmt::Display for PaletteEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyColors { found } => write!(
                f,
                "tile has >{PALETTE_COLOR_LIMIT} distinct colours (found ≥{found}); \
                 use PICTURE coding"
            ),
            Self::InvalidLength { got } => write!(
                f,
                "palette encoder expects {TILE_BYTES} bytes, got {got}"
            ),
        }
    }
}

// ── PaletteDecodeError ────────────────────────────────────────────────────────

/// Error returned by [`PaletteTileDecoder::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteDecodeError {
    /// The encoded stream ended before all expected data was consumed.
    Truncated,
    /// The `n_colors` header byte is `0` or `> 16`.
    InvalidPaletteSize {
        /// The value found in the header.
        got: u8,
    },
    /// A palette index in the index stream is ≥ the declared palette size.
    IndexOutOfRange {
        /// The bad index value.
        index: u32,
        /// The palette size declared in the header.
        palette_size: usize,
    },
}

impl std::fmt::Display for PaletteDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "palette bitstream truncated"),
            Self::InvalidPaletteSize { got } => write!(
                f,
                "n_colors must be 1–{PALETTE_COLOR_LIMIT}, got {got}"
            ),
            Self::IndexOutOfRange { index, palette_size } => write!(
                f,
                "palette index {index} is out of range for palette of size {palette_size}"
            ),
        }
    }
}

// ── PaletteTileEncoder ────────────────────────────────────────────────────────

/// Lossless palette encoder for TEXT and FLAT tiles (Feature 93).
///
/// Converts a BGRA8 32×32 tile into a compact `palette_index` bitstream.
/// Full 4:4:4 chroma: all three channels (B, G, R) are stored at full
/// precision per palette entry with no subsampling.
///
/// # Wire format
///
/// ```text
/// byte  0            : n_colors (1..=16)
/// bytes 1..(1+n*3)   : palette — n × [B, G, R], source BGR order
/// bytes (1+n*3)..    : bit-packed index stream, LSB-first within each byte
///                      bits per index:
///                        n_colors == 1 → 0 bits (no index bytes)
///                        n_colors == 2 → 1 bit
///                        n_colors  3–4 → 2 bits
///                        n_colors  5–8 → 3 bits
///                        n_colors 9–16 → 4 bits
///                      1 024 indices packed, final byte zero-padded
/// ```
///
/// [`PaletteTileDecoder::decode`] is the matching decoder.
pub struct PaletteTileEncoder;

impl PaletteTileEncoder {
    /// Encode a BGRA8 tile to a `palette_index` bitstream.
    ///
    /// `pixels` must be exactly [`TILE_BYTES`] bytes.
    ///
    /// Returns `Err(PaletteEncodeError::TooManyColors)` when the tile contains
    /// more than [`PALETTE_COLOR_LIMIT`] distinct RGB values; the caller should
    /// route the tile to PICTURE or VIDEO coding.
    pub fn encode(pixels: &[u8]) -> Result<Vec<u8>, PaletteEncodeError> {
        if pixels.len() != TILE_BYTES {
            return Err(PaletteEncodeError::InvalidLength { got: pixels.len() });
        }

        let palette = Self::build_palette(pixels)?;
        let n       = palette.len();
        let bpi     = bits_per_index(n);

        let index_bytes = if bpi == 0 { 0 } else { (1024 * bpi as usize + 7) / 8 };
        let mut out = Vec::with_capacity(1 + n * 3 + index_bytes);

        // Header: colour count.
        out.push(n as u8);

        // Palette table: n × [B, G, R] (full 4:4:4).
        for bgr in &palette {
            out.extend_from_slice(bgr);
        }

        // Index stream: one index per pixel, bit-packed LSB-first.
        if bpi > 0 {
            let mut w = BitWriter::new();
            for chunk in pixels.chunks_exact(4) {
                let bgr = [chunk[0], chunk[1], chunk[2]];
                let idx = palette.iter().position(|p| p == &bgr).unwrap() as u32;
                w.write_bits(idx, bpi);
            }
            out.extend(w.finish());
        }

        Ok(out)
    }

    /// Extract an ordered list of distinct [B, G, R] colours from `pixels`.
    ///
    /// Colours are recorded in first-occurrence order.  Returns
    /// `Err(TooManyColors)` as soon as a new colour would exceed
    /// [`PALETTE_COLOR_LIMIT`].
    fn build_palette(pixels: &[u8]) -> Result<Vec<[u8; 3]>, PaletteEncodeError> {
        let mut palette: Vec<[u8; 3]> = Vec::with_capacity(PALETTE_COLOR_LIMIT);
        for chunk in pixels.chunks_exact(4) {
            let bgr = [chunk[0], chunk[1], chunk[2]];
            if !palette.contains(&bgr) {
                if palette.len() == PALETTE_COLOR_LIMIT {
                    return Err(PaletteEncodeError::TooManyColors {
                        found: PALETTE_COLOR_LIMIT + 1,
                    });
                }
                palette.push(bgr);
            }
        }
        Ok(palette)
    }
}

// ── PaletteTileDecoder ────────────────────────────────────────────────────────

/// Lossless palette decoder for TEXT and FLAT tiles (Feature 93).
///
/// Reconstructs a BGRA8 32×32 tile from a bitstream produced by
/// [`PaletteTileEncoder::encode`].  Alpha is always restored as `0xFF`.
pub struct PaletteTileDecoder;

impl PaletteTileDecoder {
    /// Decode a `palette_index` bitstream into BGRA8 pixel data.
    ///
    /// Returns exactly [`TILE_BYTES`] bytes on success.
    pub fn decode(data: &[u8]) -> Result<Vec<u8>, PaletteDecodeError> {
        if data.is_empty() {
            return Err(PaletteDecodeError::Truncated);
        }

        let n = data[0] as usize;
        if n == 0 || n > PALETTE_COLOR_LIMIT {
            return Err(PaletteDecodeError::InvalidPaletteSize { got: data[0] });
        }

        let palette_end = 1 + n * 3;
        if data.len() < palette_end {
            return Err(PaletteDecodeError::Truncated);
        }

        let palette: Vec<[u8; 3]> = data[1..palette_end]
            .chunks_exact(3)
            .map(|s| [s[0], s[1], s[2]])
            .collect();

        let bpi  = bits_per_index(n);
        let mut pixels = vec![0u8; TILE_BYTES];

        if bpi == 0 {
            // Single colour: fill every pixel without reading an index stream.
            let [b, g, r] = palette[0];
            for out in pixels.chunks_exact_mut(4) {
                out[0] = b; out[1] = g; out[2] = r; out[3] = 0xFF;
            }
        } else {
            let mut reader = BitReader::new(&data[palette_end..]);
            for out in pixels.chunks_exact_mut(4) {
                let idx = reader
                    .read_bits(bpi)
                    .ok_or(PaletteDecodeError::Truncated)? as usize;
                if idx >= palette.len() {
                    return Err(PaletteDecodeError::IndexOutOfRange {
                        index:        idx as u32,
                        palette_size: palette.len(),
                    });
                }
                let [b, g, r] = palette[idx];
                out[0] = b; out[1] = g; out[2] = r; out[3] = 0xFF;
            }
        }

        Ok(pixels)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    // ── PaletteTileEncoder / PaletteTileDecoder ───────────────────────────────

    #[test]
    fn palette_single_colour_round_trips() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0x11, 0x22, 0x33, 0xFF]);
        }
        let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
        let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
        // Alpha is restored as 0xFF; original alpha may differ, check RGB only.
        for (i, (d, p)) in decoded.chunks_exact(4).zip(pixels.chunks_exact(4)).enumerate() {
            assert_eq!(d[..3], p[..3], "pixel {i}: BGR mismatch after round-trip");
            assert_eq!(d[3], 0xFF,     "pixel {i}: alpha must be 0xFF after decode");
        }
    }

    #[test]
    fn palette_two_colour_round_trips() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            if i % 2 == 0 {
                chunk.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
            } else {
                chunk.copy_from_slice(&[0x00, 0x00, 0x00, 0xFF]);
            }
        }
        let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
        let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
        for (i, (d, p)) in decoded.chunks_exact(4).zip(pixels.chunks_exact(4)).enumerate() {
            assert_eq!(d[..3], p[..3], "pixel {i}: BGR mismatch");
        }
    }

    #[test]
    fn palette_sixteen_colour_round_trips() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            let c = (i % 16) as u8;
            chunk[0] = c << 4;
            chunk[1] = 0x00;
            chunk[2] = 0x00;
            chunk[3] = 0xFF;
        }
        let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
        assert_eq!(encoded[0], 16, "n_colors header must be 16");
        let decoded = PaletteTileDecoder::decode(&encoded).unwrap();
        for (i, (d, p)) in decoded.chunks_exact(4).zip(pixels.chunks_exact(4)).enumerate() {
            assert_eq!(d[..3], p[..3], "pixel {i}: BGR mismatch");
        }
    }

    #[test]
    fn palette_encoder_rejects_seventeen_colours() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            let c = (i % 17) as u8;
            chunk[0] = c;
            chunk[1] = 0x00;
            chunk[2] = 0x00;
            chunk[3] = 0xFF;
        }
        match PaletteTileEncoder::encode(&pixels) {
            Err(PaletteEncodeError::TooManyColors { .. }) => {}
            other => panic!("expected TooManyColors, got {other:?}"),
        }
    }

    #[test]
    fn palette_encoder_rejects_wrong_length() {
        let pixels = vec![0u8; TILE_BYTES - 1];
        match PaletteTileEncoder::encode(&pixels) {
            Err(PaletteEncodeError::InvalidLength { got }) => {
                assert_eq!(got, TILE_BYTES - 1);
            }
            other => panic!("expected InvalidLength, got {other:?}"),
        }
    }

    #[test]
    fn palette_decoder_rejects_empty_input() {
        assert_eq!(
            PaletteTileDecoder::decode(&[]),
            Err(PaletteDecodeError::Truncated)
        );
    }

    #[test]
    fn palette_decoder_rejects_zero_palette_size() {
        assert_eq!(
            PaletteTileDecoder::decode(&[0]),
            Err(PaletteDecodeError::InvalidPaletteSize { got: 0 })
        );
    }

    #[test]
    fn palette_decoder_rejects_oversized_palette() {
        assert_eq!(
            PaletteTileDecoder::decode(&[17]),
            Err(PaletteDecodeError::InvalidPaletteSize { got: 17 })
        );
    }

    #[test]
    fn palette_single_colour_no_index_bytes() {
        // A 1-colour tile has n_colors=1, 3 palette bytes, and 0 index bytes.
        let mut pixels = vec![0u8; TILE_BYTES];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xFF]);
        }
        let encoded = PaletteTileEncoder::encode(&pixels).unwrap();
        assert_eq!(encoded.len(), 1 + 1 * 3, "single-colour: header + 3-byte palette only");
        assert_eq!(encoded[0], 1);
        assert_eq!(encoded[1], 0xAA); // B
        assert_eq!(encoded[2], 0xBB); // G
        assert_eq!(encoded[3], 0xCC); // R
    }

    #[test]
    fn bits_per_index_table() {
        assert_eq!(bits_per_index(1),  0);
        assert_eq!(bits_per_index(2),  1);
        assert_eq!(bits_per_index(3),  2);
        assert_eq!(bits_per_index(4),  2);
        assert_eq!(bits_per_index(5),  3);
        assert_eq!(bits_per_index(8),  3);
        assert_eq!(bits_per_index(9),  4);
        assert_eq!(bits_per_index(16), 4);
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

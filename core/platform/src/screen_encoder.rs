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

// ── TileGrid ──────────────────────────────────────────────────────────────────

/// Tile grid for a captured frame (Feature 91).
///
/// Maps a frame of `frame_width × frame_height` pixels onto a grid of 32×32
/// tiles.  The rightmost column and bottom row may be partial tiles when the
/// frame dimensions are not exact multiples of [`TILE_SIZE_PX`].
///
/// # Usage
///
/// ```ignore
/// let grid = TileGrid::new(1920, 1080);
/// for dirty_rect in frame.dirty_rects {
///     for coord in grid.tiles_for_rect(dirty_rect) {
///         let tile_px = grid.extract_tile(&frame.pixels, frame.stride, coord);
///         let class   = classify_tile(&tile_px);
///         // … route to coarse-pass encoder …
///     }
/// }
/// ```
pub struct TileGrid {
    /// Number of tile columns: `ceil(frame_width / TILE_SIZE_PX)`.
    pub cols: u32,
    /// Number of tile rows: `ceil(frame_height / TILE_SIZE_PX)`.
    pub rows: u32,
    frame_width:  u32,
    frame_height: u32,
}

impl TileGrid {
    /// Construct a tile grid for a frame of the given pixel dimensions.
    pub fn new(frame_width: u32, frame_height: u32) -> Self {
        let cols = frame_width.div_ceil(TILE_SIZE_PX);
        let rows = frame_height.div_ceil(TILE_SIZE_PX);
        Self { cols, rows, frame_width, frame_height }
    }

    /// Return all tile coordinates that overlap the given damage rectangle.
    ///
    /// `rect` is a damage region in frame coordinates.  Negative `x`/`y`
    /// values are clamped to zero; regions outside the frame are clipped.
    ///
    /// The returned coordinates are in raster order (row-major, left-to-right
    /// within each row, top-to-bottom across rows).
    pub fn tiles_for_rect(&self, rect: crate::screen_capture::DirtyRect) -> Vec<TileCoord> {
        let x0 = rect.x.max(0) as u32;
        let y0 = rect.y.max(0) as u32;
        let x1 = (rect.x.saturating_add(rect.width as i32)).max(0) as u32;
        let y1 = (rect.y.saturating_add(rect.height as i32)).max(0) as u32;

        let x1 = x1.min(self.frame_width);
        let y1 = y1.min(self.frame_height);

        if x1 <= x0 || y1 <= y0 {
            return Vec::new();
        }

        let col_min = x0 / TILE_SIZE_PX;
        let col_max = (x1 - 1) / TILE_SIZE_PX;
        let row_min = y0 / TILE_SIZE_PX;
        let row_max = (y1 - 1) / TILE_SIZE_PX;

        let capacity = ((col_max - col_min + 1) * (row_max - row_min + 1)) as usize;
        let mut tiles = Vec::with_capacity(capacity);
        for row in row_min..=row_max {
            for col in col_min..=col_max {
                tiles.push(TileCoord { col, row });
            }
        }
        tiles
    }

    /// Extract BGRA8 pixel data for a single tile from the frame buffer.
    ///
    /// `pixels` is the raw frame pixel data in BGRA8 format.  `stride` is the
    /// row stride in bytes (may exceed `frame_width × 4` due to alignment
    /// padding).
    ///
    /// The returned array is exactly [`TILE_BYTES`] bytes.  Pixels that fall
    /// outside the frame boundary (partial tiles at the right or bottom edge)
    /// are filled with `0x00`.
    pub fn extract_tile(
        &self,
        pixels: &[u8],
        stride: u32,
        coord:  TileCoord,
    ) -> [u8; TILE_BYTES] {
        let mut tile = [0u8; TILE_BYTES];
        let tile_x = coord.col * TILE_SIZE_PX;
        let tile_y = coord.row * TILE_SIZE_PX;

        for row in 0..TILE_SIZE_PX {
            let src_y = tile_y + row;
            if src_y >= self.frame_height {
                break;
            }
            let src_row = (src_y * stride) as usize;
            let dst_row = (row  * TILE_SIZE_PX * 4) as usize;

            for col in 0..TILE_SIZE_PX {
                let src_x = tile_x + col;
                if src_x >= self.frame_width {
                    break;
                }
                let src_off = src_row + (src_x * 4) as usize;
                let dst_off = dst_row + (col  * 4) as usize;
                tile[dst_off..dst_off + 4].copy_from_slice(&pixels[src_off..src_off + 4]);
            }
        }

        tile
    }

    /// Total number of tiles in the grid (`cols × rows`).
    #[inline]
    pub fn tile_count(&self) -> u32 {
        self.cols * self.rows
    }
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

// ── BlitCommand ───────────────────────────────────────────────────────────────

/// 16-byte wire command emitted when a scroll is detected (Feature 90).
///
/// Tells the remote renderer to:
///
/// 1. **Blit** (copy within its framebuffer) the `region`, shifted by `(dx, dy)`.
/// 2. **Paint** the newly exposed strip with the pixel data that accompanies
///    this command (see [`BlitResult::exposed_strip`]).
///
/// # Wire layout — little-endian, 16 bytes total
///
/// ```text
///  0– 1  region_x  i16  left edge of the scrolled region (screen pixels)
///  2– 3  region_y  i16  top  edge of the scrolled region (screen pixels)
///  4– 5  region_w  u16  width  of the scrolled region (pixels)
///  6– 7  region_h  u16  height of the scrolled region (pixels)
///  8– 9  dx        i16  horizontal content displacement (+ = moved right)
/// 10–11  dy        i16  vertical   content displacement (+ = moved down)
/// 12–13  strip_w   u16  width  of the newly exposed strip (pixels)
/// 14–15  strip_h   u16  height of the newly exposed strip (pixels)
/// ```
///
/// # Exposed-strip position
///
/// The origin of the exposed strip is derived from the other fields (see
/// [`strip_origin`](Self::strip_origin)):
///
/// | Condition | Strip origin              |
/// |-----------|---------------------------|
/// | `dy < 0`  | bottom of region: `(region_x, region_y + region_h + dy)` |
/// | `dy > 0`  | top    of region: `(region_x, region_y)`                 |
/// | `dx < 0`  | right  of region: `(region_x + region_w + dx, region_y)` |
/// | `dx > 0`  | left   of region: `(region_x, region_y)`                 |
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlitCommand {
    /// Left edge of the scrolled region in screen pixels.
    pub region_x: i16,
    /// Top edge of the scrolled region in screen pixels.
    pub region_y: i16,
    /// Width of the scrolled region in pixels.
    pub region_w: u16,
    /// Height of the scrolled region in pixels.
    pub region_h: u16,
    /// Horizontal content displacement in pixels.
    ///
    /// Positive: content moved right; exposed strip is on the **left**.
    /// Negative: content moved left; exposed strip is on the **right**.
    /// Zero for pure vertical scrolls.
    pub dx: i16,
    /// Vertical content displacement in pixels.
    ///
    /// Positive: content moved down; exposed strip is at the **top**.
    /// Negative: content moved up; exposed strip is at the **bottom**.
    /// Zero for pure horizontal scrolls.
    pub dy: i16,
    /// Width of the exposed strip in pixels.
    pub strip_w: u16,
    /// Height of the exposed strip in pixels.
    pub strip_h: u16,
}

const _BLIT_COMMAND_SIZE: () = assert!(
    std::mem::size_of::<BlitCommand>() == 16,
    "BlitCommand must be exactly 16 bytes"
);

impl BlitCommand {
    /// Encode to exactly 16 little-endian bytes.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..2].copy_from_slice(&self.region_x.to_le_bytes());
        out[2..4].copy_from_slice(&self.region_y.to_le_bytes());
        out[4..6].copy_from_slice(&self.region_w.to_le_bytes());
        out[6..8].copy_from_slice(&self.region_h.to_le_bytes());
        out[8..10].copy_from_slice(&self.dx.to_le_bytes());
        out[10..12].copy_from_slice(&self.dy.to_le_bytes());
        out[12..14].copy_from_slice(&self.strip_w.to_le_bytes());
        out[14..16].copy_from_slice(&self.strip_h.to_le_bytes());
        out
    }

    /// Decode from exactly 16 little-endian bytes.
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            region_x: i16::from_le_bytes([b[0],  b[1]]),
            region_y: i16::from_le_bytes([b[2],  b[3]]),
            region_w: u16::from_le_bytes([b[4],  b[5]]),
            region_h: u16::from_le_bytes([b[6],  b[7]]),
            dx:       i16::from_le_bytes([b[8],  b[9]]),
            dy:       i16::from_le_bytes([b[10], b[11]]),
            strip_w:  u16::from_le_bytes([b[12], b[13]]),
            strip_h:  u16::from_le_bytes([b[14], b[15]]),
        }
    }

    /// Screen-space `(x, y)` origin of the exposed strip.
    ///
    /// Derived from `region_*` and `dx`/`dy` — no extra bytes needed in the
    /// wire format.
    pub fn strip_origin(&self) -> (i32, i32) {
        if self.dy < 0 {
            // Content moved up: new content appears at the bottom.
            (
                self.region_x as i32,
                self.region_y as i32 + self.region_h as i32 + self.dy as i32,
            )
        } else if self.dy > 0 {
            // Content moved down: new content appears at the top.
            (self.region_x as i32, self.region_y as i32)
        } else if self.dx < 0 {
            // Content moved left: new content appears on the right.
            (
                self.region_x as i32 + self.region_w as i32 + self.dx as i32,
                self.region_y as i32,
            )
        } else {
            // Content moved right (dx > 0): new content appears on the left.
            (self.region_x as i32, self.region_y as i32)
        }
    }
}

// ── BlitResult ────────────────────────────────────────────────────────────────

/// Output of a successful scroll detection (Feature 90).
pub struct BlitResult {
    /// The 16-byte scroll descriptor.
    pub command: BlitCommand,
    /// Raw BGRA8 pixels of the newly-exposed strip.
    ///
    /// Length is exactly `command.strip_w as usize × command.strip_h as usize × 4`.
    /// Pixels are in raster order (row-major, left-to-right, top-to-bottom).
    pub exposed_strip: Vec<u8>,
}

// ── ScrollDetector ────────────────────────────────────────────────────────────

/// Maximum candidate shift per axis searched at 1/8 scale.
///
/// 16 units × 8 px/unit = 128 full-scale pixels per axis.  Covers all
/// common scroll velocities at 60 Hz.
pub const SCROLL_MAX_SMALL_SHIFT: i32 = 16;

/// Minimum dirty-region dimension (full-scale pixels) for scroll detection.
///
/// Regions smaller than 64 × 64 pixels provide insufficient correlation
/// context; the detector ignores them.
pub const SCROLL_MIN_REGION_PX: u32 = 64;

/// Fractional SAD improvement over the no-motion baseline required to
/// declare a scroll.
///
/// A value of 0.30 means the best-shift normalized-SAD must be at least 30%
/// lower than the zero-shift baseline.  Tuned to reject noise while accepting
/// genuine scrolls on typical screen content.
pub const SCROLL_CONFIDENCE_THRESHOLD: f64 = 0.30;

struct SmallFrame {
    luma:   Vec<u8>, // one byte per 8×8 source block, row-major
    w:      u32,     // width  in small-scale units
    h:      u32,     // height in small-scale units
    orig_w: u32,
    orig_h: u32,
}

/// Scroll detector using phase-correlation at 1/8-scale luma (Features 89–90).
///
/// On each call to [`detect`](Self::detect) the detector:
///
/// 1. Box-averages the current frame to 1/8 scale and converts to luma.
/// 2. Extracts the 1/8-scale luma patch for the supplied dirty region from
///    both the current and the stored previous frame.
/// 3. Searches for the translation `(u, v)` in `[−16, 16]²` (small-scale
///    units = `[−128, 128]` full-scale pixels) that minimises the
///    normalized SAD between the two patches.
/// 4. If the best shift reduces SAD by at least 30% over the zero-shift
///    baseline, emits a [`BlitResult`] with the full-scale
///    [`BlitCommand`] and the BGRA8 pixels of the newly-exposed strip
///    extracted from the current frame.
pub struct ScrollDetector {
    prev: Option<SmallFrame>,
}

impl ScrollDetector {
    /// Create a new detector with no stored previous frame.
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Attempt to detect a scroll in `region` between the previous and current
    /// frames.
    ///
    /// Returns `Some(BlitResult)` when a confident scroll is detected, `None`
    /// otherwise (first call, static frame, region too small, or weak match).
    ///
    /// **Must be called for every captured frame** so the 1/8-scale luma store
    /// stays in sync with the capture pipeline.
    pub fn detect(
        &mut self,
        pixels:  &[u8],
        frame_w: u32,
        frame_h: u32,
        stride:  u32,
        region:  crate::screen_capture::DirtyRect,
    ) -> Option<BlitResult> {
        let curr_small = compute_luma_small(pixels, frame_w, frame_h, stride);

        let result = (|| {
            let prev = self.prev.as_ref()?;

            if prev.orig_w != frame_w || prev.orig_h != frame_h {
                return None;
            }

            // Clamp and validate region.
            let rx = region.x.max(0) as u32;
            let ry = region.y.max(0) as u32;
            let x1 = (region.x + region.width  as i32).max(0).min(frame_w as i32) as u32;
            let y1 = (region.y + region.height as i32).max(0).min(frame_h as i32) as u32;
            let rw = x1.saturating_sub(rx);
            let rh = y1.saturating_sub(ry);

            if rw < SCROLL_MIN_REGION_PX || rh < SCROLL_MIN_REGION_PX {
                return None;
            }

            // Convert region to 1/8-scale coordinates.
            let sx = rx / 8;
            let sy = ry / 8;
            let sw = rw.div_ceil(8)
                .min(prev.w.saturating_sub(sx))
                .min(curr_small.w.saturating_sub(sx));
            let sh = rh.div_ceil(8)
                .min(prev.h.saturating_sub(sy))
                .min(curr_small.h.saturating_sub(sy));

            if sw < 4 || sh < 4 {
                return None;
            }

            let prev_roi = extract_roi(&prev.luma, prev.w, sx, sy, sw, sh);
            let curr_roi = extract_roi(&curr_small.luma, curr_small.w, sx, sy, sw, sh);

            let (su, sv) = find_best_shift(&prev_roi, &curr_roi, sw, sh, SCROLL_MAX_SMALL_SHIFT)?;

            // Convert to full-scale and project onto the dominant axis.
            let full_dx = -(su * 8);
            let full_dy = -(sv * 8);
            let (dx, dy) = if su.abs() >= sv.abs() {
                (full_dx, 0i32)
            } else {
                (0i32, full_dy)
            };

            if dx == 0 && dy == 0 {
                return None;
            }

            let (strip_x, strip_y, strip_w, strip_h) =
                compute_strip(rx as i32, ry as i32, rw, rh, dx, dy);

            if strip_w == 0 || strip_h == 0 {
                return None;
            }

            let exposed_strip = extract_strip_pixels(
                pixels, frame_w, frame_h, stride,
                strip_x, strip_y, strip_w, strip_h,
            );

            Some(BlitResult {
                command: BlitCommand {
                    region_x: rx as i16,
                    region_y: ry as i16,
                    region_w: rw as u16,
                    region_h: rh as u16,
                    dx: dx as i16,
                    dy: dy as i16,
                    strip_w:  strip_w as u16,
                    strip_h:  strip_h as u16,
                },
                exposed_strip,
            })
        })();

        self.prev = Some(curr_small);
        result
    }
}

impl Default for ScrollDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ── ScrollDetector internal helpers ───────────────────────────────────────────

/// Box-average the full frame to 1/8 scale and convert to luma.
///
/// Luma: `Y = (2·R + 5·G + B) >> 3` — integer approximation of BT.601
/// coefficients that avoids a multiply while preserving colour sensitivity.
fn compute_luma_small(pixels: &[u8], frame_w: u32, frame_h: u32, stride: u32) -> SmallFrame {
    let w = frame_w.div_ceil(8);
    let h = frame_h.div_ceil(8);
    let mut luma = vec![0u8; (w * h) as usize];

    for sy in 0..h {
        for sx in 0..w {
            let mut sum   = 0u32;
            let mut count = 0u32;
            for dy in 0..8u32 {
                let fy = sy * 8 + dy;
                if fy >= frame_h { break; }
                for dx in 0..8u32 {
                    let fx = sx * 8 + dx;
                    if fx >= frame_w { break; }
                    let off = (fy * stride + fx * 4) as usize;
                    let b   = pixels[off]     as u32;
                    let g   = pixels[off + 1] as u32;
                    let r   = pixels[off + 2] as u32;
                    sum += (2 * r + 5 * g + b) >> 3;
                    count += 1;
                }
            }
            luma[(sy * w + sx) as usize] = if count > 0 { (sum / count) as u8 } else { 0 };
        }
    }

    SmallFrame { luma, w, h, orig_w: frame_w, orig_h: frame_h }
}

/// Copy a rectangular sub-region from a row-major luma buffer.
fn extract_roi(luma: &[u8], luma_w: u32, rx: u32, ry: u32, rw: u32, rh: u32) -> Vec<u8> {
    let mut roi = vec![0u8; (rw * rh) as usize];
    for y in 0..rh {
        let src = ((ry + y) * luma_w + rx) as usize;
        let dst = (y * rw) as usize;
        roi[dst..dst + rw as usize].copy_from_slice(&luma[src..src + rw as usize]);
    }
    roi
}

/// Find translation `(u, v)` such that `prev[x+u, y+v] ≈ curr[x, y]`.
///
/// Returns `None` when no shift beats the zero-shift baseline by the
/// [`SCROLL_CONFIDENCE_THRESHOLD`] margin, or when the best shift is `(0, 0)`.
fn find_best_shift(
    prev: &[u8],
    curr: &[u8],
    w:    u32,
    h:    u32,
    max_shift: i32,
) -> Option<(i32, i32)> {
    let baseline = normalized_sad(prev, curr, w, h, 0, 0);
    if baseline == 0 {
        return None; // frames are identical; no scroll
    }

    let mut best_sad  = baseline;
    let mut best_u    = 0i32;
    let mut best_v    = 0i32;
    let mut best_mag2 = i64::MAX; // u² + v² of current best (tie-break: prefer smaller)

    for v in -max_shift..=max_shift {
        for u in -max_shift..=max_shift {
            if u == 0 && v == 0 { continue; }
            let sad  = normalized_sad(prev, curr, w, h, u, v);
            let mag2 = (u as i64 * u as i64) + (v as i64 * v as i64);
            let better = sad < best_sad || (sad == best_sad && mag2 < best_mag2);
            if better {
                best_sad  = sad;
                best_u    = u;
                best_v    = v;
                best_mag2 = mag2;
            }
        }
    }

    if best_u == 0 && best_v == 0 {
        return None;
    }

    let improvement = 1.0 - (best_sad as f64 / baseline as f64);
    if improvement < SCROLL_CONFIDENCE_THRESHOLD {
        return None;
    }

    Some((best_u, best_v))
}

/// Normalized SAD: per-pixel absolute difference between `prev[x+u, y+v]` and
/// `curr[x, y]` over the valid overlap, scaled to the full region area so
/// that different shifts with different overlap sizes are comparable.
fn normalized_sad(prev: &[u8], curr: &[u8], w: u32, h: u32, u: i32, v: i32) -> u64 {
    let x0 = 0i32.max(-u);
    let y0 = 0i32.max(-v);
    let x1 = (w as i32).min(w as i32 - u);
    let y1 = (h as i32).min(h as i32 - v);

    if x1 <= x0 || y1 <= y0 {
        return u64::MAX;
    }

    let overlap = ((x1 - x0) * (y1 - y0)) as u64;
    let mut sum = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let pi = ((y + v) * w as i32 + (x + u)) as usize;
            let ci = (y         * w as i32 + x     ) as usize;
            sum += (prev[pi] as i32 - curr[ci] as i32).unsigned_abs() as u64;
        }
    }

    sum * (w * h) as u64 / overlap
}

/// Derive exposed-strip position and size from the region and displacement.
fn compute_strip(rx: i32, ry: i32, rw: u32, rh: u32, dx: i32, dy: i32)
    -> (i32, i32, u32, u32) // (strip_x, strip_y, strip_w, strip_h)
{
    if dy < 0 {
        (rx, ry + rh as i32 + dy, rw, (-dy) as u32)
    } else if dy > 0 {
        (rx, ry, rw, dy as u32)
    } else if dx < 0 {
        (rx + rw as i32 + dx, ry, (-dx) as u32, rh)
    } else {
        // dx > 0
        (rx, ry, dx as u32, rh)
    }
}

/// Extract BGRA8 pixels from a rectangular strip of the frame.
fn extract_strip_pixels(
    pixels:  &[u8],
    frame_w: u32,
    frame_h: u32,
    stride:  u32,
    strip_x: i32,
    strip_y: i32,
    strip_w: u32,
    strip_h: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (strip_w * strip_h * 4) as usize];
    for row in 0..strip_h {
        let src_y = strip_y + row as i32;
        if src_y < 0 || src_y >= frame_h as i32 { continue; }
        let src_y = src_y as u32;
        for col in 0..strip_w {
            let src_x = strip_x + col as i32;
            if src_x < 0 || src_x >= frame_w as i32 { continue; }
            let src_x = src_x as u32;
            let src_off = (src_y * stride + src_x * 4) as usize;
            let dst_off = ((row * strip_w + col) * 4) as usize;
            out[dst_off..dst_off + 4].copy_from_slice(&pixels[src_off..src_off + 4]);
        }
    }
    out
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

// ── build_palette ─────────────────────────────────────────────────────────────

/// Extract an ordered list of distinct [B, G, R] colours from BGRA8 `pixels`.
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

    fn build_palette(pixels: &[u8]) -> Result<Vec<[u8; 3]>, PaletteEncodeError> {
        build_palette(pixels)
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

// ── Context Colour Order (CCO) ─────────────────────────────────────────────────

/// Build the Context Colour Order (CCO) for a pixel given left and above
/// neighbour palette indices.
///
/// The CCO is a permutation of `0..n` that places the most context-probable
/// indices first:
///
/// * `left` always occupies position 0.
/// * `above` occupies position 1 when it differs from `left`.
/// * All remaining indices follow in their natural (first-occurrence) order.
///
/// This mirrors the AV1 palette context model used for `palette_mode` blocks.
fn cco_for_context(left: usize, above: usize, n: usize) -> Vec<usize> {
    let mut cco = Vec::with_capacity(n);
    cco.push(left);
    if above != left {
        cco.push(above);
    }
    for i in 0..n {
        if i != left && i != above {
            cco.push(i);
        }
    }
    debug_assert_eq!(cco.len(), n);
    cco
}

// ── Entropy position I/O ──────────────────────────────────────────────────────

/// Write a CCO rank to a [`BitWriter`] using a hit/miss prefix code.
///
/// * Rank 0 (context hit): write "0" — 1 bit.
/// * Rank k > 0 (miss): write "1" (1 bit), then write k − 1 in
///   `bits_per_index(n − 1)` bits.
/// * n == 1: no bits written (single-colour tile; rank is always 0).
///
/// For n == 2 a miss is always "1" (1 bit, no suffix), so every pixel costs
/// exactly 1 bit regardless of rank — identical to the raw bit-packed cost.
fn write_entropy_pos(w: &mut BitWriter, rank: usize, n: usize) {
    if n <= 1 {
        return;
    }
    if rank == 0 {
        w.write_bits(0, 1);
    } else {
        w.write_bits(1, 1);
        let bpi = bits_per_index(n - 1);
        if bpi > 0 {
            w.write_bits((rank - 1) as u32, bpi);
        }
    }
}

/// Read a CCO rank from a [`BitReader`] produced by [`write_entropy_pos`].
///
/// Returns `None` on premature end-of-stream.
fn read_entropy_pos(r: &mut BitReader<'_>, n: usize) -> Option<usize> {
    if n <= 1 {
        return Some(0);
    }
    let hit = r.read_bits(1)?;
    if hit == 0 {
        Some(0)
    } else {
        let bpi = bits_per_index(n - 1);
        let miss_val = if bpi > 0 { r.read_bits(bpi)? as usize } else { 0 };
        Some(miss_val + 1)
    }
}

// ── EntropyPaletteEncoder ─────────────────────────────────────────────────────

/// Lossless palette entropy encoder for TEXT and FLAT tiles (Feature 94).
///
/// Extends the raw [`PaletteTileEncoder`] with context-adaptive coding of the
/// `palette_index` stream.  For each pixel in raster order, the **Context
/// Colour Order** (CCO) is derived from the palette indices of the left and
/// above neighbours; the current pixel's rank in the CCO is then coded with a
/// hit/miss prefix:
///
/// | rank | bits written                                     |
/// |------|--------------------------------------------------|
/// | 0    | "0"  (1 bit — context hit)                       |
/// | k>0  | "1" + (k−1) in `bits_per_index(n−1)` bits        |
///
/// # Compression properties
///
/// * n == 1: no index bytes (identical to [`PaletteTileEncoder`]).
/// * n == 2: 1 bit per pixel regardless of rank (identical to raw).
/// * n ≥ 3: better than raw when the context hit-rate exceeds a threshold that
///   depends on n (e.g. > 25 % for n == 16, > 50 % for n == 4).  Typical
///   TEXT and FLAT tiles — horizontal colour runs, solid regions — achieve
///   hit-rates of 70–95 %, yielding 50–80 % index-stream size reductions.
///
/// # Wire format
///
/// Header identical to [`PaletteTileEncoder`]:
///
/// ```text
/// byte  0            : n_colors (1..=16)
/// bytes 1..(1+n*3)   : palette — n × [B, G, R] (full 4:4:4 chroma)
/// bytes (1+n*3)..    : entropy-coded CCO-rank stream (context from left/above)
///                      one rank per pixel in raster order; final byte zero-padded
/// ```
///
/// Decoded by [`EntropyPaletteDecoder::decode`].
pub struct EntropyPaletteEncoder;

impl EntropyPaletteEncoder {
    /// Encode a BGRA8 tile with context-adaptive entropy coding of palette indices.
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

        let palette = build_palette(pixels)?;
        let n = palette.len();

        let mut out = Vec::with_capacity(1 + n * 3 + 256);
        out.push(n as u8);
        for bgr in &palette {
            out.extend_from_slice(bgr);
        }

        if n == 1 {
            return Ok(out);
        }

        let mut stored = [0u8; 1024]; // raw palette indices for context lookups
        let mut w = BitWriter::new();

        for px in 0..1024usize {
            let row = px / TILE_SIZE_PX as usize;
            let col = px % TILE_SIZE_PX as usize;
            let left  = if col > 0 { stored[px - 1]                        as usize } else { 0 };
            let above = if row > 0 { stored[px - TILE_SIZE_PX as usize]    as usize } else { 0 };
            let cco   = cco_for_context(left, above, n);

            let bgr     = [pixels[px * 4], pixels[px * 4 + 1], pixels[px * 4 + 2]];
            let raw_idx = palette.iter().position(|p| p == &bgr).unwrap();
            stored[px]  = raw_idx as u8;

            let rank = cco.iter().position(|&c| c == raw_idx).unwrap();
            write_entropy_pos(&mut w, rank, n);
        }

        out.extend(w.finish());
        Ok(out)
    }
}

// ── EntropyPaletteDecoder ─────────────────────────────────────────────────────

/// Lossless palette entropy decoder for TEXT and FLAT tiles (Feature 94).
///
/// Reconstructs BGRA8 pixel data from a bitstream produced by
/// [`EntropyPaletteEncoder::encode`].  Alpha is always restored as `0xFF`.
pub struct EntropyPaletteDecoder;

impl EntropyPaletteDecoder {
    /// Decode an entropy-coded palette bitstream into BGRA8 pixel data.
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

        let mut pixels = vec![0u8; TILE_BYTES];

        if n == 1 {
            let [b, g, r] = palette[0];
            for out in pixels.chunks_exact_mut(4) {
                out[0] = b; out[1] = g; out[2] = r; out[3] = 0xFF;
            }
            return Ok(pixels);
        }

        let mut stored = [0u8; 1024];
        let mut reader = BitReader::new(&data[palette_end..]);

        for px in 0..1024usize {
            let row = px / TILE_SIZE_PX as usize;
            let col = px % TILE_SIZE_PX as usize;
            let left  = if col > 0 { stored[px - 1]                        as usize } else { 0 };
            let above = if row > 0 { stored[px - TILE_SIZE_PX as usize]    as usize } else { 0 };
            let cco   = cco_for_context(left, above, n);

            let rank = read_entropy_pos(&mut reader, n)
                .ok_or(PaletteDecodeError::Truncated)?;
            if rank >= n {
                return Err(PaletteDecodeError::IndexOutOfRange {
                    index:        rank as u32,
                    palette_size: n,
                });
            }

            let raw_idx = cco[rank];
            stored[px]  = raw_idx as u8;

            let [b, g, r] = palette[raw_idx];
            let out = &mut pixels[px * 4..(px + 1) * 4];
            out[0] = b; out[1] = g; out[2] = r; out[3] = 0xFF;
        }

        Ok(pixels)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a tile with exactly `n_colors` distinct colours, each
    // mapping to a unique 12-bit fingerprint via `(R>>4, G>>4, B>>4)`.
    fn tile_with_n_distinct_colors(n: usize) -> Vec<u8> {
        assert!(n <= 256 * 16, "too many colours for a 32×32 tile");
        let mut px = vec![0xFFu8; TILE_BYTES];
        for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
            let c = i % n;
            // Spread colours across the B and G nibble channels so each maps to
            // a distinct (R>>4, G>>4, B>>4) fingerprint.
            chunk[0] = ((c % 16) as u8) << 4; // B nibble
            chunk[1] = ((c / 16 % 16) as u8) << 4; // G nibble
            chunk[2] = ((c / 256 % 16) as u8) << 4; // R nibble
            chunk[3] = 0xFF;
        }
        px
    }

    #[test]
    fn single_colour_tile_is_text() {
        let px = tile_with_n_distinct_colors(1);
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Text);
        assert!(class.coarse_is_lossless());
        assert!(!class.needs_refinement());
    }

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

    // ── classify_tile: class boundaries ──────────────────────────────────────

    #[test]
    fn four_colour_tile_is_text() {
        let px = tile_with_n_distinct_colors(TEXT_COLOR_LIMIT);
        assert_eq!(classify_tile(&px), TileClass::Text, "4 colours → Text");
    }

    #[test]
    fn five_colour_tile_is_flat() {
        let px = tile_with_n_distinct_colors(TEXT_COLOR_LIMIT + 1);
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Flat, "5 colours → Flat");
        assert!(class.coarse_is_lossless(), "Flat must be lossless in coarse pass");
        assert!(!class.needs_refinement(), "Flat does not need refinement");
    }

    #[test]
    fn sixteen_colour_tile_is_flat() {
        let px = tile_with_n_distinct_colors(PALETTE_COLOR_LIMIT);
        assert_eq!(classify_tile(&px), TileClass::Flat, "16 colours → Flat");
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
    fn seventeen_colour_tile_is_picture() {
        let px = tile_with_n_distinct_colors(PALETTE_COLOR_LIMIT + 1);
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Picture, "17 colours → Picture");
        assert!(!class.coarse_is_lossless());
        assert!(class.needs_refinement());
    }

    #[test]
    fn two_fifty_six_colour_tile_is_picture() {
        let px = tile_with_n_distinct_colors(VIDEO_COLOR_LIMIT);
        assert_eq!(classify_tile(&px), TileClass::Picture, "256 colours → Picture");
    }

    #[test]
    fn two_fifty_seven_colour_tile_is_video() {
        // Build a tile where the first 257 pixels each have a unique fingerprint.
        // Remaining pixels repeat existing colours (tile is only 1024 pixels).
        let mut px = vec![0u8; TILE_BYTES];
        for (i, chunk) in px.chunks_exact_mut(4).enumerate() {
            let c = i.min(VIDEO_COLOR_LIMIT); // first 257 are unique; rest repeat 257
            chunk[0] = ((c % 16) as u8) << 4;
            chunk[1] = ((c / 16 % 16) as u8) << 4;
            chunk[2] = ((c / 256 % 16) as u8) << 4;
            chunk[3] = 0xFF;
        }
        let class = classify_tile(&px);
        assert_eq!(class, TileClass::Video, "257 distinct colours → Video");
        assert!(!class.coarse_is_lossless());
        assert!(!class.needs_refinement(), "Video never enters the refinement queue");
    }

    #[test]
    fn video_class_has_lowest_saliency() {
        assert!(TileClass::Video.saliency() < TileClass::Picture.saliency());
        assert!(TileClass::Picture.saliency() < TileClass::Flat.saliency());
        assert!(TileClass::Flat.saliency() < TileClass::Text.saliency());
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

    // ── EntropyPaletteEncoder / EntropyPaletteDecoder ─────────────────────────

    #[test]
    fn entropy_single_colour_round_trips() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0x11, 0x22, 0x33, 0xFF]);
        }
        let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
        let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
        for (i, (d, p)) in decoded.chunks_exact(4).zip(pixels.chunks_exact(4)).enumerate() {
            assert_eq!(d[..3], p[..3], "pixel {i}: BGR mismatch");
            assert_eq!(d[3], 0xFF, "pixel {i}: alpha must be 0xFF");
        }
    }

    #[test]
    fn entropy_sixteen_colour_round_trips() {
        let mut pixels = vec![0u8; TILE_BYTES];
        for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            let c = (i % 16) as u8;
            chunk[0] = c << 4; chunk[1] = 0x00; chunk[2] = 0x00; chunk[3] = 0xFF;
        }
        let encoded = EntropyPaletteEncoder::encode(&pixels).unwrap();
        let decoded = EntropyPaletteDecoder::decode(&encoded).unwrap();
        for (i, (d, p)) in decoded.chunks_exact(4).zip(pixels.chunks_exact(4)).enumerate() {
            assert_eq!(d[..3], p[..3], "pixel {i}: BGR mismatch");
        }
    }

    #[test]
    fn entropy_horizontal_run_compresses_better_than_raw() {
        // A tile where each of 8 rows (4 rows per colour) is a solid colour.
        // Left-context hit rate ≈ 97 %: only the first column of each colour
        // group misses; every other pixel matches its left neighbour.
        let mut pixels = vec![0u8; TILE_BYTES];
        for (i, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            let row = i / TILE_SIZE_PX as usize;
            let color = (row / 4) as u8; // colour changes every 4 rows (8 colours)
            chunk[0] = color << 4; chunk[1] = 0x00; chunk[2] = 0x00; chunk[3] = 0xFF;
        }
        let raw     = PaletteTileEncoder::encode(&pixels).unwrap();
        let entropy = EntropyPaletteEncoder::encode(&pixels).unwrap();
        // Both produce the same header; index stream must be smaller with entropy.
        assert!(
            entropy.len() < raw.len(),
            "entropy-coded horizontal-run tile ({} bytes) must be smaller than \
             raw bit-packed ({} bytes)",
            entropy.len(), raw.len()
        );
    }

    #[test]
    fn entropy_cco_places_left_first_then_above() {
        let cco = cco_for_context(3, 7, 10);
        assert_eq!(cco[0], 3, "CCO[0] must be the left neighbour index");
        assert_eq!(cco[1], 7, "CCO[1] must be the above neighbour index when ≠ left");
        assert_eq!(cco.len(), 10, "CCO length must equal palette size");
    }

    #[test]
    fn entropy_cco_deduplicates_when_left_equals_above() {
        let cco = cco_for_context(5, 5, 8);
        assert_eq!(cco[0], 5, "CCO[0] must be left (== above)");
        assert_eq!(cco.len(), 8, "no duplicate: len must still be n");
        assert_eq!(cco.iter().filter(|&&x| x == 5).count(), 1, "5 must appear exactly once");
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

    // ── TileGrid ──────────────────────────────────────────────────────────────

    use crate::screen_capture::DirtyRect;

    fn dirty(x: i32, y: i32, width: u32, height: u32) -> DirtyRect {
        DirtyRect { x, y, width, height }
    }

    #[test]
    fn tile_grid_dimensions_for_848x480() {
        let grid = TileGrid::new(848, 480);
        assert_eq!(grid.cols, 27, "ceil(848/32) = 27");
        assert_eq!(grid.rows, 15, "ceil(480/32) = 15");
        assert_eq!(grid.tile_count(), 405);
    }

    #[test]
    fn tile_grid_exact_multiple_of_tile_size() {
        let grid = TileGrid::new(128, 64);
        assert_eq!(grid.cols, 4, "128/32 = 4");
        assert_eq!(grid.rows, 2, "64/32 = 2");
        assert_eq!(grid.tile_count(), 8);
    }

    #[test]
    fn tiles_for_rect_single_tile_aligned() {
        let grid = TileGrid::new(256, 256);
        // Rect exactly at tile (1, 2): x=32, y=64, 32×32
        let tiles = grid.tiles_for_rect(dirty(32, 64, 32, 32));
        assert_eq!(tiles, vec![TileCoord { col: 1, row: 2 }]);
    }

    #[test]
    fn tiles_for_rect_spans_two_columns() {
        let grid = TileGrid::new(256, 256);
        // Rect from x=16 to x=48 (width=32): crosses cols 0 and 1 at row 0.
        let tiles = grid.tiles_for_rect(dirty(16, 0, 32, 32));
        assert_eq!(tiles, vec![
            TileCoord { col: 0, row: 0 },
            TileCoord { col: 1, row: 0 },
        ]);
    }

    #[test]
    fn tiles_for_rect_2x2_grid_of_tiles() {
        let grid = TileGrid::new(256, 256);
        // Rect from (16, 16) to (48, 48): a 32×32 window crossing a 2×2 tile block.
        let tiles = grid.tiles_for_rect(dirty(16, 16, 32, 32));
        assert_eq!(tiles, vec![
            TileCoord { col: 0, row: 0 },
            TileCoord { col: 1, row: 0 },
            TileCoord { col: 0, row: 1 },
            TileCoord { col: 1, row: 1 },
        ]);
    }

    #[test]
    fn tiles_for_rect_full_frame() {
        let grid = TileGrid::new(64, 64);
        let tiles = grid.tiles_for_rect(dirty(0, 0, 64, 64));
        assert_eq!(tiles.len(), 4, "64×64 at tile_size=32 → 2×2 = 4 tiles");
        assert!(tiles.contains(&TileCoord { col: 0, row: 0 }));
        assert!(tiles.contains(&TileCoord { col: 1, row: 0 }));
        assert!(tiles.contains(&TileCoord { col: 0, row: 1 }));
        assert!(tiles.contains(&TileCoord { col: 1, row: 1 }));
    }

    #[test]
    fn tiles_for_rect_clamps_to_frame_boundary() {
        let grid = TileGrid::new(64, 64);
        // Rect that extends beyond the frame.
        let tiles = grid.tiles_for_rect(dirty(48, 48, 64, 64));
        // Only tile (1, 1) is within the 2×2 grid.
        assert_eq!(tiles, vec![TileCoord { col: 1, row: 1 }]);
    }

    #[test]
    fn tiles_for_rect_empty_when_out_of_frame() {
        let grid = TileGrid::new(64, 64);
        // Rect entirely outside the frame.
        let tiles = grid.tiles_for_rect(dirty(200, 200, 32, 32));
        assert!(tiles.is_empty(), "rect outside frame must yield no tiles");
    }

    #[test]
    fn tiles_for_rect_negative_origin_clamped() {
        let grid = TileGrid::new(64, 64);
        // Rect starts at (-16, -16) with size 48×48 — effective area is (0,0)→(32,32).
        let tiles = grid.tiles_for_rect(dirty(-16, -16, 48, 48));
        assert_eq!(tiles, vec![TileCoord { col: 0, row: 0 }]);
    }

    #[test]
    fn tiles_for_rect_raster_order() {
        let grid = TileGrid::new(96, 96);
        let tiles = grid.tiles_for_rect(dirty(0, 0, 96, 96));
        // 3×3 grid; tiles must be in row-major order.
        assert_eq!(tiles[0], TileCoord { col: 0, row: 0 });
        assert_eq!(tiles[1], TileCoord { col: 1, row: 0 });
        assert_eq!(tiles[2], TileCoord { col: 2, row: 0 });
        assert_eq!(tiles[3], TileCoord { col: 0, row: 1 });
        assert_eq!(tiles.len(), 9);
    }

    #[test]
    fn extract_tile_copies_pixels_from_frame() {
        // 64×64 frame, two tiles wide and two tiles tall.
        let stride = 64 * 4u32;
        let mut pixels = vec![0u8; (stride * 64) as usize];
        // Fill tile (1, 1) (x=32..64, y=32..64) with a distinctive pattern.
        for y in 32u32..64 {
            for x in 32u32..64 {
                let off = (y * stride + x * 4) as usize;
                pixels[off]     = 0xAA; // B
                pixels[off + 1] = 0xBB; // G
                pixels[off + 2] = 0xCC; // R
                pixels[off + 3] = 0xFF; // A
            }
        }

        let grid = TileGrid::new(64, 64);
        let tile = grid.extract_tile(&pixels, stride, TileCoord { col: 1, row: 1 });
        assert_eq!(tile.len(), TILE_BYTES);

        // All 1024 pixels in the extracted tile must carry the fill value.
        for chunk in tile.chunks_exact(4) {
            assert_eq!(chunk[0], 0xAA, "B channel mismatch");
            assert_eq!(chunk[1], 0xBB, "G channel mismatch");
            assert_eq!(chunk[2], 0xCC, "R channel mismatch");
            assert_eq!(chunk[3], 0xFF, "A channel mismatch");
        }
    }

    #[test]
    fn extract_tile_partial_at_right_edge_zeroes_out_of_bounds() {
        // 48×32 frame: the right column tile (col=1) is only 16 px wide.
        let stride = 48 * 4u32;
        let pixels = vec![0xFFu8; (stride * 32) as usize];
        let grid = TileGrid::new(48, 32);
        let tile = grid.extract_tile(&pixels, stride, TileCoord { col: 1, row: 0 });

        // Columns 0..16 (px 0..16 within the tile) come from the frame.
        for row in 0..32usize {
            for col in 0..32usize {
                let off = row * 32 * 4 + col * 4;
                if col < 16 {
                    assert_eq!(tile[off], 0xFF, "in-bounds pixel must be copied");
                } else {
                    assert_eq!(tile[off], 0x00, "out-of-bounds pixel must be zeroed");
                }
            }
        }
    }

    #[test]
    fn extract_tile_partial_at_bottom_edge_zeroes_out_of_bounds() {
        // 32×48 frame: the bottom row tile (row=1) is only 16 px tall.
        let stride = 32 * 4u32;
        let pixels = vec![0xFFu8; (stride * 48) as usize];
        let grid = TileGrid::new(32, 48);
        let tile = grid.extract_tile(&pixels, stride, TileCoord { col: 0, row: 1 });

        for row in 0..32usize {
            let off = row * 32 * 4;
            if row < 16 {
                assert_eq!(tile[off], 0xFF, "in-bounds row must be copied");
            } else {
                assert_eq!(tile[off], 0x00, "out-of-bounds row must be zeroed");
            }
        }
    }

    #[test]
    fn extract_tile_respects_stride_padding() {
        // 32×32 frame; stride = 128 + 16 = 144 (16 bytes of row padding after active pixels).
        let stride = 144u32;
        let mut pixels = vec![0x00u8; (stride * 32) as usize];
        // Fill the 32-pixel-wide active area with 0xAB.
        for y in 0..32u32 {
            for x in 0..32u32 {
                let off = (y * stride + x * 4) as usize;
                pixels[off..off + 4].copy_from_slice(&[0xAB, 0xCD, 0xEF, 0xFF]);
            }
        }
        let grid = TileGrid::new(32, 32);
        let tile = grid.extract_tile(&pixels, stride, TileCoord { col: 0, row: 0 });
        for chunk in tile.chunks_exact(4) {
            assert_eq!(chunk[0], 0xAB, "stride padding must not bleed into tile data");
        }
    }

    #[test]
    fn tile_grid_pixel_to_coord_mapping() {
        // Every pixel in an 848×480 frame must map into valid tile coords.
        let grid = TileGrid::new(848, 480);
        for y in 0u32..480 {
            for x in 0u32..848 {
                let col = x / TILE_SIZE_PX;
                let row = y / TILE_SIZE_PX;
                assert!(
                    col < grid.cols && row < grid.rows,
                    "pixel ({x},{y}) → tile ({col},{row}) is outside grid {}×{}",
                    grid.cols, grid.rows
                );
            }
        }
    }
}

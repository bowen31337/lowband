//! Screen-frame transfer over the encrypted session (FR-3 / NFR-4).
//!
//! The M2 "screen codec (view)" exit criterion and the NFR-4 text-legibility
//! bar are served by LowBand's real, pure-Rust lossless tile codec
//! ([`PaletteTileEncoder`] / [`PaletteTileDecoder`]) — which the eval found
//! implemented but never wired into a transmit path. This module is that
//! path: it splits a captured frame into 32×32 tiles, encodes each with the
//! lossless palette coder (falling back to raw BGRA for photographic tiles
//! until the AV1 gear lands), ships them over the [`SecureSession`], and
//! reassembles the frame on the far side.
//!
//! Because text/flat tiles round-trip **losslessly**, decoded screen text is
//! pixel-identical to the source — a strictly stronger guarantee than the
//! PRD's OCR ≥ 99.5% bar (identical pixels ⇒ identical OCR).
//!
//! Tile encodings:
//! - `0` palette — lossless, compact; TEXT/FLAT tiles (≤ 16 colors).
//! - `1` raw — lossless, uncompressed BGRA; the pre-AV1 fallback for
//!   photographic tiles that exceed the palette color limit.

use lowband_crypto::SecureSession;
use lowband_platform::{PaletteTileDecoder, PaletteTileEncoder, TileCoord, TileGrid, TILE_BYTES, TILE_SIZE_PX};

const KIND_BEGIN: u8 = 0x20;
const KIND_TILE: u8 = 0x21;
const KIND_END: u8 = 0x22;

const ENC_PALETTE: u8 = 0;
const ENC_RAW: u8 = 1;

/// A screen-transfer frame on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenFrame {
    /// Start of a frame of `width`×`height` BGRA pixels.
    Begin { width: u32, height: u32 },
    /// One encoded tile at grid position (`col`, `row`).
    Tile { col: u32, row: u32, encoding: u8, data: Vec<u8> },
    /// End of frame — the receiver reassembles and returns the framebuffer.
    End,
}

impl ScreenFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ScreenFrame::Begin { width, height } => {
                out.push(KIND_BEGIN);
                out.extend_from_slice(&(*width as u16).to_le_bytes());
                out.extend_from_slice(&(*height as u16).to_le_bytes());
            }
            ScreenFrame::Tile { col, row, encoding, data } => {
                out.push(KIND_TILE);
                out.extend_from_slice(&(*col as u16).to_le_bytes());
                out.extend_from_slice(&(*row as u16).to_le_bytes());
                out.push(*encoding);
                out.extend_from_slice(&(data.len() as u16).to_le_bytes());
                out.extend_from_slice(data);
            }
            ScreenFrame::End => out.push(KIND_END),
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ScreenError> {
        let (&kind, mut rest) = buf.split_first().ok_or(ScreenError::Truncated)?;
        match kind {
            KIND_BEGIN => {
                let width = take_u16(&mut rest)? as u32;
                let height = take_u16(&mut rest)? as u32;
                Ok(ScreenFrame::Begin { width, height })
            }
            KIND_TILE => {
                let col = take_u16(&mut rest)? as u32;
                let row = take_u16(&mut rest)? as u32;
                let (&encoding, r2) = rest.split_first().ok_or(ScreenError::Truncated)?;
                rest = r2;
                let len = take_u16(&mut rest)? as usize;
                if len > TILE_BYTES {
                    return Err(ScreenError::TooLarge);
                }
                let (data, _) = rest.split_at_checked(len).ok_or(ScreenError::Truncated)?;
                Ok(ScreenFrame::Tile { col, row, encoding, data: data.to_vec() })
            }
            KIND_END => Ok(ScreenFrame::End),
            other => Err(ScreenError::UnknownKind(other)),
        }
    }
}

/// Send a BGRA8 frame (`stride = width × 4`) over `session`, tile by tile.
///
/// Outbound half of the screen plane — exercised by tests today; the daemon
/// binds it to the screen-capture source when that platform wiring lands.
#[allow(dead_code)]
pub fn send_frame(
    session: &mut SecureSession,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), ScreenError> {
    let stride = width * 4;
    if pixels.len() < (stride * height) as usize {
        return Err(ScreenError::ShortBuffer);
    }
    let grid = TileGrid::new(width, height);
    session.send(&ScreenFrame::Begin { width, height }.encode())?;

    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let tile = grid.extract_tile(pixels, stride, TileCoord { col, row });
            // Prefer the lossless palette coder; fall back to raw BGRA for
            // tiles that exceed the palette color limit (photographic).
            let (encoding, data) = match PaletteTileEncoder::encode(&tile) {
                Ok(d) => (ENC_PALETTE, d),
                Err(_) => (ENC_RAW, tile.to_vec()),
            };
            session.send(&ScreenFrame::Tile { col, row, encoding, data }.encode())?;
        }
    }

    session.send(&ScreenFrame::End.encode())?;
    Ok(())
}

/// Reassembles a screen frame from incoming [`ScreenFrame`]s.
#[derive(Default)]
pub struct ScreenReceiver {
    width: u32,
    height: u32,
    fb: Vec<u8>,
    tiles_applied: usize,
}

impl ScreenReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one frame. On [`ScreenFrame::End`], returns the completed
    /// framebuffer (BGRA8, `width × height × 4`).
    pub fn apply(&mut self, frame: ScreenFrame) -> Result<Option<Vec<u8>>, ScreenError> {
        match frame {
            ScreenFrame::Begin { width, height } => {
                self.width = width;
                self.height = height;
                self.fb = vec![0u8; (width * height * 4) as usize];
                self.tiles_applied = 0;
                Ok(None)
            }
            ScreenFrame::Tile { col, row, encoding, data } => {
                if self.fb.is_empty() {
                    return Err(ScreenError::NoBegin);
                }
                let tile = match encoding {
                    ENC_PALETTE => {
                        PaletteTileDecoder::decode(&data).map_err(|_| ScreenError::BadTile)?
                    }
                    ENC_RAW => {
                        if data.len() != TILE_BYTES {
                            return Err(ScreenError::BadTile);
                        }
                        data
                    }
                    other => return Err(ScreenError::UnknownEncoding(other)),
                };
                self.blit(col, row, &tile);
                self.tiles_applied += 1;
                Ok(None)
            }
            ScreenFrame::End => Ok(Some(std::mem::take(&mut self.fb))),
        }
    }

    /// Number of tiles applied since the last `Begin` (test/telemetry aid).
    #[allow(dead_code)]
    pub fn tiles_applied(&self) -> usize {
        self.tiles_applied
    }

    /// Place a decoded 32×32 tile into the framebuffer at (col, row), clipping
    /// partial tiles at the right/bottom edges.
    fn blit(&mut self, col: u32, row: u32, tile: &[u8]) {
        let (tx, ty) = (col * TILE_SIZE_PX, row * TILE_SIZE_PX);
        for r in 0..TILE_SIZE_PX {
            let y = ty + r;
            if y >= self.height {
                break;
            }
            for c in 0..TILE_SIZE_PX {
                let x = tx + c;
                if x >= self.width {
                    break;
                }
                let src = ((r * TILE_SIZE_PX + c) * 4) as usize;
                let dst = ((y * self.width + x) * 4) as usize;
                self.fb[dst..dst + 4].copy_from_slice(&tile[src..src + 4]);
            }
        }
    }
}

/// Errors during screen-frame transfer.
#[derive(Debug)]
pub enum ScreenError {
    Session(lowband_crypto::SessionError),
    Truncated,
    TooLarge,
    UnknownKind(u8),
    UnknownEncoding(u8),
    /// The source buffer is smaller than width×height×4.
    ShortBuffer,
    /// A tile arrived before a Begin.
    NoBegin,
    /// A tile failed to decode.
    BadTile,
}

impl std::fmt::Display for ScreenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenError::Session(e) => write!(f, "screen session: {e}"),
            ScreenError::Truncated => f.write_str("screen frame truncated"),
            ScreenError::TooLarge => f.write_str("screen tile exceeds cap"),
            ScreenError::UnknownKind(k) => write!(f, "unknown screen frame kind {k:#04x}"),
            ScreenError::UnknownEncoding(e) => write!(f, "unknown tile encoding {e}"),
            ScreenError::ShortBuffer => f.write_str("source buffer smaller than frame"),
            ScreenError::NoBegin => f.write_str("tile received before begin"),
            ScreenError::BadTile => f.write_str("tile failed to decode"),
        }
    }
}

impl std::error::Error for ScreenError {}

impl From<lowband_crypto::SessionError> for ScreenError {
    fn from(e: lowband_crypto::SessionError) -> Self {
        ScreenError::Session(e)
    }
}

fn take_u16(rest: &mut &[u8]) -> Result<u16, ScreenError> {
    let (h, t) = rest.split_at_checked(2).ok_or(ScreenError::Truncated)?;
    *rest = t;
    Ok(u16::from_le_bytes([h[0], h[1]]))
}

/// A "terminal-like" text screen: black background, a few opaque colors — every
/// tile stays within the palette limit, so all tiles are lossless palette-coded
/// (the NFR-4 legibility case). Test-only helper shared with the inbound router
/// test.
#[cfg(test)]
pub(crate) fn text_screen(w: u32, h: u32) -> Vec<u8> {
    let mut fb = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 4) as usize;
            // A grid of "glyphs": white on black with a green cursor block.
            let text = (x / 4 + y / 8) % 3 == 0;
            let cursor = x < 8 && y < 16;
            let (b, g, r) =
                if cursor { (0, 200, 0) } else if text { (255, 255, 255) } else { (0, 0, 0) };
            fb[off] = b;
            fb[off + 1] = g;
            fb[off + 2] = r;
            fb[off + 3] = 0xFF; // opaque
        }
    }
    fb
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_crypto::StaticKeypair;
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Duration;

    /// A photographic tile region: a smooth gradient with > 16 colors, forcing
    /// the raw fallback.
    fn photo_screen(w: u32, h: u32) -> Vec<u8> {
        let mut fb = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                fb[off] = (x * 7) as u8;
                fb[off + 1] = (y * 5) as u8;
                fb[off + 2] = (x + y) as u8;
                fb[off + 3] = 0xFF;
            }
        }
        fb
    }

    #[test]
    fn frames_roundtrip() {
        for f in [
            ScreenFrame::Begin { width: 128, height: 64 },
            ScreenFrame::Tile { col: 1, row: 2, encoding: ENC_PALETTE, data: vec![1, 2, 3] },
            ScreenFrame::End,
        ] {
            assert_eq!(ScreenFrame::decode(&f.encode()).unwrap(), f);
        }
    }

    #[test]
    fn text_frame_is_lossless_in_memory() {
        let (w, h) = (80, 48);
        let src = text_screen(w, h);
        // Drive the encoder/receiver directly (no socket).
        let grid = TileGrid::new(w, h);
        let mut rx = ScreenReceiver::new();
        rx.apply(ScreenFrame::Begin { width: w, height: h }).unwrap();
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let tile = grid.extract_tile(&src, w * 4, TileCoord { col, row });
                let (encoding, data) = match PaletteTileEncoder::encode(&tile) {
                    Ok(d) => (ENC_PALETTE, d),
                    Err(_) => (ENC_RAW, tile.to_vec()),
                };
                rx.apply(ScreenFrame::Tile { col, row, encoding, data }).unwrap();
            }
        }
        assert_eq!(rx.tiles_applied() as u32, grid.tile_count(), "all tiles applied");
        let out = rx.apply(ScreenFrame::End).unwrap().unwrap();
        assert_eq!(out, src, "text screen must round-trip pixel-perfect (NFR-4)");
    }

    #[test]
    fn photo_tiles_take_raw_path_and_still_round_trip() {
        let (w, h) = (64, 32);
        let src = photo_screen(w, h);
        let grid = TileGrid::new(w, h);
        let mut used_raw = false;
        let mut rx = ScreenReceiver::new();
        rx.apply(ScreenFrame::Begin { width: w, height: h }).unwrap();
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let tile = grid.extract_tile(&src, w * 4, TileCoord { col, row });
                let (encoding, data) = match PaletteTileEncoder::encode(&tile) {
                    Ok(d) => (ENC_PALETTE, d),
                    Err(_) => {
                        used_raw = true;
                        (ENC_RAW, tile.to_vec())
                    }
                };
                rx.apply(ScreenFrame::Tile { col, row, encoding, data }).unwrap();
            }
        }
        let out = rx.apply(ScreenFrame::End).unwrap().unwrap();
        assert!(used_raw, "gradient must exceed the palette color limit somewhere");
        assert_eq!(out, src, "raw fallback must also be lossless");
    }

    #[test]
    fn screen_frame_travels_over_real_session() {
        let (w, h) = (96, 64);
        let src = text_screen(w, h);
        let src2 = src.clone();

        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100001234";

        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();

        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut rx = ScreenReceiver::new();
            loop {
                let bytes = sess.recv().unwrap();
                let frame = ScreenFrame::decode(&bytes).unwrap();
                if let Some(fb) = rx.apply(frame).unwrap() {
                    return fb;
                }
            }
        });

        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();
        send_frame(&mut client, w, h, &src).unwrap();

        let received = server.join().unwrap();
        assert_eq!(received, src2, "screen arrived pixel-perfect over the E2EE session");
    }
}

//! zstd per-chunk compression — Features 106, 107, 108.
//!
//! Compression level is selected by the caller based on transfer priority:
//!
//! | Mode | Level | Use case |
//! |------|-------|----------|
//! | [`CompressionMode::Foreground`] | 3 | User is waiting; minimise latency |
//! | [`CompressionMode::Background`] | 19 | Async bulk; maximise ratio |
//!
//! An optional pre-trained zstd dictionary (Feature 108) substantially
//! improves compression on small IT payloads (logs, registry exports, configs)
//! where generic zstd would be trained on too little data to reach its
//! potential ratio.

use thiserror::Error;

/// Zstd compression level for foreground transfers (Feature 106).
pub const LEVEL_FOREGROUND: i32 = 3;
/// Zstd compression level for background transfers (Feature 107).
pub const LEVEL_BACKGROUND: i32 = 19;

/// Selects the compression level applied by [`XferCompressor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Level 3 — fast; use when the user is waiting on the transfer.
    Foreground,
    /// Level 19 — maximum ratio; use for background bulk transfers.
    Background,
}

impl CompressionMode {
    fn level(self) -> i32 {
        match self {
            Self::Foreground => LEVEL_FOREGROUND,
            Self::Background => LEVEL_BACKGROUND,
        }
    }
}

#[derive(Debug, Error)]
pub enum CompressError {
    #[error("zstd compression failed: {0}")]
    Zstd(#[from] std::io::Error),
}

/// Per-chunk zstd compressor with optional pre-trained dictionary.
///
/// Construct once per transfer session and reuse across chunks — the internal
/// `zstd::bulk::Compressor` amortises dictionary loading cost.
pub struct XferCompressor {
    mode: CompressionMode,
    dictionary: Option<Vec<u8>>,
}

impl XferCompressor {
    /// Create a compressor without a dictionary.
    pub fn new(mode: CompressionMode) -> Self {
        Self { mode, dictionary: None }
    }

    /// Create a compressor with a pre-trained dictionary (Feature 108).
    ///
    /// `dict_bytes` must be a valid zstd dictionary (produced offline by
    /// `zstd --train` over a corpus of representative IT payloads).
    pub fn with_dictionary(mode: CompressionMode, dict_bytes: Vec<u8>) -> Self {
        Self { mode, dictionary: Some(dict_bytes) }
    }

    /// Compress `data` and return the compressed bytes.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>, CompressError> {
        let level = self.mode.level();
        let compressed = match &self.dictionary {
            None => zstd::bulk::compress(data, level)?,
            Some(dict) => {
                let mut compressor =
                    zstd::bulk::Compressor::with_dictionary(level, dict)?;
                compressor.compress(data)?
            }
        };
        Ok(compressed)
    }

    pub fn mode(&self) -> CompressionMode {
        self.mode
    }
}

/// Decompress a chunk that was compressed by [`XferCompressor`].
pub fn decompress(data: &[u8], dictionary: Option<&[u8]>) -> Result<Vec<u8>, CompressError> {
    let result = match dictionary {
        None => zstd::bulk::decompress(data, 64 * 1024 * 1024)?,
        Some(dict) => {
            let mut decompressor = zstd::bulk::Decompressor::with_dictionary(dict)?;
            decompressor.decompress(data, 64 * 1024 * 1024)?
        }
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(mode: CompressionMode, payload: &[u8]) {
        let c = XferCompressor::new(mode);
        let compressed = c.compress(payload).expect("compress failed");
        let decompressed = decompress(&compressed, None).expect("decompress failed");
        assert_eq!(decompressed, payload);
    }

    #[test]
    fn foreground_roundtrip() {
        roundtrip(CompressionMode::Foreground, b"hello world! this is a test payload.");
    }

    #[test]
    fn background_roundtrip() {
        roundtrip(CompressionMode::Background, b"hello world! this is a test payload.");
    }

    #[test]
    fn foreground_level_is_3() {
        assert_eq!(CompressionMode::Foreground.level(), LEVEL_FOREGROUND);
    }

    #[test]
    fn background_level_is_19() {
        assert_eq!(CompressionMode::Background.level(), LEVEL_BACKGROUND);
    }

    #[test]
    fn compression_reduces_compressible_data() {
        let payload: Vec<u8> = b"aaaa".repeat(10_000);
        let c = XferCompressor::new(CompressionMode::Foreground);
        let compressed = c.compress(&payload).unwrap();
        assert!(
            compressed.len() < payload.len(),
            "expected compression; got {} -> {}",
            payload.len(),
            compressed.len()
        );
    }

    #[test]
    fn foreground_faster_than_background_on_large_payload() {
        use std::time::Instant;
        let payload: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024).collect();

        let fg = XferCompressor::new(CompressionMode::Foreground);
        let bg = XferCompressor::new(CompressionMode::Background);

        let t0 = Instant::now();
        fg.compress(&payload).unwrap();
        let fg_us = t0.elapsed().as_micros();

        let t1 = Instant::now();
        bg.compress(&payload).unwrap();
        let bg_us = t1.elapsed().as_micros();

        assert!(
            fg_us < bg_us,
            "foreground ({fg_us} µs) should be faster than background ({bg_us} µs)"
        );
    }
}

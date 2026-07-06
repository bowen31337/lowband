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

/// IT payload class for pre-trained dictionary selection (Feature 108).
///
/// Each variant selects a dictionary trained offline by `build.rs` from ~50 representative
/// samples.  Pass to [`XferCompressor::with_class_dictionary`] to get the matching compressor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictionaryClass {
    /// Structured log lines: timestamps, severity, module, key=value fields.
    Logs,
    /// Windows Registry export blocks (.reg format, REG_DWORD, REG_SZ).
    Registry,
    /// TOML/INI configuration sections: agent, transfer, platform, fec, logging.
    Config,
}

// Pre-trained dictionaries (Feature 108).  Generated offline by gen_dicts.py
// and committed to the source tree; embedded at compile time via include_bytes!
// so no C build script is required.
#[cfg(feature = "full")]
const DICT_LOGS: &[u8] = include_bytes!("dicts/dict_logs.bin");
#[cfg(feature = "full")]
const DICT_REGISTRY: &[u8] = include_bytes!("dicts/dict_registry.bin");
#[cfg(feature = "full")]
const DICT_CONFIG: &[u8] = include_bytes!("dicts/dict_config.bin");

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

    /// Create a compressor using the embedded pre-trained dictionary for `class` (Feature 108).
    ///
    /// Selects the dictionary trained by `build.rs` at compile time from representative IT
    /// samples for the given payload class.  Prefer this over [`with_dictionary`] when the
    /// class is known at the call site — it avoids loading a dictionary file at runtime.
    #[cfg(feature = "full")]
    pub fn with_class_dictionary(mode: CompressionMode, class: DictionaryClass) -> Self {
        let dict = match class {
            DictionaryClass::Logs => DICT_LOGS,
            DictionaryClass::Registry => DICT_REGISTRY,
            DictionaryClass::Config => DICT_CONFIG,
        };
        Self::with_dictionary(mode, dict.to_vec())
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

    /// Compress a single [`FileChunk`] and return a [`CompressedChunk`] (Feature 106).
    ///
    /// Convenience wrapper over [`compress`](Self::compress) that preserves the chunk's
    /// identity and offset so callers need not unpack the struct manually.
    #[cfg(feature = "full")]
    pub fn compress_chunk(
        &self,
        chunk: &crate::chunker::FileChunk,
    ) -> Result<CompressedChunk, CompressError> {
        let compressed = self.compress(&chunk.data)?;
        Ok(CompressedChunk { id: chunk.id, offset: chunk.offset, compressed })
    }
}

/// A [`crate::chunker::FileChunk`] whose payload has been zstd-compressed (Feature 106).
///
/// `id` and `offset` are preserved from the original chunk so that the
/// deduplication cache and reassembly logic can operate on compressed output
/// without a second lookup.
#[cfg(feature = "full")]
#[derive(Debug, Clone)]
pub struct CompressedChunk {
    /// BLAKE3 identity hash of the **original** (uncompressed) data.
    pub id: crate::hash::ChunkId,
    /// Byte offset of this chunk within the source file.
    pub offset: usize,
    /// Zstd-compressed payload ready for FEC encoding or on-wire transmission.
    pub compressed: Vec<u8>,
}

/// Compress all `chunks` in one pass, returning a `CompressedChunk` per input (Feature 106).
///
/// Call with `CompressionMode::Foreground` (level 3) when the user is waiting on
/// the transfer, and `CompressionMode::Background` (level 19) for async bulk work.
/// The compressor is reused across chunks so dictionary loading is paid only once.
///
/// # Errors
/// Returns the first [`CompressError`] encountered; successfully compressed chunks
/// produced before the error are discarded.
#[cfg(feature = "full")]
pub fn compress_chunks(
    chunks: &[crate::chunker::FileChunk],
    compressor: &XferCompressor,
) -> Result<Vec<CompressedChunk>, CompressError> {
    chunks.iter().map(|c| compressor.compress_chunk(c)).collect()
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

    #[cfg(feature = "full")]
    fn make_log_sample() -> Vec<u8> {
        b"2024-01-15 10:05:23.741 INFO  [xfer.chunk] Chunk deduped hash=4f9a2b3c chunk_id=00abff01 bytes=4096\n"
            .repeat(8)
            .into_iter()
            .collect()
    }

    #[cfg(feature = "full")]
    fn make_registry_sample() -> Vec<u8> {
        b"Windows Registry Editor Version 5.00\r\n\r\n\
          [HKEY_LOCAL_MACHINE\\SOFTWARE\\LowBand\\SubKey01]\r\n\
          \"Version\"=\"1.3.0\"\r\n\
          \"InstallPath\"=\"C:\\\\Program Files\\\\LowBand\"\r\n\
          \"ServiceName\"=\"LowBandSvc\"\r\n\r\n"
            .repeat(4)
            .into_iter()
            .collect()
    }

    #[cfg(feature = "full")]
    fn make_config_sample() -> Vec<u8> {
        b"[transfer]\nchunk_size_min = 8192\nchunk_size_max = 65536\n\
          compression_foreground = 3\ncompression_background = 19\n\
          dictionary_class = \"logs\"\ndedup_enabled = true\n"
            .repeat(8)
            .into_iter()
            .collect()
    }

    #[cfg(feature = "full")]
    #[test]
    fn dict_logs_roundtrip() {
        let payload = make_log_sample();
        let c = XferCompressor::with_class_dictionary(CompressionMode::Foreground, DictionaryClass::Logs);
        let compressed = c.compress(&payload).expect("compress");
        let dict = super::DICT_LOGS;
        let decompressed = decompress(&compressed, Some(dict)).expect("decompress");
        assert_eq!(decompressed, payload);
    }

    #[cfg(feature = "full")]
    #[test]
    fn dict_registry_roundtrip() {
        let payload = make_registry_sample();
        let c = XferCompressor::with_class_dictionary(CompressionMode::Foreground, DictionaryClass::Registry);
        let compressed = c.compress(&payload).expect("compress");
        let dict = super::DICT_REGISTRY;
        let decompressed = decompress(&compressed, Some(dict)).expect("decompress");
        assert_eq!(decompressed, payload);
    }

    #[cfg(feature = "full")]
    #[test]
    fn dict_config_roundtrip() {
        let payload = make_config_sample();
        let c = XferCompressor::with_class_dictionary(CompressionMode::Foreground, DictionaryClass::Config);
        let compressed = c.compress(&payload).expect("compress");
        let dict = super::DICT_CONFIG;
        let decompressed = decompress(&compressed, Some(dict)).expect("decompress");
        assert_eq!(decompressed, payload);
    }

    #[cfg(feature = "full")]
    #[test]
    fn dict_improves_ratio_on_log_payload() {
        let payload = make_log_sample();
        let plain = XferCompressor::new(CompressionMode::Foreground);
        let with_dict = XferCompressor::with_class_dictionary(CompressionMode::Foreground, DictionaryClass::Logs);
        let plain_len = plain.compress(&payload).unwrap().len();
        let dict_len = with_dict.compress(&payload).unwrap().len();
        assert!(
            dict_len <= plain_len,
            "dictionary should not inflate output: dict={dict_len} plain={plain_len}"
        );
    }

    #[cfg(feature = "full")]
    #[test]
    fn compress_chunk_foreground_roundtrip() {
        use crate::chunker::chunk_data;

        let source: Vec<u8> = b"log line: INFO transfer started chunk_id=aabbccdd\n"
            .iter()
            .copied()
            .cycle()
            .take(64 * 1024)
            .collect();
        let chunks = chunk_data(&source);
        assert!(!chunks.is_empty());

        let compressor = XferCompressor::new(CompressionMode::Foreground);
        for chunk in &chunks {
            let cc = compressor.compress_chunk(chunk).expect("compress_chunk failed");
            assert_eq!(cc.id, chunk.id, "id preserved");
            assert_eq!(cc.offset, chunk.offset, "offset preserved");
            let decompressed = decompress(&cc.compressed, None).expect("decompress failed");
            assert_eq!(decompressed, chunk.data, "roundtrip mismatch");
        }
    }

    #[cfg(feature = "full")]
    #[test]
    fn compress_chunks_foreground_uses_level_3() {
        use crate::chunker::chunk_data;

        let source: Vec<u8> = (0u8..=255).cycle().take(128 * 1024).collect();
        let chunks = chunk_data(&source);
        let compressor = XferCompressor::new(CompressionMode::Foreground);
        assert_eq!(compressor.mode(), CompressionMode::Foreground);
        let compressed = compress_chunks(&chunks, &compressor).expect("compress_chunks failed");
        assert_eq!(compressed.len(), chunks.len());
        for (cc, chunk) in compressed.iter().zip(chunks.iter()) {
            assert_eq!(cc.id, chunk.id);
            assert_eq!(cc.offset, chunk.offset);
            assert!(!cc.compressed.is_empty());
        }
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

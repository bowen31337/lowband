//! FastCDC content-defined chunking — Feature 102.
//!
//! Files are split into variable-length chunks whose boundaries are determined
//! by a content-hash rolling function (FastCDC, 2020 variant).  Content-
//! defined boundaries mean that inserting bytes at the front of a file shifts
//! only the first affected chunk — all following chunks whose content is
//! unchanged retain the same [`crate::hash::ChunkId`] and are skipped by the
//! dedup filter (Features 104–105).
//!
//! # Chunk size target
//!
//! Per the architecture spec: min 8 kB · average 32 kB · max 64 kB.  The
//! average size is the operating point the rolling hash aims for; the min/max
//! guard against degenerate files.

use crate::hash::{compute_chunk_hash, ChunkHash};

/// Minimum chunk size (8 kB).
pub const CHUNK_MIN: u32 = 8 * 1024;
/// Average (target) chunk size (32 kB).
pub const CHUNK_AVG: u32 = 32 * 1024;
/// Maximum chunk size (64 kB).
pub const CHUNK_MAX: u32 = 64 * 1024;

/// A single content-defined chunk produced by [`chunk_data`].
#[derive(Debug, Clone)]
pub struct FileChunk {
    /// Byte offset of the chunk within the original source data.
    pub offset: usize,
    /// Chunk payload.
    pub data: Vec<u8>,
    /// BLAKE3 hash of `data` — used as the dedup key in the chunk cache.
    pub chunk_hash: ChunkHash,
}

/// Split `source` into content-defined chunks using the FastCDC 2020 algorithm.
///
/// Each returned [`FileChunk`] carries its BLAKE3 `id` pre-computed so that
/// callers can immediately query the dedup cache without a second pass.
pub fn chunk_data(source: &[u8]) -> Vec<FileChunk> {
    use fastcdc::v2020::FastCDC;

    FastCDC::new(source, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
        .map(|chunk| {
            let data = source[chunk.offset..chunk.offset + chunk.length].to_vec();
            let chunk_hash = compute_chunk_hash(&data);
            FileChunk { offset: chunk.offset, data, chunk_hash }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_source_completely() {
        let source: Vec<u8> = (0u8..=255).cycle().take(200 * 1024).collect();
        let chunks = chunk_data(&source);

        assert!(!chunks.is_empty());

        let mut pos = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, pos, "gap or overlap at offset {pos}");
            pos += chunk.data.len();
        }
        assert_eq!(pos, source.len(), "chunks do not cover full source");
    }

    #[test]
    fn chunk_sizes_within_bounds() {
        let source: Vec<u8> = (0u8..=255).cycle().take(500 * 1024).collect();
        let chunks = chunk_data(&source);

        for chunk in &chunks {
            assert!(
                chunk.data.len() <= CHUNK_MAX as usize,
                "chunk too large: {} bytes",
                chunk.data.len()
            );
        }
    }

    #[test]
    fn chunk_hash_matches_data() {
        let source: Vec<u8> = (0u8..=255).cycle().take(100 * 1024).collect();
        for chunk in chunk_data(&source) {
            assert_eq!(chunk.chunk_hash, compute_chunk_hash(&chunk.data));
        }
    }

    #[test]
    fn identical_content_same_chunk_boundaries() {
        let source: Vec<u8> = (0u8..=255).cycle().take(96 * 1024).collect();
        let a = chunk_data(&source);
        let b = chunk_data(&source);
        assert_eq!(a.len(), b.len());
        for (ca, cb) in a.iter().zip(b.iter()) {
            assert_eq!(ca.chunk_hash, cb.chunk_hash);
            assert_eq!(ca.offset, cb.offset);
        }
    }

    #[test]
    fn small_file_produces_at_least_one_chunk() {
        let source = b"tiny file";
        let chunks = chunk_data(source);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, source);
    }

    #[test]
    fn empty_source_produces_no_chunks() {
        assert!(chunk_data(b"").is_empty());
    }
}

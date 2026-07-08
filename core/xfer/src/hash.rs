//! BLAKE3 chunk identity hashing — Feature 103.
//!
//! Each FastCDC chunk is identified by its BLAKE3 hash (`chunk_hash`).  This
//! 32-byte digest is used as the key in the per-peer dedup index
//! ([`crate::cache`]) so that previously transferred chunks can be omitted
//! from retransmissions.

/// 32-byte BLAKE3 digest that uniquely identifies a chunk's content.
pub type ChunkHash = [u8; 32];

/// Compute the BLAKE3 [`ChunkHash`] for a chunk payload.
///
/// This is the only correct way to produce a `ChunkHash` — all code that
/// generates hashes for insertion into the chunk cache must call this function.
#[cfg(feature = "hash")]
#[inline]
pub fn compute_chunk_hash(data: &[u8]) -> ChunkHash {
    *blake3::hash(data).as_bytes()
}

#[cfg(all(test, feature = "hash"))]
mod tests {
    use super::*;

    #[test]
    fn same_data_same_hash() {
        let a = compute_chunk_hash(b"hello world");
        let b = compute_chunk_hash(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn different_data_different_hash() {
        let a = compute_chunk_hash(b"hello world");
        let b = compute_chunk_hash(b"hello WORLD");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_is_32_bytes() {
        let h = compute_chunk_hash(b"test");
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn empty_slice_has_stable_hash() {
        let h = compute_chunk_hash(b"");
        // BLAKE3 of empty input is deterministic and non-zero.
        assert_ne!(h, [0u8; 32]);
    }
}

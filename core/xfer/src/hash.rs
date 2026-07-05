//! BLAKE3 chunk identity hashing — Feature 103.
//!
//! Each FastCDC chunk is identified by its BLAKE3 hash.  This 32-byte digest
//! is used as the key in the per-peer dedup index ([`crate::cache`]) so that
//! previously transferred chunks can be omitted from retransmissions.

/// 32-byte BLAKE3 digest that uniquely identifies a chunk's content.
pub type ChunkId = [u8; 32];

/// Compute the BLAKE3 [`ChunkId`] for a chunk payload.
///
/// This is the only correct way to produce a `ChunkId` — all code that
/// generates IDs for insertion into the chunk cache must call this function.
#[cfg(feature = "full")]
#[inline]
pub fn compute_id(data: &[u8]) -> ChunkId {
    *blake3::hash(data).as_bytes()
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;

    #[test]
    fn same_data_same_id() {
        let id_a = compute_id(b"hello world");
        let id_b = compute_id(b"hello world");
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn different_data_different_id() {
        let id_a = compute_id(b"hello world");
        let id_b = compute_id(b"hello WORLD");
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn id_is_32_bytes() {
        let id = compute_id(b"test");
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn empty_slice_has_stable_id() {
        let id = compute_id(b"");
        // BLAKE3 of empty input is deterministic.
        assert_ne!(id, [0u8; 32]);
    }
}

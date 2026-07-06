//! LowBand file-transfer plugin (`xfer`).
//!
//! Implements features 102–111 of the LowBand architecture spec:
//!
//! | # | Feature |
//! |---|---------|
//! | 102 | FastCDC content-defined chunking (8–64 kB) |
//! | 103 | BLAKE3 chunk identity hashing for deduplication |
//! | 104 | Per-peer persistent chunk_cache dedup index |
//! | 105 | Delta transfer — metadata + new chunks only |
//! | 106 | zstd level 3 for foreground (user-waiting) transfers |
//! | 107 | zstd level 19 for background transfers |
//! | 108 | Pre-trained zstd dictionary for IT payload classes |
//! | 109 | RaptorQ fountain codes on channel 7 until ACK |
//! | 110 | Strict governor headroom_budget enforcement |
//! | 111 | bulk_transfer held — never queues ahead of voice or input |
//!
//! The pacing invariant (Feature 111) is the load-bearing constraint of this
//! module: bulk traffic on LBTP channel 7 must never create queuing delay for
//! voice (channel 1) or input (channel 3), even by one packet.

pub mod cache;
pub mod hash;
pub mod persistent_cache;
pub mod scheduler;

#[cfg(feature = "full")]
pub mod chunker;
#[cfg(feature = "full")]
pub mod compress;
#[cfg(any(feature = "full", feature = "fec"))]
pub mod fec;

pub use cache::{
    confirm_delivered, split_delta, ChunkCache, InMemoryChunkCache, TransferManifest,
};
pub use persistent_cache::PersistentChunkCache;
pub use hash::ChunkId;
pub use scheduler::{
    AggregatedDatagram, BulkTransferScheduler, PacerDemand, TickResult, XferFrame,
    MAX_DATAGRAM_XFER_BYTES,
};

#[cfg(feature = "full")]
pub use chunker::{chunk_data, FileChunk};
#[cfg(feature = "full")]
pub use compress::{
    compress_chunks, CompressedChunk, CompressError, CompressionMode, DictionaryClass,
    XferCompressor,
};
#[cfg(feature = "full")]
pub use compress::decompress as decompress_chunk;
#[cfg(any(feature = "full", feature = "fec"))]
pub use fec::{FecDecoder, FecEncoder, RepairSender};
#[cfg(feature = "full")]
pub use hash::compute_id;

// Feature 108 — validate the committed pre-trained dictionary artifacts.
// These tests require no C deps and run in all feature configurations.
#[cfg(test)]
mod dict_tests {
    const ZSTD_DICT_MAGIC: [u8; 4] = [0x37, 0xa4, 0x30, 0xec]; // 0xEC30A437 stored LE

    #[test]
    fn dict_logs_is_valid_zstd_dict() {
        let b = include_bytes!("dicts/dict_logs.bin");
        assert!(b.len() >= 8, "dict_logs.bin too small ({} bytes)", b.len());
        assert!(b.starts_with(&ZSTD_DICT_MAGIC), "dict_logs.bin: bad magic");
    }

    #[test]
    fn dict_registry_is_valid_zstd_dict() {
        let b = include_bytes!("dicts/dict_registry.bin");
        assert!(b.len() >= 8, "dict_registry.bin too small ({} bytes)", b.len());
        assert!(b.starts_with(&ZSTD_DICT_MAGIC), "dict_registry.bin: bad magic");
    }

    #[test]
    fn dict_config_is_valid_zstd_dict() {
        let b = include_bytes!("dicts/dict_config.bin");
        assert!(b.len() >= 8, "dict_config.bin too small ({} bytes)", b.len());
        assert!(b.starts_with(&ZSTD_DICT_MAGIC), "dict_config.bin: bad magic");
    }
}

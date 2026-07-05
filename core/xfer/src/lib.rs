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
pub mod scheduler;

#[cfg(feature = "full")]
pub mod chunker;
#[cfg(feature = "full")]
pub mod compress;
#[cfg(feature = "full")]
pub mod fec;

pub use cache::{ChunkCache, InMemoryChunkCache};
pub use hash::ChunkId;
pub use scheduler::{BulkTransferScheduler, PacerDemand, TickResult, XferFrame};

#[cfg(feature = "full")]
pub use chunker::{chunk_data, FileChunk};
#[cfg(feature = "full")]
pub use compress::{CompressionMode, XferCompressor};
#[cfg(feature = "full")]
pub use fec::{FecDecoder, FecEncoder};
#[cfg(feature = "full")]
pub use hash::compute_id;

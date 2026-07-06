//! Per-peer chunk dedup index — Features 104 & 105.
//!
//! The chunk cache records which [`ChunkId`]s a remote peer has already
//! received.  Before transmitting a chunk, the sender queries the cache; a
//! cache hit means only metadata (the `ChunkId`) needs to travel over the
//! wire — the remote can reconstruct the chunk from its local store.
//!
//! # Persistence
//!
//! In production this cache is backed by the `chunk_cache` SQLite table
//! (keyed on `peer_id` + `chunk_id`).  This module defines the [`ChunkCache`]
//! trait so the scheduler and pipeline code depend only on the abstraction;
//! the SQLite implementation lives in the host daemon's `obs/` / storage
//! layer and is injected at startup.  The [`InMemoryChunkCache`] provided
//! here is used in tests and during a session before the first flush.

use crate::hash::ChunkId;
use std::collections::HashSet;

/// Determines which chunks must be sent and which can be elided (Feature 105).
pub trait ChunkCache {
    /// Returns `true` if the remote peer already holds this chunk.
    fn contains(&self, id: &ChunkId) -> bool;

    /// Record that `id` has been successfully delivered to the remote peer.
    fn insert(&mut self, id: ChunkId);

    /// Remove a chunk from the cache (e.g. after the peer signals eviction).
    fn remove(&mut self, id: &ChunkId);
}

/// In-process chunk dedup index backed by a [`HashSet`].
///
/// Used for testing and as the session-local cache before SQLite is available.
#[derive(Debug, Default)]
pub struct InMemoryChunkCache {
    known: HashSet<ChunkId>,
}

impl InMemoryChunkCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

impl ChunkCache for InMemoryChunkCache {
    fn contains(&self, id: &ChunkId) -> bool {
        self.known.contains(id)
    }

    fn insert(&mut self, id: ChunkId) {
        self.known.insert(id);
    }

    fn remove(&mut self, id: &ChunkId) {
        self.known.remove(id);
    }
}

/// Decide which chunks from `chunks` need to be transmitted.
///
/// Returns a pair of vecs:
/// - `to_send`: chunk IDs whose data must be sent (cache miss).
/// - `already_cached`: chunk IDs the peer already has (elided).
///
/// Implements Feature 105: "send only metadata and deltas for a previously
/// cached payload."
pub fn split_delta<'a>(
    chunks: impl Iterator<Item = &'a ChunkId>,
    cache: &dyn ChunkCache,
) -> (Vec<ChunkId>, Vec<ChunkId>) {
    let mut to_send = Vec::new();
    let mut already_cached = Vec::new();

    for id in chunks {
        if cache.contains(id) {
            already_cached.push(*id);
        } else {
            to_send.push(*id);
        }
    }

    (to_send, already_cached)
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;

    fn id(byte: u8) -> ChunkId {
        let mut arr = [0u8; 32];
        arr[0] = byte;
        arr
    }

    #[test]
    fn empty_cache_misses_everything() {
        let cache = InMemoryChunkCache::new();
        assert!(!cache.contains(&id(1)));
    }

    #[test]
    fn insert_then_contains() {
        let mut cache = InMemoryChunkCache::new();
        let cid = id(2);
        cache.insert(cid);
        assert!(cache.contains(&cid));
    }

    #[test]
    fn remove_clears_entry() {
        let mut cache = InMemoryChunkCache::new();
        let cid = id(3);
        cache.insert(cid);
        cache.remove(&cid);
        assert!(!cache.contains(&cid));
    }

    #[test]
    fn split_delta_new_chunks_go_to_send() {
        let cache = InMemoryChunkCache::new();
        let ids = [id(10), id(11), id(12)];

        let (to_send, cached) = split_delta(ids.iter(), &cache);
        assert_eq!(to_send.len(), 3);
        assert!(cached.is_empty());
    }

    #[test]
    fn split_delta_known_chunks_elided() {
        let mut cache = InMemoryChunkCache::new();
        let known = id(20);
        let fresh = id(21);
        cache.insert(known);

        let ids = [known, fresh];
        let (to_send, cached) = split_delta(ids.iter(), &cache);

        assert_eq!(to_send, vec![fresh]);
        assert_eq!(cached, vec![known]);
    }

    #[test]
    fn split_delta_all_cached_sends_nothing() {
        let mut cache = InMemoryChunkCache::new();
        let ids: Vec<ChunkId> = (0u8..5).map(id).collect();
        for cid in &ids {
            cache.insert(*cid);
        }

        let (to_send, cached) = split_delta(ids.iter(), &cache);
        assert!(to_send.is_empty());
        assert_eq!(cached.len(), 5);
    }
}

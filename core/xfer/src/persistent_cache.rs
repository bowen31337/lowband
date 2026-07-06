//! File-backed [`ChunkCache`] — Feature 104 persistence layer.
//!
//! [`PersistentChunkCache`] stores chunk hashes in a per-peer write-ahead log
//! (WAL) file so the dedup index survives daemon restarts.  The in-memory
//! [`HashSet`] answers every lookup without touching disk; only mutations
//! (insert / remove) append a 33-byte entry to the log.
//!
//! # File format
//!
//! ```text
//! [1 byte tag: 0x01=insert | 0x00=remove][32 bytes BLAKE3 ChunkId]
//! ```
//!
//! Entries are replayed in order on open to reconstruct the live set.  A
//! partial entry at the end of the file (crash during write) is silently
//! dropped — the chunk is treated as unseen and will be re-sent once.
//!
//! # Compaction
//!
//! When the log contains more than `2 × |live set| + 64` entries, `open`
//! rewrites the file as a clean snapshot of the current live set, bounding
//! the file's growth over long-running sessions.
//!
//! # Per-peer isolation
//!
//! The caller (daemon) is responsible for deriving a unique path per remote
//! peer — typically `{cache_dir}/{peer_id_hex}.chunks`.  This crate does not
//! interpret the path; it only opens it.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::Path;

use crate::cache::ChunkCache;
use crate::hash::ChunkId;

const TAG_INSERT: u8 = 0x01;
const TAG_REMOVE: u8 = 0x00;
const ENTRY_LEN: usize = 33; // 1 tag + 32 hash bytes

/// File-backed per-peer chunk dedup index.
///
/// One instance is created per remote peer.  Construct with
/// [`PersistentChunkCache::open`] pointing at a peer-specific path.
pub struct PersistentChunkCache {
    known: HashSet<ChunkId>,
    file: File,
    log_entries: usize,
}

impl PersistentChunkCache {
    /// Open (or create) the WAL file at `path` and replay it into memory.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (known, log_entries) = Self::load(path)?;

        // Compact when the log has grown to more than twice the live set size.
        let compaction_threshold = known.len().saturating_mul(2).saturating_add(64);
        let (file, log_entries) = if log_entries > compaction_threshold {
            let f = Self::compact(path, &known)?;
            (f, known.len())
        } else {
            let f = OpenOptions::new().create(true).append(true).open(path)?;
            (f, log_entries)
        };

        Ok(Self { known, file, log_entries })
    }

    /// Number of chunk hashes currently held for this peer.
    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    // ── internal helpers ─────────────────────────────────────────────────────

    fn load(path: &Path) -> io::Result<(HashSet<ChunkId>, usize)> {
        let mut known = HashSet::new();
        let mut count = 0usize;

        if !path.exists() {
            return Ok((known, count));
        }

        let f = File::open(path)?;
        let mut reader = BufReader::new(f);
        let mut buf = [0u8; ENTRY_LEN];

        loop {
            match reader.read_exact(&mut buf) {
                Ok(()) => {
                    let id: ChunkId = buf[1..].try_into().expect("slice is 32 bytes");
                    if buf[0] == TAG_INSERT {
                        known.insert(id);
                    } else {
                        known.remove(&id);
                    }
                    count += 1;
                }
                // Reached end of file or found a partial entry (crash residue) — stop.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }

        Ok((known, count))
    }

    fn compact(path: &Path, live: &HashSet<ChunkId>) -> io::Result<File> {
        let mut file = File::create(path)?;
        let mut buf = [0u8; ENTRY_LEN];
        buf[0] = TAG_INSERT;
        for id in live {
            buf[1..].copy_from_slice(id);
            file.write_all(&buf)?;
        }
        file.flush()?;
        Ok(file)
    }

    fn append_entry(&mut self, tag: u8, id: &ChunkId) {
        let mut buf = [0u8; ENTRY_LEN];
        buf[0] = tag;
        buf[1..].copy_from_slice(id);
        let _ = self.file.write_all(&buf);
        let _ = self.file.flush();
        self.log_entries += 1;
    }
}

impl ChunkCache for PersistentChunkCache {
    fn contains(&self, id: &ChunkId) -> bool {
        self.known.contains(id)
    }

    fn insert(&mut self, id: ChunkId) {
        if self.known.insert(id) {
            self.append_entry(TAG_INSERT, &id);
        }
    }

    fn remove(&mut self, id: &ChunkId) {
        if self.known.remove(id) {
            self.append_entry(TAG_REMOVE, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{confirm_delivered, split_delta};
    use std::env;
    use std::path::PathBuf;

    fn chunk(b: u8) -> ChunkId {
        let mut id = [0u8; 32];
        id[0] = b;
        id
    }

    fn tmp_path(suffix: &str) -> PathBuf {
        env::temp_dir().join(format!("lowband_xfer_test_{}.chunks", suffix))
    }

    #[test]
    fn empty_cache_misses() {
        let path = tmp_path("empty");
        let _ = std::fs::remove_file(&path);
        let cache = PersistentChunkCache::open(&path).unwrap();
        assert!(!cache.contains(&chunk(42)));
        assert!(cache.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_then_contains() {
        let path = tmp_path("insert");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        cache.insert(chunk(7));
        assert!(cache.contains(&chunk(7)));
        assert_eq!(cache.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_clears_entry() {
        let path = tmp_path("remove");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        cache.insert(chunk(5));
        cache.remove(&chunk(5));
        assert!(!cache.contains(&chunk(5)));
        assert!(cache.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_is_idempotent() {
        let path = tmp_path("idem");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        cache.insert(chunk(3));
        cache.insert(chunk(3));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.log_entries, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let path = tmp_path("noop_rm");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        cache.remove(&chunk(99));
        assert!(cache.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn split_delta_with_persistent_cache() {
        let path = tmp_path("delta");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        let known = chunk(20);
        let fresh = chunk(21);
        cache.insert(known);

        let ids = [known, fresh];
        let (to_send, cached) = split_delta(ids.iter(), &cache);
        assert_eq!(to_send, vec![fresh]);
        assert_eq!(cached, vec![known]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn confirm_delivered_persists_in_memory() {
        let path = tmp_path("confirm");
        let _ = std::fs::remove_file(&path);
        let mut cache = PersistentChunkCache::open(&path).unwrap();
        let ids: Vec<ChunkId> = (0u8..4).map(chunk).collect();

        confirm_delivered(&mut cache, &ids);
        for id in &ids {
            assert!(cache.contains(id));
        }
        assert_eq!(cache.len(), 4);

        let (to_send, cached) = split_delta(ids.iter(), &cache);
        assert!(to_send.is_empty());
        assert_eq!(cached.len(), 4);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn data_survives_reopen() {
        let path = tmp_path("reopen");
        let _ = std::fs::remove_file(&path);
        let cid = chunk(77);

        {
            let mut cache = PersistentChunkCache::open(&path).unwrap();
            cache.insert(cid);
        }

        let cache = PersistentChunkCache::open(&path).unwrap();
        assert!(cache.contains(&cid), "hash must survive a reopen");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_survives_reopen() {
        let path = tmp_path("rm_reopen");
        let _ = std::fs::remove_file(&path);
        let cid = chunk(88);

        {
            let mut cache = PersistentChunkCache::open(&path).unwrap();
            cache.insert(cid);
            cache.remove(&cid);
        }

        let cache = PersistentChunkCache::open(&path).unwrap();
        assert!(!cache.contains(&cid), "removal must survive a reopen");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compaction_reduces_log_entries() {
        let path = tmp_path("compact");
        let _ = std::fs::remove_file(&path);

        // Insert 10 chunks, remove 9 of them — leaves 1 live but 19 log entries.
        {
            let mut cache = PersistentChunkCache::open(&path).unwrap();
            for b in 0u8..10 {
                cache.insert(chunk(b));
            }
            for b in 0u8..9 {
                cache.remove(&chunk(b));
            }
            assert_eq!(cache.log_entries, 19);
        }

        // Reopen forces compaction (19 > 2*1 + 64? no — threshold is 66 here).
        // Lower the threshold by having more removals than live entries.
        // Let's insert 100, remove 99 to get 199 entries with 1 live.
        let _ = std::fs::remove_file(&path);
        {
            let mut cache = PersistentChunkCache::open(&path).unwrap();
            for b in 0u8..100 {
                cache.insert(chunk(b));
            }
            for b in 0u8..99 {
                cache.remove(&chunk(b));
            }
            // 199 entries, 1 live. threshold = 2*1 + 64 = 66. 199 > 66 → compact on reopen.
        }

        let cache = PersistentChunkCache::open(&path).unwrap();
        assert!(cache.contains(&chunk(99)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.log_entries, 1, "log should be compacted to 1 entry");
        let _ = std::fs::remove_file(&path);
    }
}

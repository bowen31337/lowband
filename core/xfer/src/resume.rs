//! Transfer resume state — FR-6 "survives app restart".
//!
//! [`ResumableTransfer`] persists an in-flight object transfer (its
//! [`TransferManifest`] plus per-chunk delivery progress) to a state file so
//! a transfer interrupted by a daemon restart continues from where it
//! stopped instead of resending the whole object.  It complements the
//! per-peer dedup index ([`crate::PersistentChunkCache`]): the dedup cache
//! answers "has this peer ever held this chunk", this module answers "how
//! far did *this* transfer get".
//!
//! # File format
//!
//! ```text
//! header:   [4 bytes magic "LBXR"][1 byte version=1]
//!           [4 bytes LE chunk_count][8 bytes LE total_bytes]
//!           [chunk_count × 32-byte ChunkHash, file order]
//! progress: [4 bytes LE chunk index]*      (appended per delivered chunk)
//! ```
//!
//! Progress entries are replayed on open.  A partial trailing entry (crash
//! during write) is silently dropped — that chunk is re-sent once, matching
//! the [`crate::persistent_cache`] crash-residue policy.
//!
//! # Lifecycle
//!
//! The caller derives one state path per (peer, object) — typically
//! `{state_dir}/{peer_id_hex}/{object_hash_hex}.transfer`.  On completion
//! [`ResumableTransfer::finish`] removes the file; a fresh transfer of the
//! same object later starts a new state file.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use crate::cache::TransferManifest;
use crate::hash::ChunkHash;

const MAGIC: &[u8; 4] = b"LBXR";
const VERSION: u8 = 1;
const PROGRESS_ENTRY_LEN: usize = 4;

/// Outcome of [`ResumableTransfer::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// No usable prior state existed; the transfer starts from chunk zero.
    Fresh,
    /// Prior state matched the manifest; delivery continues mid-object.
    Resumed {
        /// Chunks already delivered in the previous run.
        delivered: usize,
    },
}

/// Persistent per-object transfer progress.
pub struct ResumableTransfer {
    manifest: TransferManifest,
    delivered: HashSet<u32>,
    file: File,
    path: PathBuf,
}

impl ResumableTransfer {
    /// Open the state file at `path` for `manifest`, resuming if a prior
    /// run left matching state behind.
    ///
    /// State is resumed only when the stored manifest is byte-identical to
    /// `manifest` (same chunk ids, order, and total size); any mismatch or
    /// corruption discards the stale file and starts fresh — never resumes
    /// into the wrong object.
    pub fn open(path: &Path, manifest: TransferManifest) -> io::Result<(Self, OpenOutcome)> {
        match Self::load(path, &manifest) {
            Ok(Some(delivered)) => {
                let file = OpenOptions::new().append(true).open(path)?;
                let outcome = OpenOutcome::Resumed { delivered: delivered.len() };
                Ok((Self { manifest, delivered, file, path: path.to_path_buf() }, outcome))
            }
            // Missing file, manifest mismatch, or corrupt header → fresh start.
            Ok(None) | Err(_) => {
                let file = Self::create(path, &manifest)?;
                let transfer = Self {
                    manifest,
                    delivered: HashSet::new(),
                    file,
                    path: path.to_path_buf(),
                };
                Ok((transfer, OpenOutcome::Fresh))
            }
        }
    }

    /// Record that the chunk at manifest position `index` reached the peer.
    ///
    /// Idempotent; out-of-range indices are ignored.
    pub fn mark_delivered(&mut self, index: usize) {
        if index >= self.manifest.chunk_count() {
            return;
        }
        let index = index as u32;
        if self.delivered.insert(index) {
            let _ = self.file.write_all(&index.to_le_bytes());
            let _ = self.file.flush();
        }
    }

    /// Manifest positions still awaiting delivery, in file order.
    pub fn remaining(&self) -> Vec<usize> {
        (0..self.manifest.chunk_count())
            .filter(|i| !self.delivered.contains(&(*i as u32)))
            .collect()
    }

    /// Number of chunks already delivered.
    pub fn delivered_count(&self) -> usize {
        self.delivered.len()
    }

    pub fn is_complete(&self) -> bool {
        self.delivered.len() == self.manifest.chunk_count()
    }

    /// Chunk ids still awaiting delivery (convenience over [`Self::remaining`]).
    pub fn remaining_ids(&self) -> Vec<ChunkHash> {
        self.remaining().iter().map(|&i| self.manifest.chunk_ids[i]).collect()
    }

    /// Complete the transfer: remove the state file.
    ///
    /// Returns an error if called before every chunk is delivered.
    pub fn finish(self) -> io::Result<()> {
        if !self.is_complete() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "finish() called with undelivered chunks",
            ));
        }
        drop(self.file);
        fs::remove_file(&self.path)
    }

    // ── internal helpers ─────────────────────────────────────────────────

    /// Returns `Some(delivered)` when `path` holds state for exactly
    /// `manifest`, `None` when the file is absent or belongs to a different
    /// manifest.  I/O and corruption errors bubble up (treated as fresh by
    /// the caller).
    fn load(path: &Path, manifest: &TransferManifest) -> io::Result<Option<HashSet<u32>>> {
        if !path.exists() {
            return Ok(None);
        }

        let mut reader = BufReader::new(File::open(path)?);

        let mut header = [0u8; 17];
        reader.read_exact(&mut header)?;
        if &header[..4] != MAGIC || header[4] != VERSION {
            return Ok(None);
        }
        let chunk_count = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        let total_bytes = u64::from_le_bytes(header[9..17].try_into().unwrap()) as usize;
        if chunk_count != manifest.chunk_count() || total_bytes != manifest.total_bytes {
            return Ok(None);
        }

        let mut id = [0u8; 32];
        for expected in &manifest.chunk_ids {
            reader.read_exact(&mut id)?;
            if &id != expected {
                return Ok(None);
            }
        }

        let mut delivered = HashSet::new();
        let mut entry = [0u8; PROGRESS_ENTRY_LEN];
        loop {
            match reader.read_exact(&mut entry) {
                Ok(()) => {
                    let index = u32::from_le_bytes(entry);
                    if (index as usize) < chunk_count {
                        delivered.insert(index);
                    }
                }
                // End of file or partial entry (crash residue) — stop.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }

        Ok(Some(delivered))
    }

    fn create(path: &Path, manifest: &TransferManifest) -> io::Result<File> {
        let mut file = File::create(path)?;
        file.write_all(MAGIC)?;
        file.write_all(&[VERSION])?;
        file.write_all(&(manifest.chunk_count() as u32).to_le_bytes())?;
        file.write_all(&(manifest.total_bytes as u64).to_le_bytes())?;
        for id in &manifest.chunk_ids {
            file.write_all(id)?;
        }
        file.flush()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn id(byte: u8) -> ChunkHash {
        let mut arr = [0u8; 32];
        arr[0] = byte;
        arr
    }

    fn manifest(n: u8) -> TransferManifest {
        TransferManifest::new((0..n).map(id).collect(), n as usize * 8192)
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("lbxr-test-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn fresh_transfer_reports_all_chunks_remaining() {
        let path = temp_path("fresh");
        let (t, outcome) = ResumableTransfer::open(&path, manifest(4)).unwrap();
        assert_eq!(outcome, OpenOutcome::Fresh);
        assert_eq!(t.remaining(), vec![0, 1, 2, 3]);
        assert!(!t.is_complete());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn restart_mid_transfer_resumes_from_recorded_progress() {
        let path = temp_path("resume");

        // First run: deliver chunks 0 and 2, then "crash" (drop).
        {
            let (mut t, _) = ResumableTransfer::open(&path, manifest(4)).unwrap();
            t.mark_delivered(0);
            t.mark_delivered(2);
        }

        // Second run: progress restored, only 1 and 3 remain.
        let (mut t, outcome) = ResumableTransfer::open(&path, manifest(4)).unwrap();
        assert_eq!(outcome, OpenOutcome::Resumed { delivered: 2 });
        assert_eq!(t.remaining(), vec![1, 3]);
        assert_eq!(t.remaining_ids(), vec![id(1), id(3)]);

        // Continue to completion; finish removes the state file.
        t.mark_delivered(1);
        t.mark_delivered(3);
        assert!(t.is_complete());
        t.finish().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn manifest_mismatch_discards_stale_state() {
        let path = temp_path("mismatch");
        {
            let (mut t, _) = ResumableTransfer::open(&path, manifest(4)).unwrap();
            t.mark_delivered(0);
        }
        // Same path, different object → fresh, no inherited progress.
        let (t, outcome) = ResumableTransfer::open(&path, manifest(5)).unwrap();
        assert_eq!(outcome, OpenOutcome::Fresh);
        assert_eq!(t.delivered_count(), 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn partial_trailing_progress_entry_is_dropped() {
        let path = temp_path("partial");
        {
            let (mut t, _) = ResumableTransfer::open(&path, manifest(3)).unwrap();
            t.mark_delivered(0);
        }
        // Simulate a crash mid-write: append 2 of 4 bytes of a progress entry.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0x01, 0x00]).unwrap();
        }
        let (t, outcome) = ResumableTransfer::open(&path, manifest(3)).unwrap();
        assert_eq!(outcome, OpenOutcome::Resumed { delivered: 1 });
        assert_eq!(t.remaining(), vec![1, 2]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mark_delivered_is_idempotent_and_bounds_checked() {
        let path = temp_path("idem");
        let (mut t, _) = ResumableTransfer::open(&path, manifest(2)).unwrap();
        t.mark_delivered(0);
        t.mark_delivered(0);
        t.mark_delivered(99); // out of range — ignored
        assert_eq!(t.delivered_count(), 1);

        // The duplicate must not have written a second entry: resume sees 1.
        drop(t);
        let (t, outcome) = ResumableTransfer::open(&path, manifest(2)).unwrap();
        assert_eq!(outcome, OpenOutcome::Resumed { delivered: 1 });
        assert_eq!(t.remaining(), vec![1]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn finish_refuses_incomplete_transfer() {
        let path = temp_path("finish-incomplete");
        let (mut t, _) = ResumableTransfer::open(&path, manifest(2)).unwrap();
        t.mark_delivered(0);
        assert!(t.finish().is_err());
        let _ = fs::remove_file(&path);
    }
}

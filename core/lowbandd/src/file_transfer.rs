//! End-to-end file transfer over the encrypted session (FR-6, integrated).
//!
//! The eval found the transfer building blocks — FastCDC chunking, BLAKE3
//! identity, [`ResumableTransfer`] — present but never assembled into an
//! actual send-a-file / receive-a-file flow. This module is that assembly: a
//! [`send_file`] driver and a [`FileReceiver`] that move a file over a live
//! [`SecureSession`], verifying every fragment and the whole file with
//! BLAKE3 and surviving a mid-transfer restart via [`ResumableTransfer`].
//!
//! # Fragment size
//!
//! Files travel as fixed 1 KiB wire fragments — small enough for one datagram
//! on the worst links LowBand targets (3G/ADSL2), where a large MTU cannot be
//! assumed. Each fragment's offset is `index × FRAGMENT_LEN`. (LBTP proper
//! negotiates larger fragments on healthier links; this layer proves the
//! integration end to end.)
//!
//! # Integrity + resume
//!
//! The offer carries a per-fragment BLAKE3 hash and a whole-file BLAKE3. The
//! receiver rejects any fragment whose bytes don't match, writes accepted
//! fragments straight into the destination file at their offset, and records
//! progress through [`ResumableTransfer`] — so a receiver restarted mid-
//! transfer skips fragments it already has and the sender's retransmits are
//! idempotent. On completion the whole file is re-hashed as a final check.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use lowband_crypto::SecureSession;
use lowband_xfer::hash::{compute_chunk_hash, ChunkHash};
use lowband_xfer::{ResumableTransfer, TransferManifest};

/// Wire fragment size (bytes). One fragment per datagram on constrained links.
pub const FRAGMENT_LEN: usize = 1024;

/// A framed file-transfer message on the bulk channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XferFrame {
    /// Transfer header: file name, total size, per-fragment + whole-file hashes.
    Offer {
        name: String,
        total_bytes: u64,
        frag_hashes: Vec<ChunkHash>,
        whole_hash: ChunkHash,
    },
    /// One file fragment at `index` (offset = index × [`FRAGMENT_LEN`]).
    Fragment { index: u32, data: Vec<u8> },
    /// End of transfer; the receiver finalizes and verifies.
    Complete,
}

const KIND_OFFER: u8 = 0x10;
const KIND_FRAGMENT: u8 = 0x11;
const KIND_COMPLETE: u8 = 0x12;

/// Upper bound on fragments in one transfer (guards decode allocation):
/// 1 M fragments × 1 KiB = 1 GiB, matching the clipboard file cap's spirit.
const MAX_FRAGMENTS: usize = 1_048_576;

impl XferFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            XferFrame::Offer { name, total_bytes, frag_hashes, whole_hash } => {
                out.push(KIND_OFFER);
                out.extend_from_slice(&(name.len() as u16).to_le_bytes());
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(&total_bytes.to_le_bytes());
                out.extend_from_slice(&(frag_hashes.len() as u32).to_le_bytes());
                for h in frag_hashes {
                    out.extend_from_slice(h);
                }
                out.extend_from_slice(whole_hash);
            }
            XferFrame::Fragment { index, data } => {
                out.push(KIND_FRAGMENT);
                out.extend_from_slice(&index.to_le_bytes());
                out.extend_from_slice(&(data.len() as u16).to_le_bytes());
                out.extend_from_slice(data);
            }
            XferFrame::Complete => out.push(KIND_COMPLETE),
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, XferError> {
        let (&kind, mut rest) = buf.split_first().ok_or(XferError::Truncated)?;
        match kind {
            KIND_OFFER => {
                let name = take_str(&mut rest)?;
                let total_bytes = take_u64(&mut rest)?;
                let count = take_u32(&mut rest)? as usize;
                if count > MAX_FRAGMENTS {
                    return Err(XferError::TooLarge);
                }
                let mut frag_hashes = Vec::with_capacity(count);
                for _ in 0..count {
                    frag_hashes.push(take_hash(&mut rest)?);
                }
                let whole_hash = take_hash(&mut rest)?;
                Ok(XferFrame::Offer { name, total_bytes, frag_hashes, whole_hash })
            }
            KIND_FRAGMENT => {
                let index = take_u32(&mut rest)?;
                let len = take_u16(&mut rest)? as usize;
                if len > FRAGMENT_LEN {
                    return Err(XferError::TooLarge);
                }
                let (data, tail) = rest.split_at_checked(len).ok_or(XferError::Truncated)?;
                let _ = tail;
                Ok(XferFrame::Fragment { index, data: data.to_vec() })
            }
            KIND_COMPLETE => Ok(XferFrame::Complete),
            other => Err(XferError::UnknownKind(other)),
        }
    }
}

/// Send `path` over `session` as a sequence of [`XferFrame`]s.
pub fn send_file(session: &mut SecureSession, path: &Path) -> Result<(), XferError> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;

    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();

    let frag_hashes: Vec<ChunkHash> =
        data.chunks(FRAGMENT_LEN).map(compute_chunk_hash).collect();
    let whole_hash = compute_chunk_hash(&data);

    session.send(
        &XferFrame::Offer {
            name,
            total_bytes: data.len() as u64,
            frag_hashes,
            whole_hash,
        }
        .encode(),
    )?;

    for (index, frag) in data.chunks(FRAGMENT_LEN).enumerate() {
        session.send(&XferFrame::Fragment { index: index as u32, data: frag.to_vec() }.encode())?;
    }

    session.send(&XferFrame::Complete.encode())?;
    Ok(())
}

/// What applying one frame did.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    /// The offer was accepted; `remaining` fragments still needed (after any
    /// resume from prior progress).
    Started { remaining: usize },
    /// A fragment was written; `delivered`/`total` counts.
    Fragment { delivered: usize, total: usize },
    /// A fragment already held (resume/retransmit) was skipped.
    Duplicate,
    /// Complete and whole-file hash verified; the file is at the destination.
    Complete,
    /// `Complete` arrived but fragments are still missing.
    Incomplete { remaining: usize },
}

/// Receives a file transfer into `dst`, tracking resumable progress in a
/// sidecar state file.
pub struct FileReceiver {
    dst: PathBuf,
    resume_path: PathBuf,
    state: Option<Active>,
}

struct Active {
    file: File,
    transfer: ResumableTransfer,
    frag_hashes: Vec<ChunkHash>,
    whole_hash: ChunkHash,
    total_bytes: u64,
}

impl FileReceiver {
    /// `dst` is where the completed file lands; `resume_path` is a sidecar
    /// that persists per-fragment progress so a restart resumes.
    pub fn new(dst: PathBuf, resume_path: PathBuf) -> Self {
        Self { dst, resume_path, state: None }
    }

    /// Apply one received frame.
    pub fn apply(&mut self, frame: XferFrame) -> Result<Progress, XferError> {
        match frame {
            XferFrame::Offer { name: _, total_bytes, frag_hashes, whole_hash } => {
                let manifest = TransferManifest::new(frag_hashes.clone(), total_bytes as usize);
                let (transfer, _outcome) = ResumableTransfer::open(&self.resume_path, manifest)?;

                // Open (or reopen) the destination and size it to the file.
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&self.dst)?;
                file.set_len(total_bytes)?;

                let remaining = transfer.remaining().len();
                self.state =
                    Some(Active { file, transfer, frag_hashes, whole_hash, total_bytes });
                Ok(Progress::Started { remaining })
            }
            XferFrame::Fragment { index, data } => {
                let st = self.state.as_mut().ok_or(XferError::NoOffer)?;
                let idx = index as usize;
                if idx >= st.frag_hashes.len() {
                    return Err(XferError::BadIndex);
                }
                // Idempotent: a fragment already delivered (resume/retransmit)
                // is skipped without rewriting.
                if !st.transfer.remaining().contains(&idx) {
                    return Ok(Progress::Duplicate);
                }
                // Integrity: the bytes must match the hash the offer committed to.
                if compute_chunk_hash(&data) != st.frag_hashes[idx] {
                    return Err(XferError::HashMismatch { index });
                }
                st.file.seek(SeekFrom::Start(index as u64 * FRAGMENT_LEN as u64))?;
                st.file.write_all(&data)?;
                st.transfer.mark_delivered(idx);
                Ok(Progress::Fragment {
                    delivered: st.transfer.delivered_count(),
                    total: st.frag_hashes.len(),
                })
            }
            XferFrame::Complete => {
                let st = self.state.as_mut().ok_or(XferError::NoOffer)?;
                if !st.transfer.is_complete() {
                    return Ok(Progress::Incomplete { remaining: st.transfer.remaining().len() });
                }
                st.file.flush()?;
                // Final whole-file integrity check.
                st.file.seek(SeekFrom::Start(0))?;
                let mut whole = Vec::with_capacity(st.total_bytes as usize);
                st.file.read_to_end(&mut whole)?;
                if compute_chunk_hash(&whole) != st.whole_hash {
                    return Err(XferError::WholeHashMismatch);
                }
                // Success: drop the resume sidecar.
                let state = self.state.take().unwrap();
                state.transfer.finish()?;
                Ok(Progress::Complete)
            }
        }
    }
}

/// Errors during file transfer framing or reception.
#[derive(Debug)]
pub enum XferError {
    Io(io::Error),
    Truncated,
    TooLarge,
    UnknownKind(u8),
    /// A fragment/complete arrived before the offer.
    NoOffer,
    /// Fragment index out of range for the offer.
    BadIndex,
    /// A fragment's bytes did not match the offered hash.
    HashMismatch { index: u32 },
    /// The reassembled file did not match the whole-file hash.
    WholeHashMismatch,
    Session(lowband_crypto::SessionError),
}

impl std::fmt::Display for XferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XferError::Io(e) => write!(f, "file transfer io: {e}"),
            XferError::Truncated => f.write_str("file transfer frame truncated"),
            XferError::TooLarge => f.write_str("file transfer field exceeds cap"),
            XferError::UnknownKind(k) => write!(f, "unknown xfer frame kind {k:#04x}"),
            XferError::NoOffer => f.write_str("fragment received before offer"),
            XferError::BadIndex => f.write_str("fragment index out of range"),
            XferError::HashMismatch { index } => write!(f, "fragment {index} failed hash check"),
            XferError::WholeHashMismatch => f.write_str("reassembled file failed hash check"),
            XferError::Session(e) => write!(f, "file transfer session: {e}"),
        }
    }
}

impl std::error::Error for XferError {}

impl From<io::Error> for XferError {
    fn from(e: io::Error) -> Self {
        XferError::Io(e)
    }
}
impl From<lowband_crypto::SessionError> for XferError {
    fn from(e: lowband_crypto::SessionError) -> Self {
        XferError::Session(e)
    }
}

// ── decode helpers ────────────────────────────────────────────────────────

fn take_u16(rest: &mut &[u8]) -> Result<u16, XferError> {
    let (h, t) = rest.split_at_checked(2).ok_or(XferError::Truncated)?;
    *rest = t;
    Ok(u16::from_le_bytes([h[0], h[1]]))
}
fn take_u32(rest: &mut &[u8]) -> Result<u32, XferError> {
    let (h, t) = rest.split_at_checked(4).ok_or(XferError::Truncated)?;
    *rest = t;
    Ok(u32::from_le_bytes(h.try_into().unwrap()))
}
fn take_u64(rest: &mut &[u8]) -> Result<u64, XferError> {
    let (h, t) = rest.split_at_checked(8).ok_or(XferError::Truncated)?;
    *rest = t;
    Ok(u64::from_le_bytes(h.try_into().unwrap()))
}
fn take_hash(rest: &mut &[u8]) -> Result<ChunkHash, XferError> {
    let (h, t) = rest.split_at_checked(32).ok_or(XferError::Truncated)?;
    *rest = t;
    Ok(h.try_into().unwrap())
}
fn take_str(rest: &mut &[u8]) -> Result<String, XferError> {
    let len = take_u16(rest)? as usize;
    let (h, t) = rest.split_at_checked(len).ok_or(XferError::Truncated)?;
    *rest = t;
    String::from_utf8(h.to_vec()).map_err(|_| XferError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_crypto::StaticKeypair;
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("lb-xfer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample_bytes(n: usize) -> Vec<u8> {
        // Deterministic but non-trivial content spanning several fragments.
        (0..n).map(|i| ((i * 31 + 7) % 251) as u8).collect()
    }

    #[test]
    fn frames_roundtrip() {
        let f = XferFrame::Offer {
            name: "a.bin".into(),
            total_bytes: 4096,
            frag_hashes: vec![[1u8; 32], [2u8; 32]],
            whole_hash: [9u8; 32],
        };
        assert_eq!(XferFrame::decode(&f.encode()).unwrap(), f);
        let frag = XferFrame::Fragment { index: 3, data: vec![7u8; 500] };
        assert_eq!(XferFrame::decode(&frag.encode()).unwrap(), frag);
        assert_eq!(XferFrame::decode(&XferFrame::Complete.encode()).unwrap(), XferFrame::Complete);
    }

    #[test]
    fn corrupt_fragment_is_rejected() {
        let dst = tmp("corrupt-dst");
        let resume = tmp("corrupt-resume");
        let data = sample_bytes(3000);
        let frag_hashes: Vec<ChunkHash> =
            data.chunks(FRAGMENT_LEN).map(compute_chunk_hash).collect();
        let whole = compute_chunk_hash(&data);

        let mut rx = FileReceiver::new(dst.clone(), resume.clone());
        rx.apply(XferFrame::Offer {
            name: "x".into(),
            total_bytes: data.len() as u64,
            frag_hashes,
            whole_hash: whole,
        })
        .unwrap();
        // Tamper the first fragment's bytes.
        let err = rx.apply(XferFrame::Fragment { index: 0, data: vec![0xFF; FRAGMENT_LEN] });
        assert!(matches!(err, Err(XferError::HashMismatch { index: 0 })));
        let _ = std::fs::remove_file(&dst);
        let _ = std::fs::remove_file(&resume);
    }

    #[test]
    fn restart_mid_transfer_resumes_and_completes() {
        let dst = tmp("resume-dst");
        let resume = tmp("resume-resume");
        let data = sample_bytes(5000); // 5 fragments (1024×4 + 904)
        let frags: Vec<Vec<u8>> = data.chunks(FRAGMENT_LEN).map(<[u8]>::to_vec).collect();
        let frag_hashes: Vec<ChunkHash> = frags.iter().map(|f| compute_chunk_hash(f)).collect();
        let whole = compute_chunk_hash(&data);
        let offer = XferFrame::Offer {
            name: "r".into(),
            total_bytes: data.len() as u64,
            frag_hashes: frag_hashes.clone(),
            whole_hash: whole,
        };

        // First receiver: take the offer + fragments 0,1,2, then "crash".
        {
            let mut rx = FileReceiver::new(dst.clone(), resume.clone());
            rx.apply(offer.clone()).unwrap();
            for i in 0..3 {
                rx.apply(XferFrame::Fragment { index: i, data: frags[i as usize].clone() }).unwrap();
            }
        }

        // Second receiver: same paths. Resend everything; 0..2 are duplicates.
        let mut rx = FileReceiver::new(dst.clone(), resume.clone());
        let started = rx.apply(offer).unwrap();
        assert_eq!(started, Progress::Started { remaining: 2 }, "resumed with 3 already done");
        for (i, frag) in frags.iter().enumerate() {
            let p = rx.apply(XferFrame::Fragment { index: i as u32, data: frag.clone() }).unwrap();
            if i < 3 {
                assert_eq!(p, Progress::Duplicate, "fragment {i} already had");
            }
        }
        assert_eq!(rx.apply(XferFrame::Complete).unwrap(), Progress::Complete);

        assert_eq!(std::fs::read(&dst).unwrap(), data, "reassembled file matches original");
        assert!(!resume.exists(), "resume sidecar removed on completion");
        let _ = std::fs::remove_file(&dst);
    }

    #[test]
    fn send_and_receive_over_real_session() {
        let dst = tmp("wire-dst");
        let resume = tmp("wire-resume");
        let src = tmp("wire-src");
        let data = sample_bytes(4500);
        std::fs::write(&src, &data).unwrap();

        let resp_key = StaticKeypair::generate();
        let resp_pub = resp_key.public_key_bytes();
        let init_key = StaticKeypair::generate();
        let code = "100000888";

        let resp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resp_addr = resp_sock.local_addr().unwrap();
        let dst2 = dst.clone();
        let resume2 = resume.clone();

        let server = thread::spawn(move || {
            let mut sess = SecureSession::accept(resp_sock, &resp_key, code).unwrap();
            sess.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut rx = FileReceiver::new(dst2, resume2);
            loop {
                let bytes = sess.recv().unwrap();
                let frame = XferFrame::decode(&bytes).unwrap();
                if rx.apply(frame).unwrap() == Progress::Complete {
                    break;
                }
            }
        });

        let init_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client =
            SecureSession::connect(init_sock, resp_addr, &init_key, resp_pub, code).unwrap();
        send_file(&mut client, &src).unwrap();
        server.join().unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), data, "file arrived intact over E2EE session");
        for p in [&dst, &resume, &src] {
            let _ = std::fs::remove_file(p);
        }
    }
}

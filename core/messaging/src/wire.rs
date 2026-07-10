//! Application message framing for the control/data plane.
//!
//! [`MessageFrame`] is the tagged envelope the daemon seals into the
//! [`SecureSession`](../../lowband_crypto/udp_session/struct.SecureSession.html)
//! and dispatches on receipt.  It carries the non-media application channels —
//! chat (FR-10), clipboard text and file offers (FR-9), and the panic notice
//! (FR-5) — over the same encrypted path, so each becomes a real peer-to-peer
//! exchange rather than a library type with no transport.
//!
//! # Wire format
//!
//! ```text
//! [1 byte kind][kind-specific payload]
//!   0x01 Chat            [u16 LE len][utf-8 text]
//!   0x02 ClipboardText   [u16 LE len][utf-8 text]
//!   0x03 ClipboardFiles  [u16 LE count]( [u16 LE name_len][name][u64 LE size] )*
//!   0x04 Panic           [4 bytes LE seq]   (see panic_key::PanicNotice)
//! ```
//!
//! Lengths are bounded by the per-channel caps (`CHAT_MAX_TEXT_BYTES`,
//! `CLIPBOARD_MAX_TEXT_BYTES`, `CLIPBOARD_MAX_FILES`); decode rejects anything
//! larger, so a peer cannot force an oversized allocation.

use crate::clipboard::{
    ClipboardFileEntry, ClipboardFileOffer, CLIPBOARD_MAX_FILES, CLIPBOARD_MAX_TEXT_BYTES,
};
use crate::chat::CHAT_MAX_TEXT_BYTES;
use crate::panic_key::PanicNotice;

const KIND_CHAT: u8 = 0x01;
const KIND_CLIPBOARD_TEXT: u8 = 0x02;
const KIND_CLIPBOARD_FILES: u8 = 0x03;
const KIND_PANIC: u8 = 0x04;

/// A decoded application message ready to dispatch to the owning subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageFrame {
    /// In-session chat text (FR-10).
    Chat(String),
    /// Remote clipboard text to apply under the clipboard grant (FR-9).
    ClipboardText(String),
    /// Remote clipboard file offer to gate + pull (FR-9 files).
    ClipboardFiles(ClipboardFileOffer),
    /// The peer's panic key fired (FR-5).
    Panic(PanicNotice),
}

/// Why a byte slice could not be decoded into a [`MessageFrame`].
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The frame was empty or a field was truncated.
    Truncated,
    /// The leading kind byte is not a known message type.
    UnknownKind(u8),
    /// A length field exceeded the channel's cap.
    TooLong,
    /// A text field was not valid UTF-8.
    BadUtf8,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Truncated => f.write_str("message frame truncated"),
            FrameError::UnknownKind(k) => write!(f, "unknown message kind {k:#04x}"),
            FrameError::TooLong => f.write_str("message frame field exceeds channel cap"),
            FrameError::BadUtf8 => f.write_str("message frame text is not valid utf-8"),
        }
    }
}

impl std::error::Error for FrameError {}

impl MessageFrame {
    /// Serialize into a self-describing byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            MessageFrame::Chat(text) => {
                out.push(KIND_CHAT);
                put_str(&mut out, text);
            }
            MessageFrame::ClipboardText(text) => {
                out.push(KIND_CLIPBOARD_TEXT);
                put_str(&mut out, text);
            }
            MessageFrame::ClipboardFiles(offer) => {
                out.push(KIND_CLIPBOARD_FILES);
                out.extend_from_slice(&(offer.entries.len() as u16).to_le_bytes());
                for e in &offer.entries {
                    put_str(&mut out, &e.name);
                    out.extend_from_slice(&e.size.to_le_bytes());
                }
            }
            MessageFrame::Panic(notice) => {
                out.push(KIND_PANIC);
                out.extend_from_slice(&notice.seq.to_le_bytes());
            }
        }
        out
    }

    /// Parse a frame produced by [`encode`](Self::encode).
    pub fn decode(buf: &[u8]) -> Result<Self, FrameError> {
        let (&kind, mut rest) = buf.split_first().ok_or(FrameError::Truncated)?;
        match kind {
            KIND_CHAT => {
                let text = take_str(&mut rest, CHAT_MAX_TEXT_BYTES)?;
                Ok(MessageFrame::Chat(text))
            }
            KIND_CLIPBOARD_TEXT => {
                let text = take_str(&mut rest, CLIPBOARD_MAX_TEXT_BYTES)?;
                Ok(MessageFrame::ClipboardText(text))
            }
            KIND_CLIPBOARD_FILES => {
                let count = take_u16(&mut rest)? as usize;
                if count > CLIPBOARD_MAX_FILES {
                    return Err(FrameError::TooLong);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    // File names use the clipboard text cap as a generous bound;
                    // safe_file_name (in clipboard.rs) does the real validation.
                    let name = take_str(&mut rest, CLIPBOARD_MAX_TEXT_BYTES)?;
                    let size = take_u64(&mut rest)?;
                    entries.push(ClipboardFileEntry { name, size });
                }
                Ok(MessageFrame::ClipboardFiles(ClipboardFileOffer { entries }))
            }
            KIND_PANIC => {
                let seq = take_u32(&mut rest)?;
                Ok(MessageFrame::Panic(PanicNotice { seq }))
            }
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

// ── encode helpers ────────────────────────────────────────────────────────

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

// ── decode helpers (advance the slice, bounds-checked) ────────────────────

fn take_u16(rest: &mut &[u8]) -> Result<u16, FrameError> {
    let (head, tail) = rest.split_at_checked(2).ok_or(FrameError::Truncated)?;
    *rest = tail;
    Ok(u16::from_le_bytes([head[0], head[1]]))
}

fn take_u32(rest: &mut &[u8]) -> Result<u32, FrameError> {
    let (head, tail) = rest.split_at_checked(4).ok_or(FrameError::Truncated)?;
    *rest = tail;
    Ok(u32::from_le_bytes(head.try_into().unwrap()))
}

fn take_u64(rest: &mut &[u8]) -> Result<u64, FrameError> {
    let (head, tail) = rest.split_at_checked(8).ok_or(FrameError::Truncated)?;
    *rest = tail;
    Ok(u64::from_le_bytes(head.try_into().unwrap()))
}

fn take_str(rest: &mut &[u8], cap: usize) -> Result<String, FrameError> {
    let len = take_u16(rest)? as usize;
    if len > cap {
        return Err(FrameError::TooLong);
    }
    let (head, tail) = rest.split_at_checked(len).ok_or(FrameError::Truncated)?;
    let s = std::str::from_utf8(head).map_err(|_| FrameError::BadUtf8)?.to_string();
    *rest = tail;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: MessageFrame) {
        assert_eq!(MessageFrame::decode(&f.encode()), Ok(f));
    }

    #[test]
    fn all_kinds_roundtrip() {
        roundtrip(MessageFrame::Chat("hello there".into()));
        roundtrip(MessageFrame::ClipboardText("copied text".into()));
        roundtrip(MessageFrame::Panic(PanicNotice { seq: 7 }));
        roundtrip(MessageFrame::ClipboardFiles(ClipboardFileOffer {
            entries: vec![
                ClipboardFileEntry { name: "a.txt".into(), size: 10 },
                ClipboardFileEntry { name: "b.bin".into(), size: 1 << 20 },
            ],
        }));
    }

    #[test]
    fn empty_and_unknown_are_errors() {
        assert_eq!(MessageFrame::decode(&[]), Err(FrameError::Truncated));
        assert_eq!(MessageFrame::decode(&[0xFF, 0, 0]), Err(FrameError::UnknownKind(0xFF)));
    }

    #[test]
    fn truncated_fields_are_rejected() {
        // KIND_CHAT with a length of 5 but no body.
        assert_eq!(
            MessageFrame::decode(&[KIND_CHAT, 5, 0]),
            Err(FrameError::Truncated)
        );
        // KIND_PANIC needs 4 bytes of seq.
        assert_eq!(MessageFrame::decode(&[KIND_PANIC, 1, 2]), Err(FrameError::Truncated));
    }

    #[test]
    fn oversized_length_field_rejected_before_allocation() {
        // Claim a chat length above the cap; decode must reject on the length,
        // not attempt to read it.
        let mut buf = vec![KIND_CHAT];
        buf.extend_from_slice(&((CHAT_MAX_TEXT_BYTES as u16 + 1).to_le_bytes()));
        assert_eq!(MessageFrame::decode(&buf), Err(FrameError::TooLong));

        // Claim a file count above the cap.
        let mut buf = vec![KIND_CLIPBOARD_FILES];
        buf.extend_from_slice(&((CLIPBOARD_MAX_FILES as u16 + 1).to_le_bytes()));
        assert_eq!(MessageFrame::decode(&buf), Err(FrameError::TooLong));
    }

    #[test]
    fn bad_utf8_rejected() {
        // KIND_CHAT, len 1, invalid byte 0xFF.
        assert_eq!(
            MessageFrame::decode(&[KIND_CHAT, 1, 0, 0xFF]),
            Err(FrameError::BadUtf8)
        );
    }
}

//! Reliable-channel wire framing for clipboard and chat — Feature 115.
//!
//! Every outgoing message is framed as:
//!
//! ```text
//! +---------+-----------+-------------------------+
//! | type(1) | length(4) | zstd-compressed body    |
//! +---------+-----------+-------------------------+
//! ```
//!
//! * `type` — `0x01` = Clipboard, `0x02` = Chat.
//! * `length` — big-endian `u32`; byte count of the compressed body that follows.
//! * body — the raw UTF-8 text compressed with zstd at level 3 (foreground latency).
//!
//! # Example
//!
//! ```
//! use lowband_messaging::channel::{ReliableChannel, Message};
//!
//! let ch = ReliableChannel::new();
//!
//! let frame = ch.encode_clipboard("copied text").unwrap();
//! assert!(matches!(ch.decode(&frame).unwrap(), Message::Clipboard(t) if t == "copied text"));
//!
//! let frame = ch.encode_chat("hello!").unwrap();
//! assert!(matches!(ch.decode(&frame).unwrap(), Message::Chat(t) if t == "hello!"));
//! ```

const TYPE_CLIPBOARD: u8 = 0x01;
const TYPE_CHAT: u8 = 0x02;
/// Zstd compression level — foreground (user-facing latency).
const ZSTD_LEVEL: i32 = 3;
/// Maximum decompressed body size accepted on decode (64 MiB).
const MAX_BODY_LEN: usize = 64 * 1024 * 1024;

/// A decoded message produced by [`ReliableChannel::decode`].
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    /// An incoming clipboard sync payload.
    Clipboard(String),
    /// An in-session chat message.
    Chat(String),
}

/// Errors returned by [`ReliableChannel`] framing operations.
#[derive(Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// The frame is shorter than the minimum 5-byte header.
    FrameTooShort,
    /// The `type` byte in the frame header is not recognised.
    UnknownType(u8),
    /// The declared body length in the header exceeds [`MAX_BODY_LEN`].
    BodyTooLarge(usize),
    /// The frame's actual byte count does not match the header length field.
    LengthMismatch { expected: usize, actual: usize },
    /// zstd compression or decompression failed.
    Zstd(String),
    /// The decompressed body is not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameTooShort => f.write_str("frame too short: must be at least 5 bytes"),
            Self::UnknownType(t) => write!(f, "unknown frame type: 0x{t:02x}"),
            Self::BodyTooLarge(n) => write!(f, "body length {n} exceeds limit"),
            Self::LengthMismatch { expected, actual } => {
                write!(f, "frame length mismatch: header says {expected}, got {actual}")
            }
            Self::Zstd(msg) => write!(f, "zstd error: {msg}"),
            Self::InvalidUtf8 => f.write_str("decompressed body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ChannelError {}

/// Wire framer for clipboard and chat payloads over the reliable channel.
///
/// Stateless — construct once and reuse freely across calls.
pub struct ReliableChannel;

impl ReliableChannel {
    pub fn new() -> Self {
        Self
    }

    /// Encode a clipboard text payload as a framed, zstd-compressed wire message.
    pub fn encode_clipboard(&self, text: &str) -> Result<Vec<u8>, ChannelError> {
        self.encode(TYPE_CLIPBOARD, text)
    }

    /// Encode a chat message as a framed, zstd-compressed wire message.
    pub fn encode_chat(&self, text: &str) -> Result<Vec<u8>, ChannelError> {
        self.encode(TYPE_CHAT, text)
    }

    /// Decode a framed wire message produced by [`encode_clipboard`] or [`encode_chat`].
    pub fn decode(&self, frame: &[u8]) -> Result<Message, ChannelError> {
        if frame.len() < 5 {
            return Err(ChannelError::FrameTooShort);
        }
        let msg_type = frame[0];
        let body_len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;

        if body_len > MAX_BODY_LEN {
            return Err(ChannelError::BodyTooLarge(body_len));
        }
        let payload = &frame[5..];
        if payload.len() != body_len {
            return Err(ChannelError::LengthMismatch {
                expected: body_len,
                actual: payload.len(),
            });
        }

        let decompressed = zstd::bulk::decompress(payload, MAX_BODY_LEN)
            .map_err(|e| ChannelError::Zstd(e.to_string()))?;
        let text =
            String::from_utf8(decompressed).map_err(|_| ChannelError::InvalidUtf8)?;

        match msg_type {
            TYPE_CLIPBOARD => Ok(Message::Clipboard(text)),
            TYPE_CHAT => Ok(Message::Chat(text)),
            t => Err(ChannelError::UnknownType(t)),
        }
    }

    fn encode(&self, msg_type: u8, text: &str) -> Result<Vec<u8>, ChannelError> {
        let compressed = zstd::bulk::compress(text.as_bytes(), ZSTD_LEVEL)
            .map_err(|e| ChannelError::Zstd(e.to_string()))?;

        let body_len = compressed.len();
        let mut frame = Vec::with_capacity(5 + body_len);
        frame.push(msg_type);
        frame.extend_from_slice(&(body_len as u32).to_be_bytes());
        frame.extend_from_slice(&compressed);
        Ok(frame)
    }
}

impl Default for ReliableChannel {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_roundtrip() {
        let ch = ReliableChannel::new();
        let frame = ch.encode_clipboard("hello clipboard").unwrap();
        assert_eq!(
            ch.decode(&frame).unwrap(),
            Message::Clipboard("hello clipboard".into()),
        );
    }

    #[test]
    fn chat_roundtrip() {
        let ch = ReliableChannel::new();
        let frame = ch.encode_chat("hey there!").unwrap();
        assert_eq!(
            ch.decode(&frame).unwrap(),
            Message::Chat("hey there!".into()),
        );
    }

    #[test]
    fn frame_has_correct_header() {
        let ch = ReliableChannel::new();
        let frame = ch.encode_clipboard("x").unwrap();
        assert_eq!(frame[0], TYPE_CLIPBOARD);
        let body_len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(frame.len(), 5 + body_len);
    }

    #[test]
    fn chat_type_byte_differs_from_clipboard() {
        let ch = ReliableChannel::new();
        let clip = ch.encode_clipboard("text").unwrap();
        let chat = ch.encode_chat("text").unwrap();
        assert_eq!(clip[0], TYPE_CLIPBOARD);
        assert_eq!(chat[0], TYPE_CHAT);
        assert_ne!(clip[0], chat[0]);
    }

    #[test]
    fn empty_string_roundtrip() {
        let ch = ReliableChannel::new();
        let frame = ch.encode_chat("").unwrap();
        assert_eq!(ch.decode(&frame).unwrap(), Message::Chat("".into()));
    }

    #[test]
    fn zstd_compresses_repetitive_text() {
        let ch = ReliableChannel::new();
        let text: String = "aaaa".repeat(1000);
        let frame = ch.encode_clipboard(&text).unwrap();
        // frame should be smaller than the raw text (5-byte header + compressed body)
        assert!(frame.len() < text.len(), "frame len {} vs text len {}", frame.len(), text.len());
    }

    #[test]
    fn decode_frame_too_short() {
        let ch = ReliableChannel::new();
        assert_eq!(ch.decode(&[0x01, 0x00, 0x00]), Err(ChannelError::FrameTooShort));
    }

    #[test]
    fn decode_unknown_type() {
        let ch = ReliableChannel::new();
        // Build a valid-looking frame with an unknown type byte
        let compressed = zstd::bulk::compress(b"test", ZSTD_LEVEL).unwrap();
        let mut frame = vec![0xFFu8];
        frame.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        frame.extend_from_slice(&compressed);
        assert_eq!(ch.decode(&frame), Err(ChannelError::UnknownType(0xFF)));
    }

    #[test]
    fn decode_length_mismatch() {
        let ch = ReliableChannel::new();
        let compressed = zstd::bulk::compress(b"data", ZSTD_LEVEL).unwrap();
        let mut frame = vec![TYPE_CHAT];
        // claim a different length than actual
        frame.extend_from_slice(&((compressed.len() + 10) as u32).to_be_bytes());
        frame.extend_from_slice(&compressed);
        assert!(matches!(ch.decode(&frame), Err(ChannelError::LengthMismatch { .. })));
    }

    #[test]
    fn unicode_roundtrip() {
        let ch = ReliableChannel::new();
        let text = "こんにちは 🌸 — émojis et accents";
        let frame = ch.encode_chat(text).unwrap();
        assert_eq!(ch.decode(&frame).unwrap(), Message::Chat(text.into()));
    }

    #[test]
    fn large_payload_roundtrip() {
        let ch = ReliableChannel::new();
        let text: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let frame = ch.encode_clipboard(&text).unwrap();
        assert_eq!(ch.decode(&frame).unwrap(), Message::Clipboard(text));
    }
}

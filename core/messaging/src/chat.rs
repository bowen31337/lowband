//! In-session text chat — Feature 114.
//!
//! Chat messages are sent on the reliable control channel and are never
//! dropped — they survive even at survival tier (48 kbps).  Unlike
//! [`ClipboardSession`], no capability grant is required: any session
//! participant may send a [`ChatMessage`] at any time.
//!
//! [`ClipboardSession`]: crate::clipboard::ClipboardSession
//!
//! # Survival-tier guarantee
//!
//! The survival tier floor is [`SURVIVAL_TIER_BPS`] (48 kbps = 6 000 B/s).
//! A max-length message ([`CHAT_MAX_TEXT_BYTES`] = 256 B) costs at most
//! 256 × 8 / 48 000 ≈ 43 ms of link time — well within any interactive
//! budget.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::chat::{ChatError, ChatSession, CHAT_MAX_TEXT_BYTES};
//!
//! let mut session = ChatSession::new();
//!
//! // No grant needed — any participant can chat.
//! let msg = session.send("hello from Tan").unwrap();
//! assert_eq!(msg.text, "hello from Tan");
//!
//! // Messages queue reliably until the transport drains them.
//! assert_eq!(session.outbox().len(), 1);
//!
//! // Text longer than the limit is rejected.
//! let too_long = "x".repeat(CHAT_MAX_TEXT_BYTES + 1);
//! assert!(matches!(
//!     session.send(&too_long),
//!     Err(ChatError::TextTooLong { .. })
//! ));
//! ```

/// Survival-tier minimum send rate (bits per second).
///
/// This is the 48 kbps floor referenced throughout the architecture spec (§8).
/// Chat messages must remain deliverable at this rate.
pub const SURVIVAL_TIER_BPS: u64 = 48_000;

/// Maximum UTF-8 byte count for a single chat message text.
///
/// Bounds the frame size so a message always fits inside a single LBTP
/// datagram and transmits in ≤ 43 ms at the survival-tier rate.
pub const CHAT_MAX_TEXT_BYTES: usize = 256;

/// Error returned when a chat message cannot be queued.
#[derive(Debug, PartialEq, Eq)]
pub enum ChatError {
    /// The message text exceeds [`CHAT_MAX_TEXT_BYTES`].
    TextTooLong {
        /// Actual byte length of the rejected text.
        len: usize,
    },
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextTooLong { len } => write!(
                f,
                "chat message text is {len} bytes, exceeds limit of {CHAT_MAX_TEXT_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ChatError {}

/// A single in-session text message.
///
/// Sent on the reliable-ordered control channel so frames are never dropped,
/// including at survival tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// UTF-8 message body.  At most [`CHAT_MAX_TEXT_BYTES`] bytes.
    pub text: String,
}

impl ChatMessage {
    /// Byte length of the text payload.
    pub fn byte_len(&self) -> usize {
        self.text.len()
    }
}

/// Per-session in-session chat gateway.
///
/// Queues outbound messages for reliable transmission.  Unlike
/// [`ClipboardSession`], no capability grant is required.
///
/// [`ClipboardSession`]: crate::clipboard::ClipboardSession
#[derive(Debug, Default)]
pub struct ChatSession {
    outbox: Vec<ChatMessage>,
}

impl ChatSession {
    /// Create a new session with an empty outbox.
    pub fn new() -> Self {
        Self { outbox: Vec::new() }
    }

    /// Queue `text` as a reliable outbound chat message.
    ///
    /// Returns `Err(`[`ChatError::TextTooLong`]`)` when `text.len() >
    /// CHAT_MAX_TEXT_BYTES`.  On success the message is appended to the
    /// [`outbox`](Self::outbox) and a reference is returned.
    ///
    /// No capability grant is needed — any participant may call this at any
    /// tier, including survival tier.
    pub fn send(&mut self, text: &str) -> Result<&ChatMessage, ChatError> {
        if text.len() > CHAT_MAX_TEXT_BYTES {
            return Err(ChatError::TextTooLong { len: text.len() });
        }
        self.outbox.push(ChatMessage { text: text.to_owned() });
        Ok(self.outbox.last().unwrap())
    }

    /// All outgoing messages queued for reliable transmission, in send order.
    pub fn outbox(&self) -> &[ChatMessage] {
        &self.outbox
    }

    /// Remove and return all queued messages (transport hand-off).
    ///
    /// After this call [`outbox`](Self::outbox) is empty.
    pub fn drain(&mut self) -> Vec<ChatMessage> {
        std::mem::take(&mut self.outbox)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Basic send behaviour ──────────────────────────────────────────────────

    #[test]
    fn send_queues_message_without_grant() {
        let mut session = ChatSession::new();
        let result = session.send("hello");
        assert!(result.is_ok(), "send must succeed without any capability grant");
        assert_eq!(session.outbox().len(), 1);
        assert_eq!(session.outbox()[0].text, "hello");
    }

    #[test]
    fn multiple_messages_queued_in_send_order() {
        let mut session = ChatSession::new();
        session.send("first").unwrap();
        session.send("second").unwrap();
        session.send("third").unwrap();

        let outbox = session.outbox();
        assert_eq!(outbox.len(), 3);
        assert_eq!(outbox[0].text, "first");
        assert_eq!(outbox[1].text, "second");
        assert_eq!(outbox[2].text, "third");
    }

    #[test]
    fn empty_text_is_accepted() {
        let mut session = ChatSession::new();
        assert!(session.send("").is_ok());
        assert_eq!(session.outbox().len(), 1);
        assert_eq!(session.outbox()[0].text, "");
    }

    #[test]
    fn send_returns_reference_to_queued_message() {
        let mut session = ChatSession::new();
        let msg = session.send("ping").unwrap();
        assert_eq!(msg.text, "ping");
    }

    // ── Text-length enforcement ───────────────────────────────────────────────

    #[test]
    fn max_length_message_accepted() {
        let mut session = ChatSession::new();
        let max = "a".repeat(CHAT_MAX_TEXT_BYTES);
        assert!(session.send(&max).is_ok());
        assert_eq!(session.outbox()[0].byte_len(), CHAT_MAX_TEXT_BYTES);
    }

    #[test]
    fn exceeding_max_length_rejected() {
        let mut session = ChatSession::new();
        let too_long = "a".repeat(CHAT_MAX_TEXT_BYTES + 1);
        assert_eq!(
            session.send(&too_long),
            Err(ChatError::TextTooLong { len: CHAT_MAX_TEXT_BYTES + 1 }),
        );
    }

    #[test]
    fn rejected_message_not_queued() {
        let mut session = ChatSession::new();
        let too_long = "x".repeat(CHAT_MAX_TEXT_BYTES + 1);
        let _ = session.send(&too_long);
        assert_eq!(session.outbox().len(), 0, "rejected message must not appear in outbox");
    }

    #[test]
    fn error_display_mentions_limit() {
        let msg = ChatError::TextTooLong { len: 300 }.to_string();
        assert!(
            msg.contains("300") && msg.contains(&CHAT_MAX_TEXT_BYTES.to_string()),
            "error message: {msg}"
        );
    }

    // ── Survival-tier guarantee ───────────────────────────────────────────────

    /// A max-length chat message must be deliverable at the survival-tier rate
    /// (48 kbps) within a 200 ms interactive budget.
    ///
    /// Transmission time = CHAT_MAX_TEXT_BYTES × 8 / SURVIVAL_TIER_BPS
    ///                   = 256 × 8 / 48 000 ≈ 43 ms  ✓
    #[test]
    fn max_message_transmits_within_200ms_at_survival_tier() {
        const BUDGET_MS: u64 = 200;
        let tx_ms = (CHAT_MAX_TEXT_BYTES as u64 * 8 * 1_000) / SURVIVAL_TIER_BPS;
        assert!(
            tx_ms <= BUDGET_MS,
            "max chat frame takes {tx_ms} ms at survival tier — exceeds {BUDGET_MS} ms budget"
        );
    }

    /// Chat must succeed even when nothing else can be sent (simulated survival
    /// tier: outbox accepts messages regardless of external rate state).
    #[test]
    fn send_succeeds_during_simulated_survival_tier() {
        // Survival tier imposes no restriction on the ChatSession itself —
        // that would be enforced by the transport.  What we verify here is
        // that ChatSession never self-throttles: messages are always accepted
        // (up to the text limit) regardless of how many are already queued.
        let mut session = ChatSession::new();
        for i in 0..10 {
            session.send(&format!("message {i}")).unwrap();
        }
        assert_eq!(session.outbox().len(), 10);
    }

    // ── drain ─────────────────────────────────────────────────────────────────

    #[test]
    fn drain_returns_all_messages_and_clears_outbox() {
        let mut session = ChatSession::new();
        session.send("a").unwrap();
        session.send("b").unwrap();

        let drained = session.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "a");
        assert_eq!(drained[1].text, "b");
        assert_eq!(session.outbox().len(), 0, "outbox must be empty after drain");
    }

    #[test]
    fn drain_on_empty_session_returns_empty_vec() {
        let mut session = ChatSession::new();
        assert!(session.drain().is_empty());
    }

    #[test]
    fn drain_then_send_works() {
        let mut session = ChatSession::new();
        session.send("before drain").unwrap();
        session.drain();
        session.send("after drain").unwrap();
        assert_eq!(session.outbox().len(), 1);
        assert_eq!(session.outbox()[0].text, "after drain");
    }

    // ── ChatMessage helpers ───────────────────────────────────────────────────

    #[test]
    fn byte_len_reports_text_byte_count() {
        let mut session = ChatSession::new();
        session.send("hello").unwrap();
        assert_eq!(session.outbox()[0].byte_len(), 5);
    }

    #[test]
    fn default_session_has_empty_outbox() {
        let session = ChatSession::default();
        assert!(session.outbox().is_empty());
    }
}

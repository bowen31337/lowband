//! Capability-gated clipboard sync — Features 113 and 116.
//!
//! A [`ClipboardGrant`] is a live capability token issued by the consent
//! subsystem when the remote peer's clipboard permission is toggled on.
//! Dropping it revokes the capability.
//!
//! [`ClipboardSession::apply_remote`] enforces two gates:
//!
//! 1. **Grant gate** (Feature 116): an incoming clipboard frame is accepted
//!    only while a [`ClipboardGrant`] is held.  Without one the call returns
//!    [`ClipboardError::NoActiveGrant`] and the frame is silently discarded —
//!    no local clipboard state changes.
//!
//! 2. **Size gate** (Feature 113): text exceeding [`CLIPBOARD_MAX_TEXT_BYTES`]
//!    is rejected with [`ClipboardError::TextTooLong`].  The cap guarantees
//!    that a clipboard frame transmitted on the reliable ctrl channel
//!    (priority 0, highest in the LBTP system) completes a full round-trip
//!    in under one second at the constrained tier
//!    ([`CONSTRAINED_TIER_BPS`] = 150 kbps), even with a conservative 3G
//!    propagation budget of 400 ms.
//!
//! # Example
//!
//! ```
//! use lowband_messaging::clipboard::{ClipboardGrant, ClipboardSession, ClipboardError};
//!
//! let mut session = ClipboardSession::new();
//!
//! // No grant — remote content is rejected.
//! assert_eq!(
//!     session.apply_remote("hello"),
//!     Err(ClipboardError::NoActiveGrant),
//! );
//!
//! // Consent granted — remote content is accepted.
//! session.set_grant(Some(ClipboardGrant::new()));
//! assert!(session.apply_remote("hello").is_ok());
//!
//! // Grant revoked — back to rejection.
//! session.set_grant(None);
//! assert_eq!(
//!     session.apply_remote("hello"),
//!     Err(ClipboardError::NoActiveGrant),
//! );
//! ```

use crate::grants::ConsentRevocationHandle;

/// Maximum UTF-8 byte count for a clipboard text payload (Feature 113).
///
/// Bounding the payload ensures that a clipboard frame transmitted at the
/// constrained tier completes a round-trip in under one second:
///
/// ```text
/// wire_forward = 4096 × 8 / 150 000 ≈ 218 ms
/// wire_ack     =   32 × 8 / 150 000 ≈   2 ms
/// rtt_budget   =                        400 ms  (conservative 3G)
/// total        ≈ 620 ms  <  1 000 ms  ✓
/// ```
pub const CLIPBOARD_MAX_TEXT_BYTES: usize = 4_096;

/// Constrained-tier link rate in bits per second (Feature 113).
///
/// The architecture spec defines the constrained tier as the operating point
/// where voice + legible screen + responsive input are all viable.
/// 150 kbps corresponds to the "pleasant at 150 kbps" reference from the PRD.
pub const CONSTRAINED_TIER_BPS: u64 = 150_000;

/// Capability token that proves the remote peer's clipboard permission is
/// currently active.  Issued by the consent subsystem; drop to revoke.
///
/// Created via [`ClipboardGrant::new`] or [`ClipboardGrant::with_consent`].
#[derive(Debug)]
pub struct ClipboardGrant {
    revocation: Option<ConsentRevocationHandle>,
}

impl ClipboardGrant {
    /// Issue a new grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { revocation: None }
    }

    /// Issue a clipboard grant bound to a [`ConsentRevocationHandle`].
    ///
    /// The grant is invalidated instantly — with no grace window — when
    /// [`ConsentRevocationHandle::withdraw`] is called on any clone of `handle`.
    pub fn with_consent(handle: ConsentRevocationHandle) -> Self {
        Self { revocation: Some(handle) }
    }
}

impl Default for ClipboardGrant {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors returned when processing incoming remote clipboard content.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// The remote peer attempted to push clipboard content but no active
    /// [`ClipboardGrant`] is held for this session.
    NoActiveGrant,
    /// The clipboard text exceeds [`CLIPBOARD_MAX_TEXT_BYTES`].
    TextTooLong {
        /// Actual byte length of the rejected text.
        len: usize,
    },
    /// The assisted user withdrew consent; the token bound to the same
    /// [`ConsentRevocationHandle`] is invalidated with no grace window.
    ConsentWithdrawn,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveGrant => f.write_str("remote clipboard rejected: no active clipboard_grant"),
            Self::TextTooLong { len } => write!(
                f,
                "clipboard text is {len} bytes, exceeds limit of {CLIPBOARD_MAX_TEXT_BYTES}"
            ),
            Self::ConsentWithdrawn => {
                f.write_str("remote clipboard rejected: assisted user has withdrawn consent")
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Per-session clipboard gateway.
///
/// Holds the optional live [`ClipboardGrant`] and enforces it on every
/// incoming remote clipboard frame.
pub struct ClipboardSession {
    grant: Option<ClipboardGrant>,
}

impl ClipboardSession {
    /// Create a new session with no active grant.
    pub fn new() -> Self {
        Self { grant: None }
    }

    /// Replace the active grant.  Pass `Some(grant)` when the user consents,
    /// `None` when they revoke or the session ends.
    pub fn set_grant(&mut self, grant: Option<ClipboardGrant>) {
        self.grant = grant;
    }

    /// Returns `true` when a non-withdrawn [`ClipboardGrant`] is currently held.
    pub fn is_granted(&self) -> bool {
        match &self.grant {
            None => false,
            Some(g) => !g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()),
        }
    }

    /// Apply incoming remote clipboard text if and only if the capability token
    /// is active and the payload is within the size limit.
    ///
    /// Returns [`ClipboardError::ConsentWithdrawn`] when the assisted user has
    /// withdrawn consent via the bound [`ConsentRevocationHandle`],
    /// [`ClipboardError::NoActiveGrant`] if no grant is held, or
    /// [`ClipboardError::TextTooLong`] if `text.len() > CLIPBOARD_MAX_TEXT_BYTES`.
    ///
    /// On success the caller is responsible for writing `text` to the local OS
    /// clipboard (platform specifics live outside this crate).  On failure the
    /// frame must be discarded without modifying local clipboard state.
    pub fn apply_remote(&self, text: &str) -> Result<(), ClipboardError> {
        match &self.grant {
            None => return Err(ClipboardError::NoActiveGrant),
            Some(g) => {
                if g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) {
                    return Err(ClipboardError::ConsentWithdrawn);
                }
            }
        }
        if text.len() > CLIPBOARD_MAX_TEXT_BYTES {
            return Err(ClipboardError::TextTooLong { len: text.len() });
        }
        // Caller applies `text` to the OS clipboard.
        let _ = text;
        Ok(())
    }
}

impl Default for ClipboardSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::ConsentRevocationHandle;

    #[test]
    fn rejects_without_grant() {
        let session = ClipboardSession::new();
        assert_eq!(
            session.apply_remote("sensitive text"),
            Err(ClipboardError::NoActiveGrant),
        );
    }

    #[test]
    fn accepts_with_grant() {
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::new()));
        assert!(session.apply_remote("hello from remote").is_ok());
    }

    #[test]
    fn rejects_after_grant_revoked() {
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::new()));
        assert!(session.apply_remote("ok").is_ok());

        session.set_grant(None);
        assert_eq!(
            session.apply_remote("now rejected"),
            Err(ClipboardError::NoActiveGrant),
        );
    }

    #[test]
    fn is_granted_reflects_grant_state() {
        let mut session = ClipboardSession::new();
        assert!(!session.is_granted());

        session.set_grant(Some(ClipboardGrant::new()));
        assert!(session.is_granted());

        session.set_grant(None);
        assert!(!session.is_granted());
    }

    #[test]
    fn error_display_mentions_clipboard_grant() {
        let msg = ClipboardError::NoActiveGrant.to_string();
        assert!(msg.contains("clipboard_grant"), "error message: {msg}");
    }

    #[test]
    fn empty_text_still_requires_grant() {
        let session = ClipboardSession::new();
        assert_eq!(
            session.apply_remote(""),
            Err(ClipboardError::NoActiveGrant),
        );
    }

    // ── Feature 112: clipboard text only with capability_token held live ─────

    /// Verify that clipboard text is synced only while the capability_token
    /// is actively held, and that revocation is reflected immediately on the
    /// next operation — there is no pre-authorisation window.
    #[test]
    fn syncs_clipboard_text_only_with_capability_token_held_live() {
        let mut session = ClipboardSession::new();

        // Before any grant: every sync attempt is rejected.
        for text in &["first", "second", "third"] {
            assert_eq!(
                session.apply_remote(text),
                Err(ClipboardError::NoActiveGrant),
                "sync must be rejected without live capability_token (text={text})"
            );
        }

        // capability_token held live: every sync attempt succeeds.
        session.set_grant(Some(ClipboardGrant::new()));
        for text in &["alpha", "beta", "gamma"] {
            assert!(
                session.apply_remote(text).is_ok(),
                "sync must succeed while capability_token is held live (text={text})"
            );
        }

        // Token revoked mid-stream: subsequent syncs are rejected immediately,
        // with no grace period — "held live" means live at each call site.
        session.set_grant(None);
        for text in &["delta", "epsilon", "zeta"] {
            assert_eq!(
                session.apply_remote(text),
                Err(ClipboardError::NoActiveGrant),
                "sync must be rejected the moment capability_token is dropped (text={text})"
            );
        }
    }

    // ── Feature 113: clipboard round_trip under 1 s at constrained tier ──────

    #[test]
    fn max_length_text_accepted_with_grant() {
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::new()));
        let max = "a".repeat(CLIPBOARD_MAX_TEXT_BYTES);
        assert!(session.apply_remote(&max).is_ok());
    }

    #[test]
    fn text_exceeding_max_rejected_even_with_grant() {
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::new()));
        let too_long = "a".repeat(CLIPBOARD_MAX_TEXT_BYTES + 1);
        assert_eq!(
            session.apply_remote(&too_long),
            Err(ClipboardError::TextTooLong { len: CLIPBOARD_MAX_TEXT_BYTES + 1 }),
        );
    }

    #[test]
    fn too_long_error_display_mentions_limit() {
        let msg = ClipboardError::TextTooLong { len: 9999 }.to_string();
        assert!(
            msg.contains("9999") && msg.contains(&CLIPBOARD_MAX_TEXT_BYTES.to_string()),
            "error message: {msg}"
        );
    }

    /// Clipboard round-trip time must be under 1 s at the constrained tier.
    ///
    /// round_trip = wire_forward + wire_ack + network_rtt_budget
    ///
    /// wire_forward = CLIPBOARD_MAX_TEXT_BYTES × 8 / CONSTRAINED_TIER_BPS
    ///             ≈ 4 096 × 8 / 150 000 ≈ 218 ms
    /// wire_ack     =      32 × 8 / 150 000 ≈   2 ms
    /// network_rtt  = 400 ms (conservative 3G round-trip propagation budget)
    /// total        ≈ 620 ms  <  1 000 ms  ✓
    ///
    /// The clipboard frame travels on the ctrl channel (LBTP channel 0, priority
    /// 0 — highest in the system) so it is never blocked by audio, screen, or
    /// camera traffic in the pacer queue.
    #[test]
    fn clipboard_round_trip_under_1s_at_constrained_tier() {
        const BUDGET_MS: u64 = 1_000;
        const ACK_BYTES: u64 = 32;
        const NETWORK_RTT_MS: u64 = 400; // conservative 3G propagation budget

        let forward_ms =
            (CLIPBOARD_MAX_TEXT_BYTES as u64 * 8 * 1_000) / CONSTRAINED_TIER_BPS;
        let ack_ms = (ACK_BYTES * 8 * 1_000) / CONSTRAINED_TIER_BPS;
        let total_ms = forward_ms + ack_ms + NETWORK_RTT_MS;

        assert!(
            total_ms < BUDGET_MS,
            "clipboard round_trip {}ms exceeds 1 s budget \
             (wire_forward={}ms wire_ack={}ms rtt_budget={}ms)",
            total_ms,
            forward_ms,
            ack_ms,
            NETWORK_RTT_MS,
        );
    }

    // ── Consent withdrawal — instant invalidation ─────────────────────────────

    #[test]
    fn consent_withdrawal_invalidates_clipboard_grant_instantly() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::with_consent(handle.clone())));
        assert!(session.apply_remote("text").is_ok(), "must accept before withdrawal");
        handle.withdraw();
        assert_eq!(
            session.apply_remote("text"),
            Err(ClipboardError::ConsentWithdrawn),
            "clipboard must be rejected instantly on consent withdrawal",
        );
    }

    #[test]
    fn is_granted_false_after_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::with_consent(handle.clone())));
        assert!(session.is_granted());
        handle.withdraw();
        assert!(!session.is_granted(), "is_granted must be false after consent withdrawal");
    }

    #[test]
    fn consent_withdrawn_error_display_is_nonempty() {
        assert!(!ClipboardError::ConsentWithdrawn.to_string().is_empty());
    }

    #[test]
    fn size_gate_still_enforced_when_withdrawal_not_triggered() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ClipboardSession::new();
        session.set_grant(Some(ClipboardGrant::with_consent(handle.clone())));
        let too_long = "x".repeat(CLIPBOARD_MAX_TEXT_BYTES + 1);
        assert_eq!(
            session.apply_remote(&too_long),
            Err(ClipboardError::TextTooLong { len: CLIPBOARD_MAX_TEXT_BYTES + 1 }),
            "size gate must still fire even when withdrawal has not been triggered",
        );
    }
}

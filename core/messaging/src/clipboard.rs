//! Capability-gated clipboard sync — Feature 116.
//!
//! A [`ClipboardGrant`] is a live capability token issued by the consent
//! subsystem when the remote peer's clipboard permission is toggled on.
//! Dropping it revokes the capability.
//!
//! [`ClipboardSession::apply_remote`] enforces the gate: an incoming clipboard
//! frame is accepted only while a [`ClipboardGrant`] is held.  Without one the
//! call returns [`ClipboardError::NoActiveGrant`] and the frame is silently
//! discarded — no local clipboard state changes.
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

/// Capability token that proves the remote peer's clipboard permission is
/// currently active.  Issued by the consent subsystem; drop to revoke.
#[derive(Debug)]
pub struct ClipboardGrant {
    _private: (),
}

impl ClipboardGrant {
    /// Issue a new grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

/// Errors returned when processing incoming remote clipboard content.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// The remote peer attempted to push clipboard content but no active
    /// [`ClipboardGrant`] is held for this session.
    NoActiveGrant,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveGrant => f.write_str("remote clipboard rejected: no active clipboard_grant"),
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

    /// Returns `true` when a [`ClipboardGrant`] is currently held.
    pub fn is_granted(&self) -> bool {
        self.grant.is_some()
    }

    /// Apply incoming remote clipboard text if and only if the capability token
    /// is active.
    ///
    /// On success the caller is responsible for writing `text` to the local OS
    /// clipboard (platform specifics live outside this crate).  On failure the
    /// frame must be discarded without modifying local clipboard state.
    pub fn apply_remote(&self, text: &str) -> Result<(), ClipboardError> {
        if self.grant.is_none() {
            return Err(ClipboardError::NoActiveGrant);
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
}

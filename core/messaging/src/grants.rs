//! Capability-token grant types for view, control, and file transfer — Features 143, 153, 156.
//!
//! Each grant is a RAII capability token issued by the consent subsystem.
//! Dropping it revokes the capability.  The corresponding `*Session` types
//! enforce the token on every operation.
//!
//! # Pattern
//!
//! ```
//! use lowband_messaging::grants::{CapabilityError, ViewGrant, ViewSession};
//!
//! let mut session = ViewSession::new();
//!
//! // No grant — frame rejected.
//! assert_eq!(session.apply_frame(), Err(CapabilityError::NoActiveGrant));
//!
//! // Consent granted.
//! session.set_grant(Some(ViewGrant::new()));
//! assert!(session.apply_frame().is_ok());
//!
//! // Grant revoked.
//! session.set_grant(None);
//! assert_eq!(session.apply_frame(), Err(CapabilityError::NoActiveGrant));
//! ```

/// Error returned when an operation is attempted without an active capability grant.
#[derive(Debug, PartialEq, Eq)]
pub enum CapabilityError {
    /// The operation was rejected because no capability grant is currently held.
    NoActiveGrant,
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveGrant => write!(f, "operation rejected: no active capability grant"),
        }
    }
}

impl std::error::Error for CapabilityError {}

// ── ViewGrant / ViewSession ───────────────────────────────────────────────────

/// Capability token for screen-view access.  Drop to revoke.
#[derive(Debug)]
pub struct ViewGrant {
    _private: (),
}

impl ViewGrant {
    /// Issue a new view grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ViewGrant {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session screen-view gateway.
pub struct ViewSession {
    grant: Option<ViewGrant>,
}

impl ViewSession {
    /// Create a session with no active grant.
    pub fn new() -> Self {
        Self { grant: None }
    }

    /// Replace the active grant.  Pass `None` to revoke.
    pub fn set_grant(&mut self, grant: Option<ViewGrant>) {
        self.grant = grant;
    }

    /// `true` while a [`ViewGrant`] is held.
    pub fn is_granted(&self) -> bool {
        self.grant.is_some()
    }

    /// Accept an incoming screen frame if and only if a [`ViewGrant`] is held.
    pub fn apply_frame(&self) -> Result<(), CapabilityError> {
        if self.grant.is_none() {
            return Err(CapabilityError::NoActiveGrant);
        }
        Ok(())
    }
}

impl Default for ViewSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── ControlGrant / ControlSession ─────────────────────────────────────────────

/// Capability token for remote input injection.  Drop to revoke.
#[derive(Debug)]
pub struct ControlGrant {
    _private: (),
}

impl ControlGrant {
    /// Issue a new control grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ControlGrant {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session input-injection gateway.
pub struct ControlSession {
    grant: Option<ControlGrant>,
}

impl ControlSession {
    /// Create a session with no active grant.
    pub fn new() -> Self {
        Self { grant: None }
    }

    /// Replace the active grant.  Pass `None` to revoke.
    pub fn set_grant(&mut self, grant: Option<ControlGrant>) {
        self.grant = grant;
    }

    /// `true` while a [`ControlGrant`] is held.
    pub fn is_granted(&self) -> bool {
        self.grant.is_some()
    }

    /// Validate an incoming control event if and only if a [`ControlGrant`] is held.
    pub fn apply_event(&self) -> Result<(), CapabilityError> {
        if self.grant.is_none() {
            return Err(CapabilityError::NoActiveGrant);
        }
        Ok(())
    }
}

impl Default for ControlSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── FileGrant / FileSession ───────────────────────────────────────────────────

/// Capability token for file-transfer access.  Drop to revoke.
#[derive(Debug)]
pub struct FileGrant {
    _private: (),
}

impl FileGrant {
    /// Issue a new file grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for FileGrant {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session file-transfer gateway.
pub struct FileSession {
    grant: Option<FileGrant>,
}

impl FileSession {
    /// Create a session with no active grant.
    pub fn new() -> Self {
        Self { grant: None }
    }

    /// Replace the active grant.  Pass `None` to revoke.
    pub fn set_grant(&mut self, grant: Option<FileGrant>) {
        self.grant = grant;
    }

    /// `true` while a [`FileGrant`] is held.
    pub fn is_granted(&self) -> bool {
        self.grant.is_some()
    }

    /// Accept an incoming file chunk if and only if a [`FileGrant`] is held.
    pub fn apply_chunk(&self, _bytes: &[u8]) -> Result<(), CapabilityError> {
        if self.grant.is_none() {
            return Err(CapabilityError::NoActiveGrant);
        }
        Ok(())
    }
}

impl Default for FileSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ViewSession ─────────────────────────────────────────────────────────────

    #[test]
    fn view_rejects_without_grant() {
        let session = ViewSession::new();
        assert_eq!(session.apply_frame(), Err(CapabilityError::NoActiveGrant));
    }

    #[test]
    fn view_accepts_with_grant() {
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::new()));
        assert!(session.apply_frame().is_ok());
    }

    #[test]
    fn view_rejects_after_revoke() {
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::new()));
        assert!(session.apply_frame().is_ok());
        session.set_grant(None);
        assert_eq!(session.apply_frame(), Err(CapabilityError::NoActiveGrant));
    }

    #[test]
    fn view_is_granted_reflects_state() {
        let mut session = ViewSession::new();
        assert!(!session.is_granted());
        session.set_grant(Some(ViewGrant::new()));
        assert!(session.is_granted());
        session.set_grant(None);
        assert!(!session.is_granted());
    }

    // ControlSession ──────────────────────────────────────────────────────────

    #[test]
    fn control_rejects_without_grant() {
        let session = ControlSession::new();
        assert_eq!(session.apply_event(), Err(CapabilityError::NoActiveGrant));
    }

    #[test]
    fn control_accepts_with_grant() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::new()));
        assert!(session.apply_event().is_ok());
    }

    #[test]
    fn control_rejects_after_revoke() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::new()));
        assert!(session.apply_event().is_ok());
        session.set_grant(None);
        assert_eq!(session.apply_event(), Err(CapabilityError::NoActiveGrant));
    }

    // FileSession ─────────────────────────────────────────────────────────────

    #[test]
    fn file_rejects_without_grant() {
        let session = FileSession::new();
        assert_eq!(
            session.apply_chunk(b"sensitive-data"),
            Err(CapabilityError::NoActiveGrant),
        );
    }

    #[test]
    fn file_accepts_with_grant() {
        let mut session = FileSession::new();
        session.set_grant(Some(FileGrant::new()));
        assert!(session.apply_chunk(b"chunk-payload").is_ok());
    }

    #[test]
    fn file_rejects_after_revoke() {
        let mut session = FileSession::new();
        session.set_grant(Some(FileGrant::new()));
        assert!(session.apply_chunk(b"ok").is_ok());
        session.set_grant(None);
        assert_eq!(
            session.apply_chunk(b"after-revoke"),
            Err(CapabilityError::NoActiveGrant),
        );
    }

    // CapabilityError ─────────────────────────────────────────────────────────

    #[test]
    fn capability_error_display_is_nonempty() {
        assert!(!CapabilityError::NoActiveGrant.to_string().is_empty());
    }
}

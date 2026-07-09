//! Capability-token grant types for view, control, and file transfer — Features 143, 153, 156, 34.
//!
//! Each grant is a RAII capability token issued by the consent subsystem.
//! Dropping it revokes the capability.  Grants may carry an optional expiry
//! instant; once elapsed, operations return [`CapabilityError::GrantExpired`]
//! and the capability is treated as revoked.  The corresponding `*Session`
//! types enforce the token on every operation.
//!
//! # Pattern
//!
//! ```
//! use lowband_messaging::grants::{CapabilityError, ViewGrant, ViewSession};
//! use std::time::Duration;
//!
//! let mut session = ViewSession::new();
//!
//! // No grant — frame rejected.
//! assert_eq!(session.apply_frame(), Err(CapabilityError::NoActiveGrant));
//!
//! // Consent granted (no expiry).
//! session.set_grant(Some(ViewGrant::new()));
//! assert!(session.apply_frame().is_ok());
//!
//! // Consent granted with a TTL.
//! session.set_grant(Some(ViewGrant::with_duration(Duration::from_secs(300))));
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
    /// The operation was rejected because the consent_grant TTL has elapsed.
    /// Equivalent to revocation for all enforcement purposes.
    GrantExpired,
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveGrant => write!(f, "operation rejected: no active capability grant"),
            Self::GrantExpired  => write!(f, "operation rejected: consent_grant has expired"),
        }
    }
}

impl std::error::Error for CapabilityError {}

// ── ViewGrant / ViewSession ───────────────────────────────────────────────────

/// Capability token for screen-view access.  Drop to revoke.
///
/// Created via [`ViewGrant::new`] (no expiry) or
/// [`ViewGrant::with_duration`] (expires after the given TTL).
#[derive(Debug)]
pub struct ViewGrant {
    expires_at: Option<std::time::Instant>,
}

impl ViewGrant {
    /// Issue a new view grant that never expires.
    pub fn new() -> Self {
        Self { expires_at: None }
    }

    /// Issue a new view grant that expires after `duration`.
    ///
    /// Once `duration` has elapsed, [`ViewSession::apply_frame`] returns
    /// [`CapabilityError::GrantExpired`] and capture is rejected.
    pub fn with_duration(duration: std::time::Duration) -> Self {
        Self { expires_at: Some(std::time::Instant::now() + duration) }
    }

    /// `true` if the consent_grant TTL has elapsed.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |exp| std::time::Instant::now() >= exp)
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

    /// `true` while a non-expired [`ViewGrant`] is held.
    pub fn is_granted(&self) -> bool {
        self.grant.as_ref().map_or(false, |g| !g.is_expired())
    }

    /// Accept an incoming screen frame if and only if a non-expired
    /// [`ViewGrant`] is held.
    ///
    /// Returns [`CapabilityError::GrantExpired`] when a grant is present but
    /// its consent_grant TTL has elapsed, and [`CapabilityError::NoActiveGrant`]
    /// when no grant has been issued.
    pub fn apply_frame(&self) -> Result<(), CapabilityError> {
        match &self.grant {
            None => Err(CapabilityError::NoActiveGrant),
            Some(g) if g.is_expired() => Err(CapabilityError::GrantExpired),
            Some(_) => Ok(()),
        }
    }
}

impl Default for ViewSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── ControlGrant / ControlSession ─────────────────────────────────────────────

/// Capability token for remote input injection.  Drop to revoke.
///
/// Created via [`ControlGrant::new`] (no expiry) or
/// [`ControlGrant::with_duration`] (expires after the given TTL).
#[derive(Debug)]
pub struct ControlGrant {
    expires_at: Option<std::time::Instant>,
}

impl ControlGrant {
    /// Issue a new control grant that never expires.
    pub fn new() -> Self {
        Self { expires_at: None }
    }

    /// Issue a new control grant that expires after `duration`.
    ///
    /// Once `duration` has elapsed, [`ControlSession::apply_event`] returns
    /// [`CapabilityError::GrantExpired`] and injection is rejected.
    pub fn with_duration(duration: std::time::Duration) -> Self {
        Self { expires_at: Some(std::time::Instant::now() + duration) }
    }

    /// `true` if the consent_grant TTL has elapsed.
    pub fn is_expired(&self) -> bool {
        self.expires_at.map_or(false, |exp| std::time::Instant::now() >= exp)
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

    /// `true` while a non-expired [`ControlGrant`] is held.
    pub fn is_granted(&self) -> bool {
        self.grant.as_ref().map_or(false, |g| !g.is_expired())
    }

    /// Validate an incoming control event if and only if a non-expired
    /// [`ControlGrant`] is held.
    ///
    /// Returns [`CapabilityError::GrantExpired`] when a grant is present but
    /// its consent_grant TTL has elapsed, and [`CapabilityError::NoActiveGrant`]
    /// when no grant has been issued.
    pub fn apply_event(&self) -> Result<(), CapabilityError> {
        match &self.grant {
            None => Err(CapabilityError::NoActiveGrant),
            Some(g) if g.is_expired() => Err(CapabilityError::GrantExpired),
            Some(_) => Ok(()),
        }
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

// ── ConsentGrant — timed wrapper that binds ViewGrant + ControlGrant ─────────

/// A timed consent grant that ties screen-capture and input-injection rights
/// together under a single TTL — Feature 34.
///
/// When the grant expires both `apply_frame` and `apply_event` on the
/// associated sessions return [`CapabilityError::GrantExpired`].
///
/// # Example
///
/// ```
/// use lowband_messaging::grants::{CapabilityError, ConsentGrant};
/// use std::time::Duration;
///
/// let grant = ConsentGrant::with_duration(Duration::from_secs(300));
/// let (view, control) = grant.into_grants();
///
/// let mut view_session = lowband_messaging::grants::ViewSession::new();
/// let mut ctrl_session = lowband_messaging::grants::ControlSession::new();
/// view_session.set_grant(Some(view));
/// ctrl_session.set_grant(Some(control));
///
/// assert!(view_session.apply_frame().is_ok());
/// assert!(ctrl_session.apply_event().is_ok());
/// ```
pub struct ConsentGrant {
    duration: Option<std::time::Duration>,
}

impl ConsentGrant {
    /// Create a consent grant whose capture and injection rights never expire.
    pub fn new() -> Self {
        Self { duration: None }
    }

    /// Create a consent grant whose capture and injection rights both expire
    /// after `duration`.
    pub fn with_duration(duration: std::time::Duration) -> Self {
        Self { duration: Some(duration) }
    }

    /// Consume the consent grant and return a `(ViewGrant, ControlGrant)` pair
    /// sharing the same expiry instant.
    pub fn into_grants(self) -> (ViewGrant, ControlGrant) {
        match self.duration {
            None => (ViewGrant::new(), ControlGrant::new()),
            Some(d) => {
                // Both grants share the same expiry moment.
                let exp = std::time::Instant::now() + d;
                (
                    ViewGrant    { expires_at: Some(exp) },
                    ControlGrant { expires_at: Some(exp) },
                )
            }
        }
    }
}

impl Default for ConsentGrant {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    // ViewGrant expiry (Feature 34) ───────────────────────────────────────────

    #[test]
    fn view_grant_with_ample_duration_is_not_expired() {
        let grant = ViewGrant::with_duration(Duration::from_secs(3600));
        assert!(!grant.is_expired(), "grant with 1-hour TTL must not be expired immediately");
    }

    #[test]
    fn view_grant_with_zero_duration_is_expired() {
        // Duration::ZERO produces an instant that is already in the past.
        let grant = ViewGrant::with_duration(Duration::ZERO);
        assert!(grant.is_expired(), "grant with zero TTL must be expired immediately");
    }

    #[test]
    fn view_session_rejects_expired_grant_with_grant_expired_error() {
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::with_duration(Duration::ZERO)));
        assert_eq!(
            session.apply_frame(),
            Err(CapabilityError::GrantExpired),
            "expired view grant must return GrantExpired, not NoActiveGrant",
        );
    }

    #[test]
    fn view_is_granted_false_when_grant_expired() {
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::with_duration(Duration::ZERO)));
        assert!(
            !session.is_granted(),
            "is_granted must be false when the consent_grant TTL has elapsed",
        );
    }

    #[test]
    fn view_grant_with_nonzero_duration_accepts_immediately() {
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::with_duration(Duration::from_secs(300))));
        assert!(
            session.apply_frame().is_ok(),
            "non-expired timed view grant must accept frames immediately after issue",
        );
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

    // ControlGrant expiry (Feature 34) ────────────────────────────────────────

    #[test]
    fn control_grant_with_ample_duration_is_not_expired() {
        let grant = ControlGrant::with_duration(Duration::from_secs(3600));
        assert!(!grant.is_expired(), "control grant with 1-hour TTL must not be expired immediately");
    }

    #[test]
    fn control_grant_with_zero_duration_is_expired() {
        let grant = ControlGrant::with_duration(Duration::ZERO);
        assert!(grant.is_expired(), "control grant with zero TTL must be expired immediately");
    }

    #[test]
    fn control_session_rejects_expired_grant_with_grant_expired_error() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_duration(Duration::ZERO)));
        assert_eq!(
            session.apply_event(),
            Err(CapabilityError::GrantExpired),
            "expired control grant must return GrantExpired, not NoActiveGrant",
        );
    }

    #[test]
    fn control_is_granted_false_when_grant_expired() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_duration(Duration::ZERO)));
        assert!(
            !session.is_granted(),
            "is_granted must be false when the consent_grant TTL has elapsed",
        );
    }

    #[test]
    fn control_grant_with_nonzero_duration_accepts_immediately() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_duration(Duration::from_secs(300))));
        assert!(
            session.apply_event().is_ok(),
            "non-expired timed control grant must accept events immediately after issue",
        );
    }

    // ConsentGrant — shared TTL for capture + injection (Feature 34) ──────────

    #[test]
    fn consent_grant_issues_non_expired_view_and_control_grants() {
        let (view, control) = ConsentGrant::with_duration(Duration::from_secs(300)).into_grants();
        assert!(!view.is_expired(),    "view grant from ConsentGrant must not be expired immediately");
        assert!(!control.is_expired(), "control grant from ConsentGrant must not be expired immediately");
    }

    #[test]
    fn consent_grant_zero_duration_issues_both_expired() {
        let (view, control) = ConsentGrant::with_duration(Duration::ZERO).into_grants();
        assert!(view.is_expired(),    "view grant from zero-TTL ConsentGrant must be expired");
        assert!(control.is_expired(), "control grant from zero-TTL ConsentGrant must be expired");
    }

    #[test]
    fn consent_grant_sessions_both_reject_after_expiry() {
        let (view, control) = ConsentGrant::with_duration(Duration::ZERO).into_grants();
        let mut view_session = ViewSession::new();
        let mut ctrl_session = ControlSession::new();
        view_session.set_grant(Some(view));
        ctrl_session.set_grant(Some(control));

        assert_eq!(
            view_session.apply_frame(),
            Err(CapabilityError::GrantExpired),
            "view session must return GrantExpired after consent_grant expiry",
        );
        assert_eq!(
            ctrl_session.apply_event(),
            Err(CapabilityError::GrantExpired),
            "control session must return GrantExpired after consent_grant expiry",
        );
    }

    #[test]
    fn consent_grant_no_expiry_issues_perpetual_grants() {
        let (view, control) = ConsentGrant::new().into_grants();
        assert!(!view.is_expired(),    "perpetual view grant must never expire");
        assert!(!control.is_expired(), "perpetual control grant must never expire");
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
        assert!(!CapabilityError::GrantExpired.to_string().is_empty());
    }
}

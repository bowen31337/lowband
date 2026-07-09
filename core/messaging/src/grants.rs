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

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// ── ConsentRevocationHandle ───────────────────────────────────────────────────

/// Shared revocation signal for all capability tokens issued from one consent event.
///
/// Pass clones of this handle to [`ViewGrant::with_consent`],
/// [`ControlGrant::with_consent`], [`FileGrant::with_consent`], and
/// [`ClipboardGrant::with_consent`] when issuing grants.  A single call to
/// [`withdraw`] instantly invalidates every token bound to the same handle —
/// the very next `apply_*` call on each bound session returns
/// [`CapabilityError::ConsentWithdrawn`] (or its clipboard equivalent) with no
/// grace window.
///
/// [`withdraw`]: ConsentRevocationHandle::withdraw
/// [`ClipboardGrant::with_consent`]: crate::clipboard::ClipboardGrant::with_consent
#[derive(Clone)]
pub struct ConsentRevocationHandle(Arc<AtomicBool>);

impl ConsentRevocationHandle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Signal consent withdrawal.  All tokens bound to this handle reject their
    /// next operation with [`CapabilityError::ConsentWithdrawn`].
    pub fn withdraw(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// `true` if [`withdraw`] has been called on any clone of this handle.
    ///
    /// [`withdraw`]: Self::withdraw
    pub fn is_withdrawn(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for ConsentRevocationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsentRevocationHandle")
            .field("withdrawn", &self.is_withdrawn())
            .finish()
    }
}

// ── CapabilityError ───────────────────────────────────────────────────────────

/// Error returned when an operation is attempted without an active capability grant.
#[derive(Debug, PartialEq, Eq)]
pub enum CapabilityError {
    /// The operation was rejected because no capability grant is currently held.
    NoActiveGrant,
    /// The operation was rejected because the consent_grant TTL has elapsed.
    /// Equivalent to revocation for all enforcement purposes.
    GrantExpired,
    /// The assisted user withdrew consent; the capability token bound to the
    /// same [`ConsentRevocationHandle`] is invalidated with no grace window.
    ConsentWithdrawn,
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveGrant    => write!(f, "operation rejected: no active capability grant"),
            Self::GrantExpired     => write!(f, "operation rejected: consent_grant has expired"),
            Self::ConsentWithdrawn => write!(f, "operation rejected: assisted user has withdrawn consent"),
        }
    }
}

impl std::error::Error for CapabilityError {}

// ── ViewGrant / ViewSession ───────────────────────────────────────────────────

/// Capability token for screen-view access.  Drop to revoke.
///
/// Created via [`ViewGrant::new`] (no expiry),
/// [`ViewGrant::with_duration`] (expires after the given TTL), or
/// [`ViewGrant::with_consent`] (tied to a [`ConsentRevocationHandle`]).
#[derive(Debug)]
pub struct ViewGrant {
    expires_at: Option<std::time::Instant>,
    revocation: Option<ConsentRevocationHandle>,
}

impl ViewGrant {
    /// Issue a new view grant that never expires.
    pub fn new() -> Self {
        Self { expires_at: None, revocation: None }
    }

    /// Issue a new view grant that expires after `duration`.
    ///
    /// Once `duration` has elapsed, [`ViewSession::apply_frame`] returns
    /// [`CapabilityError::GrantExpired`] and capture is rejected.
    pub fn with_duration(duration: std::time::Duration) -> Self {
        Self { expires_at: Some(std::time::Instant::now() + duration), revocation: None }
    }

    /// Issue a view grant bound to a [`ConsentRevocationHandle`].
    ///
    /// The grant is invalidated instantly — with no grace window — when
    /// [`ConsentRevocationHandle::withdraw`] is called on any clone of `handle`.
    pub fn with_consent(handle: ConsentRevocationHandle) -> Self {
        Self { expires_at: None, revocation: Some(handle) }
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

    /// `true` while a non-expired, non-withdrawn [`ViewGrant`] is held.
    pub fn is_granted(&self) -> bool {
        match &self.grant {
            None => false,
            Some(g) => {
                !g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) && !g.is_expired()
            }
        }
    }

    /// Accept an incoming screen frame if and only if a non-expired,
    /// non-withdrawn [`ViewGrant`] is held.
    ///
    /// Returns [`CapabilityError::ConsentWithdrawn`] when the assisted user has
    /// withdrawn consent via the bound [`ConsentRevocationHandle`],
    /// [`CapabilityError::GrantExpired`] when a grant is present but its TTL
    /// has elapsed, and [`CapabilityError::NoActiveGrant`] when no grant has
    /// been issued.
    pub fn apply_frame(&self) -> Result<(), CapabilityError> {
        match &self.grant {
            None => Err(CapabilityError::NoActiveGrant),
            Some(g) => {
                if g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) {
                    return Err(CapabilityError::ConsentWithdrawn);
                }
                if g.is_expired() {
                    return Err(CapabilityError::GrantExpired);
                }
                Ok(())
            }
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
/// Created via [`ControlGrant::new`] (no expiry),
/// [`ControlGrant::with_duration`] (expires after the given TTL), or
/// [`ControlGrant::with_consent`] (tied to a [`ConsentRevocationHandle`]).
#[derive(Debug)]
pub struct ControlGrant {
    expires_at: Option<std::time::Instant>,
    revocation: Option<ConsentRevocationHandle>,
}

impl ControlGrant {
    /// Issue a new control grant that never expires.
    pub fn new() -> Self {
        Self { expires_at: None, revocation: None }
    }

    /// Issue a new control grant that expires after `duration`.
    ///
    /// Once `duration` has elapsed, [`ControlSession::apply_event`] returns
    /// [`CapabilityError::GrantExpired`] and injection is rejected.
    pub fn with_duration(duration: std::time::Duration) -> Self {
        Self { expires_at: Some(std::time::Instant::now() + duration), revocation: None }
    }

    /// Issue a control grant bound to a [`ConsentRevocationHandle`].
    ///
    /// The grant is invalidated instantly — with no grace window — when
    /// [`ConsentRevocationHandle::withdraw`] is called on any clone of `handle`.
    pub fn with_consent(handle: ConsentRevocationHandle) -> Self {
        Self { expires_at: None, revocation: Some(handle) }
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

    /// `true` while a non-expired, non-withdrawn [`ControlGrant`] is held.
    pub fn is_granted(&self) -> bool {
        match &self.grant {
            None => false,
            Some(g) => {
                !g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) && !g.is_expired()
            }
        }
    }

    /// Validate an incoming control event if and only if a non-expired,
    /// non-withdrawn [`ControlGrant`] is held.
    ///
    /// Returns [`CapabilityError::ConsentWithdrawn`] when the assisted user has
    /// withdrawn consent via the bound [`ConsentRevocationHandle`],
    /// [`CapabilityError::GrantExpired`] when a grant is present but its TTL
    /// has elapsed, and [`CapabilityError::NoActiveGrant`] when no grant has
    /// been issued.
    pub fn apply_event(&self) -> Result<(), CapabilityError> {
        match &self.grant {
            None => Err(CapabilityError::NoActiveGrant),
            Some(g) => {
                if g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) {
                    return Err(CapabilityError::ConsentWithdrawn);
                }
                if g.is_expired() {
                    return Err(CapabilityError::GrantExpired);
                }
                Ok(())
            }
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
///
/// Created via [`FileGrant::new`] or [`FileGrant::with_consent`].
#[derive(Debug)]
pub struct FileGrant {
    revocation: Option<ConsentRevocationHandle>,
}

impl FileGrant {
    /// Issue a new file grant.  Only the consent subsystem should call this.
    pub fn new() -> Self {
        Self { revocation: None }
    }

    /// Issue a file grant bound to a [`ConsentRevocationHandle`].
    ///
    /// The grant is invalidated instantly — with no grace window — when
    /// [`ConsentRevocationHandle::withdraw`] is called on any clone of `handle`.
    pub fn with_consent(handle: ConsentRevocationHandle) -> Self {
        Self { revocation: Some(handle) }
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

    /// `true` while a non-withdrawn [`FileGrant`] is held.
    pub fn is_granted(&self) -> bool {
        match &self.grant {
            None => false,
            Some(g) => !g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()),
        }
    }

    /// Accept an incoming file chunk if and only if a non-withdrawn
    /// [`FileGrant`] is held.
    ///
    /// Returns [`CapabilityError::ConsentWithdrawn`] when the assisted user has
    /// withdrawn consent via the bound [`ConsentRevocationHandle`], or
    /// [`CapabilityError::NoActiveGrant`] when no grant has been issued.
    pub fn apply_chunk(&self, _bytes: &[u8]) -> Result<(), CapabilityError> {
        match &self.grant {
            None => Err(CapabilityError::NoActiveGrant),
            Some(g) => {
                if g.revocation.as_ref().map_or(false, |r| r.is_withdrawn()) {
                    return Err(CapabilityError::ConsentWithdrawn);
                }
                Ok(())
            }
        }
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
/// Use [`issue_all`] to get all four capability grants and a shared
/// [`ConsentRevocationHandle`] that can invalidate them all instantly.
///
/// [`issue_all`]: ConsentGrant::issue_all
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
                    ViewGrant    { expires_at: Some(exp), revocation: None },
                    ControlGrant { expires_at: Some(exp), revocation: None },
                )
            }
        }
    }

    /// Consume the consent grant and return view, control, and file grants all
    /// bound to the same [`ConsentRevocationHandle`].
    ///
    /// Calling [`ConsentRevocationHandle::withdraw`] on the returned handle
    /// (or any clone of it) instantly invalidates all three grants.  To also
    /// bind clipboard access, pass a clone of the handle to
    /// [`ClipboardGrant::with_consent`].
    ///
    /// [`ClipboardGrant::with_consent`]: crate::clipboard::ClipboardGrant::with_consent
    pub fn issue_all(self) -> (ViewGrant, ControlGrant, FileGrant, ConsentRevocationHandle) {
        let handle = ConsentRevocationHandle::new();
        let exp = self.duration.map(|d| std::time::Instant::now() + d);
        (
            ViewGrant    { expires_at: exp, revocation: Some(handle.clone()) },
            ControlGrant { expires_at: exp, revocation: Some(handle.clone()) },
            FileGrant    { revocation: Some(handle.clone()) },
            handle,
        )
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
        assert!(!CapabilityError::ConsentWithdrawn.to_string().is_empty());
    }

    // ConsentRevocationHandle — instant withdrawal ────────────────────────────

    #[test]
    fn revocation_handle_not_withdrawn_on_creation() {
        let handle = ConsentRevocationHandle::new();
        assert!(!handle.is_withdrawn());
    }

    #[test]
    fn revocation_handle_withdrawn_after_withdraw_call() {
        let handle = ConsentRevocationHandle::new();
        handle.withdraw();
        assert!(handle.is_withdrawn());
    }

    #[test]
    fn revocation_handle_clone_sees_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let clone = handle.clone();
        handle.withdraw();
        assert!(clone.is_withdrawn(), "cloned handle must reflect withdrawal on original");
    }

    #[test]
    fn view_grant_rejects_instantly_on_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::with_consent(handle.clone())));
        assert!(session.apply_frame().is_ok(), "must accept before withdrawal");
        handle.withdraw();
        assert_eq!(
            session.apply_frame(),
            Err(CapabilityError::ConsentWithdrawn),
            "view must be rejected instantly on consent withdrawal",
        );
    }

    #[test]
    fn view_is_granted_false_after_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ViewSession::new();
        session.set_grant(Some(ViewGrant::with_consent(handle.clone())));
        assert!(session.is_granted());
        handle.withdraw();
        assert!(!session.is_granted(), "is_granted must be false after consent withdrawal");
    }

    #[test]
    fn control_grant_rejects_instantly_on_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_consent(handle.clone())));
        assert!(session.apply_event().is_ok(), "must accept before withdrawal");
        handle.withdraw();
        assert_eq!(
            session.apply_event(),
            Err(CapabilityError::ConsentWithdrawn),
            "control must be rejected instantly on consent withdrawal",
        );
    }

    #[test]
    fn control_is_granted_false_after_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_consent(handle.clone())));
        assert!(session.is_granted());
        handle.withdraw();
        assert!(!session.is_granted(), "is_granted must be false after consent withdrawal");
    }

    #[test]
    fn file_grant_rejects_instantly_on_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = FileSession::new();
        session.set_grant(Some(FileGrant::with_consent(handle.clone())));
        assert!(session.apply_chunk(b"data").is_ok(), "must accept before withdrawal");
        handle.withdraw();
        assert_eq!(
            session.apply_chunk(b"data"),
            Err(CapabilityError::ConsentWithdrawn),
            "file must be rejected instantly on consent withdrawal",
        );
    }

    #[test]
    fn file_is_granted_false_after_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = FileSession::new();
        session.set_grant(Some(FileGrant::with_consent(handle.clone())));
        assert!(session.is_granted());
        handle.withdraw();
        assert!(!session.is_granted(), "is_granted must be false after consent withdrawal");
    }

    #[test]
    fn consent_grant_issue_all_links_all_three_to_same_revocation_handle() {
        let (view_grant, ctrl_grant, file_grant, handle) = ConsentGrant::new().issue_all();
        let mut view = ViewSession::new();
        let mut ctrl = ControlSession::new();
        let mut file = FileSession::new();
        view.set_grant(Some(view_grant));
        ctrl.set_grant(Some(ctrl_grant));
        file.set_grant(Some(file_grant));

        assert!(view.apply_frame().is_ok());
        assert!(ctrl.apply_event().is_ok());
        assert!(file.apply_chunk(b"ok").is_ok());

        handle.withdraw();

        assert_eq!(view.apply_frame(),       Err(CapabilityError::ConsentWithdrawn));
        assert_eq!(ctrl.apply_event(),       Err(CapabilityError::ConsentWithdrawn));
        assert_eq!(file.apply_chunk(b"ok"), Err(CapabilityError::ConsentWithdrawn));
    }

    #[test]
    fn consent_grant_issue_all_respects_timed_expiry_before_withdrawal() {
        let (view_grant, ctrl_grant, _file_grant, _handle) =
            ConsentGrant::with_duration(Duration::ZERO).issue_all();
        let mut view = ViewSession::new();
        let mut ctrl = ControlSession::new();
        view.set_grant(Some(view_grant));
        ctrl.set_grant(Some(ctrl_grant));

        assert_eq!(view.apply_frame(), Err(CapabilityError::GrantExpired));
        assert_eq!(ctrl.apply_event(), Err(CapabilityError::GrantExpired));
    }
}

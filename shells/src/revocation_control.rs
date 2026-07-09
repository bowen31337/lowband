//! Revocation control button — one-click capability_token removal.
//!
//! Tracks session grant state and revocation events from IPC and exposes the
//! display state the UI shell must render for the one-click access revocation
//! control.
//!
//! When the user clicks "Revoke access", the UI sends a revoke command to the
//! daemon which calls [`ConsentRevocationHandle::withdraw`].  This instantly
//! invalidates all capability tokens (view, control, file, clipboard) bound to
//! the same handle — with no grace window — severing all session access rights.
//!
//! [`ConsentRevocationHandle::withdraw`]: lowband_messaging::grants::ConsentRevocationHandle::withdraw
//!
//! # States
//!
//! | State | Condition | Rendering |
//! |-------|-----------|-----------|
//! | `NoSession` | No active session | Control not shown |
//! | `NoGrant` | Session active, no grants issued yet | Control hidden |
//! | `GrantActive` | Capability grants are live | Red revoke button visible |
//! | `AccessRevoked` | All tokens withdrawn | Persistent revoked indicator |
//!
//! # Design colours
//!
//! - [`REVOKE_BUTTON_COLOR`] (`#dc2626`, red) — the active revoke button
//! - [`REVOKE_CONFIRMED_COLOR`] (`#16a34a`, green) — after access is revoked
//!
//! # Usage
//!
//! ```
//! use lowband_shells::revocation_control::{RevocationControl, RevocationControlState};
//!
//! let mut rc = RevocationControl::new();
//!
//! // Session established with capability grants issued.
//! rc.set_session_active(true);
//! rc.set_grant_active(true);
//! assert_eq!(rc.state(), RevocationControlState::GrantActive);
//!
//! // User clicks "Revoke access"; daemon confirms all tokens withdrawn.
//! rc.on_access_revoked();
//! assert_eq!(rc.state(), RevocationControlState::AccessRevoked);
//!
//! // Session ends.
//! rc.set_session_active(false);
//! assert_eq!(rc.state(), RevocationControlState::NoSession);
//! ```

/// Red danger colour for the active revoke button.
///
/// RGB hex `#dc2626` — matches the `Danger` token in the LowBand design system.
pub const REVOKE_BUTTON_COLOR: &str = "#dc2626";

/// Green colour shown once all capability tokens have been withdrawn.
///
/// RGB hex `#16a34a` — matches the `Consent` token in the LowBand design
/// system, signalling that access has been safely removed.
pub const REVOKE_CONFIRMED_COLOR: &str = "#16a34a";

/// Label rendered on the revoke button when capability grants are active.
pub const REVOKE_BUTTON_LABEL: &str = "Revoke access";

/// Label shown once all capability tokens have been withdrawn.
pub const REVOKE_CONFIRMED_LABEL: &str = "Access revoked";

/// Display state for the revocation control button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationControlState {
    /// No active session; the revocation control must not be rendered.
    NoSession,
    /// A session is active but no capability grants have been issued yet;
    /// the control is hidden because there is nothing to revoke.
    NoGrant,
    /// Capability grants are live (view, control, file, or clipboard); the
    /// revoke button must be rendered with label [`REVOKE_BUTTON_LABEL`] in
    /// colour [`REVOKE_BUTTON_COLOR`].
    GrantActive,
    /// The user revoked access; all capability tokens bound to the shared
    /// [`ConsentRevocationHandle`] are withdrawn.  Render a persistent
    /// indicator with label [`REVOKE_CONFIRMED_LABEL`] in colour
    /// [`REVOKE_CONFIRMED_COLOR`].
    ///
    /// [`ConsentRevocationHandle`]: lowband_messaging::grants::ConsentRevocationHandle
    AccessRevoked,
}

/// Tracks session grant state and revocation events from IPC and derives the
/// revocation control display state.
///
/// Construct one `RevocationControl` per in-session UI instance.  Drive it with:
/// - [`RevocationControl::set_session_active`] on `IpcEvent::SessionState`.
/// - [`RevocationControl::set_grant_active`] on `IpcEvent::TokenGranted`.
/// - [`RevocationControl::on_access_revoked`] on `IpcEvent::TokenRevoked`.
///
/// Read [`RevocationControl::state`] on every IPC event and re-render the
/// control when the returned [`RevocationControlState`] changes.
pub struct RevocationControl {
    session_active: bool,
    grant_active: bool,
    revoked: bool,
}

impl RevocationControl {
    /// Create a new revocation control in the idle (no session) state.
    pub fn new() -> Self {
        Self { session_active: false, grant_active: false, revoked: false }
    }

    /// Record that the LBTP session became active (`active = true`) or ended
    /// (`active = false`).
    ///
    /// Ending the session resets the grant and revocation flags so the next
    /// session starts from a clean state.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        if !active {
            self.grant_active = false;
            self.revoked = false;
        }
    }

    /// Record that the daemon issued capability grants (`active = true`) or
    /// that there are no active grants (`active = false`).
    ///
    /// A fresh grant issued after a re-consent flow clears the `revoked` flag
    /// so the UI transitions back to [`RevocationControlState::GrantActive`].
    pub fn set_grant_active(&mut self, active: bool) {
        self.grant_active = active;
        if active {
            self.revoked = false;
        }
    }

    /// Record that the daemon confirmed all capability tokens were withdrawn
    /// (`IpcEvent::TokenRevoked`).
    ///
    /// Sets the `revoked` flag, which takes priority over `grant_active` in
    /// [`state`](Self::state).  No-op when no session is active.
    pub fn on_access_revoked(&mut self) {
        if self.session_active {
            self.revoked = true;
        }
    }

    /// Return the current revocation control display state.
    ///
    /// Priority order (highest first):
    /// 1. No session → [`RevocationControlState::NoSession`]
    /// 2. Revoked → [`RevocationControlState::AccessRevoked`]
    /// 3. Grant held → [`RevocationControlState::GrantActive`]
    /// 4. Otherwise → [`RevocationControlState::NoGrant`]
    pub fn state(&self) -> RevocationControlState {
        if !self.session_active {
            return RevocationControlState::NoSession;
        }
        if self.revoked {
            return RevocationControlState::AccessRevoked;
        }
        if self.grant_active {
            RevocationControlState::GrantActive
        } else {
            RevocationControlState::NoGrant
        }
    }
}

impl Default for RevocationControl {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn initial_state_is_no_session() {
        assert_eq!(RevocationControl::new().state(), RevocationControlState::NoSession);
    }

    // ── Session active, no grant ──────────────────────────────────────────────

    #[test]
    fn session_active_without_grant_is_no_grant() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        assert_eq!(rc.state(), RevocationControlState::NoGrant);
    }

    // ── Grant active ──────────────────────────────────────────────────────────

    #[test]
    fn session_and_grant_active_shows_grant_active() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        assert_eq!(rc.state(), RevocationControlState::GrantActive);
    }

    // ── One-click revocation ──────────────────────────────────────────────────

    #[test]
    fn on_access_revoked_transitions_to_access_revoked() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.on_access_revoked();
        assert_eq!(rc.state(), RevocationControlState::AccessRevoked);
    }

    #[test]
    fn revoke_without_prior_grant_still_revokes() {
        // Daemon may confirm revocation before the IPC grant event arrives.
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.on_access_revoked();
        assert_eq!(rc.state(), RevocationControlState::AccessRevoked);
    }

    #[test]
    fn on_access_revoked_without_session_is_noop() {
        let mut rc = RevocationControl::new();
        rc.on_access_revoked();
        assert_eq!(rc.state(), RevocationControlState::NoSession);
    }

    // ── Grant removal without revoke ──────────────────────────────────────────

    #[test]
    fn removing_grant_transitions_to_no_grant() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.set_grant_active(false);
        assert_eq!(rc.state(), RevocationControlState::NoGrant);
    }

    // ── Re-consent after revocation ───────────────────────────────────────────

    #[test]
    fn fresh_grant_after_revoke_restores_grant_active() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.on_access_revoked();
        assert_eq!(rc.state(), RevocationControlState::AccessRevoked);

        // User completes a new consent flow; daemon issues fresh grants.
        rc.set_grant_active(true);
        assert_eq!(rc.state(), RevocationControlState::GrantActive);
    }

    // ── Session end resets everything ─────────────────────────────────────────

    #[test]
    fn session_end_returns_to_no_session() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.on_access_revoked();
        rc.set_session_active(false);
        assert_eq!(rc.state(), RevocationControlState::NoSession);
    }

    #[test]
    fn session_end_clears_revoked_and_grant_for_next_session() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.on_access_revoked();
        rc.set_session_active(false);

        rc.set_session_active(true);
        assert_eq!(
            rc.state(),
            RevocationControlState::NoGrant,
            "new session must start without any grants",
        );
    }

    #[test]
    fn second_session_with_grant_is_grant_active_not_revoked() {
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        rc.on_access_revoked();
        rc.set_session_active(false);

        rc.set_session_active(true);
        rc.set_grant_active(true);
        assert_eq!(
            rc.state(),
            RevocationControlState::GrantActive,
            "second session must not inherit revoked flag from the previous session",
        );
    }

    // ── Single-click: all tokens withdrawn atomically ─────────────────────────

    #[test]
    fn revocation_reports_access_revoked_on_ui_side() {
        // This is the UI side of the capability_token removal contract.  The
        // daemon side is exercised by ConsentRevocationHandle::withdraw
        // (grants.rs compliance tests).
        let mut rc = RevocationControl::new();
        rc.set_session_active(true);
        rc.set_grant_active(true);
        assert_eq!(rc.state(), RevocationControlState::GrantActive, "precondition");

        rc.on_access_revoked();

        assert_eq!(
            rc.state(),
            RevocationControlState::AccessRevoked,
            "UI must report AccessRevoked after single-click capability_token removal",
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn revoke_button_color_is_danger_red() {
        assert_eq!(REVOKE_BUTTON_COLOR, "#dc2626");
    }

    #[test]
    fn revoke_confirmed_color_is_consent_green() {
        assert_eq!(REVOKE_CONFIRMED_COLOR, "#16a34a");
    }

    #[test]
    fn revoke_button_label_is_nonempty() {
        assert!(!REVOKE_BUTTON_LABEL.is_empty());
    }

    #[test]
    fn revoke_confirmed_label_is_nonempty() {
        assert!(!REVOKE_CONFIRMED_LABEL.is_empty());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            RevocationControl::new().state(),
            RevocationControl::default().state(),
        );
    }

    // ── RevocationControlState is Copy ────────────────────────────────────────

    #[test]
    fn revocation_control_state_is_copy() {
        let s = RevocationControlState::GrantActive;
        let _s2 = s;
        let _s3 = s;
    }
}

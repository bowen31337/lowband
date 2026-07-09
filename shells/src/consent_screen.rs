//! Consent screen — per-capability access request dialog — Feature 143.
//!
//! When Tan (the technician) joins a session, the daemon sends a consent
//! request event.  The UI shell renders this screen so Ana (the assisted
//! user) can choose exactly which capabilities to allow.  Critically, she
//! can grant screen-view access via `view_grant` while **withholding** remote
//! input injection by choosing [`GrantChoice::ViewOnly`].
//!
//! The shell is a pure state machine; it does not issue capability tokens.
//! The daemon reads [`GrantChoice`] from the IPC message that the shell
//! produces and calls the appropriate grant constructors:
//!
//! - [`GrantChoice::ViewOnly`] → daemon issues `ViewGrant::with_consent(handle)`,
//!   no `ControlGrant` is issued.
//! - [`GrantChoice::ViewAndControl`] → daemon issues both
//!   `ViewGrant::with_consent(handle)` and `ControlGrant::with_consent(handle)`
//!   bound to the same [`ConsentRevocationHandle`].
//!
//! [`ConsentRevocationHandle`]: lowband_messaging::grants::ConsentRevocationHandle
//!
//! # States
//!
//! | State | Condition | Rendering |
//! |-------|-----------|-----------|
//! | `Idle` | No pending request | Screen not shown |
//! | `PendingRequest` | Consent request received from remote peer | Screen shown with choice buttons |
//! | `Granted(GrantChoice)` | User confirmed a grant choice | Screen dismissed; daemon issues tokens |
//! | `Denied` | User denied the request | Screen dismissed; no tokens issued |
//!
//! # Design labels
//!
//! - [`CONSENT_SCREEN_TITLE`] — dialog heading
//! - [`GRANT_VIEW_ONLY_LABEL`] — button for view-only access (withholds control)
//! - [`GRANT_VIEW_AND_CONTROL_LABEL`] — button for full view + control access
//! - [`DENY_LABEL`] — button to deny all access
//!
//! # Usage
//!
//! ```
//! use lowband_shells::consent_screen::{ConsentScreen, ConsentScreenState, GrantChoice};
//!
//! let mut screen = ConsentScreen::new();
//!
//! // Daemon signals that Tan has requested access.
//! screen.on_consent_request();
//! assert_eq!(screen.state(), ConsentScreenState::PendingRequest);
//!
//! // Ana grants view access only — control is withheld.
//! screen.grant(GrantChoice::ViewOnly);
//! assert_eq!(screen.state(), ConsentScreenState::Granted(GrantChoice::ViewOnly));
//!
//! // Daemon issues ViewGrant; shell resets for the next event.
//! screen.reset();
//! assert_eq!(screen.state(), ConsentScreenState::Idle);
//! ```

/// Dialog heading shown at the top of the consent screen.
pub const CONSENT_SCREEN_TITLE: &str = "Access request";

/// Button label that grants screen-view access while withholding control.
///
/// Selecting this causes the daemon to issue a [`ViewGrant`] but no
/// [`ControlGrant`], so the remote peer can see the screen but cannot
/// inject keyboard or mouse events.
///
/// [`ViewGrant`]: lowband_messaging::grants::ViewGrant
/// [`ControlGrant`]: lowband_messaging::grants::ControlGrant
pub const GRANT_VIEW_ONLY_LABEL: &str = "View only";

/// Button label that grants both screen-view access and remote input control.
pub const GRANT_VIEW_AND_CONTROL_LABEL: &str = "Allow control";

/// Button label that denies the access request entirely.
pub const DENY_LABEL: &str = "Deny";

/// The capability level that the assisted user chose to grant.
///
/// Returned inside [`ConsentScreenState::Granted`]; the daemon uses this to
/// decide which capability tokens to issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantChoice {
    /// Screen capture only.  No `ControlGrant` is issued; the remote peer
    /// cannot inject keyboard or mouse events.
    ViewOnly,
    /// Screen capture **and** remote input injection.  Both `ViewGrant` and
    /// `ControlGrant` are issued, bound to the same `ConsentRevocationHandle`.
    ViewAndControl,
}

/// Display state for the consent screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentScreenState {
    /// No pending consent request; the consent screen must not be rendered.
    Idle,
    /// A consent request has arrived from the remote peer.  The screen must
    /// be shown with the [`GRANT_VIEW_ONLY_LABEL`], [`GRANT_VIEW_AND_CONTROL_LABEL`],
    /// and [`DENY_LABEL`] buttons.
    PendingRequest,
    /// The assisted user confirmed access.  The screen is dismissed.
    ///
    /// The contained [`GrantChoice`] tells the daemon which capability tokens
    /// to issue: [`ViewOnly`](GrantChoice::ViewOnly) withholds control,
    /// [`ViewAndControl`](GrantChoice::ViewAndControl) grants both.
    Granted(GrantChoice),
    /// The assisted user denied the access request.  No capability tokens are
    /// issued.  The screen is dismissed.
    Denied,
}

/// Tracks consent request and grant/deny events from IPC and derives the
/// consent screen display state.
///
/// Construct one `ConsentScreen` per in-session UI instance.  Drive it with:
/// - [`ConsentScreen::on_consent_request`] on `IpcEvent::ConsentRequested`.
/// - [`ConsentScreen::grant`] when the user taps a grant button.
/// - [`ConsentScreen::deny`] when the user taps "Deny".
/// - [`ConsentScreen::reset`] after the daemon confirms tokens were issued.
/// - [`ConsentScreen::on_session_ended`] on `IpcEvent::SessionState(false)`.
///
/// Read [`ConsentScreen::state`] on every IPC event and re-render when the
/// returned [`ConsentScreenState`] changes.
pub struct ConsentScreen {
    state: ConsentScreenState,
}

impl ConsentScreen {
    /// Create a new consent screen in the idle (no request) state.
    pub fn new() -> Self {
        Self { state: ConsentScreenState::Idle }
    }

    /// Record that the daemon received a consent request from the remote peer.
    ///
    /// Transitions from [`Idle`](ConsentScreenState::Idle) to
    /// [`PendingRequest`](ConsentScreenState::PendingRequest).  A request
    /// while already [`PendingRequest`] is a no-op.
    pub fn on_consent_request(&mut self) {
        if self.state == ConsentScreenState::Idle {
            self.state = ConsentScreenState::PendingRequest;
        }
    }

    /// Record that the user chose to grant access at the given capability level.
    ///
    /// Transitions from [`PendingRequest`](ConsentScreenState::PendingRequest)
    /// to [`Granted(choice)`](ConsentScreenState::Granted).  No-op when no
    /// request is pending.
    pub fn grant(&mut self, choice: GrantChoice) {
        if self.state == ConsentScreenState::PendingRequest {
            self.state = ConsentScreenState::Granted(choice);
        }
    }

    /// Record that the user denied the access request.
    ///
    /// Transitions from [`PendingRequest`](ConsentScreenState::PendingRequest)
    /// to [`Denied`](ConsentScreenState::Denied).  No-op when no request is
    /// pending.
    pub fn deny(&mut self) {
        if self.state == ConsentScreenState::PendingRequest {
            self.state = ConsentScreenState::Denied;
        }
    }

    /// Reset to [`Idle`](ConsentScreenState::Idle) after the daemon has
    /// processed the grant or denial and the screen has been dismissed.
    pub fn reset(&mut self) {
        self.state = ConsentScreenState::Idle;
    }

    /// Record that the LBTP session ended.  Resets all state so the next
    /// session starts from a clean [`Idle`](ConsentScreenState::Idle).
    pub fn on_session_ended(&mut self) {
        self.state = ConsentScreenState::Idle;
    }

    /// Return the current consent screen display state.
    pub fn state(&self) -> ConsentScreenState {
        self.state
    }
}

impl Default for ConsentScreen {
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
    fn initial_state_is_idle() {
        assert_eq!(ConsentScreen::new().state(), ConsentScreenState::Idle);
    }

    // ── Consent request ───────────────────────────────────────────────────────

    #[test]
    fn consent_request_transitions_to_pending() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        assert_eq!(screen.state(), ConsentScreenState::PendingRequest);
    }

    #[test]
    fn consent_request_while_pending_is_noop() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.on_consent_request();
        assert_eq!(screen.state(), ConsentScreenState::PendingRequest);
    }

    // ── View-only grant — the key Feature 143 behaviour ───────────────────────

    #[test]
    fn grant_view_only_from_pending_transitions_to_granted_view_only() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Granted(GrantChoice::ViewOnly),
            "view_grant issued without control must produce Granted(ViewOnly)",
        );
    }

    #[test]
    fn granted_view_only_choice_is_not_view_and_control() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        assert_ne!(
            screen.state(),
            ConsentScreenState::Granted(GrantChoice::ViewAndControl),
            "ViewOnly must be distinguishable from ViewAndControl so the daemon withholds ControlGrant",
        );
    }

    // ── View + control grant ──────────────────────────────────────────────────

    #[test]
    fn grant_view_and_control_from_pending_transitions_to_granted_view_and_control() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewAndControl);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Granted(GrantChoice::ViewAndControl),
        );
    }

    // ── Denial ────────────────────────────────────────────────────────────────

    #[test]
    fn deny_from_pending_transitions_to_denied() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.deny();
        assert_eq!(screen.state(), ConsentScreenState::Denied);
    }

    // ── No-ops when not pending ───────────────────────────────────────────────

    #[test]
    fn grant_without_pending_request_is_noop() {
        let mut screen = ConsentScreen::new();
        screen.grant(GrantChoice::ViewOnly);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Idle,
            "grant while Idle must not change state",
        );
    }

    #[test]
    fn deny_without_pending_request_is_noop() {
        let mut screen = ConsentScreen::new();
        screen.deny();
        assert_eq!(
            screen.state(),
            ConsentScreenState::Idle,
            "deny while Idle must not change state",
        );
    }

    #[test]
    fn grant_after_granted_is_noop() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        screen.grant(GrantChoice::ViewAndControl);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Granted(GrantChoice::ViewOnly),
            "second grant call must not override first decision",
        );
    }

    #[test]
    fn deny_after_denied_is_noop() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.deny();
        screen.grant(GrantChoice::ViewOnly);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Denied,
            "grant after deny must not override denial",
        );
    }

    // ── Reset ─────────────────────────────────────────────────────────────────

    #[test]
    fn reset_from_granted_returns_to_idle() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        screen.reset();
        assert_eq!(screen.state(), ConsentScreenState::Idle);
    }

    #[test]
    fn reset_from_denied_returns_to_idle() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.deny();
        screen.reset();
        assert_eq!(screen.state(), ConsentScreenState::Idle);
    }

    #[test]
    fn reset_from_pending_returns_to_idle() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.reset();
        assert_eq!(screen.state(), ConsentScreenState::Idle);
    }

    // ── Session end ───────────────────────────────────────────────────────────

    #[test]
    fn session_end_from_pending_returns_to_idle() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.on_session_ended();
        assert_eq!(screen.state(), ConsentScreenState::Idle);
    }

    #[test]
    fn session_end_from_granted_returns_to_idle() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewAndControl);
        screen.on_session_ended();
        assert_eq!(screen.state(), ConsentScreenState::Idle);
    }

    #[test]
    fn new_request_after_session_end_is_accepted() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        screen.on_session_ended();

        screen.on_consent_request();
        assert_eq!(
            screen.state(),
            ConsentScreenState::PendingRequest,
            "new session must accept fresh consent request after prior session ended",
        );
    }

    // ── Round-trip: view-only → reset → view+control ──────────────────────────

    #[test]
    fn re_consent_can_upgrade_from_view_only_to_view_and_control() {
        let mut screen = ConsentScreen::new();
        screen.on_consent_request();
        screen.grant(GrantChoice::ViewOnly);
        screen.reset();

        screen.on_consent_request();
        screen.grant(GrantChoice::ViewAndControl);
        assert_eq!(
            screen.state(),
            ConsentScreenState::Granted(GrantChoice::ViewAndControl),
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn consent_screen_title_is_nonempty() {
        assert!(!CONSENT_SCREEN_TITLE.is_empty());
    }

    #[test]
    fn grant_view_only_label_is_nonempty() {
        assert!(!GRANT_VIEW_ONLY_LABEL.is_empty());
    }

    #[test]
    fn grant_view_and_control_label_is_nonempty() {
        assert!(!GRANT_VIEW_AND_CONTROL_LABEL.is_empty());
    }

    #[test]
    fn deny_label_is_nonempty() {
        assert!(!DENY_LABEL.is_empty());
    }

    // ── GrantChoice is Copy ───────────────────────────────────────────────────

    #[test]
    fn grant_choice_is_copy() {
        let choice = GrantChoice::ViewOnly;
        let _c2 = choice;
        let _c3 = choice;
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            ConsentScreen::new().state(),
            ConsentScreen::default().state(),
        );
    }
}

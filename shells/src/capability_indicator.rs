//! Persistent capability indicator — Feature 142.
//!
//! Tracks live capabilities from `CapabilityGrant` IPC events and exposes the
//! display state the UI shell must render as a persistent "you are being helped"
//! indicator on the assisted machine.
//!
//! The indicator is **always visible** whenever any capability (view, control,
//! file, or clipboard) is live and **cannot be suppressed** — Ana must always
//! know when Tan has active access to her machine.  The design system specifies
//! a subtle pulse animation while the indicator is shown (see `animations` in
//! the design spec).
//!
//! # States
//!
//! | State | Condition | Rendering |
//! |-------|-----------|-----------|
//! | `Hidden` | No session, or session with no live capabilities | Indicator not shown |
//! | `Live` | At least one capability is active | Persistent pulsing indicator shown |
//!
//! # Design
//!
//! - [`INDICATOR_COLOR`] (`#16a34a`, green) — Consent token; signals active access
//! - [`INDICATOR_LABEL`] — rendered text on the persistent indicator
//!
//! # Usage
//!
//! ```
//! use lowband_shells::capability_indicator::{CapabilityIndicator, CapabilityIndicatorState};
//!
//! let mut indicator = CapabilityIndicator::new();
//!
//! // Session established, view grant issued.
//! indicator.set_session_active(true);
//! indicator.set_capability_live(true);
//! assert_eq!(indicator.state(), CapabilityIndicatorState::Live);
//!
//! // All capabilities revoked — indicator hides.
//! indicator.set_capability_live(false);
//! assert_eq!(indicator.state(), CapabilityIndicatorState::Hidden);
//!
//! // Session ends.
//! indicator.set_session_active(false);
//! assert_eq!(indicator.state(), CapabilityIndicatorState::Hidden);
//! ```

/// Consent green used for the persistent live-capability indicator.
///
/// RGB hex `#16a34a` — matches the `Consent` token in the LowBand design
/// system, signalling that an active grant is in effect.
pub const INDICATOR_COLOR: &str = "#16a34a";

/// Text rendered on the indicator while any capability is live.
pub const INDICATOR_LABEL: &str = "You are being helped";

/// Display state for the persistent capability indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityIndicatorState {
    /// No session is active, or a session is active but no capability grants
    /// have been issued.  The indicator must not be rendered.
    Hidden,
    /// At least one capability (view, control, file, or clipboard) is live.
    /// The indicator must be rendered persistently with label
    /// [`INDICATOR_LABEL`] in colour [`INDICATOR_COLOR`] and a subtle pulse
    /// animation.  This state cannot be suppressed.
    Live,
}

/// Tracks live capability grants from IPC events and derives the persistent
/// indicator display state.
///
/// Construct one `CapabilityIndicator` per in-session UI instance.  Drive it with:
/// - [`CapabilityIndicator::set_session_active`] on `IpcEvent::SessionState`.
/// - [`CapabilityIndicator::set_capability_live`] on any `IpcEvent::CapabilityGrant`
///   or `IpcEvent::CapabilityRevoked`.
///
/// Read [`CapabilityIndicator::state`] on every IPC event and re-render the
/// indicator when the returned [`CapabilityIndicatorState`] changes.
pub struct CapabilityIndicator {
    session_active: bool,
    any_capability_live: bool,
}

impl CapabilityIndicator {
    /// Create a new capability indicator in the idle (no session) state.
    pub fn new() -> Self {
        Self { session_active: false, any_capability_live: false }
    }

    /// Record that the LBTP session became active (`active = true`) or ended
    /// (`active = false`).
    ///
    /// Ending the session clears all live capability state so the next session
    /// starts from a clean [`Hidden`](CapabilityIndicatorState::Hidden) state.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        if !active {
            self.any_capability_live = false;
        }
    }

    /// Record that at least one capability grant became live (`live = true`)
    /// or that all grants have been withdrawn (`live = false`).
    ///
    /// Call with `true` on any `IpcEvent::CapabilityGrant` (view, control,
    /// file, or clipboard).  Call with `false` when the daemon confirms that
    /// all tokens have been revoked or the panic key has severed access.
    ///
    /// No-op when no session is active.
    pub fn set_capability_live(&mut self, live: bool) {
        if self.session_active {
            self.any_capability_live = live;
        }
    }

    /// Return the current indicator display state.
    pub fn state(&self) -> CapabilityIndicatorState {
        if self.session_active && self.any_capability_live {
            CapabilityIndicatorState::Live
        } else {
            CapabilityIndicatorState::Hidden
        }
    }
}

impl Default for CapabilityIndicator {
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
    fn initial_state_is_hidden() {
        assert_eq!(CapabilityIndicator::new().state(), CapabilityIndicatorState::Hidden);
    }

    // ── No session ────────────────────────────────────────────────────────────

    #[test]
    fn capability_live_without_session_is_hidden() {
        let mut ind = CapabilityIndicator::new();
        ind.set_capability_live(true);
        assert_eq!(ind.state(), CapabilityIndicatorState::Hidden);
    }

    // ── Session active, no capabilities ───────────────────────────────────────

    #[test]
    fn session_active_without_capability_is_hidden() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        assert_eq!(ind.state(), CapabilityIndicatorState::Hidden);
    }

    // ── Capability live ───────────────────────────────────────────────────────

    #[test]
    fn session_and_capability_shows_live() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        assert_eq!(ind.state(), CapabilityIndicatorState::Live);
    }

    // ── Revocation hides indicator ────────────────────────────────────────────

    #[test]
    fn revoking_capability_hides_indicator() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        ind.set_capability_live(false);
        assert_eq!(ind.state(), CapabilityIndicatorState::Hidden);
    }

    // ── Session end ───────────────────────────────────────────────────────────

    #[test]
    fn session_end_hides_indicator() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        ind.set_session_active(false);
        assert_eq!(ind.state(), CapabilityIndicatorState::Hidden);
    }

    #[test]
    fn session_end_clears_capability_for_next_session() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        ind.set_session_active(false);

        ind.set_session_active(true);
        assert_eq!(
            ind.state(),
            CapabilityIndicatorState::Hidden,
            "new session must start without inherited capability state",
        );
    }

    #[test]
    fn new_capability_grant_after_session_restart_shows_live() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        ind.set_session_active(false);

        ind.set_session_active(true);
        ind.set_capability_live(true);
        assert_eq!(ind.state(), CapabilityIndicatorState::Live);
    }

    // ── Idempotent transitions ────────────────────────────────────────────────

    #[test]
    fn setting_capability_live_twice_stays_live() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        ind.set_capability_live(true);
        assert_eq!(ind.state(), CapabilityIndicatorState::Live);
    }

    #[test]
    fn setting_capability_false_twice_stays_hidden() {
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(false);
        ind.set_capability_live(false);
        assert_eq!(ind.state(), CapabilityIndicatorState::Hidden);
    }

    // ── Feature 142: indicator cannot be suppressed while capability is live ──

    #[test]
    fn indicator_is_live_whenever_any_capability_is_granted() {
        // Simulate view-only grant (the minimum capability).
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        ind.set_capability_live(true);
        assert_eq!(
            ind.state(),
            CapabilityIndicatorState::Live,
            "indicator must be Live for any active capability, including view-only",
        );
    }

    #[test]
    fn indicator_is_hidden_with_session_but_no_grant() {
        // Session up but no capability issued yet — indicator must not show.
        let mut ind = CapabilityIndicator::new();
        ind.set_session_active(true);
        assert_eq!(
            ind.state(),
            CapabilityIndicatorState::Hidden,
            "indicator must remain Hidden until a capability grant is live",
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn indicator_color_is_consent_green() {
        assert_eq!(INDICATOR_COLOR, "#16a34a");
    }

    #[test]
    fn indicator_label_is_nonempty() {
        assert!(!INDICATOR_LABEL.is_empty());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            CapabilityIndicator::new().state(),
            CapabilityIndicator::default().state(),
        );
    }

    // ── CapabilityIndicatorState is Copy ─────────────────────────────────────

    #[test]
    fn capability_indicator_state_is_copy() {
        let s = CapabilityIndicatorState::Live;
        let _s2 = s;
        let _s3 = s;
    }
}

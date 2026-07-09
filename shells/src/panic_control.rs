//! Panic control button — Feature 145.
//!
//! Tracks control-injection state and panic events from IPC and exposes the
//! display state the UI shell must render for the in-session panic control.
//!
//! When the assisted user presses the panic button, the UI sends a panic
//! command to the daemon which calls `PanicController::fire_panic`.  This
//! severs injection on **both** sides within 50 ms (Feature 29 SLA) while
//! keeping the LBTP transport alive so the voice call continues (Feature 30).
//!
//! # States
//!
//! | State | Condition | Rendering |
//! |-------|-----------|-----------|
//! | `NoSession` | No active session | Button not shown |
//! | `ControlNotGranted` | Session active, no control grant | Button hidden |
//! | `ControlActive` | Control injection live | Red panic button visible |
//! | `InjectionSevered` | Panic fired, call still live | Green severed indicator |
//!
//! # Design colours
//!
//! - [`PANIC_BUTTON_COLOR`] (`#dc2626`, red) — the active panic button
//! - [`PANIC_SEVERED_COLOR`] (`#16a34a`, green) — after injection is severed
//!
//! # Usage
//!
//! ```
//! use lowband_shells::panic_control::{PanicControl, PanicControlState};
//!
//! let mut pc = PanicControl::new();
//!
//! // Session established with control grant.
//! pc.set_session_active(true);
//! pc.set_control_granted(true);
//! assert_eq!(pc.state(), PanicControlState::ControlActive);
//!
//! // Assisted user presses the panic button; daemon confirms panic fired.
//! pc.on_panic_fired();
//! assert_eq!(pc.state(), PanicControlState::InjectionSevered);
//!
//! // Session ends.
//! pc.set_session_active(false);
//! assert_eq!(pc.state(), PanicControlState::NoSession);
//! ```

/// Red danger colour for the active panic control button.
///
/// RGB hex `#dc2626` — matches the `Danger` token in the LowBand design system.
pub const PANIC_BUTTON_COLOR: &str = "#dc2626";

/// Green colour shown once injection has been severed by the panic control.
///
/// RGB hex `#16a34a` — matches the `Consent` / panic-cleared token in the
/// LowBand design system, signalling the session is now safe.
pub const PANIC_SEVERED_COLOR: &str = "#16a34a";

/// Label rendered on the panic button when control injection is active.
pub const PANIC_BUTTON_LABEL: &str = "Stop control";

/// Label shown when injection has been severed but the call continues.
pub const PANIC_SEVERED_LABEL: &str = "Control severed";

/// Display state for the panic control button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicControlState {
    /// No active session; the panic control must not be rendered.
    NoSession,
    /// A session is active but no control grant has been issued; the panic
    /// control is hidden because there is no injection to sever.
    ControlNotGranted,
    /// Control injection is live; the panic button must be rendered with label
    /// [`PANIC_BUTTON_LABEL`] in colour [`PANIC_BUTTON_COLOR`].
    ControlActive,
    /// Panic was fired; injection is severed on both sides but the LBTP
    /// transport remains alive (Feature 30).  Render a persistent indicator
    /// with label [`PANIC_SEVERED_LABEL`] in colour [`PANIC_SEVERED_COLOR`].
    InjectionSevered,
}

/// Tracks control-injection and panic state from IPC events and derives the
/// panic control display state.
///
/// Construct one `PanicControl` per in-session UI instance.  Drive it with:
/// - [`PanicControl::set_session_active`] on `IpcEvent::SessionState`.
/// - [`PanicControl::set_control_granted`] on `IpcEvent::ControlGrant`.
/// - [`PanicControl::on_panic_fired`] on `IpcEvent::PanicFired`.
///
/// Read [`PanicControl::state`] on every IPC event and re-render the panic
/// control when the returned [`PanicControlState`] changes.
pub struct PanicControl {
    session_active:  bool,
    control_granted: bool,
    panic_fired:     bool,
}

impl PanicControl {
    /// Create a new panic control in the idle (no session) state.
    pub fn new() -> Self {
        Self { session_active: false, control_granted: false, panic_fired: false }
    }

    /// Record that the LBTP session became active (`active = true`) or ended
    /// (`active = false`).
    ///
    /// Ending the session resets the control grant and panic flags so the next
    /// session starts from a clean state.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        if !active {
            self.control_granted = false;
            self.panic_fired = false;
        }
    }

    /// Record that the control capability grant was issued (`granted = true`)
    /// or revoked (`granted = false`).
    ///
    /// A fresh grant issued after a re-consent flow clears the `panic_fired`
    /// flag so the UI transitions back to [`PanicControlState::ControlActive`].
    pub fn set_control_granted(&mut self, granted: bool) {
        self.control_granted = granted;
        if granted {
            self.panic_fired = false;
        }
    }

    /// Record that the daemon fired the panic key (`IpcEvent::PanicFired`).
    ///
    /// Sets the `panic_fired` flag, which takes priority over `control_granted`
    /// in [`state`](Self::state).  The call is still live — only injection is
    /// severed (Feature 30).  No-op when no session is active.
    pub fn on_panic_fired(&mut self) {
        if self.session_active {
            self.panic_fired = true;
        }
    }

    /// Return the current panic control display state.
    ///
    /// Priority order (highest first):
    /// 1. No session → [`PanicControlState::NoSession`]
    /// 2. Panic fired → [`PanicControlState::InjectionSevered`]
    /// 3. Control grant held → [`PanicControlState::ControlActive`]
    /// 4. Otherwise → [`PanicControlState::ControlNotGranted`]
    pub fn state(&self) -> PanicControlState {
        if !self.session_active {
            return PanicControlState::NoSession;
        }
        if self.panic_fired {
            return PanicControlState::InjectionSevered;
        }
        if self.control_granted {
            PanicControlState::ControlActive
        } else {
            PanicControlState::ControlNotGranted
        }
    }
}

impl Default for PanicControl {
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
        assert_eq!(PanicControl::new().state(), PanicControlState::NoSession);
    }

    // ── Session active, no control grant ──────────────────────────────────────

    #[test]
    fn session_active_without_control_grant_is_control_not_granted() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        assert_eq!(pc.state(), PanicControlState::ControlNotGranted);
    }

    // ── Control active ────────────────────────────────────────────────────────

    #[test]
    fn session_and_control_grant_shows_control_active() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        assert_eq!(pc.state(), PanicControlState::ControlActive);
    }

    // ── Panic fired ───────────────────────────────────────────────────────────

    #[test]
    fn on_panic_fired_transitions_to_injection_severed() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();
        assert_eq!(pc.state(), PanicControlState::InjectionSevered);
    }

    #[test]
    fn panic_fires_even_without_prior_control_grant() {
        // Physical panic key may fire before the IPC grant event arrives.
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.on_panic_fired();
        assert_eq!(pc.state(), PanicControlState::InjectionSevered);
    }

    #[test]
    fn on_panic_fired_without_session_is_noop() {
        let mut pc = PanicControl::new();
        pc.on_panic_fired();
        assert_eq!(pc.state(), PanicControlState::NoSession);
    }

    // ── Grant revocation without panic ────────────────────────────────────────

    #[test]
    fn revoking_control_grant_transitions_to_control_not_granted() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.set_control_granted(false);
        assert_eq!(pc.state(), PanicControlState::ControlNotGranted);
    }

    // ── Re-consent after panic ────────────────────────────────────────────────

    #[test]
    fn fresh_control_grant_after_panic_restores_control_active() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();
        assert_eq!(pc.state(), PanicControlState::InjectionSevered);

        // Assisted user completes a new consent flow; daemon issues a fresh grant.
        pc.set_control_granted(true);
        assert_eq!(pc.state(), PanicControlState::ControlActive);
    }

    // ── Session end resets everything ─────────────────────────────────────────

    #[test]
    fn session_end_returns_to_no_session() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();
        pc.set_session_active(false);
        assert_eq!(pc.state(), PanicControlState::NoSession);
    }

    #[test]
    fn session_end_clears_panic_and_grant_for_next_session() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();
        pc.set_session_active(false);

        pc.set_session_active(true);
        assert_eq!(
            pc.state(),
            PanicControlState::ControlNotGranted,
            "new session must start without a control grant",
        );
    }

    #[test]
    fn second_session_with_grant_is_control_active_not_severed() {
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();
        pc.set_session_active(false);

        pc.set_session_active(true);
        pc.set_control_granted(true);
        assert_eq!(
            pc.state(),
            PanicControlState::ControlActive,
            "second session must not inherit panic_fired from the previous session",
        );
    }

    // ── Feature 145: severs injection on both sides ───────────────────────────

    #[test]
    fn panic_reports_injection_severed_on_ui_side() {
        // This is the UI side of the "both sides" contract.  The daemon side is
        // exercised by PanicController::fire_panic (Feature 29/30 unit tests).
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        assert_eq!(pc.state(), PanicControlState::ControlActive, "precondition");

        pc.on_panic_fired();

        assert_eq!(
            pc.state(),
            PanicControlState::InjectionSevered,
            "UI must report InjectionSevered after panic fires on both sides",
        );
    }

    #[test]
    fn transport_stays_alive_after_panic() {
        // Feature 30 regression guard: InjectionSevered must not imply the
        // session ended — only injection is off, the call continues.
        let mut pc = PanicControl::new();
        pc.set_session_active(true);
        pc.set_control_granted(true);
        pc.on_panic_fired();

        // Session remains active — further IPC events are still valid.
        pc.set_session_active(true);
        assert_eq!(
            pc.state(),
            PanicControlState::InjectionSevered,
            "reasserting session active must keep InjectionSevered; transport is still up",
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn panic_button_color_is_danger_red() {
        assert_eq!(PANIC_BUTTON_COLOR, "#dc2626");
    }

    #[test]
    fn panic_severed_color_is_consent_green() {
        assert_eq!(PANIC_SEVERED_COLOR, "#16a34a");
    }

    #[test]
    fn panic_button_label_is_nonempty() {
        assert!(!PANIC_BUTTON_LABEL.is_empty());
    }

    #[test]
    fn panic_severed_label_is_nonempty() {
        assert!(!PANIC_SEVERED_LABEL.is_empty());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(PanicControl::new().state(), PanicControl::default().state());
    }

    // ── PanicControlState is Copy ─────────────────────────────────────────────

    #[test]
    fn panic_control_state_is_copy() {
        let s = PanicControlState::ControlActive;
        let _s2 = s;
        let _s3 = s;
    }
}

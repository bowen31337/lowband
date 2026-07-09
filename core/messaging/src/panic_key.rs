//! Panic-key state machine — Feature 30.
//!
//! When the assisted user presses the panic key the system must:
//!
//! 1. Block `input_injection` immediately (Feature 29; enforced by the caller
//!    via [`ControlSession::set_grant(None)`](crate::grants::ControlSession::set_grant)).
//! 2. Keep the LBTP transport alive so the humans can keep talking (Feature 30;
//!    this module).
//!
//! [`PanicController`] tracks the `transport_up` liveness flag independently of
//! the injection gate.  After a panic the flag stays `true` even though injection
//! is severed, preserving voice and screen-view continuity.
//!
//! # State machine
//!
//! ```text
//! idle ──set_transport_up(true)──► running
//!          transport_up=false         transport_up=true
//!          panic_fired=false          panic_fired=false
//!                                          │
//!                                     fire_panic()
//!                                          │ transport stays UP
//!                                          ▼
//!                                      panicked
//!                                      transport_up=true
//!                                      panic_fired=true
//!                                          │
//!                              clear_panic() or set_transport_up(false)
//! ```
//!
//! # Example
//!
//! ```
//! use lowband_messaging::panic_key::PanicController;
//! use lowband_messaging::grants::ControlSession;
//!
//! let mut ctrl = ControlSession::new();
//! let mut pc   = PanicController::new();
//!
//! // Session established.
//! pc.set_transport_up(true);
//!
//! // Panic key pressed — injection severed, transport stays up.
//! let effect = pc.fire_panic(&mut ctrl);
//! assert!(effect.injection_revoked);
//! assert!(effect.transport_up);
//! assert!(pc.transport_up());
//! ```

use crate::grants::ControlSession;

/// Maximum time (milliseconds) from panic-key press to the first rejected
/// injection event — Feature 29 SLA.
///
/// `fire_panic` is synchronous: the control grant is revoked in the same call,
/// so the actual latency is sub-microsecond.  The constant lets bench tests
/// gate the SLA without hard-coding the number in multiple places.
pub const PANIC_INJECTION_BLOCK_DEADLINE_MS: u64 = 50;

/// Returned by [`PanicController::fire_panic`]; describes what the panic
/// activation changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanicEffect {
    /// `true` when `fire_panic` revoked the active control grant.
    ///
    /// The caller must call `control_session.set_grant(None)` before
    /// `fire_panic` returns — this field confirms it happened.
    pub injection_revoked: bool,

    /// `true` when the LBTP transport remains live after the panic.
    ///
    /// A `true` value here means voice and screen-view continuity is
    /// preserved — the humans can keep talking (Feature 30).
    pub transport_up: bool,
}

/// Panic-key controller.
///
/// Manages the `transport_up` liveness flag and the `panic_fired` status
/// flag as a pair so callers can reliably distinguish:
///
/// | `transport_up` | `panic_fired` | Meaning |
/// |---|---|---|
/// | `false` | `false` | No active session |
/// | `true` | `false` | Session live, injection allowed |
/// | `true` | `true` | Panic fired — injection off, transport on |
///
/// The underlying LBTP `Connection` is never touched by this type; the
/// caller keeps the connection object alive independently, which is the
/// mechanism that keeps the call active.
pub struct PanicController {
    transport_up: bool,
    panic_fired: bool,
}

impl PanicController {
    /// Create a controller in the idle (no active session) state.
    pub fn new() -> Self {
        Self { transport_up: false, panic_fired: false }
    }

    /// Signal that the LBTP connection was established (`up = true`) or
    /// disconnected (`up = false`).
    ///
    /// A clean session disconnect also resets the `panic_fired` flag because
    /// the next session starts fresh.
    pub fn set_transport_up(&mut self, up: bool) {
        self.transport_up = up;
        if !up {
            self.panic_fired = false;
        }
    }

    /// `true` while the LBTP transport connection is live.
    ///
    /// Remains `true` after a panic so that voice and screen-view channels
    /// can continue operating (Feature 30).
    pub fn transport_up(&self) -> bool {
        self.transport_up
    }

    /// `true` after [`fire_panic`](Self::fire_panic) has been called this
    /// session.
    pub fn panic_fired(&self) -> bool {
        self.panic_fired
    }

    /// Activate the panic key.
    ///
    /// Revokes the active control grant on `control` (setting it to `None`)
    /// and marks `panic_fired`.  The LBTP transport is **not** torn down —
    /// `transport_up` stays `true` so the humans can keep talking.
    ///
    /// Returns a [`PanicEffect`] describing what changed.  When the transport
    /// is not up there is no active session to panic; both effect fields are
    /// `false` and `control` is left unchanged.
    pub fn fire_panic(&mut self, control: &mut ControlSession) -> PanicEffect {
        if !self.transport_up {
            return PanicEffect { injection_revoked: false, transport_up: false };
        }
        self.panic_fired = true;
        control.set_grant(None);
        PanicEffect { injection_revoked: true, transport_up: true }
    }

    /// Clear the `panic_fired` flag (e.g. the assisted user re-consented after
    /// reviewing what happened).
    ///
    /// Does **not** restore the control grant — a fresh consent flow must be
    /// completed before injection can resume.
    pub fn clear_panic(&mut self) {
        self.panic_fired = false;
    }
}

impl Default for PanicController {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{CapabilityError, ControlGrant, ControlSession};

    fn active_control() -> ControlSession {
        let mut s = ControlSession::new();
        s.set_grant(Some(ControlGrant::new()));
        s
    }

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn new_transport_is_down() {
        let pc = PanicController::new();
        assert!(!pc.transport_up());
    }

    #[test]
    fn new_panic_not_fired() {
        let pc = PanicController::new();
        assert!(!pc.panic_fired());
    }

    // ── set_transport_up ─────────────────────────────────────────────────────

    #[test]
    fn set_transport_up_true_marks_transport_live() {
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        assert!(pc.transport_up());
    }

    #[test]
    fn set_transport_up_false_marks_transport_down() {
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.set_transport_up(false);
        assert!(!pc.transport_up());
    }

    #[test]
    fn disconnect_resets_panic_fired_flag() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.fire_panic(&mut ctrl);
        assert!(pc.panic_fired());

        pc.set_transport_up(false);
        assert!(!pc.panic_fired(), "disconnect must clear panic_fired for next session");
    }

    // ── fire_panic: Feature 30 core invariant ────────────────────────────────

    #[test]
    fn fire_panic_keeps_transport_up() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);

        let effect = pc.fire_panic(&mut ctrl);

        assert!(effect.transport_up, "transport must stay up after panic (Feature 30)");
        assert!(pc.transport_up(),   "transport_up state must remain true after panic");
    }

    #[test]
    fn fire_panic_revokes_injection() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);

        let effect = pc.fire_panic(&mut ctrl);

        assert!(effect.injection_revoked, "panic effect must report injection_revoked");
        assert_eq!(
            ctrl.apply_event(),
            Err(CapabilityError::NoActiveGrant),
            "injection must be blocked after panic",
        );
    }

    #[test]
    fn fire_panic_sets_panic_fired_flag() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.fire_panic(&mut ctrl);
        assert!(pc.panic_fired());
    }

    // ── fire_panic when transport is down ────────────────────────────────────

    #[test]
    fn fire_panic_noop_when_transport_down() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();

        let effect = pc.fire_panic(&mut ctrl);

        assert!(!effect.injection_revoked, "no injection to revoke when transport is down");
        assert!(!effect.transport_up,      "transport was not up, so effect reports down");
        assert!(!pc.panic_fired(),         "panic_fired must not be set when transport is down");
        assert!(
            ctrl.apply_event().is_ok(),
            "control grant must be untouched when transport is down",
        );
    }

    // ── Fire panic multiple times is idempotent ───────────────────────────────

    #[test]
    fn fire_panic_twice_leaves_transport_up() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);

        pc.fire_panic(&mut ctrl);
        let effect2 = pc.fire_panic(&mut ctrl);

        assert!(effect2.transport_up, "second panic must not drop transport");
        assert!(pc.transport_up());
    }

    // ── clear_panic ───────────────────────────────────────────────────────────

    #[test]
    fn clear_panic_resets_panic_fired() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.fire_panic(&mut ctrl);

        pc.clear_panic();

        assert!(!pc.panic_fired(), "clear_panic must reset the panic_fired flag");
    }

    #[test]
    fn clear_panic_does_not_restore_grant() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.fire_panic(&mut ctrl);
        pc.clear_panic();

        assert_eq!(
            ctrl.apply_event(),
            Err(CapabilityError::NoActiveGrant),
            "clear_panic must not restore the control grant",
        );
    }

    #[test]
    fn clear_panic_does_not_affect_transport_up() {
        let mut ctrl = active_control();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);
        pc.fire_panic(&mut ctrl);
        pc.clear_panic();

        assert!(pc.transport_up(), "clear_panic must not touch transport_up");
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        let a = PanicController::new();
        let b = PanicController::default();
        assert_eq!(a.transport_up(), b.transport_up());
        assert_eq!(a.panic_fired(),  b.panic_fired());
    }

    // ── PanicEffect fields ────────────────────────────────────────────────────

    #[test]
    fn panic_effect_is_copy() {
        let e = PanicEffect { injection_revoked: true, transport_up: true };
        let _e2 = e;
        let _e3 = e;
    }

    // ── Narrative: Feature 30 end-to-end scenario ─────────────────────────────

    #[test]
    fn humans_keep_talking_after_panic() {
        // Simulate: session established → panic key pressed → voice continues.
        let mut ctrl = active_control();
        let mut pc = PanicController::new();

        // LBTP session comes up.
        pc.set_transport_up(true);
        assert!(pc.transport_up());
        assert!(ctrl.apply_event().is_ok(), "injection allowed before panic");

        // Ana presses the panic key.
        let effect = pc.fire_panic(&mut ctrl);

        // Injection is severed …
        assert!(effect.injection_revoked);
        assert_eq!(ctrl.apply_event(), Err(CapabilityError::NoActiveGrant));

        // … but the transport remains up so the conversation continues.
        assert!(effect.transport_up,  "transport must stay up (Feature 30)");
        assert!(pc.transport_up(),    "PanicController confirms transport_up after panic");
        assert!(pc.panic_fired());
    }
}

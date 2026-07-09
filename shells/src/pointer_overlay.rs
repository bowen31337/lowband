//! Technician pointer overlay — Feature 66.
//!
//! Consumes 60 Hz cursor-position deltas from the reliable-ordered cursor
//! channel and exposes the display state the viewing UI must render as a
//! floating "technician cursor" overlay.  The overlay is active only in
//! **view-only mode**: when Tan (the technician) has a live `view_grant` but
//! has **not** been issued a `control_grant`.
//!
//! When the assisted user grants full view-and-control access the technician's
//! pointer drives the real OS cursor, so the overlay is suppressed to avoid a
//! double-cursor effect.
//!
//! # States
//!
//! | State | Condition | Rendering |
//! |-------|-----------|-----------|
//! | `Hidden` | No session, or control is granted (real cursor is in use) | Overlay not shown |
//! | `Visible { x, y }` | View-only session, cursor-channel delta received | Overlay dot at `(x, y)` pixels |
//!
//! # Design
//!
//! - [`OVERLAY_COLOR`] (`#4f46e5`, indigo) — distinct from consent-green; marks
//!   the pointer as a technician's guide cursor, not a live-control cursor
//! - [`OVERLAY_LABEL`] — tooltip text shown beside the overlay dot
//!
//! # Usage
//!
//! ```
//! use lowband_shells::pointer_overlay::{PointerOverlay, PointerOverlayState};
//!
//! let mut overlay = PointerOverlay::new();
//!
//! // Session established in view-only mode.
//! overlay.set_session_active(true);
//! overlay.set_view_only(true);
//!
//! // Cursor-channel delta arrives (60 Hz).
//! overlay.apply_delta(320.0, 240.0);
//! assert_eq!(overlay.state(), PointerOverlayState::Visible { x: 320.0, y: 240.0 });
//!
//! // Further delta.
//! overlay.apply_delta(10.0, -5.0);
//! assert_eq!(overlay.state(), PointerOverlayState::Visible { x: 330.0, y: 235.0 });
//!
//! // Control granted — overlay suppressed (real cursor takes over).
//! overlay.set_view_only(false);
//! assert_eq!(overlay.state(), PointerOverlayState::Hidden);
//! ```

/// Indigo used for the technician pointer overlay dot and tooltip border.
///
/// RGB hex `#4f46e5` — visually distinct from the consent-green capability
/// indicator so assisted users can distinguish "being watched" from "being guided".
pub const OVERLAY_COLOR: &str = "#4f46e5";

/// Tooltip text rendered beside the overlay dot while it is visible.
pub const OVERLAY_LABEL: &str = "Technician";

/// Display state for the technician pointer overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerOverlayState {
    /// The overlay must not be rendered.
    ///
    /// Either there is no active session, or the technician has been granted
    /// full control (real OS cursor is in use; an overlay would double-cursor).
    Hidden,

    /// The overlay must be rendered at the given pixel coordinates.
    ///
    /// `x` and `y` are in the coordinate space of the remote display frame as
    /// delivered by the screen codec; the UI composites the overlay dot at this
    /// position on top of the rendered frame.
    Visible {
        /// Horizontal position in remote-display pixels.
        x: f64,
        /// Vertical position in remote-display pixels.
        y: f64,
    },
}

/// Tracks cursor-channel deltas and derives the pointer overlay display state.
///
/// Construct one `PointerOverlay` per viewing session.  Drive it with:
/// - [`set_session_active`](Self::set_session_active) on session start/end.
/// - [`set_view_only`](Self::set_view_only) when the grant mode changes.
/// - [`apply_delta`](Self::apply_delta) on every 60 Hz cursor-channel message.
///
/// Read [`state`](Self::state) after each `apply_delta` call and re-render the
/// overlay whenever the returned [`PointerOverlayState`] changes.
pub struct PointerOverlay {
    session_active: bool,
    view_only: bool,
    x: f64,
    y: f64,
}

impl PointerOverlay {
    /// Create a new overlay in the idle (no session) state.
    ///
    /// The initial cursor position is `(0.0, 0.0)`; the first
    /// [`apply_delta`](Self::apply_delta) call moves it from there.
    pub fn new() -> Self {
        Self { session_active: false, view_only: false, x: 0.0, y: 0.0 }
    }

    /// Record that the LBTP session became active (`active = true`) or ended
    /// (`active = false`).
    ///
    /// Ending the session resets the grant mode and accumulated cursor position
    /// so the next session starts clean.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        if !active {
            self.view_only = false;
            self.x = 0.0;
            self.y = 0.0;
        }
    }

    /// Record that the grant mode is view-only (`view_only = true`) or that a
    /// control grant has been issued (`view_only = false`).
    ///
    /// - `true` → view-only: the technician's pointer is a guide cursor and
    ///   must be rendered as an overlay so the assisted user can follow it.
    /// - `false` → control granted: the real OS cursor is driven by the
    ///   technician; the overlay is suppressed to avoid a double-cursor effect.
    ///
    /// No-op when no session is active.
    pub fn set_view_only(&mut self, view_only: bool) {
        if self.session_active {
            self.view_only = view_only;
        }
    }

    /// Accumulate a cursor-channel delta into the running position.
    ///
    /// Called on every 60 Hz cursor-channel message.  `dx` and `dy` are
    /// signed pixel offsets in the remote-display coordinate space (positive
    /// `dx` moves right, positive `dy` moves down).
    ///
    /// No-op when the session is not active.  Deltas are accumulated regardless
    /// of the current grant mode so the position is up-to-date when the overlay
    /// becomes visible.
    pub fn apply_delta(&mut self, dx: f64, dy: f64) {
        if self.session_active {
            self.x += dx;
            self.y += dy;
        }
    }

    /// Return the current overlay display state.
    ///
    /// Returns [`PointerOverlayState::Visible`] only when the session is active
    /// and the grant mode is view-only.
    pub fn state(&self) -> PointerOverlayState {
        if self.session_active && self.view_only {
            PointerOverlayState::Visible { x: self.x, y: self.y }
        } else {
            PointerOverlayState::Hidden
        }
    }
}

impl Default for PointerOverlay {
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
        assert_eq!(PointerOverlay::new().state(), PointerOverlayState::Hidden);
    }

    // ── No session ────────────────────────────────────────────────────────────

    #[test]
    fn view_only_without_session_stays_hidden() {
        let mut ov = PointerOverlay::new();
        ov.set_view_only(true);
        assert_eq!(ov.state(), PointerOverlayState::Hidden);
    }

    #[test]
    fn delta_without_session_is_noop() {
        let mut ov = PointerOverlay::new();
        ov.apply_delta(100.0, 200.0);
        assert_eq!(ov.state(), PointerOverlayState::Hidden);
    }

    // ── Session active, control granted ───────────────────────────────────────

    #[test]
    fn session_with_control_is_hidden() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(false);
        ov.apply_delta(50.0, 80.0);
        assert_eq!(ov.state(), PointerOverlayState::Hidden);
    }

    // ── View-only mode ────────────────────────────────────────────────────────

    #[test]
    fn view_only_session_is_visible_after_delta() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(320.0, 240.0);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 320.0, y: 240.0 },
        );
    }

    #[test]
    fn view_only_visible_at_origin_before_any_delta() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 0.0, y: 0.0 },
        );
    }

    #[test]
    fn deltas_accumulate_correctly() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(100.0, 50.0);
        ov.apply_delta(-20.0, 30.0);
        ov.apply_delta(5.0, -5.0);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 85.0, y: 75.0 },
        );
    }

    #[test]
    fn subpixel_deltas_accumulate_without_rounding() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(0.5, 0.25);
        ov.apply_delta(0.5, 0.25);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 1.0, y: 0.5 },
        );
    }

    // ── Grant mode transitions ────────────────────────────────────────────────

    #[test]
    fn granting_control_hides_overlay() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(200.0, 100.0);
        assert!(matches!(ov.state(), PointerOverlayState::Visible { .. }));

        ov.set_view_only(false);
        assert_eq!(ov.state(), PointerOverlayState::Hidden);
    }

    #[test]
    fn revoking_control_restores_overlay_at_accumulated_position() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(200.0, 100.0);
        ov.set_view_only(false); // control granted — hidden

        // Delta during control mode is still tracked.
        ov.apply_delta(50.0, 25.0);
        ov.set_view_only(true); // back to view-only
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 250.0, y: 125.0 },
            "position must reflect deltas accumulated during control mode",
        );
    }

    // ── Session end ───────────────────────────────────────────────────────────

    #[test]
    fn session_end_hides_overlay() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(400.0, 300.0);
        ov.set_session_active(false);
        assert_eq!(ov.state(), PointerOverlayState::Hidden);
    }

    #[test]
    fn session_end_resets_position_for_next_session() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.apply_delta(999.0, 888.0);
        ov.set_session_active(false);

        ov.set_session_active(true);
        ov.set_view_only(true);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 0.0, y: 0.0 },
            "new session must start with cursor at origin, not inherited from previous session",
        );
    }

    #[test]
    fn session_end_resets_view_only_for_next_session() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        ov.set_session_active(false);

        // New session — view_only must default to false until explicitly set.
        ov.set_session_active(true);
        assert_eq!(
            ov.state(),
            PointerOverlayState::Hidden,
            "new session must not inherit view-only mode from previous session",
        );
    }

    // ── Feature 66: overlay is visible iff session is active and view-only ────

    #[test]
    fn overlay_visible_only_in_view_only_mode() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);
        assert!(
            matches!(ov.state(), PointerOverlayState::Visible { .. }),
            "pointer_overlay must be Visible when session is active and grant is view-only",
        );
    }

    #[test]
    fn overlay_hidden_when_control_grant_is_live() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        // view_only defaults to false (full control)
        assert_eq!(
            ov.state(),
            PointerOverlayState::Hidden,
            "pointer_overlay must be Hidden when a control_grant is live",
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn overlay_color_is_indigo() {
        assert_eq!(OVERLAY_COLOR, "#4f46e5");
    }

    #[test]
    fn overlay_label_is_nonempty() {
        assert!(!OVERLAY_LABEL.is_empty());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            PointerOverlay::new().state(),
            PointerOverlay::default().state(),
        );
    }

    // ── PointerOverlayState is Copy ───────────────────────────────────────────

    #[test]
    fn pointer_overlay_state_is_copy() {
        let s = PointerOverlayState::Visible { x: 1.0, y: 2.0 };
        let _s2 = s;
        let _s3 = s;
    }

    #[test]
    fn pointer_overlay_state_hidden_is_copy() {
        let s = PointerOverlayState::Hidden;
        let _s2 = s;
        let _s3 = s;
    }

    // ── Cursor channel: 60 Hz stress ─────────────────────────────────────────

    #[test]
    fn sixty_hz_deltas_accumulate_without_loss() {
        let mut ov = PointerOverlay::new();
        ov.set_session_active(true);
        ov.set_view_only(true);

        // Simulate 1 second of 60 Hz deltas of (1.0, 0.5) each.
        for _ in 0..60 {
            ov.apply_delta(1.0, 0.5);
        }
        assert_eq!(
            ov.state(),
            PointerOverlayState::Visible { x: 60.0, y: 30.0 },
        );
    }
}

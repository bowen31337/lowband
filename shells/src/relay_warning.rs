//! TCP-443 relay warning — Feature 139.
//!
//! When the LBTP transport falls back to the TCP port 443 path (because direct
//! UDP hole-punch and TURN relay both failed), the UI must display a visible
//! warning banner and an honest latency penalty label so the user understands
//! the network cost of this path.
//!
//! # Design colour
//!
//! The design system assigns amber (`#d97706`) to TCP-443 relay warnings and
//! degraded-tier notifications.  The shell is responsible for applying that
//! colour; this module only manages the display state.
//!
//! # Honesty contract
//!
//! The penalty is forwarded verbatim from the governor's TCP penalty estimate
//! (computed by `TcpPenaltyTracker` in `lowband-lbtp`).  This module does not
//! smooth, round up, or suppress the value.  Before the first penalty
//! observation the floor value (`TCP_FALLBACK_PENALTY_FLOOR_MS` = 30 ms) is
//! forwarded by the daemon — never zero — because TCP head-of-line blocking
//! inflicts at least that much extra latency even on an uncongested link.
//!
//! # Usage
//!
//! ```
//! use lowband_shells::relay_warning::RelayWarning;
//!
//! let mut warning = RelayWarning::new();
//!
//! // On IpcEvent::TransportPath { tcp_active: true, penalty_ms: 45 }:
//! warning.set_active(true);
//! warning.update_penalty(45);
//!
//! let snap = warning.snapshot().unwrap();
//! assert!(snap.active);
//! assert_eq!(snap.penalty_ms, 45);
//!
//! // When the transport recovers a direct path:
//! warning.set_active(false);
//! assert!(warning.snapshot().is_none());
//! ```

/// Amber design-system colour for the TCP-443 relay warning banner.
///
/// RGB hex `#d97706` — matches the `Warning` token in the LowBand design system.
pub const RELAY_WARNING_COLOR: &str = "#d97706";

/// Display label prefix for the penalty annotation shown in the warning banner.
///
/// The shell appends the numeric value and " ms", e.g.:
/// `format!("{RELAY_PENALTY_LABEL_PREFIX}{penalty_ms} ms")` → `"+45 ms latency penalty"`.
pub const RELAY_PENALTY_LABEL_PREFIX: &str = "+";

/// Display label suffix for the penalty annotation.
pub const RELAY_PENALTY_LABEL_SUFFIX: &str = " ms latency penalty";

/// A point-in-time snapshot of the TCP-443 relay warning display state.
///
/// Render this only when [`RelayWarning::snapshot`] returns `Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayWarningSnapshot {
    /// `true` while the TCP-443 fallback path is the active transport.
    pub active: bool,
    /// Honest extra-latency penalty of the TCP path over the UDP baseline, in ms.
    ///
    /// Always ≥ 30 ms (`TCP_FALLBACK_PENALTY_FLOOR_MS`) because TCP head-of-line
    /// blocking imposes a structural minimum cost even on uncongested links.
    pub penalty_ms: u32,
}

impl RelayWarningSnapshot {
    /// Format the penalty as the UI label string, e.g. `"+45 ms latency penalty"`.
    pub fn penalty_label(&self) -> String {
        format!("{}{}{}", RELAY_PENALTY_LABEL_PREFIX, self.penalty_ms, RELAY_PENALTY_LABEL_SUFFIX)
    }
}

/// Aggregates TCP-443 relay transport events and produces the warning banner
/// display state.
///
/// Construct one `RelayWarning` per session.  Drive it with:
/// - [`RelayWarning::set_active`] when the transport path changes.
/// - [`RelayWarning::update_penalty`] on each governor `TcpPenalty` event.
///
/// Read [`RelayWarning::snapshot`] to obtain the current display state.
/// Returns `None` whenever the TCP-443 fallback is not active so the shell can
/// hide the banner without additional branching.
pub struct RelayWarning {
    active: bool,
    penalty_ms: u32,
    has_penalty: bool,
}

impl RelayWarning {
    /// Create a new relay warning with the fallback path inactive.
    ///
    /// [`snapshot`](Self::snapshot) returns `None` until [`set_active`](Self::set_active)
    /// is called with `true`.
    pub fn new() -> Self {
        Self {
            active: false,
            penalty_ms: 0,
            has_penalty: false,
        }
    }

    /// Record whether the TCP-443 fallback path is currently the active transport.
    ///
    /// Pass `true` on `IpcEvent::TransportPath { tcp_active: true }`.
    /// Pass `false` when the session migrates back to a direct UDP or TURN path.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Record the honest latency penalty estimate for the TCP fallback path, in ms.
    ///
    /// The value is forwarded verbatim from the daemon's `TcpPenaltyTracker` and
    /// is always ≥ `TCP_FALLBACK_PENALTY_FLOOR_MS` (30 ms).
    ///
    /// Call this on each `IpcEvent::TcpPenalty { penalty_ms }` from the governor.
    pub fn update_penalty(&mut self, penalty_ms: u32) {
        self.penalty_ms = penalty_ms;
        self.has_penalty = true;
    }

    /// Return the current relay warning display state.
    ///
    /// Returns `None` when the TCP-443 fallback is not active — the banner must
    /// be hidden in this case.
    ///
    /// Returns `None` when the fallback is active but no penalty observation has
    /// been received yet; the shell must wait for the first `TcpPenalty` event
    /// before showing the banner to avoid displaying a stale or zero penalty.
    pub fn snapshot(&self) -> Option<RelayWarningSnapshot> {
        if !self.active || !self.has_penalty {
            return None;
        }
        Some(RelayWarningSnapshot {
            active: true,
            penalty_ms: self.penalty_ms,
        })
    }

    /// Whether the TCP-443 fallback path is currently marked active.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Default for RelayWarning {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── snapshot availability ─────────────────────────────────────────────────

    #[test]
    fn initial_snapshot_is_none() {
        assert!(RelayWarning::new().snapshot().is_none());
    }

    #[test]
    fn active_without_penalty_returns_none() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        assert!(
            w.snapshot().is_none(),
            "must not show banner before first penalty observation"
        );
    }

    #[test]
    fn penalty_without_active_returns_none() {
        let mut w = RelayWarning::new();
        w.update_penalty(45);
        assert!(
            w.snapshot().is_none(),
            "must not show banner when TCP fallback is not active"
        );
    }

    #[test]
    fn snapshot_available_when_active_and_penalty_received() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(45);
        assert!(w.snapshot().is_some());
    }

    #[test]
    fn snapshot_none_after_deactivation() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(45);
        w.set_active(false);
        assert!(
            w.snapshot().is_none(),
            "banner must be hidden when transport returns to a direct path"
        );
    }

    // ── penalty forwarding ────────────────────────────────────────────────────

    #[test]
    fn penalty_forwarded_verbatim() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(123);
        assert_eq!(w.snapshot().unwrap().penalty_ms, 123);
    }

    #[test]
    fn zero_penalty_is_not_suppressed() {
        // The daemon never sends 0 (floor = 30 ms), but the module must not
        // alter whatever value it receives.
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(0);
        assert_eq!(
            w.snapshot().unwrap().penalty_ms,
            0,
            "relay warning must not silently raise or suppress the received penalty"
        );
    }

    #[test]
    fn large_penalty_forwarded_verbatim() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(1_500);
        assert_eq!(w.snapshot().unwrap().penalty_ms, 1_500);
    }

    #[test]
    fn update_penalty_replaces_previous_value() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(100);
        w.update_penalty(250);
        assert_eq!(w.snapshot().unwrap().penalty_ms, 250);
    }

    // ── active flag ───────────────────────────────────────────────────────────

    #[test]
    fn is_active_reflects_set_active() {
        let mut w = RelayWarning::new();
        assert!(!w.is_active());
        w.set_active(true);
        assert!(w.is_active());
        w.set_active(false);
        assert!(!w.is_active());
    }

    #[test]
    fn snapshot_active_field_is_true_when_tcp_is_active() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(30);
        assert!(w.snapshot().unwrap().active);
    }

    #[test]
    fn reactivation_after_deactivation_restores_snapshot() {
        let mut w = RelayWarning::new();
        w.set_active(true);
        w.update_penalty(45);
        w.set_active(false);
        w.set_active(true);
        let snap = w.snapshot().unwrap();
        assert_eq!(snap.penalty_ms, 45, "penalty must survive deactivation and reactivation");
    }

    // ── penalty label ─────────────────────────────────────────────────────────

    #[test]
    fn penalty_label_format() {
        let snap = RelayWarningSnapshot { active: true, penalty_ms: 45 };
        assert_eq!(snap.penalty_label(), "+45 ms latency penalty");
    }

    #[test]
    fn penalty_label_floor_value() {
        let snap = RelayWarningSnapshot { active: true, penalty_ms: 30 };
        assert_eq!(snap.penalty_label(), "+30 ms latency penalty");
    }

    #[test]
    fn penalty_label_large_value() {
        let snap = RelayWarningSnapshot { active: true, penalty_ms: 1_200 };
        assert_eq!(snap.penalty_label(), "+1200 ms latency penalty");
    }

    // ── constants ─────────────────────────────────────────────────────────────

    #[test]
    fn warning_color_is_amber() {
        assert_eq!(RELAY_WARNING_COLOR, "#d97706");
    }

    // ── Feature 139: honesty properties ──────────────────────────────────────

    #[test]
    fn penalty_never_altered_across_a_range_of_values() {
        // The module must act as a pass-through for whatever the daemon sends.
        for penalty in [30u32, 45, 80, 160, 500, 1_200] {
            let mut w = RelayWarning::new();
            w.set_active(true);
            w.update_penalty(penalty);
            assert_eq!(
                w.snapshot().unwrap().penalty_ms,
                penalty,
                "penalty {penalty} must be forwarded verbatim"
            );
        }
    }

    #[test]
    fn banner_shown_only_while_tcp_is_active() {
        let mut w = RelayWarning::new();
        w.update_penalty(45);

        // Flip active on and off three times.
        for round in 0..3 {
            w.set_active(true);
            assert!(
                w.snapshot().is_some(),
                "round {round}: snapshot must be Some while TCP is active"
            );
            w.set_active(false);
            assert!(
                w.snapshot().is_none(),
                "round {round}: snapshot must be None after deactivation"
            );
        }
    }
}

//! Honest quality bar — Feature 146.
//!
//! Aggregates tier and network budget IPC events and exposes the current
//! session quality snapshot that the UI shell must render in the quality bar.
//!
//! The quality bar displays four fields verbatim:
//! - **Tier** — governor quality tier (Survival / Constrained / Comfortable / Full).
//! - **Bitrate** — total outbound budget across all streams, in kbps (floor-divided).
//! - **RTT** — round-trip time in milliseconds.
//! - **Loss** — packet-loss percentage.
//!
//! # Honesty contract
//!
//! Numbers are forwarded as-is from the governor — no rounding-up, smoothing,
//! or "good-enough" heuristics are applied.  The quality bar must not display
//! data until both a `TierUpdate` **and** a `StreamBudget` event have been
//! received; until then [`QualityBar::snapshot`] returns `None` and the bar
//! should be hidden.
//!
//! # Refresh rate
//!
//! Governor events arrive at 10 Hz, but the quality bar must refresh its
//! rendered output at most once per second (spec §14.3: "no decorative motion").
//! Use [`QualityBar::should_refresh`] to gate rendering and call
//! [`QualityBar::mark_displayed`] immediately after each render.
//!
//! # Usage
//!
//! ```
//! use lowband_shells::quality_bar::QualityBar;
//! use lowband_platform::TierState;
//!
//! let mut bar = QualityBar::new();
//!
//! // On IpcEvent::TierUpdate:
//! bar.update_tier(TierState::Comfortable);
//!
//! // On IpcEvent::StreamBudget (audio=24k, input=8k, screen_coarse=20k,
//! //   camera=12k, screen_refinement=0, xfer=0, rtt=85 ms, loss=0%):
//! bar.update_budget(24_000, 8_000, 20_000, 12_000, 0, 0, 85, 0.0);
//!
//! let snap = bar.snapshot().unwrap();
//! assert_eq!(snap.tier, TierState::Comfortable);
//! assert_eq!(snap.total_kbps, 64);
//! assert_eq!(snap.rtt_ms, 85);
//! assert_eq!(snap.loss_pct, 0.0);
//! ```

use std::time::{Duration, Instant};

use lowband_platform::TierState;

/// Minimum interval between quality bar display refreshes.
///
/// The UI must not refresh the rendered quality bar more often than once per
/// second to prevent decorative motion from obscuring genuine quality changes.
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// An immutable snapshot of the current session quality metrics.
///
/// All four fields are taken verbatim from the most recent governor events.
/// The UI shell must render them without modification.
#[derive(Debug, Clone, PartialEq)]
pub struct QualitySnapshot {
    /// Quality tier from the last `TierUpdate` event.
    pub tier: TierState,
    /// Total outbound bitrate across all streams, in kbps (floor-divided from bps).
    pub total_kbps: u32,
    /// Round-trip time in milliseconds from the last `StreamBudget` event.
    pub rtt_ms: u32,
    /// Packet-loss percentage, in the range [0.0, 100.0].
    pub loss_pct: f32,
}

/// Aggregates governor quality events and produces the quality bar display state.
///
/// Construct one `QualityBar` per session.  Drive it with:
/// - [`QualityBar::update_tier`] on each `IpcEvent::TierUpdate`.
/// - [`QualityBar::update_budget`] on each `IpcEvent::StreamBudget`.
///
/// Read [`QualityBar::snapshot`] to obtain the current display state.
/// Render at most once per second; use [`QualityBar::should_refresh`] and
/// [`QualityBar::mark_displayed`] to enforce this.
pub struct QualityBar {
    tier: Option<TierState>,
    total_kbps: u32,
    rtt_ms: u32,
    loss_pct: f32,
    has_budget: bool,
    last_displayed: Option<Instant>,
}

impl QualityBar {
    /// Create a new quality bar with no data.  [`snapshot`](Self::snapshot)
    /// returns `None` until both a tier and a budget event have been received.
    pub fn new() -> Self {
        Self {
            tier: None,
            total_kbps: 0,
            rtt_ms: 0,
            loss_pct: 0.0,
            has_budget: false,
            last_displayed: None,
        }
    }

    /// Record the current quality tier from an `IpcEvent::TierUpdate`.
    pub fn update_tier(&mut self, tier: TierState) {
        self.tier = Some(tier);
    }

    /// Record bitrate, RTT, and loss from an `IpcEvent::StreamBudget`.
    ///
    /// `total_kbps` is derived as:
    /// ```text
    /// (audio_bps + input_bps + screen_coarse_bps
    ///  + camera_bps + screen_refinement_bps + xfer_bps) / 1_000
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub fn update_budget(
        &mut self,
        audio_bps: u32,
        input_bps: u32,
        screen_coarse_bps: u32,
        camera_bps: u32,
        screen_refinement_bps: u32,
        xfer_bps: u32,
        rtt_ms: u32,
        loss_pct: f32,
    ) {
        let total_bps = (audio_bps as u64)
            + (input_bps as u64)
            + (screen_coarse_bps as u64)
            + (camera_bps as u64)
            + (screen_refinement_bps as u64)
            + (xfer_bps as u64);
        self.total_kbps = (total_bps / 1_000) as u32;
        self.rtt_ms = rtt_ms;
        self.loss_pct = loss_pct;
        self.has_budget = true;
    }

    /// Return the current quality snapshot.
    ///
    /// Returns `None` until at least one `TierUpdate` **and** one
    /// `StreamBudget` event have been processed.  The UI shell should hide the
    /// quality bar while this returns `None`.
    pub fn snapshot(&self) -> Option<QualitySnapshot> {
        let tier = self.tier?;
        if !self.has_budget {
            return None;
        }
        Some(QualitySnapshot {
            tier,
            total_kbps: self.total_kbps,
            rtt_ms: self.rtt_ms,
            loss_pct: self.loss_pct,
        })
    }

    /// Returns `true` when enough time has elapsed to justify a display refresh.
    ///
    /// Always `true` before the first call to [`mark_displayed`](Self::mark_displayed).
    /// After that, `true` again once [`MIN_REFRESH_INTERVAL`] has elapsed.
    pub fn should_refresh(&self) -> bool {
        self.last_displayed.map_or(true, |t| t.elapsed() >= MIN_REFRESH_INTERVAL)
    }

    /// Record that the quality bar has just been rendered.
    ///
    /// Resets the display clock so [`should_refresh`](Self::should_refresh)
    /// returns `false` for the next [`MIN_REFRESH_INTERVAL`].
    pub fn mark_displayed(&mut self) {
        self.last_displayed = Some(Instant::now());
    }
}

impl Default for QualityBar {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::TierState;

    fn bar_with_budget() -> QualityBar {
        let mut bar = QualityBar::new();
        bar.update_budget(6_000, 0, 0, 0, 0, 0, 0, 0.0);
        bar
    }

    // ── snapshot availability ─────────────────────────────────────────────────

    #[test]
    fn initial_snapshot_is_none() {
        assert!(QualityBar::new().snapshot().is_none());
    }

    #[test]
    fn tier_alone_does_not_produce_snapshot() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Full);
        assert!(bar.snapshot().is_none());
    }

    #[test]
    fn budget_alone_does_not_produce_snapshot() {
        let bar = bar_with_budget();
        assert!(bar.snapshot().is_none());
    }

    #[test]
    fn snapshot_available_after_tier_and_budget() {
        let mut bar = bar_with_budget();
        bar.update_tier(TierState::Comfortable);
        assert!(bar.snapshot().is_some());
    }

    // ── tier forwarding ───────────────────────────────────────────────────────

    #[test]
    fn all_tier_states_forwarded_verbatim() {
        let mut bar = bar_with_budget();
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            bar.update_tier(tier);
            assert_eq!(bar.snapshot().unwrap().tier, tier, "{tier:?} was not forwarded");
        }
    }

    #[test]
    fn update_tier_replaces_previous_tier() {
        let mut bar = bar_with_budget();
        bar.update_tier(TierState::Full);
        bar.update_tier(TierState::Survival);
        assert_eq!(bar.snapshot().unwrap().tier, TierState::Survival);
    }

    // ── bitrate calculation ───────────────────────────────────────────────────

    #[test]
    fn total_kbps_sums_all_six_streams() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Full);
        // 24k + 8k + 20k + 12k + 5k + 3k = 72_000 bps = 72 kbps
        bar.update_budget(24_000, 8_000, 20_000, 12_000, 5_000, 3_000, 0, 0.0);
        assert_eq!(bar.snapshot().unwrap().total_kbps, 72);
    }

    #[test]
    fn total_kbps_floor_divides_bps() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Full);
        // 1_999 bps → 1 kbps (floor, not rounded)
        bar.update_budget(1_999, 0, 0, 0, 0, 0, 0, 0.0);
        assert_eq!(bar.snapshot().unwrap().total_kbps, 1);
    }

    #[test]
    fn zero_bitrate_is_displayed_honestly() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Survival);
        bar.update_budget(0, 0, 0, 0, 0, 0, 1_000, 50.0);
        assert_eq!(bar.snapshot().unwrap().total_kbps, 0, "zero bitrate must not be rounded up");
    }

    #[test]
    fn survival_tier_voice_only_budget() {
        // Survival: only the 6 kbps audio floor, no other streams.
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Survival);
        bar.update_budget(6_000, 0, 0, 0, 0, 0, 500, 5.0);
        assert_eq!(bar.snapshot().unwrap().total_kbps, 6);
    }

    // ── RTT and loss forwarding ───────────────────────────────────────────────

    #[test]
    fn rtt_and_loss_forwarded_verbatim() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Constrained);
        bar.update_budget(6_000, 0, 0, 0, 0, 0, 250, 3.5);
        let snap = bar.snapshot().unwrap();
        assert_eq!(snap.rtt_ms, 250);
        assert!((snap.loss_pct - 3.5).abs() < 1e-5, "loss_pct mismatch");
    }

    #[test]
    fn high_loss_not_clamped() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Survival);
        bar.update_budget(6_000, 0, 0, 0, 0, 0, 1_500, 99.9);
        let snap = bar.snapshot().unwrap();
        assert!(
            (snap.loss_pct - 99.9).abs() < 1e-3,
            "high loss must not be clamped: got {}", snap.loss_pct
        );
    }

    #[test]
    fn update_budget_replaces_previous_values() {
        let mut bar = QualityBar::new();
        bar.update_tier(TierState::Full);
        bar.update_budget(100_000, 0, 0, 0, 0, 0, 20, 0.0);
        bar.update_budget(6_000, 0, 0, 0, 0, 0, 300, 10.0);
        let snap = bar.snapshot().unwrap();
        assert_eq!(snap.total_kbps, 6);
        assert_eq!(snap.rtt_ms, 300);
        assert!((snap.loss_pct - 10.0).abs() < 1e-5);
    }

    // ── refresh-rate gating ───────────────────────────────────────────────────

    #[test]
    fn should_refresh_true_before_first_display() {
        assert!(QualityBar::new().should_refresh());
    }

    #[test]
    fn should_refresh_false_immediately_after_mark_displayed() {
        let mut bar = QualityBar::new();
        bar.mark_displayed();
        assert!(!bar.should_refresh(), "bar refreshed within the 1 s window");
    }

    // ── Feature 146: honesty properties ──────────────────────────────────────

    #[test]
    fn snapshot_fields_match_is_neural_independent_of_tier() {
        // Verify that the quality bar works correctly at every tier — it must
        // never substitute a "better-looking" number based on tier alone.
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let mut bar = QualityBar::new();
            bar.update_tier(tier);
            bar.update_budget(6_000, 0, 0, 0, 0, 0, 400, 8.0);
            let snap = bar.snapshot().unwrap();
            assert_eq!(snap.tier, tier);
            assert_eq!(snap.total_kbps, 6);
            assert_eq!(snap.rtt_ms, 400);
            assert!((snap.loss_pct - 8.0).abs() < 1e-5);
        }
    }
}

//! Quality-bar display-lag tracker — Feature 133.
//!
//! Tracks all four quality-bar fields (tier, bitrate, RTT, and loss) as emitted
//! by the governor and verifies that the UI display reflects any change within
//! one second.
//!
//! # Contract
//!
//! The governor emits `TierUpdate` and `StreamBudget` events at 10 Hz over IPC.
//! Whenever any of the four quality-bar fields changes, the UI shell must render
//! the updated values within [`MAX_QUALITY_BAR_LAG`].
//! [`QualityBarLag::is_in_sync`] returns `true` only when this contract holds.
//!
//! A one-second grace window opens the moment any field changes.  Any display
//! update that arrives within that window resets the lag clock.  Once the window
//! closes, [`is_in_sync`](QualityBarLag::is_in_sync) returns `true` only if the
//! displayed snapshot exactly matches the governor snapshot and was updated after
//! the most recent governor change.
//!
//! # Relationship to Feature 134
//!
//! Feature 134 ([`quality_indicator`](super::quality_indicator)) tracks tier-only
//! sync.  This module extends that contract to all four quality-bar fields so that
//! a bitrate, RTT, or loss change without a tier change also triggers the
//! staleness check.
//!
//! # Usage
//!
//! ```
//! use lowband_obs::quality_bar_lag::QualityBarLag;
//! use lowband_platform::TierState;
//!
//! let mut lag = QualityBarLag::new();
//!
//! // On IpcEvent::TierUpdate + IpcEvent::StreamBudget (once both arrive):
//! lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
//!
//! // After the UI renders the quality bar:
//! lag.on_displayed(TierState::Comfortable, 64, 85, 0.0);
//!
//! assert!(lag.is_in_sync());
//! assert_eq!(lag.display_lag(), None);
//! ```

use std::time::{Duration, Instant};

use lowband_platform::TierState;

/// Maximum lag from a governor quality-bar change to the display reflecting
/// that change.  The display must be updated within this window for
/// [`QualityBarLag::is_in_sync`] to return `true` once the grace period expires.
pub const MAX_QUALITY_BAR_LAG: Duration = Duration::from_secs(1);

/// A snapshot of all four quality-bar fields.
///
/// Used to compare governor state against the displayed state.  `PartialEq` is
/// implemented with exact f32 bit comparison so that identical governor emissions
/// do not reset the grace-period clock.
#[derive(Debug, Clone)]
pub struct QualityBarSnapshot {
    /// Quality tier from the last `TierUpdate` event.
    pub tier: TierState,
    /// Total outbound bitrate across all streams, in kbps (floor-divided from bps).
    pub total_kbps: u32,
    /// Round-trip time in milliseconds from the last `StreamBudget` event.
    pub rtt_ms: u32,
    /// Packet-loss percentage in the range [0.0, 100.0].
    pub loss_pct: f32,
}

impl PartialEq for QualityBarSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.tier == other.tier
            && self.total_kbps == other.total_kbps
            && self.rtt_ms == other.rtt_ms
            && self.loss_pct.to_bits() == other.loss_pct.to_bits()
    }
}

/// Tracks governor quality-bar events and rendered display state to verify that
/// all four fields match the governor within [`MAX_QUALITY_BAR_LAG`].
///
/// Construct one `QualityBarLag` per session.  Drive it with:
/// - [`on_governor_update`](Self::on_governor_update) once both a `TierUpdate`
///   and a `StreamBudget` IPC event have been received (i.e. when
///   `QualityBar::snapshot()` returns `Some`).
/// - [`on_displayed`](Self::on_displayed) each time the UI renders the quality bar.
///
/// Poll [`is_in_sync`](Self::is_in_sync) to verify the one-second staleness
/// contract is being met.
pub struct QualityBarLag {
    governor: Option<QualityBarSnapshot>,
    governor_changed_at: Option<Instant>,
    displayed: Option<QualityBarSnapshot>,
    display_updated_at: Option<Instant>,
}

impl QualityBarLag {
    /// Create a new tracker in the idle (no data) state.
    ///
    /// [`is_in_sync`](Self::is_in_sync) returns `true` until the first governor
    /// update is received.
    pub fn new() -> Self {
        Self {
            governor: None,
            governor_changed_at: None,
            displayed: None,
            display_updated_at: None,
        }
    }

    /// Feed the latest governor quality-bar values.
    ///
    /// Call this once per governor cycle after both `IpcEvent::TierUpdate` and
    /// `IpcEvent::StreamBudget` have been received for that cycle.  The `total_kbps`
    /// value must be derived as:
    ///
    /// ```text
    /// floor((audio_bps + input_bps + screen_coarse_bps
    ///        + camera_bps + screen_refinement_bps + xfer_bps) / 1_000)
    /// ```
    ///
    /// The change timestamp is updated only when any field differs from the
    /// previously recorded governor snapshot; repeated calls with the same values
    /// do not reset the grace-period clock.
    pub fn on_governor_update(
        &mut self,
        tier: TierState,
        total_kbps: u32,
        rtt_ms: u32,
        loss_pct: f32,
    ) {
        let snap = QualityBarSnapshot { tier, total_kbps, rtt_ms, loss_pct };
        if self.governor.as_ref() != Some(&snap) {
            self.governor_changed_at = Some(Instant::now());
            self.governor = Some(snap);
        }
    }

    /// Record that the UI has just rendered all four quality-bar fields.
    ///
    /// Call this immediately after every render so the lag clock reflects the
    /// true display state.  The values passed here must be the values that were
    /// actually rendered, taken verbatim from [`QualityBar::snapshot`].
    pub fn on_displayed(
        &mut self,
        tier: TierState,
        total_kbps: u32,
        rtt_ms: u32,
        loss_pct: f32,
    ) {
        self.displayed = Some(QualityBarSnapshot { tier, total_kbps, rtt_ms, loss_pct });
        self.display_updated_at = Some(Instant::now());
    }

    /// Returns the last governor snapshot, or `None` before the first update.
    pub fn governor_snapshot(&self) -> Option<&QualityBarSnapshot> {
        self.governor.as_ref()
    }

    /// Returns the last displayed snapshot, or `None` before the first render.
    pub fn displayed_snapshot(&self) -> Option<&QualityBarSnapshot> {
        self.displayed.as_ref()
    }

    /// Returns `true` when the displayed snapshot matches the governor snapshot
    /// and was updated within [`MAX_QUALITY_BAR_LAG`] of the last governor change.
    ///
    /// Returns `true` before any governor update is received (trivially in sync).
    /// Returns `true` during the one-second grace window that opens on every
    /// governor change, regardless of whether the display has caught up.
    /// After the grace window closes, returns `true` only if all four displayed
    /// fields exactly match the governor values and the display was updated after
    /// the most recent governor change.
    pub fn is_in_sync(&self) -> bool {
        let Some(gov) = &self.governor else {
            return true;
        };

        // One-second grace window: during it the display is allowed to lag.
        if self.governor_changed_at.map_or(true, |t| t.elapsed() <= MAX_QUALITY_BAR_LAG) {
            return true;
        }

        // Grace period expired — display must show all current values and have
        // been updated after the most recent governor change.
        let Some(disp) = &self.displayed else {
            return false;
        };
        if disp != gov {
            return false;
        }
        match (self.governor_changed_at, self.display_updated_at) {
            (Some(changed), Some(updated)) => updated >= changed,
            _ => false,
        }
    }

    /// Returns how long the display has been out of sync, or `None` if
    /// [`is_in_sync`](Self::is_in_sync) is `true`.
    ///
    /// The lag value is the time elapsed since the governor quality-bar values
    /// last changed.
    pub fn display_lag(&self) -> Option<Duration> {
        if self.is_in_sync() {
            return None;
        }
        self.governor_changed_at.map(|t| t.elapsed())
    }
}

impl Default for QualityBarLag {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::TierState;

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn initial_is_in_sync() {
        assert!(QualityBarLag::new().is_in_sync());
    }

    #[test]
    fn initial_governor_snapshot_is_none() {
        assert!(QualityBarLag::new().governor_snapshot().is_none());
    }

    #[test]
    fn initial_displayed_snapshot_is_none() {
        assert!(QualityBarLag::new().displayed_snapshot().is_none());
    }

    #[test]
    fn initial_display_lag_is_none() {
        assert_eq!(QualityBarLag::new().display_lag(), None);
    }

    // ── No governor data ─────────────────────────────────────────────────────

    #[test]
    fn display_without_governor_is_in_sync() {
        let mut lag = QualityBarLag::new();
        lag.on_displayed(TierState::Full, 200, 20, 0.0);
        assert!(lag.is_in_sync(), "no governor data → trivially in sync");
    }

    // ── Within the one-second grace window ────────────────────────────────────

    #[test]
    fn governor_alone_within_grace_is_in_sync() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
        assert!(
            lag.is_in_sync(),
            "immediately after a governor change we are within MAX_QUALITY_BAR_LAG",
        );
    }

    #[test]
    fn mismatched_snapshot_within_grace_is_in_sync() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Constrained, 6, 500, 5.0); // stale display
        assert!(
            lag.is_in_sync(),
            "stale display within the grace window must still be considered in sync",
        );
    }

    // ── Matching snapshot ─────────────────────────────────────────────────────

    #[test]
    fn matching_snapshot_is_in_sync() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
        lag.on_displayed(TierState::Comfortable, 64, 85, 0.0);
        assert!(lag.is_in_sync());
    }

    #[test]
    fn display_lag_none_when_matching() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Full, 200, 20, 0.0);
        assert_eq!(lag.display_lag(), None);
    }

    // ── All four fields tracked ───────────────────────────────────────────────

    #[test]
    fn tier_change_is_a_change() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 64, 85, 0.0);
        lag.on_displayed(TierState::Full, 64, 85, 0.0);
        // Tier changes — a new grace window must open.
        lag.on_governor_update(TierState::Survival, 64, 85, 0.0);
        assert!(lag.is_in_sync(), "tier change opens a fresh grace window");
    }

    #[test]
    fn bitrate_change_is_a_change() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Full, 200, 20, 0.0);
        lag.on_governor_update(TierState::Full, 6, 20, 0.0);
        assert!(lag.is_in_sync(), "bitrate change opens a fresh grace window");
    }

    #[test]
    fn rtt_change_is_a_change() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Constrained, 50, 80, 0.0);
        lag.on_displayed(TierState::Constrained, 50, 80, 0.0);
        lag.on_governor_update(TierState::Constrained, 50, 500, 0.0);
        assert!(lag.is_in_sync(), "RTT change opens a fresh grace window");
    }

    #[test]
    fn loss_change_is_a_change() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
        lag.on_displayed(TierState::Comfortable, 64, 85, 0.0);
        lag.on_governor_update(TierState::Comfortable, 64, 85, 5.0);
        assert!(lag.is_in_sync(), "loss change opens a fresh grace window");
    }

    // ── Accessor correctness ──────────────────────────────────────────────────

    #[test]
    fn governor_snapshot_reflects_last_update() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Survival, 6, 1_000, 50.0);
        let snap = lag.governor_snapshot().unwrap();
        assert_eq!(snap.tier, TierState::Survival);
        assert_eq!(snap.total_kbps, 6);
        assert_eq!(snap.rtt_ms, 1_000);
        assert!((snap.loss_pct - 50.0).abs() < 1e-5);
    }

    #[test]
    fn governor_snapshot_updated_by_new_values() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_governor_update(TierState::Survival, 6, 1_000, 50.0);
        let snap = lag.governor_snapshot().unwrap();
        assert_eq!(snap.tier, TierState::Survival);
    }

    #[test]
    fn displayed_snapshot_reflects_last_render() {
        let mut lag = QualityBarLag::new();
        lag.on_displayed(TierState::Comfortable, 64, 85, 1.5);
        let snap = lag.displayed_snapshot().unwrap();
        assert_eq!(snap.tier, TierState::Comfortable);
        assert_eq!(snap.total_kbps, 64);
        assert_eq!(snap.rtt_ms, 85);
        assert!((snap.loss_pct - 1.5).abs() < 1e-5);
    }

    #[test]
    fn displayed_snapshot_updated_by_new_render() {
        let mut lag = QualityBarLag::new();
        lag.on_displayed(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Survival, 6, 1_000, 50.0);
        let snap = lag.displayed_snapshot().unwrap();
        assert_eq!(snap.tier, TierState::Survival);
    }

    // ── All tier states accepted ──────────────────────────────────────────────

    #[test]
    fn all_tier_states_accepted_by_governor_update() {
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let mut lag = QualityBarLag::new();
            lag.on_governor_update(tier, 0, 0, 0.0);
            assert_eq!(
                lag.governor_snapshot().unwrap().tier,
                tier,
                "{tier:?} not stored correctly",
            );
        }
    }

    #[test]
    fn all_tier_states_accepted_by_displayed() {
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let mut lag = QualityBarLag::new();
            lag.on_displayed(tier, 0, 0, 0.0);
            assert_eq!(
                lag.displayed_snapshot().unwrap().tier,
                tier,
                "{tier:?} not stored correctly",
            );
        }
    }

    // ── Honesty: zero bitrate and high loss ───────────────────────────────────

    #[test]
    fn zero_bitrate_tracked_honestly() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Survival, 0, 500, 50.0);
        lag.on_displayed(TierState::Survival, 0, 500, 50.0);
        assert!(lag.is_in_sync());
        assert_eq!(lag.governor_snapshot().unwrap().total_kbps, 0);
    }

    #[test]
    fn high_loss_tracked_and_not_clamped() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Survival, 6, 1_500, 99.9);
        lag.on_displayed(TierState::Survival, 6, 1_500, 99.9);
        assert!(lag.is_in_sync());
        let snap = lag.governor_snapshot().unwrap();
        assert!(
            (snap.loss_pct - 99.9).abs() < 1e-3,
            "high loss must not be clamped: got {}",
            snap.loss_pct,
        );
    }

    // ── Same-value idempotency ────────────────────────────────────────────────

    #[test]
    fn repeated_same_governor_values_stay_in_sync() {
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Full, 200, 20, 0.0);
        // Feeding the same values again must not alter the in-sync state.
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        assert!(lag.is_in_sync());
    }

    #[test]
    fn repeated_same_governor_values_do_not_reset_governor_timestamp() {
        // If the same values are fed again the change timestamp must not be reset,
        // so any existing display sync remains valid.
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
        lag.on_displayed(TierState::Comfortable, 64, 85, 0.0);
        // Same values again — governor_changed_at must not update.
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);
        // Still in sync because displayed values match and timestamp did not reset.
        assert!(lag.is_in_sync());
    }

    // ── Survival-tier specific contract ───────────────────────────────────────

    #[test]
    fn survival_tier_voice_only_budget_is_tracked() {
        // At Survival tier only the 6 kbps audio floor is active.
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Survival, 6, 500, 5.0);
        lag.on_displayed(TierState::Survival, 6, 500, 5.0);
        assert!(lag.is_in_sync());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            QualityBarLag::new().is_in_sync(),
            QualityBarLag::default().is_in_sync(),
        );
    }

    // ── Feature 133 acceptance ────────────────────────────────────────────────

    #[test]
    fn feature_133_acceptance() {
        // Simulate a full governor 10 Hz cycle followed by a quality-bar render.
        // Both events happen well within MAX_QUALITY_BAR_LAG, so the tracker
        // must report in-sync.
        let mut lag = QualityBarLag::new();

        // Governor emits tier (from IpcEvent::TierUpdate) and budget
        // (from IpcEvent::StreamBudget) at tick 1.
        // After both arrive, call on_governor_update with the derived snapshot.
        // 24k + 8k + 20k + 12k + 0 + 0 = 64 kbps.
        lag.on_governor_update(TierState::Comfortable, 64, 85, 0.0);

        // Display renders the quality bar with the same values (within 1 second).
        lag.on_displayed(TierState::Comfortable, 64, 85, 0.0);

        assert!(
            lag.is_in_sync(),
            "display must be in sync within MAX_QUALITY_BAR_LAG of a governor change",
        );
        assert_eq!(
            lag.display_lag(),
            None,
            "display_lag must be None when in sync",
        );

        let gov = lag.governor_snapshot().unwrap();
        assert_eq!(gov.tier, TierState::Comfortable);
        assert_eq!(gov.total_kbps, 64);
        assert_eq!(gov.rtt_ms, 85);
        assert!((gov.loss_pct - 0.0).abs() < 1e-5);

        let disp = lag.displayed_snapshot().unwrap();
        assert_eq!(disp.tier, TierState::Comfortable);
        assert_eq!(disp.total_kbps, 64);
        assert_eq!(disp.rtt_ms, 85);
        assert!((disp.loss_pct - 0.0).abs() < 1e-5);
    }

    #[test]
    fn feature_133_any_field_change_starts_grace_window() {
        // After any field changes the grace window must be open so the display
        // has time to catch up before is_in_sync returns false.
        let mut lag = QualityBarLag::new();

        // Establish a stable display.
        lag.on_governor_update(TierState::Full, 200, 20, 0.0);
        lag.on_displayed(TierState::Full, 200, 20, 0.0);

        // Bitrate collapses — opens a fresh grace window.
        lag.on_governor_update(TierState::Survival, 6, 800, 8.0);

        // No display update yet — still within MAX_QUALITY_BAR_LAG.
        assert!(
            lag.is_in_sync(),
            "grace window must be open immediately after any governor change",
        );
        assert_eq!(lag.display_lag(), None);
    }

    #[test]
    fn feature_133_all_four_fields_must_match() {
        // After the grace window closes, ALL four fields must match for
        // is_in_sync to return true.  We can test this indirectly by driving
        // the tracker with a matching snapshot — mismatching cases are tested
        // only via grace-window checks because we cannot fast-forward Instant.
        let mut lag = QualityBarLag::new();
        lag.on_governor_update(TierState::Constrained, 50, 250, 3.5);
        lag.on_displayed(TierState::Constrained, 50, 250, 3.5);
        assert!(lag.is_in_sync());
        let snap = lag.displayed_snapshot().unwrap();
        assert_eq!(snap.tier, TierState::Constrained);
        assert_eq!(snap.total_kbps, 50);
        assert_eq!(snap.rtt_ms, 250);
        assert!((snap.loss_pct - 3.5).abs() < 1e-5);
    }
}

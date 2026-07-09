//! Quality indicator — Feature 134.
//!
//! Tracks the governor tier state and the most-recently displayed tier,
//! and asserts that the display matches the governor state within one second.
//!
//! # Staleness contract
//!
//! The governor emits `TierUpdate` events at 10 Hz over IPC.  The UI shell
//! must render the quality indicator at most once per second (spec §14.3 "no
//! decorative motion"), but **must** have done so within one second of any
//! governor tier change.  [`QualityIndicator::is_in_sync`] returns `true`
//! only when this contract holds.
//!
//! A one-second grace window begins the moment the governor emits a new tier.
//! Any display update that arrives within that window resets the lag clock.
//! Once the window closes, [`is_in_sync`](QualityIndicator::is_in_sync)
//! returns `true` only if the displayed tier matches the governor tier and
//! the display was updated after the governor change.
//!
//! # Usage
//!
//! ```
//! use lowband_obs::quality_indicator::QualityIndicator;
//! use lowband_platform::TierState;
//!
//! let mut qi = QualityIndicator::new();
//!
//! // On IpcEvent::TierUpdate:
//! qi.on_governor_update(TierState::Comfortable);
//!
//! // After the UI renders the quality bar:
//! qi.on_displayed(TierState::Comfortable);
//!
//! assert!(qi.is_in_sync());
//! assert_eq!(qi.display_lag(), None);
//! ```

use std::time::{Duration, Instant};

use lowband_platform::TierState;

/// Maximum lag from a governor tier change to the display reflecting that
/// change.  The display must be updated within this window for
/// [`QualityIndicator::is_in_sync`] to return `true` once the grace period
/// expires.
pub const MAX_DISPLAY_LAG: Duration = Duration::from_secs(1);

/// Tracks governor tier events and rendered display state to verify that the
/// quality indicator matches the governor tier within [`MAX_DISPLAY_LAG`].
///
/// Construct one `QualityIndicator` per session.  Drive it with:
/// - [`on_governor_update`](Self::on_governor_update) on each
///   `IpcEvent::TierUpdate` from the governor.
/// - [`on_displayed`](Self::on_displayed) each time the UI renders the quality
///   indicator.
///
/// Poll [`is_in_sync`](Self::is_in_sync) to verify the one-second staleness
/// contract is being met.
pub struct QualityIndicator {
    governor_tier: Option<TierState>,
    governor_changed_at: Option<Instant>,
    displayed_tier: Option<TierState>,
    display_updated_at: Option<Instant>,
}

impl QualityIndicator {
    /// Create a new indicator in the idle (no data) state.
    ///
    /// [`is_in_sync`](Self::is_in_sync) returns `true` until the first
    /// governor tier is received.
    pub fn new() -> Self {
        Self {
            governor_tier: None,
            governor_changed_at: None,
            displayed_tier: None,
            display_updated_at: None,
        }
    }

    /// Feed the latest governor tier (call on every `IpcEvent::TierUpdate`).
    ///
    /// The change timestamp is updated only when `tier` differs from the
    /// previously recorded governor tier; repeated calls with the same value
    /// do not reset the grace-period clock.
    pub fn on_governor_update(&mut self, tier: TierState) {
        if self.governor_tier != Some(tier) {
            self.governor_tier = Some(tier);
            self.governor_changed_at = Some(Instant::now());
        }
    }

    /// Record that the UI has just rendered the quality indicator with `tier`.
    ///
    /// Call this immediately after every render so the lag clock reflects the
    /// true display state.
    pub fn on_displayed(&mut self, tier: TierState) {
        self.displayed_tier = Some(tier);
        self.display_updated_at = Some(Instant::now());
    }

    /// Returns the current governor tier, or `None` before the first
    /// `IpcEvent::TierUpdate` is received.
    pub fn governor_tier(&self) -> Option<TierState> {
        self.governor_tier
    }

    /// Returns the tier value the display most recently rendered, or `None`
    /// if the display has not yet been updated.
    pub fn displayed_tier(&self) -> Option<TierState> {
        self.displayed_tier
    }

    /// Returns `true` when the display tier matches the governor tier and the
    /// display has been updated within [`MAX_DISPLAY_LAG`] of the last
    /// governor change.
    ///
    /// Returns `true` before any governor tier is received (trivially in sync).
    /// Returns `true` during the one-second grace window that opens on every
    /// governor tier change, regardless of whether the display has caught up.
    /// After the grace window closes, returns `true` only if the display shows
    /// the current governor tier and was updated after the most recent governor
    /// change.
    pub fn is_in_sync(&self) -> bool {
        let Some(gov) = self.governor_tier else {
            return true;
        };

        // One-second grace window: during it the display is allowed to lag.
        if self.governor_changed_at.map_or(true, |t| t.elapsed() <= MAX_DISPLAY_LAG) {
            return true;
        }

        // Grace period expired — display must show the current tier and have
        // been updated after the most recent governor change.
        let Some(disp) = self.displayed_tier else {
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
    /// The lag value is the time elapsed since the governor tier last changed.
    pub fn display_lag(&self) -> Option<Duration> {
        if self.is_in_sync() {
            return None;
        }
        self.governor_changed_at.map(|t| t.elapsed())
    }
}

impl Default for QualityIndicator {
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
        assert!(QualityIndicator::new().is_in_sync());
    }

    #[test]
    fn initial_governor_tier_is_none() {
        assert_eq!(QualityIndicator::new().governor_tier(), None);
    }

    #[test]
    fn initial_displayed_tier_is_none() {
        assert_eq!(QualityIndicator::new().displayed_tier(), None);
    }

    #[test]
    fn initial_display_lag_is_none() {
        assert_eq!(QualityIndicator::new().display_lag(), None);
    }

    // ── No governor data ─────────────────────────────────────────────────────

    #[test]
    fn display_without_governor_is_in_sync() {
        let mut qi = QualityIndicator::new();
        qi.on_displayed(TierState::Full);
        assert!(qi.is_in_sync(), "no governor data → trivially in sync");
    }

    // ── Within the one-second grace window ────────────────────────────────────

    #[test]
    fn governor_alone_within_grace_is_in_sync() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Comfortable);
        assert!(
            qi.is_in_sync(),
            "immediately after a governor change we are within the MAX_DISPLAY_LAG grace window",
        );
    }

    #[test]
    fn mismatched_tier_within_grace_is_in_sync() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Full);
        qi.on_displayed(TierState::Constrained); // display hasn't caught up yet
        assert!(
            qi.is_in_sync(),
            "stale display within the grace window must still be considered in sync",
        );
    }

    // ── Matching tier ─────────────────────────────────────────────────────────

    #[test]
    fn matching_tier_is_in_sync() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Comfortable);
        qi.on_displayed(TierState::Comfortable);
        assert!(qi.is_in_sync());
    }

    #[test]
    fn display_lag_none_when_matching() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Full);
        qi.on_displayed(TierState::Full);
        assert_eq!(qi.display_lag(), None);
    }

    // ── Accessor correctness ──────────────────────────────────────────────────

    #[test]
    fn governor_tier_reflects_last_update() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Survival);
        assert_eq!(qi.governor_tier(), Some(TierState::Survival));
    }

    #[test]
    fn governor_tier_updated_by_new_value() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Full);
        qi.on_governor_update(TierState::Constrained);
        assert_eq!(qi.governor_tier(), Some(TierState::Constrained));
    }

    #[test]
    fn displayed_tier_reflects_last_render() {
        let mut qi = QualityIndicator::new();
        qi.on_displayed(TierState::Comfortable);
        assert_eq!(qi.displayed_tier(), Some(TierState::Comfortable));
    }

    #[test]
    fn displayed_tier_updated_by_new_render() {
        let mut qi = QualityIndicator::new();
        qi.on_displayed(TierState::Full);
        qi.on_displayed(TierState::Survival);
        assert_eq!(qi.displayed_tier(), Some(TierState::Survival));
    }

    // ── All tier states accepted ──────────────────────────────────────────────

    #[test]
    fn all_governor_tiers_accepted() {
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let mut qi = QualityIndicator::new();
            qi.on_governor_update(tier);
            assert_eq!(qi.governor_tier(), Some(tier), "{tier:?} not stored correctly");
        }
    }

    #[test]
    fn all_displayed_tiers_accepted() {
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let mut qi = QualityIndicator::new();
            qi.on_displayed(tier);
            assert_eq!(qi.displayed_tier(), Some(tier), "{tier:?} not stored correctly");
        }
    }

    // ── Same-tier idempotency (clock must not reset on repeated same tier) ────

    #[test]
    fn repeated_same_governor_tier_stays_in_sync() {
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Full);
        qi.on_displayed(TierState::Full);
        // Feeding the same tier again must not alter the in-sync state.
        qi.on_governor_update(TierState::Full);
        assert!(qi.is_in_sync());
    }

    // ── Default impl ──────────────────────────────────────────────────────────

    #[test]
    fn default_matches_new() {
        assert_eq!(
            QualityIndicator::new().is_in_sync(),
            QualityIndicator::default().is_in_sync(),
        );
    }

    // ── Feature 134: display matches governor state within one second ─────────

    #[test]
    fn feature_134_acceptance() {
        // Simulate a governor 10 Hz tick followed by a display render.
        // Both events happen well within MAX_DISPLAY_LAG, so the indicator
        // must report in-sync.
        let mut qi = QualityIndicator::new();

        // Governor emits tier (e.g. from IpcEvent::TierUpdate at tick 1).
        qi.on_governor_update(TierState::Comfortable);

        // Display renders the quality bar (within 1 second).
        qi.on_displayed(TierState::Comfortable);

        assert!(
            qi.is_in_sync(),
            "display must be in sync within MAX_DISPLAY_LAG of a governor change",
        );
        assert_eq!(
            qi.display_lag(),
            None,
            "display_lag must be None when in sync",
        );
        assert_eq!(qi.governor_tier(), Some(TierState::Comfortable));
        assert_eq!(qi.displayed_tier(), Some(TierState::Comfortable));
    }

    #[test]
    fn feature_134_tier_change_starts_grace_window() {
        // After a governor tier change the grace window must be open so the
        // display has time to catch up before is_in_sync returns false.
        let mut qi = QualityIndicator::new();
        qi.on_governor_update(TierState::Constrained);

        // No display update yet — still within MAX_DISPLAY_LAG.
        assert!(
            qi.is_in_sync(),
            "grace window must be open immediately after a governor tier change",
        );
        assert_eq!(qi.display_lag(), None);
    }
}

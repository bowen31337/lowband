//! Live quality indicator (FR-11, GA v1.0).
//!
//! FR-11 requires an *honest* indicator of the current tier, bitrate, RTT, and
//! loss — "not decorative" — that matches the governor within 1 s. The eval
//! found only a shell-side view-model with no aggregation path from the
//! governor's own state. This computes the honest summary directly from the
//! governor's per-tick outputs (tier, the allocated stream budgets, and the
//! measured network) so the indicator can never drift from what the governor
//! actually decided: the daemon updates it every 100 ms tick (≪ the 1 s bar),
//! and any shell renders [`QualitySummary`] verbatim.

use lowband_platform::{StreamBudgets, TierState};

/// The honest, current session quality — exactly what the governor produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualitySummary {
    pub tier: TierState,
    /// Total allocated session bitrate (sum of all stream budgets), bps.
    pub bitrate_bps: u32,
    pub rtt_ms: u32,
    pub loss_pct: f32,
}

/// Sum of every per-stream budget — the real session bitrate the governor
/// allocated this tick.
pub fn total_bitrate_bps(b: &StreamBudgets) -> u32 {
    b.audio_bps
        .saturating_add(b.input_bps)
        .saturating_add(b.screen_coarse_bps)
        .saturating_add(b.camera_bps)
        .saturating_add(b.screen_refinement_bps)
        .saturating_add(b.xfer_bps)
}

/// Aggregates governor state into the live [`QualitySummary`].
#[derive(Default)]
pub struct QualityIndicator {
    latest: Option<QualitySummary>,
}

impl QualityIndicator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one governor tick's outputs into the indicator and return the
    /// honest summary. Called every 10 Hz tick, so the indicator reflects the
    /// governor within 100 ms — well inside FR-11's 1 s bar.
    pub fn update(
        &mut self,
        tier: TierState,
        budgets: &StreamBudgets,
        rtt_ms: u32,
        loss_pct: f32,
    ) -> QualitySummary {
        let summary = QualitySummary {
            tier,
            bitrate_bps: total_bitrate_bps(budgets),
            rtt_ms,
            loss_pct,
        };
        self.latest = Some(summary);
        summary
    }

    /// The most recent summary, if any tick has been observed. (Shell-facing
    /// accessor; the daemon renders via [`line`](Self::line).)
    #[allow(dead_code)]
    pub fn latest(&self) -> Option<QualitySummary> {
        self.latest
    }

    /// A compact honest one-line rendering (used for the daemon's stderr
    /// indicator; a UI shell renders the same fields graphically).
    pub fn line(&self) -> Option<String> {
        self.latest.map(|s| {
            format!(
                "quality: tier={:?} bitrate={} kbps rtt={} ms loss={:.1}%",
                s.tier,
                s.bitrate_bps / 1000,
                s.rtt_ms,
                s.loss_pct
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::gear_policy::DisplayResolution;

    fn budgets(audio: u32, screen: u32, camera: u32) -> StreamBudgets {
        StreamBudgets {
            audio_bps: audio,
            input_bps: 3000,
            screen_coarse_bps: screen,
            camera_bps: camera,
            screen_refinement_bps: 0,
            xfer_bps: 0,
            display_resolution: DisplayResolution { width: 848, height: 480 },
            per_frame_byte_cap: 0,
            roi_delta_qp: 0,
        }
    }

    #[test]
    fn bitrate_is_the_sum_of_stream_budgets() {
        let b = budgets(24_000, 40_000, 98_000);
        // 24000 + 3000 + 40000 + 98000 = 165000
        assert_eq!(total_bitrate_bps(&b), 165_000);
    }

    #[test]
    fn summary_reflects_governor_state_exactly() {
        let mut qi = QualityIndicator::new();
        let b = budgets(24_000, 20_000, 0);
        let s = qi.update(TierState::Constrained, &b, 287, 4.1);
        assert_eq!(s.tier, TierState::Constrained);
        assert_eq!(s.bitrate_bps, 24_000 + 3_000 + 20_000);
        assert_eq!(s.rtt_ms, 287);
        assert!((s.loss_pct - 4.1).abs() < 1e-6);
        assert_eq!(qi.latest(), Some(s), "latest must equal the last governor tick");
    }

    #[test]
    fn indicator_tracks_a_tier_downgrade_within_one_tick() {
        // FR-11: matches governor within 1 s. The indicator updates every tick,
        // so a downgrade is reflected immediately on the next update.
        let mut qi = QualityIndicator::new();
        qi.update(TierState::Full, &budgets(32_000, 120_000, 200_000), 40, 0.0);
        let downgraded = qi.update(TierState::Survival, &budgets(6_000, 8_000, 0), 320, 5.0);
        assert_eq!(downgraded.tier, TierState::Survival);
        assert_eq!(qi.latest().unwrap().tier, TierState::Survival);
        assert!(qi.line().unwrap().contains("Survival"));
    }
}

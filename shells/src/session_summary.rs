//! End-of-session summary displayed in the UI shell — Feature 151.
//!
//! The UI shell constructs a [`SessionTracker`] when a session begins.  As
//! governor events arrive over the IPC socket, the shell calls
//! [`SessionTracker::record_budget_tick`] for each [`StreamBudget`] event and
//! [`SessionTracker::mark_*_used`] when the corresponding capability grant is
//! first issued.  When the session ends, the shell calls
//! [`SessionTracker::finish`] to obtain a [`SessionSummary`] and displays it
//! via [`SessionSummary::format_summary`].
//!
//! # Data accounting
//!
//! `StreamBudget` events arrive at 10 Hz (one per 100 ms governor tick).
//! The byte estimate for each tick is:
//!
//! ```text
//! bytes_per_tick = (audio_bps + input_bps + screen_coarse_bps
//!                  + camera_bps + screen_refinement_bps + xfer_bps) / 80
//! ```
//!
//! (`/ 80` = `/ 8 bits` × `/ 10 ticks-per-second`.)
//!
//! # Example
//!
//! ```
//! use lowband_shells::session_summary::{SessionTracker, SessionSummary};
//! use std::time::Duration;
//!
//! let mut tracker = SessionTracker::new();
//!
//! // Capability grants issued during the call.
//! tracker.mark_view_used();
//! tracker.mark_control_used();
//!
//! // 1 800 StreamBudget ticks at 64 kbps (30-minute session).
//! for _ in 0..1_800 {
//!     tracker.record_budget_tick(24_000, 8_000, 20_000, 12_000, 0, 0);
//! }
//!
//! let summary = tracker.finish_with_duration(Duration::from_secs(1800));
//! let line = summary.format_summary();
//! assert!(line.contains("view, control"));
//! assert!(line.contains("MB"));
//! ```

use std::time::Duration;

/// Which capabilities were active at any point during the session.
///
/// Each field is `true` if the corresponding grant was issued at least once.
/// The UI shell sets these flags when the consent subsystem issues a grant,
/// not when the grant is revoked, so the summary reflects every capability
/// the assisted user consented to — even briefly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CapabilitiesUsed {
    /// Screen-view (remote can see the display).
    pub view: bool,
    /// Remote control (input injection into keyboard and mouse).
    pub control: bool,
    /// File transfer (files sent or received).
    pub file: bool,
    /// Clipboard sync (clipboard content shared).
    pub clipboard: bool,
}

impl CapabilitiesUsed {
    /// Return the display names of every capability that was active.
    ///
    /// The order is fixed: `["view", "control", "file", "clipboard"]`.
    /// Returns an empty `Vec` when no capabilities were used.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.view     { names.push("view"); }
        if self.control  { names.push("control"); }
        if self.file     { names.push("file"); }
        if self.clipboard { names.push("clipboard"); }
        names
    }
}

/// Immutable snapshot of a completed session, ready for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Wall-clock duration of the session.
    pub duration: Duration,
    /// Which capability grants were issued during the session.
    pub capabilities: CapabilitiesUsed,
    /// Estimated total bytes transferred across all streams, derived from
    /// `StreamBudget` IPC events (see module-level docs for the formula).
    pub total_bytes: u64,
}

impl SessionSummary {
    /// Produce a single summary line for display in the UI shell.
    ///
    /// Format: `"Duration: Xm Ys  |  Capabilities: a, b  |  Data: Z.Z MB"`
    ///
    /// If no capabilities were used the capabilities field reads `"none"`.
    /// Data is rendered in MB (metric, 1 MB = 1 000 000 bytes), one decimal place.
    pub fn format_summary(&self) -> String {
        let secs = self.duration.as_secs();
        let minutes = secs / 60;
        let remaining_secs = secs % 60;

        let names = self.capabilities.names();
        let caps_str = if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        };

        let mb = self.total_bytes as f64 / 1_000_000.0;

        format!(
            "Duration: {}m {}s  |  Capabilities: {}  |  Data: {:.1} MB",
            minutes, remaining_secs, caps_str, mb,
        )
    }
}

/// Accumulates session statistics from IPC events during a live session.
///
/// Construct with [`SessionTracker::new`] when the session starts.  Drive it
/// with `mark_*_used` and `record_budget_tick` as IPC events arrive.  Call
/// [`SessionTracker::finish`] when the session ends to get the displayable
/// [`SessionSummary`].
pub struct SessionTracker {
    start: std::time::Instant,
    capabilities: CapabilitiesUsed,
    total_bytes: u64,
}

impl SessionTracker {
    /// Start tracking a new session.
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            capabilities: CapabilitiesUsed::default(),
            total_bytes: 0,
        }
    }

    /// Mark the screen-view capability as having been active.
    pub fn mark_view_used(&mut self) {
        self.capabilities.view = true;
    }

    /// Mark the remote-control capability as having been active.
    pub fn mark_control_used(&mut self) {
        self.capabilities.control = true;
    }

    /// Mark the file-transfer capability as having been active.
    pub fn mark_file_used(&mut self) {
        self.capabilities.file = true;
    }

    /// Mark the clipboard-sync capability as having been active.
    pub fn mark_clipboard_used(&mut self) {
        self.capabilities.clipboard = true;
    }

    /// Accumulate the byte estimate for one `StreamBudget` governor tick.
    ///
    /// Each tick spans 100 ms (10 Hz governor rate).  The byte estimate is
    /// `(audio_bps + input_bps + screen_coarse_bps + camera_bps
    ///   + screen_refinement_bps + xfer_bps) / 80`.
    ///
    /// Call this once per `StreamBudget` IPC event received from the daemon.
    pub fn record_budget_tick(
        &mut self,
        audio_bps: u32,
        input_bps: u32,
        screen_coarse_bps: u32,
        camera_bps: u32,
        screen_refinement_bps: u32,
        xfer_bps: u32,
    ) {
        let total_bps = audio_bps as u64
            + input_bps as u64
            + screen_coarse_bps as u64
            + camera_bps as u64
            + screen_refinement_bps as u64
            + xfer_bps as u64;
        // One 10 Hz tick covers 100 ms → bytes = bps / 8 / 10 = bps / 80.
        self.total_bytes += total_bps / 80;
    }

    /// Finish the session and return the displayable summary.
    ///
    /// Computes duration from the wall clock.  In production code, call this
    /// exactly once, immediately after the session ends.
    pub fn finish(self) -> SessionSummary {
        SessionSummary {
            duration: self.start.elapsed(),
            capabilities: self.capabilities,
            total_bytes: self.total_bytes,
        }
    }

    /// Finish the session with an explicit `duration` instead of computing it
    /// from the wall clock.
    ///
    /// Intended for unit tests where a deterministic duration is required.
    pub fn finish_with_duration(self, duration: Duration) -> SessionSummary {
        SessionSummary {
            duration,
            capabilities: self.capabilities,
            total_bytes: self.total_bytes,
        }
    }
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── CapabilitiesUsed::names ───────────────────────────────────────────────

    #[test]
    fn no_capabilities_returns_empty_names() {
        let caps = CapabilitiesUsed::default();
        assert!(caps.names().is_empty());
    }

    #[test]
    fn names_order_is_view_control_file_clipboard() {
        let caps = CapabilitiesUsed { view: true, control: true, file: true, clipboard: true };
        assert_eq!(caps.names(), &["view", "control", "file", "clipboard"]);
    }

    #[test]
    fn partial_capabilities_names_only_active() {
        let caps = CapabilitiesUsed { view: true, control: false, file: true, clipboard: false };
        assert_eq!(caps.names(), &["view", "file"]);
    }

    // ── SessionSummary::format_summary ────────────────────────────────────────

    #[test]
    fn format_summary_no_caps_reads_none() {
        let summary = SessionSummary {
            duration: Duration::from_secs(90),
            capabilities: CapabilitiesUsed::default(),
            total_bytes: 0,
        };
        let line = summary.format_summary();
        assert!(line.contains("Capabilities: none"), "got: {line}");
    }

    #[test]
    fn format_summary_lists_active_capabilities() {
        let summary = SessionSummary {
            duration: Duration::from_secs(60),
            capabilities: CapabilitiesUsed { view: true, control: true, file: false, clipboard: false },
            total_bytes: 1_000_000,
        };
        let line = summary.format_summary();
        assert!(line.contains("view, control"), "got: {line}");
        assert!(!line.contains("file"), "got: {line}");
    }

    #[test]
    fn format_summary_duration_minutes_and_seconds() {
        let summary = SessionSummary {
            duration: Duration::from_secs(4 * 60 + 32),
            capabilities: CapabilitiesUsed::default(),
            total_bytes: 0,
        };
        let line = summary.format_summary();
        assert!(line.contains("Duration: 4m 32s"), "got: {line}");
    }

    #[test]
    fn format_summary_data_in_megabytes() {
        let summary = SessionSummary {
            duration: Duration::from_secs(60),
            capabilities: CapabilitiesUsed::default(),
            total_bytes: 6_200_000,
        };
        let line = summary.format_summary();
        assert!(line.contains("6.2 MB"), "got: {line}");
    }

    // ── SessionTracker::record_budget_tick ────────────────────────────────────

    #[test]
    fn record_budget_tick_accumulates_bytes_at_64kbps() {
        // At 64 kbps with the constrained-tier stream split
        // (audio=24k, input=8k, screen_coarse=20k, camera=12k), one tick:
        //   (24_000 + 8_000 + 20_000 + 12_000) / 80 = 64_000 / 80 = 800 bytes.
        let mut tracker = SessionTracker::new();
        tracker.record_budget_tick(24_000, 8_000, 20_000, 12_000, 0, 0);
        assert_eq!(tracker.total_bytes, 800);
    }

    #[test]
    fn record_budget_tick_accumulates_across_ticks() {
        let mut tracker = SessionTracker::new();
        // 1 800 ticks = 30-minute session at 10 Hz.
        for _ in 0..1_800 {
            tracker.record_budget_tick(24_000, 8_000, 20_000, 12_000, 0, 0);
        }
        // 800 bytes/tick × 1 800 ticks = 1 440 000 bytes ≈ 1.44 MB.
        assert_eq!(tracker.total_bytes, 800 * 1_800);
    }

    #[test]
    fn record_budget_tick_zero_streams_adds_no_bytes() {
        let mut tracker = SessionTracker::new();
        tracker.record_budget_tick(0, 0, 0, 0, 0, 0);
        assert_eq!(tracker.total_bytes, 0);
    }

    // ── SessionTracker capability marking ────────────────────────────────────

    #[test]
    fn mark_view_used_sets_view_flag() {
        let mut tracker = SessionTracker::new();
        tracker.mark_view_used();
        let summary = tracker.finish_with_duration(Duration::from_secs(0));
        assert!(summary.capabilities.view);
        assert!(!summary.capabilities.control);
    }

    #[test]
    fn mark_control_used_sets_control_flag() {
        let mut tracker = SessionTracker::new();
        tracker.mark_control_used();
        let summary = tracker.finish_with_duration(Duration::from_secs(0));
        assert!(summary.capabilities.control);
    }

    #[test]
    fn mark_file_used_sets_file_flag() {
        let mut tracker = SessionTracker::new();
        tracker.mark_file_used();
        let summary = tracker.finish_with_duration(Duration::from_secs(0));
        assert!(summary.capabilities.file);
    }

    #[test]
    fn mark_clipboard_used_sets_clipboard_flag() {
        let mut tracker = SessionTracker::new();
        tracker.mark_clipboard_used();
        let summary = tracker.finish_with_duration(Duration::from_secs(0));
        assert!(summary.capabilities.clipboard);
    }

    #[test]
    fn finish_with_duration_preserves_total_bytes_and_capabilities() {
        let mut tracker = SessionTracker::new();
        tracker.mark_view_used();
        tracker.mark_control_used();
        tracker.record_budget_tick(24_000, 8_000, 20_000, 12_000, 0, 0);

        let duration = Duration::from_secs(1800);
        let summary = tracker.finish_with_duration(duration);

        assert_eq!(summary.duration, duration);
        assert!(summary.capabilities.view);
        assert!(summary.capabilities.control);
        assert!(!summary.capabilities.file);
        assert_eq!(summary.total_bytes, 800);
    }

    // ── Feature 151: 30-minute constrained session display ───────────────────

    #[test]
    fn constrained_30_min_summary_format_includes_all_fields() {
        // Simulate a 30-minute constrained assist session with view + control.
        let mut tracker = SessionTracker::new();
        tracker.mark_view_used();
        tracker.mark_control_used();

        for _ in 0..1_800 {
            tracker.record_budget_tick(24_000, 8_000, 20_000, 12_000, 0, 0);
        }

        let summary = tracker.finish_with_duration(Duration::from_secs(30 * 60));
        let line = summary.format_summary();

        assert!(line.contains("Duration: 30m 0s"), "got: {line}");
        assert!(line.contains("view, control"), "got: {line}");
        // 800 bytes × 1_800 ticks = 1.44 MB
        assert!(line.contains("1.4 MB"), "got: {line}");
    }
}

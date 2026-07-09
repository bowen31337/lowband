//! Governor audio step-down policy on bandwidth collapse — Feature 75.
//!
//! When the link bandwidth estimate falls by more than
//! [`COLLAPSE_THRESHOLD_RATIO`] in a single governor tick (10 Hz), the
//! governor applies two immediate changes:
//!
//! 1. **Audio stepped to [`COLLAPSE_AUDIO_BPS`] (12 kbps)** — between the
//!    Survival fallback floor (9 kbps) and the Constrained target (16 kbps).
//!    Pure SILK-WB at 12 kbps keeps voice alive while freeing headroom for
//!    screen and control streams to drain queued frames.
//!
//! 2. **DRED depth deepened to [`COLLAPSE_DRED_DEPTH_FRAMES`]** — packet loss
//!    typically spikes during a rate collapse, so pre-emptively deepening the
//!    DRED redundancy window covers the loss burst that accompanies the rate
//!    drop.  This covers bursts up to the 1 000 ms architecture ceiling.
//!
//! Both changes are held for [`COLLAPSE_HOLD_TICKS`] governor ticks after the
//! collapse is first detected, then released so the tier logic can recover
//! normally.  A second collapse within the hold period resets the counter.
//!
//! # Interaction with the tier system
//!
//! The collapse policy operates *within* the existing tier system: it acts
//! when the tier is Constrained or better (a sudden rate drop while already
//! at Survival is moot — there is nowhere lower to step).  After the hold
//! expires the tier logic re-evaluates the link and may promote or demote the
//! session as usual.
//!
//! # Example
//!
//! ```rust
//! use lowband_platform::collapse_audio::{
//!     CollapseAudioGovernor, CollapseAudioResponse, COLLAPSE_AUDIO_BPS,
//!     COLLAPSE_DRED_DEPTH_FRAMES,
//! };
//!
//! let mut gov = CollapseAudioGovernor::new();
//!
//! // Stable link — no collapse.
//! assert_eq!(gov.tick(400_000.0), CollapseAudioResponse::Normal);
//! assert_eq!(gov.tick(390_000.0), CollapseAudioResponse::Normal);
//!
//! // Bandwidth collapses: 400 → 200 kbps (50% drop in one tick).
//! assert_eq!(
//!     gov.tick(200_000.0),
//!     CollapseAudioResponse::Stepped {
//!         audio_bps: COLLAPSE_AUDIO_BPS,
//!         dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
//!     }
//! );
//! assert!(gov.is_in_collapse());
//! ```

use crate::dred_sender::MAX_DRED_DEPTH_FRAMES;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Audio bitrate (bps) applied when bandwidth collapses.
///
/// 12 kbps sits between the Survival-fallback floor (9 kbps) and the
/// Constrained-tier target (16 kbps).  SILK-WB at 12 kbps delivers
/// intelligible speech; the 4 kbps saving vs. 16 kbps is material when the
/// link is at or near 64 kbps.
pub const COLLAPSE_AUDIO_BPS: u32 = 12_000;

/// DRED depth (frames) used when bandwidth collapses.
///
/// Set to [`MAX_DRED_DEPTH_FRAMES`] (50 frames = 1 000 ms) because a
/// bandwidth collapse is typically accompanied by a burst-loss event that
/// exhausts shallow DRED coverage.  Full-depth DRED costs ≈ 40 kbps overhead
/// but eliminates voice gaps for every burst within the architecture ceiling.
pub const COLLAPSE_DRED_DEPTH_FRAMES: usize = MAX_DRED_DEPTH_FRAMES;

/// Bandwidth drop ratio that triggers the collapse response.
///
/// A collapse is declared when `current_bps < prev_bps × COLLAPSE_THRESHOLD_RATIO`.
/// At 0.70 this triggers when the estimate drops by more than 30 % in a single
/// 10 Hz tick — characteristic of a congestion collapse rather than routine
/// gradual adaptation.
pub const COLLAPSE_THRESHOLD_RATIO: f64 = 0.70;

/// Number of 10 Hz governor ticks the stepped-down settings are held after a
/// collapse is first detected.
///
/// At 10 Hz, 30 ticks = 3 seconds.  This hold prevents oscillation between
/// 12 kbps and 16 kbps while the bandwidth estimate is unstable in the
/// seconds immediately following a collapse.
pub const COLLAPSE_HOLD_TICKS: u32 = 30;

// ── CollapseAudioResponse ─────────────────────────────────────────────────────

/// The audio and DRED settings the governor must apply on each tick.
///
/// Returned by [`CollapseAudioGovernor::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapseAudioResponse {
    /// No active collapse; use tier-default audio and DRED settings.
    Normal,
    /// Collapse in progress; apply the stepped-down audio rate and deep DRED.
    Stepped {
        /// Audio encoder target bitrate (bps).  Always [`COLLAPSE_AUDIO_BPS`].
        audio_bps: u32,
        /// DRED depth in frames.  Always [`COLLAPSE_DRED_DEPTH_FRAMES`].
        dred_depth_frames: usize,
    },
}

// ── CollapseAudioGovernor ─────────────────────────────────────────────────────

/// Stateful governor that steps audio to 12 kbps and deepens DRED depth on a
/// bandwidth collapse (Feature 75).
///
/// Call [`tick`](CollapseAudioGovernor::tick) once per governor interval
/// (10 Hz typical) with the current bandwidth estimate.  The first call after
/// construction always returns [`CollapseAudioResponse::Normal`] because there
/// is no previous estimate to compare against.
///
/// Zero heap allocation; cheap to construct and clone.
#[derive(Debug, Clone)]
pub struct CollapseAudioGovernor {
    /// Last observed bandwidth estimate (bps).  `None` on the first tick.
    prev_bps: Option<f64>,
    /// Ticks remaining in the post-collapse hold-down.  Zero means the hold
    /// has expired and `Normal` may be returned on the next tick.
    hold_remaining: u32,
}

impl CollapseAudioGovernor {
    /// Create a new governor, ready on the first tick.
    pub fn new() -> Self {
        Self { prev_bps: None, hold_remaining: 0 }
    }

    /// Feed the current bandwidth estimate and return the audio/DRED policy.
    ///
    /// Must be called once per governor tick (10 Hz typical).
    ///
    /// A collapse is declared when `current_bps < prev_bps × COLLAPSE_THRESHOLD_RATIO`.
    /// The first call is always [`CollapseAudioResponse::Normal`] (no previous
    /// estimate).  A second collapse within the hold window resets the counter
    /// to [`COLLAPSE_HOLD_TICKS`].
    pub fn tick(&mut self, current_bps: f64) -> CollapseAudioResponse {
        let is_collapse = match self.prev_bps {
            Some(prev) => prev > 0.0 && current_bps < prev * COLLAPSE_THRESHOLD_RATIO,
            None => false,
        };

        if is_collapse {
            // (Re)start the hold-down on every new collapse event.
            self.hold_remaining = COLLAPSE_HOLD_TICKS;
        }

        self.prev_bps = Some(current_bps);

        let in_collapse = is_collapse || self.hold_remaining > 0;

        // Decrement after the response check so all COLLAPSE_HOLD_TICKS ticks
        // following a collapse tick are covered by the Stepped response.
        if !is_collapse && self.hold_remaining > 0 {
            self.hold_remaining -= 1;
        }

        if in_collapse {
            CollapseAudioResponse::Stepped {
                audio_bps: COLLAPSE_AUDIO_BPS,
                dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
            }
        } else {
            CollapseAudioResponse::Normal
        }
    }

    /// Whether the collapse hold-down is currently active.
    ///
    /// Returns `true` from the tick that first detected a collapse until
    /// [`COLLAPSE_HOLD_TICKS`] ticks later (inclusive of the collapse tick).
    pub fn is_in_collapse(&self) -> bool {
        self.hold_remaining > 0
    }

    /// Ticks remaining in the current hold-down window.
    ///
    /// Zero when idle; positive while the governor is in the stepped-down
    /// state following a collapse.
    pub fn hold_remaining(&self) -> u32 {
        self.hold_remaining
    }
}

impl Default for CollapseAudioGovernor {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn collapse_audio_bps_is_12kbps() {
        assert_eq!(COLLAPSE_AUDIO_BPS, 12_000, "Feature 75: collapse audio target must be 12 kbps");
    }

    #[test]
    fn collapse_dred_depth_is_max() {
        assert_eq!(
            COLLAPSE_DRED_DEPTH_FRAMES,
            MAX_DRED_DEPTH_FRAMES,
            "DRED depth must be the architecture ceiling (50 frames = 1 000 ms) on collapse"
        );
    }

    #[test]
    fn collapse_audio_bps_between_survival_and_constrained() {
        use crate::opus_encoder::{CONSTRAINED_AUDIO_BPS, SURVIVAL_FALLBACK_AUDIO_BPS};
        assert!(
            COLLAPSE_AUDIO_BPS > SURVIVAL_FALLBACK_AUDIO_BPS,
            "collapse target ({COLLAPSE_AUDIO_BPS} bps) must be above survival floor \
             ({SURVIVAL_FALLBACK_AUDIO_BPS} bps)"
        );
        assert!(
            COLLAPSE_AUDIO_BPS < CONSTRAINED_AUDIO_BPS,
            "collapse target ({COLLAPSE_AUDIO_BPS} bps) must be below constrained target \
             ({CONSTRAINED_AUDIO_BPS} bps)"
        );
    }

    #[test]
    fn collapse_threshold_ratio_is_0_70() {
        assert!(
            (COLLAPSE_THRESHOLD_RATIO - 0.70).abs() < 1e-9,
            "threshold ratio must be 0.70 (30 % drop triggers collapse)"
        );
    }

    #[test]
    fn collapse_hold_ticks_is_30() {
        assert_eq!(COLLAPSE_HOLD_TICKS, 30, "hold must be 30 ticks (3 s at 10 Hz)");
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_has_no_hold_and_no_prev() {
        let gov = CollapseAudioGovernor::new();
        assert_eq!(gov.hold_remaining(), 0);
        assert!(!gov.is_in_collapse());
    }

    #[test]
    fn default_equals_new() {
        let a = CollapseAudioGovernor::new();
        let b = CollapseAudioGovernor::default();
        assert_eq!(a.hold_remaining(), b.hold_remaining());
        assert_eq!(a.is_in_collapse(), b.is_in_collapse());
    }

    // ── First tick is always Normal ───────────────────────────────────────────

    #[test]
    fn first_tick_is_normal_regardless_of_rate() {
        for rate in [1.0_f64, 100_000.0, 1_000_000.0] {
            let mut gov = CollapseAudioGovernor::new();
            assert_eq!(
                gov.tick(rate),
                CollapseAudioResponse::Normal,
                "first tick must be Normal (no previous estimate) at rate {rate}"
            );
        }
    }

    #[test]
    fn first_tick_does_not_set_hold() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(1.0); // arbitrarily low — no previous to compare against
        assert_eq!(gov.hold_remaining(), 0);
        assert!(!gov.is_in_collapse());
    }

    // ── Normal: no collapse on stable or rising bandwidth ────────────────────

    #[test]
    fn stable_rate_returns_normal() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        assert_eq!(gov.tick(400_000.0), CollapseAudioResponse::Normal);
    }

    #[test]
    fn rising_rate_returns_normal() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(200_000.0);
        assert_eq!(gov.tick(400_000.0), CollapseAudioResponse::Normal);
    }

    #[test]
    fn gradual_drop_below_threshold_returns_normal() {
        // 29 % drop — just below the 30 % trigger.
        let mut gov = CollapseAudioGovernor::new();
        let prev = 400_000.0_f64;
        gov.tick(prev);
        let slight_drop = prev * 0.71; // 29 % drop — above COLLAPSE_THRESHOLD_RATIO
        assert_eq!(
            gov.tick(slight_drop),
            CollapseAudioResponse::Normal,
            "a 29 % drop ({slight_drop:.0} bps) must not trigger collapse"
        );
    }

    // ── Collapse trigger ──────────────────────────────────────────────────────

    #[test]
    fn thirty_percent_drop_triggers_collapse() {
        let mut gov = CollapseAudioGovernor::new();
        let prev = 400_000.0_f64;
        gov.tick(prev);
        // Exactly at threshold: current = prev × 0.70 − 1 bps (just below).
        let at_collapse = prev * COLLAPSE_THRESHOLD_RATIO - 1.0;
        assert_eq!(
            gov.tick(at_collapse),
            CollapseAudioResponse::Stepped {
                audio_bps: COLLAPSE_AUDIO_BPS,
                dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
            },
            "a >30 % drop ({at_collapse:.0} bps from {prev:.0}) must trigger collapse"
        );
    }

    #[test]
    fn fifty_percent_drop_triggers_collapse() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        let result = gov.tick(200_000.0);
        assert_eq!(
            result,
            CollapseAudioResponse::Stepped {
                audio_bps: COLLAPSE_AUDIO_BPS,
                dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
            }
        );
    }

    #[test]
    fn collapse_response_carries_correct_audio_bps() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        let resp = gov.tick(100_000.0);
        if let CollapseAudioResponse::Stepped { audio_bps, .. } = resp {
            assert_eq!(audio_bps, COLLAPSE_AUDIO_BPS);
        } else {
            panic!("expected Stepped, got {resp:?}");
        }
    }

    #[test]
    fn collapse_response_carries_correct_dred_depth() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        let resp = gov.tick(100_000.0);
        if let CollapseAudioResponse::Stepped { dred_depth_frames, .. } = resp {
            assert_eq!(dred_depth_frames, COLLAPSE_DRED_DEPTH_FRAMES);
        } else {
            panic!("expected Stepped, got {resp:?}");
        }
    }

    #[test]
    fn is_in_collapse_true_immediately_after_detection() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(100_000.0); // collapse
        assert!(gov.is_in_collapse());
    }

    // ── Hold-down: Stepped persists for COLLAPSE_HOLD_TICKS ──────────────────

    #[test]
    fn hold_set_to_collapse_hold_ticks_after_collapse() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(100_000.0); // collapse tick → hold_remaining = COLLAPSE_HOLD_TICKS
        assert_eq!(gov.hold_remaining(), COLLAPSE_HOLD_TICKS);
    }

    #[test]
    fn stepped_persists_during_hold_on_stable_rate() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(100_000.0); // collapse

        // Stable rate during hold: all ticks must remain Stepped.
        for tick in 0..COLLAPSE_HOLD_TICKS {
            let resp = gov.tick(100_000.0);
            assert_eq!(
                resp,
                CollapseAudioResponse::Stepped {
                    audio_bps: COLLAPSE_AUDIO_BPS,
                    dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
                },
                "hold tick {tick}: expected Stepped, hold_remaining={}",
                gov.hold_remaining()
            );
        }
    }

    #[test]
    fn normal_resumes_after_hold_expires() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(200_000.0); // collapse

        // Drain the hold with stable ticks.
        for _ in 0..COLLAPSE_HOLD_TICKS {
            gov.tick(200_000.0);
        }

        // One more tick at the same rate: hold expired, rate not dropping — Normal.
        assert_eq!(
            gov.tick(200_000.0),
            CollapseAudioResponse::Normal,
            "after hold expires, Normal must resume when rate is stable"
        );
    }

    #[test]
    fn hold_remaining_decrements_each_tick() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(100_000.0); // collapse: hold = COLLAPSE_HOLD_TICKS

        for expected in (0..COLLAPSE_HOLD_TICKS).rev() {
            gov.tick(100_000.0);
            assert_eq!(
                gov.hold_remaining(),
                expected,
                "hold_remaining must decrement monotonically"
            );
        }
        // After the loop, hold = 0 and is_in_collapse is false.
        assert!(!gov.is_in_collapse());
    }

    #[test]
    fn hold_zero_after_expiry() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(100_000.0); // collapse

        for _ in 0..=COLLAPSE_HOLD_TICKS {
            gov.tick(200_000.0);
        }
        assert_eq!(gov.hold_remaining(), 0);
        assert!(!gov.is_in_collapse());
    }

    // ── Second collapse resets hold ───────────────────────────────────────────

    #[test]
    fn second_collapse_resets_hold_counter() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(200_000.0); // first collapse: hold = 30

        // Burn 15 ticks of hold.
        for _ in 0..15 {
            gov.tick(200_000.0);
        }
        assert_eq!(gov.hold_remaining(), COLLAPSE_HOLD_TICKS - 15);

        // Second collapse: 200k → 80k (60 % drop).
        gov.tick(80_000.0);
        assert_eq!(
            gov.hold_remaining(),
            COLLAPSE_HOLD_TICKS,
            "second collapse must reset the hold counter to COLLAPSE_HOLD_TICKS"
        );
    }

    #[test]
    fn stepped_still_active_during_second_collapse_hold() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(200_000.0); // first collapse

        // Let half the hold drain.
        for _ in 0..(COLLAPSE_HOLD_TICKS / 2) {
            gov.tick(200_000.0);
        }

        // Second collapse.
        let resp = gov.tick(50_000.0);
        assert_eq!(
            resp,
            CollapseAudioResponse::Stepped {
                audio_bps: COLLAPSE_AUDIO_BPS,
                dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
            }
        );
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn zero_prev_bps_does_not_trigger_collapse() {
        // If the previous estimate was 0 the comparison is undefined; the guard
        // `prev > 0.0` ensures we never divide-by-zero or false-trigger.
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(0.0); // first tick — stored as prev, no collapse possible
        // Second tick: prev = 0.0 → guard fires, no collapse.
        assert_eq!(
            gov.tick(0.0),
            CollapseAudioResponse::Normal,
            "zero prev bps must not trigger a collapse"
        );
    }

    #[test]
    fn rate_recovering_to_above_threshold_after_hold_is_normal() {
        let mut gov = CollapseAudioGovernor::new();
        gov.tick(400_000.0);
        gov.tick(200_000.0); // collapse

        for _ in 0..COLLAPSE_HOLD_TICKS {
            gov.tick(200_000.0);
        }

        // Rate has recovered; hold has expired.
        assert_eq!(gov.tick(380_000.0), CollapseAudioResponse::Normal);
    }

    #[test]
    fn collapse_exact_at_threshold_boundary_is_normal() {
        // current_bps == prev * COLLAPSE_THRESHOLD_RATIO is NOT a collapse;
        // the condition is strictly less-than.
        let mut gov = CollapseAudioGovernor::new();
        let prev = 400_000.0_f64;
        gov.tick(prev);
        let exactly_at = prev * COLLAPSE_THRESHOLD_RATIO;
        assert_eq!(
            gov.tick(exactly_at),
            CollapseAudioResponse::Normal,
            "rate exactly at threshold (not below) must not trigger collapse"
        );
    }

    // ── Full scenario: 400 → 200 kbps collapse then recovery ─────────────────

    #[test]
    fn full_collapse_and_recovery_scenario() {
        let mut gov = CollapseAudioGovernor::new();

        // Phase 1: stable link at 400 kbps.
        for _ in 0..10 {
            assert_eq!(
                gov.tick(400_000.0),
                if gov.hold_remaining() == 0 {
                    CollapseAudioResponse::Normal
                } else {
                    CollapseAudioResponse::Stepped {
                        audio_bps: COLLAPSE_AUDIO_BPS,
                        dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
                    }
                }
            );
        }

        // Phase 2: collapse at tick 11.
        let resp = gov.tick(200_000.0);
        assert_eq!(
            resp,
            CollapseAudioResponse::Stepped {
                audio_bps: COLLAPSE_AUDIO_BPS,
                dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
            },
            "collapse tick must return Stepped"
        );
        assert_eq!(gov.hold_remaining(), COLLAPSE_HOLD_TICKS);

        // Phase 3: hold period at 200 kbps (stable).
        for tick in 0..COLLAPSE_HOLD_TICKS {
            let resp = gov.tick(200_000.0);
            assert_eq!(
                resp,
                CollapseAudioResponse::Stepped {
                    audio_bps: COLLAPSE_AUDIO_BPS,
                    dred_depth_frames: COLLAPSE_DRED_DEPTH_FRAMES,
                },
                "hold tick {tick} must still be Stepped"
            );
        }

        // Phase 4: hold expired — Normal resumes.
        assert_eq!(
            gov.tick(200_000.0),
            CollapseAudioResponse::Normal,
            "first tick after hold expiry must be Normal"
        );
        assert!(!gov.is_in_collapse());
    }
}

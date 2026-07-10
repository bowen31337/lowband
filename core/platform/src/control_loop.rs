//! Governor 10 Hz control loop — Feature 67.
//!
//! [`ControlLoop`] is the single entry point the governor main thread calls
//! once every 100 ms.  It consumes four network/platform observables,
//! propagates them through the tier state machine and budget allocator, and
//! returns a fully computed [`ControlLoopOutput`] ready for downstream
//! consumers.
//!
//! # Loop inputs
//!
//! | Field        | Type                | Source                              |
//! |--------------|---------------------|-------------------------------------|
//! | `bwe_bps`    | `u32`               | Delay-gradient congestion controller|
//! | `rtt_ms`     | `u32`               | LBTP probe echoes                   |
//! | `loss_ppm`   | `u32`               | LBTP sequence-number gaps           |
//! | `thermal`    | [`ThermalPressure`] | [`ThermalMonitor::sample`]          |
//!
//! # Loop outputs
//!
//! | Field      | Type               | Consumer                              |
//! |------------|--------------------|---------------------------------------|
//! | `tier`     | [`TierState`]      | Load-shedding policy, load display    |
//! | `budgets`  | [`StreamBudgets`]  | Codec encoders, LBTP pacer            |
//! | `summary`  | [`GovernorSummary`]| Peer exchange for weaker-peer convergence |
//!
//! # Usage
//!
//! ```rust
//! use lowband_platform::control_loop::{ControlLoop, ControlLoopInput};
//! use lowband_platform::thermal::ThermalPressure;
//! use lowband_platform::TierState;
//!
//! let mut gov = ControlLoop::new();
//!
//! // Simulate one tick at 400 kbps, 30 ms RTT, 0.1% loss, nominal thermal.
//! let input = ControlLoopInput {
//!     bwe_bps:  400_000,
//!     rtt_ms:   30,
//!     loss_ppm: 1_000,
//!     thermal:  ThermalPressure::Nominal,
//! };
//! let out = gov.tick(input);
//!
//! // After one tick from a cold start the emitter has not yet accumulated the
//! // 50-tick upgrade hold required to leave Survival, so the tier remains Survival.
//! // The allocator still funds audio above the 6 kbps floor.
//! use lowband_platform::gear_policy::AUDIO_FLOOR_BPS;
//! assert!(out.budgets.audio_bps >= AUDIO_FLOOR_BPS);
//!
//! // The summary captures this tick's observables for peer exchange.
//! assert_eq!(out.summary.rtt_ms, 30);
//! assert_eq!(out.summary.loss_ppm, 1_000);
//! ```

use crate::gear_policy::{allocate, GearConstraints, StreamBudgets};
use crate::governor_summary::GovernorSummary;
use crate::thermal::ThermalPressure;
use crate::tier::TierState;
use crate::tier_classifier::GovernorTierEmitter;

// ── ControlLoopInput ─────────────────────────────────────────────────────────

/// All four observables consumed by one 10 Hz governor tick.
///
/// The congestion controller and transport layer refresh these values every
/// tick; the governor reads them once and propagates them through the tier
/// state machine and budget allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlLoopInput {
    /// Current bandwidth estimate from the congestion controller (bps).
    ///
    /// The delay-gradient controller (Feature 13) produces this value.
    /// It is the primary signal driving tier classification and budget
    /// allocation.
    pub bwe_bps: u32,

    /// Round-trip time measured on the local path (milliseconds).
    ///
    /// Derived from LBTP probe echoes.  Included in the [`GovernorSummary`]
    /// sent to the remote peer; both sides take the conservative (higher) RTT.
    pub rtt_ms: u32,

    /// Observed packet loss rate in parts per million (0 = no loss,
    /// 1_000_000 = 100%).
    ///
    /// Derived from LBTP sequence-number gaps.  Included in the
    /// [`GovernorSummary`]; both sides drive FEC depth from the worst path.
    pub loss_ppm: u32,

    /// Current thermal pressure level sampled from the OS.
    ///
    /// Obtained by calling [`crate::thermal::ThermalMonitor::sample`] before
    /// each tick.  Governs codec-gear degradation (Feature 161) and the
    /// constraints passed to the budget allocator.
    pub thermal: ThermalPressure,
}

// ── ControlLoopOutput ────────────────────────────────────────────────────────

/// Fully-computed outputs of one 10 Hz governor tick.
///
/// All fields are ready for immediate downstream consumption; no further
/// computation is required by the caller.
#[derive(Debug, Clone, Copy)]
pub struct ControlLoopOutput {
    /// Session quality tier emitted this interval (Feature 68).
    ///
    /// Produced by the hysteretic tier emitter: fast downgrade (Feature 70)
    /// fires within one tick; probe-validated upgrade (Feature 71) requires
    /// 50 consecutive ticks of sufficient headroom.
    pub tier: TierState,

    /// Per-stream bitrate allocations for this interval (Feature 69).
    ///
    /// Computed by strict-priority allocation under the thermal constraints;
    /// `audio_bps` is always ≥ [`crate::gear_policy::AUDIO_FLOOR_BPS`].
    pub budgets: StreamBudgets,

    /// Local governor snapshot for peer exchange (Feature 73).
    ///
    /// The remote peer calls [`crate::governor_summary::converge_summaries`]
    /// on receiving this to derive session-wide effective parameters.
    pub summary: GovernorSummary,
}

// ── ControlLoop ──────────────────────────────────────────────────────────────

/// Governor 10 Hz control loop — Feature 67.
///
/// Holds the only mutable state needed across ticks: the current tier and the
/// upgrade-hold counter inside [`GovernorTierEmitter`].  Everything else is
/// computed fresh each tick from the four input observables.
///
/// # Tick contract
///
/// Callers must invoke [`tick`](ControlLoop::tick) exactly once per 100 ms
/// governor interval.  Skipping or doubling ticks violates the 5-second
/// upgrade hold semantics.
///
/// # Thread safety
///
/// `ControlLoop` is `!Send + !Sync` by default (no inner synchronisation).
/// The governor main thread owns it exclusively; cross-thread access requires
/// an external lock.
#[derive(Debug)]
pub struct ControlLoop {
    emitter: GovernorTierEmitter,
    current_tier: TierState,
}

impl ControlLoop {
    /// Create a new control loop in the initial Survival tier with no
    /// accumulated upgrade hold.
    pub fn new() -> Self {
        Self {
            emitter: GovernorTierEmitter::new(),
            current_tier: TierState::Survival,
        }
    }

    /// Run one 10 Hz governor tick.
    ///
    /// # Returns
    ///
    /// A [`ControlLoopOutput`] containing the emitted tier, per-stream budget
    /// allocations, and a [`GovernorSummary`] ready to transmit to the remote
    /// peer.
    pub fn tick(&mut self, input: ControlLoopInput) -> ControlLoopOutput {
        // 1. Tier state machine: fast downgrade + probe-validated upgrade.
        let tier = self.emitter.tick(self.current_tier, input.bwe_bps);
        self.current_tier = tier;

        // 2. Thermal-adjusted encoder constraints.
        let constraints = GearConstraints::from_thermal(input.thermal);

        // 3. Strict-priority budget allocation.
        let budgets = allocate(input.bwe_bps, &constraints);

        // 4. Governor summary for peer exchange.
        let summary = GovernorSummary {
            tier,
            bwe_bps: input.bwe_bps,
            rtt_ms: input.rtt_ms,
            loss_ppm: input.loss_ppm,
        };

        ControlLoopOutput { tier, budgets, summary }
    }

    /// The tier emitted on the most recent tick.
    ///
    /// Before the first tick this returns [`TierState::Survival`].
    #[inline]
    pub fn current_tier(&self) -> TierState {
        self.current_tier
    }

    /// Consecutive ticks of upgrade-headroom accumulated toward the next tier.
    ///
    /// Delegates to [`GovernorTierEmitter::upgrade_hold_ticks`].  Useful for
    /// diagnostics and tests.
    #[inline]
    pub fn upgrade_hold_ticks(&self) -> u32 {
        self.emitter.upgrade_hold_ticks()
    }
}

impl Default for ControlLoop {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear_policy::AUDIO_FLOOR_BPS;
    use crate::tier_downgrade_guard::{
        COMFORTABLE_FLOOR_BPS, CONSTRAINED_FLOOR_BPS, FULL_FLOOR_BPS,
    };

    fn nominal_input(bwe_bps: u32) -> ControlLoopInput {
        ControlLoopInput {
            bwe_bps,
            rtt_ms: 40,
            loss_ppm: 0,
            thermal: ThermalPressure::Nominal,
        }
    }

    // ── cold start ────────────────────────────────────────────────────────────

    #[test]
    fn initial_tier_is_survival() {
        let gov = ControlLoop::new();
        assert_eq!(gov.current_tier(), TierState::Survival,
            "ControlLoop must start at Survival before any tick");
    }

    #[test]
    fn initial_upgrade_hold_is_zero() {
        let gov = ControlLoop::new();
        assert_eq!(gov.upgrade_hold_ticks(), 0);
    }

    // ── audio floor always preserved ─────────────────────────────────────────

    #[test]
    fn audio_floor_preserved_at_zero_bwe() {
        let mut gov = ControlLoop::new();
        let out = gov.tick(nominal_input(0));
        assert!(
            out.budgets.audio_bps >= AUDIO_FLOOR_BPS,
            "audio_bps must be ≥ {AUDIO_FLOOR_BPS} even at 0 bps BWE; got {}",
            out.budgets.audio_bps
        );
    }

    #[test]
    fn audio_floor_preserved_at_survival_level() {
        let mut gov = ControlLoop::new();
        let out = gov.tick(nominal_input(48_000));
        assert!(out.budgets.audio_bps >= AUDIO_FLOOR_BPS);
    }

    #[test]
    fn audio_floor_preserved_at_full_tier() {
        let mut gov = ControlLoop::new();
        let out = gov.tick(nominal_input(FULL_FLOOR_BPS + 50_000));
        assert!(out.budgets.audio_bps >= AUDIO_FLOOR_BPS);
    }

    // ── summary captures inputs verbatim ─────────────────────────────────────

    #[test]
    fn summary_captures_rtt_and_loss() {
        let mut gov = ControlLoop::new();
        let input = ControlLoopInput {
            bwe_bps:  128_000,
            rtt_ms:   75,
            loss_ppm: 12_000,
            thermal:  ThermalPressure::Nominal,
        };
        let out = gov.tick(input);
        assert_eq!(out.summary.rtt_ms, 75, "summary.rtt_ms must mirror input.rtt_ms");
        assert_eq!(out.summary.loss_ppm, 12_000, "summary.loss_ppm must mirror input.loss_ppm");
        assert_eq!(out.summary.bwe_bps, 128_000, "summary.bwe_bps must mirror input.bwe_bps");
    }

    #[test]
    fn summary_tier_matches_emitted_tier() {
        let mut gov = ControlLoop::new();
        // Drive up to Constrained (50 ticks).
        for _ in 0..50 {
            gov.tick(nominal_input(CONSTRAINED_FLOOR_BPS));
        }
        let out = gov.tick(nominal_input(CONSTRAINED_FLOOR_BPS));
        assert_eq!(out.summary.tier, out.tier,
            "summary.tier must equal the emitted tier");
    }

    // ── tier progression mirrors GovernorTierEmitter ──────────────────────────

    #[test]
    fn upgrade_blocked_until_50_ticks() {
        let mut gov = ControlLoop::new();
        for _ in 0..49 {
            let out = gov.tick(nominal_input(CONSTRAINED_FLOOR_BPS));
            assert_eq!(out.tier, TierState::Survival,
                "tier must stay at Survival before 50-tick hold");
        }
        let out = gov.tick(nominal_input(CONSTRAINED_FLOOR_BPS));
        assert_eq!(out.tier, TierState::Constrained,
            "tier must step to Constrained on the 50th tick");
    }

    #[test]
    fn downgrade_fires_on_next_tick() {
        let mut gov = ControlLoop::new();
        // Reach Constrained.
        for _ in 0..50 {
            gov.tick(nominal_input(CONSTRAINED_FLOOR_BPS));
        }
        // BWE drops well below the Constrained downgrade trigger.
        let trigger = CONSTRAINED_FLOOR_BPS * 4 / 5;
        let out = gov.tick(nominal_input(trigger - 1));
        assert_eq!(out.tier, TierState::Survival,
            "downgrade must fire immediately when BWE drops below 0.8 × floor");
    }

    // ── thermal routing ───────────────────────────────────────────────────────

    #[test]
    fn critical_thermal_zeros_camera_budget() {
        let mut gov = ControlLoop::new();
        // Push to Full tier first so camera would normally be funded.
        for _ in 0..160 {
            gov.tick(nominal_input(FULL_FLOOR_BPS + 50_000));
        }
        let out = gov.tick(ControlLoopInput {
            bwe_bps:  FULL_FLOOR_BPS + 50_000,
            rtt_ms:   20,
            loss_ppm: 0,
            thermal:  ThermalPressure::Critical,
        });
        assert_eq!(out.budgets.camera_bps, 0,
            "camera must be off at Critical thermal pressure");
        assert!(out.budgets.audio_bps >= AUDIO_FLOOR_BPS,
            "audio floor must hold even at Critical thermal");
    }

    #[test]
    fn nominal_thermal_allows_camera_at_full_tier() {
        let mut gov = ControlLoop::new();
        for _ in 0..160 {
            gov.tick(nominal_input(FULL_FLOOR_BPS + 100_000));
        }
        let out = gov.tick(nominal_input(FULL_FLOOR_BPS + 100_000));
        assert!(out.budgets.camera_bps > 0,
            "camera must be funded at Full tier with Nominal thermal");
    }

    // ── current_tier accessor ─────────────────────────────────────────────────

    #[test]
    fn current_tier_tracks_last_tick_output() {
        let mut gov = ControlLoop::new();
        for _ in 0..50 {
            gov.tick(nominal_input(COMFORTABLE_FLOOR_BPS));
        }
        assert_eq!(gov.current_tier(), TierState::Constrained,
            "current_tier() must reflect the tier from the last tick");
    }

    // ── default == new ────────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        let a = ControlLoop::new();
        let b = ControlLoop::default();
        assert_eq!(a.current_tier(), b.current_tier());
        assert_eq!(a.upgrade_hold_ticks(), b.upgrade_hold_ticks());
    }
}

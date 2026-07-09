//! Governor summary exchange — Feature 73.
//!
//! Each peer sends a compact [`GovernorSummary`] of its local governor state at
//! every 10 Hz tick.  When the remote peer's summary arrives, the session
//! converges on the weaker peer by taking the minimum bandwidth estimate and
//! tier via [`converge_summaries`].
//!
//! # Weaker-peer convergence
//!
//! On an asymmetric link (common on 3G/ADSL), upload capacity differs between
//! peers.  Both must encode at the rate the *other side can receive*, not at the
//! rate their own uplink allows.  Exchanging governor summaries lets each side
//! learn the remote bandwidth estimate and apply the more conservative value:
//!
//! | Quantity   | Rule                          | Rationale                        |
//! |------------|-------------------------------|----------------------------------|
//! | `bwe_bps`  | `min(local, remote)`          | Bottleneck is the tighter uplink |
//! | `tier`     | `min(local, remote)`          | Session runs at the lowest tier  |
//! | `rtt_ms`   | `max(local, remote)`          | Conservative latency budget      |
//! | `loss_ppm` | `max(local, remote)`          | Worst observed path loss         |
//!
//! The governor main loop feeds the resulting [`ConvergedSummary`] into the
//! allocator and codec-gear selection in place of relying solely on local BWE.
//!
//! # Usage
//!
//! ```rust
//! use lowband_platform::governor_summary::{
//!     GovernorSummary, converge_summaries,
//! };
//! use lowband_platform::TierState;
//!
//! let local = GovernorSummary {
//!     tier: TierState::Comfortable,
//!     bwe_bps: 150_000,
//!     rtt_ms: 80,
//!     loss_ppm: 5_000,   // 0.5 %
//! };
//! let remote = GovernorSummary {
//!     tier: TierState::Constrained,
//!     bwe_bps: 64_000,
//!     rtt_ms: 120,
//!     loss_ppm: 20_000,  // 2 %
//! };
//!
//! let effective = converge_summaries(&local, &remote);
//! assert_eq!(effective.tier, TierState::Constrained);
//! assert_eq!(effective.bwe_bps, 64_000);
//! assert_eq!(effective.rtt_ms, 120);
//! assert_eq!(effective.loss_ppm, 20_000);
//! ```

use crate::tier::TierState;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Parts-per-million divisor for converting [`GovernorSummary::loss_ppm`] to a
/// fractional loss rate: `loss_rate = loss_ppm as f64 / LOSS_PPM_SCALE as f64`.
pub const LOSS_PPM_SCALE: u32 = 1_000_000;

/// Interval at which summaries are generated and sent (100 ms, matching the
/// 10 Hz governor control loop).
pub const SUMMARY_INTERVAL_MS: u32 = 100;

// ── GovernorSummary ──────────────────────────────────────────────────────────

/// A compact snapshot of the local governor's state, sent to the remote peer
/// once per 10 Hz governor tick (Feature 73).
///
/// All fields are measured locally.  The remote peer runs
/// [`converge_summaries`] after receiving this struct to compute the
/// session-wide effective parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorSummary {
    /// Current tier emitted by the local governor (Feature 68).
    pub tier: TierState,

    /// Local bandwidth estimate in bits per second.
    ///
    /// Derived from the delay-gradient congestion controller (Feature 13).
    /// The receiver takes `min(local.bwe_bps, remote.bwe_bps)` as the
    /// session-wide effective budget.
    pub bwe_bps: u32,

    /// Round-trip time measured on the local path in milliseconds.
    ///
    /// Computed from LBTP probe echoes.  Convergence takes
    /// `max(local.rtt_ms, remote.rtt_ms)` to bound jitter-buffer sizing
    /// conservatively.
    pub rtt_ms: u32,

    /// Observed packet loss rate in parts per million (0 = no loss,
    /// 1_000_000 = 100 %).
    ///
    /// Computed from LBTP sequence-number gaps.  Convergence takes
    /// `max(local.loss_ppm, remote.loss_ppm)` to drive FEC depth from the
    /// worse of the two paths.
    pub loss_ppm: u32,
}

impl GovernorSummary {
    /// Convert `loss_ppm` to a fractional loss rate in `[0.0, 1.0]`.
    #[inline]
    pub fn loss_fraction(&self) -> f64 {
        self.loss_ppm as f64 / LOSS_PPM_SCALE as f64
    }
}

// ── ConvergedSummary ─────────────────────────────────────────────────────────

/// Effective session parameters after converging local and remote governor
/// summaries onto the weaker peer.
///
/// Produced by [`converge_summaries`] and consumed by the governor's allocator
/// and codec-gear selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvergedSummary {
    /// Effective session tier — the lower of the two peers' tiers.
    pub tier: TierState,

    /// Effective bandwidth budget in bps — the lower of the two peers' BWE.
    ///
    /// This is the value passed to [`crate::gear_policy::allocate`] each tick
    /// once convergence is in effect.
    pub bwe_bps: u32,

    /// Conservative round-trip time in milliseconds — the higher of the two
    /// peers' RTT measurements.
    pub rtt_ms: u32,

    /// Worst-path loss in parts per million — the higher of the two peers'
    /// loss observations.
    pub loss_ppm: u32,
}

// ── converge_summaries ───────────────────────────────────────────────────────

/// Produce the session-wide effective parameters by converging `local` and
/// `remote` governor summaries onto the weaker peer (Feature 73).
///
/// Convergence rules (see module doc for rationale):
///
/// * `tier`     — `min(local.tier,     remote.tier)`
/// * `bwe_bps`  — `min(local.bwe_bps,  remote.bwe_bps)`
/// * `rtt_ms`   — `max(local.rtt_ms,   remote.rtt_ms)`
/// * `loss_ppm` — `max(local.loss_ppm, remote.loss_ppm)`
///
/// # Panics
///
/// Never panics.  All inputs are `u32`; min/max operations are infallible.
#[inline]
pub fn converge_summaries(
    local: &GovernorSummary,
    remote: &GovernorSummary,
) -> ConvergedSummary {
    ConvergedSummary {
        tier:     local.tier.min(remote.tier),
        bwe_bps:  local.bwe_bps.min(remote.bwe_bps),
        rtt_ms:   local.rtt_ms.max(remote.rtt_ms),
        loss_ppm: local.loss_ppm.max(remote.loss_ppm),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn full_peer(bwe_bps: u32, rtt_ms: u32, loss_ppm: u32) -> GovernorSummary {
        GovernorSummary { tier: TierState::Full, bwe_bps, rtt_ms, loss_ppm }
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn loss_ppm_scale_is_one_million() {
        assert_eq!(LOSS_PPM_SCALE, 1_000_000);
    }

    #[test]
    fn summary_interval_is_100ms() {
        assert_eq!(SUMMARY_INTERVAL_MS, 100, "10 Hz governor → 100 ms interval");
    }

    // ── loss_fraction ─────────────────────────────────────────────────────────

    #[test]
    fn loss_fraction_zero_ppm_is_zero() {
        let s = full_peer(100_000, 50, 0);
        assert!((s.loss_fraction() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn loss_fraction_one_million_ppm_is_one() {
        let s = full_peer(100_000, 50, LOSS_PPM_SCALE);
        assert!((s.loss_fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn loss_fraction_50_000_ppm_is_5_percent() {
        let s = full_peer(100_000, 50, 50_000);
        assert!((s.loss_fraction() - 0.05).abs() < 1e-9);
    }

    // ── symmetric peers ───────────────────────────────────────────────────────

    #[test]
    fn identical_peers_converge_to_same_values() {
        let s = GovernorSummary {
            tier: TierState::Comfortable,
            bwe_bps: 150_000,
            rtt_ms: 80,
            loss_ppm: 10_000,
        };
        let c = converge_summaries(&s, &s);
        assert_eq!(c.tier, TierState::Comfortable);
        assert_eq!(c.bwe_bps, 150_000);
        assert_eq!(c.rtt_ms, 80);
        assert_eq!(c.loss_ppm, 10_000);
    }

    // ── bwe_bps: min of the two peers ─────────────────────────────────────────

    #[test]
    fn effective_bwe_is_local_when_local_is_weaker() {
        let local  = full_peer(64_000,  80, 0);
        let remote = full_peer(150_000, 60, 0);
        let c = converge_summaries(&local, &remote);
        assert_eq!(c.bwe_bps, 64_000, "weaker local uplink must constrain effective BWE");
    }

    #[test]
    fn effective_bwe_is_remote_when_remote_is_weaker() {
        let local  = full_peer(400_000, 40, 0);
        let remote = full_peer(48_000,  90, 0);
        let c = converge_summaries(&local, &remote);
        assert_eq!(c.bwe_bps, 48_000, "weaker remote uplink must constrain effective BWE");
    }

    #[test]
    fn effective_bwe_tie_returns_the_same_value() {
        let local  = full_peer(100_000, 50, 0);
        let remote = full_peer(100_000, 50, 0);
        assert_eq!(converge_summaries(&local, &remote).bwe_bps, 100_000);
    }

    // ── tier: min of the two peers ────────────────────────────────────────────

    #[test]
    fn effective_tier_is_lower_of_two() {
        let local = GovernorSummary {
            tier: TierState::Full,
            bwe_bps: 400_000,
            rtt_ms: 30,
            loss_ppm: 0,
        };
        let remote = GovernorSummary {
            tier: TierState::Survival,
            bwe_bps: 64_000,
            rtt_ms: 200,
            loss_ppm: 50_000,
        };
        assert_eq!(
            converge_summaries(&local, &remote).tier,
            TierState::Survival,
            "session must not run above the weaker peer's tier"
        );
    }

    #[test]
    fn tier_constrained_local_beats_comfortable_remote() {
        let local = GovernorSummary { tier: TierState::Constrained, bwe_bps: 80_000, rtt_ms: 100, loss_ppm: 0 };
        let remote = GovernorSummary { tier: TierState::Comfortable, bwe_bps: 200_000, rtt_ms: 60, loss_ppm: 0 };
        assert_eq!(converge_summaries(&local, &remote).tier, TierState::Constrained);
    }

    #[test]
    fn tier_convergence_is_commutative() {
        let a = GovernorSummary { tier: TierState::Comfortable, bwe_bps: 150_000, rtt_ms: 80, loss_ppm: 0 };
        let b = GovernorSummary { tier: TierState::Constrained, bwe_bps: 64_000, rtt_ms: 120, loss_ppm: 0 };
        let ab = converge_summaries(&a, &b);
        let ba = converge_summaries(&b, &a);
        assert_eq!(ab.tier, ba.tier);
        assert_eq!(ab.bwe_bps, ba.bwe_bps);
    }

    // ── rtt_ms: max of the two peers ─────────────────────────────────────────

    #[test]
    fn effective_rtt_is_higher_of_two() {
        let local  = full_peer(100_000, 40,  0);
        let remote = full_peer(100_000, 120, 0);
        let c = converge_summaries(&local, &remote);
        assert_eq!(c.rtt_ms, 120, "conservative RTT must take the higher measurement");
    }

    #[test]
    fn effective_rtt_local_higher() {
        let local  = full_peer(100_000, 300, 0);
        let remote = full_peer(100_000, 50,  0);
        assert_eq!(converge_summaries(&local, &remote).rtt_ms, 300);
    }

    // ── loss_ppm: max of the two peers ────────────────────────────────────────

    #[test]
    fn effective_loss_is_worse_of_two_paths() {
        let local  = full_peer(100_000, 50, 5_000);   // 0.5 %
        let remote = full_peer(100_000, 50, 50_000);  // 5 %
        assert_eq!(
            converge_summaries(&local, &remote).loss_ppm,
            50_000,
            "effective loss must be the worst observed across both paths"
        );
    }

    #[test]
    fn effective_loss_local_worse() {
        let local  = full_peer(100_000, 50, 100_000); // 10 %
        let remote = full_peer(100_000, 50, 10_000);  // 1 %
        assert_eq!(converge_summaries(&local, &remote).loss_ppm, 100_000);
    }

    #[test]
    fn zero_loss_both_sides_stays_zero() {
        let local  = full_peer(200_000, 30, 0);
        let remote = full_peer(150_000, 40, 0);
        assert_eq!(converge_summaries(&local, &remote).loss_ppm, 0);
    }

    // ── combined scenario: asymmetric 3G vs office link ──────────────────────

    #[test]
    fn asymmetric_3g_vs_office_converges_on_3g_peer() {
        // Office technician: good uplink, low RTT, low loss.
        let office = GovernorSummary {
            tier: TierState::Full,
            bwe_bps: 400_000,
            rtt_ms: 25,
            loss_ppm: 500,   // 0.05 %
        };
        // Assisted user on 3G: tight uplink, higher RTT and loss.
        let mobile = GovernorSummary {
            tier: TierState::Constrained,
            bwe_bps: 64_000,
            rtt_ms: 180,
            loss_ppm: 30_000, // 3 %
        };

        let effective = converge_summaries(&office, &mobile);

        assert_eq!(effective.tier,     TierState::Constrained, "must converge to 3G peer's tier");
        assert_eq!(effective.bwe_bps,  64_000,                 "must use 3G uplink as budget");
        assert_eq!(effective.rtt_ms,   180,                    "must use higher RTT");
        assert_eq!(effective.loss_ppm, 30_000,                 "must use worse path loss");
    }

    // ── survival-tier corner case ─────────────────────────────────────────────

    #[test]
    fn survival_tier_remote_forces_survival_regardless_of_local() {
        for local_tier in [TierState::Full, TierState::Comfortable, TierState::Constrained] {
            let local = GovernorSummary { tier: local_tier, bwe_bps: 400_000, rtt_ms: 30, loss_ppm: 0 };
            let remote = GovernorSummary { tier: TierState::Survival, bwe_bps: 48_000, rtt_ms: 300, loss_ppm: 80_000 };
            let c = converge_summaries(&local, &remote);
            assert_eq!(
                c.tier, TierState::Survival,
                "remote at Survival must force Survival regardless of local tier ({local_tier:?})"
            );
        }
    }

    // ── commutativity: order of local/remote must not matter ─────────────────

    #[test]
    fn convergence_is_commutative_for_all_fields() {
        let a = GovernorSummary {
            tier: TierState::Comfortable,
            bwe_bps: 200_000,
            rtt_ms: 60,
            loss_ppm: 5_000,
        };
        let b = GovernorSummary {
            tier: TierState::Constrained,
            bwe_bps: 80_000,
            rtt_ms: 150,
            loss_ppm: 25_000,
        };
        let ab = converge_summaries(&a, &b);
        let ba = converge_summaries(&b, &a);
        assert_eq!(ab, ba, "convergence must be commutative — caller order must not matter");
    }
}

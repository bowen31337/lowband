//! Feature 73 — System sends a governor_summary so the session converges on
//! the weaker peer.
//!
//! # What this test verifies
//!
//! 1. **Weaker-uplink convergence** — when one peer's BWE is lower, the
//!    effective `bwe_bps` equals that peer's estimate regardless of which side
//!    is `local` vs `remote`.
//!
//! 2. **Tier convergence** — the effective tier is always `≤` both individual
//!    tiers; the weaker tier wins.
//!
//! 3. **Conservative RTT** — the effective RTT is the higher of the two
//!    measurements, preventing jitter-buffer underrun from an optimistic
//!    estimate.
//!
//! 4. **Worst-path loss** — the effective loss is the higher of the two
//!    observations, so DRED / FEC depth is sized from the *worse* path.
//!
//! 5. **Commutativity** — `converge_summaries(a, b) == converge_summaries(b, a)`
//!    so the caller does not need to track which summary is "local".
//!
//! 6. **Audio floor preserved** — the effective BWE is always fed to
//!    `allocate()`, which in turn always funds audio above `AUDIO_FLOOR_BPS`.
//!    Confirming this end-to-end ensures the convergence path cannot starve voice.
//!
//! 7. **Asymmetric 3G scenario** — a concrete end-to-end scenario where a
//!    mobile user on 3G (64 kbps, 180 ms RTT, 3 % loss) connects to an office
//!    technician (400 kbps, 25 ms RTT, 0.05 % loss).  The session must run at
//!    the mobile peer's capacity.

use lowband_platform::{
    converge_summaries, GovernorSummary,
    gear_policy::{allocate, GearConstraints, AUDIO_FLOOR_BPS},
    thermal::ThermalPressure,
    TierState,
};

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_summary(tier: TierState, bwe_bps: u32, rtt_ms: u32, loss_ppm: u32) -> GovernorSummary {
    GovernorSummary { tier, bwe_bps, rtt_ms, loss_ppm }
}

// ── 1. Weaker-uplink convergence ──────────────────────────────────────────────

#[test]
fn effective_bwe_tracks_weaker_uplink_local_constrained() {
    let local  = make_summary(TierState::Constrained, 64_000,  120, 30_000);
    let remote = make_summary(TierState::Full,        400_000, 25,  500);
    let c = converge_summaries(&local, &remote);
    assert_eq!(
        c.bwe_bps, 64_000,
        "Feature 73: effective BWE must track the weaker uplink (local = 64 kbps)"
    );
}

#[test]
fn effective_bwe_tracks_weaker_uplink_remote_constrained() {
    let local  = make_summary(TierState::Full,        400_000, 25,  500);
    let remote = make_summary(TierState::Constrained, 64_000,  120, 30_000);
    let c = converge_summaries(&local, &remote);
    assert_eq!(
        c.bwe_bps, 64_000,
        "Feature 73: effective BWE must track the weaker uplink (remote = 64 kbps)"
    );
}

#[test]
fn bwe_convergence_is_commutative() {
    let a = make_summary(TierState::Comfortable, 150_000, 80,  5_000);
    let b = make_summary(TierState::Constrained, 64_000,  120, 30_000);
    assert_eq!(
        converge_summaries(&a, &b).bwe_bps,
        converge_summaries(&b, &a).bwe_bps,
        "BWE convergence must be commutative"
    );
}

// ── 2. Tier convergence ───────────────────────────────────────────────────────

#[test]
fn effective_tier_is_lower_of_the_two() {
    let local  = make_summary(TierState::Full,     400_000, 25, 0);
    let remote = make_summary(TierState::Survival, 48_000, 300, 80_000);
    assert_eq!(
        converge_summaries(&local, &remote).tier,
        TierState::Survival,
        "Feature 73: session must not exceed the weaker peer's tier"
    );
}

#[test]
fn tier_convergence_covers_all_downgrade_paths() {
    // For every pair (a, b) of tiers where a >= b, converging must yield b.
    let tiers = [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ];
    for &stronger in &tiers {
        for &weaker in &tiers {
            if stronger >= weaker {
                let stronger_peer = make_summary(stronger, 400_000, 30, 0);
                let weaker_peer   = make_summary(weaker,   64_000,  150, 0);
                let c = converge_summaries(&stronger_peer, &weaker_peer);
                assert_eq!(
                    c.tier, weaker,
                    "convergence of {stronger:?} + {weaker:?} must yield {weaker:?}"
                );
            }
        }
    }
}

// ── 3. Conservative RTT ───────────────────────────────────────────────────────

#[test]
fn effective_rtt_is_higher_of_the_two() {
    let local  = make_summary(TierState::Full, 200_000, 40,  0);
    let remote = make_summary(TierState::Full, 200_000, 200, 0);
    assert_eq!(
        converge_summaries(&local, &remote).rtt_ms,
        200,
        "Feature 73: conservative RTT must take the higher measurement"
    );
}

#[test]
fn rtt_convergence_is_commutative() {
    let a = make_summary(TierState::Full, 200_000, 40,  0);
    let b = make_summary(TierState::Full, 200_000, 200, 0);
    assert_eq!(
        converge_summaries(&a, &b).rtt_ms,
        converge_summaries(&b, &a).rtt_ms,
        "RTT convergence must be commutative"
    );
}

// ── 4. Worst-path loss ────────────────────────────────────────────────────────

#[test]
fn effective_loss_is_worse_of_the_two_paths() {
    let local  = make_summary(TierState::Full, 200_000, 50, 5_000);   // 0.5 %
    let remote = make_summary(TierState::Full, 200_000, 50, 50_000);  // 5 %
    assert_eq!(
        converge_summaries(&local, &remote).loss_ppm,
        50_000,
        "Feature 73: loss must track the worse-path observation"
    );
}

#[test]
fn loss_convergence_is_commutative() {
    let a = make_summary(TierState::Full, 200_000, 50, 5_000);
    let b = make_summary(TierState::Full, 200_000, 50, 50_000);
    assert_eq!(
        converge_summaries(&a, &b).loss_ppm,
        converge_summaries(&b, &a).loss_ppm,
    );
}

// ── 5. Commutativity — all fields ─────────────────────────────────────────────

#[test]
fn convergence_is_fully_commutative() {
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
    assert_eq!(
        converge_summaries(&a, &b),
        converge_summaries(&b, &a),
        "Feature 73: convergence must be commutative — caller order must not matter"
    );
}

// ── 6. Audio floor preserved through the full convergence path ────────────────

#[test]
fn audio_floor_preserved_when_effective_bwe_is_64kbps() {
    let local  = make_summary(TierState::Full,        400_000, 25,  0);
    let remote = make_summary(TierState::Constrained, 64_000,  180, 30_000);
    let effective = converge_summaries(&local, &remote);

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(effective.bwe_bps, &constraints);

    assert!(
        budgets.audio_bps >= AUDIO_FLOOR_BPS,
        "Feature 73: audio floor must be ≥ {} bps even after converging on 64 kbps peer; \
         got {} bps",
        AUDIO_FLOOR_BPS,
        budgets.audio_bps,
    );
}

#[test]
fn audio_floor_preserved_when_effective_bwe_is_survival_level() {
    // 48 kbps is a typical Survival-tier budget.
    let local  = make_summary(TierState::Full,     400_000, 25, 0);
    let remote = make_summary(TierState::Survival, 48_000, 300, 80_000);
    let effective = converge_summaries(&local, &remote);

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(effective.bwe_bps, &constraints);

    assert!(
        budgets.audio_bps >= AUDIO_FLOOR_BPS,
        "audio floor must survive even when converging on a Survival-tier 48 kbps peer; \
         got {} bps",
        budgets.audio_bps,
    );
}

// ── 7. Asymmetric 3G scenario ─────────────────────────────────────────────────

#[test]
fn asymmetric_3g_vs_office_session_runs_at_mobile_capacity() {
    // Office technician on a fast broadband link.
    let office = GovernorSummary {
        tier:     TierState::Full,
        bwe_bps:  400_000,
        rtt_ms:   25,
        loss_ppm: 500,    // 0.05 %
    };
    // Assisted user on a 3G dongle.
    let mobile = GovernorSummary {
        tier:     TierState::Constrained,
        bwe_bps:  64_000,
        rtt_ms:   180,
        loss_ppm: 30_000, // 3 %
    };

    let effective = converge_summaries(&office, &mobile);

    // Session converges on the mobile user's capacity.
    assert_eq!(
        effective.tier, TierState::Constrained,
        "session tier must converge on the mobile peer's Constrained tier"
    );
    assert_eq!(
        effective.bwe_bps, 64_000,
        "session budget must converge on the mobile peer's 64 kbps uplink"
    );
    assert_eq!(
        effective.rtt_ms, 180,
        "RTT must use the mobile peer's higher measurement"
    );
    assert_eq!(
        effective.loss_ppm, 30_000,
        "loss must use the mobile peer's worse path observation (3 %)"
    );

    // Allocate at the effective budget and confirm audio is protected.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(effective.bwe_bps, &constraints);
    assert!(
        budgets.audio_bps >= AUDIO_FLOOR_BPS,
        "voice must be funded above {} bps at 64 kbps; got {}",
        AUDIO_FLOOR_BPS,
        budgets.audio_bps,
    );
    eprintln!(
        "3G scenario: tier={:?} bwe={}kbps rtt={}ms loss_ppm={} → \
         audio={}bps screen_coarse={}bps camera={}bps",
        effective.tier,
        effective.bwe_bps / 1_000,
        effective.rtt_ms,
        effective.loss_ppm,
        budgets.audio_bps,
        budgets.screen_coarse_bps,
        budgets.camera_bps,
    );
}

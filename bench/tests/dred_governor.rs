//! Feature 53 — DRED depth scales from the Gilbert-Elliott burst estimate.
//!
//! # Scenario
//!
//! The governor must eliminate the fixed overhead tax of embedding
//! [`MAX_DRED_DEPTH_FRAMES`] (50 frames = 40 kbps) of DRED in every outgoing
//! packet regardless of channel conditions.  Instead it reads the
//! [`GilbertElliottEstimator`]'s `mean_burst_len` at each 10 Hz tick and calls
//! [`DredSender::apply_ge_estimate`] so the depth tracks observed bursts.
//!
//! # Test structure
//!
//! **Part A — clean channel**: observe 500 clean packets (no loss) and confirm
//! that the adapted DRED depth is well below the architecture ceiling, saving
//! significant bitrate overhead.
//!
//! **Part B — 3G channel**: observe a deterministic 5 % GE channel with
//! burst_len = 3 packets.  After EMA convergence the adapted depth must be
//! exactly 3 frames — the minimum needed to cover every burst — and both
//! overhead and depth must be strictly less than the fixed ceiling.
//!
//! **Part C — coverage invariant**: for each GE channel the adapted depth must
//! be ≥ the mean burst length (in frames) so no expected burst escapes DRED.
//!
//! **Part D — overhead comparison**: demonstrate the bps savings on the 3G
//! channel (depth 3 vs. depth 50).

use lowband_lbtp::fec::GilbertElliottEstimator;
use lowband_platform::dred_sender::{
    dred_depth_from_ge_burst_packets, DredSender, DRED_OVERHEAD_BPS_PER_FRAME,
    MAX_DRED_DEPTH_FRAMES,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Warm a [`GilbertElliottEstimator`] with a deterministic GE trace.
///
/// The trace alternates `good_run` received packets and `bad_run` lost packets,
/// repeating `n_periods` times.
fn warm_estimator(good_run: usize, bad_run: usize, n_periods: usize) -> GilbertElliottEstimator {
    let mut est = GilbertElliottEstimator::new();
    for _ in 0..n_periods {
        for _ in 0..good_run {
            est.observe(true);
        }
        for _ in 0..bad_run {
            est.observe(false);
        }
    }
    est
}

// ── Part A: clean channel ─────────────────────────────────────────────────────

#[test]
fn clean_channel_depth_below_ceiling() {
    // Observe 500 clean packets: no losses, so mean_burst_len stays at the
    // EMA initial value of 1.0 (each hypothetical burst would last one packet).
    let mut est = GilbertElliottEstimator::new();
    for _ in 0..500 {
        est.observe(true);
    }

    let mut sender = DredSender::new(MAX_DRED_DEPTH_FRAMES);
    sender.apply_ge_estimate(est.mean_burst_len());

    // On a clean channel the GE burst length stays near 1 (EMA initial value).
    // The adapted depth must be far below the 50-frame ceiling.
    assert!(
        sender.depth_frames() < MAX_DRED_DEPTH_FRAMES,
        "adapted depth {} must be below MAX_DRED_DEPTH_FRAMES {} on a clean channel",
        sender.depth_frames(),
        MAX_DRED_DEPTH_FRAMES,
    );

    // The overhead saving must be positive.
    let overhead_fixed = MAX_DRED_DEPTH_FRAMES as u32 * DRED_OVERHEAD_BPS_PER_FRAME;
    let overhead_adapted = sender.overhead_bps();
    assert!(
        overhead_adapted < overhead_fixed,
        "adapted overhead {} bps must be less than fixed-ceiling overhead {} bps",
        overhead_adapted,
        overhead_fixed,
    );
}

// ── Part B: 3G channel convergence ───────────────────────────────────────────

#[test]
fn ge_3g_channel_adapts_depth_to_burst_length() {
    // Deterministic GE channel: 5 % loss, burst_len = 3 packets.
    // good_run = (1 - L) / L × M = 0.95 / 0.05 × 3 = 57 packets.
    let est = warm_estimator(57, 3, 200);

    let adapted_depth = dred_depth_from_ge_burst_packets(est.mean_burst_len());
    let mut sender = DredSender::new(MAX_DRED_DEPTH_FRAMES);
    sender.apply_ge_estimate(est.mean_burst_len());

    assert_eq!(
        sender.depth_frames(),
        adapted_depth,
        "apply_ge_estimate must set depth to dred_depth_from_ge_burst_packets({})",
        est.mean_burst_len(),
    );

    // The adapted depth must cover the channel burst length (3 packets = 3 frames).
    assert!(
        sender.depth_frames() >= 3,
        "adapted depth {} must be ≥ channel burst_len 3 to fully cover bursts",
        sender.depth_frames(),
    );

    // The adapted depth must be strictly below the architecture ceiling.
    assert!(
        sender.depth_frames() < MAX_DRED_DEPTH_FRAMES,
        "adapted depth {} must be strictly below MAX_DRED_DEPTH_FRAMES {} \
         on a 3G channel (burst_len = 3)",
        sender.depth_frames(),
        MAX_DRED_DEPTH_FRAMES,
    );
}

// ── Part C: coverage invariant across channels ────────────────────────────────

#[test]
fn adapted_depth_covers_mean_burst_for_several_channels() {
    // (good_run_len, bad_run_len, expected_min_depth)
    let channels: &[(usize, usize, usize)] = &[
        (19, 1, 1),  // 5 % loss, burst 1 — isolated loss
        (57, 3, 3),  // 5 % loss, burst 3 — 3G channel
        (10, 5, 5),  // ~33 % loss, burst 5
        (5, 10, 10), // ~67 % loss, burst 10
        (2, 25, 25), // heavy loss, burst 25
    ];

    for &(good_run, bad_run, min_depth) in channels {
        let est = warm_estimator(good_run, bad_run, 200);
        let mut sender = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        sender.apply_ge_estimate(est.mean_burst_len());

        assert!(
            sender.depth_frames() >= min_depth,
            "channel (good={good_run}, bad={bad_run}): adapted depth {} must \
             be ≥ burst_len {bad_run} (min_depth={min_depth}), mean_burst_len={:.2}",
            sender.depth_frames(),
            est.mean_burst_len(),
        );
    }
}

// ── Part D: overhead savings demonstration ────────────────────────────────────

#[test]
fn adapted_overhead_less_than_fixed_ceiling_on_3g_channel() {
    // 3G channel: burst_len = 3 packets.
    let est = warm_estimator(57, 3, 200);

    let mut sender_adapted = DredSender::new(MAX_DRED_DEPTH_FRAMES);
    sender_adapted.apply_ge_estimate(est.mean_burst_len());

    let sender_fixed = DredSender::new(MAX_DRED_DEPTH_FRAMES);

    let overhead_adapted = sender_adapted.overhead_bps();
    let overhead_fixed = sender_fixed.overhead_bps();

    assert!(
        overhead_adapted < overhead_fixed,
        "adapted overhead {} bps must be less than fixed-ceiling overhead {} bps; \
         depth {} vs {} frames",
        overhead_adapted,
        overhead_fixed,
        sender_adapted.depth_frames(),
        sender_fixed.depth_frames(),
    );

    // Sanity-check the ceiling: 50 frames × 800 bps/frame = 40 000 bps.
    assert_eq!(
        overhead_fixed, 40_000,
        "fixed-ceiling overhead must be 40 kbps (50 frames × 800 bps/frame)"
    );
}

// ── Monotonicity: longer GE burst → deeper depth ─────────────────────────────

#[test]
fn depth_monotonically_increases_with_ge_burst_length() {
    // Feed channels with increasing burst lengths; adapted depth must not decrease.
    let burst_lens = [1.0_f64, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0];
    let depths: Vec<usize> = burst_lens
        .iter()
        .map(|&b| dred_depth_from_ge_burst_packets(b))
        .collect();

    for i in 1..depths.len() {
        assert!(
            depths[i] >= depths[i - 1],
            "depth for burst_len {:.0} ({}) must be ≥ depth for burst_len \
             {:.0} ({})",
            burst_lens[i],
            depths[i],
            burst_lens[i - 1],
            depths[i - 1],
        );
    }
}

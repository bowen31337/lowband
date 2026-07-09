//! Feature 49 — 60 ms audio frames at Survival tier to cut packet_count.
//!
//! # Scenario
//!
//! At Survival tier the link is so constrained that per-packet protocol headers
//! (IP + UDP + LBTP ≈ 40 bytes) consume a large fraction of each datagram.
//! Switching from the standard 20 ms Opus frame to the maximum 60 ms frame
//! triples the codec payload per packet, cutting the header-overhead fraction
//! and reducing the total packet rate from 50 to 16 packets/s.
//!
//! At tiers above Survival (Constrained, Comfortable, Full) the 40 ms
//! additional packetisation latency is undesirable and unnecessary — those
//! tiers retain the standard 20 ms framing.
//!
//! # Test structure
//!
//! **Part A — tier selection**: Survival tier selects 60 ms; every other tier
//! selects 20 ms.
//!
//! **Part B — packet count reduction**: the packet rate at 60 ms is at most
//! one-third the rate at 20 ms, matching the 3× frame-duration ratio.
//!
//! **Part C — header overhead improvement**: at the Survival-tier voice rate
//! (9 kbps), the 60 ms frame's header-overhead fraction is measurably lower
//! than the 20 ms frame's, confirming the bandwidth-efficiency argument.
//!
//! **Part D — monotone with link quality**: as tier improves (Survival →
//! Constrained → … → Full), the frame duration must not increase — a
//! longer frame is the *degraded* setting, reserved for the worst link.

use lowband_platform::{
    frame_duration_ms_from_tier, header_overhead_fraction, packets_per_second,
    DEFAULT_FRAME_MS, SURVIVAL_FRAME_MS,
};
use lowband_platform::TierState;

/// IP (20) + UDP (8) + LBTP (12) header bytes per packet.
const HEADER_BYTES: u32 = 40;

/// Nominal Opus SILK-WB bitrate used at Survival tier (bps).
/// Architecture §8.1 fallback (no NPU): SILK-WB at 9–12 kbps.
const SURVIVAL_VOICE_BPS: u32 = 9_000;

// ── Part A: tier → frame duration ────────────────────────────────────────────

#[test]
fn survival_tier_selects_60ms_frame() {
    let ms = frame_duration_ms_from_tier(TierState::Survival);
    assert_eq!(
        ms, SURVIVAL_FRAME_MS,
        "Survival tier must select {SURVIVAL_FRAME_MS} ms frames to cut packet_count; got {ms} ms"
    );
}

#[test]
fn constrained_tier_selects_20ms_frame() {
    let ms = frame_duration_ms_from_tier(TierState::Constrained);
    assert_eq!(
        ms, DEFAULT_FRAME_MS,
        "Constrained tier must use the standard {DEFAULT_FRAME_MS} ms frame; got {ms} ms"
    );
}

#[test]
fn comfortable_tier_selects_20ms_frame() {
    let ms = frame_duration_ms_from_tier(TierState::Comfortable);
    assert_eq!(
        ms, DEFAULT_FRAME_MS,
        "Comfortable tier must use {DEFAULT_FRAME_MS} ms frames; got {ms} ms"
    );
}

#[test]
fn full_tier_selects_20ms_frame() {
    let ms = frame_duration_ms_from_tier(TierState::Full);
    assert_eq!(
        ms, DEFAULT_FRAME_MS,
        "Full tier must use {DEFAULT_FRAME_MS} ms frames; got {ms} ms"
    );
}

// ── Part B: packet count reduction ───────────────────────────────────────────

#[test]
fn survival_frame_yields_16_packets_per_second() {
    let pps = packets_per_second(SURVIVAL_FRAME_MS);
    assert_eq!(
        pps, 16,
        "60 ms frame must produce 16 packets/s (floor(1000/60)); got {pps}"
    );
}

#[test]
fn default_frame_yields_50_packets_per_second() {
    let pps = packets_per_second(DEFAULT_FRAME_MS);
    assert_eq!(
        pps, 50,
        "20 ms frame must produce 50 packets/s; got {pps}"
    );
}

#[test]
fn packet_count_at_survival_tier_is_at_most_one_third_of_default() {
    let pps_survival = packets_per_second(frame_duration_ms_from_tier(TierState::Survival));
    let pps_default  = packets_per_second(frame_duration_ms_from_tier(TierState::Constrained));
    assert!(
        pps_survival * 3 <= pps_default,
        "Survival-tier packet rate ({pps_survival} pps) must be ≤ ⅓ of default \
         ({pps_default} pps at Constrained)"
    );

    eprintln!(
        "audio_60ms_frames — survival_pps={pps_survival}  default_pps={pps_default}  \
         reduction_ratio={:.2}×",
        pps_default as f64 / pps_survival as f64
    );
}

// ── Part C: header overhead improvement ──────────────────────────────────────

#[test]
fn header_overhead_lower_at_60ms_than_20ms_at_survival_voice_rate() {
    let overhead_60 = header_overhead_fraction(60, SURVIVAL_VOICE_BPS, HEADER_BYTES);
    let overhead_20 = header_overhead_fraction(20, SURVIVAL_VOICE_BPS, HEADER_BYTES);

    assert!(
        overhead_60 < overhead_20,
        "60 ms frame must have lower header overhead than 20 ms at {SURVIVAL_VOICE_BPS} bps: \
         overhead_60={overhead_60:.3} overhead_20={overhead_20:.3}"
    );

    eprintln!(
        "audio_60ms_frames — header_overhead: 20ms={:.1}%  60ms={:.1}%  \
         improvement={:.1} pp",
        overhead_20 * 100.0,
        overhead_60 * 100.0,
        (overhead_20 - overhead_60) * 100.0,
    );
}

#[test]
fn header_overhead_fraction_is_sub_unity_for_realistic_inputs() {
    for &tier in &[TierState::Survival, TierState::Constrained] {
        let frame_ms = frame_duration_ms_from_tier(tier);
        let f = header_overhead_fraction(frame_ms, SURVIVAL_VOICE_BPS, HEADER_BYTES);
        assert!(
            f > 0.0 && f < 1.0,
            "overhead fraction must be in (0,1) at {tier:?}: got {f}"
        );
    }
}

// ── Part D: frame duration monotone with link quality ─────────────────────────

#[test]
fn frame_duration_does_not_increase_as_tier_improves() {
    // As tier improves (Survival < Constrained < Comfortable < Full), the
    // frame duration must not grow — a longer frame is a degraded setting.
    let tiers_ascending = [
        TierState::Survival,
        TierState::Constrained,
        TierState::Comfortable,
        TierState::Full,
    ];
    let durations: Vec<u32> = tiers_ascending
        .iter()
        .map(|&t| frame_duration_ms_from_tier(t))
        .collect();

    for i in 1..durations.len() {
        assert!(
            durations[i] <= durations[i - 1],
            "frame duration must not increase as tier improves: \
             {:?}={} ms → {:?}={} ms",
            tiers_ascending[i - 1], durations[i - 1],
            tiers_ascending[i],     durations[i]
        );
    }
}

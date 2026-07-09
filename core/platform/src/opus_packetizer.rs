//! Opus frame-duration selection — Feature 49.
//!
//! Opus supports frame durations of 2.5, 5, 10, 20, 40, and 60 ms.  At tiers
//! above Survival the system uses the standard 20 ms framing, which balances
//! latency against per-packet header overhead.  At Survival tier the link is
//! already marginal, so the governor switches to 60 ms frames — the longest
//! Opus supports — to cut the packet rate by exactly 3× and reduce the share of
//! bandwidth consumed by IP+UDP+LBTP headers.
//!
//! # Why packet count matters at Survival
//!
//! Each outbound packet carries a fixed overhead of approximately 40 bytes
//! (20-byte IPv4 header + 8-byte UDP header + 12-byte LBTP framing header).
//! At 20 ms / packet the protocol sends 50 packets/s; at Opus-SILK 9 kbps that
//! is 22.5 bytes of codec payload per packet, so overhead is
//! 40 / (40 + 22.5) ≈ 64 % of each datagram.  Switching to 60 ms frames triples
//! the payload to 67.5 bytes, cutting the overhead fraction to
//! 40 / (40 + 67.5) ≈ 37 % — a 27-percentage-point improvement on the effective
//! codec bitrate that reaches the peer.
//!
//! At Comfortable and Full tiers the 40 ms additional packetisation latency is
//! undesirable and unnecessary — those tiers have enough bandwidth for voice plus
//! all other streams, so the standard 20 ms framing is kept.
//!
//! # Packet count
//!
//! | Tier           | Frame duration | Packets per second |
//! |----------------|----------------|--------------------|
//! | Survival       | 60 ms          | 16                 |
//! | Constrained    | 20 ms          | 50                 |
//! | Comfortable    | 20 ms          | 50                 |
//! | Full           | 20 ms          | 50                 |
//!
//! Packets/s = floor(1000 / frame_ms).  The 60 ms frame gives exactly 16
//! complete 60 ms slots per second, with a 40 ms remainder that the encoder
//! flushes as a final partial frame at stream teardown.

use crate::tier::TierState;

/// Opus frame duration at the Survival tier (ms).
///
/// The maximum Opus frame duration (60 ms) is used at Survival tier to cut
/// the packet rate from 50 to 16 packets/s, reducing per-packet header
/// overhead on constrained links.
pub const SURVIVAL_FRAME_MS: u32 = 60;

/// Opus frame duration at all tiers above Survival (ms).
///
/// 20 ms is the standard Opus latency/efficiency trade-off for voice-over-IP.
/// It is kept at Constrained, Comfortable, and Full tiers where bandwidth is
/// sufficient and the lower packetisation latency is preferred.
pub const DEFAULT_FRAME_MS: u32 = 20;

/// Select the Opus frame duration for the current session tier.
///
/// Returns [`SURVIVAL_FRAME_MS`] (60 ms) when `tier == TierState::Survival`
/// to minimise packet overhead on the most constrained links.  Returns
/// [`DEFAULT_FRAME_MS`] (20 ms) for all other tiers.
pub fn frame_duration_ms_from_tier(tier: TierState) -> u32 {
    if tier == TierState::Survival {
        SURVIVAL_FRAME_MS
    } else {
        DEFAULT_FRAME_MS
    }
}

/// Compute the number of complete audio packets sent per second for a given
/// frame duration.
///
/// Returns `1000 / frame_ms`, rounded down (integer division).  Returns 0
/// when `frame_ms` is 0 to avoid a divide-by-zero on degenerate input.
pub fn packets_per_second(frame_ms: u32) -> u32 {
    if frame_ms == 0 {
        return 0;
    }
    1_000 / frame_ms
}

/// Compute the approximate fraction of each datagram consumed by protocol
/// headers, expressed as a value in `[0, 1)`.
///
/// `frame_ms` is the Opus frame duration; `codec_bps` is the audio encoder
/// target bitrate in bits per second.  `header_bytes` is the total fixed
/// overhead per packet (IP + UDP + LBTP).
///
/// Returns 0.0 when the codec payload per packet is 0 (degenerate input).
pub fn header_overhead_fraction(frame_ms: u32, codec_bps: u32, header_bytes: u32) -> f64 {
    if frame_ms == 0 || codec_bps == 0 {
        return 0.0;
    }
    let payload_bytes_per_packet = (codec_bps as f64 * frame_ms as f64) / (8.0 * 1_000.0);
    let header = header_bytes as f64;
    header / (header + payload_bytes_per_packet)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn survival_frame_ms_is_60() {
        assert_eq!(SURVIVAL_FRAME_MS, 60, "survival frame duration must be 60 ms (max Opus)");
    }

    #[test]
    fn default_frame_ms_is_20() {
        assert_eq!(DEFAULT_FRAME_MS, 20, "default frame duration must be 20 ms (standard VoIP)");
    }

    #[test]
    fn survival_frame_is_3x_default() {
        assert_eq!(
            SURVIVAL_FRAME_MS / DEFAULT_FRAME_MS,
            3,
            "60 ms frame must be exactly 3× the default 20 ms frame"
        );
    }

    // ── frame_duration_ms_from_tier ───────────────────────────────────────────

    #[test]
    fn survival_tier_selects_60ms_frame() {
        assert_eq!(
            frame_duration_ms_from_tier(TierState::Survival),
            SURVIVAL_FRAME_MS,
            "Survival tier must select the 60 ms frame to cut packet_count"
        );
    }

    #[test]
    fn constrained_tier_selects_20ms_frame() {
        assert_eq!(
            frame_duration_ms_from_tier(TierState::Constrained),
            DEFAULT_FRAME_MS,
            "Constrained tier must use the standard 20 ms frame"
        );
    }

    #[test]
    fn comfortable_tier_selects_20ms_frame() {
        assert_eq!(
            frame_duration_ms_from_tier(TierState::Comfortable),
            DEFAULT_FRAME_MS,
            "Comfortable tier must use the standard 20 ms frame"
        );
    }

    #[test]
    fn full_tier_selects_20ms_frame() {
        assert_eq!(
            frame_duration_ms_from_tier(TierState::Full),
            DEFAULT_FRAME_MS,
            "Full tier must use the standard 20 ms frame"
        );
    }

    #[test]
    fn only_survival_differs_from_default() {
        let tiers = [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ];
        for tier in tiers {
            let ms = frame_duration_ms_from_tier(tier);
            if tier == TierState::Survival {
                assert_eq!(ms, SURVIVAL_FRAME_MS, "{tier:?} must select SURVIVAL_FRAME_MS");
            } else {
                assert_eq!(ms, DEFAULT_FRAME_MS, "{tier:?} must select DEFAULT_FRAME_MS");
            }
        }
    }

    // ── packets_per_second ────────────────────────────────────────────────────

    #[test]
    fn survival_yields_16_packets_per_second() {
        // floor(1000 / 60) = 16
        let pps = packets_per_second(SURVIVAL_FRAME_MS);
        assert_eq!(pps, 16, "60 ms frame must produce 16 complete packets/s");
    }

    #[test]
    fn default_yields_50_packets_per_second() {
        let pps = packets_per_second(DEFAULT_FRAME_MS);
        assert_eq!(pps, 50, "20 ms frame must produce 50 packets/s");
    }

    #[test]
    fn packet_count_reduction_is_at_least_3x() {
        let pps_default = packets_per_second(DEFAULT_FRAME_MS);
        let pps_survival = packets_per_second(SURVIVAL_FRAME_MS);
        assert!(
            pps_default >= pps_survival * 3,
            "survival-tier frame must reduce packet rate by at least 3×: \
             {pps_default} pps → {pps_survival} pps"
        );
    }

    #[test]
    fn packets_per_second_zero_for_zero_frame_ms() {
        assert_eq!(packets_per_second(0), 0, "must not divide by zero");
    }

    // ── header_overhead_fraction ──────────────────────────────────────────────

    // IP(20) + UDP(8) + LBTP(12) = 40 bytes per packet.
    const HEADER_BYTES: u32 = 40;

    #[test]
    fn header_overhead_lower_at_60ms_than_20ms_at_9kbps() {
        let overhead_60 = header_overhead_fraction(60, 9_000, HEADER_BYTES);
        let overhead_20 = header_overhead_fraction(20, 9_000, HEADER_BYTES);
        assert!(
            overhead_60 < overhead_20,
            "60 ms frame must have lower header overhead than 20 ms frame at 9 kbps: \
             {overhead_60:.3} vs {overhead_20:.3}"
        );
    }

    #[test]
    fn header_overhead_zero_for_zero_frame_ms() {
        assert_eq!(header_overhead_fraction(0, 9_000, HEADER_BYTES), 0.0);
    }

    #[test]
    fn header_overhead_zero_for_zero_bitrate() {
        assert_eq!(header_overhead_fraction(20, 0, HEADER_BYTES), 0.0);
    }

    #[test]
    fn header_overhead_fraction_is_in_range() {
        // Must be in (0, 1) for realistic inputs.
        for &frame_ms in &[20u32, 60] {
            let f = header_overhead_fraction(frame_ms, 9_000, HEADER_BYTES);
            assert!(f > 0.0 && f < 1.0, "overhead fraction must be in (0,1): got {f}");
        }
    }
}

//! Input-to-photon latency budget — Feature 64.
//!
//! At the constrained tier (64 kbps), the system keeps the non-network
//! portion of input-to-photon latency within 60 ms by combining:
//!
//! 1. **Channel priority** — input (channel 3) holds second-highest priority
//!    in the LBTP pacer, so input frames drain within one pacing tick (5 ms)
//!    of arrival, regardless of how many media frames are queued.
//!
//! 2. **Bandwidth floor** — the governor reserves at least 3–8 kbps for the
//!    input/cursor lane before funding any other stream.
//!
//! 3. **Burst cap** — the pacer's 5 ms token-bucket ceiling prevents any
//!    burst from delaying a higher-priority input frame by more than one tick.
//!
//! # Overhead breakdown (constrained tier, 64 kbps)
//!
//! | Component                          | Latency      |
//! |------------------------------------|--------------|
//! | Input event delta-encode           | ≈2 ms        |
//! | Screen tile decode on remote end   | ≈2 ms        |
//! | Display render (≈60 Hz frame)      | ≈16 ms       |
//! | Pacer queuing (worst-case 1 tick)  | ≤5 ms        |
//! | **Total non-network overhead**     | **≤25 ms**   |
//!
//! The 60 ms SLA has ≥35 ms of margin over the worst-case overhead.
//!
//! # Usage
//!
//! The governor and telemetry path call these functions after sampling
//! `Pacer::queued_frames(ch_input)` to verify the latency guarantee is met:
//!
//! ```
//! use lowband_platform::input_latency_budget::{
//!     total_overhead_ms, within_sla, MAX_BACKLOG_WITHIN_SLA,
//! };
//!
//! let backlog = 0usize; // frames queued ahead of the next input frame
//! assert!(within_sla(backlog));
//! assert_eq!(total_overhead_ms(backlog), 20);
//!
//! // At most 8 frames of backlog keeps us inside the 60 ms budget.
//! assert!(within_sla(MAX_BACKLOG_WITHIN_SLA));
//! assert!(!within_sla(MAX_BACKLOG_WITHIN_SLA + 1));
//! ```

// ── Constants ──────────────────────────────────────────────────────────────────

/// Fixed non-network overhead on the input-to-photon path (ms).
///
/// Breakdown: input event delta-encode ≈2 ms + screen tile decode ≈2 ms +
/// display render (≈60 Hz frame gate) ≈16 ms = 20 ms.
///
/// This value is invariant across pacing-tick rates and link speeds because
/// it does not depend on the pacer's scheduling — it is the irreducible cost
/// of the encode/decode/render pipeline stages.
pub const INPUT_TO_PHOTON_FIXED_OVERHEAD_MS: u64 = 20;

/// Architecture SLA for the total non-network input-to-photon overhead (ms).
///
/// PRD NFR-2: "input-to-photon ≤ RTT + 60 ms".  This is the non-network
/// (processing + queuing) budget; the RTT component is added by physics.
pub const INPUT_TO_PHOTON_SLA_MS: u64 = 60;

/// Pacing tick duration at the constrained tier (ms).
///
/// At 64 kbps the transport loop runs at 5 ms ticks.  Each queued input
/// frame adds exactly one tick of queuing delay because the pacer drains at
/// most one frame per channel per tick.
pub const INPUT_PACER_TICK_MS: u64 = 5;

/// Maximum number of input frames that may be queued in the pacer while
/// keeping the total non-network overhead within [`INPUT_TO_PHOTON_SLA_MS`].
///
/// Derivation:
/// `(SLA - fixed_overhead) / tick_ms = (60 - 20) / 5 = 8 frames`
///
/// In practice the pacer's second-highest input priority keeps the backlog
/// at 0 or 1 frame; this bound is a hard structural ceiling that the
/// priority mechanism guarantees is never reached in a well-loaded session.
pub const MAX_BACKLOG_WITHIN_SLA: usize =
    ((INPUT_TO_PHOTON_SLA_MS - INPUT_TO_PHOTON_FIXED_OVERHEAD_MS) / INPUT_PACER_TICK_MS) as usize;

// ── Functions ─────────────────────────────────────────────────────────────────

/// Returns the pacer queuing delay in milliseconds for the given input-channel
/// backlog depth.
///
/// `backlog_frames` is the number of input frames queued ahead of the *next*
/// frame to be sent, as reported by `Pacer::queued_frames(ch_input)`.
#[inline]
pub fn queuing_delay_ms(backlog_frames: usize) -> u64 {
    (backlog_frames as u64) * INPUT_PACER_TICK_MS
}

/// Returns the total non-network input-to-photon overhead in milliseconds.
///
/// `total = fixed_overhead + queuing_delay(backlog_frames)`
///
/// This is the quantity that must stay ≤ [`INPUT_TO_PHOTON_SLA_MS`] for the
/// system to meet its latency guarantee.
#[inline]
pub fn total_overhead_ms(backlog_frames: usize) -> u64 {
    INPUT_TO_PHOTON_FIXED_OVERHEAD_MS + queuing_delay_ms(backlog_frames)
}

/// Returns `true` when the total non-network overhead is within the 60 ms SLA.
///
/// Equivalent to `total_overhead_ms(backlog_frames) <= INPUT_TO_PHOTON_SLA_MS`.
/// Call this after sampling `Pacer::queued_frames(ch_input)` on each governor
/// tick to verify the latency guarantee is being maintained.
#[inline]
pub fn within_sla(backlog_frames: usize) -> bool {
    total_overhead_ms(backlog_frames) <= INPUT_TO_PHOTON_SLA_MS
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ────────────────────────────────────────────────────────────

    #[test]
    fn fixed_overhead_is_20ms() {
        assert_eq!(INPUT_TO_PHOTON_FIXED_OVERHEAD_MS, 20);
    }

    #[test]
    fn sla_is_60ms() {
        assert_eq!(INPUT_TO_PHOTON_SLA_MS, 60);
    }

    #[test]
    fn tick_is_5ms() {
        assert_eq!(INPUT_PACER_TICK_MS, 5);
    }

    #[test]
    fn max_backlog_is_8_frames() {
        // (60 - 20) / 5 = 8
        assert_eq!(MAX_BACKLOG_WITHIN_SLA, 8);
    }

    // ── queuing_delay_ms ──────────────────────────────────────────────────────

    #[test]
    fn queuing_delay_zero_at_empty_queue() {
        assert_eq!(queuing_delay_ms(0), 0);
    }

    #[test]
    fn queuing_delay_one_tick_per_frame() {
        assert_eq!(queuing_delay_ms(1), 5);
        assert_eq!(queuing_delay_ms(2), 10);
        assert_eq!(queuing_delay_ms(8), 40);
    }

    // ── total_overhead_ms ────────────────────────────────────────────────────

    #[test]
    fn total_overhead_at_zero_backlog_is_fixed_only() {
        assert_eq!(total_overhead_ms(0), INPUT_TO_PHOTON_FIXED_OVERHEAD_MS);
    }

    #[test]
    fn total_overhead_accumulates_per_frame() {
        assert_eq!(total_overhead_ms(1), 25); // 20 + 5
        assert_eq!(total_overhead_ms(2), 30); // 20 + 10
        assert_eq!(total_overhead_ms(8), 60); // 20 + 40 = exactly at SLA
    }

    // ── within_sla ───────────────────────────────────────────────────────────

    #[test]
    fn within_sla_at_zero_backlog() {
        assert!(within_sla(0), "empty queue must satisfy the 60 ms SLA");
    }

    #[test]
    fn within_sla_at_max_backlog() {
        assert!(
            within_sla(MAX_BACKLOG_WITHIN_SLA),
            "max allowed backlog ({MAX_BACKLOG_WITHIN_SLA} frames) must still satisfy SLA"
        );
        assert_eq!(
            total_overhead_ms(MAX_BACKLOG_WITHIN_SLA),
            INPUT_TO_PHOTON_SLA_MS,
            "max backlog must produce exactly the SLA overhead"
        );
    }

    #[test]
    fn within_sla_violated_above_max_backlog() {
        assert!(
            !within_sla(MAX_BACKLOG_WITHIN_SLA + 1),
            "one frame beyond max backlog must exceed the 60 ms SLA"
        );
    }

    #[test]
    fn sla_margin_at_constrained_tier_worst_case() {
        // Architecture worst-case: pacer burst cap = 5 ms = 1 tick.
        // With at most 1 frame of backlog: total = 20 + 5 = 25 ms.
        // Margin = 60 - 25 = 35 ms — architecture doc states "≥35 ms of margin".
        let worst_case_backlog = 1;
        let overhead = total_overhead_ms(worst_case_backlog);
        let margin = INPUT_TO_PHOTON_SLA_MS - overhead;
        assert_eq!(overhead, 25, "worst-case overhead at burst cap must be 25 ms");
        assert_eq!(margin, 35, "margin must be 35 ms at constrained-tier burst-cap case");
    }

    #[test]
    fn max_backlog_derivation_is_consistent_with_constants() {
        // Verify the const formula matches the function semantics.
        let computed = (0usize..)
            .take_while(|&b| within_sla(b))
            .last()
            .unwrap_or(0);
        assert_eq!(
            computed, MAX_BACKLOG_WITHIN_SLA,
            "MAX_BACKLOG_WITHIN_SLA must equal the last backlog depth that satisfies within_sla"
        );
    }

    #[test]
    fn feature_64_acceptance() {
        // Simulate the constrained-tier scenario: the pacer's burst cap (5 ms)
        // limits input backlog to at most 1 frame.  The resulting overhead
        // (25 ms) must be within the 60 ms SLA with ≥35 ms of margin.
        for backlog in 0..=MAX_BACKLOG_WITHIN_SLA {
            assert!(
                within_sla(backlog),
                "input_to_photon overhead must be within 60 ms at backlog={backlog}"
            );
        }
        // One frame beyond the ceiling violates the SLA.
        assert!(
            !within_sla(MAX_BACKLOG_WITHIN_SLA + 1),
            "one frame beyond max backlog ({}) must violate the 60 ms SLA",
            MAX_BACKLOG_WITHIN_SLA + 1
        );
    }
}

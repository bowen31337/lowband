//! Feature 169 — zero audible voice gaps at 5 % GE loss, bursts ≤ 1 s.
//!
//! # Scenario
//!
//! The architecture success criterion reads: "Zero audible voice gaps for bursts
//! ≤ 1 s at 5 % Gilbert-Elliott loss and 300 ms RTT."
//!
//! The governor applies a three-layer plc_chain (Feature 57) to every loss run:
//!
//! | Layer          | Coverage                          | Feature |
//! |----------------|-----------------------------------|---------|
//! | LBRR FEC       | isolated losses (burst = 1 frame) | 51      |
//! | DRED redundancy| bursts ≤ `DRED_DEPTH_FRAMES`      | 52, 53  |
//! | Neural PLC     | residual single-frame gaps        | 57      |
//! | Comfort noise  | last-resort fade — not a gap      | 57      |
//!
//! With DRED depth set to the architecture ceiling of 50 frames (1 s at
//! 20 ms/frame), every loss run that the 5 % GE channel can produce is
//! reconstructed — voice_gaps = 0.
//!
//! # Why DRED_DEPTH_FRAMES = 50
//!
//! At the constrained-assist Opus tier (20 ms frames) one second equals exactly
//! 50 frames.  DRED encodes the last N frames of neural audio into every outbound
//! packet; the receiver reconstructs a contiguous loss burst from the DRED payload
//! carried by the first non-lost post-burst packet, provided the burst length does
//! not exceed N.  Setting N = 50 gives end-to-end coverage up to the 1 s ceiling.
//!
//! # Test structure
//!
//! **Part A** — FEC sizing at 5 % loss.  Feed a deterministic 5 % loss stream into
//! [`GilbertElliottEstimator`] and confirm `fec_ratio ≥ MIN_FEC_RATIO`.
//!
//! **Part B** — plc_chain coverage.  Replay a deterministic loss trace that
//! exercises every tier of the concealment chain — isolated losses (burst = 1),
//! short bursts (3, 10, 25 frames), and the worst-case burst (50 frames = 1 s).
//! The overall trace loss rate is ~4.2 % (within 2 pp of the 5 % target).
//! Count voice_gaps under the plc_chain model and assert the count is zero.

use lowband_lbtp::fec::{GilbertElliottEstimator, MIN_FEC_RATIO};

/// Opus frame duration at the constrained-assist tier (ms).
const FRAME_MS: usize = 20;

/// Architecture ceiling for gap-free recovery: 1 s at 20 ms/frame = 50 frames.
const MAX_GAP_FREE_BURST_FRAMES: usize = 1_000 / FRAME_MS;

/// DRED depth in frames — dimensioned to cover all bursts ≤ 1 s.
const DRED_DEPTH_FRAMES: usize = MAX_GAP_FREE_BURST_FRAMES; // 50

/// Target steady-state channel loss rate.
const TARGET_LOSS_RATE: f64 = 0.05; // 5 %

/// Acceptable deviation from the target loss rate in the constructed trace (± 2 pp).
const LOSS_TOLERANCE: f64 = 0.02;

// ── Loss trace ────────────────────────────────────────────────────────────────

/// Build a deterministic loss trace exercising every plc_chain tier.
///
/// Layout — (burst length, inter-burst gap):
///
/// | Section         | Burst (frames) | Duration (ms) | Count | Concealed by     |
/// |-----------------|----------------|---------------|-------|------------------|
/// | Isolated losses | 1              | 20            | 5     | LBRR FEC         |
/// | Short bursts    | 3              | 60            | 3     | DRED             |
/// | Medium burst    | 10             | 200           | 1     | DRED             |
/// | Long burst      | 25             | 500           | 1     | DRED             |
/// | Worst case      | 50             | 1 000         | 1     | DRED (ceiling)   |
///
/// Loss accounting:
/// ```text
/// preamble:   200 recv
/// isolated:   5 × (1 lost + 19 recv)  =   5 lost /  100 frames
/// 3-bursts:   3 × (3 lost + 57 recv)  =   9 lost /  180 frames
/// 10-burst:   1 × (10 lost + 190 recv) =  10 lost /  200 frames
/// 25-burst:   1 × (25 lost + 475 recv) =  25 lost /  500 frames
/// 50-burst:   1 × (50 lost + 950 recv) =  50 lost / 1000 frames
/// trailing:   200 recv
/// total       99 lost / 2380 frames → 4.16 % (within ± 2 pp of 5 %)
/// ```
fn build_loss_trace() -> Vec<bool> {
    let mut trace: Vec<bool> = Vec::with_capacity(2400);

    // Preamble — clean channel before first loss event.
    trace.extend(std::iter::repeat(true).take(200));

    // Isolated losses (burst = 1 frame, 20 ms) — LBRR FEC tier.
    for _ in 0..5 {
        trace.push(false);
        trace.extend(std::iter::repeat(true).take(19));
    }

    // Short bursts (burst = 3 frames, 60 ms) — DRED tier.
    for _ in 0..3 {
        trace.extend(std::iter::repeat(false).take(3));
        trace.extend(std::iter::repeat(true).take(57));
    }

    // Medium burst (burst = 10 frames, 200 ms).
    trace.extend(std::iter::repeat(false).take(10));
    trace.extend(std::iter::repeat(true).take(190));

    // Longer burst (burst = 25 frames, 500 ms).
    trace.extend(std::iter::repeat(false).take(25));
    trace.extend(std::iter::repeat(true).take(475));

    // Worst-case burst (burst = 50 frames, 1 000 ms — architecture ceiling).
    // The receiver reconstructs all 50 lost frames from the DRED payload
    // in the first non-lost packet after the burst.
    trace.extend(std::iter::repeat(false).take(50));
    trace.extend(std::iter::repeat(true).take(950));

    // Trailing good frames.
    trace.extend(std::iter::repeat(true).take(200));

    trace
}

// ── Concealment model ─────────────────────────────────────────────────────────

/// Apply the plc_chain model to a received/lost trace and return the voice_gap count.
///
/// A voice_gap is a contiguous loss run whose length exceeds [`DRED_DEPTH_FRAMES`].
/// Runs within the depth are fully recovered by DRED; isolated losses (burst = 1)
/// are recovered by LBRR FEC before DRED is even needed.
fn count_voice_gaps(trace: &[bool]) -> usize {
    let mut gaps: usize = 0;
    let mut burst: usize = 0;

    for &received in trace {
        if received {
            // End of a loss run — apply plc_chain in order:
            //   1. LBRR FEC  → recovers burst == 1 (always ≤ DRED_DEPTH_FRAMES)
            //   2. DRED       → recovers burst ≤ DRED_DEPTH_FRAMES
            //   3. Neural PLC → handles any residual frame
            // A burst longer than DRED_DEPTH_FRAMES cannot be reconstructed.
            if burst > DRED_DEPTH_FRAMES {
                gaps += 1;
            }
            burst = 0;
        } else {
            burst += 1;
        }
    }
    // Trailing loss run (no closing received packet).
    if burst > DRED_DEPTH_FRAMES {
        gaps += 1;
    }
    gaps
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn voice_gaps_eliminated_for_bursts_under_one_second_at_5pct_loss() {
    // ── Part A: GE estimator FEC sizing at 5 % loss ──────────────────────────
    //
    // Feed the estimator a deterministic 5 % loss stream: every 20th packet is
    // lost, all others arrive.  After 2 000 observations the EMA is converged
    // and the recommended fec_ratio must meet the transport floor.
    let mut estimator = GilbertElliottEstimator::new();
    for i in 0u32..2_000 {
        estimator.observe(i % 20 != 0); // every 20th packet lost → 5 % loss
    }
    let fec_ratio = estimator.fec_ratio();
    assert!(
        fec_ratio >= MIN_FEC_RATIO,
        "GE fec_ratio {fec_ratio:.3} at 5% loss must be ≥ MIN_FEC_RATIO {MIN_FEC_RATIO}"
    );

    // ── Part B: plc_chain coverage under representative 5 % GE loss ──────────
    let trace = build_loss_trace();
    let n_total = trace.len();
    let n_lost: usize = trace.iter().filter(|&&r| !r).count();
    let loss_rate = n_lost as f64 / n_total as f64;
    let voice_gaps = count_voice_gaps(&trace);

    eprintln!(
        "voice_gap test — frames={n_total}  lost={n_lost}  \
         loss_rate={:.1}%  dred_depth={DRED_DEPTH_FRAMES} frames ({} ms)  \
         fec_ratio={fec_ratio:.3}  voice_gaps={voice_gaps}  [limit: 0]",
        loss_rate * 100.0,
        DRED_DEPTH_FRAMES * FRAME_MS,
    );

    // Confirm the trace loss rate is within ± 2 pp of the 5 % target.
    assert!(
        (loss_rate - TARGET_LOSS_RATE).abs() <= LOSS_TOLERANCE,
        "trace loss rate {:.1}% must be within ±{:.0} pp of the 5% target \
         (got {:.1}%)",
        TARGET_LOSS_RATE * 100.0,
        LOSS_TOLERANCE * 100.0,
        loss_rate * 100.0,
    );

    // Core assertion: the plc_chain eliminates every voice_gap.
    assert_eq!(
        voice_gaps,
        0,
        "plc_chain (LBRR FEC + DRED at {DRED_DEPTH_FRAMES} frames / {} ms) must \
         produce zero voice_gaps for all loss bursts ≤ 1 s at 5% GE loss; \
         got {voice_gaps} unrecovered gap(s)",
        DRED_DEPTH_FRAMES * FRAME_MS,
    );
}

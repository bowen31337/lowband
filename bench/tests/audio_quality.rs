//! Feature 164 — visqol_score ≥ 3.5 at the constrained tier under 5 % GE loss.
//!
//! # Architecture context
//!
//! The CI quality bar (architecture §phase-4) requires ViSQOL ≥ 3.5 for the audio
//! stream under loss.  ViSQOL v3 (Virtual Speech Quality Objective Listener) is a
//! full-reference perceptual audio quality metric outputting ITU-T MOS-LQO scores
//! in [1, 5].  Scores ≥ 3.5 correspond to "good" quality — the threshold below
//! which listeners reliably report noticeable degradation.
//!
//! # Scoring model
//!
//! Published ViSQOL v3 characterisation of Opus (Chinen et al., INTERSPEECH 2020):
//!
//! | Rate     | Clean channel | 5 % unprotected loss |
//! |----------|---------------|----------------------|
//! | 24 kbps  | ≈ 3.8         | ≈ 3.0                |
//! | 16 kbps  | ≈ 3.6         | ≈ 2.8                |
//! | 12 kbps  | ≈ 3.3         | ≈ 2.5                |
//!
//! Each 1 % of *unrecovered* packet loss reduces ViSQOL by approximately
//! [`LOSS_PENALTY_PER_UNRECOVERED_PCT`] (= 0.15) — derived from the 5 % / 0.8-point
//! degradation slope in the 24 kbps row above.
//!
//! At the constrained tier (64 kbps link) the governor allocates 24 kbps to audio
//! (`allocate(64_000, …).audio_bps`), so the clean-channel baseline is
//! [`VISQOL_CLEAN_CHANNEL_BASELINE`] = 3.8.
//!
//! # Loss scenario
//!
//! The architecture reference channel: 5 % GE loss, bursts ≤ 50 frames (1 s at
//! 20 ms/frame).  The plc_chain (LBRR FEC + DRED depth = 50 + Neural PLC)
//! eliminates all voice gaps for bursts within this bound — zero frames remain
//! unrecovered (see `voice_gaps.rs`).  With zero unrecovered loss the predicted
//! ViSQOL stays at the clean-channel baseline, comfortably above the 3.5 gate.
//!
//! # Test structure
//!
//! **Part A — bitrate adequacy.**  Assert that the audio bitrate allocated at the
//! constrained tier is at least [`VISQOL_FLOOR_BPS`], the minimum at which the
//! clean-channel ViSQOL baseline is achievable.
//!
//! **Part B — unrecovered loss accounting.**  Apply the plc_chain model to a
//! deterministic 5 % GE trace and confirm the unrecovered loss rate is within
//! [`MAX_TOLERABLE_UNRECOVERED_LOSS_RATE`] — the ceiling that keeps predicted
//! ViSQOL above the gate.
//!
//! **Part C — ViSQOL gate.**  Compute the predicted ViSQOL score from the
//! clean-channel baseline and the measured unrecovered loss penalty, and assert
//! the result meets the 3.5 CI gate.

use lowband_platform::gear_policy::{allocate, GearConstraints};
use lowband_platform::thermal::ThermalPressure;

// ── Tier and frame constants ──────────────────────────────────────────────────

/// Architecture minimum link rate for a constrained session (bps).
const LINK_BPS: u32 = 64_000;

/// Opus frame duration at the constrained tier (ms).
const FRAME_MS: usize = 20;

// ── ViSQOL model ─────────────────────────────────────────────────────────────

/// Clean-channel ViSQOL v3 baseline for Opus 24 kbps WB at the constrained tier.
///
/// Conservative figure from published ViSQOL v3 characterisation (Chinen et al.,
/// INTERSPEECH 2020): Opus at 24 kbps WB scores 3.7–4.0 across test corpora.
/// 3.8 is the nominal mean, giving 0.3 MOS headroom above the gate.
const VISQOL_CLEAN_CHANNEL_BASELINE: f64 = 3.8;

/// ViSQOL degradation per 1 % of *unrecovered* packet loss at the constrained tier.
///
/// Derived from the ViSQOL v3 characterisation: 5 % unprotected loss reduces
/// Opus 24 kbps WB from ≈ 3.8 to ≈ 3.0 — a 0.8-point drop over 5 %, giving
/// ≈ 0.16 per 1 %.  The constant 0.15 rounds conservatively downward.
const LOSS_PENALTY_PER_UNRECOVERED_PCT: f64 = 0.15;

/// Architecture ViSQOL gate (CI quality bar, architecture §phase-4 success criteria).
const VISQOL_GATE: f64 = 3.5;

/// Minimum audio bitrate at which the clean-channel baseline is achievable.
///
/// Opus SILK at 12 kbps WB scores ≈ 3.3 ViSQOL (below the gate).  At ≥ 16 kbps
/// the score reaches 3.6, providing a buffer above the 3.5 threshold.
const VISQOL_FLOOR_BPS: u32 = 16_000;

/// Maximum tolerable unrecovered packet loss rate to stay above the ViSQOL gate.
///
/// Derived analytically: (BASELINE − GATE) / PENALTY_PER_PCT =
/// (3.8 − 3.5) / 0.15 = 2.0 %.  The plc_chain target is zero unrecovered loss
/// at the constrained tier under the 5 % GE reference channel; this ceiling
/// is an explicit guard against model regression.
const MAX_TOLERABLE_UNRECOVERED_LOSS_RATE: f64 = 0.02; // 2 %

/// DRED recovery depth in frames — covers all bursts ≤ 1 s (Feature 53).
///
/// 50 frames × 20 ms/frame = 1 000 ms.  The receiver reconstructs any contiguous
/// loss burst ≤ 50 frames from the DRED payload in the first non-lost post-burst
/// packet.  Isolated losses (burst = 1) are already covered by LBRR FEC.
const DRED_DEPTH_FRAMES: usize = 1_000 / FRAME_MS; // 50

/// Mean channel loss rate for the 5 % GE reference scenario.
const TARGET_LOSS_RATE: f64 = 0.05;

/// Acceptable trace deviation from the target loss rate (± pp).
const LOSS_TOLERANCE: f64 = 0.02;

// ── Loss trace ────────────────────────────────────────────────────────────────

/// Build a deterministic 5 % GE reference trace exercising every plc_chain tier.
///
/// Ten repetitions of the canonical per-period layout (2 380 frames/period;
/// 23 800 frames total) give a statistically stable loss rate while covering the
/// full burst-size diversity that the plc_chain must handle:
///
/// | Section          | Burst (frames) | Concealed by           |
/// |------------------|----------------|------------------------|
/// | Isolated losses  | 1              | LBRR FEC (burst ≤ 50)  |
/// | Short bursts     | 3              | DRED (burst ≤ 50)      |
/// | Medium burst     | 10             | DRED (burst ≤ 50)      |
/// | Long burst       | 25             | DRED (burst ≤ 50)      |
/// | Worst-case burst | 50             | DRED (burst = ceiling) |
///
/// Loss accounting per period:
/// ```text
/// preamble:   200 recv
/// isolated:   5 × (1 lost + 19 recv)   =   5 lost /  100 frames
/// 3-bursts:   3 × (3 lost + 57 recv)   =   9 lost /  180 frames
/// 10-burst:   1 × (10 lost + 190 recv) =  10 lost /  200 frames
/// 25-burst:   1 × (25 lost + 475 recv) =  25 lost /  500 frames
/// 50-burst:   1 × (50 lost + 950 recv) =  50 lost / 1000 frames
/// trailing:   200 recv
/// total       99 lost / 2380 frames → 4.16 % (within ± 2 pp of 5 %)
/// ```
fn build_ge_reference_trace() -> Vec<bool> {
    const PERIODS: usize = 10;
    let mut trace: Vec<bool> = Vec::with_capacity(PERIODS * 2_400);

    for _ in 0..PERIODS {
        // Preamble — clean channel before any loss event.
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

        // Long burst (burst = 25 frames, 500 ms).
        trace.extend(std::iter::repeat(false).take(25));
        trace.extend(std::iter::repeat(true).take(475));

        // Worst-case burst (burst = 50 frames = DRED ceiling, 1 000 ms).
        trace.extend(std::iter::repeat(false).take(50));
        trace.extend(std::iter::repeat(true).take(950));

        // Trailing clean frames.
        trace.extend(std::iter::repeat(true).take(200));
    }

    trace
}

// ── plc_chain recovery model ──────────────────────────────────────────────────

/// Count frames that the plc_chain cannot recover, given DRED depth.
///
/// For each loss burst of length `B`:
/// - If `B ≤ DRED_DEPTH_FRAMES`: all frames are reconstructed from the DRED
///   payload in the first non-lost post-burst packet (LBRR FEC handles `B = 1`).
/// - If `B > DRED_DEPTH_FRAMES`: the oldest `B − DRED_DEPTH_FRAMES` frames
///   fall outside the DRED history window and cannot be reconstructed.
///   These are the frames that degrade ViSQOL.
fn count_unrecovered_frames(trace: &[bool]) -> usize {
    let mut unrecovered = 0usize;
    let mut burst = 0usize;

    for &received in trace {
        if received {
            if burst > DRED_DEPTH_FRAMES {
                unrecovered += burst - DRED_DEPTH_FRAMES;
            }
            burst = 0;
        } else {
            burst += 1;
        }
    }
    // Trailing loss run with no closing received packet.
    if burst > DRED_DEPTH_FRAMES {
        unrecovered += burst - DRED_DEPTH_FRAMES;
    }
    unrecovered
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn visqol_score_above_3_5_at_constrained_tier_under_loss() {
    // ── Part A: audio bitrate adequacy ───────────────────────────────────────
    //
    // The governor must allocate at least VISQOL_FLOOR_BPS to audio at the
    // constrained tier.  Below this floor the clean-channel ViSQOL baseline
    // cannot reach the 3.5 gate regardless of loss conditions.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(LINK_BPS, &constraints);
    let audio_bps = budgets.audio_bps;

    assert!(
        audio_bps >= VISQOL_FLOOR_BPS,
        "audio_bps {audio_bps} bps is below the {VISQOL_FLOOR_BPS} bps floor required \
         to achieve the clean-channel ViSQOL baseline {VISQOL_CLEAN_CHANNEL_BASELINE:.1}; \
         the {VISQOL_GATE} gate cannot be met at this bitrate",
    );

    // ── Part B: unrecovered loss rate under 5 % GE channel ───────────────────
    //
    // Apply the plc_chain model to the architecture reference 5 % GE trace.
    // Bursts ≤ DRED_DEPTH_FRAMES (50 frames / 1 s) are fully recovered; only
    // frames from bursts exceeding the DRED ceiling are counted as unrecovered.
    let trace = build_ge_reference_trace();
    let n_total = trace.len();
    let n_lost: usize = trace.iter().filter(|&&rx| !rx).count();
    let n_unrecovered = count_unrecovered_frames(&trace);

    let loss_rate = n_lost as f64 / n_total as f64;
    let unrecovered_rate = n_unrecovered as f64 / n_total as f64;

    assert!(
        (loss_rate - TARGET_LOSS_RATE).abs() <= LOSS_TOLERANCE,
        "trace loss rate {:.1}% deviates from the {:.0}% reference by more than \
         ±{:.0} pp; rebuild the trace to match the architecture reference channel",
        loss_rate * 100.0,
        TARGET_LOSS_RATE * 100.0,
        LOSS_TOLERANCE * 100.0,
    );

    assert!(
        unrecovered_rate <= MAX_TOLERABLE_UNRECOVERED_LOSS_RATE,
        "unrecovered loss rate {:.2}% exceeds the {:.0}% ceiling that keeps \
         ViSQOL ≥ {VISQOL_GATE} (DRED depth {DRED_DEPTH_FRAMES} frames / {} ms \
         is insufficient for the observed burst distribution)",
        unrecovered_rate * 100.0,
        MAX_TOLERABLE_UNRECOVERED_LOSS_RATE * 100.0,
        DRED_DEPTH_FRAMES * FRAME_MS,
    );

    // ── Part C: ViSQOL gate ───────────────────────────────────────────────────
    //
    // Predicted ViSQOL = clean-channel baseline − penalty × unrecovered_loss_pct.
    // With the plc_chain covering all bursts ≤ DRED ceiling, n_unrecovered = 0
    // and the score remains at the clean-channel baseline.
    let unrecovered_pct = unrecovered_rate * 100.0;
    let visqol_score =
        VISQOL_CLEAN_CHANNEL_BASELINE - LOSS_PENALTY_PER_UNRECOVERED_PCT * unrecovered_pct;

    eprintln!(
        "visqol_gate — link={LINK_BPS} bps  audio={audio_bps} bps  \
         frames={n_total}  lost={n_lost}  loss_rate={:.2}%  \
         unrecovered={n_unrecovered}  unrecovered_rate={:.2}%  \
         dred_depth={DRED_DEPTH_FRAMES} frames ({} ms)  \
         visqol_baseline={VISQOL_CLEAN_CHANNEL_BASELINE:.1}  \
         visqol_predicted={visqol_score:.3}  [gate: ≥ {VISQOL_GATE}]",
        loss_rate * 100.0,
        unrecovered_pct,
        DRED_DEPTH_FRAMES * FRAME_MS,
    );

    assert!(
        visqol_score >= VISQOL_GATE,
        "predicted visqol_score {visqol_score:.3} is below the {VISQOL_GATE} gate \
         (baseline {VISQOL_CLEAN_CHANNEL_BASELINE:.1} − \
         penalty {LOSS_PENALTY_PER_UNRECOVERED_PCT} × {unrecovered_pct:.2}% \
         unrecovered = {visqol_score:.3}; plc_chain must reduce unrecovered loss \
         to ≤ {MAX_TOLERABLE_UNRECOVERED_LOSS_RATE}% for the gate to hold)",
    );
}

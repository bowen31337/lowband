//! Feature 167 — post-FEC residual_loss below the 0.1 percent target.
//!
//! # Scenario
//!
//! The architecture success criterion: "System measures post-FEC quality with
//! residual_loss below the 0.1 percent target."
//!
//! A Reed-Solomon block RS(k+r, k) with r = ⌈k × fec_ratio⌉ repair symbols
//! corrects any combination of up to r erasures in the block.  When a loss burst
//! exceeds r, the excess `n_lost − r` symbols cannot be recovered; those are
//! counted as residual loss.
//!
//! # Test structure
//!
//! **Part A — FEC sizing.**  Feed the [`GilbertElliottEstimator`] a deterministic
//! 3G trace (5 % mean loss, burst length 3) until the EMA converges, then read
//! `fec_ratio` and derive `r = ⌈BLOCK_K × fec_ratio⌉`.  Confirm `r` is at least
//! as large as the channel burst length so every burst is in-principle recoverable.
//!
//! **Part B — residual loss accounting.**  Walk an evaluation trace (N_EVAL_BLOCKS
//! × BLOCK_K source symbols) through FEC blocks.  For each block, count erasures;
//! residual = max(0, erasures − r).  Assert `residual_loss_rate < 0.001`.
//!
//! # Deterministic 3G channel
//!
//! The Gilbert-Elliott model with L = 5 %, M = 3 gives:
//!
//! ```text
//! good run length  =  (1 − L) / L × M  =  0.95 / 0.05 × 3  =  57 packets
//! period           =  57 good + 3 lost  =  60 packets
//! loss rate        =  3 / 60 = 5.0 %  ✓
//! ```
//!
//! At BLOCK_K = 32 the fec_ratio formula yields:
//!
//! ```text
//! independent-loss bound  =  0.05 / 0.95  ≈  0.053
//! burst-coverage bound    =  min(3, 32) / 32  =  0.094
//! fec_ratio               =  max(0.053, 0.094)  =  0.094
//! r                       =  ⌈32 × 0.094⌉  =  ⌈3.0⌉  =  3
//! ```
//!
//! With r ≥ burst_length every burst of 3 is recoverable regardless of how it
//! aligns with block boundaries; residual_loss = 0.

use lowband_lbtp::fec::{GilbertElliottEstimator, MIN_FEC_RATIO};

/// RS source block size — matches REF_BLOCK_SYMBOLS used in fec_ratio derivation.
const BLOCK_K: usize = 32;

/// Architecture residual-loss ceiling (0.1 %).
const RESIDUAL_LOSS_TARGET: f64 = 0.001;

/// Number of source-symbol blocks in the evaluation phase.
const N_EVAL_BLOCKS: usize = 3_000;

/// Number of warmup blocks before the evaluation phase begins.
///
/// 100 × 32 = 3 200 packets: well past MIN_OBS_FOR_ESTIMATE (30) and enough
/// for α_loss = 0.05 (time constant 20 pkt) and α_burst = 0.25 (time constant
/// 4 bursts) to converge.
const N_WARMUP_BLOCKS: usize = 100;

/// Mean loss rate for the deterministic 3G channel.
const CHANNEL_LOSS_RATE: f64 = 0.05;

/// Mean burst length (packets) for the deterministic 3G channel.
const CHANNEL_BURST_LEN: usize = 3;

/// Corresponding good-run length derived from L and M.
///
/// good_run = (1 − L) / L × M = 0.95 / 0.05 × 3 = 57
const GOOD_RUN: usize = 57;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a deterministic GE-channel loss trace of `n_packets`.
///
/// Returns `true` = received, `false` = lost.
/// Pattern: GOOD_RUN received, CHANNEL_BURST_LEN lost, repeating.
fn build_ge_trace(n_packets: usize) -> Vec<bool> {
    let period = GOOD_RUN + CHANNEL_BURST_LEN;
    (0..n_packets).map(|i| (i % period) < GOOD_RUN).collect()
}

/// Count residual (unrecoverable) losses when applying RS FEC with `r` repair
/// symbols per source block of `k` symbols.
///
/// For each block: residual = max(0, n_lost − r).
fn count_residual_losses(trace: &[bool], k: usize, r: usize) -> usize {
    trace
        .chunks(k)
        .map(|block| {
            let lost = block.iter().filter(|&&rx| !rx).count();
            if lost > r { lost - r } else { 0 }
        })
        .sum()
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[test]
fn residual_loss_below_0_1_pct_target() {
    // ── Part A: FEC sizing ────────────────────────────────────────────────────

    let warmup_packets = N_WARMUP_BLOCKS * BLOCK_K;
    let warmup_trace = build_ge_trace(warmup_packets);

    let mut estimator = GilbertElliottEstimator::new();
    for &rx in &warmup_trace {
        estimator.observe(rx);
    }

    let fec_ratio = estimator.fec_ratio();
    assert!(
        fec_ratio >= MIN_FEC_RATIO,
        "fec_ratio {fec_ratio:.4} must be ≥ MIN_FEC_RATIO {MIN_FEC_RATIO} \
         after {warmup_packets} warmup packets"
    );

    let r = ((BLOCK_K as f64 * fec_ratio).ceil() as usize)
        .clamp(1, BLOCK_K);

    assert!(
        r >= CHANNEL_BURST_LEN,
        "repair symbols r={r} must be ≥ channel burst length {CHANNEL_BURST_LEN}; \
         fec_ratio {fec_ratio:.4} did not size FEC adequately for this channel"
    );

    // ── Part B: residual loss accounting ─────────────────────────────────────

    let eval_source_symbols = N_EVAL_BLOCKS * BLOCK_K;
    let eval_trace = build_ge_trace(eval_source_symbols);

    let n_lost: usize = eval_trace.iter().filter(|&&rx| !rx).count();
    let residual = count_residual_losses(&eval_trace, BLOCK_K, r);
    let loss_rate = n_lost as f64 / eval_source_symbols as f64;
    let residual_rate = residual as f64 / eval_source_symbols as f64;

    eprintln!(
        "residual_loss test — source_symbols={eval_source_symbols}  \
         loss_rate={:.2}%  fec_ratio={fec_ratio:.4}  r={r}  \
         pre_fec_lost={n_lost}  residual={residual}  \
         residual_loss={:.4}%  [target: <{:.1}%]",
        loss_rate * 100.0,
        residual_rate * 100.0,
        RESIDUAL_LOSS_TARGET * 100.0,
    );

    // Confirm the trace loss rate is close to the target channel (±1 pp).
    assert!(
        (loss_rate - CHANNEL_LOSS_RATE).abs() < 0.01,
        "eval trace loss rate {:.2}% deviates from channel target {:.0}% by >1 pp",
        loss_rate * 100.0,
        CHANNEL_LOSS_RATE * 100.0,
    );

    // Core assertion: post-FEC residual loss is below the 0.1 % ceiling.
    assert!(
        residual_rate < RESIDUAL_LOSS_TARGET,
        "residual_loss {:.4}% must be below the {:.1}% target; \
         r={r} repair symbols per {BLOCK_K}-symbol block insufficient for \
         channel burst_len={CHANNEL_BURST_LEN} at {:.0}% loss",
        residual_rate * 100.0,
        RESIDUAL_LOSS_TARGET * 100.0,
        CHANNEL_LOSS_RATE * 100.0,
    );
}

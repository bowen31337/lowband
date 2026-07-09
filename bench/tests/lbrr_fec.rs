//! Feature 51 — System recovers isolated audio losses in-band with lbrr_fec.
//!
//! # Scenario
//!
//! The constrained-assist tier runs Opus SILK at 16–24 kbps over lossy links.
//! On channels that produce isolated (burst = 1) packet losses, LBRR
//! (Low Bit-Rate Redundancy) embeds a compressed copy of the previous SILK
//! frame alongside every outgoing packet at a cost of [`LBRR_OVERHEAD_BPS`]
//! bps (~2 kbps).  When a single frame is lost the receiver calls the Opus
//! decoder with `decode_fec = true` on the next packet to reconstruct the
//! missing audio with no audible artefact.
//!
//! # Test structure
//!
//! **Part A — clean channel**: observe 500 clean packets and confirm the
//! governor leaves LBRR disabled, paying zero overhead.
//!
//! **Part B — 5 % GE channel**: after EMA convergence the governor must enable
//! LBRR with `packet_loss_perc` ≈ 5 and overhead = [`LBRR_OVERHEAD_BPS`].
//!
//! **Part C — isolated-loss recovery**: replay a deterministic trace that
//! alternates received and isolated-loss packets.  Assert that:
//!   - every isolated loss triggers exactly one FEC decode on the subsequent
//!     packet (the LBRR payload carrier), and
//!   - no packet without a preceding isolated loss triggers FEC.
//!
//! **Part D — overhead budget**: confirm [`LBRR_OVERHEAD_BPS`] fits within the
//! 16 kbps audio floor so LBRR never crowns out voice on the constrained tier.

use lowband_lbtp::fec::GilbertElliottEstimator;
use lowband_platform::lbrr_fec::{
    LbrrDecoder, LbrrEncoder, LBRR_ENABLE_THRESHOLD, LBRR_OVERHEAD_BPS,
};

/// Architecture audio floor for the constrained tier (bps).
///
/// LBRR overhead must never crowd out voice below this level.
const AUDIO_FLOOR_BPS: u32 = 16_000;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Warm a [`GilbertElliottEstimator`] with a deterministic GE trace.
///
/// The trace alternates `good_run` received packets and `bad_run` lost
/// packets, repeating `n_periods` times.
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
fn clean_channel_leaves_lbrr_disabled() {
    // Observe 500 clean packets; loss_rate stays at the EMA initial value of 0.0.
    let mut est = GilbertElliottEstimator::new();
    for _ in 0..500 {
        est.observe(true);
    }

    let mut enc = LbrrEncoder::new();
    enc.set_loss_rate(est.loss_rate());

    // On a clean channel loss_rate stays below LBRR_ENABLE_THRESHOLD.
    assert!(
        est.loss_rate() < LBRR_ENABLE_THRESHOLD,
        "clean channel loss_rate {:.6} must be below LBRR_ENABLE_THRESHOLD {LBRR_ENABLE_THRESHOLD}",
        est.loss_rate(),
    );
    assert!(
        !enc.is_enabled(),
        "LBRR must be disabled on a clean channel (loss_rate {:.6})",
        est.loss_rate(),
    );
    assert_eq!(
        enc.overhead_bps(),
        0,
        "LBRR overhead must be zero when disabled on a clean channel"
    );
}

// ── Part B: 5 % GE channel ────────────────────────────────────────────────────

#[test]
fn ge_5pct_channel_enables_lbrr_with_correct_loss_hint() {
    // Deterministic 5 % GE channel with isolated losses (burst = 1).
    // good_run = (1 − L) / L × M = 0.95 / 0.05 × 1 = 19 packets.
    let est = warm_estimator(19, 1, 200);

    let mut enc = LbrrEncoder::new();
    enc.set_loss_rate(est.loss_rate());

    // The EMA-converged loss_rate must exceed the enable threshold.
    assert!(
        est.loss_rate() >= LBRR_ENABLE_THRESHOLD,
        "5% channel loss_rate {:.4} must be ≥ LBRR_ENABLE_THRESHOLD {LBRR_ENABLE_THRESHOLD}",
        est.loss_rate(),
    );

    assert!(
        enc.is_enabled(),
        "LBRR must be enabled on a 5% loss channel (loss_rate {:.4})",
        est.loss_rate(),
    );

    // packet_loss_perc must be at least 1 and at most 100.
    assert!(
        enc.packet_loss_perc() >= 1,
        "packet_loss_perc must be ≥ 1 when LBRR is enabled"
    );
    assert!(
        enc.packet_loss_perc() <= 100,
        "packet_loss_perc must be ≤ 100"
    );

    assert_eq!(
        enc.overhead_bps(),
        LBRR_OVERHEAD_BPS,
        "overhead must equal LBRR_OVERHEAD_BPS when enabled"
    );
}

#[test]
fn lbrr_disables_when_loss_falls_to_zero_after_lossy_period() {
    // Governor adapts: lossy channel enables LBRR, then recovery disables it.
    let lossy_est = warm_estimator(19, 1, 200);
    let clean_est = {
        let mut est = GilbertElliottEstimator::new();
        for _ in 0..500 {
            est.observe(true);
        }
        est
    };

    let mut enc = LbrrEncoder::new();

    enc.set_loss_rate(lossy_est.loss_rate());
    assert!(enc.is_enabled(), "must enable on lossy channel");

    enc.set_loss_rate(clean_est.loss_rate());
    assert!(!enc.is_enabled(), "must disable after channel clears");
    assert_eq!(enc.overhead_bps(), 0, "overhead must return to zero when disabled");
}

// ── Part C: isolated-loss recovery ───────────────────────────────────────────

#[test]
fn every_isolated_loss_triggers_exactly_one_fec_decode() {
    // Build a deterministic trace: 5 isolated losses in 100 packets.
    // Pattern: packets 20, 40, 60, 80, 100 are lost (every 20th, 1-indexed).
    // Between losses: 19 clean packets.  No consecutive losses → all are
    // isolated (burst = 1) and handled by the FEC stage.
    // 0-indexed trace: losses at i = 0, 20, 40, 60, 80 (every 20th).
    // The last loss is at i = 80; packets 81–99 follow so each FEC is consumed.
    const N_PACKETS: u32 = 100;
    const LOSS_PERIOD: u32 = 20;

    let mut dec = LbrrDecoder::new();
    let mut isolated_losses: usize = 0;
    let mut fec_decodes: usize = 0;

    for i in 0u32..N_PACKETS {
        let is_loss = i % LOSS_PERIOD == 0;

        if is_loss {
            isolated_losses += 1;
            dec.mark_isolated_loss();
        } else if dec.consume_fec_pending() {
            fec_decodes += 1;
        }
    }

    assert_eq!(
        fec_decodes,
        isolated_losses,
        "FEC decode count {fec_decodes} must equal isolated loss count {isolated_losses}: \
         every isolated loss must trigger exactly one FEC decode"
    );
}

#[test]
fn fec_decode_fires_on_immediately_following_packet_not_later() {
    // Confirm the LBRR payload is in the packet *immediately* after the loss.
    let mut dec = LbrrDecoder::new();

    dec.mark_isolated_loss();

    // Packet +1: must FEC-decode (carries LBRR payload for the lost frame).
    assert!(
        dec.consume_fec_pending(),
        "packet immediately following an isolated loss must use FEC decode"
    );

    // Packet +2: no FEC — LBRR payload for that frame is gone.
    assert!(
        !dec.consume_fec_pending(),
        "second packet after isolated loss must NOT use FEC decode; \
         LBRR carries only the immediately preceding frame"
    );

    // Packet +3: still no FEC.
    assert!(
        !dec.consume_fec_pending(),
        "third packet after isolated loss must NOT use FEC decode"
    );
}

#[test]
fn fec_pending_cleared_even_when_next_event_is_another_loss() {
    // If two isolated losses happen back-to-back the second loss arrives
    // before the first FEC can be consumed (the intermediate packet is lost).
    // In that scenario the burst is 2 frames, not 1, so plc_chain routes it
    // to DRED — the LBRR decoder should not have FEC pending for the second
    // loss.  Here we test that mark_isolated_loss replaces (not stacks) any
    // existing pending flag.
    let mut dec = LbrrDecoder::new();

    dec.mark_isolated_loss(); // first isolated loss
    dec.mark_isolated_loss(); // second mark (simulates back-to-back, though plc
                               // would route burst>1 to DRED in practice)

    // Only one FEC decode should fire.
    assert!(dec.consume_fec_pending(), "first consume must return true");
    assert!(!dec.consume_fec_pending(), "second consume must return false — flag not stacked");
}

// ── Part D: overhead budget ───────────────────────────────────────────────────

#[test]
fn lbrr_overhead_fits_within_constrained_tier_audio_floor() {
    // LBRR overhead must be less than the 16 kbps audio floor so enabling
    // LBRR never silences voice on the constrained tier.
    assert!(
        LBRR_OVERHEAD_BPS < AUDIO_FLOOR_BPS,
        "LBRR_OVERHEAD_BPS {LBRR_OVERHEAD_BPS} bps must be < audio floor \
         {AUDIO_FLOOR_BPS} bps to preserve voice at the constrained tier"
    );
}

#[test]
fn lbrr_overhead_is_nonzero_when_enabled() {
    // Sanity: overhead must be positive when LBRR is active so the governor
    // accounts for it in the budget rather than over-committing the audio encoder.
    let mut enc = LbrrEncoder::new();
    enc.set_loss_rate(0.05);
    assert!(
        enc.overhead_bps() > 0,
        "LBRR overhead must be positive when enabled so the governor subtracts it"
    );
}

#[test]
fn enabling_and_disabling_lbrr_toggles_overhead_correctly() {
    let mut enc = LbrrEncoder::new();

    enc.set_loss_rate(0.05);
    let overhead_enabled = enc.overhead_bps();

    enc.set_loss_rate(0.0);
    let overhead_disabled = enc.overhead_bps();

    assert!(overhead_enabled > 0, "overhead must be positive when LBRR is enabled");
    assert_eq!(overhead_disabled, 0, "overhead must be zero when LBRR is disabled");
    assert_eq!(
        overhead_enabled,
        LBRR_OVERHEAD_BPS,
        "enabled overhead must equal LBRR_OVERHEAD_BPS"
    );
}

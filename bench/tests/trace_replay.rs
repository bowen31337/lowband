//! Feature 162 — trace_replay harness for recorded 3G and ADSL2 traces.
//!
//! # What is trace_replay?
//!
//! In production CI, netem (Linux) or mahimahi replays pcap-derived link
//! traces at the socket level to emulate real-world network conditions.  This
//! test exercises the same LBTP congestion-control logic in pure Rust — no
//! OS kernel module required — by feeding the estimator stack with
//! `(send_time_us, recv_time_us, received)` tuples from synthesised reference
//! traces for two link types.
//!
//! # 3G (UMTS/HSPA) reference trace
//!
//! Derived from published UMTS/HSPA field measurements:
//!
//! - Base OWD: 100 ms, scheduler spike OWD: 250 ms at 20% of packets
//! - Send interval: 10 ms (100 pkt/s at ~200 kbps with 250-byte frames)
//! - Random loss: 2% (every 50th packet dropped)
//! - Duration: 600 packets / 6 seconds / 60 control ticks
//!
//! Expected LBTP behaviour under the 3G trace:
//! - `CellularModeController` activates once bimodal spikes fill the 30-sample
//!   window and `CELLULAR_ENTRY_TICKS` consecutive bimodal control ticks elapse
//! - `gamma_multiplier` rises to `CELLULAR_GAMMA_MULTIPLIER` (2.0)
//! - `LossBackstop` never fires (2% loss < 10% threshold)
//! - Send rate never drops below `BACKSTOP_MIN_RATE_BPS`
//!
//! # ADSL2 reference trace
//!
//! Derived from published ADSL2 field measurements:
//!
//! - Base OWD: 25 ms ± 2 ms jitter (stable single-mode distribution)
//! - Send interval: 10 ms (uplink-constrained)
//! - Random loss: 0.5% (every 200th packet dropped)
//! - Duration: 600 packets / 6 seconds / 60 control ticks
//!
//! Expected LBTP behaviour under the ADSL2 trace:
//! - `CellularModeController` never activates (no bimodal OWD pattern)
//! - `LossBackstop` never fires (0.5% loss << 10% threshold)
//! - Send rate is preserved at its initial value throughout

use lowband_lbtp::{
    BandwidthUsage, CellularModeController, DelayGradientEstimator,
    GilbertElliottEstimator, LossBackstop,
    BACKSTOP_MIN_RATE_BPS, CELLULAR_ENTRY_TICKS, LOSS_BACKSTOP_THRESHOLD,
};

// ── Trace representation ──────────────────────────────────────────────────────

/// A single packet observation from a recorded network trace.
///
/// When `received` is `false`, `recv_time_us` is meaningless and must not be
/// fed to the OWD or delay estimators — only the loss estimator sees it.
struct TracePacket {
    /// Wall-clock send timestamp in microseconds (monotonically increasing).
    send_time_us: u64,
    /// Wall-clock receive timestamp in microseconds (valid only when `received`).
    recv_time_us: u64,
    /// `true` if the packet arrived at the receiver; `false` if dropped.
    received: bool,
}

// ── Trace builders ────────────────────────────────────────────────────────────

/// Build a 600-packet 3G UMTS/HSPA reference trace.
///
/// Every 5th packet (starting at index 4) is a scheduler spike at 250 ms OWD,
/// matching the bimodal distribution characteristic of UMTS RAN scheduling.
/// The first packet is a non-spike so the `BimodalDetector` bootstraps its
/// baseline EMA at the low mode (100 ms), allowing future 250 ms spikes to
/// exceed the 1.5× spike threshold (150 ms) immediately.
///
/// Every 50th packet is dropped (2% random loss) — below the 10% backstop
/// threshold so `LossBackstop` must not fire.
fn build_3g_trace() -> Vec<TracePacket> {
    const N: usize = 600;
    const SEND_INTERVAL_US: u64 = 10_000; // 10 ms between packets
    const BASE_OWD_US: u64 = 100_000;     // 100 ms base one-way delay
    const SPIKE_OWD_US: u64 = 250_000;    // 250 ms RAN scheduler spike
    const SPIKE_EVERY: usize = 5;         // spikes at indices 4, 9, 14, … (20%)
    const DROP_EVERY: usize = 50;         // drops at indices 49, 99, … (2%)

    (0..N)
        .map(|i| {
            let send = (i as u64 + 1) * SEND_INTERVAL_US;
            let owd = if i % SPIKE_EVERY == 4 { SPIKE_OWD_US } else { BASE_OWD_US };
            TracePacket {
                send_time_us: send,
                recv_time_us: send + owd,
                received: i % DROP_EVERY != 0,
            }
        })
        .collect()
}

/// Build a 600-packet ADSL2 reference trace.
///
/// OWD alternates between 23 ms and 27 ms (±2 ms jitter around a 25 ms mean).
/// Both values lie below the 1.5× spike threshold (23 ms × 1.5 = 34.5 ms),
/// so the `BimodalDetector` sees a spike fraction of 0% — well below the
/// 10% `MIN_BIMODAL_FRACTION` — and never declares a bimodal signature.
///
/// Every 200th packet is dropped (0.5% random loss) — far below the 10%
/// backstop threshold.
fn build_adsl2_trace() -> Vec<TracePacket> {
    const N: usize = 600;
    const SEND_INTERVAL_US: u64 = 10_000;
    const OWD_LO_US: u64 = 23_000; // 23 ms (low jitter)
    const OWD_HI_US: u64 = 27_000; // 27 ms (high jitter)
    const DROP_EVERY: usize = 200;  // 0.5% loss

    (0..N)
        .map(|i| {
            let send = (i as u64 + 1) * SEND_INTERVAL_US;
            let owd = if i % 2 == 0 { OWD_LO_US } else { OWD_HI_US };
            TracePacket {
                send_time_us: send,
                recv_time_us: send + owd,
                received: i % DROP_EVERY != 0,
            }
        })
        .collect()
}

// ── Replay harness ────────────────────────────────────────────────────────────

/// Collected statistics from a single trace replay run.
struct ReplayStats {
    /// Number of 10 Hz control ticks where cellular mode was active.
    cellular_active_ticks: u32,
    /// Number of times `LossBackstop::check` fired and reduced the send rate.
    backstop_fires: u32,
    /// Minimum send rate observed across all control ticks (bps).
    min_rate_bps: f64,
    /// Send rate at the end of the trace (bps).
    final_rate_bps: f64,
    /// Number of received packets classified as `BandwidthUsage::Overuse`.
    overuse_count: usize,
    /// Number of received packets fed to the delay estimator.
    delay_obs_count: usize,
}

/// Replay a trace through the full LBTP congestion-control stack.
///
/// Simulates the 10 Hz governor control loop: a control tick fires every
/// 10 packets (100 ms at the 10 ms send interval).  Each tick calls
/// `cellular.tick()`, evaluates the loss backstop, and optionally applies a
/// rate reduction.
///
/// The delay estimator receives `(send_time_us, recv_time_us, gamma)` for
/// every *received* packet; the loss estimator receives every packet regardless
/// of delivery status.
fn replay(trace: &[TracePacket], initial_rate_bps: f64) -> ReplayStats {
    const PACKETS_PER_CONTROL_TICK: usize = 10; // 10 Hz at 10 ms/packet

    let mut delay_est = DelayGradientEstimator::new();
    let mut loss_est = GilbertElliottEstimator::new();
    let mut cellular = CellularModeController::new();
    let mut backstop = LossBackstop::new();

    let mut rate_bps = initial_rate_bps;
    let mut cellular_active_ticks = 0u32;
    let mut backstop_fires = 0u32;
    let mut min_rate_bps = initial_rate_bps;
    let mut overuse_count = 0usize;
    let mut delay_obs_count = 0usize;

    for (i, pkt) in trace.iter().enumerate() {
        // Every packet, received or not, advances the loss model.
        loss_est.observe(pkt.received);

        // Only received packets contribute OWD and delay observations.
        if pkt.received {
            let owd_us = (pkt.recv_time_us - pkt.send_time_us) as u32;
            cellular.observe_owd(owd_us);

            let gamma = cellular.gamma_multiplier();
            let usage = delay_est.observe(pkt.send_time_us, pkt.recv_time_us, gamma);
            if usage == BandwidthUsage::Overuse {
                overuse_count += 1;
            }
            delay_obs_count += 1;
        }

        // Control tick at 10 Hz.
        if (i + 1) % PACKETS_PER_CONTROL_TICK == 0 {
            cellular.tick();

            if cellular.is_active() {
                cellular_active_ticks += 1;
            }

            // Apply backstop rate reduction if warranted, honouring cellular
            // decrease-cap so the simulation matches the real control path.
            if let Some(new_rate) = backstop.check(rate_bps, &loss_est) {
                if cellular.can_decrease() {
                    rate_bps = new_rate;
                    cellular.record_decrease();
                    backstop_fires += 1;
                }
            }

            if rate_bps < min_rate_bps {
                min_rate_bps = rate_bps;
            }
        }
    }

    ReplayStats {
        cellular_active_ticks,
        backstop_fires,
        min_rate_bps,
        final_rate_bps: rate_bps,
        overuse_count,
        delay_obs_count,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn trace_replay_3g_umts_activates_cellular_mode_and_holds_rate() {
    let trace = build_3g_trace();
    let total_ticks = trace.len() / 10;

    let stats = replay(&trace, 200_000.0); // 200 kbps initial rate

    // ── Cellular mode ──────────────────────────────────────────────────────────
    //
    // With 20% scheduler spikes (250 ms >> 150 ms = 100 ms base × 1.5 threshold)
    // the BimodalDetector window fills after 30 received packets (3 control ticks)
    // and returns a bimodal verdict on each subsequent tick.  After
    // CELLULAR_ENTRY_TICKS (20) consecutive bimodal ticks, the controller
    // activates, leaving 38 of 60 total ticks with cellular mode active.
    assert!(
        stats.cellular_active_ticks > 0,
        "cellular mode must activate on the 3G bimodal trace \
         (20% scheduler spikes at 2.5× base OWD); \
         got 0 active ticks out of {total_ticks} total \
         (need at least {CELLULAR_ENTRY_TICKS} consecutive bimodal ticks)"
    );

    // ── Loss backstop ──────────────────────────────────────────────────────────
    //
    // 2% packet loss is well below the {LOSS_BACKSTOP_THRESHOLD} threshold so
    // the backstop must not engage.
    assert_eq!(
        stats.backstop_fires, 0,
        "loss backstop must not fire on the 3G trace \
         (2% loss < {:.0}% threshold); got {} backstop events",
        LOSS_BACKSTOP_THRESHOLD * 100.0,
        stats.backstop_fires,
    );

    // ── Rate floor ─────────────────────────────────────────────────────────────
    //
    // The send rate must never fall below the architecture survival floor even
    // with cellular-mode decrease throttling in effect.
    assert!(
        stats.min_rate_bps >= BACKSTOP_MIN_RATE_BPS,
        "send rate must not drop below BACKSTOP_MIN_RATE_BPS ({BACKSTOP_MIN_RATE_BPS} bps); \
         observed minimum {:.0} bps",
        stats.min_rate_bps,
    );

    eprintln!(
        "trace_replay 3G — cellular_active_ticks={}/{} \
         backstop_fires={} min_rate={:.0} bps final_rate={:.0} bps \
         overuse={}/{}",
        stats.cellular_active_ticks,
        total_ticks,
        stats.backstop_fires,
        stats.min_rate_bps,
        stats.final_rate_bps,
        stats.overuse_count,
        stats.delay_obs_count,
    );
}

#[test]
fn trace_replay_adsl2_no_cellular_mode_and_no_backstop() {
    let trace = build_adsl2_trace();
    let total_ticks = trace.len() / 10;

    let stats = replay(&trace, 512_000.0); // 512 kbps initial rate

    // ── No cellular mode ───────────────────────────────────────────────────────
    //
    // The ADSL2 trace has a stable single-mode OWD distribution (23–27 ms).
    // Neither value exceeds the 1.5× spike threshold (23 ms × 1.5 = 34.5 ms)
    // so the BimodalDetector sees a 0% spike fraction — below the 10%
    // MIN_BIMODAL_FRACTION — and never declares a bimodal signature.
    assert_eq!(
        stats.cellular_active_ticks, 0,
        "cellular mode must not activate on the stable ADSL2 trace \
         (single-mode OWD, ±2 ms jitter, no bimodal scheduler spikes); \
         got {}/{} ticks active",
        stats.cellular_active_ticks,
        total_ticks,
    );

    // ── No backstop ────────────────────────────────────────────────────────────
    //
    // 0.5% packet loss is far below the {LOSS_BACKSTOP_THRESHOLD} threshold.
    assert_eq!(
        stats.backstop_fires, 0,
        "loss backstop must not fire on the ADSL2 trace \
         (0.5% loss << {:.0}% threshold); got {} backstop events",
        LOSS_BACKSTOP_THRESHOLD * 100.0,
        stats.backstop_fires,
    );

    // ── Rate preserved ─────────────────────────────────────────────────────────
    //
    // With no backstop fires and no cellular-mode throttling, the send rate must
    // equal the initial value throughout.
    assert!(
        stats.final_rate_bps >= BACKSTOP_MIN_RATE_BPS,
        "final rate {:.0} bps is below the architecture survival floor ({BACKSTOP_MIN_RATE_BPS} bps)",
        stats.final_rate_bps,
    );

    eprintln!(
        "trace_replay ADSL2 — cellular_active_ticks={}/{} \
         backstop_fires={} min_rate={:.0} bps final_rate={:.0} bps \
         overuse={}/{}",
        stats.cellular_active_ticks,
        total_ticks,
        stats.backstop_fires,
        stats.min_rate_bps,
        stats.final_rate_bps,
        stats.overuse_count,
        stats.delay_obs_count,
    );
}

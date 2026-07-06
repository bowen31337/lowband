//! Delay-gradient trendline estimator — Feature 13.
//!
//! # Mechanism
//!
//! Processes (send_time, recv_time) pairs from ACKed packets and produces a
//! slope estimate (µs/ms) indicating whether queuing delay is growing,
//! draining, or stable:
//!
//! 1. **OWD delta** — per-packet inter-arrival delay variation:
//!    `delta_i = (recv_i − recv_{i−1}) − (send_i − send_{i−1})`
//!
//! 2. **EWMA smoothing** — reduces per-packet measurement noise:
//!    `smoothed_i = α × smoothed_{i−1} + (1 − α) × delta_i`
//!
//! 3. **Accumulated OWD** — running sum of smoothed deltas forms the y-axis
//!    of the regression.  A building queue causes the accumulated OWD to rise;
//!    a draining queue causes it to fall.
//!
//! 4. **Least-squares trendline** — fit over a sliding window of
//!    `(cumulative_send_time_ms, accumulated_owd_delta_µs)` pairs.  The slope
//!    (µs per ms of elapsed send time) is the primary congestion signal.
//!
//! 5. **Overuse detection** — the slope is compared to the threshold γ
//!    (scaled by the caller's `gamma_multiplier`).  The [`BandwidthUsage`]
//!    output drives rate-control decisions in the congestion controller.
//!
//! # Integration
//!
//! ```rust,no_run
//! use lowband_lbtp::delay::{DelayGradientEstimator, BandwidthUsage};
//! use lowband_lbtp::cellular::CellularModeController;
//!
//! let mut estimator = DelayGradientEstimator::new();
//! let mut cellular = CellularModeController::new();
//!
//! // Per-ACK: feed the timestamp pair and pass the cellular gamma multiplier.
//! let (send_us, recv_us): (u64, u64) = (0, 0); // from ACK report
//! let usage = estimator.observe(send_us, recv_us, cellular.gamma_multiplier());
//!
//! // For cellular-mode increase gating, pass the estimator slope:
//! let can_up = cellular.can_increase(estimator.slope());
//! ```

use std::collections::VecDeque;

// ── Constants ────────────────────────────────────────────────────────────────

/// Number of `(send_time, accumulated_owd)` pairs kept in the regression window.
///
/// 20 samples at one ACK per 10 ms ≈ 200 ms of history — long enough to
/// distinguish a trend from noise, short enough to react within one RTT on a
/// 3G path (typically 200–400 ms).
pub const TRENDLINE_WINDOW_SIZE: usize = 20;

/// EWMA smoothing factor α for OWD deltas.
///
/// 0.9 (90 % weight on history) gives an effective window of
/// `1 / (1 − α) = 10` samples.  This filters per-packet jitter while
/// tracking a queuing-delay trend within ~100 ms.
pub const TRENDLINE_SMOOTHING_ALPHA: f64 = 0.9;

/// Overuse threshold γ in µs per ms of elapsed send time.
///
/// When the trendline slope exceeds γ the link is classified as
/// [`BandwidthUsage::Overuse`].  12.5 µs/ms ≡ 12.5 ms/s of queuing-delay
/// growth, matching the WebRTC TrendlineEstimator default and appropriate for
/// links with RTTs from 50 to 500 ms.
///
/// The caller scales this threshold at runtime via the `gamma_multiplier`
/// argument to [`DelayGradientEstimator::observe`]; the
/// [`CellularModeController`](crate::cellular::CellularModeController) doubles
/// it to avoid false overuse from RAN-scheduler spikes.
pub const OVERUSE_THRESHOLD_GAMMA_US_PER_MS: f64 = 12.5;

/// Minimum number of window samples required before a non-zero slope is computed.
///
/// A regression over a single point is undefined.
pub const MIN_WINDOW_FOR_SLOPE: usize = 2;

// ── BandwidthUsage ────────────────────────────────────────────────────────────

/// Link-usage classification produced by the overuse detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthUsage {
    /// Queuing delay is draining; the congestion controller may probe for more
    /// bandwidth.
    Underuse,
    /// Delay trend is stable; the sender may hold its rate or apply a slow
    /// additive increase.
    Normal,
    /// Queuing delay is growing; the congestion controller should reduce its
    /// send rate.
    Overuse,
}

// ── DelayGradientEstimator ────────────────────────────────────────────────────

/// OWD trendline estimator for LBTP congestion control.
///
/// One instance per active stream.  Feed `(send_time_us, recv_time_us)` pairs
/// in ACK order via [`observe`](Self::observe).
///
/// Zero heap allocation beyond the fixed-capacity sliding window.
///
/// ## Usage
///
/// ```rust,no_run
/// use lowband_lbtp::delay::{DelayGradientEstimator, BandwidthUsage};
///
/// let mut estimator = DelayGradientEstimator::new();
///
/// // Each control tick, after collecting ACKs:
/// let (send_us, recv_us): (u64, u64) = (0, 0); // from ACK report
/// let usage = estimator.observe(send_us, recv_us, 1.0 /* gamma_multiplier */);
/// let slope = estimator.slope(); // pass to CellularModeController::can_increase
/// ```
#[derive(Debug)]
pub struct DelayGradientEstimator {
    /// Sliding window of `(cumulative_send_ms, accumulated_owd_µs)` pairs.
    window: VecDeque<(f64, f64)>,
    /// EWMA of per-packet OWD deltas in µs.
    smoothed_delta_us: f64,
    /// Running sum of `smoothed_delta_us` — the regression y-axis.
    accumulated_owd_us: f64,
    /// Running sum of send-side inter-packet intervals — the regression x-axis.
    accumulated_send_ms: f64,
    /// Send timestamp of the previous ACKed packet (µs).
    prev_send_us: Option<u64>,
    /// Receive timestamp of the previous ACKed packet (µs).
    prev_recv_us: Option<u64>,
    /// Most recently computed trendline slope (µs/ms).
    slope: f64,
    /// Current link-usage classification.
    usage: BandwidthUsage,
}

impl Default for DelayGradientEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayGradientEstimator {
    /// Create a new estimator with an empty window and neutral state.
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(TRENDLINE_WINDOW_SIZE + 1),
            smoothed_delta_us: 0.0,
            accumulated_owd_us: 0.0,
            accumulated_send_ms: 0.0,
            prev_send_us: None,
            prev_recv_us: None,
            slope: 0.0,
            usage: BandwidthUsage::Normal,
        }
    }

    /// Feed an ACKed-packet timestamp pair and update the congestion estimate.
    ///
    /// Returns the updated [`BandwidthUsage`] classification.
    ///
    /// # Arguments
    ///
    /// * `send_time_us` — the packet's send timestamp in microseconds.
    /// * `recv_time_us` — the packet's receive timestamp in microseconds
    ///   (from the receiver's clock, corrected for clock offset when available).
    /// * `gamma_multiplier` — overuse-threshold multiplier from
    ///   [`CellularModeController::gamma_multiplier()`](crate::cellular::CellularModeController::gamma_multiplier);
    ///   pass `1.0` outside cellular mode, `2.0` inside, to prevent
    ///   RAN-scheduler spikes from declaring spurious overuse.
    pub fn observe(
        &mut self,
        send_time_us: u64,
        recv_time_us: u64,
        gamma_multiplier: f64,
    ) -> BandwidthUsage {
        if let (Some(prev_s), Some(prev_r)) = (self.prev_send_us, self.prev_recv_us) {
            let send_delta_us = send_time_us.saturating_sub(prev_s) as f64;
            let recv_delta_us = recv_time_us.saturating_sub(prev_r) as f64;
            let owd_delta_us = recv_delta_us - send_delta_us;

            self.smoothed_delta_us = TRENDLINE_SMOOTHING_ALPHA * self.smoothed_delta_us
                + (1.0 - TRENDLINE_SMOOTHING_ALPHA) * owd_delta_us;

            self.accumulated_send_ms += send_delta_us / 1_000.0;
            self.accumulated_owd_us += self.smoothed_delta_us;

            self.window
                .push_back((self.accumulated_send_ms, self.accumulated_owd_us));
            if self.window.len() > TRENDLINE_WINDOW_SIZE {
                self.window.pop_front();
            }

            self.slope = self.compute_slope();

            let gamma = OVERUSE_THRESHOLD_GAMMA_US_PER_MS * gamma_multiplier.max(0.0);
            self.usage = if self.slope > gamma {
                BandwidthUsage::Overuse
            } else if self.slope < -gamma {
                BandwidthUsage::Underuse
            } else {
                BandwidthUsage::Normal
            };
        }

        self.prev_send_us = Some(send_time_us);
        self.prev_recv_us = Some(recv_time_us);
        self.usage
    }

    /// Trendline slope in µs per ms of elapsed send time.
    ///
    /// - **Positive**: queuing delay is growing (potential overuse).
    /// - **Negative**: queuing delay is draining (underuse).
    /// - **Zero**: queue is stable or fewer than [`MIN_WINDOW_FOR_SLOPE`] samples
    ///   have been observed.
    ///
    /// Pass this value to
    /// [`CellularModeController::can_increase`](crate::cellular::CellularModeController::can_increase)
    /// to gate rate increases while the RAN scheduler is spiking.
    pub fn slope(&self) -> f64 {
        self.slope
    }

    /// Current link-usage classification.
    pub fn bandwidth_usage(&self) -> BandwidthUsage {
        self.usage
    }

    /// Number of `(send_time, accumulated_owd)` pairs currently in the window.
    ///
    /// Reaches [`TRENDLINE_WINDOW_SIZE`] after the window is full and stays
    /// there; older entries are evicted as new ones arrive.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    /// Compute the least-squares slope over the current window.
    ///
    /// Uses the standard closed-form: `Σ(x−x̄)(y−ȳ) / Σ(x−x̄)²`.
    /// Returns `0.0` when the window has fewer than [`MIN_WINDOW_FOR_SLOPE`]
    /// entries or the x-variance is negligible (degenerate: all same send time).
    fn compute_slope(&self) -> f64 {
        let n = self.window.len();
        if n < MIN_WINDOW_FOR_SLOPE {
            return 0.0;
        }

        let n_f = n as f64;
        let x_mean: f64 = self.window.iter().map(|(x, _)| x).sum::<f64>() / n_f;
        let y_mean: f64 = self.window.iter().map(|(_, y)| y).sum::<f64>() / n_f;

        let numerator: f64 = self
            .window
            .iter()
            .map(|(x, y)| (x - x_mean) * (y - y_mean))
            .sum();
        let denominator: f64 = self
            .window
            .iter()
            .map(|(x, _)| (x - x_mean).powi(2))
            .sum();

        if denominator < 1e-10 {
            0.0
        } else {
            numerator / denominator
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Send interval used across all tests: 10 ms between packets.
    const SEND_INTERVAL_US: u64 = 10_000;
    /// Baseline OWD for the synthetic packet stream.
    const BASE_OWD_US: i64 = 80_000; // 80 ms

    /// Feed `n` synthetic packets into `estimator`.
    ///
    /// Each packet is sent `SEND_INTERVAL_US` after the previous one.
    /// The one-way delay starts at `BASE_OWD_US` and changes by
    /// `owd_delta_per_packet_us` each step (positive = growing, negative =
    /// draining).  Returns the [`BandwidthUsage`] from the final packet.
    fn feed_packets(
        estimator: &mut DelayGradientEstimator,
        n: usize,
        owd_delta_per_packet_us: i64,
        gamma_multiplier: f64,
    ) -> BandwidthUsage {
        let mut send_us: u64 = 0;
        let mut current_owd_us: i64 = BASE_OWD_US;
        let mut result = BandwidthUsage::Normal;

        for _ in 0..n {
            send_us += SEND_INTERVAL_US;
            let recv_us = (send_us as i64 + current_owd_us).max(0) as u64;
            result = estimator.observe(send_us, recv_us, gamma_multiplier);
            current_owd_us += owd_delta_per_packet_us;
        }
        result
    }

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn starts_with_normal_usage_zero_slope_and_empty_window() {
        let est = DelayGradientEstimator::new();
        assert_eq!(est.bandwidth_usage(), BandwidthUsage::Normal);
        assert_eq!(est.slope(), 0.0);
        assert_eq!(est.window_len(), 0);
    }

    #[test]
    fn first_observe_returns_normal_and_does_not_add_to_window() {
        let mut est = DelayGradientEstimator::new();
        // First packet only sets the "prev" timestamps — no delta to compute.
        let usage = est.observe(SEND_INTERVAL_US, BASE_OWD_US as u64 + SEND_INTERVAL_US, 1.0);
        assert_eq!(
            usage,
            BandwidthUsage::Normal,
            "first packet cannot trigger overuse: no previous sample exists"
        );
        assert_eq!(
            est.window_len(),
            0,
            "first packet must not add an entry to the regression window"
        );
    }

    // ── Slope sign ───────────────────────────────────────────────────────────

    #[test]
    fn slope_near_zero_on_constant_owd() {
        let mut est = DelayGradientEstimator::new();
        // 80 packets, no OWD variation: every recv_delta == send_delta.
        feed_packets(&mut est, 80, 0, 1.0);
        assert!(
            est.slope().abs() < 1.0,
            "slope must be near zero for constant OWD; got {:.4}",
            est.slope()
        );
    }

    #[test]
    fn slope_positive_on_growing_owd() {
        let mut est = DelayGradientEstimator::new();
        // OWD grows 1 ms per packet: recv_delta = 11 ms, send_delta = 10 ms.
        // After EWMA warmup the slope converges to ≈ 1 000 µs / 10 ms = 100 µs/ms.
        feed_packets(&mut est, 80, 1_000, 1.0);
        assert!(
            est.slope() > 0.0,
            "slope must be positive when OWD grows; got {:.4}",
            est.slope()
        );
    }

    #[test]
    fn slope_negative_on_draining_owd() {
        let mut est = DelayGradientEstimator::new();
        // OWD shrinks 1 ms per packet for 60 steps (baseline 80 ms stays positive).
        feed_packets(&mut est, 60, -1_000, 1.0);
        assert!(
            est.slope() < 0.0,
            "slope must be negative when OWD drains; got {:.4}",
            est.slope()
        );
    }

    // ── BandwidthUsage classification ─────────────────────────────────────────

    #[test]
    fn overuse_declared_on_growing_owd() {
        let mut est = DelayGradientEstimator::new();
        // slope ≈ 100 µs/ms >> OVERUSE_THRESHOLD_GAMMA_US_PER_MS (12.5).
        let usage = feed_packets(&mut est, 80, 1_000, 1.0);
        assert_eq!(
            usage,
            BandwidthUsage::Overuse,
            "strongly growing OWD must classify as Overuse"
        );
    }

    #[test]
    fn underuse_declared_on_draining_owd() {
        let mut est = DelayGradientEstimator::new();
        // slope ≈ -100 µs/ms << -OVERUSE_THRESHOLD_GAMMA_US_PER_MS (-12.5).
        let usage = feed_packets(&mut est, 60, -1_000, 1.0);
        assert_eq!(
            usage,
            BandwidthUsage::Underuse,
            "strongly draining OWD must classify as Underuse"
        );
    }

    #[test]
    fn normal_declared_on_constant_owd() {
        let mut est = DelayGradientEstimator::new();
        let usage = feed_packets(&mut est, 80, 0, 1.0);
        assert_eq!(
            usage,
            BandwidthUsage::Normal,
            "constant OWD must classify as Normal"
        );
    }

    // ── Gamma multiplier ──────────────────────────────────────────────────────

    #[test]
    fn gamma_multiplier_widens_threshold_to_suppress_cellular_overuse() {
        // OWD grows 0.2 ms per 10 ms send interval.
        // After EWMA convergence: slope ≈ 200 µs / 10 ms = 20 µs/ms.
        //   With gamma = 1.0: 20 > 12.5 → Overuse
        //   With gamma = 2.0: 20 < 25   → not Overuse (Normal or Underuse)
        let mut est_default = DelayGradientEstimator::new();
        let mut est_cellular = DelayGradientEstimator::new();

        let usage_default = feed_packets(&mut est_default, 80, 200, 1.0);
        let usage_cellular = feed_packets(&mut est_cellular, 80, 200, 2.0);

        assert_eq!(
            usage_default,
            BandwidthUsage::Overuse,
            "borderline slope should be Overuse with default gamma"
        );
        assert_ne!(
            usage_cellular,
            BandwidthUsage::Overuse,
            "same slope must not be Overuse when gamma is doubled (cellular mode)"
        );
    }

    // ── Window management ─────────────────────────────────────────────────────

    #[test]
    fn window_fills_and_caps_at_window_size() {
        let mut est = DelayGradientEstimator::new();
        // First packet sets prev; each subsequent packet adds one window entry.
        feed_packets(&mut est, TRENDLINE_WINDOW_SIZE + 10, 0, 1.0);
        assert_eq!(
            est.window_len(),
            TRENDLINE_WINDOW_SIZE,
            "window must not exceed TRENDLINE_WINDOW_SIZE"
        );
    }

    #[test]
    fn slope_zero_with_single_window_sample() {
        let mut est = DelayGradientEstimator::new();
        // Packet 1: sets prev, adds nothing to window.
        est.observe(SEND_INTERVAL_US, BASE_OWD_US as u64 + SEND_INTERVAL_US, 1.0);
        // Packet 2: adds first window entry (len=1 < MIN_WINDOW_FOR_SLOPE=2).
        est.observe(2 * SEND_INTERVAL_US, BASE_OWD_US as u64 + 2 * SEND_INTERVAL_US, 1.0);

        assert_eq!(est.window_len(), 1);
        assert_eq!(
            est.slope(),
            0.0,
            "slope must be zero when fewer than MIN_WINDOW_FOR_SLOPE samples exist"
        );
    }

    // ── Default / structural ──────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        let a = DelayGradientEstimator::new();
        let b = DelayGradientEstimator::default();
        assert_eq!(a.slope().to_bits(), b.slope().to_bits());
        assert_eq!(a.bandwidth_usage(), b.bandwidth_usage());
        assert_eq!(a.window_len(), b.window_len());
    }
}

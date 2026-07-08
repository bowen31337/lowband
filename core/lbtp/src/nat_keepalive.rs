//! NAT keepalive timer — Feature 4.
//!
//! # Mechanism
//!
//! Consumer NAT devices typically time out idle UDP bindings after 30–60 s.
//! To keep a binding alive for the lifetime of the session, the transport
//! sends a small keepalive datagram before any binding can expire.
//!
//! [`NatKeepaliveController`] counts down a caller-chosen interval and emits
//! [`KeepaliveEvent::Keepalive`] when the deadline arrives.  The interval is
//! constrained to [`NAT_KEEPALIVE_MIN_TICKS`]–[`NAT_KEEPALIVE_MAX_TICKS`]
//! (15–25 s at the nominal 10 Hz control rate).  The caller supplies a fresh
//! jittered interval on each [`reset`](NatKeepaliveController::reset) call,
//! spreading keepalives from concurrent sessions across the window so they
//! do not synchronise into a burst on the TURN / STUN infrastructure.
//!
//! After [`tick`](NatKeepaliveController::tick) emits `Keepalive` the
//! controller disarms itself.  The caller must call `reset` with the next
//! jittered interval — typically drawn from a CSPRNG in the transport event
//! loop — before ticking again.
//!
//! # Tick semantics
//!
//! [`tick`](NatKeepaliveController::tick) fires after exactly
//! `interval_ticks` calls, where `interval_ticks` is the value passed to
//! [`new`](NatKeepaliveController::new) or the most recent
//! [`reset`](NatKeepaliveController::reset).  In a disarmed state
//! (immediately after construction with an uninitialized interval, or while
//! waiting for `reset` after a keepalive has fired) `tick` returns `None`.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::nat_keepalive::{
//!     NatKeepaliveController, KeepaliveEvent,
//!     NAT_KEEPALIVE_MIN_TICKS, NAT_KEEPALIVE_MAX_TICKS,
//! };
//!
//! // Event loop: pick an initial jittered interval within [MIN, MAX].
//! // In production the interval comes from a CSPRNG; here we use the midpoint.
//! let initial_interval = (NAT_KEEPALIVE_MIN_TICKS + NAT_KEEPALIVE_MAX_TICKS) / 2;
//! let mut ctrl = NatKeepaliveController::new(initial_interval);
//!
//! // On each 10 Hz control tick:
//! // if let Some(KeepaliveEvent::Keepalive) = ctrl.tick() {
//! //     send_nat_keepalive_datagram();
//! //     let next = rng.gen_range(NAT_KEEPALIVE_MIN_TICKS..=NAT_KEEPALIVE_MAX_TICKS);
//! //     ctrl.reset(next);
//! // }
//! ```

// ── Constants ─────────────────────────────────────────────────────────────────

/// Minimum keepalive interval in control ticks.
///
/// At the nominal 10 Hz control rate, 150 ticks = 15 seconds — the lower
/// bound of the 15–25 s jitter window.  Passing a smaller value to
/// [`NatKeepaliveController::new`] or [`NatKeepaliveController::reset`]
/// clamps to this floor.
pub const NAT_KEEPALIVE_MIN_TICKS: u32 = 150;

/// Maximum keepalive interval in control ticks.
///
/// At the nominal 10 Hz control rate, 250 ticks = 25 seconds — the upper
/// bound of the 15–25 s jitter window.  Passing a larger value to
/// [`NatKeepaliveController::new`] or [`NatKeepaliveController::reset`]
/// clamps to this ceiling.
pub const NAT_KEEPALIVE_MAX_TICKS: u32 = 250;

// ── KeepaliveEvent ────────────────────────────────────────────────────────────

/// Event emitted by [`NatKeepaliveController::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepaliveEvent {
    /// Keepalive interval elapsed; send a NAT keepalive datagram now.
    ///
    /// After receiving this event the caller must call
    /// [`NatKeepaliveController::reset`] with the next jittered interval
    /// to re-arm the timer.
    Keepalive,
}

// ── NatKeepaliveController ────────────────────────────────────────────────────

/// NAT keepalive timer controller (Feature 4).
///
/// Counts down a caller-chosen interval in the range
/// [`NAT_KEEPALIVE_MIN_TICKS`]–[`NAT_KEEPALIVE_MAX_TICKS`] and emits
/// [`KeepaliveEvent::Keepalive`] when the deadline arrives.  After firing
/// the controller disarms itself until [`reset`](Self::reset) is called with
/// the next jittered interval.
///
/// One instance per active path.  Cheap to construct; zero heap allocation.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct NatKeepaliveController {
    /// Countdown. `0` means disarmed (waiting for `reset` or never armed).
    ticks_remaining: u32,
}

impl NatKeepaliveController {
    /// Create a controller armed with `interval_ticks`.
    ///
    /// `interval_ticks` is clamped to
    /// [`NAT_KEEPALIVE_MIN_TICKS`]–[`NAT_KEEPALIVE_MAX_TICKS`].
    pub fn new(interval_ticks: u32) -> Self {
        Self {
            ticks_remaining: interval_ticks
                .clamp(NAT_KEEPALIVE_MIN_TICKS, NAT_KEEPALIVE_MAX_TICKS),
        }
    }

    /// Advance the keepalive timer by one control tick.
    ///
    /// Returns `Some(KeepaliveEvent::Keepalive)` after exactly
    /// `interval_ticks` calls; thereafter returns `None` until
    /// [`reset`](Self::reset) re-arms the timer.
    ///
    /// Returns `None` immediately when the controller is disarmed
    /// (`ticks_remaining == 0`).
    pub fn tick(&mut self) -> Option<KeepaliveEvent> {
        if self.ticks_remaining == 0 {
            return None;
        }
        self.ticks_remaining -= 1;
        if self.ticks_remaining == 0 {
            Some(KeepaliveEvent::Keepalive)
        } else {
            None
        }
    }

    /// Re-arm the timer with a new (typically jittered) interval.
    ///
    /// Call after receiving [`KeepaliveEvent::Keepalive`] to schedule the
    /// next keepalive.  In the transport event loop the next interval is
    /// drawn from a CSPRNG in the range
    /// `NAT_KEEPALIVE_MIN_TICKS..=NAT_KEEPALIVE_MAX_TICKS`.
    ///
    /// `interval_ticks` is clamped to
    /// [`NAT_KEEPALIVE_MIN_TICKS`]–[`NAT_KEEPALIVE_MAX_TICKS`].
    pub fn reset(&mut self, interval_ticks: u32) {
        self.ticks_remaining = interval_ticks
            .clamp(NAT_KEEPALIVE_MIN_TICKS, NAT_KEEPALIVE_MAX_TICKS);
    }

    /// Remaining ticks until the next keepalive, or `0` when disarmed.
    pub fn ticks_remaining(&self) -> u32 {
        self.ticks_remaining
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_arms_with_given_interval() {
        let ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MIN_TICKS);
    }

    #[test]
    fn new_clamps_below_minimum_to_min() {
        let ctrl = NatKeepaliveController::new(0);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MIN_TICKS);
    }

    #[test]
    fn new_clamps_above_maximum_to_max() {
        let ctrl = NatKeepaliveController::new(u32::MAX);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MAX_TICKS);
    }

    #[test]
    fn new_with_min_ticks_is_valid() {
        let ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MIN_TICKS);
    }

    #[test]
    fn new_with_max_ticks_is_valid() {
        let ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MAX_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MAX_TICKS);
    }

    // ── tick(): countdown and fire ────────────────────────────────────────────

    #[test]
    fn tick_returns_none_before_interval_expires() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        for tick in 0..NAT_KEEPALIVE_MIN_TICKS - 1 {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None before interval expires"
            );
        }
    }

    #[test]
    fn tick_fires_exactly_at_interval_min() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        // Advance to just before the deadline.
        for _ in 0..NAT_KEEPALIVE_MIN_TICKS - 1 {
            ctrl.tick();
        }
        assert_eq!(
            ctrl.tick(),
            Some(KeepaliveEvent::Keepalive),
            "keepalive must fire on tick {NAT_KEEPALIVE_MIN_TICKS}"
        );
    }

    #[test]
    fn tick_fires_exactly_at_interval_max() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MAX_TICKS);
        for _ in 0..NAT_KEEPALIVE_MAX_TICKS - 1 {
            ctrl.tick();
        }
        assert_eq!(
            ctrl.tick(),
            Some(KeepaliveEvent::Keepalive),
            "keepalive must fire on tick {NAT_KEEPALIVE_MAX_TICKS}"
        );
    }

    #[test]
    fn tick_fires_exactly_at_midpoint_interval() {
        let mid = (NAT_KEEPALIVE_MIN_TICKS + NAT_KEEPALIVE_MAX_TICKS) / 2;
        let mut ctrl = NatKeepaliveController::new(mid);
        for _ in 0..mid - 1 {
            ctrl.tick();
        }
        assert_eq!(ctrl.tick(), Some(KeepaliveEvent::Keepalive));
    }

    // ── tick(): disarmed state after firing ───────────────────────────────────

    #[test]
    fn tick_returns_none_after_firing_until_reset() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        drain_to_keepalive(&mut ctrl, NAT_KEEPALIVE_MIN_TICKS);

        // Controller is now disarmed; further ticks must be no-ops.
        for extra_tick in 0..10 {
            assert!(
                ctrl.tick().is_none(),
                "tick {extra_tick} after keepalive: must return None until reset"
            );
        }
        assert_eq!(ctrl.ticks_remaining(), 0, "disarmed state is ticks_remaining == 0");
    }

    // ── reset() ───────────────────────────────────────────────────────────────

    #[test]
    fn reset_re_arms_with_new_interval() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        drain_to_keepalive(&mut ctrl, NAT_KEEPALIVE_MIN_TICKS);

        // Reset with a different interval.
        ctrl.reset(NAT_KEEPALIVE_MAX_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MAX_TICKS);
    }

    #[test]
    fn reset_clamps_below_minimum() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        ctrl.reset(0);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MIN_TICKS);
    }

    #[test]
    fn reset_clamps_above_maximum() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        ctrl.reset(u32::MAX);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MAX_TICKS);
    }

    #[test]
    fn reset_accepts_minimum_interval() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MAX_TICKS);
        ctrl.reset(NAT_KEEPALIVE_MIN_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MIN_TICKS);
    }

    #[test]
    fn reset_accepts_maximum_interval() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        ctrl.reset(NAT_KEEPALIVE_MAX_TICKS);
        assert_eq!(ctrl.ticks_remaining(), NAT_KEEPALIVE_MAX_TICKS);
    }

    // ── Repeated keepalives ───────────────────────────────────────────────────

    #[test]
    fn keepalive_fires_repeatedly_with_min_interval() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        for round in 0..3 {
            drain_to_keepalive(&mut ctrl, NAT_KEEPALIVE_MIN_TICKS);
            ctrl.reset(NAT_KEEPALIVE_MIN_TICKS);
            assert_eq!(
                ctrl.ticks_remaining(),
                NAT_KEEPALIVE_MIN_TICKS,
                "round {round}: must be re-armed after reset"
            );
        }
    }

    #[test]
    fn keepalive_fires_repeatedly_with_max_interval() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MAX_TICKS);
        for round in 0..3 {
            drain_to_keepalive(&mut ctrl, NAT_KEEPALIVE_MAX_TICKS);
            ctrl.reset(NAT_KEEPALIVE_MAX_TICKS);
            assert_eq!(
                ctrl.ticks_remaining(),
                NAT_KEEPALIVE_MAX_TICKS,
                "round {round}: must be re-armed after reset"
            );
        }
    }

    #[test]
    fn keepalive_fires_with_alternating_intervals() {
        let intervals = [
            NAT_KEEPALIVE_MIN_TICKS,
            NAT_KEEPALIVE_MAX_TICKS,
            NAT_KEEPALIVE_MIN_TICKS + 30, // midpoint-ish
            NAT_KEEPALIVE_MAX_TICKS - 10,
        ];
        let mut ctrl = NatKeepaliveController::new(intervals[0]);
        for (round, &interval) in intervals.iter().enumerate() {
            if round > 0 {
                ctrl.reset(interval);
            }
            let event = drain_to_keepalive(&mut ctrl, interval);
            assert_eq!(
                event,
                KeepaliveEvent::Keepalive,
                "round {round}: keepalive must fire after {interval} ticks"
            );
        }
    }

    // ── ticks_remaining() ─────────────────────────────────────────────────────

    #[test]
    fn ticks_remaining_decrements_each_tick() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        for expected in (1..NAT_KEEPALIVE_MIN_TICKS).rev() {
            ctrl.tick();
            assert_eq!(ctrl.ticks_remaining(), expected);
        }
    }

    #[test]
    fn ticks_remaining_zero_after_keepalive_fires() {
        let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
        drain_to_keepalive(&mut ctrl, NAT_KEEPALIVE_MIN_TICKS);
        assert_eq!(ctrl.ticks_remaining(), 0);
    }

    // ── Constants sanity ──────────────────────────────────────────────────────

    #[test]
    fn min_ticks_corresponds_to_15_seconds_at_10_hz() {
        assert_eq!(NAT_KEEPALIVE_MIN_TICKS, 150, "150 ticks × 100 ms = 15 s at 10 Hz");
    }

    #[test]
    fn max_ticks_corresponds_to_25_seconds_at_10_hz() {
        assert_eq!(NAT_KEEPALIVE_MAX_TICKS, 250, "250 ticks × 100 ms = 25 s at 10 Hz");
    }

    #[test]
    fn min_ticks_less_than_max_ticks() {
        assert!(NAT_KEEPALIVE_MIN_TICKS < NAT_KEEPALIVE_MAX_TICKS);
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Advance the controller for `n` ticks, asserting that the last tick
    /// returns `KeepaliveEvent::Keepalive` and all earlier ones return `None`.
    fn drain_to_keepalive(ctrl: &mut NatKeepaliveController, n: u32) -> KeepaliveEvent {
        assert!(n >= 1, "interval must be at least 1 tick");
        for tick in 0..n - 1 {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick} of {n}: must return None before the deadline"
            );
        }
        ctrl.tick()
            .expect("final tick must return KeepaliveEvent::Keepalive")
    }
}

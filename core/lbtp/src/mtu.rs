//! Path MTU Discovery controller — Feature 8.
//!
//! # Mechanism
//!
//! LBTP defaults to a 1 200-byte datagram ceiling (Feature 7) to survive the
//! worst-case path.  This module implements opportunistic Path MTU Discovery
//! (PMTUD) by sending progressively larger padding frames on channel 8
//! (probes, first-to-drop / Realtime delivery class) and raising `path_mtu`
//! whenever the peer acknowledges the probe.
//!
//! 1. The caller invokes [`PathMtuController::start_probe`].  The controller
//!    moves to `Probing` and returns an [`MtuProbeFrame`] the transport enqueues
//!    on channel 8.
//!
//! 2. Each [`PathMtuController::tick`] decrements the per-attempt timer.
//!    On timeout with retries remaining it returns
//!    [`MtuEvent::Retransmit`] — the same probe is re-sent and the timer
//!    resets.  Once all retries are exhausted the controller returns to `Idle`
//!    and emits [`MtuEvent::ProbeFailed`].
//!
//! 3. When the peer acknowledges the probe, the transport calls
//!    [`PathMtuController::on_probe_acked`].  A matching size causes the
//!    controller to raise `path_mtu`, advance the step pointer, and return
//!    [`MtuEvent::MtuRaised`].
//!
//! # Probe ladder
//!
//! ```text
//! MTU_BASE_BYTES (1200) → probe 1400 → probe 1452 → probe 1500 → Complete
//! ```
//!
//! Each successful probe raises `path_mtu` to the confirmed size.  A failed
//! probe leaves `path_mtu` unchanged and the controller returns to `Idle`;
//! the caller may schedule a later retry or treat the current value as the
//! ceiling.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::mtu::{
//!     PathMtuController, MtuEvent, MTU_BASE_BYTES,
//! };
//!
//! let mut ctrl = PathMtuController::new();
//! assert_eq!(ctrl.path_mtu(), MTU_BASE_BYTES);
//!
//! // Begin probing the first step (1400 bytes).
//! let probe = ctrl.start_probe().unwrap();
//! // → enqueue `vec![0u8; probe.payload_bytes as usize]` on channel 8 …
//!
//! // On receiving an ack from the peer for this probe size:
//! let event = ctrl.on_probe_acked(probe.target_mtu).unwrap();
//! assert_eq!(event, MtuEvent::MtuRaised { new_mtu: 1400 });
//! assert_eq!(ctrl.path_mtu(), 1400);
//! ```

/// The base LBTP datagram size used before any probe succeeds.
///
/// This conservative floor survives the worst-case network path and matches
/// the QUIC minimum PMTU (RFC 9000 §14.1).
pub const MTU_BASE_BYTES: u16 = 1200;

/// Candidate datagram sizes probed in ascending order.
///
/// Each element is the total LBTP datagram size (19-byte overhead + frame
/// payload bytes) as seen at the UDP-payload layer.  Steps map to common
/// link-layer MTU landmarks:
///
/// - 1 400 B: conservative first step (GRE/VPN tunnels, many cellular uplinks).
/// - 1 452 B: PPPoE over Ethernet (1 500 − 20 IP − 8 UDP − 8 PPPoE header −
///   12 B additional encap = ~1 452 B available).
/// - 1 500 B: standard IEEE 802.3 Ethernet.
pub const MTU_PROBE_STEPS: [u16; 3] = [1400, 1452, 1500];

/// Ticks between successive probe attempts.
///
/// At the nominal 10 Hz controller rate, 10 ticks ≈ 1 second per attempt —
/// sufficient for high-latency paths (satellite RTT ≈ 600 ms).
pub const MTU_PROBE_TIMEOUT_TICKS: u32 = 10;

/// Maximum retransmissions per probe size before declaring it unreachable.
///
/// 3 retries × ≈1 s = up to ≈4 s of probing per step, matching the QUIC
/// DPLPMTUD recommendation (RFC 8899 §5.1.2).
pub const MTU_PROBE_MAX_RETRIES: u8 = 3;

/// LBTP datagram overhead: 3-byte envelope (short form) + 16-byte AEAD tag.
///
/// Mirrors `DATAGRAM_OVERHEAD` in `pacer.rs` (private there).  The probe
/// payload pushed on channel 8 is `target_mtu − LBTP_OVERHEAD` bytes.
const LBTP_OVERHEAD: u16 = 19;

/// A probe frame the transport must enqueue on LBTP channel 8 (probes / padding).
///
/// The transport creates a `PacerFrame` with `channel = ChannelId(8)` and
/// `data = vec![0u8; payload_bytes as usize]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtuProbeFrame {
    /// Total LBTP datagram size being probed (bytes at the UDP-payload layer).
    pub target_mtu: u16,
    /// Padding bytes the transport places in the channel-8 frame.
    ///
    /// Equals `target_mtu − 19` (LBTP overhead).
    pub payload_bytes: u16,
}

/// Events emitted by [`PathMtuController::tick`] and
/// [`PathMtuController::on_probe_acked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MtuEvent {
    /// The previous attempt timed out and retries remain — re-send this probe.
    Retransmit(MtuProbeFrame),
    /// The probe was acknowledged; `path_mtu` has been raised to `new_mtu`.
    MtuRaised { new_mtu: u16 },
    /// All retries exhausted for `target_mtu`; `path_mtu` is unchanged.
    ProbeFailed { target_mtu: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MtuState {
    Idle { next_step_idx: usize },
    Probing {
        target_mtu: u16,
        step_idx: usize,
        retries_left: u8,
        ticks_remaining: u32,
    },
    Complete,
}

/// Path MTU Discovery controller (Feature 8).
///
/// One instance per active session.  Zero heap allocation.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct PathMtuController {
    path_mtu: u16,
    state: MtuState,
}

impl Default for PathMtuController {
    fn default() -> Self {
        Self::new()
    }
}

impl PathMtuController {
    /// Create a new controller at [`MTU_BASE_BYTES`], ready to probe step 0.
    pub fn new() -> Self {
        Self {
            path_mtu: MTU_BASE_BYTES,
            state: MtuState::Idle { next_step_idx: 0 },
        }
    }

    /// The currently confirmed `path_mtu` in bytes (UDP-payload datagram size).
    pub fn path_mtu(&self) -> u16 {
        self.path_mtu
    }

    /// Whether all probe steps have been successfully climbed.
    pub fn is_complete(&self) -> bool {
        matches!(self.state, MtuState::Complete)
    }

    /// Whether a probe is currently in flight.
    pub fn is_probing(&self) -> bool {
        matches!(self.state, MtuState::Probing { .. })
    }

    /// Begin probing the next candidate MTU size.
    ///
    /// Returns the [`MtuProbeFrame`] the transport enqueues on channel 8, or
    /// `None` when all steps have already been probed (complete ladder or
    /// after a `ProbeFailed` at the last step caused `Idle` with no successor).
    ///
    /// Calling `start_probe` while a probe is already in flight re-arms the
    /// same target (resets timer and retries; does not advance the step).
    pub fn start_probe(&mut self) -> Option<MtuProbeFrame> {
        let next_step_idx = match self.state {
            MtuState::Idle { next_step_idx } => next_step_idx,
            MtuState::Probing { step_idx, .. } => step_idx,
            MtuState::Complete => return None,
        };

        let Some(&target_mtu) = MTU_PROBE_STEPS.get(next_step_idx) else {
            self.state = MtuState::Complete;
            return None;
        };

        self.state = MtuState::Probing {
            target_mtu,
            step_idx: next_step_idx,
            retries_left: MTU_PROBE_MAX_RETRIES,
            ticks_remaining: MTU_PROBE_TIMEOUT_TICKS,
        };
        Some(make_probe(target_mtu))
    }

    /// Advance the probe timer by one tick.
    ///
    /// Returns:
    /// - `Some(MtuEvent::Retransmit(frame))` — the attempt timed out and
    ///   retries remain; re-send `frame` on channel 8.
    /// - `Some(MtuEvent::ProbeFailed { target_mtu })` — all retries exhausted;
    ///   `path_mtu` is unchanged, controller returns to `Idle`.
    /// - `None` — the timer is still running, or no probe is active.
    pub fn tick(&mut self) -> Option<MtuEvent> {
        let (target_mtu, step_idx, retries_left) = match &mut self.state {
            MtuState::Probing {
                target_mtu,
                step_idx,
                retries_left,
                ticks_remaining,
            } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                (*target_mtu, *step_idx, *retries_left)
            }
            _ => return None,
        };

        if retries_left == 0 {
            // All retries exhausted — return to Idle without advancing the step.
            self.state = MtuState::Idle { next_step_idx: step_idx };
            return Some(MtuEvent::ProbeFailed { target_mtu });
        }

        self.state = MtuState::Probing {
            target_mtu,
            step_idx,
            retries_left: retries_left - 1,
            ticks_remaining: MTU_PROBE_TIMEOUT_TICKS,
        };
        Some(MtuEvent::Retransmit(make_probe(target_mtu)))
    }

    /// Process a probe acknowledgement from the peer.
    ///
    /// If `acked_mtu` matches the outstanding probe size, raises `path_mtu`,
    /// advances the step pointer, and transitions to `Idle` (or `Complete` if
    /// the last step succeeded).  Returns `Some(MtuEvent::MtuRaised)`.
    ///
    /// Returns `None` when:
    /// - No probe is active.
    /// - `acked_mtu` does not match the outstanding probe size (ignored; the
    ///   in-flight probe remains active).
    pub fn on_probe_acked(&mut self, acked_mtu: u16) -> Option<MtuEvent> {
        let (target_mtu, step_idx) = match self.state {
            MtuState::Probing {
                target_mtu,
                step_idx,
                ..
            } => (target_mtu, step_idx),
            _ => return None,
        };

        if acked_mtu != target_mtu {
            return None;
        }

        self.path_mtu = target_mtu;
        let next_idx = step_idx + 1;
        self.state = if next_idx >= MTU_PROBE_STEPS.len() {
            MtuState::Complete
        } else {
            MtuState::Idle { next_step_idx: next_idx }
        };
        Some(MtuEvent::MtuRaised { new_mtu: target_mtu })
    }
}

fn make_probe(target_mtu: u16) -> MtuProbeFrame {
    MtuProbeFrame {
        target_mtu,
        payload_bytes: target_mtu.saturating_sub(LBTP_OVERHEAD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn new_controller_starts_at_base_mtu() {
        let ctrl = PathMtuController::new();
        assert_eq!(ctrl.path_mtu(), MTU_BASE_BYTES);
    }

    #[test]
    fn new_controller_is_not_probing() {
        let ctrl = PathMtuController::new();
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_complete());
    }

    #[test]
    fn default_equals_new() {
        let a = PathMtuController::new();
        let b = PathMtuController::default();
        assert_eq!(a.path_mtu(), b.path_mtu());
        assert_eq!(a.is_probing(), b.is_probing());
        assert_eq!(a.is_complete(), b.is_complete());
    }

    // ── start_probe() ────────────────────────────────────────────────────

    #[test]
    fn start_probe_returns_first_step_frame() {
        let mut ctrl = PathMtuController::new();
        let probe = ctrl.start_probe().unwrap();
        assert_eq!(probe.target_mtu, MTU_PROBE_STEPS[0]);
    }

    #[test]
    fn probe_frame_payload_bytes_excludes_overhead() {
        let mut ctrl = PathMtuController::new();
        let probe = ctrl.start_probe().unwrap();
        assert_eq!(probe.payload_bytes, probe.target_mtu - 19);
    }

    #[test]
    fn start_probe_transitions_to_probing() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        assert!(ctrl.is_probing());
    }

    #[test]
    fn start_probe_while_probing_rearms_same_target() {
        let mut ctrl = PathMtuController::new();
        let first = ctrl.start_probe().unwrap();
        let second = ctrl.start_probe().unwrap();
        assert_eq!(first.target_mtu, second.target_mtu);
        assert!(ctrl.is_probing());
    }

    // ── tick(): timer countdown ───────────────────────────────────────────

    #[test]
    fn tick_returns_none_while_timer_running() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        for tick in 0..MTU_PROBE_TIMEOUT_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_retransmit_when_timer_expires() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);

        match ctrl.tick() {
            Some(MtuEvent::Retransmit(frame)) => {
                assert_eq!(
                    frame.target_mtu,
                    MTU_PROBE_STEPS[0],
                    "retransmit must carry the original target"
                );
            }
            other => panic!("expected Retransmit, got {other:?}"),
        }
    }

    #[test]
    fn retransmit_resets_timer_for_next_attempt() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit; timer reset.

        for tick in 0..MTU_PROBE_TIMEOUT_TICKS {
            assert!(
                ctrl.tick().is_none(),
                "tick {tick} after retransmit: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_fires_probe_failed_after_all_retries_exhausted() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        exhaust_retries(&mut ctrl);

        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        match ctrl.tick() {
            Some(MtuEvent::ProbeFailed { target_mtu }) => {
                assert_eq!(target_mtu, MTU_PROBE_STEPS[0]);
            }
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
        assert!(!ctrl.is_probing());
    }

    #[test]
    fn retransmit_count_matches_max_retries() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        let mut retransmit_count = 0u8;
        loop {
            burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
            match ctrl.tick() {
                Some(MtuEvent::Retransmit(_)) => retransmit_count += 1,
                Some(MtuEvent::ProbeFailed { .. }) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }

        assert_eq!(
            retransmit_count, MTU_PROBE_MAX_RETRIES,
            "exactly MTU_PROBE_MAX_RETRIES retransmissions before failure"
        );
    }

    #[test]
    fn tick_idle_returns_none() {
        let mut ctrl = PathMtuController::new();
        assert_eq!(ctrl.tick(), None);
    }

    #[test]
    fn tick_complete_returns_none() {
        let mut ctrl = climb_all_steps();
        for _ in 0..5 {
            assert_eq!(ctrl.tick(), None);
        }
    }

    // ── on_probe_acked(): path_mtu update ─────────────────────────────────

    #[test]
    fn on_probe_acked_raises_path_mtu() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        ctrl.on_probe_acked(MTU_PROBE_STEPS[0]).unwrap();
        assert_eq!(ctrl.path_mtu(), MTU_PROBE_STEPS[0]);
    }

    #[test]
    fn on_probe_acked_returns_mtu_raised_event() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        let event = ctrl.on_probe_acked(MTU_PROBE_STEPS[0]).unwrap();
        assert_eq!(
            event,
            MtuEvent::MtuRaised { new_mtu: MTU_PROBE_STEPS[0] }
        );
    }

    #[test]
    fn on_probe_acked_transitions_out_of_probing() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        ctrl.on_probe_acked(MTU_PROBE_STEPS[0]).unwrap();
        assert!(!ctrl.is_probing());
    }

    #[test]
    fn on_probe_acked_wrong_size_returns_none() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        let result = ctrl.on_probe_acked(1234);
        assert_eq!(result, None, "mismatched size must be ignored");
        assert!(ctrl.is_probing(), "probe must remain active after mismatch");
    }

    #[test]
    fn on_probe_acked_idle_returns_none() {
        let mut ctrl = PathMtuController::new();
        assert_eq!(ctrl.on_probe_acked(MTU_PROBE_STEPS[0]), None);
    }

    #[test]
    fn on_probe_acked_complete_returns_none() {
        let mut ctrl = climb_all_steps();
        assert_eq!(ctrl.on_probe_acked(MTU_PROBE_STEPS[0]), None);
    }

    #[test]
    fn on_probe_acked_after_failure_returns_none() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        exhaust_retries(&mut ctrl);
        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // ProbeFailed — back to Idle

        assert_eq!(ctrl.on_probe_acked(MTU_PROBE_STEPS[0]), None);
    }

    // ── Ladder progression ────────────────────────────────────────────────

    #[test]
    fn probing_all_steps_reaches_complete_and_max_mtu() {
        let ctrl = climb_all_steps();
        assert!(ctrl.is_complete());
        assert_eq!(
            ctrl.path_mtu(),
            *MTU_PROBE_STEPS.last().unwrap(),
            "path_mtu must equal the largest probe step after full climb"
        );
    }

    #[test]
    fn start_probe_returns_none_when_complete() {
        let mut ctrl = climb_all_steps();
        assert!(ctrl.start_probe().is_none());
    }

    #[test]
    fn each_step_probes_next_larger_size() {
        let mut ctrl = PathMtuController::new();
        for (i, &expected_mtu) in MTU_PROBE_STEPS.iter().enumerate() {
            let probe = ctrl.start_probe().expect("step {i} must have a probe");
            assert_eq!(
                probe.target_mtu, expected_mtu,
                "step {i} must probe {expected_mtu}"
            );
            ctrl.on_probe_acked(expected_mtu).unwrap();
        }
        assert!(ctrl.is_complete());
    }

    #[test]
    fn path_mtu_does_not_change_on_probe_failed() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        let before = ctrl.path_mtu();
        exhaust_retries(&mut ctrl);
        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // ProbeFailed

        assert_eq!(ctrl.path_mtu(), before, "path_mtu must be unchanged after failure");
    }

    #[test]
    fn ack_accepted_mid_timer() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS / 2);

        let event = ctrl.on_probe_acked(MTU_PROBE_STEPS[0]).unwrap();
        assert_eq!(event, MtuEvent::MtuRaised { new_mtu: MTU_PROBE_STEPS[0] });
        assert!(!ctrl.is_probing());
    }

    #[test]
    fn ack_accepted_after_retransmit() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit

        let event = ctrl.on_probe_acked(MTU_PROBE_STEPS[0]).unwrap();
        assert_eq!(event, MtuEvent::MtuRaised { new_mtu: MTU_PROBE_STEPS[0] });
    }

    #[test]
    fn failed_controller_can_restart_probing_same_step() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();

        exhaust_retries(&mut ctrl);
        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // ProbeFailed

        // Caller may retry the same step later.
        let probe = ctrl.start_probe().expect("must allow retry after failure");
        assert_eq!(probe.target_mtu, MTU_PROBE_STEPS[0]);
    }

    // ── Probe frame correctness ───────────────────────────────────────────

    #[test]
    fn all_probe_step_payload_bytes_are_positive() {
        for &step in &MTU_PROBE_STEPS {
            assert!(
                step > LBTP_OVERHEAD,
                "probe step {step} must exceed overhead {LBTP_OVERHEAD}"
            );
        }
    }

    #[test]
    fn retransmit_frame_carries_same_target_mtu() {
        let mut ctrl = PathMtuController::new();
        ctrl.start_probe();
        burn_ticks(&mut ctrl, MTU_PROBE_TIMEOUT_TICKS);
        match ctrl.tick() {
            Some(MtuEvent::Retransmit(f)) => assert_eq!(f.target_mtu, MTU_PROBE_STEPS[0]),
            other => panic!("expected Retransmit, got {other:?}"),
        }
    }

    #[test]
    fn probe_steps_are_strictly_increasing() {
        let steps = MTU_PROBE_STEPS;
        for w in steps.windows(2) {
            assert!(w[1] > w[0], "probe steps must be strictly ascending");
        }
    }

    #[test]
    fn probe_steps_all_exceed_base_mtu() {
        for &step in &MTU_PROBE_STEPS {
            assert!(
                step > MTU_BASE_BYTES,
                "probe step {step} must exceed base MTU {MTU_BASE_BYTES}"
            );
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn burn_ticks(ctrl: &mut PathMtuController, n: u32) {
        for _ in 0..n {
            ctrl.tick();
        }
    }

    fn exhaust_retries(ctrl: &mut PathMtuController) {
        for _ in 0..MTU_PROBE_MAX_RETRIES {
            burn_ticks(ctrl, MTU_PROBE_TIMEOUT_TICKS);
            let event = ctrl.tick();
            assert!(
                matches!(event, Some(MtuEvent::Retransmit(_))),
                "expected Retransmit during exhaustion, got {event:?}"
            );
        }
    }

    fn climb_all_steps() -> PathMtuController {
        let mut ctrl = PathMtuController::new();
        for &step in &MTU_PROBE_STEPS {
            ctrl.start_probe().unwrap();
            ctrl.on_probe_acked(step).unwrap();
        }
        ctrl
    }
}

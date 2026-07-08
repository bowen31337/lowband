//! Paced UDP hole-punch probe controller — Feature 3.
//!
//! # Mechanism
//!
//! NATs typically block unsolicited inbound UDP datagrams.  To open a direct
//! path between two peers, each endpoint must send UDP datagrams to the
//! other's server-reflexive address; the outbound packet creates a NAT
//! mapping that admits the peer's concurrent probe.  This is the classic
//! UDP hole-punching technique.
//!
//! [`HolePunchController`] drives the local side of this exchange:
//!
//! 1. Call [`HolePunchController::start`] with a caller-chosen 4-byte nonce.
//!    The controller enters the `Probing` state and returns the first
//!    [`HolePunchProbeFrame`] to send on LBTP channel 8 (probes / realtime).
//!
//! 2. Call [`HolePunchController::tick`] once per 10 Hz control tick.
//!    When the inter-probe interval ([`HOLE_PUNCH_PROBE_INTERVAL_TICKS`])
//!    elapses it returns [`HolePunchEvent::SendProbe`] with the next frame
//!    to enqueue.  After [`HOLE_PUNCH_MAX_PROBES`] probes without a response
//!    the controller transitions to `Failed` and emits
//!    [`HolePunchEvent::Failed`].
//!
//! 3. When any datagram arrives from the peer on the candidate address, call
//!    [`HolePunchController::on_probe_received`].  The controller immediately
//!    transitions to `Connected` and returns [`HolePunchEvent::Connected`].
//!    Receiving a single datagram is sufficient proof that the NAT binding is
//!    open on both sides.
//!
//! # Pacing
//!
//! Probes are spaced [`HOLE_PUNCH_PROBE_INTERVAL_TICKS`] ticks apart
//! (5 ticks = 500 ms at the nominal 10 Hz rate), matching the initial RTO
//! recommendation in RFC 8445 §14.  This keeps probe traffic under 1 packet
//! per 500 ms per candidate pair — negligible on any path.
//!
//! Probes are sent on channel 8 (probes / realtime, first-to-drop), which
//! the pacer gives the lowest priority.  They never compete with media or
//! control traffic.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::hole_punch::{
//!     HolePunchController, HolePunchEvent,
//!     HOLE_PUNCH_PROBE_INTERVAL_TICKS, HOLE_PUNCH_MAX_PROBES,
//! };
//!
//! let nonce = 0xDEAD_BEEF_u32;
//! let mut ctrl = HolePunchController::new();
//!
//! // Start probing; send the returned frame on channel 8.
//! let first = ctrl.start(nonce);
//! // → enqueue `first.payload` on channel 8 …
//!
//! // On each 10 Hz tick:
//! // if let Some(HolePunchEvent::SendProbe(frame)) = ctrl.tick() {
//! //     enqueue(frame);
//! // }
//!
//! // When any UDP datagram arrives from the candidate address:
//! let event = ctrl.on_probe_received();
//! assert_eq!(event, Some(HolePunchEvent::Connected));
//! assert!(ctrl.is_connected());
//! ```

// ── Constants ─────────────────────────────────────────────────────────────────

/// Control ticks between consecutive hole-punch probes.
///
/// At the nominal 10 Hz control rate, 5 ticks = 500 ms per probe —
/// matching the initial RTO recommended by RFC 8445 §14 for ICE
/// connectivity checks.
pub const HOLE_PUNCH_PROBE_INTERVAL_TICKS: u32 = 5;

/// Maximum probes sent before declaring the candidate path unreachable.
///
/// 10 probes × 500 ms = 5 seconds of total probing time.  If neither
/// peer can open a direct path in 5 seconds the caller should fall back to
/// a TURN relay (Feature 5).
pub const HOLE_PUNCH_MAX_PROBES: u8 = 10;

/// Fixed byte length of a hole-punch probe payload.
///
/// 4 bytes nonce + 1 byte sequence number = 5 bytes.  Minimising probe
/// size reduces the chance that a middlebox drops it due to size heuristics.
pub const HOLE_PUNCH_PROBE_PAYLOAD_BYTES: usize = 5;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A probe frame ready to be enqueued on LBTP channel 8.
///
/// The transport creates a `PacerFrame` with `channel = ChannelId(8)` and
/// `data = frame.payload`.  The 5-byte payload carries a 4-byte session
/// nonce followed by a 1-byte sequence number, letting the receiver
/// distinguish live probes from replays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolePunchProbeFrame {
    /// Serialised probe payload: `[nonce[0..4], seq]`.
    pub payload: [u8; HOLE_PUNCH_PROBE_PAYLOAD_BYTES],
}

impl HolePunchProbeFrame {
    fn new(nonce: u32, seq: u8) -> Self {
        let n = nonce.to_le_bytes();
        Self { payload: [n[0], n[1], n[2], n[3], seq] }
    }
}

/// Events emitted by [`HolePunchController::start`],
/// [`HolePunchController::tick`], and
/// [`HolePunchController::on_probe_received`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolePunchEvent {
    /// Interval elapsed — send this probe frame on channel 8.
    SendProbe(HolePunchProbeFrame),
    /// A datagram arrived from the peer; the direct path is open.
    Connected,
    /// All probes sent with no response; the direct path is unreachable.
    Failed,
}

// ── State machine ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum HolePunchState {
    Idle,
    Probing {
        nonce: u32,
        probes_sent: u8,
        ticks_remaining: u32,
    },
    Connected,
    Failed,
}

// ── HolePunchController ───────────────────────────────────────────────────────

/// Paced UDP hole-punch probe controller (Feature 3).
///
/// One instance per candidate address pair being probed.  Cheap to
/// construct; zero heap allocation.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct HolePunchController {
    state: HolePunchState,
}

impl Default for HolePunchController {
    fn default() -> Self {
        Self::new()
    }
}

impl HolePunchController {
    /// Create a new controller in the `Idle` state.
    pub fn new() -> Self {
        Self { state: HolePunchState::Idle }
    }

    /// Begin probing with the given `nonce`.
    ///
    /// Sends the first probe immediately by returning a
    /// [`HolePunchProbeFrame`] the transport must enqueue on channel 8.
    /// The controller enters the `Probing` state and arms the inter-probe
    /// interval timer.
    ///
    /// Calling `start` while probing is already in progress resets the
    /// probe sequence with the new nonce (e.g. when the caller rotates
    /// candidates).
    pub fn start(&mut self, nonce: u32) -> HolePunchProbeFrame {
        self.state = HolePunchState::Probing {
            nonce,
            probes_sent: 1,
            ticks_remaining: HOLE_PUNCH_PROBE_INTERVAL_TICKS,
        };
        HolePunchProbeFrame::new(nonce, 0)
    }

    /// Advance the probe timer by one control tick.
    ///
    /// Returns:
    /// - `Some(HolePunchEvent::SendProbe(frame))` — interval elapsed; enqueue
    ///   `frame` on channel 8.
    /// - `Some(HolePunchEvent::Failed)` — all [`HOLE_PUNCH_MAX_PROBES`] probes
    ///   sent with no response; the path is unreachable.
    /// - `None` — timer is still running, or the controller is `Idle`,
    ///   `Connected`, or `Failed`.
    pub fn tick(&mut self) -> Option<HolePunchEvent> {
        let (nonce, probes_sent) = match &mut self.state {
            HolePunchState::Probing { nonce, probes_sent, ticks_remaining } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                (*nonce, *probes_sent)
            }
            _ => return None,
        };

        if probes_sent >= HOLE_PUNCH_MAX_PROBES {
            self.state = HolePunchState::Failed;
            return Some(HolePunchEvent::Failed);
        }

        let seq = probes_sent;
        self.state = HolePunchState::Probing {
            nonce,
            probes_sent: probes_sent + 1,
            ticks_remaining: HOLE_PUNCH_PROBE_INTERVAL_TICKS,
        };
        Some(HolePunchEvent::SendProbe(HolePunchProbeFrame::new(nonce, seq)))
    }

    /// Notify the controller that a datagram has arrived from the peer.
    ///
    /// A single received datagram proves that the NAT binding is open on
    /// both sides.  The controller transitions to `Connected` and returns
    /// `Some(HolePunchEvent::Connected)`.
    ///
    /// Returns `None` when the controller is not actively probing (`Idle`,
    /// `Connected`, or `Failed`).
    pub fn on_probe_received(&mut self) -> Option<HolePunchEvent> {
        match self.state {
            HolePunchState::Probing { .. } => {
                self.state = HolePunchState::Connected;
                Some(HolePunchEvent::Connected)
            }
            _ => None,
        }
    }

    /// Whether the direct path has been confirmed open.
    pub fn is_connected(&self) -> bool {
        self.state == HolePunchState::Connected
    }

    /// Whether all probes were sent with no response.
    pub fn is_failed(&self) -> bool {
        self.state == HolePunchState::Failed
    }

    /// Whether the controller is actively sending probes.
    pub fn is_probing(&self) -> bool {
        matches!(self.state, HolePunchState::Probing { .. })
    }

    /// Number of probes sent so far (0 when idle, 1 after the first `start`
    /// call, up to [`HOLE_PUNCH_MAX_PROBES`] before failure).
    pub fn probes_sent(&self) -> u8 {
        match self.state {
            HolePunchState::Probing { probes_sent, .. } => probes_sent,
            HolePunchState::Failed => HOLE_PUNCH_MAX_PROBES,
            _ => 0,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE_A: u32 = 0xDEAD_BEEF;
    const NONCE_B: u32 = 0xCAFE_BABE;

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_controller_is_idle() {
        let ctrl = HolePunchController::new();
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_connected());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn default_equals_new() {
        let a = HolePunchController::new();
        let b = HolePunchController::default();
        assert_eq!(a.is_probing(), b.is_probing());
        assert_eq!(a.is_connected(), b.is_connected());
        assert_eq!(a.is_failed(), b.is_failed());
    }

    #[test]
    fn probes_sent_is_zero_before_start() {
        let ctrl = HolePunchController::new();
        assert_eq!(ctrl.probes_sent(), 0);
    }

    // ── start() ──────────────────────────────────────────────────────────────

    #[test]
    fn start_returns_probe_frame_with_seq_zero() {
        let mut ctrl = HolePunchController::new();
        let frame = ctrl.start(NONCE_A);
        assert_eq!(frame.payload[4], 0, "first probe must carry seq=0");
    }

    #[test]
    fn start_encodes_nonce_in_le_bytes() {
        let mut ctrl = HolePunchController::new();
        let frame = ctrl.start(NONCE_A);
        let expected = NONCE_A.to_le_bytes();
        assert_eq!(&frame.payload[..4], &expected);
    }

    #[test]
    fn start_transitions_to_probing() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        assert!(ctrl.is_probing());
        assert!(!ctrl.is_connected());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn start_records_one_probe_sent() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        assert_eq!(ctrl.probes_sent(), 1);
    }

    #[test]
    fn start_while_probing_resets_nonce() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        let frame = ctrl.start(NONCE_B);
        let expected = NONCE_B.to_le_bytes();
        assert_eq!(&frame.payload[..4], &expected);
        assert!(ctrl.is_probing());
    }

    #[test]
    fn start_while_probing_resets_seq_to_zero() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        let frame = ctrl.start(NONCE_B);
        assert_eq!(frame.payload[4], 0, "reset must restart seq from 0");
    }

    // ── tick(): interval countdown ────────────────────────────────────────────

    #[test]
    fn tick_returns_none_while_interval_running() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        for tick in 0..HOLE_PUNCH_PROBE_INTERVAL_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None while interval is running"
            );
        }
    }

    #[test]
    fn tick_returns_send_probe_when_interval_elapses() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);

        match ctrl.tick() {
            Some(HolePunchEvent::SendProbe(frame)) => {
                let expected = NONCE_A.to_le_bytes();
                assert_eq!(&frame.payload[..4], &expected, "nonce must match");
                assert_eq!(frame.payload[4], 1, "second probe has seq=1");
            }
            other => panic!("expected SendProbe, got {other:?}"),
        }
    }

    #[test]
    fn send_probe_resets_interval_timer() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
        ctrl.tick(); // SendProbe — timer resets.

        for tick in 0..HOLE_PUNCH_PROBE_INTERVAL_TICKS {
            assert!(
                ctrl.tick().is_none(),
                "tick {tick} after send: must return None while interval is running"
            );
        }
    }

    #[test]
    fn probe_seq_increments_each_interval() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A); // seq=0

        for expected_seq in 1u8..4 {
            burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
            match ctrl.tick() {
                Some(HolePunchEvent::SendProbe(frame)) => {
                    assert_eq!(
                        frame.payload[4], expected_seq,
                        "probe {expected_seq} must carry seq={expected_seq}"
                    );
                }
                other => panic!("expected SendProbe, got {other:?}"),
            }
        }
    }

    #[test]
    fn tick_idle_returns_none() {
        let mut ctrl = HolePunchController::new();
        assert_eq!(ctrl.tick(), None);
    }

    #[test]
    fn tick_connected_returns_none() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        ctrl.on_probe_received();

        for _ in 0..5 {
            assert_eq!(ctrl.tick(), None);
        }
    }

    #[test]
    fn tick_failed_returns_none() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        exhaust_probes(&mut ctrl);

        for _ in 0..5 {
            assert_eq!(ctrl.tick(), None);
        }
    }

    // ── tick(): exhaustion and failure ────────────────────────────────────────

    #[test]
    fn tick_returns_failed_after_max_probes_exhausted() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        exhaust_probes(&mut ctrl);

        assert!(ctrl.is_failed());
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_connected());
    }

    #[test]
    fn probe_count_before_failure_equals_max_probes() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A); // probe 1

        let mut send_count = 1u8;
        loop {
            burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
            match ctrl.tick() {
                Some(HolePunchEvent::SendProbe(_)) => send_count += 1,
                Some(HolePunchEvent::Failed) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }

        assert_eq!(
            send_count, HOLE_PUNCH_MAX_PROBES,
            "exactly HOLE_PUNCH_MAX_PROBES probes sent before failure"
        );
    }

    #[test]
    fn probes_sent_tracks_count() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        assert_eq!(ctrl.probes_sent(), 1);

        burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
        ctrl.tick(); // SendProbe (probe 2)
        assert_eq!(ctrl.probes_sent(), 2);

        burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
        ctrl.tick(); // SendProbe (probe 3)
        assert_eq!(ctrl.probes_sent(), 3);
    }

    #[test]
    fn probes_sent_equals_max_after_failure() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        exhaust_probes(&mut ctrl);
        assert_eq!(ctrl.probes_sent(), HOLE_PUNCH_MAX_PROBES);
    }

    // ── on_probe_received() ───────────────────────────────────────────────────

    #[test]
    fn on_probe_received_returns_connected_when_probing() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        let event = ctrl.on_probe_received();
        assert_eq!(event, Some(HolePunchEvent::Connected));
    }

    #[test]
    fn on_probe_received_transitions_to_connected() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        ctrl.on_probe_received();
        assert!(ctrl.is_connected());
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn on_probe_received_idle_returns_none() {
        let mut ctrl = HolePunchController::new();
        assert_eq!(ctrl.on_probe_received(), None);
    }

    #[test]
    fn on_probe_received_already_connected_returns_none() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        ctrl.on_probe_received();
        // Duplicate arrival must be a no-op.
        assert_eq!(ctrl.on_probe_received(), None);
    }

    #[test]
    fn on_probe_received_failed_returns_none() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);
        exhaust_probes(&mut ctrl);
        assert_eq!(ctrl.on_probe_received(), None);
    }

    // ── mid-probe reception ───────────────────────────────────────────────────

    #[test]
    fn on_probe_received_accepted_mid_interval() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS / 2);

        let event = ctrl.on_probe_received();
        assert_eq!(event, Some(HolePunchEvent::Connected));
        assert!(ctrl.is_connected());
    }

    #[test]
    fn on_probe_received_accepted_after_several_probes_sent() {
        let mut ctrl = HolePunchController::new();
        ctrl.start(NONCE_A);

        // Send a few probes first.
        for _ in 0..3 {
            burn_ticks(&mut ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
            ctrl.tick();
        }
        assert!(ctrl.is_probing());

        let event = ctrl.on_probe_received();
        assert_eq!(event, Some(HolePunchEvent::Connected));
        assert!(ctrl.is_connected());
    }

    // ── Probe payload structure ───────────────────────────────────────────────

    #[test]
    fn probe_payload_length_equals_constant() {
        let mut ctrl = HolePunchController::new();
        let frame = ctrl.start(NONCE_A);
        assert_eq!(frame.payload.len(), HOLE_PUNCH_PROBE_PAYLOAD_BYTES);
    }

    #[test]
    fn probe_frame_new_encodes_nonce_and_seq() {
        let frame = HolePunchProbeFrame::new(0x0102_0304, 7);
        // Little-endian nonce: 0x04, 0x03, 0x02, 0x01.
        assert_eq!(frame.payload, [0x04, 0x03, 0x02, 0x01, 0x07]);
    }

    #[test]
    fn probe_payload_bytes_constant_matches_payload_size() {
        assert_eq!(
            HOLE_PUNCH_PROBE_PAYLOAD_BYTES, 5,
            "4 bytes nonce + 1 byte seq = 5 bytes"
        );
    }

    // ── Constants sanity ──────────────────────────────────────────────────────

    #[test]
    fn probe_interval_ticks_corresponds_to_500ms_at_10hz() {
        assert_eq!(
            HOLE_PUNCH_PROBE_INTERVAL_TICKS, 5,
            "5 ticks × 100 ms = 500 ms inter-probe interval (RFC 8445 initial RTO)"
        );
    }

    #[test]
    fn max_probes_gives_5_second_window() {
        let total_ms =
            (HOLE_PUNCH_MAX_PROBES as u32) * HOLE_PUNCH_PROBE_INTERVAL_TICKS * 100;
        assert_eq!(total_ms, 5_000, "10 probes × 500 ms = 5 s probing window");
    }

    #[test]
    fn max_probes_is_positive() {
        assert!(HOLE_PUNCH_MAX_PROBES > 0);
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn burn_ticks(ctrl: &mut HolePunchController, n: u32) {
        for _ in 0..n {
            ctrl.tick();
        }
    }

    /// Drive the controller through all probes until it reaches `Failed`.
    fn exhaust_probes(ctrl: &mut HolePunchController) {
        loop {
            burn_ticks(ctrl, HOLE_PUNCH_PROBE_INTERVAL_TICKS);
            match ctrl.tick() {
                Some(HolePunchEvent::Failed) => break,
                Some(HolePunchEvent::SendProbe(_)) => {}
                other => panic!("unexpected event during exhaustion: {other:?}"),
            }
        }
    }
}

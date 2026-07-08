//! TURN relay transport — Feature 5.
//!
//! # Mechanism
//!
//! When ICE connectivity checks exhaust all direct UDP candidate pairs
//! (Feature 3), the transport falls back to routing ciphertext through the
//! coturn TURN fleet.  Data flows as TURN ChannelData messages
//! (RFC 5766 §11.4): a 4-byte header (channel number + data length) followed
//! by the E2EE ciphertext payload.  The TURN server forwards opaque bytes and
//! cannot decrypt, modify, or inject session content.
//!
//! # E2EE transparency invariant
//!
//! ChaCha20-Poly1305 encryption (Feature 21) is applied to every datagram
//! *before* it reaches [`TurnChannelDataFramer::encode`].
//! [`TurnChannelDataFramer`] accepts an opaque `&[u8]` payload without any
//! knowledge of its structure, enforcing the invariant structurally: the TURN
//! server therefore handles only ciphertext — it cannot read, modify, or inject
//! session content.
//!
//! # ChannelData message format (RFC 5766 §11.4)
//!
//! ```text
//! 0                   1                   2                   3
//! 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |         Channel Number        |            Data Length         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                      Application Data                          |
//! |                            ...                                 |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! # State machine
//!
//! ```text
//! Idle ─── on_direct_path_failed() ─────────────────────────────→ Probing
//!
//! Probing ─ on_relay_activated() ────────────────────────────────→ Active
//! Probing ─ tick() × (TIMEOUT+1) × (RETRIES+1) ──────────────────→ Failed
//!
//! Active ── on_relay_ack() ──────────────────────── (reset timer) → Active
//! Active ── on_direct_path_recovered() ──────────────────────────→ Idle
//! Active ── tick() fires keepalive ───── (RelayEvent::Keepalive) → Active
//!
//! Failed ── terminal: caller uses TCP port-443 fallback (Feature 6)
//! ```
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::turn_relay::{
//!     TurnRelayController, TurnChannelDataFramer, RelayEvent,
//!     TURN_DEFAULT_CHANNEL_NUMBER,
//! };
//!
//! let mut ctrl = TurnRelayController::new();
//! let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
//!
//! // Direct ICE probes all expired — activate relay.
//! ctrl.on_direct_path_failed();
//! assert!(ctrl.is_probing());
//!
//! // TURN binding confirmed (TURN ALLOCATE response received):
//! let event = ctrl.on_relay_activated();
//! assert_eq!(event, Some(RelayEvent::Activated));
//! assert!(ctrl.is_active());
//!
//! // E2EE ciphertext (already encrypted by ChaCha20-Poly1305):
//! let ciphertext = vec![0xABu8; 80]; // opaque encrypted payload
//! let channel_data = framer.encode(&ciphertext).expect("payload within limit");
//! // → write channel_data to the UDP socket bound to the TURN server …
//! ```

// ── Constants ─────────────────────────────────────────────────────────────────

/// Byte length of the TURN ChannelData header (channel number + data length).
///
/// Two bytes for the channel number and two bytes for the data length field,
/// matching the RFC 5766 §11.4 wire format.
pub const TURN_CHANNEL_HEADER_BYTES: usize = 4;

/// Default TURN channel number used for the first peer binding.
///
/// RFC 5766 §11.1 reserves channel numbers 0x4000–0x7FFF for ChannelData
/// messages.  0x4000 is the lowest valid value and is used for the initial
/// channel binding to the remote peer address.
pub const TURN_DEFAULT_CHANNEL_NUMBER: u16 = 0x4000;

/// Lowest valid TURN channel number (RFC 5766 §11.1).
pub const TURN_MIN_CHANNEL_NUMBER: u16 = 0x4000;

/// Highest valid TURN channel number (RFC 5766 §11.1).
pub const TURN_MAX_CHANNEL_NUMBER: u16 = 0x7FFF;

/// Maximum ciphertext payload the framer will accept, in bytes.
///
/// Matches the LBTP 1 200-byte datagram ceiling (Feature 7).  Adding the
/// 4-byte ChannelData header yields a 1 204-byte UDP datagram, safely below
/// the 1 280-byte IPv6 minimum MTU.
pub const TURN_MAX_PAYLOAD_BYTES: usize = 1_200;

/// Ticks between successive relay-activation probe attempts.
///
/// At the nominal 10 Hz controller rate, 5 ticks = 0.5 s per attempt.  The
/// timer fires after `TURN_PROBE_TIMEOUT_TICKS + 1` calls to
/// [`TurnRelayController::tick`], following the same convention as
/// [`PathMigrationController`](crate::path::PathMigrationController).
pub const TURN_PROBE_TIMEOUT_TICKS: u32 = 5;

/// Maximum relay probe retransmissions before declaring the relay unreachable.
///
/// 4 retries × 0.5 s each = up to 2.5 s total probing time before the
/// controller transitions to `Failed` and the caller falls back to TCP-443
/// (Feature 6).
pub const TURN_PROBE_MAX_RETRIES: u8 = 4;

/// Ticks between keepalive refreshes in the `Active` state.
///
/// RFC 5766 §10 specifies a default TURN allocation lifetime of 600 s.
/// Keepalives are sent every 120 s (1 200 ticks at 10 Hz) — well within the
/// expiry window — so the allocation stays alive for the duration of the
/// session.  The timer fires after `TURN_KEEPALIVE_INTERVAL_TICKS + 1` calls
/// to [`TurnRelayController::tick`].
pub const TURN_KEEPALIVE_INTERVAL_TICKS: u32 = 1_200;

// ── RelayEvent ────────────────────────────────────────────────────────────────

/// Events emitted by [`TurnRelayController::tick`] and
/// [`TurnRelayController::on_relay_activated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEvent {
    /// Probe timer expired and retries remain — re-send the TURN binding probe.
    Retransmit,
    /// TURN allocation confirmed; ciphertext can now flow via ChannelData.
    Activated,
    /// Keepalive interval elapsed — send a TURN Refresh to hold the allocation.
    Keepalive,
    /// All relay probes exhausted — relay unreachable; fall back to TCP-443.
    Failed,
}

// ── RelayState ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayState {
    /// Direct path is active; relay not needed.
    Idle,
    /// Direct path failed; probing TURN relay binding.
    Probing {
        retries_left: u8,
        ticks_remaining: u32,
    },
    /// TURN relay confirmed; ciphertext flows as ChannelData messages.
    Active {
        keepalive_ticks: u32,
    },
    /// Relay unreachable; caller should use TCP port-443 fallback.
    Failed,
}

// ── TurnRelayController ───────────────────────────────────────────────────────

/// State machine for TURN relay fallback (Feature 5).
///
/// One instance per active session.  Cheap to construct; zero heap allocation.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct TurnRelayController {
    state: RelayState,
}

impl Default for TurnRelayController {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnRelayController {
    /// Create a new controller in the `Idle` state.
    pub fn new() -> Self {
        Self {
            state: RelayState::Idle,
        }
    }

    /// Direct UDP path probes have all failed; begin TURN relay activation.
    ///
    /// Transitions the controller from `Idle` to `Probing`.
    /// Has no effect if the controller is already in `Probing`, `Active`, or
    /// `Failed` — the caller must not restart a probe mid-flight or after
    /// failure without first recovering the direct path.
    pub fn on_direct_path_failed(&mut self) {
        if self.state == RelayState::Idle {
            self.state = RelayState::Probing {
                retries_left: TURN_PROBE_MAX_RETRIES,
                ticks_remaining: TURN_PROBE_TIMEOUT_TICKS,
            };
        }
    }

    /// TURN binding confirmed — allocation response received from the server.
    ///
    /// Transitions from `Probing` to `Active` and returns
    /// `Some(RelayEvent::Activated)`.
    ///
    /// Returns `None` when the controller is not in `Probing` state (the
    /// response is stale or the controller was already in a terminal state).
    pub fn on_relay_activated(&mut self) -> Option<RelayEvent> {
        if matches!(self.state, RelayState::Probing { .. }) {
            self.state = RelayState::Active {
                keepalive_ticks: TURN_KEEPALIVE_INTERVAL_TICKS,
            };
            Some(RelayEvent::Activated)
        } else {
            None
        }
    }

    /// A relay acknowledgement was received (ChannelData echo or Refresh OK).
    ///
    /// Resets the keepalive timer so the TURN allocation does not expire.
    /// Has no effect outside the `Active` state.
    pub fn on_relay_ack(&mut self) {
        if let RelayState::Active { keepalive_ticks } = &mut self.state {
            *keepalive_ticks = TURN_KEEPALIVE_INTERVAL_TICKS;
        }
    }

    /// An ICE restart has found a new direct UDP path; stop using the relay.
    ///
    /// Transitions from `Active` to `Idle`.
    /// Has no effect outside the `Active` state.
    pub fn on_direct_path_recovered(&mut self) {
        if matches!(self.state, RelayState::Active { .. }) {
            self.state = RelayState::Idle;
        }
    }

    /// Advance the relay controller by one tick.
    ///
    /// Must be called once per control tick (10 Hz nominal) while a relay
    /// probe is in flight or the relay is active.
    ///
    /// Returns:
    /// - `Some(RelayEvent::Retransmit)` — probe timed out, retries remain;
    ///   re-send the TURN binding probe and reset the timer.
    /// - `Some(RelayEvent::Failed)` — all retries exhausted with no
    ///   activation; transition to `Failed`, caller should use TCP-443.
    /// - `Some(RelayEvent::Keepalive)` — keepalive interval elapsed in
    ///   `Active` state; send a TURN Refresh to hold the allocation open.
    /// - `None` — timer still running or state is `Idle`/`Failed`.
    pub fn tick(&mut self) -> Option<RelayEvent> {
        // Active keepalive.
        if let RelayState::Active { keepalive_ticks } = &mut self.state {
            if *keepalive_ticks > 0 {
                *keepalive_ticks -= 1;
                return None;
            }
            *keepalive_ticks = TURN_KEEPALIVE_INTERVAL_TICKS;
            return Some(RelayEvent::Keepalive);
        }

        // Probing timer countdown; capture retries_left to release the borrow.
        let retries_left = match &mut self.state {
            RelayState::Probing {
                retries_left,
                ticks_remaining,
            } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                *retries_left
            }
            _ => return None,
        };

        // Timer at zero.
        if retries_left == 0 {
            self.state = RelayState::Failed;
            return Some(RelayEvent::Failed);
        }
        self.state = RelayState::Probing {
            retries_left: retries_left - 1,
            ticks_remaining: TURN_PROBE_TIMEOUT_TICKS,
        };
        Some(RelayEvent::Retransmit)
    }

    /// Whether the controller is in the `Idle` state (direct path active).
    pub fn is_idle(&self) -> bool {
        self.state == RelayState::Idle
    }

    /// Whether a relay probe is currently in flight.
    pub fn is_probing(&self) -> bool {
        matches!(self.state, RelayState::Probing { .. })
    }

    /// Whether the TURN relay is confirmed active and ready for ciphertext.
    pub fn is_active(&self) -> bool {
        matches!(self.state, RelayState::Active { .. })
    }

    /// Whether all relay probes were exhausted with no activation.
    pub fn is_failed(&self) -> bool {
        self.state == RelayState::Failed
    }
}

// ── TurnChannelDataFramer ─────────────────────────────────────────────────────

/// Encodes and decodes TURN ChannelData messages (RFC 5766 §11.4).
///
/// Wraps E2EE ciphertext datagrams in a 4-byte ChannelData header for
/// delivery to the TURN server.  The struct is stateless beyond the channel
/// number; it does not inspect or interpret the payload.
///
/// # E2EE transparency
///
/// The framer accepts an opaque `&[u8]` payload.  The transport layer is
/// responsible for encrypting the LBTP datagram with ChaCha20-Poly1305
/// (Feature 21) *before* passing it here.  The TURN server therefore handles
/// only ciphertext — it cannot read or modify session content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnChannelDataFramer {
    channel_number: u16,
}

impl TurnChannelDataFramer {
    /// Create a framer for the given TURN `channel_number`.
    ///
    /// `channel_number` must be in the valid TURN channel range
    /// [`TURN_MIN_CHANNEL_NUMBER`]–[`TURN_MAX_CHANNEL_NUMBER`] (0x4000–0x7FFF).
    /// Values outside this range are rejected in debug builds via
    /// `debug_assert!`.
    pub fn new(channel_number: u16) -> Self {
        debug_assert!(
            channel_number >= TURN_MIN_CHANNEL_NUMBER
                && channel_number <= TURN_MAX_CHANNEL_NUMBER,
            "channel_number {channel_number:#06x} is outside the valid TURN range \
             {TURN_MIN_CHANNEL_NUMBER:#06x}–{TURN_MAX_CHANNEL_NUMBER:#06x}"
        );
        Self { channel_number }
    }

    /// Encode `ciphertext` as a TURN ChannelData message.
    ///
    /// Prepends a 4-byte header `[chan_hi, chan_lo, len_hi, len_lo]` to the
    /// payload bytes.
    ///
    /// Returns `None` if `ciphertext.len() > TURN_MAX_PAYLOAD_BYTES` — the
    /// caller should log and drop the datagram rather than silently truncating.
    pub fn encode(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        if ciphertext.len() > TURN_MAX_PAYLOAD_BYTES {
            return None;
        }
        let chan_bytes = self.channel_number.to_be_bytes();
        let len_bytes = (ciphertext.len() as u16).to_be_bytes();
        let mut out = Vec::with_capacity(TURN_CHANNEL_HEADER_BYTES + ciphertext.len());
        out.extend_from_slice(&chan_bytes);
        out.extend_from_slice(&len_bytes);
        out.extend_from_slice(ciphertext);
        Some(out)
    }

    /// Decode a TURN ChannelData message received from the TURN server.
    ///
    /// Returns `Some((channel_number, ciphertext))` where `ciphertext` is a
    /// slice into `bytes` pointing at the payload.
    ///
    /// Returns `None` when:
    /// - `bytes` is shorter than [`TURN_CHANNEL_HEADER_BYTES`].
    /// - The data-length field in the header does not match the remaining
    ///   bytes (malformed or truncated ChannelData message).
    pub fn decode(bytes: &[u8]) -> Option<(u16, &[u8])> {
        if bytes.len() < TURN_CHANNEL_HEADER_BYTES {
            return None;
        }
        let channel = u16::from_be_bytes([bytes[0], bytes[1]]);
        let data_len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if bytes.len() != TURN_CHANNEL_HEADER_BYTES + data_len {
            return None;
        }
        Some((channel, &bytes[TURN_CHANNEL_HEADER_BYTES..]))
    }

    /// The TURN channel number this framer encodes into.
    pub fn channel_number(&self) -> u16 {
        self.channel_number
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn burn_probe_ticks(ctrl: &mut TurnRelayController, n: u32) {
        for _ in 0..n {
            ctrl.tick();
        }
    }

    fn exhaust_relay_probes(ctrl: &mut TurnRelayController) {
        loop {
            burn_probe_ticks(ctrl, TURN_PROBE_TIMEOUT_TICKS);
            match ctrl.tick() {
                Some(RelayEvent::Failed) => break,
                Some(RelayEvent::Retransmit) => {}
                other => panic!("unexpected event during probe exhaustion: {other:?}"),
            }
        }
    }

    // ── TurnRelayController — construction ────────────────────────────────────

    #[test]
    fn new_controller_is_idle() {
        let ctrl = TurnRelayController::new();
        assert!(ctrl.is_idle());
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_active());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn default_equals_new() {
        let a = TurnRelayController::new();
        let b = TurnRelayController::default();
        assert_eq!(a.is_idle(), b.is_idle());
        assert_eq!(a.is_probing(), b.is_probing());
        assert_eq!(a.is_active(), b.is_active());
        assert_eq!(a.is_failed(), b.is_failed());
    }

    // ── on_direct_path_failed() ───────────────────────────────────────────────

    #[test]
    fn on_direct_path_failed_transitions_idle_to_probing() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        assert!(ctrl.is_probing(), "Idle → Probing when direct path fails");
        assert!(!ctrl.is_idle());
    }

    #[test]
    fn on_direct_path_failed_while_probing_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed(); // → Probing
        ctrl.on_direct_path_failed(); // must be a no-op
        assert!(ctrl.is_probing(), "re-calling on_direct_path_failed in Probing must be a no-op");
    }

    #[test]
    fn on_direct_path_failed_while_active_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();
        ctrl.on_direct_path_failed(); // must be a no-op
        assert!(ctrl.is_active(), "on_direct_path_failed must not disrupt Active state");
    }

    #[test]
    fn on_direct_path_failed_while_failed_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        exhaust_relay_probes(&mut ctrl);
        ctrl.on_direct_path_failed(); // must be a no-op
        assert!(ctrl.is_failed(), "on_direct_path_failed must not reset Failed state");
    }

    // ── on_relay_activated() ──────────────────────────────────────────────────

    #[test]
    fn on_relay_activated_probing_to_active() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        let event = ctrl.on_relay_activated();
        assert_eq!(event, Some(RelayEvent::Activated));
        assert!(ctrl.is_active());
        assert!(!ctrl.is_probing());
    }

    #[test]
    fn on_relay_activated_in_idle_returns_none() {
        let mut ctrl = TurnRelayController::new();
        assert_eq!(ctrl.on_relay_activated(), None, "activation in Idle must be ignored");
        assert!(ctrl.is_idle());
    }

    #[test]
    fn on_relay_activated_in_active_returns_none() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();
        assert_eq!(ctrl.on_relay_activated(), None, "activation in Active must be a no-op");
        assert!(ctrl.is_active());
    }

    #[test]
    fn on_relay_activated_in_failed_returns_none() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        exhaust_relay_probes(&mut ctrl);
        assert_eq!(ctrl.on_relay_activated(), None, "activation in Failed must be a no-op");
        assert!(ctrl.is_failed());
    }

    // ── on_relay_ack() ────────────────────────────────────────────────────────

    #[test]
    fn on_relay_ack_resets_keepalive_timer_in_active_state() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();

        // Burn most of the keepalive window.
        burn_probe_ticks(&mut ctrl, TURN_KEEPALIVE_INTERVAL_TICKS - 2);
        // ACK resets the timer to TURN_KEEPALIVE_INTERVAL_TICKS.
        ctrl.on_relay_ack();
        // Burn the original remaining ticks — without an ACK this would fire.
        burn_probe_ticks(&mut ctrl, 3);
        // Timer is still running (reset occurred); no keepalive fired.
        assert!(
            ctrl.is_active(),
            "relay must remain Active after ack resets the keepalive timer"
        );
    }

    #[test]
    fn on_relay_ack_in_probing_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_ack(); // must be a no-op
        assert!(ctrl.is_probing(), "on_relay_ack must not affect Probing state");
    }

    #[test]
    fn on_relay_ack_in_idle_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_relay_ack(); // must be a no-op
        assert!(ctrl.is_idle());
    }

    // ── on_direct_path_recovered() ────────────────────────────────────────────

    #[test]
    fn on_direct_path_recovered_active_to_idle() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();
        ctrl.on_direct_path_recovered();
        assert!(ctrl.is_idle(), "Active → Idle when direct path recovers");
        assert!(!ctrl.is_active());
    }

    #[test]
    fn on_direct_path_recovered_in_idle_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_recovered();
        assert!(ctrl.is_idle(), "on_direct_path_recovered in Idle must be a no-op");
    }

    #[test]
    fn on_direct_path_recovered_in_probing_is_ignored() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_direct_path_recovered();
        assert!(
            ctrl.is_probing(),
            "on_direct_path_recovered must not affect Probing state"
        );
    }

    // ── tick() — Probing state ────────────────────────────────────────────────

    #[test]
    fn tick_idle_returns_none() {
        let mut ctrl = TurnRelayController::new();
        assert_eq!(ctrl.tick(), None);
    }

    #[test]
    fn tick_returns_none_while_probe_timer_running() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        for tick in 0..TURN_PROBE_TIMEOUT_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_retransmit_when_probe_timer_expires() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        burn_probe_ticks(&mut ctrl, TURN_PROBE_TIMEOUT_TICKS);
        assert_eq!(
            ctrl.tick(),
            Some(RelayEvent::Retransmit),
            "must return Retransmit when timer expires and retries remain"
        );
    }

    #[test]
    fn retransmit_resets_probe_timer() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        burn_probe_ticks(&mut ctrl, TURN_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit; timer reset.

        // Full timer must run again before the next event.
        for tick in 0..TURN_PROBE_TIMEOUT_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick} after retransmit: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_failed_when_all_retries_exhausted() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        exhaust_relay_probes(&mut ctrl);

        assert!(ctrl.is_failed(), "controller must be Failed after all retries exhausted");
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_active());
    }

    #[test]
    fn retransmit_count_matches_max_retries() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        let mut retransmit_count: u8 = 0;
        loop {
            burn_probe_ticks(&mut ctrl, TURN_PROBE_TIMEOUT_TICKS);
            match ctrl.tick() {
                Some(RelayEvent::Retransmit) => retransmit_count += 1,
                Some(RelayEvent::Failed) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            retransmit_count, TURN_PROBE_MAX_RETRIES,
            "exactly TURN_PROBE_MAX_RETRIES retransmissions before failure"
        );
    }

    #[test]
    fn failed_tick_is_noop() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        exhaust_relay_probes(&mut ctrl);

        for _ in 0..5 {
            assert_eq!(ctrl.tick(), None, "tick in Failed must be a no-op");
        }
    }

    // ── tick() — Active state keepalive ───────────────────────────────────────

    #[test]
    fn tick_in_active_returns_none_while_keepalive_timer_running() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();

        for tick in 0..TURN_KEEPALIVE_INTERVAL_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None while keepalive timer is running"
            );
        }
    }

    #[test]
    fn tick_in_active_returns_keepalive_when_timer_expires() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();

        burn_probe_ticks(&mut ctrl, TURN_KEEPALIVE_INTERVAL_TICKS);
        assert_eq!(
            ctrl.tick(),
            Some(RelayEvent::Keepalive),
            "must return Keepalive when keepalive timer expires"
        );
        assert!(ctrl.is_active(), "controller must remain Active after keepalive");
    }

    #[test]
    fn keepalive_fires_repeatedly() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();

        for round in 0..3 {
            burn_probe_ticks(&mut ctrl, TURN_KEEPALIVE_INTERVAL_TICKS);
            let event = ctrl.tick();
            assert_eq!(
                event,
                Some(RelayEvent::Keepalive),
                "keepalive must fire on round {round}"
            );
            assert!(ctrl.is_active(), "must remain Active after keepalive on round {round}");
        }
    }

    // ── Activation after retransmit ───────────────────────────────────────────

    #[test]
    fn relay_can_be_activated_after_retransmit() {
        let mut ctrl = TurnRelayController::new();
        ctrl.on_direct_path_failed();

        // Let one probe attempt time out.
        burn_probe_ticks(&mut ctrl, TURN_PROBE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit.

        // Server finally responds on the retransmitted probe.
        let event = ctrl.on_relay_activated();
        assert_eq!(event, Some(RelayEvent::Activated));
        assert!(ctrl.is_active(), "relay must become Active on late activation");
    }

    // ── Round-trip: fail then recover via direct path ─────────────────────────

    #[test]
    fn relay_to_idle_round_trip() {
        let mut ctrl = TurnRelayController::new();

        // Direct path fails → relay activates.
        ctrl.on_direct_path_failed();
        ctrl.on_relay_activated();
        assert!(ctrl.is_active());

        // ICE restart finds a new direct path.
        ctrl.on_direct_path_recovered();
        assert!(ctrl.is_idle(), "controller must return to Idle after path recovery");

        // Direct path can fail again and relay can be probed again.
        ctrl.on_direct_path_failed();
        assert!(ctrl.is_probing(), "relay probe must restart after a second path failure");
    }

    // ── TurnChannelDataFramer — encode ────────────────────────────────────────

    #[test]
    fn encode_prepends_4_byte_header() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload = b"ciphertext";
        let encoded = framer.encode(payload).unwrap();
        assert_eq!(
            encoded.len(),
            TURN_CHANNEL_HEADER_BYTES + payload.len(),
            "encoded length must be header ({TURN_CHANNEL_HEADER_BYTES} B) + payload"
        );
    }

    #[test]
    fn encode_writes_channel_number_big_endian() {
        let channel: u16 = 0x4001;
        let framer = TurnChannelDataFramer::new(channel);
        let encoded = framer.encode(b"x").unwrap();
        let wire_channel = u16::from_be_bytes([encoded[0], encoded[1]]);
        assert_eq!(wire_channel, channel, "channel number must be big-endian in bytes 0-1");
    }

    #[test]
    fn encode_writes_data_length_big_endian() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload = b"hello";
        let encoded = framer.encode(payload).unwrap();
        let wire_len = u16::from_be_bytes([encoded[2], encoded[3]]) as usize;
        assert_eq!(wire_len, payload.len(), "data length must be big-endian in bytes 2-3");
    }

    #[test]
    fn encode_payload_follows_header_verbatim() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload: Vec<u8> = (0u8..32).collect();
        let encoded = framer.encode(&payload).unwrap();
        assert_eq!(
            &encoded[TURN_CHANNEL_HEADER_BYTES..],
            payload.as_slice(),
            "payload bytes must follow the header verbatim without modification"
        );
    }

    #[test]
    fn encode_accepts_max_size_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload = vec![0xFFu8; TURN_MAX_PAYLOAD_BYTES];
        assert!(
            framer.encode(&payload).is_some(),
            "max-size payload ({TURN_MAX_PAYLOAD_BYTES} B) must be accepted"
        );
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload = vec![0u8; TURN_MAX_PAYLOAD_BYTES + 1];
        assert!(
            framer.encode(&payload).is_none(),
            "payload exceeding TURN_MAX_PAYLOAD_BYTES must be rejected"
        );
    }

    #[test]
    fn encode_accepts_empty_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let encoded = framer.encode(b"").unwrap();
        assert_eq!(
            encoded.len(),
            TURN_CHANNEL_HEADER_BYTES,
            "empty payload encodes to header only"
        );
        let wire_len = u16::from_be_bytes([encoded[2], encoded[3]]);
        assert_eq!(wire_len, 0, "data length must be 0 for empty payload");
    }

    // ── TurnChannelDataFramer — decode ────────────────────────────────────────

    #[test]
    fn decode_recovers_channel_and_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload = b"lbtp ciphertext";
        let encoded = framer.encode(payload).unwrap();
        let (channel, data) = TurnChannelDataFramer::decode(&encoded).unwrap();
        assert_eq!(channel, TURN_DEFAULT_CHANNEL_NUMBER);
        assert_eq!(data, payload);
    }

    #[test]
    fn decode_returns_none_for_too_short_input() {
        for len in 0..TURN_CHANNEL_HEADER_BYTES {
            let bytes = vec![0u8; len];
            assert!(
                TurnChannelDataFramer::decode(&bytes).is_none(),
                "decode must return None for {len}-byte input (shorter than header)"
            );
        }
    }

    #[test]
    fn decode_returns_none_when_length_field_exceeds_remaining_bytes() {
        let mut bytes = vec![0x40u8, 0x00u8, 0x00u8, 0x10u8]; // channel=0x4000, len=16
        bytes.extend_from_slice(&[0u8; 8]); // only 8 bytes, not 16
        assert!(
            TurnChannelDataFramer::decode(&bytes).is_none(),
            "decode must return None when length field exceeds actual data"
        );
    }

    #[test]
    fn decode_returns_none_when_trailing_bytes_present() {
        // Header says len=5 but there are 6 payload bytes — trailing garbage.
        let mut bytes = vec![0x40u8, 0x00u8, 0x00u8, 0x05u8];
        bytes.extend_from_slice(&[0u8; 6]);
        assert!(
            TurnChannelDataFramer::decode(&bytes).is_none(),
            "decode must return None when actual payload exceeds length field"
        );
    }

    // ── Encode → decode round-trips ───────────────────────────────────────────

    #[test]
    fn encode_decode_roundtrip_arbitrary_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload: Vec<u8> = (0u8..=127).collect();
        let encoded = framer.encode(&payload).unwrap();
        let (chan, data) = TurnChannelDataFramer::decode(&encoded).unwrap();
        assert_eq!(chan, TURN_DEFAULT_CHANNEL_NUMBER);
        assert_eq!(data, payload.as_slice());
    }

    #[test]
    fn encode_decode_roundtrip_max_size_payload() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let payload: Vec<u8> = (0..TURN_MAX_PAYLOAD_BYTES).map(|i| i as u8).collect();
        let encoded = framer.encode(&payload).unwrap();
        let (chan, data) = TurnChannelDataFramer::decode(&encoded).unwrap();
        assert_eq!(chan, TURN_DEFAULT_CHANNEL_NUMBER);
        assert_eq!(data, payload.as_slice());
    }

    #[test]
    fn channel_number_preserved_through_encode_decode() {
        for channel in [0x4000u16, 0x5555, 0x7FFF] {
            let framer = TurnChannelDataFramer::new(channel);
            let encoded = framer.encode(b"x").unwrap();
            let (decoded_channel, _) = TurnChannelDataFramer::decode(&encoded).unwrap();
            assert_eq!(
                decoded_channel, channel,
                "channel {channel:#06x} must survive encode→decode"
            );
        }
    }

    // ── E2EE transparency: framer is payload-agnostic ─────────────────────────

    #[test]
    fn encode_does_not_modify_payload_bytes() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        // Simulate opaque E2EE ciphertext — arbitrary byte pattern.
        let ciphertext: Vec<u8> = (0u8..80).map(|b| b.wrapping_mul(37).wrapping_add(13)).collect();
        let encoded = framer.encode(&ciphertext).unwrap();
        assert_eq!(
            &encoded[TURN_CHANNEL_HEADER_BYTES..],
            ciphertext.as_slice(),
            "framer must not alter payload bytes — TURN server must see only unmodified ciphertext"
        );
    }

    #[test]
    fn decode_returns_payload_without_modification() {
        let framer = TurnChannelDataFramer::new(TURN_DEFAULT_CHANNEL_NUMBER);
        let ciphertext: Vec<u8> = (0u8..=200).map(|b| !b).collect(); // bitwise NOT pattern
        let encoded = framer.encode(&ciphertext).unwrap();
        let (_, decoded) = TurnChannelDataFramer::decode(&encoded).unwrap();
        assert_eq!(
            decoded, ciphertext.as_slice(),
            "decode must return the exact payload bytes without modification"
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn turn_channel_header_is_4_bytes() {
        assert_eq!(TURN_CHANNEL_HEADER_BYTES, 4);
    }

    #[test]
    fn turn_default_channel_is_in_valid_range() {
        assert!(TURN_DEFAULT_CHANNEL_NUMBER >= TURN_MIN_CHANNEL_NUMBER);
        assert!(TURN_DEFAULT_CHANNEL_NUMBER <= TURN_MAX_CHANNEL_NUMBER);
    }

    #[test]
    fn turn_max_payload_matches_lbtp_datagram_ceiling() {
        assert_eq!(
            TURN_MAX_PAYLOAD_BYTES, 1_200,
            "TURN payload limit must match the LBTP 1 200-byte datagram ceiling (Feature 7)"
        );
    }

    #[test]
    fn channel_number_accessor_returns_configured_value() {
        let framer = TurnChannelDataFramer::new(0x5A5A);
        assert_eq!(framer.channel_number(), 0x5A5A);
    }
}

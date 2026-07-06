//! Path-challenge / path-response migration controller — Feature 12.
//!
//! # Mechanism
//!
//! When the local endpoint wants to migrate an active session to a new network
//! path (e.g. a new local or remote address), it:
//!
//! 1. Calls [`PathMigrationController::start`] with a randomly-generated
//!    8-byte token.  The controller moves into the `Probing` state and returns
//!    a [`PathChallengeFrame`] that the transport must send to the candidate
//!    remote address on the new path.
//!
//! 2. Each call to [`PathMigrationController::tick`] decrements the
//!    per-attempt timer.  When the timer reaches zero and retries remain, it
//!    returns [`MigrationEvent::Retransmit`] — the same challenge is re-sent
//!    and the timer is reset.  Once all retries are exhausted the controller
//!    transitions to `Failed` and returns [`MigrationEvent::Failed`].
//!
//! 3. When the peer echoes the token in a [`PathResponseFrame`], the transport
//!    calls [`PathMigrationController::on_response`].  A matching token causes
//!    the controller to return [`MigrationEvent::Migrated`] and move to the
//!    `Migrated` state.  A mismatched token is silently ignored; the in-flight
//!    challenge remains active.
//!
//! # No renegotiation
//!
//! The existing session keys are valid on any path — only the UDP 5-tuple
//! changes.  The migration controller does not touch the crypto layer.  It
//! confirms only that the new path can carry the challenge token round-trip,
//! proving the remote can receive traffic on the new address before the stack
//! commits to it.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::path::{
//!     PathMigrationController, PathResponseFrame, MigrationEvent,
//! };
//!
//! let mut ctrl = PathMigrationController::new();
//!
//! // The transport generates a cryptographically random token and starts the probe.
//! let token = [0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x7a, 0x8b];
//! let challenge = ctrl.start(token);
//! // → send `challenge` to the candidate remote address …
//!
//! // On each control tick (10 Hz typical):
//! // if let Some(event) = ctrl.tick() { handle(event); }
//!
//! // On receiving a PATH_RESPONSE frame from the peer:
//! let response = PathResponseFrame { token };
//! assert_eq!(ctrl.on_response(&response), Some(MigrationEvent::Migrated));
//! assert!(ctrl.is_migrated());
//! ```

/// 8-byte opaque challenge token — mirrors the QUIC PATH_CHALLENGE wire width
/// (RFC 9000 §19.17).
pub type ChallengeToken = [u8; 8];

/// Ticks between successive challenge attempts.
///
/// At the nominal 10 Hz controller rate, 10 ticks correspond to 1 second per
/// attempt — sufficient for high-latency paths (satellite RTT ≈ 600 ms) while
/// still failing fast enough to be operationally useful.
///
/// The timer fires after `CHALLENGE_TIMEOUT_TICKS + 1` calls to
/// [`PathMigrationController::tick`], matching the convention used by the
/// [`LossBackstop`] cooldown timer.
pub const CHALLENGE_TIMEOUT_TICKS: u32 = 10;

/// Maximum retransmissions of the path_challenge before declaring failure.
///
/// 3 retries × ≈1 s each = up to ≈4 s total probing time before the
/// controller moves to `Failed`.  This matches the QUIC recommendation of
/// three path probes (RFC 9000 §9.2).
pub const MAX_CHALLENGE_RETRIES: u8 = 3;

/// A `PATH_CHALLENGE` frame ready to be sent to the candidate new path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChallengeFrame {
    /// Random token the peer must echo back unchanged in a PATH_RESPONSE.
    pub token: ChallengeToken,
}

/// A `PATH_RESPONSE` frame received from the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResponseFrame {
    /// Token copied verbatim from the peer's PATH_CHALLENGE.
    pub token: ChallengeToken,
}

/// Events emitted by [`PathMigrationController::tick`] and
/// [`PathMigrationController::on_response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationEvent {
    /// The previous attempt timed out and retries remain — re-send this frame.
    Retransmit(PathChallengeFrame),
    /// The peer echoed back a matching token; the session has migrated.
    Migrated,
    /// All retries exhausted with no valid response.
    Failed,
}

/// Internal state of the migration controller.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MigrationState {
    Idle,
    Probing {
        token: ChallengeToken,
        retries_left: u8,
        ticks_remaining: u32,
    },
    Migrated,
    Failed,
}

/// Path-challenge / path-response migration controller (Feature 12).
///
/// One instance per active session.  Cheap to construct; zero heap allocation.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct PathMigrationController {
    state: MigrationState,
}

impl Default for PathMigrationController {
    fn default() -> Self {
        Self::new()
    }
}

impl PathMigrationController {
    /// Create a new controller in the `Idle` state.
    pub fn new() -> Self {
        Self {
            state: MigrationState::Idle,
        }
    }

    /// Begin probing the new path with the given `token`.
    ///
    /// Transitions the controller to `Probing` and returns the
    /// [`PathChallengeFrame`] that the transport must send to the candidate
    /// remote address.
    ///
    /// Calling `start` while a probe is already in flight resets the probe
    /// with the new token (e.g. when the caller rotates the token after an
    /// address change).
    pub fn start(&mut self, token: ChallengeToken) -> PathChallengeFrame {
        self.state = MigrationState::Probing {
            token,
            retries_left: MAX_CHALLENGE_RETRIES,
            ticks_remaining: CHALLENGE_TIMEOUT_TICKS,
        };
        PathChallengeFrame { token }
    }

    /// Advance the migration timer by one tick.
    ///
    /// Must be called once per control tick while a probe is in flight.
    ///
    /// Returns:
    /// - `Some(MigrationEvent::Retransmit(frame))` — the attempt timed out and
    ///   retries remain; the transport must re-send the returned frame.
    /// - `Some(MigrationEvent::Failed)` — all retries are exhausted; migration
    ///   cannot proceed on this path.
    /// - `None` — the probe timer is still running; no action required.
    ///
    /// Has no effect (returns `None`) when the controller is `Idle`,
    /// `Migrated`, or `Failed`.
    pub fn tick(&mut self) -> Option<MigrationEvent> {
        // Capture token and retries_left when the timer expires, releasing the
        // mutable borrow on self.state before we reassign it below.
        let (token, retries_left) = match &mut self.state {
            MigrationState::Probing {
                token,
                retries_left,
                ticks_remaining,
            } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                (*token, *retries_left)
            }
            _ => return None,
        };

        // Timer at zero.
        if retries_left == 0 {
            self.state = MigrationState::Failed;
            return Some(MigrationEvent::Failed);
        }

        self.state = MigrationState::Probing {
            token,
            retries_left: retries_left - 1,
            ticks_remaining: CHALLENGE_TIMEOUT_TICKS,
        };
        Some(MigrationEvent::Retransmit(PathChallengeFrame { token }))
    }

    /// Process a `PATH_RESPONSE` frame received from the peer.
    ///
    /// If the token matches the outstanding challenge, the controller
    /// transitions to `Migrated` and returns `Some(MigrationEvent::Migrated)`.
    ///
    /// Returns `None` when:
    /// - No probe is active (controller is `Idle`, `Migrated`, or `Failed`).
    /// - The response token does not match the outstanding challenge token
    ///   (the in-flight probe remains active in this case).
    pub fn on_response(&mut self, response: &PathResponseFrame) -> Option<MigrationEvent> {
        let token_matches = match &self.state {
            MigrationState::Probing { token, .. } => response.token == *token,
            _ => return None,
        };

        if !token_matches {
            return None;
        }

        self.state = MigrationState::Migrated;
        Some(MigrationEvent::Migrated)
    }

    /// Whether the session has successfully migrated to the new path.
    pub fn is_migrated(&self) -> bool {
        self.state == MigrationState::Migrated
    }

    /// Whether all probes were exhausted with no valid response.
    pub fn is_failed(&self) -> bool {
        self.state == MigrationState::Failed
    }

    /// Whether a path_challenge is currently in flight.
    pub fn is_probing(&self) -> bool {
        matches!(self.state, MigrationState::Probing { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_A: ChallengeToken = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    const TOKEN_B: ChallengeToken = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn new_controller_is_idle() {
        let ctrl = PathMigrationController::new();
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_migrated());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn default_equals_new() {
        let a = PathMigrationController::new();
        let b = PathMigrationController::default();
        assert_eq!(a.is_probing(), b.is_probing());
        assert_eq!(a.is_migrated(), b.is_migrated());
        assert_eq!(a.is_failed(), b.is_failed());
    }

    // ── start() ──────────────────────────────────────────────────────────

    #[test]
    fn start_returns_challenge_frame_with_token() {
        let mut ctrl = PathMigrationController::new();
        let frame = ctrl.start(TOKEN_A);
        assert_eq!(frame.token, TOKEN_A);
    }

    #[test]
    fn start_transitions_to_probing() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        assert!(ctrl.is_probing());
    }

    #[test]
    fn start_while_probing_resets_probe_with_new_token() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        let frame = ctrl.start(TOKEN_B);
        assert_eq!(frame.token, TOKEN_B);
        assert!(ctrl.is_probing());

        // Old token must no longer match.
        assert_eq!(ctrl.on_response(&PathResponseFrame { token: TOKEN_A }), None);
        // New token must match.
        assert_eq!(
            ctrl.on_response(&PathResponseFrame { token: TOKEN_B }),
            Some(MigrationEvent::Migrated)
        );
    }

    // ── tick(): timer countdown ───────────────────────────────────────────

    #[test]
    fn tick_returns_none_while_timer_running() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        for tick in 0..CHALLENGE_TIMEOUT_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick}: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_retransmit_when_timer_expires() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);

        // The next tick should fire.
        match ctrl.tick() {
            Some(MigrationEvent::Retransmit(frame)) => {
                assert_eq!(frame.token, TOKEN_A, "retransmit must carry the original token");
            }
            other => panic!("expected Retransmit, got {other:?}"),
        }
    }

    #[test]
    fn retransmit_resets_timer_for_next_attempt() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        // First expiry.
        burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit; timer reset.

        // Timer must run for a full window again before the next expiry.
        for tick in 0..CHALLENGE_TIMEOUT_TICKS {
            let result = ctrl.tick();
            assert!(
                result.is_none(),
                "tick {tick} after retransmit: must return None while timer is running"
            );
        }
    }

    #[test]
    fn tick_fires_failed_after_all_retries_exhausted() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        // Consume all retransmit opportunities.
        for _ in 0..MAX_CHALLENGE_RETRIES {
            burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);
            let event = ctrl.tick();
            assert!(
                matches!(event, Some(MigrationEvent::Retransmit(_))),
                "expected Retransmit before exhaustion"
            );
        }

        // One more expiry with no retries left.
        burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);
        assert_eq!(ctrl.tick(), Some(MigrationEvent::Failed));
        assert!(ctrl.is_failed());
    }

    #[test]
    fn failed_controller_tick_is_noop() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        exhaust_retries(&mut ctrl);

        // tick() must be silent after failure.
        for _ in 0..5 {
            assert_eq!(ctrl.tick(), None);
        }
    }

    #[test]
    fn tick_idle_returns_none() {
        let mut ctrl = PathMigrationController::new();
        assert_eq!(ctrl.tick(), None);
    }

    #[test]
    fn tick_migrated_returns_none() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        ctrl.on_response(&PathResponseFrame { token: TOKEN_A });

        assert_eq!(ctrl.tick(), None);
    }

    // ── on_response(): token matching ─────────────────────────────────────

    #[test]
    fn on_response_returns_migrated_on_matching_token() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        let event = ctrl.on_response(&PathResponseFrame { token: TOKEN_A });
        assert_eq!(event, Some(MigrationEvent::Migrated));
    }

    #[test]
    fn on_response_transitions_to_migrated() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        ctrl.on_response(&PathResponseFrame { token: TOKEN_A });

        assert!(ctrl.is_migrated());
        assert!(!ctrl.is_probing());
        assert!(!ctrl.is_failed());
    }

    #[test]
    fn on_response_wrong_token_returns_none() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        let result = ctrl.on_response(&PathResponseFrame { token: TOKEN_B });
        assert_eq!(result, None, "mismatched token must be ignored");
        assert!(ctrl.is_probing(), "probe must still be active after mismatch");
    }

    #[test]
    fn on_response_idle_returns_none() {
        let mut ctrl = PathMigrationController::new();
        assert_eq!(ctrl.on_response(&PathResponseFrame { token: TOKEN_A }), None);
    }

    #[test]
    fn on_response_failed_returns_none() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        exhaust_retries(&mut ctrl);

        assert_eq!(ctrl.on_response(&PathResponseFrame { token: TOKEN_A }), None);
    }

    #[test]
    fn on_response_after_migration_returns_none() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);
        ctrl.on_response(&PathResponseFrame { token: TOKEN_A });

        // A second (replayed) PATH_RESPONSE must be ignored.
        assert_eq!(ctrl.on_response(&PathResponseFrame { token: TOKEN_A }), None);
    }

    // ── Late response after retransmit ────────────────────────────────────

    #[test]
    fn response_accepted_during_any_probing_tick() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        // Advance partway through the timer — not yet expired.
        burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS / 2);

        let event = ctrl.on_response(&PathResponseFrame { token: TOKEN_A });
        assert_eq!(event, Some(MigrationEvent::Migrated));
        assert!(ctrl.is_migrated());
    }

    #[test]
    fn response_accepted_after_retransmit() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        // Let the first attempt time out and retransmit.
        burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);
        ctrl.tick(); // Retransmit

        // A delayed response to the original or retransmitted challenge is accepted
        // because the token is the same.
        let event = ctrl.on_response(&PathResponseFrame { token: TOKEN_A });
        assert_eq!(event, Some(MigrationEvent::Migrated));
    }

    // ── Retransmit count is decremented correctly ─────────────────────────

    #[test]
    fn retransmit_count_matches_max_retries() {
        let mut ctrl = PathMigrationController::new();
        ctrl.start(TOKEN_A);

        let mut retransmit_count = 0u8;
        loop {
            burn_ticks(&mut ctrl, CHALLENGE_TIMEOUT_TICKS);
            match ctrl.tick() {
                Some(MigrationEvent::Retransmit(_)) => retransmit_count += 1,
                Some(MigrationEvent::Failed) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }

        assert_eq!(
            retransmit_count, MAX_CHALLENGE_RETRIES,
            "exactly MAX_CHALLENGE_RETRIES retransmissions before failure"
        );
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Advance `n` ticks without triggering the expiry.
    fn burn_ticks(ctrl: &mut PathMigrationController, n: u32) {
        for _ in 0..n {
            ctrl.tick();
        }
    }

    /// Drive the controller through all retries until it reaches `Failed`.
    fn exhaust_retries(ctrl: &mut PathMigrationController) {
        loop {
            burn_ticks(ctrl, CHALLENGE_TIMEOUT_TICKS);
            match ctrl.tick() {
                Some(MigrationEvent::Failed) => break,
                Some(MigrationEvent::Retransmit(_)) => {}
                other => panic!("unexpected event during exhaustion: {other:?}"),
            }
        }
    }
}

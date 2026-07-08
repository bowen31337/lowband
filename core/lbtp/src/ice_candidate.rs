//! ICE candidate gathering — Feature 2.
//!
//! # Overview
//!
//! Gathers the three candidate classes defined by RFC 8445 §5.1.1 and signals
//! each one to the caller as soon as it becomes available:
//!
//! | Class | Source | Priority type preference |
//! |---|---|---|
//! | **Host** | Local UDP socket addresses | 126 |
//! | **Server-reflexive** (srflx) | STUN Binding response (XOR-MAPPED-ADDRESS) | 100 |
//! | **Relayed** | TURN Allocate response (XOR-RELAYED-ADDRESS) | 0 |
//!
//! # Gathering sequence
//!
//! 1. Call [`IceCandidateGatherer::start`] with the local host socket addresses.
//!    The method returns all host [`IceCandidateEvent::CandidateReady`] events
//!    followed by a [`IceCandidateEvent::SendStunRequest`] event; the caller
//!    must send a STUN Binding request to its configured STUN server.
//!
//! 2. When the STUN Binding response arrives, call
//!    [`IceCandidateGatherer::on_stun_response`] with the mapped address.
//!    Returns the server-reflexive candidate followed by
//!    [`IceCandidateEvent::SendTurnAllocate`]; the caller must send a TURN
//!    Allocate request.
//!
//! 3. When the TURN allocation succeeds, call
//!    [`IceCandidateGatherer::on_turn_allocated`] with the relay address.
//!    Returns the relay candidate followed by
//!    [`IceCandidateEvent::GatheringComplete`].
//!
//! # Timeout handling
//!
//! [`IceCandidateGatherer::tick`] must be called once per 10 Hz control tick.
//! In the `GatheringServerReflexive` state, expired ticks produce
//! [`IceCandidateEvent::RetransmitStun`] until
//! [`ICE_STUN_MAX_RETRIES`] is exhausted, then the gatherer skips to TURN.
//! Equivalently for the `GatheringRelayed` state with [`ICE_TURN_MAX_RETRIES`].
//!
//! # Priority formula (RFC 8445 §5.1.2.1)
//!
//! ```text
//! priority = (2²⁴ × type_pref) | (2⁸ × local_pref) | (256 − component_id)
//! ```
//!
//! Component ID is fixed at 1 (single data stream).  Local preference is
//! 65535 for the first address and decrements by one for each additional host
//! address, ranking candidates from preferred to least-preferred.
//!
//! # Integration
//!
//! ```rust
//! use std::net::SocketAddr;
//! use lowband_lbtp::ice_candidate::{
//!     IceCandidateGatherer, IceCandidateEvent, IceCandidateType,
//! };
//!
//! let host: SocketAddr = "192.168.1.42:5000".parse().unwrap();
//! let stun_mapped: SocketAddr = "203.0.113.1:12345".parse().unwrap();
//! let turn_relay: SocketAddr = "198.51.100.1:54321".parse().unwrap();
//!
//! let mut g = IceCandidateGatherer::new();
//!
//! // Gather host candidates and trigger STUN.
//! let start_events = g.start(&[host]);
//! assert!(start_events.iter().any(|e| matches!(e, IceCandidateEvent::SendStunRequest)));
//!
//! // STUN response → server-reflexive candidate + trigger TURN.
//! let stun_events = g.on_stun_response(stun_mapped).unwrap();
//! assert!(stun_events.iter().any(|e| matches!(e, IceCandidateEvent::SendTurnAllocate)));
//!
//! // TURN response → relay candidate + gathering complete.
//! let turn_events = g.on_turn_allocated(turn_relay).unwrap();
//! assert!(g.is_complete());
//! ```

use std::net::SocketAddr;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Control ticks before a STUN Binding request times out.
///
/// At the nominal 10 Hz rate, 5 ticks = 500 ms — matching the initial RTO
/// in RFC 8489 §7.2.1 and consistent with [`crate::hole_punch`] probe pacing.
pub const ICE_STUN_TIMEOUT_TICKS: u32 = 5;

/// Maximum STUN Binding retransmissions before skipping to TURN gathering.
///
/// 2 retransmissions × 500 ms = 1.5 s total STUN budget before falling through.
pub const ICE_STUN_MAX_RETRIES: u8 = 2;

/// Control ticks before a TURN Allocate request times out.
///
/// At the nominal 10 Hz rate, 10 ticks = 1 000 ms — TURN round trips are
/// typically higher latency than direct STUN.
pub const ICE_TURN_TIMEOUT_TICKS: u32 = 10;

/// Maximum TURN Allocate retransmissions before declaring gathering complete
/// without a relay candidate.
///
/// 2 retransmissions × 1 000 ms = 3 s total TURN budget.
pub const ICE_TURN_MAX_RETRIES: u8 = 2;

/// RFC 8445 §5.1.2.1 type-preference value for host candidates (highest).
pub const ICE_HOST_TYPE_PREFERENCE: u32 = 126;

/// RFC 8445 §5.1.2.1 type-preference value for server-reflexive candidates.
pub const ICE_SRFLX_TYPE_PREFERENCE: u32 = 100;

/// RFC 8445 §5.1.2.1 type-preference value for relayed candidates (lowest).
pub const ICE_RELAY_TYPE_PREFERENCE: u32 = 0;

/// Local preference for the first (most-preferred) host address.
///
/// Decrements by one per additional host address so the first interface is
/// ranked above subsequent ones.
pub const ICE_LOCAL_PREFERENCE_BASE: u32 = 65_535;

// Fixed component ID: LBTP carries one multiplexed data stream.
const ICE_COMPONENT_ID: u32 = 1;

/// Compute the RFC 8445 §5.1.2.1 candidate priority.
///
/// `priority = (2²⁴ × type_pref) | (2⁸ × local_pref) | (256 − component_id)`
#[inline]
pub fn ice_priority(type_pref: u32, local_pref: u32) -> u32 {
    (type_pref << 24) | (local_pref << 8) | (256 - ICE_COMPONENT_ID)
}

// ── IceCandidateType ──────────────────────────────────────────────────────────

/// The three candidate classes defined by RFC 8445 §5.1.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IceCandidateType {
    /// Local UDP socket address on a network interface.
    Host,
    /// NAT-mapped address returned by a STUN Binding response.
    ServerReflexive,
    /// TURN-allocated relay address used when direct paths fail.
    Relayed,
}

// ── IceCandidate ──────────────────────────────────────────────────────────────

/// A single ICE candidate ready to be signaled to the remote peer.
///
/// `priority` and `foundation` follow the RFC 8445 definitions; callers should
/// forward both values verbatim in the signaling exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCandidate {
    /// Candidate class.
    pub candidate_type: IceCandidateType,
    /// Transport address of this candidate.
    pub addr: SocketAddr,
    /// RFC 8445 §5.1.2.1 priority — higher is preferred.
    pub priority: u32,
    /// RFC 8445 §5.1.1.3 foundation — opaque identifier shared by candidates
    /// with the same type, base address, protocol, and STUN/TURN server.
    pub foundation: u32,
}

impl IceCandidate {
    fn host(addr: SocketAddr, local_pref: u32, foundation: u32) -> Self {
        Self {
            candidate_type: IceCandidateType::Host,
            addr,
            priority: ice_priority(ICE_HOST_TYPE_PREFERENCE, local_pref),
            foundation,
        }
    }

    fn server_reflexive(addr: SocketAddr, foundation: u32) -> Self {
        Self {
            candidate_type: IceCandidateType::ServerReflexive,
            addr,
            priority: ice_priority(ICE_SRFLX_TYPE_PREFERENCE, ICE_LOCAL_PREFERENCE_BASE),
            foundation,
        }
    }

    fn relayed(addr: SocketAddr, foundation: u32) -> Self {
        Self {
            candidate_type: IceCandidateType::Relayed,
            addr,
            priority: ice_priority(ICE_RELAY_TYPE_PREFERENCE, ICE_LOCAL_PREFERENCE_BASE),
            foundation,
        }
    }
}

// ── IceCandidateEvent ─────────────────────────────────────────────────────────

/// Events emitted by [`IceCandidateGatherer`] methods and
/// [`IceCandidateGatherer::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IceCandidateEvent {
    /// A new candidate is available — signal it to the remote peer.
    CandidateReady(IceCandidate),
    /// Send a STUN Binding request to the configured STUN server now.
    SendStunRequest,
    /// STUN request timed out; re-send the Binding request.
    RetransmitStun,
    /// Send a TURN Allocate request to the configured TURN server now.
    SendTurnAllocate,
    /// TURN Allocate timed out; re-send the Allocate request.
    RetransmitTurn,
    /// All candidate types have been gathered (or timed out).
    GatheringComplete,
}

// ── GatherState ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatherState {
    Idle,
    GatheringServerReflexive {
        retries_left: u8,
        ticks_remaining: u32,
    },
    GatheringRelayed {
        retries_left: u8,
        ticks_remaining: u32,
    },
    Complete,
}

// ── IceCandidateGatherer ──────────────────────────────────────────────────────

/// ICE candidate gatherer — Feature 2.
///
/// One instance per peer session.  Cheap to construct; zero heap allocation
/// beyond the event vecs returned from methods.
///
/// See the [module-level documentation](self) for the integration pattern.
#[derive(Debug)]
pub struct IceCandidateGatherer {
    state: GatherState,
    /// Monotonically increasing foundation counter.
    next_foundation: u32,
}

impl Default for IceCandidateGatherer {
    fn default() -> Self {
        Self::new()
    }
}

impl IceCandidateGatherer {
    /// Create a new gatherer in the `Idle` state.
    pub fn new() -> Self {
        Self {
            state: GatherState::Idle,
            next_foundation: 1,
        }
    }

    /// Begin gathering candidates from the provided local socket addresses.
    ///
    /// Returns a `Vec` containing:
    /// - One [`IceCandidateEvent::CandidateReady`] event per host address,
    ///   ordered from highest to lowest priority.
    /// - A trailing [`IceCandidateEvent::SendStunRequest`] event directing
    ///   the caller to send a STUN Binding request to its STUN server.
    ///
    /// The gatherer transitions to `GatheringServerReflexive` and arms the
    /// STUN timeout timer.  Calling `start` again while gathering is in
    /// progress resets the state machine (useful for ICE restarts).
    pub fn start(&mut self, host_addrs: &[SocketAddr]) -> Vec<IceCandidateEvent> {
        let mut events = Vec::with_capacity(host_addrs.len() + 1);

        for (i, &addr) in host_addrs.iter().enumerate() {
            let local_pref = ICE_LOCAL_PREFERENCE_BASE.saturating_sub(i as u32);
            let foundation = self.next_foundation;
            self.next_foundation += 1;
            events.push(IceCandidateEvent::CandidateReady(IceCandidate::host(
                addr, local_pref, foundation,
            )));
        }

        self.state = GatherState::GatheringServerReflexive {
            retries_left: ICE_STUN_MAX_RETRIES,
            ticks_remaining: ICE_STUN_TIMEOUT_TICKS,
        };
        events.push(IceCandidateEvent::SendStunRequest);

        events
    }

    /// Notify the gatherer that a STUN Binding response arrived.
    ///
    /// Emits the server-reflexive candidate from `mapped_addr` followed by
    /// [`IceCandidateEvent::SendTurnAllocate`], directing the caller to send
    /// a TURN Allocate request.  Transitions to `GatheringRelayed`.
    ///
    /// Returns `None` when the gatherer is not in `GatheringServerReflexive`
    /// state (duplicate or stale response — safe to ignore).
    pub fn on_stun_response(&mut self, mapped_addr: SocketAddr) -> Option<Vec<IceCandidateEvent>> {
        if !matches!(self.state, GatherState::GatheringServerReflexive { .. }) {
            return None;
        }
        let foundation = self.next_foundation;
        self.next_foundation += 1;
        let candidate = IceCandidate::server_reflexive(mapped_addr, foundation);
        self.state = GatherState::GatheringRelayed {
            retries_left: ICE_TURN_MAX_RETRIES,
            ticks_remaining: ICE_TURN_TIMEOUT_TICKS,
        };
        Some(vec![
            IceCandidateEvent::CandidateReady(candidate),
            IceCandidateEvent::SendTurnAllocate,
        ])
    }

    /// Notify the gatherer that a TURN Allocate response arrived.
    ///
    /// Emits the relay candidate from `relay_addr` followed by
    /// [`IceCandidateEvent::GatheringComplete`].  Transitions to `Complete`.
    ///
    /// Returns `None` when the gatherer is not in `GatheringRelayed` state
    /// (duplicate or stale response — safe to ignore).
    pub fn on_turn_allocated(&mut self, relay_addr: SocketAddr) -> Option<Vec<IceCandidateEvent>> {
        if !matches!(self.state, GatherState::GatheringRelayed { .. }) {
            return None;
        }
        let foundation = self.next_foundation;
        self.next_foundation += 1;
        let candidate = IceCandidate::relayed(relay_addr, foundation);
        self.state = GatherState::Complete;
        Some(vec![
            IceCandidateEvent::CandidateReady(candidate),
            IceCandidateEvent::GatheringComplete,
        ])
    }

    /// Advance the gather timer by one control tick.
    ///
    /// Must be called once per 10 Hz control tick.  Returns:
    ///
    /// - `Some(RetransmitStun)` — STUN timer expired, retries remain; re-send
    ///   the Binding request.
    /// - `Some(SendTurnAllocate)` — STUN retries exhausted with no response;
    ///   skip srflx and begin TURN gathering.
    /// - `Some(RetransmitTurn)` — TURN timer expired, retries remain; re-send
    ///   the Allocate request.
    /// - `Some(GatheringComplete)` — TURN retries exhausted with no response;
    ///   gathering ends without a relay candidate.
    /// - `None` — timer still running or gatherer is `Idle` / `Complete`.
    pub fn tick(&mut self) -> Option<IceCandidateEvent> {
        match &mut self.state {
            GatherState::GatheringServerReflexive {
                retries_left,
                ticks_remaining,
            } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                // Timer at zero.
                let retries = *retries_left;
                if retries == 0 {
                    // No STUN response after all retries; skip to TURN.
                    self.state = GatherState::GatheringRelayed {
                        retries_left: ICE_TURN_MAX_RETRIES,
                        ticks_remaining: ICE_TURN_TIMEOUT_TICKS,
                    };
                    return Some(IceCandidateEvent::SendTurnAllocate);
                }
                *retries_left = retries - 1;
                *ticks_remaining = ICE_STUN_TIMEOUT_TICKS;
                Some(IceCandidateEvent::RetransmitStun)
            }

            GatherState::GatheringRelayed {
                retries_left,
                ticks_remaining,
            } => {
                if *ticks_remaining > 0 {
                    *ticks_remaining -= 1;
                    return None;
                }
                let retries = *retries_left;
                if retries == 0 {
                    // No TURN response after all retries; gathering ends.
                    self.state = GatherState::Complete;
                    return Some(IceCandidateEvent::GatheringComplete);
                }
                *retries_left = retries - 1;
                *ticks_remaining = ICE_TURN_TIMEOUT_TICKS;
                Some(IceCandidateEvent::RetransmitTurn)
            }

            _ => None,
        }
    }

    /// Whether gathering has not yet started.
    pub fn is_idle(&self) -> bool {
        self.state == GatherState::Idle
    }

    /// Whether a STUN or TURN request is in flight.
    pub fn is_gathering(&self) -> bool {
        matches!(
            self.state,
            GatherState::GatheringServerReflexive { .. } | GatherState::GatheringRelayed { .. }
        )
    }

    /// Whether all candidate types have been gathered (or timed out).
    pub fn is_complete(&self) -> bool {
        self.state == GatherState::Complete
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), port)
    }

    fn stun_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), 12345)
    }

    fn turn_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)), 54321)
    }

    fn burn_ticks(g: &mut IceCandidateGatherer, n: u32) {
        for _ in 0..n {
            g.tick();
        }
    }

    // ── Construction ──────────────────────────────────────────────────────────

    #[test]
    fn new_is_idle() {
        let g = IceCandidateGatherer::new();
        assert!(g.is_idle());
        assert!(!g.is_gathering());
        assert!(!g.is_complete());
    }

    #[test]
    fn default_equals_new() {
        let a = IceCandidateGatherer::new();
        let b = IceCandidateGatherer::default();
        assert_eq!(a.is_idle(), b.is_idle());
        assert_eq!(a.is_gathering(), b.is_gathering());
        assert_eq!(a.is_complete(), b.is_complete());
    }

    #[test]
    fn tick_idle_returns_none() {
        let mut g = IceCandidateGatherer::new();
        assert_eq!(g.tick(), None);
    }

    // ── start(): host candidates ──────────────────────────────────────────────

    #[test]
    fn start_emits_host_candidate_per_address() {
        let mut g = IceCandidateGatherer::new();
        let addrs = [addr(5000), addr(5001), addr(5002)];
        let events = g.start(&addrs);
        let host_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, IceCandidateEvent::CandidateReady(c)
                if c.candidate_type == IceCandidateType::Host))
            .collect();
        assert_eq!(host_events.len(), 3, "one host candidate per address");
    }

    #[test]
    fn start_host_candidate_address_matches_input() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[addr(5000)]);
        if let IceCandidateEvent::CandidateReady(c) = &events[0] {
            assert_eq!(c.addr, addr(5000));
            assert_eq!(c.candidate_type, IceCandidateType::Host);
        } else {
            panic!("first event must be a host CandidateReady");
        }
    }

    #[test]
    fn start_last_event_is_send_stun_request() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[addr(5000)]);
        assert_eq!(*events.last().unwrap(), IceCandidateEvent::SendStunRequest);
    }

    #[test]
    fn start_no_host_addrs_still_sends_stun_request() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[]);
        assert_eq!(events, vec![IceCandidateEvent::SendStunRequest]);
    }

    #[test]
    fn start_transitions_to_gathering() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        assert!(g.is_gathering());
        assert!(!g.is_idle());
        assert!(!g.is_complete());
    }

    #[test]
    fn start_host_priorities_decrease_with_index() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[addr(5000), addr(5001), addr(5002)]);
        let priorities: Vec<u32> = events
            .iter()
            .filter_map(|e| {
                if let IceCandidateEvent::CandidateReady(c) = e {
                    if c.candidate_type == IceCandidateType::Host {
                        return Some(c.priority);
                    }
                }
                None
            })
            .collect();
        for i in 0..priorities.len() - 1 {
            assert!(
                priorities[i] > priorities[i + 1],
                "host[{i}] priority must exceed host[{}] priority", i + 1
            );
        }
    }

    #[test]
    fn start_host_priority_type_pref_is_126() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[addr(5000)]);
        if let IceCandidateEvent::CandidateReady(c) = &events[0] {
            assert_eq!(c.priority >> 24, ICE_HOST_TYPE_PREFERENCE,
                "host type-preference must be 126");
        }
    }

    #[test]
    fn start_assigns_unique_foundations_to_host_candidates() {
        let mut g = IceCandidateGatherer::new();
        let events = g.start(&[addr(5000), addr(5001), addr(5002)]);
        let foundations: Vec<u32> = events
            .iter()
            .filter_map(|e| {
                if let IceCandidateEvent::CandidateReady(c) = e {
                    Some(c.foundation)
                } else {
                    None
                }
            })
            .collect();
        let unique: std::collections::HashSet<_> = foundations.iter().collect();
        assert_eq!(unique.len(), foundations.len(), "all host foundations must be unique");
    }

    #[test]
    fn start_while_gathering_resets_state_machine() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        // Restart.
        let events = g.start(&[addr(5001)]);
        assert!(g.is_gathering(), "gatherer must be in gathering state after restart");
        assert!(events.iter().any(|e| e == &IceCandidateEvent::SendStunRequest));
    }

    // ── tick(): STUN timeout / retransmit ────────────────────────────────────

    #[test]
    fn tick_returns_none_while_stun_timer_running() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        for tick in 0..ICE_STUN_TIMEOUT_TICKS {
            assert!(
                g.tick().is_none(),
                "tick {tick}: must return None while STUN timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_retransmit_stun_when_timer_expires_with_retries() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        burn_ticks(&mut g, ICE_STUN_TIMEOUT_TICKS);
        assert_eq!(g.tick(), Some(IceCandidateEvent::RetransmitStun));
    }

    #[test]
    fn retransmit_stun_resets_timer() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        burn_ticks(&mut g, ICE_STUN_TIMEOUT_TICKS);
        g.tick(); // RetransmitStun; timer resets.
        for tick in 0..ICE_STUN_TIMEOUT_TICKS {
            assert!(
                g.tick().is_none(),
                "tick {tick} after retransmit: timer must restart"
            );
        }
    }

    #[test]
    fn stun_retransmit_count_matches_max_retries() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        let mut count: u8 = 0;
        loop {
            burn_ticks(&mut g, ICE_STUN_TIMEOUT_TICKS);
            match g.tick() {
                Some(IceCandidateEvent::RetransmitStun) => count += 1,
                Some(IceCandidateEvent::SendTurnAllocate) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(count, ICE_STUN_MAX_RETRIES, "exactly ICE_STUN_MAX_RETRIES retransmissions");
    }

    #[test]
    fn stun_exhaustion_emits_send_turn_allocate() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        // Exhaust all STUN retries.
        for _ in 0..=ICE_STUN_MAX_RETRIES {
            burn_ticks(&mut g, ICE_STUN_TIMEOUT_TICKS);
            g.tick();
        }
        assert!(g.is_gathering(), "must still be gathering (now GatheringRelayed)");
    }

    #[test]
    fn after_stun_exhaustion_tick_drives_turn_retransmit() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        // Exhaust STUN retries → SendTurnAllocate.
        for _ in 0..=ICE_STUN_MAX_RETRIES {
            burn_ticks(&mut g, ICE_STUN_TIMEOUT_TICKS);
            g.tick();
        }
        // Now in GatheringRelayed; TURN timeout drives RetransmitTurn.
        burn_ticks(&mut g, ICE_TURN_TIMEOUT_TICKS);
        assert_eq!(g.tick(), Some(IceCandidateEvent::RetransmitTurn));
    }

    // ── on_stun_response() ────────────────────────────────────────────────────

    #[test]
    fn stun_response_emits_srflx_candidate() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        let events = g.on_stun_response(stun_addr()).unwrap();
        let srflx = events.iter().find_map(|e| {
            if let IceCandidateEvent::CandidateReady(c) = e {
                if c.candidate_type == IceCandidateType::ServerReflexive {
                    return Some(c);
                }
            }
            None
        });
        assert!(srflx.is_some(), "must emit a server-reflexive candidate");
        assert_eq!(srflx.unwrap().addr, stun_addr());
    }

    #[test]
    fn stun_response_includes_send_turn_allocate() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        let events = g.on_stun_response(stun_addr()).unwrap();
        assert!(
            events.contains(&IceCandidateEvent::SendTurnAllocate),
            "stun_response must include SendTurnAllocate"
        );
    }

    #[test]
    fn stun_response_transitions_to_gathering_relayed() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        assert!(g.is_gathering());
        assert!(!g.is_complete());
    }

    #[test]
    fn stun_response_srflx_priority_type_pref_is_100() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        let events = g.on_stun_response(stun_addr()).unwrap();
        let srflx = events.iter().find_map(|e| {
            if let IceCandidateEvent::CandidateReady(c) = e {
                if c.candidate_type == IceCandidateType::ServerReflexive { Some(c) } else { None }
            } else {
                None
            }
        }).unwrap();
        assert_eq!(srflx.priority >> 24, ICE_SRFLX_TYPE_PREFERENCE,
            "srflx type-preference must be 100");
    }

    #[test]
    fn stun_response_srflx_priority_lower_than_host() {
        let mut g = IceCandidateGatherer::new();
        let start_events = g.start(&[addr(5000)]);
        let host_priority = if let IceCandidateEvent::CandidateReady(c) = &start_events[0] {
            c.priority
        } else {
            panic!("first event must be host candidate");
        };
        let stun_events = g.on_stun_response(stun_addr()).unwrap();
        let srflx_priority = if let IceCandidateEvent::CandidateReady(c) = &stun_events[0] {
            c.priority
        } else {
            panic!("first stun event must be srflx candidate");
        };
        assert!(
            host_priority > srflx_priority,
            "host priority ({host_priority}) must exceed srflx priority ({srflx_priority})"
        );
    }

    #[test]
    fn stun_response_ignored_when_not_gathering_srflx() {
        let mut g = IceCandidateGatherer::new();
        // Idle state.
        assert!(g.on_stun_response(stun_addr()).is_none());
    }

    #[test]
    fn stun_response_ignored_when_in_gathering_relayed_state() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr()); // → GatheringRelayed
        // Duplicate / stale response must be a no-op.
        assert!(g.on_stun_response(stun_addr()).is_none());
    }

    #[test]
    fn stun_response_foundation_differs_from_host_foundations() {
        let mut g = IceCandidateGatherer::new();
        let start_events = g.start(&[addr(5000)]);
        let host_foundation = if let IceCandidateEvent::CandidateReady(c) = &start_events[0] {
            c.foundation
        } else {
            panic!()
        };
        let stun_events = g.on_stun_response(stun_addr()).unwrap();
        let srflx_foundation = if let IceCandidateEvent::CandidateReady(c) = &stun_events[0] {
            c.foundation
        } else {
            panic!()
        };
        assert_ne!(host_foundation, srflx_foundation, "foundations must be distinct");
    }

    // ── tick(): TURN timeout / retransmit ─────────────────────────────────────

    #[test]
    fn tick_returns_none_while_turn_timer_running() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        for tick in 0..ICE_TURN_TIMEOUT_TICKS {
            assert!(
                g.tick().is_none(),
                "tick {tick}: must return None while TURN timer is running"
            );
        }
    }

    #[test]
    fn tick_returns_retransmit_turn_when_turn_timer_expires() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        burn_ticks(&mut g, ICE_TURN_TIMEOUT_TICKS);
        assert_eq!(g.tick(), Some(IceCandidateEvent::RetransmitTurn));
    }

    #[test]
    fn turn_retransmit_count_matches_max_retries() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        let mut count: u8 = 0;
        loop {
            burn_ticks(&mut g, ICE_TURN_TIMEOUT_TICKS);
            match g.tick() {
                Some(IceCandidateEvent::RetransmitTurn) => count += 1,
                Some(IceCandidateEvent::GatheringComplete) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(count, ICE_TURN_MAX_RETRIES, "exactly ICE_TURN_MAX_RETRIES retransmissions");
    }

    #[test]
    fn turn_exhaustion_emits_gathering_complete() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        for _ in 0..=ICE_TURN_MAX_RETRIES {
            burn_ticks(&mut g, ICE_TURN_TIMEOUT_TICKS);
            g.tick();
        }
        assert!(g.is_complete(), "gatherer must be complete after TURN exhaustion");
    }

    #[test]
    fn tick_complete_returns_none() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        g.on_turn_allocated(turn_addr());
        for _ in 0..5 {
            assert_eq!(g.tick(), None, "tick in Complete state must be a no-op");
        }
    }

    // ── on_turn_allocated() ───────────────────────────────────────────────────

    #[test]
    fn turn_allocated_emits_relay_candidate() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        let events = g.on_turn_allocated(turn_addr()).unwrap();
        let relay = events.iter().find_map(|e| {
            if let IceCandidateEvent::CandidateReady(c) = e {
                if c.candidate_type == IceCandidateType::Relayed { Some(c) } else { None }
            } else {
                None
            }
        });
        assert!(relay.is_some(), "must emit a relay candidate");
        assert_eq!(relay.unwrap().addr, turn_addr());
    }

    #[test]
    fn turn_allocated_includes_gathering_complete() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        let events = g.on_turn_allocated(turn_addr()).unwrap();
        assert!(
            events.contains(&IceCandidateEvent::GatheringComplete),
            "on_turn_allocated must include GatheringComplete"
        );
    }

    #[test]
    fn turn_allocated_transitions_to_complete() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        g.on_turn_allocated(turn_addr());
        assert!(g.is_complete());
        assert!(!g.is_gathering());
    }

    #[test]
    fn turn_allocated_relay_priority_type_pref_is_zero() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        let events = g.on_turn_allocated(turn_addr()).unwrap();
        let relay = events.iter().find_map(|e| {
            if let IceCandidateEvent::CandidateReady(c) = e {
                if c.candidate_type == IceCandidateType::Relayed { Some(c) } else { None }
            } else {
                None
            }
        }).unwrap();
        assert_eq!(relay.priority >> 24, ICE_RELAY_TYPE_PREFERENCE,
            "relay type-preference must be 0");
    }

    #[test]
    fn relay_priority_lower_than_srflx_priority() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        let stun_events = g.on_stun_response(stun_addr()).unwrap();
        let srflx_priority = if let IceCandidateEvent::CandidateReady(c) = &stun_events[0] {
            c.priority
        } else {
            panic!()
        };
        let turn_events = g.on_turn_allocated(turn_addr()).unwrap();
        let relay_priority = if let IceCandidateEvent::CandidateReady(c) = &turn_events[0] {
            c.priority
        } else {
            panic!()
        };
        assert!(
            srflx_priority > relay_priority,
            "srflx priority ({srflx_priority}) must exceed relay priority ({relay_priority})"
        );
    }

    #[test]
    fn turn_allocated_ignored_when_idle() {
        let mut g = IceCandidateGatherer::new();
        assert!(g.on_turn_allocated(turn_addr()).is_none());
    }

    #[test]
    fn turn_allocated_ignored_when_gathering_srflx() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        // Still in GatheringServerReflexive — relay response must be ignored.
        assert!(g.on_turn_allocated(turn_addr()).is_none());
    }

    #[test]
    fn turn_allocated_ignored_when_complete() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        g.on_turn_allocated(turn_addr());
        // Duplicate response must be a no-op.
        assert!(g.on_turn_allocated(turn_addr()).is_none());
    }

    #[test]
    fn turn_allocated_after_retransmit_accepted() {
        let mut g = IceCandidateGatherer::new();
        g.start(&[addr(5000)]);
        g.on_stun_response(stun_addr());
        // Let one TURN probe time out.
        burn_ticks(&mut g, ICE_TURN_TIMEOUT_TICKS);
        g.tick(); // RetransmitTurn
        // Server finally responds.
        let events = g.on_turn_allocated(turn_addr()).unwrap();
        assert!(events.contains(&IceCandidateEvent::GatheringComplete));
        assert!(g.is_complete());
    }

    // ── Full happy path ───────────────────────────────────────────────────────

    #[test]
    fn full_gather_sequence_host_srflx_relay() {
        let mut g = IceCandidateGatherer::new();

        // Host candidates.
        let start_events = g.start(&[addr(5000), addr(5001)]);
        let host_count = start_events
            .iter()
            .filter(|e| matches!(e, IceCandidateEvent::CandidateReady(c)
                if c.candidate_type == IceCandidateType::Host))
            .count();
        assert_eq!(host_count, 2);
        assert!(start_events.contains(&IceCandidateEvent::SendStunRequest));

        // Server-reflexive candidate.
        let stun_events = g.on_stun_response(stun_addr()).unwrap();
        let srflx_count = stun_events
            .iter()
            .filter(|e| matches!(e, IceCandidateEvent::CandidateReady(c)
                if c.candidate_type == IceCandidateType::ServerReflexive))
            .count();
        assert_eq!(srflx_count, 1);
        assert!(stun_events.contains(&IceCandidateEvent::SendTurnAllocate));

        // Relay candidate.
        let turn_events = g.on_turn_allocated(turn_addr()).unwrap();
        let relay_count = turn_events
            .iter()
            .filter(|e| matches!(e, IceCandidateEvent::CandidateReady(c)
                if c.candidate_type == IceCandidateType::Relayed))
            .count();
        assert_eq!(relay_count, 1);
        assert!(turn_events.contains(&IceCandidateEvent::GatheringComplete));

        assert!(g.is_complete());
    }

    #[test]
    fn all_foundations_unique_across_full_gather() {
        let mut g = IceCandidateGatherer::new();
        let mut foundations = Vec::new();

        let start_events = g.start(&[addr(5000), addr(5001)]);
        for e in &start_events {
            if let IceCandidateEvent::CandidateReady(c) = e {
                foundations.push(c.foundation);
            }
        }
        let stun_events = g.on_stun_response(stun_addr()).unwrap();
        for e in &stun_events {
            if let IceCandidateEvent::CandidateReady(c) = e {
                foundations.push(c.foundation);
            }
        }
        let turn_events = g.on_turn_allocated(turn_addr()).unwrap();
        for e in &turn_events {
            if let IceCandidateEvent::CandidateReady(c) = e {
                foundations.push(c.foundation);
            }
        }

        let unique: std::collections::HashSet<_> = foundations.iter().collect();
        assert_eq!(unique.len(), foundations.len(), "all foundations must be unique");
    }

    // ── Priority formula ──────────────────────────────────────────────────────

    #[test]
    fn ice_priority_host_beats_srflx_beats_relay() {
        let host = ice_priority(ICE_HOST_TYPE_PREFERENCE, ICE_LOCAL_PREFERENCE_BASE);
        let srflx = ice_priority(ICE_SRFLX_TYPE_PREFERENCE, ICE_LOCAL_PREFERENCE_BASE);
        let relay = ice_priority(ICE_RELAY_TYPE_PREFERENCE, ICE_LOCAL_PREFERENCE_BASE);
        assert!(host > srflx, "host must outrank srflx");
        assert!(srflx > relay, "srflx must outrank relay");
    }

    #[test]
    fn ice_priority_encodes_type_pref_in_upper_byte() {
        let p = ice_priority(126, 65535);
        assert_eq!(p >> 24, 126);
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn stun_timeout_ticks_corresponds_to_500ms_at_10hz() {
        assert_eq!(ICE_STUN_TIMEOUT_TICKS, 5, "5 ticks × 100 ms = 500 ms");
    }

    #[test]
    fn turn_timeout_ticks_corresponds_to_1000ms_at_10hz() {
        assert_eq!(ICE_TURN_TIMEOUT_TICKS, 10, "10 ticks × 100 ms = 1 000 ms");
    }

    #[test]
    fn host_type_preference_is_126() {
        assert_eq!(ICE_HOST_TYPE_PREFERENCE, 126);
    }

    #[test]
    fn srflx_type_preference_is_100() {
        assert_eq!(ICE_SRFLX_TYPE_PREFERENCE, 100);
    }

    #[test]
    fn relay_type_preference_is_zero() {
        assert_eq!(ICE_RELAY_TYPE_PREFERENCE, 0);
    }
}

//! TCP port 443 fallback transport — Feature 6.
//!
//! # Mechanism
//!
//! Corporate, hotel, and captive-portal networks frequently block all UDP
//! traffic.  When direct UDP hole-punch and TURN relay (Feature 5) both fail,
//! LBTP falls back to a single outbound TCP connection on port 443.  TCP is a
//! byte stream, so each LBTP datagram is wrapped with a 2-byte big-endian
//! length prefix so the receiver can reconstruct datagram boundaries.
//!
//! This module does not manage the TCP socket.  The transport event loop owns
//! the socket; this module provides the framing and penalty-accounting pieces:
//!
//! - [`TcpFramer`] — encodes LBTP datagrams for the TCP send path by
//!   prepending a 2-byte big-endian length prefix.
//! - [`TcpReassembler`] — reconstructs LBTP datagrams from a TCP receive
//!   byte stream using a two-state machine (header → body).
//! - [`TcpPenaltyTracker`] — smooths RTT measurements over the TCP path and
//!   reports the honest extra-latency penalty relative to a UDP baseline,
//!   feeding the UI warning (Feature 139).
//!
//! # Honest latency penalty
//!
//! TCP port 443 imposes additional latency beyond a direct UDP path:
//!
//! - **Head-of-line blocking** — a lost TCP segment stalls all following bytes
//!   regardless of datagram priority, turning every packet loss into a
//!   session-wide stall until retransmission arrives.
//! - **Congestion control** — TCP's own cwnd/ssthresh logic competes with
//!   LBTP's pacing and can introduce additional queuing.
//! - **Middle-box inspection** — deep-packet inspection at corporate firewalls
//!   adds variable processing delay.
//!
//! [`TcpPenaltyTracker::observe_rtt_ms`] feeds measured TCP RTTs into an
//! EWMA estimator (α = 0.125, matching RFC 9002 §5.3).  `penalty_ms()` returns
//! the difference between the smoothed TCP RTT and the caller-supplied UDP
//! baseline, floored at [`TCP_FALLBACK_PENALTY_FLOOR_MS`] so the UI label
//! never understates the cost.
//!
//! # Integration
//!
//! ```rust
//! use lowband_lbtp::tcp_fallback::{
//!     TcpFramer, TcpReassembler, TcpPenaltyTracker, TCP_FALLBACK_PORT,
//! };
//!
//! let framer = TcpFramer::new();
//! let mut reassembler = TcpReassembler::new();
//! let mut penalty = TcpPenaltyTracker::new(/* udp_baseline_ms */ 40);
//!
//! // Send path — one call per outbound LBTP datagram.
//! let datagram = b"hello lbtp";
//! let tcp_bytes = framer.encode(datagram).expect("datagram within 1200-byte limit");
//! // write tcp_bytes to the TCP socket on TCP_FALLBACK_PORT …
//!
//! // Receive path — feed whatever the TCP socket delivers.
//! let received: Vec<Vec<u8>> = reassembler.push(&tcp_bytes);
//! assert_eq!(received[0], datagram);
//!
//! // RTT observation — call once per ACK or per round-trip probe.
//! penalty.observe_rtt_ms(120);
//! println!("TCP penalty: {} ms", penalty.penalty_ms());
//! ```

// ── Constants ─────────────────────────────────────────────────────────────────

/// Well-known destination port for the TCP fallback path.
///
/// Port 443 (HTTPS) is chosen because it traverses virtually all corporate and
/// hotel firewalls that block UDP or arbitrary TCP ports.
pub const TCP_FALLBACK_PORT: u16 = 443;

/// Number of bytes in the length prefix prepended to each LBTP datagram on the
/// TCP stream (2-byte big-endian `u16`).
///
/// A 2-byte prefix supports frames up to 65 535 bytes — well above the 1 200-byte
/// LBTP datagram ceiling — while costing only 0.17 % overhead on a 1 200-byte frame.
pub const TCP_LENGTH_PREFIX_BYTES: usize = 2;

/// Maximum LBTP datagram size the reassembler will accept, in bytes.
///
/// Matches the LBTP 1 200-byte datagram ceiling (Feature 7).  The reassembler
/// rejects any frame whose length prefix exceeds this value and increments
/// [`TcpReassembler::oversized_rejected`] — the stream is assumed to have
/// lost sync and the controller resets to the header-wait state.
pub const TCP_MAX_DATAGRAM_BYTES: usize = 1200;

/// EWMA smoothing factor α applied to TCP RTT samples.
///
/// 0.125 (⅛) matches the RTT estimator weight in RFC 9002 §5.3 (QUIC loss
/// detection).  At this weight the effective window is 8 samples — stable
/// after roughly 8 RTTs while still tracking step changes within ~1 second
/// at a typical 10 Hz ACK rate.
pub const TCP_RTT_EWMA_ALPHA: f64 = 0.125;

/// Minimum honest latency penalty reported for the TCP port 443 fallback, in
/// milliseconds.
///
/// Even on an uncongested network TCP head-of-line blocking inflicts at least
/// this much extra latency on a real-time audio/video session.  The floor
/// prevents the UI label from showing "0 ms extra" on a temporarily quiet link
/// when the structural cost is still present.
pub const TCP_FALLBACK_PENALTY_FLOOR_MS: u32 = 30;

// ── TcpFramer ─────────────────────────────────────────────────────────────────

/// Encodes outbound LBTP datagrams for transmission over a TCP stream.
///
/// Each call to [`encode`](TcpFramer::encode) prepends a 2-byte big-endian
/// length field to the datagram bytes.  The struct carries no state.
#[derive(Debug, Default, Clone, Copy)]
pub struct TcpFramer;

impl TcpFramer {
    /// Create a new framer.
    pub fn new() -> Self {
        Self
    }

    /// Encode `datagram` as a length-prefixed TCP frame.
    ///
    /// Returns `Some(bytes)` where `bytes = [len_hi, len_lo, data...]`.
    ///
    /// Returns `None` if `datagram.len() > TCP_MAX_DATAGRAM_BYTES` — the
    /// caller should log and drop the datagram rather than silently truncating.
    pub fn encode(&self, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() > TCP_MAX_DATAGRAM_BYTES {
            return None;
        }
        let prefix = (datagram.len() as u16).to_be_bytes();
        let mut out = Vec::with_capacity(TCP_LENGTH_PREFIX_BYTES + datagram.len());
        out.extend_from_slice(&prefix);
        out.extend_from_slice(datagram);
        Some(out)
    }
}

// ── TcpReassembler ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum ReassemblerState {
    /// Waiting to accumulate a complete 2-byte length prefix.
    Header { buf: [u8; 2], filled: usize },
    /// Waiting to accumulate `expected` bytes of datagram body.
    Body { expected: usize, buf: Vec<u8> },
}

/// Reconstructs LBTP datagrams from a TCP byte stream.
///
/// Feed successive chunks of received TCP bytes via [`push`](TcpReassembler::push).
/// The reassembler buffers partial frames across calls and returns all
/// completed datagrams in each batch.
///
/// # Oversized frames
///
/// If a length prefix exceeds [`TCP_MAX_DATAGRAM_BYTES`] the reassembler
/// cannot safely advance to the body (the body length is unknown relative to
/// the remaining stream bytes), so it resets to the header-wait state and
/// increments [`oversized_rejected`](TcpReassembler::oversized_rejected).
/// The stream is considered desynced; the caller should close and reopen the
/// TCP connection.
#[derive(Debug)]
pub struct TcpReassembler {
    state: ReassemblerState,
    oversized_rejected: u32,
}

impl Default for TcpReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpReassembler {
    /// Create a new reassembler, ready to read a length prefix.
    pub fn new() -> Self {
        Self {
            state: ReassemblerState::Header { buf: [0u8; 2], filled: 0 },
            oversized_rejected: 0,
        }
    }

    /// Feed `bytes` from the TCP receive buffer and collect completed datagrams.
    ///
    /// May return zero, one, or multiple datagrams per call depending on how
    /// many complete frames are present in `bytes`.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut completed = Vec::new();
        let mut pos = 0;

        while pos < bytes.len() {
            // Temporarily move the state out so we can transition it freely.
            let state = core::mem::replace(
                &mut self.state,
                ReassemblerState::Header { buf: [0u8; 2], filled: 0 },
            );

            match state {
                ReassemblerState::Header { mut buf, mut filled } => {
                    let need = TCP_LENGTH_PREFIX_BYTES - filled;
                    let avail = bytes.len() - pos;
                    let copy = need.min(avail);
                    buf[filled..filled + copy].copy_from_slice(&bytes[pos..pos + copy]);
                    filled += copy;
                    pos += copy;

                    if filled == TCP_LENGTH_PREFIX_BYTES {
                        let len = u16::from_be_bytes(buf) as usize;
                        if len > TCP_MAX_DATAGRAM_BYTES {
                            self.oversized_rejected += 1;
                            // Reset to header; stream is desynced.
                            self.state = ReassemblerState::Header { buf: [0u8; 2], filled: 0 };
                        } else {
                            // len == 0 produces an empty body that completes immediately below.
                            self.state = ReassemblerState::Body {
                                expected: len,
                                buf: Vec::with_capacity(len),
                            };
                        }
                    } else {
                        self.state = ReassemblerState::Header { buf, filled };
                    }
                }

                ReassemblerState::Body { expected, mut buf } => {
                    let need = expected - buf.len();
                    let avail = bytes.len() - pos;
                    let copy = need.min(avail);
                    buf.extend_from_slice(&bytes[pos..pos + copy]);
                    pos += copy;

                    if buf.len() == expected {
                        if expected > 0 {
                            completed.push(buf);
                        }
                        self.state = ReassemblerState::Header { buf: [0u8; 2], filled: 0 };
                    } else {
                        self.state = ReassemblerState::Body { expected, buf };
                    }
                }
            }
        }

        completed
    }

    /// Number of frames whose length prefix exceeded [`TCP_MAX_DATAGRAM_BYTES`]
    /// and were discarded.  A non-zero count indicates stream desync.
    pub fn oversized_rejected(&self) -> u32 {
        self.oversized_rejected
    }
}

// ── TcpPenaltyTracker ─────────────────────────────────────────────────────────

/// Tracks the latency penalty of the TCP port 443 fallback relative to a UDP
/// baseline and exposes it to the UI layer (Feature 139).
///
/// Feed TCP RTT measurements via
/// [`observe_rtt_ms`](TcpPenaltyTracker::observe_rtt_ms).  The tracker smooths
/// them with an EWMA (α = [`TCP_RTT_EWMA_ALPHA`]) and reports
/// `penalty_ms()` as `max(smoothed_tcp_rtt − udp_baseline, TCP_FALLBACK_PENALTY_FLOOR_MS)`.
///
/// Before the first observation, `penalty_ms()` returns
/// [`TCP_FALLBACK_PENALTY_FLOOR_MS`] — a conservative honest estimate that
/// never understates the structural cost.
#[derive(Debug)]
pub struct TcpPenaltyTracker {
    udp_baseline_ms: u32,
    /// EWMA RTT over the TCP path in milliseconds (`f64` for sub-ms accuracy).
    /// `None` until the first observation.
    smoothed_rtt: Option<f64>,
}

impl TcpPenaltyTracker {
    /// Create a tracker with the given `udp_baseline_ms`.
    ///
    /// `udp_baseline_ms` is the expected RTT on a direct UDP path, obtained
    /// from ICE path probing or the TURN echo test before the fallback
    /// activated.  Pass 0 when no prior measurement is available; the floor
    /// will still apply.
    pub fn new(udp_baseline_ms: u32) -> Self {
        Self {
            udp_baseline_ms,
            smoothed_rtt: None,
        }
    }

    /// Record one TCP RTT measurement, in milliseconds.
    ///
    /// The first observation sets the smoothed RTT directly (no prior history).
    /// Subsequent observations blend in via EWMA:
    /// `s ← s × (1 − α) + rtt × α`
    pub fn observe_rtt_ms(&mut self, rtt_ms: u32) {
        let sample = rtt_ms as f64;
        self.smoothed_rtt = Some(match self.smoothed_rtt {
            None => sample,
            Some(prev) => prev * (1.0 - TCP_RTT_EWMA_ALPHA) + sample * TCP_RTT_EWMA_ALPHA,
        });
    }

    /// Honest latency penalty of the TCP path over the UDP baseline, in
    /// milliseconds.
    ///
    /// Returns `max(smoothed_tcp_rtt − udp_baseline, TCP_FALLBACK_PENALTY_FLOOR_MS)`.
    /// Before any observation the floor is returned, representing the minimum
    /// structural cost of TCP head-of-line blocking.
    pub fn penalty_ms(&self) -> u32 {
        let raw = match self.smoothed_rtt {
            None => 0,
            Some(s) => {
                let diff = s - self.udp_baseline_ms as f64;
                diff.max(0.0).round() as u32
            }
        };
        raw.max(TCP_FALLBACK_PENALTY_FLOOR_MS)
    }

    /// Smoothed TCP RTT in milliseconds, or `None` before the first sample.
    pub fn smoothed_rtt_ms(&self) -> Option<u32> {
        self.smoothed_rtt.map(|s| s.round() as u32)
    }

    /// Replace the UDP baseline used for penalty computation.
    ///
    /// Call this when a more accurate UDP measurement arrives (e.g. when
    /// re-probing via TURN while TCP is still active).
    pub fn set_udp_baseline_ms(&mut self, baseline_ms: u32) {
        self.udp_baseline_ms = baseline_ms;
    }

    /// The current UDP baseline in milliseconds.
    pub fn udp_baseline_ms(&self) -> u32 {
        self.udp_baseline_ms
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TcpFramer ────────────────────────────────────────────────────────────

    #[test]
    fn encode_prepends_big_endian_length_prefix() {
        let framer = TcpFramer::new();
        let data = b"hello";
        let encoded = framer.encode(data).unwrap();
        assert_eq!(&encoded[..2], &[0u8, 5u8], "length prefix must be big-endian u16 = 5");
        assert_eq!(&encoded[2..], data);
    }

    #[test]
    fn encode_length_prefix_matches_datagram_size() {
        let framer = TcpFramer::new();
        for size in [0, 1, 80, 100, 1200] {
            let data = vec![0xAAu8; size];
            let encoded = framer.encode(&data).unwrap();
            let prefix = u16::from_be_bytes([encoded[0], encoded[1]]) as usize;
            assert_eq!(prefix, size, "length prefix must equal datagram byte count for size {size}");
        }
    }

    #[test]
    fn encode_total_length_is_prefix_plus_data() {
        let framer = TcpFramer::new();
        let data = vec![0u8; 200];
        let encoded = framer.encode(&data).unwrap();
        assert_eq!(encoded.len(), TCP_LENGTH_PREFIX_BYTES + 200);
    }

    #[test]
    fn encode_accepts_max_size_datagram() {
        let framer = TcpFramer::new();
        let data = vec![0u8; TCP_MAX_DATAGRAM_BYTES];
        assert!(framer.encode(&data).is_some(), "max-size datagram must be accepted");
    }

    #[test]
    fn encode_rejects_oversized_datagram() {
        let framer = TcpFramer::new();
        let data = vec![0u8; TCP_MAX_DATAGRAM_BYTES + 1];
        assert!(framer.encode(&data).is_none(), "datagram exceeding limit must be rejected");
    }

    #[test]
    fn encode_empty_datagram() {
        let framer = TcpFramer::new();
        let encoded = framer.encode(b"").unwrap();
        assert_eq!(encoded, [0u8, 0u8], "empty datagram encodes to two zero bytes");
    }

    // ── TcpReassembler ────────────────────────────────────────────────────────

    #[test]
    fn single_datagram_in_one_push() {
        let framer = TcpFramer::new();
        let mut r = TcpReassembler::new();
        let data = b"lbtp frame";
        let encoded = framer.encode(data).unwrap();
        let out = r.push(&encoded);
        assert_eq!(out.len(), 1, "one push of a complete frame must yield one datagram");
        assert_eq!(out[0], data);
    }

    #[test]
    fn datagram_split_mid_header() {
        let framer = TcpFramer::new();
        let mut r = TcpReassembler::new();
        let data = b"split header test";
        let encoded = framer.encode(data).unwrap();

        // Send only the first byte of the length prefix.
        let a = r.push(&encoded[..1]);
        assert!(a.is_empty(), "first byte of header: no complete datagram yet");

        // Deliver the rest.
        let b = r.push(&encoded[1..]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0], data);
    }

    #[test]
    fn datagram_split_mid_body() {
        let framer = TcpFramer::new();
        let mut r = TcpReassembler::new();
        let data = b"body split";
        let encoded = framer.encode(data).unwrap();
        let mid = encoded.len() / 2;

        let a = r.push(&encoded[..mid]);
        assert!(a.is_empty(), "partial body: no datagram yet");

        let b = r.push(&encoded[mid..]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0], data);
    }

    #[test]
    fn datagram_split_at_every_byte_boundary() {
        let framer = TcpFramer::new();
        let data: Vec<u8> = (0u8..50).collect();
        let encoded = framer.encode(&data).unwrap();

        for split in 1..encoded.len() {
            let mut r = TcpReassembler::new();
            let a = r.push(&encoded[..split]);
            let b = r.push(&encoded[split..]);
            let all: Vec<Vec<u8>> = a.into_iter().chain(b).collect();
            assert_eq!(all.len(), 1, "split at byte {split} must yield exactly one datagram");
            assert_eq!(all[0], data, "split at byte {split}: data must match");
        }
    }

    #[test]
    fn multiple_datagrams_in_one_push() {
        let framer = TcpFramer::new();
        let mut r = TcpReassembler::new();

        let a = b"first";
        let b = b"second";
        let c = b"third";
        let mut buf = Vec::new();
        buf.extend_from_slice(&framer.encode(a).unwrap());
        buf.extend_from_slice(&framer.encode(b).unwrap());
        buf.extend_from_slice(&framer.encode(c).unwrap());

        let out = r.push(&buf);
        assert_eq!(out.len(), 3, "three concatenated frames must produce three datagrams");
        assert_eq!(out[0], a);
        assert_eq!(out[1], b);
        assert_eq!(out[2], c);
    }

    #[test]
    fn push_empty_slice_returns_empty() {
        let mut r = TcpReassembler::new();
        assert!(r.push(&[]).is_empty(), "empty push must return no datagrams");
    }

    #[test]
    fn oversized_length_increments_rejected_counter() {
        let mut r = TcpReassembler::new();
        // Craft a length prefix just over the limit.
        let bad_len = (TCP_MAX_DATAGRAM_BYTES as u16 + 1).to_be_bytes();
        r.push(&bad_len);
        assert_eq!(r.oversized_rejected(), 1, "oversized length prefix must increment counter");
    }

    #[test]
    fn new_reassembler_has_zero_oversized_rejected() {
        let r = TcpReassembler::new();
        assert_eq!(r.oversized_rejected(), 0);
    }

    // ── TcpFramer + TcpReassembler round-trip ─────────────────────────────────

    #[test]
    fn framer_reassembler_roundtrip_max_size_datagram() {
        let framer = TcpFramer::new();
        let mut r = TcpReassembler::new();
        let data: Vec<u8> = (0..TCP_MAX_DATAGRAM_BYTES).map(|i| i as u8).collect();
        let encoded = framer.encode(&data).unwrap();
        let out = r.push(&encoded);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], data);
    }

    #[test]
    fn framer_reassembler_byte_by_byte_delivery() {
        let framer = TcpFramer::new();
        let data = b"drip feed test payload";
        let encoded = framer.encode(data).unwrap();

        let mut r = TcpReassembler::new();
        let mut collected: Vec<Vec<u8>> = Vec::new();
        for &byte in &encoded {
            collected.extend(r.push(&[byte]));
        }
        assert_eq!(collected.len(), 1, "byte-by-byte delivery must yield one datagram");
        assert_eq!(collected[0], data);
    }

    // ── TcpPenaltyTracker ─────────────────────────────────────────────────────

    #[test]
    fn no_samples_returns_floor_penalty() {
        let tracker = TcpPenaltyTracker::new(40);
        assert_eq!(
            tracker.penalty_ms(),
            TCP_FALLBACK_PENALTY_FLOOR_MS,
            "before any observation the floor must be reported"
        );
    }

    #[test]
    fn smoothed_rtt_is_none_before_first_sample() {
        let tracker = TcpPenaltyTracker::new(40);
        assert!(tracker.smoothed_rtt_ms().is_none());
    }

    #[test]
    fn first_sample_sets_smoothed_rtt_exactly() {
        let mut tracker = TcpPenaltyTracker::new(40);
        tracker.observe_rtt_ms(120);
        assert_eq!(tracker.smoothed_rtt_ms(), Some(120));
    }

    #[test]
    fn penalty_above_floor_when_tcp_exceeds_udp_baseline() {
        let mut tracker = TcpPenaltyTracker::new(40);
        // First sample: 200 ms TCP RTT with 40 ms UDP baseline → 160 ms penalty.
        tracker.observe_rtt_ms(200);
        assert!(
            tracker.penalty_ms() >= TCP_FALLBACK_PENALTY_FLOOR_MS,
            "penalty must be at least the floor"
        );
        assert!(
            tracker.penalty_ms() > TCP_FALLBACK_PENALTY_FLOOR_MS,
            "penalty must exceed the floor when tcp rtt >> udp baseline"
        );
    }

    #[test]
    fn penalty_equals_floor_when_tcp_rtt_equals_udp_baseline() {
        let mut tracker = TcpPenaltyTracker::new(80);
        // TCP RTT matches UDP baseline → raw penalty = 0 → clamp to floor.
        tracker.observe_rtt_ms(80);
        assert_eq!(
            tracker.penalty_ms(),
            TCP_FALLBACK_PENALTY_FLOOR_MS,
            "penalty must be the floor when tcp rtt = udp baseline"
        );
    }

    #[test]
    fn penalty_equals_floor_when_tcp_rtt_below_udp_baseline() {
        // Unlikely in practice but the formula must not go negative.
        let mut tracker = TcpPenaltyTracker::new(100);
        tracker.observe_rtt_ms(50); // TCP faster than baseline somehow
        assert_eq!(
            tracker.penalty_ms(),
            TCP_FALLBACK_PENALTY_FLOOR_MS,
            "penalty must be at least the floor even when smoothed rtt < baseline"
        );
    }

    #[test]
    fn penalty_converges_toward_measured_excess() {
        // UDP baseline = 40 ms, TCP RTT = 200 ms → excess = 160 ms.
        // After many observations the EWMA converges near 200 ms.
        let mut tracker = TcpPenaltyTracker::new(40);
        for _ in 0..64 {
            tracker.observe_rtt_ms(200);
        }
        let penalty = tracker.penalty_ms();
        // After convergence: smoothed ≈ 200, penalty ≈ 160.  Allow ±5 ms for rounding.
        assert!(
            penalty >= 155 && penalty <= 165,
            "converged penalty must be near 160 ms but got {penalty}"
        );
    }

    #[test]
    fn ewma_blends_new_samples_with_history() {
        // Start with a high RTT, then deliver many low samples.
        let mut tracker = TcpPenaltyTracker::new(0);
        tracker.observe_rtt_ms(1000);
        for _ in 0..64 {
            tracker.observe_rtt_ms(50);
        }
        let s = tracker.smoothed_rtt_ms().unwrap();
        // After convergence the EWMA must be near 50, not 1000.
        assert!(s < 100, "EWMA must converge toward new stable value but got {s}");
    }

    #[test]
    fn set_udp_baseline_changes_penalty() {
        let mut tracker = TcpPenaltyTracker::new(40);
        tracker.observe_rtt_ms(200);
        let penalty_before = tracker.penalty_ms();

        // Raising the baseline reduces the penalty.
        tracker.set_udp_baseline_ms(180);
        let penalty_after = tracker.penalty_ms();

        assert!(
            penalty_after < penalty_before,
            "raising udp baseline must reduce reported penalty"
        );
    }

    #[test]
    fn udp_baseline_ms_reflects_set_value() {
        let tracker = TcpPenaltyTracker::new(55);
        assert_eq!(tracker.udp_baseline_ms(), 55);
    }

    #[test]
    fn set_udp_baseline_reflected_by_accessor() {
        let mut tracker = TcpPenaltyTracker::new(55);
        tracker.set_udp_baseline_ms(99);
        assert_eq!(tracker.udp_baseline_ms(), 99);
    }

    #[test]
    fn penalty_is_never_below_floor() {
        // Exhaustive check: random-ish RTT and baseline combinations.
        for udp in [0u32, 10, 40, 100, 500] {
            for tcp in [0u32, 10, 40, 80, 100, 200, 500, 1000] {
                let mut tracker = TcpPenaltyTracker::new(udp);
                tracker.observe_rtt_ms(tcp);
                assert!(
                    tracker.penalty_ms() >= TCP_FALLBACK_PENALTY_FLOOR_MS,
                    "penalty must be >= floor for udp={udp} tcp={tcp}"
                );
            }
        }
    }

    // ── Port constant ─────────────────────────────────────────────────────────

    #[test]
    fn tcp_fallback_port_is_443() {
        assert_eq!(TCP_FALLBACK_PORT, 443);
    }
}

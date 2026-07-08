//! Token-bucket paced send path — Feature 10.
//!
//! # Invariant
//!
//! Every outbound frame is dequeued from the [`Pacer`] only when the token
//! balance covers its byte cost.  No frame reaches the UDP socket via any path
//! that bypasses [`Connection::tick`] or [`Connection::tick_ns`], preventing
//! encoder bursts from flooding the uplink queue.
//!
//! # Event-loop contract
//!
//! ```rust
//! use lowband_lbtp::connection::Connection;
//! use lowband_lbtp::pacer::{ChannelId, PacerFrame};
//!
//! let mut conn = Connection::new(150_000.0);   // 150 kbps initial rate
//!
//! // Encode workers push frames here; the event loop drains SPSC channels
//! // into the connection before each tick.
//! conn.enqueue(PacerFrame::new(ChannelId::new(1), vec![0u8; 80])); // audio
//! conn.enqueue(PacerFrame::new(ChannelId::new(3), vec![0u8; 32])); // input
//!
//! // One call per event-loop iteration.
//! if let Some(datagram) = conn.tick() {
//!     // send datagram.frames over the UDP socket
//!     let _ = datagram;
//! }
//! ```
//!
//! In unit tests or simulations that cannot rely on wall-clock time, use
//! [`Connection::tick_ns`] to advance by a fixed number of nanoseconds instead.

use std::time::Instant;

use crate::pacer::{ChannelId, Pacer, PacerAggregatedDatagram, PacerFrame};

/// Paced transport connection send path.
///
/// Wraps a [`Pacer`] with monotonic timing and enforces the token-bucket
/// invariant on every outbound frame.
///
/// ## Thread safety
///
/// Not thread-safe.  The transport event loop is single-threaded; this type
/// must be called exclusively from that thread.
#[derive(Debug)]
pub struct Connection {
    pacer: Pacer,
    /// Wall-clock instant of the previous [`tick`](Connection::tick) call.
    /// Used to compute elapsed nanoseconds without the caller tracking time.
    last_tick: Instant,
}

impl Connection {
    /// Create a paced connection with `initial_rate_bps` bits-per-second send rate.
    pub fn new(initial_rate_bps: f64) -> Self {
        Self {
            pacer: Pacer::new(initial_rate_bps),
            last_tick: Instant::now(),
        }
    }

    /// Update the controlled send rate.
    ///
    /// Clamps the current token balance to the new burst cap immediately so a
    /// rate reduction takes effect without delay.  Delegates to
    /// [`Pacer::set_rate`].
    pub fn set_rate(&mut self, rate_bps: f64) {
        self.pacer.set_rate(rate_bps);
    }

    /// Current controlled send rate in bits per second.
    pub fn rate_bps(&self) -> f64 {
        self.pacer.rate_bps()
    }

    /// Enqueue a frame for paced transmission.
    ///
    /// The frame joins the channel's FIFO queue inside the pacer and will only
    /// be emitted once the token bucket has sufficient balance.
    pub fn enqueue(&mut self, frame: PacerFrame) {
        self.pacer.enqueue(frame);
    }

    /// Advance the token bucket by the elapsed wall-clock time and drain
    /// eligible frames into a single aggregated datagram.
    ///
    /// Records the current instant, computes nanoseconds elapsed since the
    /// previous call, grants the corresponding tokens to the pacer, then
    /// returns all frames that the token budget allows in one
    /// [`PacerAggregatedDatagram`] (Feature 18 coalescing).
    ///
    /// Returns `None` when the queue is empty or the token balance is
    /// insufficient for any waiting frame.
    pub fn tick(&mut self) -> Option<PacerAggregatedDatagram> {
        let now = Instant::now();
        let elapsed_ns = now.duration_since(self.last_tick).as_nanos() as u64;
        self.last_tick = now;
        self.pacer.advance(elapsed_ns);
        self.pacer.drain_tick()
    }

    /// Advance the token bucket by a fixed number of nanoseconds and drain
    /// eligible frames.
    ///
    /// Deterministic alternative to [`tick`](Connection::tick) for unit tests
    /// and network simulations that control time externally.
    pub fn tick_ns(&mut self, elapsed_ns: u64) -> Option<PacerAggregatedDatagram> {
        self.pacer.advance(elapsed_ns);
        self.pacer.drain_tick()
    }

    /// Total number of frames waiting across all channels.
    pub fn total_queued_frames(&self) -> usize {
        self.pacer.total_queued_frames()
    }

    /// Number of frames waiting on the given channel.
    pub fn queued_frames(&self, ch: ChannelId) -> usize {
        self.pacer.queued_frames(ch)
    }

    /// Bytes pending on the given channel.
    pub fn pending_bytes(&self, ch: ChannelId) -> usize {
        self.pacer.pending_bytes(ch)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacer::{ChannelId, DeliveryClass, PacerFrame, PRIORITY_ORDER, NUM_CHANNELS};

    const CH_CTRL: ChannelId = ChannelId(0);
    const CH_AUDIO: ChannelId = ChannelId(1);
    const CH_INPUT: ChannelId = ChannelId(3);
    const CH_XFER: ChannelId = ChannelId(7);

    fn frame(ch: ChannelId, size: usize) -> PacerFrame {
        PacerFrame::new(ch, vec![0u8; size])
    }

    /// Nanoseconds required to earn `bytes` tokens at `rate_bps`.
    fn ns_for_bytes(bytes: usize, rate_bps: f64) -> u64 {
        ((bytes as f64) * 8_000_000_000.0 / rate_bps).ceil() as u64
    }

    // ── Feature 9: three delivery classes on one five_tuple ──────────────

    // Canonical representatives for each delivery class.
    // Realtime       → audio    (channel 1)
    // ReliableUnordered → xfer (channel 7)
    // ReliableOrdered → input  (channel 3)

    #[test]
    fn realtime_frame_exits_through_connection() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80)); // realtime
        let dg = conn.tick_ns(1_000_000).expect("realtime frame must be sent");
        let ch = dg.frames[0].channel;
        assert_eq!(ch.delivery_class(), DeliveryClass::Realtime);
    }

    #[test]
    fn reliable_unordered_frame_exits_through_connection() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_XFER, 200)); // reliable-unordered
        let dg = conn.tick_ns(1_000_000).expect("reliable-unordered frame must be sent");
        let ch = dg.frames[0].channel;
        assert_eq!(ch.delivery_class(), DeliveryClass::ReliableUnordered);
    }

    #[test]
    fn reliable_ordered_frame_exits_through_connection() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_INPUT, 32)); // reliable-ordered
        let dg = conn.tick_ns(1_000_000).expect("reliable-ordered frame must be sent");
        let ch = dg.frames[0].channel;
        assert_eq!(ch.delivery_class(), DeliveryClass::ReliableOrdered);
    }

    #[test]
    fn all_three_delivery_classes_coalesce_into_one_datagram() {
        // All three delivery classes enqueued simultaneously; one tick must drain
        // them all into a single datagram — the single-five-tuple invariant.
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_INPUT, 32));  // reliable-ordered
        conn.enqueue(frame(CH_XFER, 100)); // reliable-unordered
        conn.enqueue(frame(CH_AUDIO, 80)); // realtime

        let dg = conn.tick_ns(1_000_000).expect("all three classes must be admitted");
        assert_eq!(dg.frames.len(), 3, "one datagram carries all three delivery classes");

        let classes: std::collections::HashSet<_> = dg.frames
            .iter()
            .map(|f| f.channel.delivery_class())
            .collect();
        assert!(classes.contains(&DeliveryClass::Realtime));
        assert!(classes.contains(&DeliveryClass::ReliableUnordered));
        assert!(classes.contains(&DeliveryClass::ReliableOrdered));
    }

    #[test]
    fn three_class_datagram_priority_order_preserved() {
        // When all three classes are in the same datagram, priority order
        // must still hold: reliable-ordered (ctrl ch0) > reliable-ordered (input ch3) >
        // realtime (audio ch1) > reliable-unordered (xfer ch7).
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_XFER, 20));  // reliable-unordered, lowest priority
        conn.enqueue(frame(CH_AUDIO, 20)); // realtime
        conn.enqueue(frame(CH_INPUT, 20)); // reliable-ordered, high priority

        let dg = conn.tick_ns(1_000_000).expect("all frames admitted");
        assert_eq!(dg.frames.len(), 3);
        // Priority order for these three channels: input(3) > audio(1) > xfer(7)
        assert_eq!(dg.frames[0].channel.0, 3, "input first");
        assert_eq!(dg.frames[1].channel.0, 1, "audio second");
        assert_eq!(dg.frames[2].channel.0, 7, "xfer third");
    }

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn new_has_no_queued_frames() {
        let conn = Connection::new(150_000.0);
        assert_eq!(conn.total_queued_frames(), 0);
    }

    #[test]
    fn rate_bps_reflects_initial_rate() {
        let conn = Connection::new(300_000.0);
        assert!((conn.rate_bps() - 300_000.0).abs() < 1.0);
    }

    // ── Token-bucket gating: no frames without tokens ─────────────────────

    #[test]
    fn zero_elapsed_admits_no_frames() {
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        assert!(conn.tick_ns(0).is_none(), "zero elapsed → zero tokens → no send");
        assert_eq!(conn.total_queued_frames(), 1, "frame must remain queued");
    }

    #[test]
    fn tick_ns_returns_none_when_queue_empty() {
        let mut conn = Connection::new(150_000.0);
        assert!(conn.tick_ns(1_000_000_000).is_none());
    }

    #[test]
    fn frame_admitted_after_sufficient_elapsed_time() {
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        let ns = ns_for_bytes(80, 150_000.0);
        assert!(conn.tick_ns(ns).is_some(), "frame admitted once tokens cover its cost");
    }

    // ── Pacing: rate-limited output ──────────────────────────────────────

    #[test]
    fn second_tick_without_tokens_admits_nothing() {
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_AUDIO, 80));

        // First tick: exactly enough tokens for one frame.
        let ns = ns_for_bytes(80, 150_000.0);
        let first = conn.tick_ns(ns).expect("first frame must be admitted");
        assert_eq!(first.frames.len(), 1);

        // Second tick with zero elapsed: no new tokens, second frame waits.
        assert!(conn.tick_ns(0).is_none(), "second frame must wait for tokens");
        assert_eq!(conn.total_queued_frames(), 1);
    }

    #[test]
    fn tokens_accumulate_across_ticks() {
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));

        // Two half-ticks, each granting half the required tokens.
        let half_ns = ns_for_bytes(40, 150_000.0);
        assert!(conn.tick_ns(half_ns).is_none(), "half tokens: frame still waiting");
        assert!(conn.tick_ns(half_ns).is_some(), "second half fills budget: frame admitted");
    }

    // ── Priority ordering through Connection ─────────────────────────────

    #[test]
    fn input_precedes_audio_in_drained_datagram() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_INPUT, 32));

        let dg = conn.tick_ns(1_000_000).unwrap();
        assert_eq!(dg.frames.len(), 2);
        assert_eq!(dg.frames[0].channel.0, 3, "input must lead (higher priority)");
        assert_eq!(dg.frames[1].channel.0, 1, "audio must follow");
    }

    #[test]
    fn ctrl_precedes_input_in_drained_datagram() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_INPUT, 32));
        conn.enqueue(frame(CH_CTRL, 20));

        let dg = conn.tick_ns(1_000_000).unwrap();
        assert_eq!(dg.frames[0].channel.0, 0, "ctrl/ACK must be highest priority");
    }

    #[test]
    fn all_channels_drained_in_priority_order() {
        let mut conn = Connection::new(10_000_000.0);
        for &ch in PRIORITY_ORDER.iter().rev() {
            conn.enqueue(frame(ChannelId(ch), 10));
        }

        let dg = conn.tick_ns(1_000_000_000).unwrap();
        assert_eq!(dg.frames.len(), NUM_CHANNELS);
        for (i, f) in dg.frames.iter().enumerate() {
            assert_eq!(
                f.channel.0, PRIORITY_ORDER[i],
                "frame {i} must come from channel {} (PRIORITY_ORDER[{i}])",
                PRIORITY_ORDER[i]
            );
        }
    }

    // ── Coalescing ────────────────────────────────────────────────────────

    #[test]
    fn multiple_frames_coalesced_per_tick() {
        let mut conn = Connection::new(10_000_000.0);
        for _ in 0..4 {
            conn.enqueue(frame(CH_AUDIO, 100));
        }
        let dg = conn.tick_ns(1_000_000).unwrap();
        assert_eq!(dg.frames.len(), 4, "all four frames must coalesce into one datagram");
        assert_eq!(conn.total_queued_frames(), 0);
    }

    #[test]
    fn fifo_preserved_within_channel_through_connection() {
        let mut conn = Connection::new(10_000_000.0);
        for i in 0u8..4 {
            conn.enqueue(PacerFrame::new(CH_AUDIO, vec![i; 10]));
        }
        let dg = conn.tick_ns(1_000_000).unwrap();
        for (i, f) in dg.frames.iter().enumerate() {
            assert_eq!(f.data[0], i as u8, "FIFO order must be preserved within a channel");
        }
    }

    // ── Rate control ──────────────────────────────────────────────────────

    #[test]
    fn rate_increase_allows_previously_blocked_frame() {
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));

        // 2 ms at 150 kbps earns ~37.5 bytes — not enough for an 80-byte frame.
        assert!(conn.tick_ns(2_000_000).is_none(), "frame must wait at 150 kbps");

        // Double the rate; 2 ms at 300 kbps earns 75 bytes — still not enough.
        // Use 10 Mbps so 2 ms earns 2 500 bytes, well above the 80-byte frame.
        conn.set_rate(10_000_000.0);
        assert!(conn.tick_ns(2_000_000).is_some(), "frame admitted after rate increase");
    }

    #[test]
    fn set_rate_reflected_immediately() {
        let mut conn = Connection::new(150_000.0);
        conn.set_rate(500_000.0);
        assert!((conn.rate_bps() - 500_000.0).abs() < 1.0);
    }

    // ── Burst cap: tokens do not accumulate beyond rate × 5 ms / 8 ───────

    #[test]
    fn burst_cap_limits_token_carryover_between_ticks() {
        // 150 kbps burst cap = 150_000 × 5 / 8_000 = 93.75 bytes.
        // After 1 second of idle time the pacer holds ≤ 93.75 bytes, not
        // 18 750 bytes.  One 80-byte frame is admitted; ≈ 13.75 bytes remain —
        // not enough for a second 80-byte frame on the next tick.
        let mut conn = Connection::new(150_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));

        // 1 second elapsed: tokens capped to 93.75 bytes; 80-byte frame admitted.
        let first = conn.tick_ns(1_000_000_000);
        assert!(first.is_some(), "first frame must be admitted");

        // Enqueue a second frame; remaining tokens (~13.75 B) < 80 B → no send.
        conn.enqueue(frame(CH_AUDIO, 80));
        assert!(
            conn.tick_ns(0).is_none(),
            "burst cap must prevent excess token carryover from a long idle period"
        );
    }

    // ── Diagnostics ───────────────────────────────────────────────────────

    #[test]
    fn queued_frames_decrements_after_successful_tick() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_INPUT, 32));
        assert_eq!(conn.total_queued_frames(), 2);
        conn.tick_ns(1_000_000);
        assert_eq!(conn.total_queued_frames(), 0);
    }

    #[test]
    fn queued_frames_per_channel_accurate() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_XFER, 200));
        assert_eq!(conn.queued_frames(CH_AUDIO), 2);
        assert_eq!(conn.queued_frames(CH_XFER), 1);
        assert_eq!(conn.queued_frames(CH_INPUT), 0);
    }

    #[test]
    fn pending_bytes_reflects_enqueued_data() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.enqueue(frame(CH_AUDIO, 60));
        assert_eq!(conn.pending_bytes(CH_AUDIO), 140);
    }

    #[test]
    fn pending_bytes_decrements_after_tick() {
        let mut conn = Connection::new(10_000_000.0);
        conn.enqueue(frame(CH_AUDIO, 80));
        conn.tick_ns(1_000_000);
        assert_eq!(conn.pending_bytes(CH_AUDIO), 0);
    }
}

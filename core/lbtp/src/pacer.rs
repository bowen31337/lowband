//! Channel-priority pacer — Feature 17.
//!
//! # The invariant
//!
//! Input frames (channel 3) must beat every media frame (channels 1, 4, 5) at
//! every dequeue decision.  The pacer enforces this by maintaining one
//! [`VecDeque`] per channel and iterating [`PRIORITY_ORDER`] on every
//! [`Pacer::dequeue`] call, returning the front frame from the highest-priority
//! non-empty queue that fits within the current token-bucket budget.
//!
//! # Priority order
//!
//! ```text
//! ctrl/ACK(0) > input(3) > cursor(2) > audio(1) >
//! screen-rt(4) > video-rt(5) > reliable(6) > xfer(7) > probes(8)
//! ```
//!
//! # Token-bucket pacing
//!
//! The congestion controller calls [`Pacer::set_rate`] to set the send rate.
//! The transport event loop calls [`Pacer::advance`] with the elapsed
//! nanoseconds before each dequeue loop.  Tokens accumulate up to a burst cap
//! of `rate × BURST_TOLERANCE_MS / 8000` bytes; unused tokens carry over
//! within a tick but are capped so the pacer never inherits encoder bursts.
//!
//! [`Pacer::dequeue`] returns `None` either when all queues are empty or when
//! the token balance is insufficient for the front frame of every non-empty
//! priority queue.
//!
//! # Per-tick frame coalescing (Feature 19)
//!
//! [`Pacer::drain_tick`] collects all frames that the token budget allows in a
//! single pacing tick — across every channel, in [`PRIORITY_ORDER`] — into one
//! [`PacerAggregatedDatagram`].  Packing N frames into one UDP datagram pays
//! the 19-byte LBTP envelope + AEAD overhead once instead of N times.

use std::collections::VecDeque;

/// Number of LBTP channels (0–8 inclusive).
pub const NUM_CHANNELS: usize = 9;

/// Maximum burst tolerance the pacer allows, in milliseconds.
///
/// Burst cap = `rate_bps × BURST_TOLERANCE_MS / 8_000`.  With a 150 kbps
/// send rate this is ≈ 94 bytes — roughly one audio frame — keeping
/// self-induced queuing well below 5 ms on any path.
const BURST_TOLERANCE_MS: f64 = 5.0;

/// Per-datagram overhead: 3-byte LBTP envelope (short form) + 16-byte AEAD tag.
const DATAGRAM_OVERHEAD: usize = 19;

/// Per-frame overhead within an aggregated datagram: 1-byte channel/type tag +
/// 1-byte varint length (sufficient for payloads ≤ 127 B; we budget 2 bytes
/// as a conservative approximation matching the architecture spec example).
const FRAME_HEADER_OVERHEAD: usize = 2;

/// Maximum bytes available for frame payloads (sum of per-frame headers +
/// their data) in a single LBTP datagram.
pub const MAX_DATAGRAM_PAYLOAD_BYTES: usize = 1200 - DATAGRAM_OVERHEAD;

/// Maximum `data` bytes a single [`PacerFrame`] may carry.
///
/// A frame whose `data` length exceeds this value would produce a datagram
/// larger than 1 200 bytes once the per-datagram overhead (19 B) and the
/// per-frame header (2 B) are added.  Such frames are rejected at
/// [`Pacer::enqueue`] time (Feature 7).
///
/// Derivation: `1 200 − DATAGRAM_OVERHEAD(19) − FRAME_HEADER_OVERHEAD(2) = 1 179`.
pub const MAX_FRAME_DATA_BYTES: usize = MAX_DATAGRAM_PAYLOAD_BYTES - FRAME_HEADER_OVERHEAD;

/// Canonical send-priority order for LBTP channels.
///
/// Interpretation: lower index → higher priority.  Iterate this array on every
/// dequeue; the first non-empty queue whose front frame fits within the token
/// budget wins.
///
/// Order derived from the architecture spec §6.3:
/// `ctrl/ACK(0) > input(3) > cursor(2) > audio(1) > screen-rt(4) >
///  video-rt(5) > reliable(6) > xfer(7) > probes(8)`
pub const PRIORITY_ORDER: [u8; NUM_CHANNELS] = [0, 3, 2, 1, 4, 5, 6, 7, 8];

/// Delivery semantics applied to frames on an LBTP channel.
///
/// All three classes are multiplexed on a single UDP five-tuple; the channel
/// ID in the LBTP frame header is the sole differentiator.  The transport
/// event loop emits frames from all classes through one [`Connection`] and
/// one UDP socket — no parallel paths, no additional ports.
///
/// [`Connection`]: crate::connection::Connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryClass {
    /// Fire-and-forget: frames are never retransmitted.
    ///
    /// Used by time-sensitive media (audio, screen-rt, video-rt, probes)
    /// where a stale retransmission would arrive too late to be useful.
    Realtime,
    /// Reliable delivery with no ordering guarantee between frames.
    ///
    /// Used by bulk transfers (screen lossless, video reference frames,
    /// file transfer) where each chunk is independently useful.
    ReliableUnordered,
    /// Reliable delivery in the order frames were sent.
    ///
    /// Used by control traffic (ctrl/ACK, cursor deltas, input events)
    /// where the receiver must process frames in sequence.
    ReliableOrdered,
}

/// Delivery class for each channel, indexed by [`ChannelId`]`.0`.
///
/// The mapping is fixed by the LBTP architecture spec §6.2 and must not
/// change without a protocol version bump.
pub const CHANNEL_DELIVERY_CLASS: [DeliveryClass; NUM_CHANNELS] = [
    DeliveryClass::ReliableOrdered,   // 0  ctrl / ACK
    DeliveryClass::Realtime,          // 1  audio
    DeliveryClass::ReliableOrdered,   // 2  cursor
    DeliveryClass::ReliableOrdered,   // 3  input events
    DeliveryClass::Realtime,          // 4  screen-rt
    DeliveryClass::Realtime,          // 5  video-rt
    DeliveryClass::ReliableUnordered, // 6  reliable bulk (screen lossless, video ref)
    DeliveryClass::ReliableUnordered, // 7  xfer / file transfer
    DeliveryClass::Realtime,          // 8  probes (padding, first-to-drop)
];

/// A validated LBTP channel identifier (0–8 inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelId(pub u8);

impl ChannelId {
    /// Create a `ChannelId`, panicking in debug builds if out of range.
    pub fn new(ch: u8) -> Self {
        debug_assert!((ch as usize) < NUM_CHANNELS, "channel {ch} out of range 0–8");
        Self(ch)
    }

    /// Delivery class assigned to this channel by the LBTP spec.
    pub fn delivery_class(self) -> DeliveryClass {
        CHANNEL_DELIVERY_CLASS[self.0 as usize]
    }
}

/// A datagram-ready payload queued in the pacer.
///
/// Frames must not exceed the LBTP MTU (1 200 bytes); the transport pipeline
/// is responsible for ensuring this before enqueue.
#[derive(Debug, Clone)]
pub struct PacerFrame {
    pub channel: ChannelId,
    /// Serialised, encrypted datagram payload — ready for the UDP socket.
    pub data: Vec<u8>,
}

impl PacerFrame {
    pub fn new(channel: ChannelId, data: Vec<u8>) -> Self {
        debug_assert!(
            data.len() <= MAX_FRAME_DATA_BYTES,
            "PacerFrame on channel {} exceeds per-frame data limit: {} > {} bytes",
            channel.0,
            data.len(),
            MAX_FRAME_DATA_BYTES,
        );
        Self { channel, data }
    }
}

/// All frames dequeued from the pacer in a single pacing tick, coalesced into
/// one logical LBTP datagram to amortise the 19-byte envelope + AEAD overhead.
///
/// The lbtp framer packs every frame's raw payload into one UDP datagram,
/// paying the overhead once rather than once per frame (LBTP Feature 19).
/// Frames are ordered by [`PRIORITY_ORDER`]; FIFO within each channel.
#[derive(Debug)]
pub struct PacerAggregatedDatagram {
    /// Frames in priority order; always non-empty.
    pub frames: Vec<PacerFrame>,
}

impl PacerAggregatedDatagram {
    /// Sum of raw payload bytes across all frames (excludes per-frame LBTP headers).
    pub fn data_bytes(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }
}

/// Channel-priority token-bucket pacer.
///
/// Maintains one FIFO queue per channel and releases frames to the transport
/// socket in [`PRIORITY_ORDER`], subject to the token-bucket rate limit.
///
/// ## Usage
///
/// ```rust
/// use lowband_lbtp::pacer::{ChannelId, Pacer, PacerFrame};
///
/// // 1 Mbps → burst cap = 1_000_000 * 5 / 8_000 = 625 bytes, enough for
/// // both frames (80 + 32 = 112 bytes) in one tick.
/// let mut pacer = Pacer::new(1_000_000.0);
///
/// // Enqueue an audio frame and an input frame simultaneously.
/// pacer.enqueue(PacerFrame::new(ChannelId::new(1), vec![0u8; 80]));  // audio
/// pacer.enqueue(PacerFrame::new(ChannelId::new(3), vec![0u8; 32]));  // input
///
/// // Grant tokens for ~5 ms of traffic (fills to burst cap: 625 bytes).
/// pacer.advance(5_000_000);
///
/// // Input dequeues first (priority 3 > 1).
/// let first = pacer.dequeue().unwrap();
/// assert_eq!(first.channel.0, 3);
///
/// let second = pacer.dequeue().unwrap();
/// assert_eq!(second.channel.0, 1);
/// ```
///
/// ## Thread safety
///
/// Not thread-safe.  The transport event loop is single-threaded; this type is
/// always called from that thread only.  Encode workers deliver frames through
/// lock-free SPSC rings that the event loop drains into the pacer at the start
/// of each tick.
#[derive(Debug)]
pub struct Pacer {
    queues: [VecDeque<PacerFrame>; NUM_CHANNELS],
    /// Token balance in bytes.
    token_bytes: f64,
    /// Controlled send rate in bits per second (set by congestion controller).
    rate_bps: f64,
    /// Token-bucket cap: `rate_bps × BURST_TOLERANCE_MS / 8_000` bytes.
    burst_cap_bytes: f64,
}

impl Pacer {
    /// Create a new pacer with the given initial send rate.
    pub fn new(initial_rate_bps: f64) -> Self {
        let burst_cap = burst_cap_for_rate(initial_rate_bps);
        Self {
            // SAFETY: VecDeque<PacerFrame> implements Default.
            queues: std::array::from_fn(|_| VecDeque::new()),
            token_bytes: 0.0,
            rate_bps: initial_rate_bps,
            burst_cap_bytes: burst_cap,
        }
    }

    /// Update the controlled send rate (called by the congestion controller).
    ///
    /// Clamps the current token balance to the new burst cap immediately so
    /// a rate reduction does not let a large pre-existing surplus drain at the
    /// old rate.
    pub fn set_rate(&mut self, rate_bps: f64) {
        self.rate_bps = rate_bps;
        self.burst_cap_bytes = burst_cap_for_rate(rate_bps);
        self.token_bytes = self.token_bytes.min(self.burst_cap_bytes);
    }

    /// Returns the current controlled send rate in bps.
    pub fn rate_bps(&self) -> f64 {
        self.rate_bps
    }

    /// Returns the current token balance in bytes.
    pub fn token_bytes(&self) -> f64 {
        self.token_bytes
    }

    /// Advance the token bucket by `elapsed_ns` nanoseconds.
    ///
    /// Called by the transport event loop before each dequeue loop.  Tokens
    /// accumulate at the controlled rate and are capped at the burst limit.
    pub fn advance(&mut self, elapsed_ns: u64) {
        let added = self.rate_bps * (elapsed_ns as f64) / 8_000_000_000.0;
        self.token_bytes = (self.token_bytes + added).min(self.burst_cap_bytes);
    }

    /// Enqueue a frame for paced transmission on the given channel.
    ///
    /// Returns `true` if the frame was accepted, or `false` if it was rejected
    /// because `frame.data.len() > MAX_FRAME_DATA_BYTES` — i.e. the assembled
    /// datagram (19-byte envelope + 2-byte frame header + data) would exceed
    /// the 1 200-byte ceiling (Feature 7).
    ///
    /// Accepted frames are sent in FIFO order within each channel.
    pub fn enqueue(&mut self, frame: PacerFrame) -> bool {
        if frame.data.len() > MAX_FRAME_DATA_BYTES {
            return false;
        }
        self.queues[frame.channel.0 as usize].push_back(frame);
        true
    }

    /// Dequeue the highest-priority frame that fits within the token budget.
    ///
    /// Iterates [`PRIORITY_ORDER`] and returns the front frame from the first
    /// non-empty channel whose byte count is covered by `token_bytes`.
    ///
    /// Returns `None` when:
    /// - all queues are empty, or
    /// - no non-empty queue's front frame fits in the remaining token balance.
    ///
    /// On success, the frame's byte count is deducted from `token_bytes`.
    pub fn dequeue(&mut self) -> Option<PacerFrame> {
        for &ch in &PRIORITY_ORDER {
            let queue = &mut self.queues[ch as usize];
            if let Some(frame) = queue.front() {
                let needed = frame.data.len() as f64;
                if self.token_bytes >= needed {
                    self.token_bytes -= needed;
                    return queue.pop_front();
                }
            }
        }
        None
    }

    /// Drain all frames eligible in the current pacing tick into one aggregated datagram.
    ///
    /// Scans channels repeatedly in [`PRIORITY_ORDER`], admitting the front frame
    /// of the highest-priority non-empty channel on each pass, subject to:
    ///
    /// - **Token budget**: each frame's byte count is deducted from `token_bytes`.
    ///   A frame is skipped (not the whole drain) if there are insufficient tokens.
    /// - **Datagram MTU**: frames are packed until the next would push the running
    ///   slot total (sum of `FRAME_HEADER_OVERHEAD + data.len()` per frame) past
    ///   [`MAX_DATAGRAM_PAYLOAD_BYTES`].  The first frame is always admitted
    ///   regardless of size (best-effort for near-MTU frames, matching the
    ///   xfer-scheduler convention).
    ///
    /// Returns `None` when all queues are empty or no frame fits in the token budget.
    ///
    /// ## Ordering guarantee
    ///
    /// Frames appear in the returned `Vec` in descending priority order; FIFO is
    /// preserved within each channel.  A lower-priority channel's frame may appear
    /// in the same datagram as a higher-priority one whenever the higher-priority
    /// channel's next frame would exceed the remaining datagram capacity.
    pub fn drain_tick(&mut self) -> Option<PacerAggregatedDatagram> {
        let mut frames: Vec<PacerFrame> = Vec::new();
        let mut datagram_used: usize = 0;

        // Each iteration of the outer loop either admits one frame or drops one
        // oversized frame; it breaks when a full priority scan finds nothing to do.
        'outer: loop {
            for &ch in &PRIORITY_ORDER {
                let queue = &mut self.queues[ch as usize];
                let Some(front) = queue.front() else { continue };

                let token_needed = front.data.len() as f64;
                if self.token_bytes < token_needed {
                    continue; // insufficient tokens; try lower-priority channel
                }

                let slot = FRAME_HEADER_OVERHEAD + front.data.len();

                // Frame can never fit in a 1 200-byte datagram — drop it so it
                // does not block lower-priority channels indefinitely (Feature 7).
                if slot > MAX_DATAGRAM_PAYLOAD_BYTES {
                    queue.pop_front();
                    continue 'outer; // restart from highest priority
                }

                // Adding this frame would overflow the current datagram — try
                // lower-priority channels that might be smaller.
                if !frames.is_empty() && datagram_used + slot > MAX_DATAGRAM_PAYLOAD_BYTES {
                    continue;
                }

                self.token_bytes -= token_needed;
                datagram_used += slot;
                frames.push(queue.pop_front().unwrap());
                continue 'outer; // restart from highest priority
            }
            break; // full pass found nothing to admit or drop
        }

        if frames.is_empty() {
            None
        } else {
            Some(PacerAggregatedDatagram { frames })
        }
    }

    /// Number of frames currently queued on the given channel.
    pub fn queued_frames(&self, ch: ChannelId) -> usize {
        self.queues[ch.0 as usize].len()
    }

    /// Total frames waiting across all channels.
    pub fn total_queued_frames(&self) -> usize {
        self.queues.iter().map(|q| q.len()).sum()
    }

    /// Bytes pending on a channel — used by the xfer scheduler to build
    /// `PacerDemand` (voice_pending, input_pending).
    pub fn pending_bytes(&self, ch: ChannelId) -> usize {
        self.queues[ch.0 as usize].iter().map(|f| f.data.len()).sum()
    }
}

fn burst_cap_for_rate(rate_bps: f64) -> f64 {
    rate_bps * BURST_TOLERANCE_MS / 8_000.0
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;

    const CH_CTRL: ChannelId = ChannelId(0);
    const CH_AUDIO: ChannelId = ChannelId(1);
    const CH_CURSOR: ChannelId = ChannelId(2);
    const CH_INPUT: ChannelId = ChannelId(3);
    const CH_SCREEN_RT: ChannelId = ChannelId(4);
    const CH_VIDEO_RT: ChannelId = ChannelId(5);
    const CH_RELIABLE: ChannelId = ChannelId(6);
    const CH_XFER: ChannelId = ChannelId(7);
    const CH_PROBES: ChannelId = ChannelId(8);

    fn pacer_with_tokens(tokens: usize) -> Pacer {
        // Use a high rate so burst_cap >> tokens we manually set.
        let mut p = Pacer::new(10_000_000.0);
        // Overwrite token balance directly by advancing enough ns.
        // Advance enough to fill to cap then drain by dequeuing a dummy frame
        // — simpler: just use set_rate to drive a known state.
        // Actually, easiest is to just advance exactly the right amount.
        // rate=10Mbps → 10_000_000 / 8_000_000_000 bytes/ns = 0.00125 bytes/ns
        // For `tokens` bytes: elapsed_ns = tokens / 0.00125 = tokens * 800
        p.advance((tokens as u64) * 800);
        p
    }

    fn frame(ch: ChannelId, size: usize) -> PacerFrame {
        PacerFrame::new(ch, vec![0u8; size])
    }

    // ── Feature 9: DeliveryClass assignment ──────────────────────────────

    #[test]
    fn ctrl_channel_is_reliable_ordered() {
        assert_eq!(CH_CTRL.delivery_class(), DeliveryClass::ReliableOrdered);
    }

    #[test]
    fn audio_channel_is_realtime() {
        assert_eq!(CH_AUDIO.delivery_class(), DeliveryClass::Realtime);
    }

    #[test]
    fn cursor_channel_is_reliable_ordered() {
        assert_eq!(CH_CURSOR.delivery_class(), DeliveryClass::ReliableOrdered);
    }

    #[test]
    fn input_channel_is_reliable_ordered() {
        assert_eq!(CH_INPUT.delivery_class(), DeliveryClass::ReliableOrdered);
    }

    #[test]
    fn screen_rt_channel_is_realtime() {
        assert_eq!(CH_SCREEN_RT.delivery_class(), DeliveryClass::Realtime);
    }

    #[test]
    fn video_rt_channel_is_realtime() {
        assert_eq!(CH_VIDEO_RT.delivery_class(), DeliveryClass::Realtime);
    }

    #[test]
    fn reliable_bulk_channel_is_reliable_unordered() {
        assert_eq!(CH_RELIABLE.delivery_class(), DeliveryClass::ReliableUnordered);
    }

    #[test]
    fn xfer_channel_is_reliable_unordered() {
        assert_eq!(CH_XFER.delivery_class(), DeliveryClass::ReliableUnordered);
    }

    #[test]
    fn probes_channel_is_realtime() {
        assert_eq!(CH_PROBES.delivery_class(), DeliveryClass::Realtime);
    }

    #[test]
    fn all_three_delivery_classes_present_across_channels() {
        let classes: Vec<_> = (0..NUM_CHANNELS as u8)
            .map(|ch| ChannelId(ch).delivery_class())
            .collect();
        assert!(classes.contains(&DeliveryClass::Realtime));
        assert!(classes.contains(&DeliveryClass::ReliableUnordered));
        assert!(classes.contains(&DeliveryClass::ReliableOrdered));
    }

    #[test]
    fn channel_delivery_class_table_covers_all_channels() {
        assert_eq!(CHANNEL_DELIVERY_CLASS.len(), NUM_CHANNELS);
    }

    // ── PRIORITY_ORDER correctness ─────────────────────────────────────────

    #[test]
    fn priority_order_starts_with_ctrl() {
        assert_eq!(PRIORITY_ORDER[0], 0, "ctrl must be highest priority");
    }

    #[test]
    fn priority_order_input_before_audio() {
        let input_rank = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
        let audio_rank = PRIORITY_ORDER.iter().position(|&c| c == 1).unwrap();
        assert!(input_rank < audio_rank, "input(ch3) must beat audio(ch1)");
    }

    #[test]
    fn priority_order_input_before_screen_rt() {
        let input_rank = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
        let screen_rank = PRIORITY_ORDER.iter().position(|&c| c == 4).unwrap();
        assert!(input_rank < screen_rank, "input(ch3) must beat screen-rt(ch4)");
    }

    #[test]
    fn priority_order_input_before_video_rt() {
        let input_rank = PRIORITY_ORDER.iter().position(|&c| c == 3).unwrap();
        let video_rank = PRIORITY_ORDER.iter().position(|&c| c == 5).unwrap();
        assert!(input_rank < video_rank, "input(ch3) must beat video-rt(ch5)");
    }

    #[test]
    fn priority_order_xfer_before_probes() {
        let xfer_rank = PRIORITY_ORDER.iter().position(|&c| c == 7).unwrap();
        let probe_rank = PRIORITY_ORDER.iter().position(|&c| c == 8).unwrap();
        assert!(xfer_rank < probe_rank, "xfer(ch7) must beat probes(ch8)");
    }

    #[test]
    fn priority_order_covers_all_channels() {
        let mut seen = [false; NUM_CHANNELS];
        for &ch in &PRIORITY_ORDER {
            assert!(!seen[ch as usize], "channel {ch} appears more than once");
            seen[ch as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "not all channels appear in PRIORITY_ORDER");
    }

    // ── Feature 17: input beats media ─────────────────────────────────────

    #[test]
    fn input_beats_audio_when_both_enqueued() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_AUDIO, 80));
        p.enqueue(frame(CH_INPUT, 32));

        let first = p.dequeue().unwrap();
        assert_eq!(first.channel.0, 3, "input must dequeue before audio");

        let second = p.dequeue().unwrap();
        assert_eq!(second.channel.0, 1);
    }

    #[test]
    fn input_beats_screen_rt_when_both_enqueued() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_SCREEN_RT, 1000));
        p.enqueue(frame(CH_INPUT, 32));

        let first = p.dequeue().unwrap();
        assert_eq!(first.channel.0, 3, "input must dequeue before screen-rt");
    }

    #[test]
    fn input_beats_video_rt_when_both_enqueued() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_VIDEO_RT, 800));
        p.enqueue(frame(CH_INPUT, 32));

        let first = p.dequeue().unwrap();
        assert_eq!(first.channel.0, 3, "input must dequeue before video-rt");
    }

    #[test]
    fn input_beats_all_media_simultaneously() {
        let mut p = pacer_with_tokens(100_000);
        p.enqueue(frame(CH_AUDIO, 80));
        p.enqueue(frame(CH_SCREEN_RT, 400));
        p.enqueue(frame(CH_VIDEO_RT, 600));
        p.enqueue(frame(CH_XFER, 1000));
        p.enqueue(frame(CH_INPUT, 32));

        let first = p.dequeue().unwrap();
        assert_eq!(first.channel.0, 3, "input must lead even against all media channels");
    }

    #[test]
    fn ctrl_beats_input() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_INPUT, 32));
        p.enqueue(frame(CH_CTRL, 20));

        let first = p.dequeue().unwrap();
        assert_eq!(first.channel.0, 0, "ctrl/ACK must be highest priority");
    }

    // ── Complete priority ordering ─────────────────────────────────────────

    #[test]
    fn dequeue_order_matches_priority_order_exactly() {
        let mut p = pacer_with_tokens(100_000);
        // Enqueue one frame per channel in reverse-priority order so arrival
        // order cannot explain the dequeue order.
        for &ch in PRIORITY_ORDER.iter().rev() {
            p.enqueue(frame(ChannelId(ch), 10));
        }

        for &expected_ch in &PRIORITY_ORDER {
            let f = p.dequeue().expect("should have a frame");
            assert_eq!(
                f.channel.0, expected_ch,
                "expected channel {expected_ch}, got {}",
                f.channel.0
            );
        }

        assert!(p.dequeue().is_none(), "all queues should be empty");
    }

    // ── FIFO ordering within a channel ────────────────────────────────────

    #[test]
    fn fifo_within_channel() {
        let mut p = pacer_with_tokens(100_000);
        for i in 0u8..4 {
            p.enqueue(PacerFrame::new(CH_INPUT, vec![i; 10]));
        }

        for expected in 0u8..4 {
            let f = p.dequeue().unwrap();
            assert_eq!(f.data[0], expected, "frames must dequeue in FIFO order");
        }
    }

    // ── Token-bucket gating ───────────────────────────────────────────────

    #[test]
    fn dequeue_returns_none_with_no_tokens() {
        let mut p = Pacer::new(150_000.0);
        // No advance — token balance is 0.
        p.enqueue(frame(CH_INPUT, 32));

        assert!(p.dequeue().is_none(), "must not send without tokens");
        assert_eq!(p.queued_frames(CH_INPUT), 1, "frame must remain in queue");
    }

    #[test]
    fn advance_grants_enough_tokens_for_one_frame() {
        let mut p = Pacer::new(150_000.0);
        p.enqueue(frame(CH_INPUT, 32));

        // 150 kbps → 150_000 / 8_000_000_000 bytes/ns = 1.875e-5 bytes/ns
        // For 32 bytes: need 32 / 1.875e-5 ≈ 1_706_667 ns
        p.advance(2_000_000); // 2 ms at 150 kbps → ≈37.5 bytes

        assert!(p.dequeue().is_some(), "should send after sufficient advance");
    }

    #[test]
    fn token_balance_decrements_by_frame_size() {
        let mut p = pacer_with_tokens(10_000);
        let initial_tokens = p.token_bytes();

        p.enqueue(frame(CH_INPUT, 100));
        p.dequeue().unwrap();

        let after = p.token_bytes();
        assert!(
            (initial_tokens - after - 100.0).abs() < 0.01,
            "tokens must decrease by frame size"
        );
    }

    #[test]
    fn burst_cap_prevents_token_accumulation_above_limit() {
        let rate = 150_000.0_f64;
        let expected_cap = rate * BURST_TOLERANCE_MS / 8_000.0;
        let mut p = Pacer::new(rate);

        // Advance a very long time — tokens must not exceed the burst cap.
        p.advance(1_000_000_000_000); // 1000 seconds

        assert!(
            p.token_bytes() <= expected_cap + 0.01,
            "tokens {} exceed burst cap {}",
            p.token_bytes(),
            expected_cap
        );
    }

    #[test]
    fn set_rate_clamps_existing_tokens_to_new_burst_cap() {
        let mut p = Pacer::new(10_000_000.0); // 10 Mbps → large burst cap
        p.advance(1_000_000_000); // fill to cap

        let new_rate = 64_000.0_f64; // 64 kbps → tiny burst cap
        let new_cap = new_rate * BURST_TOLERANCE_MS / 8_000.0;

        p.set_rate(new_rate);

        assert!(
            p.token_bytes() <= new_cap + 0.01,
            "tokens must be clamped to new burst cap after set_rate"
        );
    }

    #[test]
    fn dequeue_skips_channel_when_insufficient_tokens() {
        let mut p = Pacer::new(150_000.0);
        // Grant tokens for exactly 20 bytes.
        // 20 bytes at 150 kbps: 20 / (150_000/8_000_000_000) = 1_066_667 ns
        p.advance(1_100_000);

        // Input frame requires 100 bytes (more than available tokens).
        // Audio frame requires 10 bytes (fits).
        p.enqueue(frame(CH_INPUT, 100));
        p.enqueue(frame(CH_AUDIO, 10));

        // Even though input has higher priority, it can't fit — audio wins.
        let f = p.dequeue().unwrap();
        assert_eq!(
            f.channel.0, 1,
            "audio (10 B) should dequeue when input (100 B) cannot fit in token budget"
        );
    }

    // ── Pending bytes helper ───────────────────────────────────────────────

    #[test]
    fn pending_bytes_counts_all_enqueued_data() {
        let mut p = Pacer::new(1_000_000.0);
        p.enqueue(frame(CH_AUDIO, 80));
        p.enqueue(frame(CH_AUDIO, 60));

        assert_eq!(p.pending_bytes(CH_AUDIO), 140);
    }

    #[test]
    fn pending_bytes_decrements_after_dequeue() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_INPUT, 32));
        assert_eq!(p.pending_bytes(CH_INPUT), 32);

        p.dequeue().unwrap();
        assert_eq!(p.pending_bytes(CH_INPUT), 0);
    }

    // ── total_queued_frames ───────────────────────────────────────────────

    #[test]
    fn total_queued_frames_sums_all_channels() {
        let mut p = Pacer::new(1_000_000.0);
        p.enqueue(frame(CH_AUDIO, 10));
        p.enqueue(frame(CH_INPUT, 10));
        p.enqueue(frame(CH_XFER, 10));
        assert_eq!(p.total_queued_frames(), 3);
    }

    // ── Feature 19: drain_tick coalescing ────────────────────────────────

    #[test]
    fn drain_tick_returns_none_when_empty() {
        let mut p = pacer_with_tokens(10_000);
        assert!(p.drain_tick().is_none(), "empty pacer must return None");
    }

    #[test]
    fn drain_tick_returns_none_with_no_tokens() {
        let mut p = Pacer::new(150_000.0); // tokens start at 0
        p.enqueue(frame(CH_AUDIO, 80));
        assert!(p.drain_tick().is_none(), "no tokens → no frames admitted");
        assert_eq!(p.total_queued_frames(), 1, "frame must remain enqueued");
    }

    #[test]
    fn drain_tick_single_frame() {
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_INPUT, 32));

        let agg = p.drain_tick().expect("should return aggregated datagram");
        assert_eq!(agg.frames.len(), 1);
        assert_eq!(agg.frames[0].channel.0, 3);
        assert_eq!(agg.data_bytes(), 32);
        assert_eq!(p.total_queued_frames(), 0);
    }

    #[test]
    fn drain_tick_coalesces_same_channel_multiple_frames() {
        // Three 100-B frames from audio: 3 × (2 + 100) = 306 B ≤ MAX_DATAGRAM_PAYLOAD_BYTES.
        // All should be coalesced into one datagram.
        let mut p = pacer_with_tokens(10_000);
        for _ in 0u8..3 {
            p.enqueue(frame(CH_AUDIO, 100));
        }

        let agg = p.drain_tick().expect("should aggregate");
        assert_eq!(agg.frames.len(), 3, "all three audio frames must coalesce");
        assert_eq!(agg.data_bytes(), 300);
        assert_eq!(p.total_queued_frames(), 0);
    }

    #[test]
    fn drain_tick_coalesces_across_channels_in_priority_order() {
        // ctrl (ch 0) + input (ch 3) + audio (ch 1) — all small, all fit.
        // Expected order in result: ctrl(0) → input(3) → audio(1).
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_AUDIO, 20));
        p.enqueue(frame(CH_INPUT, 20));
        p.enqueue(frame(CH_CTRL, 20));

        let agg = p.drain_tick().expect("should aggregate");
        assert_eq!(agg.frames.len(), 3);
        assert_eq!(agg.frames[0].channel.0, 0, "ctrl must be first");
        assert_eq!(agg.frames[1].channel.0, 3, "input must be second");
        assert_eq!(agg.frames[2].channel.0, 1, "audio must be third");
        assert_eq!(p.total_queued_frames(), 0);
    }

    #[test]
    fn drain_tick_stops_at_mtu() {
        // 590-B frames: slot = 592 B (≤ MAX_DATAGRAM_PAYLOAD_BYTES 1 181 → first frame fits).
        // Two slots = 1 184 B > 1 181 → second frame would overflow; it is deferred.
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_AUDIO, 590));
        p.enqueue(frame(CH_AUDIO, 590));

        let agg = p.drain_tick().expect("should aggregate");
        assert_eq!(agg.frames.len(), 1, "second frame must be deferred — datagram full");
        assert_eq!(p.total_queued_frames(), 1, "one frame left in queue");
    }

    #[test]
    fn drain_tick_stops_when_token_budget_exhausted() {
        // Grant exactly enough tokens for one 80-B frame; a second frame must not be sent.
        let mut p = Pacer::new(150_000.0);
        // 80 bytes at 150 kbps: 80 / (150_000 / 8_000_000_000) ns = 4_266_667 ns
        p.advance(4_300_000);
        p.enqueue(frame(CH_AUDIO, 80));
        p.enqueue(frame(CH_AUDIO, 80));

        let agg = p.drain_tick().expect("should send one frame");
        assert_eq!(agg.frames.len(), 1, "only one frame fits in token budget");
        assert_eq!(p.total_queued_frames(), 1, "second frame remains queued");
    }

    #[test]
    fn drain_tick_lower_priority_fills_remaining_datagram_space() {
        // ctrl frame = 590 B (slot 592); audio frame = 589 B (slot 591).
        // Total: 592 + 591 = 1 183 B > MAX_DATAGRAM_PAYLOAD_BYTES (1 181) — audio doesn't fit.
        // Substitute smaller audio: 500 B (slot 502). 592 + 502 = 1 094 ≤ 1 181 → both fit.
        let mut p = pacer_with_tokens(10_000);
        p.enqueue(frame(CH_CTRL, 590));
        p.enqueue(frame(CH_AUDIO, 500));

        let agg = p.drain_tick().expect("should aggregate");
        assert_eq!(agg.frames.len(), 2, "ctrl + audio must both coalesce");
        assert_eq!(agg.frames[0].channel.0, 0, "ctrl first");
        assert_eq!(agg.frames[1].channel.0, 1, "audio second");
        assert_eq!(p.total_queued_frames(), 0);
    }

    #[test]
    fn drain_tick_skips_channel_insufficient_tokens_admits_smaller() {
        // Tokens enough for only a 10-B frame.
        // Ch 3 (input) has an 80-B frame — too big.
        // Ch 1 (audio) has a 10-B frame — fits.
        // drain_tick must admit the audio frame even though input is higher priority.
        let mut p = Pacer::new(150_000.0);
        // 10 bytes: 10 / (150_000 / 8_000_000_000) = 533_333 ns
        p.advance(600_000);
        p.enqueue(frame(CH_INPUT, 80));
        p.enqueue(frame(CH_AUDIO, 10));

        let agg = p.drain_tick().expect("audio frame should be admitted");
        assert_eq!(agg.frames.len(), 1);
        assert_eq!(agg.frames[0].channel.0, 1, "audio admitted when input lacks tokens");
        assert_eq!(p.total_queued_frames(), 1, "input frame still queued");
    }

    #[test]
    fn drain_tick_deducts_tokens_for_each_admitted_frame() {
        let mut p = pacer_with_tokens(10_000);
        let before = p.token_bytes();
        p.enqueue(frame(CH_CTRL, 100));
        p.enqueue(frame(CH_AUDIO, 200));

        p.drain_tick().unwrap();

        let after = p.token_bytes();
        assert!(
            (before - after - 300.0).abs() < 0.01,
            "tokens must decrease by total payload bytes (100 + 200 = 300)"
        );
    }

    #[test]
    fn drain_tick_fifo_within_channel() {
        let mut p = pacer_with_tokens(10_000);
        for i in 0u8..4 {
            p.enqueue(PacerFrame::new(CH_AUDIO, vec![i; 10]));
        }

        let agg = p.drain_tick().expect("all frames fit");
        assert_eq!(agg.frames.len(), 4);
        for (i, f) in agg.frames.iter().enumerate() {
            assert_eq!(f.data[0], i as u8, "FIFO order must be preserved within a channel");
        }
    }

    #[test]
    fn drain_tick_all_channels_coalesced_in_priority_order() {
        // One frame per channel, all tiny, all fit in one datagram.
        // Result must follow PRIORITY_ORDER exactly.
        let mut p = pacer_with_tokens(100_000);
        for &ch in PRIORITY_ORDER.iter().rev() {
            p.enqueue(frame(ChannelId(ch), 10));
        }

        let agg = p.drain_tick().expect("all 9 channels fit");
        assert_eq!(agg.frames.len(), NUM_CHANNELS);
        for (i, f) in agg.frames.iter().enumerate() {
            assert_eq!(
                f.channel.0, PRIORITY_ORDER[i],
                "frame {} must come from channel {} (PRIORITY_ORDER)",
                i, PRIORITY_ORDER[i]
            );
        }
        assert_eq!(p.total_queued_frames(), 0);
    }

    // ── Feature 7: reject datagrams larger than 1 200 bytes ──────────────

    #[test]
    fn max_frame_data_bytes_constant_fills_exactly_1200_byte_datagram() {
        // DATAGRAM_OVERHEAD(19) + FRAME_HEADER_OVERHEAD(2) + MAX_FRAME_DATA_BYTES == 1 200.
        assert_eq!(
            DATAGRAM_OVERHEAD + FRAME_HEADER_OVERHEAD + MAX_FRAME_DATA_BYTES,
            1200,
            "a max-size frame must produce exactly a 1 200-byte datagram"
        );
    }

    #[test]
    fn enqueue_rejects_frame_exceeding_max_frame_data_bytes() {
        let mut p = Pacer::new(10_000_000.0);
        let oversized = PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES + 1] };
        assert!(!p.enqueue(oversized), "frame exceeding max must be rejected");
        assert_eq!(p.total_queued_frames(), 0, "queue must remain empty after rejection");
    }

    #[test]
    fn enqueue_accepts_frame_at_exact_max_frame_data_bytes() {
        let mut p = Pacer::new(10_000_000.0);
        let max_frame = PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES] };
        assert!(p.enqueue(max_frame), "frame at the exact limit must be accepted");
        assert_eq!(p.total_queued_frames(), 1);
    }

    #[test]
    fn enqueue_accepts_frame_one_byte_below_limit() {
        let mut p = Pacer::new(10_000_000.0);
        let near_max = PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES - 1] };
        assert!(p.enqueue(near_max), "frame one byte below limit must be accepted");
    }

    #[test]
    fn drain_tick_sends_max_frame_data_bytes_frame() {
        // A MAX_FRAME_DATA_BYTES frame must be emitted; total datagram = 1 200 bytes.
        let mut p = pacer_with_tokens(10_000);
        let max_frame = PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES] };
        assert!(p.enqueue(max_frame));
        let agg = p.drain_tick().expect("max-size frame must be emitted");
        assert_eq!(agg.frames.len(), 1);
        assert_eq!(agg.frames[0].data.len(), MAX_FRAME_DATA_BYTES);
        let total_datagram_bytes = DATAGRAM_OVERHEAD + FRAME_HEADER_OVERHEAD + agg.frames[0].data.len();
        assert_eq!(total_datagram_bytes, 1200, "total datagram must be exactly 1 200 bytes");
    }

    #[test]
    fn drain_tick_drops_oversized_frame_and_emits_valid_lower_priority() {
        // An oversized frame inserted directly (bypassing enqueue) must be dropped
        // by drain_tick; a valid lower-priority frame must still be emitted.
        let mut p = pacer_with_tokens(10_000);
        // Force an oversized frame onto the high-priority input channel.
        p.queues[CH_INPUT.0 as usize].push_back(
            PacerFrame { channel: CH_INPUT, data: vec![0u8; MAX_FRAME_DATA_BYTES + 1] },
        );
        // Valid audio frame on a lower-priority channel.
        assert!(p.enqueue(PacerFrame::new(CH_AUDIO, vec![0u8; 80])));

        let agg = p.drain_tick().expect("audio frame must be emitted");
        assert_eq!(agg.frames.len(), 1, "only the valid audio frame must be in the datagram");
        assert_eq!(agg.frames[0].channel.0, CH_AUDIO.0, "audio must be emitted");
        assert_eq!(p.total_queued_frames(), 0, "oversized input frame must be dropped");
    }

    #[test]
    fn drain_tick_drops_oversized_first_frame_returns_none_when_no_other_frames() {
        // If the only queued frame is oversized, drain_tick must drop it and return None.
        let mut p = pacer_with_tokens(10_000);
        p.queues[CH_AUDIO.0 as usize].push_back(
            PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES + 1] },
        );
        assert!(p.drain_tick().is_none(), "no valid frames remain after drop — must return None");
        assert_eq!(p.total_queued_frames(), 0, "oversized frame must have been dropped");
    }

    #[test]
    fn enqueue_rejected_frame_does_not_appear_in_drain_tick() {
        let mut p = pacer_with_tokens(10_000);
        // Rejection at enqueue means the frame must not appear later.
        let rejected = PacerFrame { channel: CH_AUDIO, data: vec![0u8; MAX_FRAME_DATA_BYTES + 1] };
        assert!(!p.enqueue(rejected));
        assert!(p.drain_tick().is_none(), "no frame must be sent after rejection");
    }
}

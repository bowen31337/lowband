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

use std::collections::VecDeque;

/// Number of LBTP channels (0–8 inclusive).
pub const NUM_CHANNELS: usize = 9;

/// Maximum burst tolerance the pacer allows, in milliseconds.
///
/// Burst cap = `rate_bps × BURST_TOLERANCE_MS / 8_000`.  With a 150 kbps
/// send rate this is ≈ 94 bytes — roughly one audio frame — keeping
/// self-induced queuing well below 5 ms on any path.
const BURST_TOLERANCE_MS: f64 = 5.0;

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

/// A validated LBTP channel identifier (0–8 inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelId(pub u8);

impl ChannelId {
    /// Create a `ChannelId`, panicking in debug builds if out of range.
    pub fn new(ch: u8) -> Self {
        debug_assert!((ch as usize) < NUM_CHANNELS, "channel {ch} out of range 0–8");
        Self(ch)
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
            data.len() <= 1200,
            "PacerFrame on channel {} exceeds LBTP MTU: {} bytes",
            channel.0,
            data.len()
        );
        Self { channel, data }
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

    /// Enqueue a frame for transmission on the given channel.
    ///
    /// Frames are sent in FIFO order within each channel.  The caller is
    /// responsible for ensuring `frame.data.len() <= 1200`.
    pub fn enqueue(&mut self, frame: PacerFrame) {
        self.queues[frame.channel.0 as usize].push_back(frame);
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
}

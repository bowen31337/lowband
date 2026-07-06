//! Bulk-transfer pacing scheduler — Features 110, 111, and frame coalescing (LBTP Feature 18).
//!
//! # The absolute rule
//!
//! Bulk transfer (LBTP channel 7) must never add a millisecond of queuing
//! delay ahead of voice (channel 1) or input (channel 3).  This module
//! enforces that guarantee unconditionally.
//!
//! # Mechanism
//!
//! The transport event loop calls [`BulkTransferScheduler::tick`] each time it
//! considers sending an xfer datagram.  Before releasing any bytes the
//! scheduler checks two gates in order:
//!
//! 1. **Priority gate** (Feature 111): if [`PacerDemand::blocks_bulk`] is
//!    true — voice or input bytes are waiting in the pacer — the scheduler
//!    returns [`TickResult::HeldForPriority`] and touches nothing.
//! 2. **Headroom gate** (Feature 110): if the governor has not granted any
//!    headroom for this control interval, returns
//!    [`TickResult::HeldForHeadroom`].
//!
//! Only when both gates are clear does the scheduler coalesce as many queued
//! [`XferFrame`]s as will fit in one LBTP datagram and returns them as
//! [`TickResult::SendAggregated`].
//!
//! # Frame coalescing (LBTP Feature 18)
//!
//! Each LBTP datagram pays a fixed overhead of ~19 bytes (3-byte envelope in
//! short form + 16-byte AEAD tag).  Sending N xfer frames as N separate
//! datagrams multiplies that tax by N.  Instead, `tick` packs every frame that
//! fits within [`MAX_DATAGRAM_XFER_BYTES`] into a single [`AggregatedDatagram`],
//! paying the overhead once.  Each frame within the datagram costs an
//! additional 2-byte LBTP frame header (1-byte channel/type + 1-byte varint
//! length for payloads ≤ 127 B, 2-byte for larger — we budget 2 bytes as a
//! conservative approximation that matches the architecture spec example).
//!
//! A single near-MTU frame that cannot share a datagram is still admitted
//! alone; coalescing is best-effort.
//!
//! # Headroom accounting
//!
//! The governor runs at 10 Hz.  Each interval it calls
//! [`BulkTransferScheduler::set_headroom`] with the byte budget for xfer.
//! Unused budget is **discarded** at the next `set_headroom` call — xfer
//! cannot accumulate credit across intervals (the governor is the sole source
//! of truth on what the link can carry).

use std::collections::VecDeque;

/// Per-datagram overhead: 3-byte LBTP envelope (short form) + 16-byte AEAD tag.
const DATAGRAM_OVERHEAD: usize = 19;
/// Per-frame overhead within a datagram: 1-byte channel/type + 1-byte varint length.
const FRAME_HEADER_OVERHEAD: usize = 2;
/// Maximum bytes available for xfer frame payloads in a single LBTP datagram.
pub const MAX_DATAGRAM_XFER_BYTES: usize = 1200 - DATAGRAM_OVERHEAD;

/// Per-channel pending byte counts as seen by the lbtp pacer.
///
/// The transport event loop samples these from the pacer's priority queues
/// before each call to [`BulkTransferScheduler::tick`].
#[derive(Debug, Default, Clone, Copy)]
pub struct PacerDemand {
    /// Bytes waiting in the voice queue (channel 1).
    pub voice_pending: usize,
    /// Bytes waiting in the input queue (channel 3).
    pub input_pending: usize,
}

impl PacerDemand {
    /// Returns `true` if any voice or input bytes are waiting in the pacer.
    ///
    /// When true, `bulk_transfer` must be held unconditionally — it may not
    /// add even one datagram to the send path while either queue is non-empty.
    #[inline]
    pub fn blocks_bulk(self) -> bool {
        self.voice_pending > 0 || self.input_pending > 0
    }
}

/// A datagram-sized payload ready to be sent on LBTP channel 7.
///
/// Frames are produced by the FEC encoder ([`crate::fec`]) and held here
/// until the scheduler permits transmission.  Maximum size matches the LBTP
/// MTU (1200 bytes); the xfer pipeline must not exceed this limit.
#[derive(Debug, Clone)]
pub struct XferFrame {
    /// Serialised RaptorQ encoding packet (source or repair symbol).
    pub data: Vec<u8>,
    /// Logical transfer object this frame belongs to (for ACK tracking).
    pub object_id: u64,
    /// Encoding symbol ID within the object's source block.
    pub esi: u32,
}

impl XferFrame {
    pub fn new(data: Vec<u8>, object_id: u64, esi: u32) -> Self {
        debug_assert!(
            data.len() <= 1200,
            "XferFrame exceeds LBTP MTU: {} bytes",
            data.len()
        );
        Self { data, object_id, esi }
    }
}

/// Multiple [`XferFrame`]s coalesced into a single LBTP datagram payload.
///
/// The lbtp framer packs every frame in this aggregate into one UDP datagram,
/// paying the 19-byte IP/UDP/AEAD-tag overhead once rather than once per frame
/// (LBTP Feature 18).
#[derive(Debug)]
pub struct AggregatedDatagram {
    /// Frames in FIFO transmission order; always non-empty.
    pub frames: Vec<XferFrame>,
}

impl AggregatedDatagram {
    /// Sum of payload bytes across all frames (excludes per-frame LBTP headers).
    pub fn data_bytes(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }
}

/// Outcome of a single scheduler tick.
#[derive(Debug)]
pub enum TickResult {
    /// One or more xfer frames coalesced into one datagram; hand to the lbtp framer on channel 7.
    SendAggregated(AggregatedDatagram),
    /// Held — voice or input bytes are pending in the pacer (Feature 111).
    HeldForPriority,
    /// Held — the governor headroom budget is exhausted (Feature 110).
    HeldForHeadroom,
    /// The send queue is empty.
    Idle,
}

/// Bulk-transfer send scheduler.
///
/// Maintains a FIFO queue of [`XferFrame`]s and releases them to the
/// transport layer only when both priority and headroom conditions are met.
///
/// ## Thread safety
///
/// Not thread-safe.  The transport event loop is single-threaded; this type
/// is called from that thread only.  Frames are submitted by encode workers
/// via a lock-free SPSC ring that is drained into [`BulkTransferScheduler`]
/// at the start of each tick (not modelled here — that ring lives in `lbtp`).
#[derive(Debug, Default)]
pub struct BulkTransferScheduler {
    queue: VecDeque<XferFrame>,
    /// Bytes the governor has authorised xfer to send in the current interval.
    headroom_remaining: usize,
}

impl BulkTransferScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by the governor (~10 Hz) to set the byte budget for this
    /// control interval.  Replaces any previously unspent headroom.
    pub fn set_headroom(&mut self, bytes: usize) {
        self.headroom_remaining = bytes;
    }

    /// Returns the headroom remaining in the current governor interval.
    pub fn headroom_remaining(&self) -> usize {
        self.headroom_remaining
    }

    /// Enqueue a frame for future transmission.
    ///
    /// Frames are sent in FIFO order.  The encode pipeline pushes frames here
    /// as RaptorQ symbols are produced.
    pub fn enqueue(&mut self, frame: XferFrame) {
        self.queue.push_back(frame);
    }

    /// Number of frames currently waiting in the send queue.
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }

    /// Scheduling tick — called by the transport event loop.
    ///
    /// Implements the two-gate pacing logic, then coalesces (LBTP Feature 18):
    ///
    /// 1. **Priority gate** (Feature 111): returns [`TickResult::HeldForPriority`]
    ///    while `demand.blocks_bulk()` is true.
    /// 2. **Headroom gate** (Feature 110): returns [`TickResult::HeldForHeadroom`]
    ///    when the budget is zero or the front frame exceeds remaining budget.
    /// 3. **Coalescing**: greedily packs queued frames into one
    ///    [`AggregatedDatagram`] until the next frame would overflow
    ///    [`MAX_DATAGRAM_XFER_BYTES`] or exhaust `headroom_remaining`.
    ///    A single oversized frame is always admitted alone.
    pub fn tick(&mut self, demand: PacerDemand) -> TickResult {
        // Gate 1 — Feature 111: absolute priority hold.
        if demand.blocks_bulk() {
            return TickResult::HeldForPriority;
        }

        // Gate 2 — Feature 110: governor headroom.
        if self.headroom_remaining == 0 {
            return TickResult::HeldForHeadroom;
        }

        // Short-circuit if the queue is empty or the front frame exceeds headroom.
        match self.queue.front() {
            None => return TickResult::Idle,
            Some(frame) if frame.data.len() > self.headroom_remaining => {
                return TickResult::HeldForHeadroom;
            }
            _ => {}
        }

        // Coalesce: pack as many frames as fit into one datagram.
        let mut frames = Vec::new();
        let mut datagram_used: usize = 0;
        let mut headroom_used: usize = 0;

        loop {
            let Some(next) = self.queue.front() else { break };
            let slot = FRAME_HEADER_OVERHEAD + next.data.len();

            // Always admit the first frame (best-effort for near-MTU frames);
            // for subsequent frames, stop if the datagram would overflow.
            if !frames.is_empty() && datagram_used + slot > MAX_DATAGRAM_XFER_BYTES {
                break;
            }
            // Stop if adding this frame would exceed the remaining headroom budget.
            if headroom_used + next.data.len() > self.headroom_remaining {
                break;
            }

            let frame = self.queue.pop_front().unwrap();
            datagram_used += slot;
            headroom_used += frame.data.len();
            frames.push(frame);
        }

        self.headroom_remaining -= headroom_used;
        TickResult::SendAggregated(AggregatedDatagram { frames })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(size: usize) -> XferFrame {
        XferFrame::new(vec![0u8; size], 1, 0)
    }

    fn no_demand() -> PacerDemand {
        PacerDemand { voice_pending: 0, input_pending: 0 }
    }

    fn voice_demand(bytes: usize) -> PacerDemand {
        PacerDemand { voice_pending: bytes, input_pending: 0 }
    }

    fn input_demand(bytes: usize) -> PacerDemand {
        PacerDemand { voice_pending: 0, input_pending: bytes }
    }

    fn both_demand(v: usize, i: usize) -> PacerDemand {
        PacerDemand { voice_pending: v, input_pending: i }
    }

    // ── PacerDemand::blocks_bulk ──────────────────────────────────────────

    #[test]
    fn blocks_bulk_false_when_no_demand() {
        assert!(!no_demand().blocks_bulk());
    }

    #[test]
    fn blocks_bulk_true_when_voice_pending() {
        assert!(voice_demand(1).blocks_bulk());
        assert!(voice_demand(1200).blocks_bulk());
    }

    #[test]
    fn blocks_bulk_true_when_input_pending() {
        assert!(input_demand(1).blocks_bulk());
        assert!(input_demand(500).blocks_bulk());
    }

    #[test]
    fn blocks_bulk_true_when_both_pending() {
        assert!(both_demand(100, 200).blocks_bulk());
    }

    // ── BulkTransferScheduler — idle / empty queue ────────────────────────

    #[test]
    fn idle_when_queue_empty_and_headroom_available() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        assert!(matches!(s.tick(no_demand()), TickResult::Idle));
    }

    // ── Feature 111: priority gate ────────────────────────────────────────

    #[test]
    fn held_for_priority_when_voice_pending() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(500));

        let result = s.tick(voice_demand(1200));
        assert!(
            matches!(result, TickResult::HeldForPriority),
            "expected HeldForPriority, got {:?}", result
        );
        assert_eq!(s.queued_frames(), 1, "frame must not be consumed while held");
        assert_eq!(s.headroom_remaining(), 10_000, "headroom must not change while held");
    }

    #[test]
    fn held_for_priority_when_input_pending() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(300));

        assert!(matches!(s.tick(input_demand(64)), TickResult::HeldForPriority));
        assert_eq!(s.queued_frames(), 1);
    }

    #[test]
    fn held_for_priority_when_both_voice_and_input_pending() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(1000));

        assert!(matches!(s.tick(both_demand(400, 200)), TickResult::HeldForPriority));
    }

    #[test]
    fn priority_gate_checked_before_headroom_gate() {
        // Even if headroom is 0, a non-empty voice queue must report
        // HeldForPriority (not HeldForHeadroom).  The priority invariant is
        // absolute — reporting the wrong reason would hide latency violations.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(0);
        s.enqueue(make_frame(500));

        assert!(matches!(s.tick(voice_demand(100)), TickResult::HeldForPriority));
    }

    // ── Feature 110: headroom gate ────────────────────────────────────────

    #[test]
    fn held_for_headroom_when_budget_zero() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(0);
        s.enqueue(make_frame(200));

        assert!(matches!(s.tick(no_demand()), TickResult::HeldForHeadroom));
        assert_eq!(s.queued_frames(), 1, "frame must remain in queue");
    }

    #[test]
    fn held_for_headroom_when_frame_exceeds_remaining_budget() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(400);
        s.enqueue(make_frame(401));

        assert!(matches!(s.tick(no_demand()), TickResult::HeldForHeadroom));
        assert_eq!(s.queued_frames(), 1);
        assert_eq!(s.headroom_remaining(), 400, "budget must not be charged");
    }

    // ── Successful send path ──────────────────────────────────────────────

    #[test]
    fn send_frame_when_no_priority_demand_and_headroom_available() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(500));

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 1);
                assert_eq!(agg.frames[0].data.len(), 500);
                assert_eq!(agg.frames[0].object_id, 1);
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert_eq!(s.queued_frames(), 0);
        assert_eq!(s.headroom_remaining(), 9_500);
    }

    #[test]
    fn headroom_decrements_correctly_across_multiple_sends() {
        // 900-B frames: slot = 902 B; two slots = 1 804 B > MAX_DATAGRAM_XFER_BYTES (1 181).
        // Each tick therefore emits exactly one frame in its own datagram.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(3_000);

        for i in 0..3 {
            s.enqueue(XferFrame::new(vec![i as u8; 900], 42, i as u32));
        }

        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
        assert_eq!(s.headroom_remaining(), 2_100);

        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
        assert_eq!(s.headroom_remaining(), 1_200);

        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
        assert_eq!(s.headroom_remaining(), 300);

        assert!(matches!(s.tick(no_demand()), TickResult::Idle));
    }

    #[test]
    fn set_headroom_replaces_previous_budget() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(5_000);
        assert_eq!(s.headroom_remaining(), 5_000);

        // Simulate governor ticking without xfer using any budget.
        s.set_headroom(1_000);
        assert_eq!(s.headroom_remaining(), 1_000, "unused budget must not carry over");
    }

    // ── Priority lifts while headroom is available ────────────────────────

    #[test]
    fn sends_when_voice_clears() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(600));

        assert!(matches!(s.tick(voice_demand(1200)), TickResult::HeldForPriority));
        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
    }

    #[test]
    fn sends_when_input_clears() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(300));

        assert!(matches!(s.tick(input_demand(32)), TickResult::HeldForPriority));
        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
    }

    // ── Invariant: bulk never consumes headroom while held ─────────────────

    #[test]
    fn headroom_unchanged_across_priority_holds() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(8_000);
        s.enqueue(make_frame(1_000));

        // Ten ticks with voice demand — headroom must stay at 8 000.
        for _ in 0..10 {
            s.tick(voice_demand(500));
        }
        assert_eq!(s.headroom_remaining(), 8_000);
    }

    // ── FIFO ordering ────────────────────────────────────────────────────

    #[test]
    fn frames_sent_in_fifo_order() {
        // Four 100-B frames: 4 × (2 + 100) = 408 B ≤ MAX_DATAGRAM_XFER_BYTES (1 181).
        // All coalesce into one datagram; FIFO order must be preserved within it.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);

        for esi in 0u32..4 {
            s.enqueue(XferFrame::new(vec![esi as u8; 100], 99, esi));
        }

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 4);
                for (i, f) in agg.frames.iter().enumerate() {
                    assert_eq!(f.esi, i as u32, "FIFO order within aggregated datagram");
                }
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert!(matches!(s.tick(no_demand()), TickResult::Idle));
    }

    // ── Frame coalescing (LBTP Feature 18) ───────────────────────────────

    #[test]
    fn coalesces_small_frames_into_one_datagram() {
        // Three 200-B frames: 3 × 202 = 606 B ≤ 1 181. All fit in one datagram.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        for esi in 0u32..3 {
            s.enqueue(XferFrame::new(vec![esi as u8; 200], 1, esi));
        }

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 3, "all three frames coalesced");
                assert_eq!(agg.data_bytes(), 600);
                for (i, f) in agg.frames.iter().enumerate() {
                    assert_eq!(f.esi, i as u32, "FIFO order preserved");
                }
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert_eq!(s.queued_frames(), 0);
    }

    #[test]
    fn coalescing_stops_at_datagram_capacity() {
        // 500-B frames: slot = 502 B. Two fit (1 004 B ≤ 1 181); three do not (1 506 B).
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        for esi in 0u32..3 {
            s.enqueue(XferFrame::new(vec![0u8; 500], 1, esi));
        }

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 2, "datagram capacity limits coalescing to 2 frames");
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert_eq!(s.queued_frames(), 1, "third frame deferred to next tick");
        assert_eq!(s.headroom_remaining(), 9_000);
    }

    #[test]
    fn coalescing_respects_headroom() {
        // Three 400-B frames; headroom = 900 B. First two (800 B total) fit; third (1 200 B) exceeds.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(900);
        for esi in 0u32..3 {
            s.enqueue(XferFrame::new(vec![0u8; 400], 1, esi));
        }

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 2, "headroom caps coalescing at 2 frames");
                assert_eq!(agg.data_bytes(), 800);
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert_eq!(s.headroom_remaining(), 100);
        assert_eq!(s.queued_frames(), 1, "third frame waits for next interval");
    }

    #[test]
    fn single_large_frame_sent_alone() {
        // A near-MTU frame (1 100 B): slot = 1 102 B > MAX_DATAGRAM_XFER_BYTES (1 181)?
        // Actually 1 102 ≤ 1 181 — fine. Use a frame that fills the datagram so nothing else fits.
        // 580-B frames: one slot = 582; two slots = 1 164 ≤ 1 181.
        // Use 590-B frames: one slot = 592; two slots = 1 184 > 1 181 — only one fits.
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(XferFrame::new(vec![0u8; 590], 1, 0));
        s.enqueue(XferFrame::new(vec![1u8; 590], 1, 1));

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.frames.len(), 1, "only one 590-B frame fits per datagram");
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
        assert_eq!(s.queued_frames(), 1);
        // Second frame goes in the next datagram.
        assert!(matches!(s.tick(no_demand()), TickResult::SendAggregated(_)));
        assert_eq!(s.queued_frames(), 0);
    }

    #[test]
    fn aggregated_datagram_data_bytes_sums_all_frames() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(XferFrame::new(vec![0u8; 100], 1, 0));
        s.enqueue(XferFrame::new(vec![0u8; 200], 1, 1));
        s.enqueue(XferFrame::new(vec![0u8; 50], 1, 2));

        match s.tick(no_demand()) {
            TickResult::SendAggregated(agg) => {
                assert_eq!(agg.data_bytes(), 350);
            }
            other => panic!("expected SendAggregated, got {:?}", other),
        }
    }
}

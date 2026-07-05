//! Bulk-transfer pacing scheduler — Features 110 & 111.
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
//! Only when both gates are clear does the scheduler dequeue the front
//! [`XferFrame`] and hand it to the framer.
//!
//! # Headroom accounting
//!
//! The governor runs at 10 Hz.  Each interval it calls
//! [`BulkTransferScheduler::set_headroom`] with the byte budget for xfer.
//! Unused budget is **discarded** at the next `set_headroom` call — xfer
//! cannot accumulate credit across intervals (the governor is the sole source
//! of truth on what the link can carry).

use std::collections::VecDeque;

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

/// Outcome of a single scheduler tick.
#[derive(Debug)]
pub enum TickResult {
    /// A frame is ready; hand its bytes to the lbtp framer on channel 7.
    Send(XferFrame),
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
    /// Implements the two-gate pacing logic:
    ///
    /// 1. **Priority gate** (Feature 111): returns [`TickResult::HeldForPriority`]
    ///    while `demand.blocks_bulk()` is true.
    /// 2. **Headroom gate** (Feature 110): returns [`TickResult::HeldForHeadroom`]
    ///    when the budget is zero or the front frame exceeds remaining budget.
    ///
    /// On success, pops the front frame, deducts its byte count from
    /// `headroom_remaining`, and returns [`TickResult::Send`].
    pub fn tick(&mut self, demand: PacerDemand) -> TickResult {
        // Gate 1 — Feature 111: absolute priority hold.
        if demand.blocks_bulk() {
            return TickResult::HeldForPriority;
        }

        // Gate 2 — Feature 110: governor headroom.
        if self.headroom_remaining == 0 {
            return TickResult::HeldForHeadroom;
        }

        match self.queue.front() {
            None => TickResult::Idle,
            Some(frame) if frame.data.len() > self.headroom_remaining => {
                // Front frame exceeds remaining budget; wait for next interval.
                TickResult::HeldForHeadroom
            }
            Some(_) => {
                let frame = self.queue.pop_front().unwrap();
                self.headroom_remaining -= frame.data.len();
                TickResult::Send(frame)
            }
        }
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
            TickResult::Send(f) => {
                assert_eq!(f.data.len(), 500);
                assert_eq!(f.object_id, 1);
            }
            other => panic!("expected Send, got {:?}", other),
        }
        assert_eq!(s.queued_frames(), 0);
        assert_eq!(s.headroom_remaining(), 9_500);
    }

    #[test]
    fn headroom_decrements_correctly_across_multiple_sends() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(3_000);

        for i in 0..3 {
            s.enqueue(XferFrame::new(vec![i as u8; 900], 42, i as u32));
        }

        // First two sends fit within 3 000 bytes.
        assert!(matches!(s.tick(no_demand()), TickResult::Send(_)));
        assert_eq!(s.headroom_remaining(), 2_100);

        assert!(matches!(s.tick(no_demand()), TickResult::Send(_)));
        assert_eq!(s.headroom_remaining(), 1_200);

        // Third send (900 B) fits — headroom 1 200.
        assert!(matches!(s.tick(no_demand()), TickResult::Send(_)));
        assert_eq!(s.headroom_remaining(), 300);

        // Queue is now empty.
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

        // Voice is pending — held.
        assert!(matches!(s.tick(voice_demand(1200)), TickResult::HeldForPriority));

        // Voice drains — now the frame can go.
        assert!(matches!(s.tick(no_demand()), TickResult::Send(_)));
    }

    #[test]
    fn sends_when_input_clears() {
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);
        s.enqueue(make_frame(300));

        assert!(matches!(s.tick(input_demand(32)), TickResult::HeldForPriority));
        assert!(matches!(s.tick(no_demand()), TickResult::Send(_)));
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
        let mut s = BulkTransferScheduler::new();
        s.set_headroom(10_000);

        for esi in 0u32..4 {
            s.enqueue(XferFrame::new(vec![esi as u8; 100], 99, esi));
        }

        for expected_esi in 0u32..4 {
            match s.tick(no_demand()) {
                TickResult::Send(f) => assert_eq!(f.esi, expected_esi),
                other => panic!("expected Send, got {:?}", other),
            }
        }
    }
}

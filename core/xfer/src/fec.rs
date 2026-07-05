//! RaptorQ fountain coding for channel 7 — Feature 109.
//!
//! Bulk transfer uses RaptorQ (RFC 6330) fountain coding rather than
//! selective-ARQ retransmission.  On high-RTT paths (3G, 200–400 ms RTT)
//! waiting a full round trip per lost packet is expensive; the fountain
//! approach lets the sender stream repair symbols continuously — the receiver
//! can reconstruct the object from any sufficient subset of received symbols,
//! regardless of which specific symbols were lost.
//!
//! # Protocol contract
//!
//! The sender encodes a chunk (compressed output of [`crate::compress`]) into
//! source + repair symbols, then streams them via [`crate::scheduler`] on
//! channel 7 until the receiver ACKs the complete object.  The receiver uses
//! [`FecDecoder`] to accumulate symbols and recover the original data.

/// Maximum symbol payload size (bytes).  Must fit inside one LBTP datagram
/// after framing overhead (11-byte public envelope + ~6-byte frame header).
pub const SYMBOL_SIZE: u16 = 1_180;

#[derive(Debug)]
pub enum FecError {
    ObjectTooLarge,
    InsufficientSymbols,
}

impl std::fmt::Display for FecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FecError::ObjectTooLarge => f.write_str("object too large for single source block"),
            FecError::InsufficientSymbols => {
                f.write_str("decoding failed: insufficient symbols received")
            }
        }
    }
}

impl std::error::Error for FecError {}

/// Encodes one chunk into a set of source symbols plus on-demand repair symbols.
///
/// Wraps a `raptorq::SourceBlockEncoder` (single-block) and exposes the
/// source-streaming + repair-on-request pattern used by the xfer send loop.
/// Chunks produced by FastCDC top out at 64 kB; at 1 180 bytes per symbol
/// that is ≈54 source symbols — well within a single RaptorQ source block.
pub struct FecEncoder {
    block: raptorq::SourceBlockEncoder,
    oti: raptorq::ObjectTransmissionInformation,
    object_id: u64,
}

impl FecEncoder {
    /// Create an encoder for `data` (one compressed chunk).
    ///
    /// `object_id` is echoed in every [`crate::scheduler::XferFrame`] so the
    /// receiver can route symbols to the correct decoder instance.
    pub fn new(data: &[u8], object_id: u64) -> Self {
        let encoder = raptorq::Encoder::with_defaults(data, SYMBOL_SIZE);
        let oti = encoder.get_config();
        let block = encoder
            .get_block_encoders()
            .first()
            .expect("RaptorQ produced no source blocks")
            .clone();
        Self { block, oti, object_id }
    }

    pub fn object_id(&self) -> u64 {
        self.object_id
    }

    /// Source symbols — sent first before any repair symbols are added.
    pub fn source_packets(&self) -> Vec<raptorq::EncodingPacket> {
        self.block.source_packets()
    }

    /// Generate `count` repair symbols.
    ///
    /// `start_repair_id` is the repair-relative index (0 = first repair
    /// symbol).  The sender calls this in a loop, incrementing
    /// `start_repair_id` by `count` each call, until the receiver ACKs.
    /// Absolute ESIs in the returned packets are K + start_repair_id … where
    /// K is the number of source symbols.
    pub fn repair_packets(
        &self,
        start_repair_id: u32,
        count: u32,
    ) -> Vec<raptorq::EncodingPacket> {
        self.block.repair_packets(start_repair_id, count)
    }

    /// Serialise a [`raptorq::EncodingPacket`] into an [`crate::scheduler::XferFrame`].
    pub fn packet_to_frame(
        &self,
        packet: raptorq::EncodingPacket,
    ) -> crate::scheduler::XferFrame {
        let esi = packet.payload_id().encoding_symbol_id();
        let data = packet.serialize();
        crate::scheduler::XferFrame::new(data, self.object_id, esi)
    }

    /// Transmission information needed by the remote [`FecDecoder`].
    pub fn transmission_info(&self) -> raptorq::ObjectTransmissionInformation {
        self.oti
    }
}

/// Accumulates RaptorQ symbols and reconstructs the original chunk data.
pub struct FecDecoder {
    inner: raptorq::Decoder,
    object_id: u64,
}

impl FecDecoder {
    /// Create a decoder for the object described by `oti`.
    ///
    /// `oti` is communicated out-of-band in the xfer metadata packet that
    /// precedes the first symbol.
    pub fn new(
        oti: raptorq::ObjectTransmissionInformation,
        object_id: u64,
    ) -> Self {
        let inner = raptorq::Decoder::new(oti);
        Self { inner, object_id }
    }

    pub fn object_id(&self) -> u64 {
        self.object_id
    }

    /// Feed a received symbol into the decoder.
    ///
    /// Returns `Some(data)` once enough symbols have been received to
    /// reconstruct the original object; returns `None` otherwise.
    pub fn decode(
        &mut self,
        packet: raptorq::EncodingPacket,
    ) -> Option<Vec<u8>> {
        self.inner.decode(packet)
    }
}

/// Drives the repair-symbol send loop for one transfer object (Feature 109).
///
/// After source symbols have been transmitted the transport event loop calls
/// [`RepairSender::feed`] every scheduler cycle.  Each call generates the next
/// batch of repair symbols and enqueues them into the
/// [`crate::scheduler::BulkTransferScheduler`].  Repair generation stops as
/// soon as [`RepairSender::ack`] is called, which the transport layer invokes
/// when a peer ACK for this `object_id` arrives on channel 7.
///
/// # ESI continuity
///
/// RaptorQ repair symbols are identified by their Encoding Symbol ID (ESI).
/// `RepairSender` increments `next_repair_esi` by the actual number of packets
/// returned by the encoder on each call, so consecutive calls produce a
/// gap-free ESI stream regardless of how many symbols the encoder returns per
/// call.
pub struct RepairSender {
    encoder: FecEncoder,
    next_repair_esi: u32,
    acked: bool,
}

impl RepairSender {
    pub fn new(encoder: FecEncoder) -> Self {
        Self { encoder, next_repair_esi: 0, acked: false }
    }

    pub fn object_id(&self) -> u64 {
        self.encoder.object_id()
    }

    /// Signal that the remote peer has successfully reconstructed this object.
    ///
    /// After this call [`feed`](Self::feed) is a no-op and
    /// [`is_done`](Self::is_done) returns `true`.
    pub fn ack(&mut self) {
        self.acked = true;
    }

    /// Returns `true` once [`ack`](Self::ack) has been called.
    pub fn is_done(&self) -> bool {
        self.acked
    }

    /// Generate the next `batch` repair symbols and push them into
    /// `scheduler`.
    ///
    /// Returns the number of symbols enqueued (0 if the object is already
    /// ACK'd).  The transport loop should call this until `is_done()` returns
    /// `true`.
    pub fn feed(
        &mut self,
        scheduler: &mut crate::scheduler::BulkTransferScheduler,
        batch: u32,
    ) -> u32 {
        if self.acked {
            return 0;
        }
        let packets = self.encoder.repair_packets(self.next_repair_esi, batch);
        let count = packets.len() as u32;
        for packet in packets {
            scheduler.enqueue(self.encoder.packet_to_frame(packet));
        }
        self.next_repair_esi += count;
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_decode(data: &[u8]) -> Vec<u8> {
        let encoder = FecEncoder::new(data, 42);

        let source = encoder.source_packets();
        let oti = encoder.transmission_info();

        let mut decoder = FecDecoder::new(oti, 42);
        let mut result = None;
        for packet in source {
            result = decoder.decode(packet);
            if result.is_some() {
                break;
            }
        }
        result.expect("decoding should succeed with all source symbols")
    }

    #[test]
    fn roundtrip_small_payload() {
        let data = b"small chunk of data for FEC test";
        let recovered = encode_decode(data);
        assert_eq!(&recovered[..data.len()], data);
    }

    #[test]
    fn roundtrip_larger_payload() {
        let data: Vec<u8> = (0u8..=255).cycle().take(16 * 1024).collect();
        let recovered = encode_decode(&data);
        assert_eq!(&recovered[..data.len()], &data[..]);
    }

    #[test]
    fn packet_to_frame_produces_valid_xfer_frame() {
        let data = b"frame conversion test payload";
        let encoder = FecEncoder::new(data, 99);
        let packets = encoder.source_packets();
        assert!(!packets.is_empty());

        let frame = encoder.packet_to_frame(packets.into_iter().next().unwrap());
        assert_eq!(frame.object_id, 99);
        assert!(frame.data.len() <= 1200, "frame exceeds LBTP MTU");
    }

    #[test]
    fn repair_symbols_enable_recovery_after_source_loss() {
        let data: Vec<u8> = (0u8..=255).cycle().take(4 * 1024).collect();
        let encoder = FecEncoder::new(&data, 7);
        let oti = encoder.transmission_info();

        // Drop all source symbols; use only repair symbols.
        let repair = encoder.repair_packets(0, 20);

        let mut decoder = FecDecoder::new(oti, 7);
        let mut result = None;
        for packet in repair {
            result = decoder.decode(packet);
            if result.is_some() {
                break;
            }
        }
        let recovered = result.expect("should recover from repair symbols alone");
        assert_eq!(&recovered[..data.len()], &data[..]);
    }

    // ── RepairSender ─────────────────────────────────────────────────────────

    fn make_repair_sender(object_id: u64) -> RepairSender {
        let data: Vec<u8> = (0u8..=255).cycle().take(4 * 1024).collect();
        RepairSender::new(FecEncoder::new(&data, object_id))
    }

    #[test]
    fn repair_sender_enqueues_symbols_into_scheduler() {
        let mut sender = make_repair_sender(1);
        let mut scheduler = crate::scheduler::BulkTransferScheduler::new();
        scheduler.set_headroom(usize::MAX);

        let count = sender.feed(&mut scheduler, 4);
        assert_eq!(count, 4);
        assert_eq!(scheduler.queued_frames(), 4);
        assert!(!sender.is_done());
    }

    #[test]
    fn repair_sender_stops_feeding_after_ack() {
        let mut sender = make_repair_sender(2);
        let mut scheduler = crate::scheduler::BulkTransferScheduler::new();
        scheduler.set_headroom(usize::MAX);

        sender.feed(&mut scheduler, 4);
        sender.ack();
        assert!(sender.is_done());

        // Subsequent feed must be a no-op.
        let count = sender.feed(&mut scheduler, 4);
        assert_eq!(count, 0);
        assert_eq!(scheduler.queued_frames(), 4, "no new frames after ACK");
    }

    #[test]
    fn repair_sender_esi_is_contiguous_across_feed_calls() {
        let mut sender = make_repair_sender(3);
        let mut scheduler = crate::scheduler::BulkTransferScheduler::new();
        scheduler.set_headroom(usize::MAX);

        sender.feed(&mut scheduler, 3);
        sender.feed(&mut scheduler, 3);

        // Repair ESIs start at K (number of source symbols) and must be
        // strictly contiguous across the two feed() calls (no gaps).
        let mut prev_esi: Option<u32> = None;
        let mut total = 0u32;
        loop {
            use crate::scheduler::TickResult;
            match scheduler.tick(crate::scheduler::PacerDemand::default()) {
                TickResult::Send(f) => {
                    assert_eq!(f.object_id, 3);
                    if let Some(p) = prev_esi {
                        assert_eq!(f.esi, p + 1, "ESI gap between feed() calls");
                    }
                    prev_esi = Some(f.esi);
                    total += 1;
                }
                TickResult::Idle => break,
                other => panic!("unexpected tick result: {:?}", other),
            }
        }
        assert_eq!(total, 6, "expected 6 repair symbols total");
    }

    #[test]
    fn repair_sender_object_id_matches_encoder() {
        let sender = make_repair_sender(42);
        assert_eq!(sender.object_id(), 42);
    }

    #[test]
    fn repair_sender_not_done_before_ack() {
        let sender = make_repair_sender(5);
        assert!(!sender.is_done());
    }
}

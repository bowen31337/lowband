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
/// Wraps a `raptorq::Encoder` and exposes the source-streaming + repair-on-
/// request pattern used by the xfer send loop.
pub struct FecEncoder {
    inner: raptorq::Encoder,
    object_id: u64,
}

impl FecEncoder {
    /// Create an encoder for `data` (one compressed chunk).
    ///
    /// `object_id` is echoed in every [`crate::scheduler::XferFrame`] so the
    /// receiver can route symbols to the correct decoder instance.
    pub fn new(data: &[u8], object_id: u64) -> Self {
        let inner = raptorq::Encoder::new(data, SYMBOL_SIZE);
        Self { inner, object_id }
    }

    pub fn object_id(&self) -> u64 {
        self.object_id
    }

    /// Source symbols — sent first before any repair symbols are added.
    pub fn source_packets(&self) -> Vec<raptorq::EncodingPacket> {
        self.inner.source_packets()
    }

    /// Generate `count` repair symbols starting at ESI `start_esi`.
    ///
    /// The sender calls this in a loop, incrementing `start_esi` by `count`
    /// each call, until the receiver ACKs.
    pub fn repair_packets(
        &self,
        start_esi: u32,
        count: u32,
    ) -> Vec<raptorq::EncodingPacket> {
        self.inner.repair_packets(start_esi, count)
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
        self.inner.get_config()
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
}

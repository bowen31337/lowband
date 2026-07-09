//! Gear A AV1 intra reference-frame sender and receiver — Feature 117.
//!
//! At Gear A session start the sender encodes **one AV1 intra keyframe per
//! talking head** and transmits it to the receiver.  The receiver stores the
//! decoded pixels as a [`ReferenceFrame`] inside [`SynthesisNetwork`]; all
//! subsequent Gear A frames are lightweight motion-latent delta packets
//! (Feature 119) that warp the stored reference rather than retransmitting
//! pixel data.
//!
//! Because this module manages the *lifecycle* of the reference frame (one per
//! head, re-sent on reset or appearance change) rather than wrapping a codec
//! library, the packet format is a thin framing layer:
//!
//! ```text
//! [TAG_REFERENCE_FRAME (1 B)] [width: u32 LE (4 B)] [height: u32 LE (4 B)]
//! [RGB-8 pixels (width × height × 3 B)]
//! ```
//!
//! In production the pixel block is replaced by a real SVT-AV1 intra bitstream
//! and dav1d decodes it at the receiver; the framing and lifecycle contract
//! remain identical.
//!
//! # Sender lifecycle
//!
//! ```
//! use lowband_platform::reference_frame_sender::{GearAReferenceEncoder, ReferenceCodecError};
//! use lowband_platform::keypoint_extractor::CameraFrame;
//!
//! let mut enc = GearAReferenceEncoder::new();
//! assert!(enc.needs_reference_frame(), "first frame: reference must be sent");
//!
//! let frame = CameraFrame { pixels: vec![128u8; 64 * 64 * 3], width: 64, height: 64 };
//! let packet = enc.encode_reference_frame(&frame).unwrap();
//! assert!(!enc.needs_reference_frame(), "after sending: reference already delivered");
//!
//! // Force a re-send on reconnect or appearance change.
//! enc.reset();
//! assert!(enc.needs_reference_frame());
//! ```
//!
//! # Receiver lifecycle
//!
//! ```
//! use lowband_platform::reference_frame_sender::{GearAReferenceDecoder, GearAReferenceEncoder};
//! use lowband_platform::synthesis_network::{SynthesisConfig, SynthesisNetwork};
//! use lowband_platform::keypoint_extractor::CameraFrame;
//!
//! let mut enc = GearAReferenceEncoder::new();
//! let frame = CameraFrame { pixels: vec![100u8; 64 * 64 * 3], width: 64, height: 64 };
//! let packet = enc.encode_reference_frame(&frame).unwrap();
//!
//! let ref_frame = GearAReferenceDecoder::decode(packet.bytes()).unwrap();
//! let mut net = SynthesisNetwork::new(SynthesisConfig::default());
//! net.load_reference_frame(ref_frame).unwrap();
//! assert!(net.has_reference_frame());
//! ```

use crate::keypoint_extractor::CameraFrame;
use crate::synthesis_network::ReferenceFrame;

// ── Packet tag ────────────────────────────────────────────────────────────────

/// First byte of a Gear A AV1 reference-frame packet.
pub const TAG_REFERENCE_FRAME: u8 = b'R'; // 0x52

// ── Header layout ─────────────────────────────────────────────────────────────

const HEADER_WIDTH_OFFSET: usize = 1;
const HEADER_HEIGHT_OFFSET: usize = 5;

/// Total header length: tag (1) + width u32 (4) + height u32 (4) = 9 bytes.
pub const HEADER_LEN: usize = 9;

// ── ReferenceCodecError ───────────────────────────────────────────────────────

/// Errors returned by [`GearAReferenceEncoder`] and [`GearAReferenceDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceCodecError {
    /// The camera frame pixel buffer does not match `width × height × 3`, or
    /// a dimension is zero.
    InvalidFrame,
    /// The packet is shorter than the 9-byte minimum header.
    PacketTooShort,
    /// The first byte is not [`TAG_REFERENCE_FRAME`].
    UnknownTag(u8),
    /// Declared width or height in the header is zero.
    ZeroDimension,
    /// Pixel payload length does not match `width × height × 3`.
    PixelLengthMismatch { expected: usize, actual: usize },
}

// ── ReferenceFramePacket ──────────────────────────────────────────────────────

/// Encoded Gear A AV1 intra reference-frame packet ready for wire transmission.
///
/// Created by [`GearAReferenceEncoder::encode_reference_frame`]; decoded by
/// [`GearAReferenceDecoder::decode`].
#[derive(Debug, Clone)]
pub struct ReferenceFramePacket {
    data: Vec<u8>,
}

impl ReferenceFramePacket {
    /// Raw packet bytes for wire transmission.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Total byte length (header + pixel payload).
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` when the packet is empty (never true for a valid packet).
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Width of the encoded frame, as declared in the header.
    pub fn width(&self) -> u32 {
        u32::from_le_bytes(
            self.data[HEADER_WIDTH_OFFSET..HEADER_WIDTH_OFFSET + 4]
                .try_into()
                .unwrap(),
        )
    }

    /// Height of the encoded frame, as declared in the header.
    pub fn height(&self) -> u32 {
        u32::from_le_bytes(
            self.data[HEADER_HEIGHT_OFFSET..HEADER_HEIGHT_OFFSET + 4]
                .try_into()
                .unwrap(),
        )
    }
}

// ── GearAReferenceEncoder ─────────────────────────────────────────────────────

/// Sender-side reference-frame lifecycle manager for Gear A (Feature 117).
///
/// Tracks whether a reference frame has been transmitted for the current
/// talking-head session.  Exactly one reference frame is sent at session start;
/// subsequent frames carry only motion latents (Feature 119).
///
/// Call [`reset`] to force a fresh reference — e.g. on reconnect, after a
/// Gear B detour triggered by the guardrail detector, or when a significant
/// appearance change is detected by the sender's vision pipeline.
///
/// [`reset`]: Self::reset
pub struct GearAReferenceEncoder {
    reference_sent: bool,
}

impl GearAReferenceEncoder {
    /// Create a new encoder.  A reference frame must be sent before motion
    /// latent streaming can begin ([`needs_reference_frame`] returns `true`).
    ///
    /// [`needs_reference_frame`]: Self::needs_reference_frame
    pub fn new() -> Self {
        Self { reference_sent: false }
    }

    /// `true` when no reference frame has been sent (or after [`reset`]).
    ///
    /// The caller must transmit a reference packet produced by
    /// [`encode_reference_frame`] before sending motion-latent packets.
    ///
    /// [`reset`]: Self::reset
    /// [`encode_reference_frame`]: Self::encode_reference_frame
    pub fn needs_reference_frame(&self) -> bool {
        !self.reference_sent
    }

    /// `true` after a reference frame has been successfully sent.
    pub fn reference_frame_sent(&self) -> bool {
        self.reference_sent
    }

    /// Encode `frame` as a Gear A AV1 intra reference-frame packet.
    ///
    /// On success, marks the reference as sent so [`needs_reference_frame`]
    /// returns `false` until [`reset`] is called.
    ///
    /// Returns [`ReferenceCodecError::InvalidFrame`] when the pixel buffer
    /// length does not match `width × height × 3`, or either dimension is zero.
    /// A failed encode does **not** change the sent state.
    ///
    /// [`needs_reference_frame`]: Self::needs_reference_frame
    /// [`reset`]: Self::reset
    pub fn encode_reference_frame(
        &mut self,
        frame: &CameraFrame,
    ) -> Result<ReferenceFramePacket, ReferenceCodecError> {
        if !frame.is_valid() {
            return Err(ReferenceCodecError::InvalidFrame);
        }

        let pixel_bytes = (frame.width * frame.height * 3) as usize;
        let mut data = Vec::with_capacity(HEADER_LEN + pixel_bytes);
        data.push(TAG_REFERENCE_FRAME);
        data.extend_from_slice(&frame.width.to_le_bytes());
        data.extend_from_slice(&frame.height.to_le_bytes());
        data.extend_from_slice(&frame.pixels);

        self.reference_sent = true;
        Ok(ReferenceFramePacket { data })
    }

    /// Force the next [`encode_reference_frame`] call to retransmit a reference
    /// frame.
    ///
    /// Call on reconnect, after a Gear B detour, or when an appearance change
    /// requires the receiver to update its stored reference.
    ///
    /// [`encode_reference_frame`]: Self::encode_reference_frame
    pub fn reset(&mut self) {
        self.reference_sent = false;
    }
}

impl Default for GearAReferenceEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ── GearAReferenceDecoder ─────────────────────────────────────────────────────

/// Receiver-side decoder for Gear A AV1 intra reference-frame packets
/// (Feature 117).
///
/// Decodes the packet produced by [`GearAReferenceEncoder::encode_reference_frame`]
/// into a [`ReferenceFrame`] suitable for
/// [`crate::synthesis_network::SynthesisNetwork::load_reference_frame`].
pub struct GearAReferenceDecoder;

impl GearAReferenceDecoder {
    /// Decode a reference-frame packet into a [`ReferenceFrame`].
    ///
    /// # Errors
    ///
    /// | Error                          | Cause                                          |
    /// |-------------------------------|------------------------------------------------|
    /// | [`PacketTooShort`]             | `data` shorter than 9-byte header              |
    /// | [`UnknownTag`]                 | First byte ≠ [`TAG_REFERENCE_FRAME`]           |
    /// | [`ZeroDimension`]              | Declared width or height is zero               |
    /// | [`PixelLengthMismatch`]        | Pixel payload ≠ `width × height × 3`           |
    ///
    /// [`PacketTooShort`]: ReferenceCodecError::PacketTooShort
    /// [`UnknownTag`]: ReferenceCodecError::UnknownTag
    /// [`ZeroDimension`]: ReferenceCodecError::ZeroDimension
    /// [`PixelLengthMismatch`]: ReferenceCodecError::PixelLengthMismatch
    pub fn decode(data: &[u8]) -> Result<ReferenceFrame, ReferenceCodecError> {
        if data.len() < HEADER_LEN {
            return Err(ReferenceCodecError::PacketTooShort);
        }

        if data[0] != TAG_REFERENCE_FRAME {
            return Err(ReferenceCodecError::UnknownTag(data[0]));
        }

        let width = u32::from_le_bytes(
            data[HEADER_WIDTH_OFFSET..HEADER_WIDTH_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let height = u32::from_le_bytes(
            data[HEADER_HEIGHT_OFFSET..HEADER_HEIGHT_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        if width == 0 || height == 0 {
            return Err(ReferenceCodecError::ZeroDimension);
        }

        let expected = (width * height * 3) as usize;
        let actual = data.len() - HEADER_LEN;
        if actual != expected {
            return Err(ReferenceCodecError::PixelLengthMismatch { expected, actual });
        }

        Ok(ReferenceFrame {
            pixels: data[HEADER_LEN..].to_vec(),
            width,
            height,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis_network::{SynthesisConfig, SynthesisNetwork};

    fn make_frame(width: u32, height: u32, fill: u8) -> CameraFrame {
        CameraFrame { pixels: vec![fill; (width * height * 3) as usize], width, height }
    }

    // ── Encoder lifecycle ─────────────────────────────────────────────────────

    #[test]
    fn new_encoder_needs_reference_frame() {
        let enc = GearAReferenceEncoder::new();
        assert!(enc.needs_reference_frame());
        assert!(!enc.reference_frame_sent());
    }

    #[test]
    fn after_encode_reference_not_needed() {
        let mut enc = GearAReferenceEncoder::new();
        enc.encode_reference_frame(&make_frame(64, 64, 128)).unwrap();
        assert!(!enc.needs_reference_frame());
        assert!(enc.reference_frame_sent());
    }

    #[test]
    fn reset_requires_new_reference_frame() {
        let mut enc = GearAReferenceEncoder::new();
        enc.encode_reference_frame(&make_frame(64, 64, 128)).unwrap();
        enc.reset();
        assert!(enc.needs_reference_frame());
        assert!(!enc.reference_frame_sent());
    }

    #[test]
    fn encode_invalid_frame_leaves_state_unchanged() {
        let mut enc = GearAReferenceEncoder::new();
        let bad = CameraFrame { pixels: vec![0u8; 5], width: 2, height: 2 };
        assert!(
            matches!(enc.encode_reference_frame(&bad), Err(ReferenceCodecError::InvalidFrame)),
            "mismatched pixel buffer must return InvalidFrame"
        );
        assert!(enc.needs_reference_frame(), "failed encode must not mark reference as sent");
    }

    #[test]
    fn encode_zero_width_frame_returns_error() {
        let mut enc = GearAReferenceEncoder::new();
        let bad = CameraFrame { pixels: vec![], width: 0, height: 64 };
        assert!(matches!(
            enc.encode_reference_frame(&bad),
            Err(ReferenceCodecError::InvalidFrame)
        ));
    }

    #[test]
    fn exactly_one_reference_needed_at_session_start() {
        let mut enc = GearAReferenceEncoder::new();
        assert!(enc.needs_reference_frame());
        enc.encode_reference_frame(&make_frame(64, 64, 0)).unwrap();
        assert!(!enc.needs_reference_frame());
    }

    // ── ReferenceFramePacket ──────────────────────────────────────────────────

    #[test]
    fn packet_first_byte_is_tag_reference_frame() {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(32, 32, 200)).unwrap();
        assert_eq!(pkt.bytes()[0], TAG_REFERENCE_FRAME);
        assert_eq!(pkt.bytes()[0], b'R');
    }

    #[test]
    fn packet_width_and_height_accessors_match_input() {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(320, 240, 50)).unwrap();
        assert_eq!(pkt.width(), 320);
        assert_eq!(pkt.height(), 240);
    }

    #[test]
    fn packet_length_is_header_plus_pixels() {
        let (w, h) = (128u32, 96u32);
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(w, h, 0)).unwrap();
        assert_eq!(pkt.len(), HEADER_LEN + (w * h * 3) as usize);
    }

    #[test]
    fn packet_is_not_empty() {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(16, 16, 10)).unwrap();
        assert!(!pkt.is_empty());
    }

    // ── Decoder error paths ───────────────────────────────────────────────────

    #[test]
    fn decode_empty_slice_returns_too_short() {
        assert!(matches!(
            GearAReferenceDecoder::decode(&[]),
            Err(ReferenceCodecError::PacketTooShort)
        ));
    }

    #[test]
    fn decode_short_header_returns_too_short() {
        let partial = [TAG_REFERENCE_FRAME, 0, 0, 0, 0, 0, 0, 0]; // 8 bytes < HEADER_LEN
        assert!(matches!(
            GearAReferenceDecoder::decode(&partial),
            Err(ReferenceCodecError::PacketTooShort)
        ));
    }

    #[test]
    fn decode_wrong_tag_returns_unknown_tag() {
        let mut data = vec![0u8; HEADER_LEN + 12];
        data[0] = 0xFF;
        assert!(matches!(
            GearAReferenceDecoder::decode(&data),
            Err(ReferenceCodecError::UnknownTag(0xFF))
        ));
    }

    #[test]
    fn decode_zero_width_returns_zero_dimension() {
        let mut data = vec![0u8; HEADER_LEN];
        data[0] = TAG_REFERENCE_FRAME;
        data[HEADER_WIDTH_OFFSET..HEADER_WIDTH_OFFSET + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        data[HEADER_HEIGHT_OFFSET..HEADER_HEIGHT_OFFSET + 4]
            .copy_from_slice(&10u32.to_le_bytes());
        assert!(matches!(
            GearAReferenceDecoder::decode(&data),
            Err(ReferenceCodecError::ZeroDimension)
        ));
    }

    #[test]
    fn decode_zero_height_returns_zero_dimension() {
        let mut data = vec![0u8; HEADER_LEN];
        data[0] = TAG_REFERENCE_FRAME;
        data[HEADER_WIDTH_OFFSET..HEADER_WIDTH_OFFSET + 4]
            .copy_from_slice(&10u32.to_le_bytes());
        data[HEADER_HEIGHT_OFFSET..HEADER_HEIGHT_OFFSET + 4]
            .copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            GearAReferenceDecoder::decode(&data),
            Err(ReferenceCodecError::ZeroDimension)
        ));
    }

    #[test]
    fn decode_pixel_length_mismatch_returns_error() {
        // Header claims 4×4 (48 pixels) but only 10 bytes follow.
        let mut data = vec![0u8; HEADER_LEN + 10];
        data[0] = TAG_REFERENCE_FRAME;
        data[HEADER_WIDTH_OFFSET..HEADER_WIDTH_OFFSET + 4]
            .copy_from_slice(&4u32.to_le_bytes());
        data[HEADER_HEIGHT_OFFSET..HEADER_HEIGHT_OFFSET + 4]
            .copy_from_slice(&4u32.to_le_bytes());
        assert!(matches!(
            GearAReferenceDecoder::decode(&data),
            Err(ReferenceCodecError::PixelLengthMismatch { expected: 48, actual: 10 })
        ));
    }

    // ── Round-trip fidelity ───────────────────────────────────────────────────

    #[test]
    fn round_trip_produces_identical_pixels() {
        let fill = 77u8;
        let (w, h) = (64u32, 64u32);
        let frame = make_frame(w, h, fill);

        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&frame).unwrap();
        let decoded = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);
        assert_eq!(decoded.pixels.len(), (w * h * 3) as usize);
        assert!(decoded.pixels.iter().all(|&b| b == fill));
    }

    #[test]
    fn decoded_frame_passes_is_valid() {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(64, 48, 100)).unwrap();
        let decoded = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();
        assert!(decoded.is_valid());
    }

    #[test]
    fn round_trip_at_384px_head_resolution() {
        let (w, h) = (384u32, 384u32);
        let pixels: Vec<u8> = (0..(w * h * 3) as usize).map(|i| (i % 251) as u8).collect();
        let frame = CameraFrame { pixels: pixels.clone(), width: w, height: h };

        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&frame).unwrap();
        let decoded = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

        assert_eq!(decoded.width, w);
        assert_eq!(decoded.height, h);
        assert_eq!(decoded.pixels, pixels);
    }

    // ── Integration with SynthesisNetwork ─────────────────────────────────────

    #[test]
    fn decoded_reference_loads_into_synthesis_network() {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(256, 256, 88)).unwrap();
        let ref_frame = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        assert!(net.load_reference_frame(ref_frame).is_ok());
        assert!(net.has_reference_frame());
    }

    #[test]
    fn reset_followed_by_new_reference_updates_synthesis_network() {
        let mut enc = GearAReferenceEncoder::new();

        let pkt1 = enc.encode_reference_frame(&make_frame(256, 256, 10)).unwrap();
        let ref1 = GearAReferenceDecoder::decode(pkt1.bytes()).unwrap();

        enc.reset();

        let pkt2 = enc.encode_reference_frame(&make_frame(256, 256, 200)).unwrap();
        let ref2 = GearAReferenceDecoder::decode(pkt2.bytes()).unwrap();

        let mut net = SynthesisNetwork::new(SynthesisConfig::default());
        net.load_reference_frame(ref1).unwrap();
        net.load_reference_frame(ref2).unwrap();
        assert!(net.has_reference_frame());
    }
}

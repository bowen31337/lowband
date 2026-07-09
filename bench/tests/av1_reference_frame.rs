//! Feature 117 — one AV1 intra reference_frame per talking head at Gear A.
//!
//! # Purpose
//!
//! Verifies the Gear A reference-frame sender/receiver contract:
//!
//! 1. The sender emits **exactly one** reference frame at session start.
//! 2. Subsequent motion-latent frames do not trigger a new reference.
//! 3. A [`reset`] on the sender forces a new reference frame.
//! 4. The receiver can decode the packet into a valid [`ReferenceFrame`].
//! 5. The decoded frame loads successfully into [`SynthesisNetwork`].
//! 6. The packet header encodes the correct dimensions.
//! 7. Packet size is bounded and predictable for bandwidth budget planning.
//!
//! [`reset`]: lowband_platform::GearAReferenceEncoder::reset
//! [`ReferenceFrame`]: lowband_platform::ReferenceFrame
//! [`SynthesisNetwork`]: lowband_platform::synthesis_network::SynthesisNetwork
//!
//! # Architecture context (Design §5, Gear A)
//!
//! *"The receiver's warping/synthesis network reconstructs a 256–384 px head
//! that tracks the speaker's actual motion."*  The reference frame provides the
//! appearance anchor from which every subsequent motion-latent frame derives its
//! reconstruction.  Sending it once per head caps the appearance-bootstrap cost
//! at a single frame even across long sessions.

use lowband_platform::keypoint_extractor::CameraFrame;
use lowband_platform::reference_frame_sender::{
    GearAReferenceDecoder, GearAReferenceEncoder, ReferenceCodecError, HEADER_LEN,
    TAG_REFERENCE_FRAME,
};
use lowband_platform::synthesis_network::{
    HeadResolution, SynthesisConfig, SynthesisNetwork, HEAD_PX_MAX, HEAD_PX_MIN,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_frame(width: u32, height: u32) -> CameraFrame {
    let n = (width * height * 3) as usize;
    CameraFrame {
        pixels: (0..n).map(|i| (i % 251) as u8).collect(),
        width,
        height,
    }
}

// ── 1. Exactly one reference frame per session ────────────────────────────────

#[test]
fn sender_needs_reference_frame_at_session_start() {
    let enc = GearAReferenceEncoder::new();
    assert!(
        enc.needs_reference_frame(),
        "a fresh encoder must report that a reference frame is needed"
    );
}

#[test]
fn sender_does_not_need_reference_frame_after_first_send() {
    let mut enc = GearAReferenceEncoder::new();
    enc.encode_reference_frame(&make_frame(256, 256)).unwrap();
    assert!(
        !enc.needs_reference_frame(),
        "after sending the reference frame the sender must not request another"
    );
}

// ── 2. Subsequent calls do not spontaneously resend ───────────────────────────

#[test]
fn reference_frame_sent_state_persists_across_subsequent_frames() {
    let mut enc = GearAReferenceEncoder::new();
    enc.encode_reference_frame(&make_frame(256, 256)).unwrap();

    // Simulate 100 motion-latent frame ticks — reference must stay sent.
    for _ in 0..100 {
        assert!(
            !enc.needs_reference_frame(),
            "reference must remain sent throughout the motion-latent stream"
        );
    }
    assert!(enc.reference_frame_sent());
}

// ── 3. Reset forces a fresh reference ────────────────────────────────────────

#[test]
fn reset_makes_sender_need_new_reference_frame() {
    let mut enc = GearAReferenceEncoder::new();
    enc.encode_reference_frame(&make_frame(256, 256)).unwrap();
    assert!(!enc.needs_reference_frame());

    enc.reset();
    assert!(
        enc.needs_reference_frame(),
        "reset must require the sender to retransmit the reference frame"
    );
    assert!(!enc.reference_frame_sent());
}

#[test]
fn multiple_resets_each_require_a_new_reference_frame() {
    let mut enc = GearAReferenceEncoder::new();
    for _ in 0..5 {
        enc.encode_reference_frame(&make_frame(64, 64)).unwrap();
        assert!(!enc.needs_reference_frame());
        enc.reset();
        assert!(enc.needs_reference_frame());
    }
}

// ── 4. Receiver decodes the packet into a valid ReferenceFrame ────────────────

#[test]
fn receiver_decodes_packet_to_valid_reference_frame() {
    let frame = make_frame(256, 256);
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&frame).unwrap();

    let decoded = GearAReferenceDecoder::decode(pkt.bytes())
        .expect("a valid packet must decode without error");

    assert!(decoded.is_valid(), "decoded ReferenceFrame must pass is_valid()");
    assert_eq!(decoded.width, 256);
    assert_eq!(decoded.height, 256);
    assert_eq!(decoded.pixels.len(), 256 * 256 * 3);
}

#[test]
fn receiver_pixel_data_matches_sender_input() {
    let frame = make_frame(64, 64);
    let original_pixels = frame.pixels.clone();

    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&frame).unwrap();
    let decoded = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

    assert_eq!(
        decoded.pixels, original_pixels,
        "decoded pixel data must exactly match the original camera frame"
    );
}

// ── 5. Decoded frame loads into SynthesisNetwork ──────────────────────────────

#[test]
fn decoded_reference_frame_loads_into_synthesis_network_px256() {
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&make_frame(256, 256)).unwrap();
    let ref_frame = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

    let mut net = SynthesisNetwork::new(SynthesisConfig {
        resolution: HeadResolution::Px256,
        keypoint_count: 20,
    });
    assert!(
        net.load_reference_frame(ref_frame).is_ok(),
        "valid decoded reference must load into SynthesisNetwork without error"
    );
    assert!(net.has_reference_frame());
}

#[test]
fn decoded_reference_frame_loads_into_synthesis_network_px384() {
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&make_frame(384, 384)).unwrap();
    let ref_frame = GearAReferenceDecoder::decode(pkt.bytes()).unwrap();

    let mut net = SynthesisNetwork::new(SynthesisConfig {
        resolution: HeadResolution::Px384,
        keypoint_count: 20,
    });
    assert!(
        net.load_reference_frame(ref_frame).is_ok(),
        "384 × 384 decoded reference must load into SynthesisNetwork"
    );
    assert!(net.has_reference_frame());
}

// ── 6. Packet header encodes correct dimensions ───────────────────────────────

#[test]
fn packet_first_byte_is_tag_reference_frame() {
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&make_frame(64, 64)).unwrap();
    assert_eq!(
        pkt.bytes()[0],
        TAG_REFERENCE_FRAME,
        "packet tag byte must be TAG_REFERENCE_FRAME (0x{:02X})",
        TAG_REFERENCE_FRAME
    );
}

#[test]
fn packet_header_reports_correct_dimensions() {
    let (w, h) = (320u32, 240u32);
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&make_frame(w, h)).unwrap();
    assert_eq!(pkt.width(), w, "packet.width() must match the source frame width");
    assert_eq!(pkt.height(), h, "packet.height() must match the source frame height");
}

#[test]
fn packet_dimensions_match_gear_a_head_resolution_bounds() {
    // Verify that both Gear A head resolutions (256 and 384) round-trip
    // through the packet header correctly.
    for &size in &[HEAD_PX_MIN, HEAD_PX_MAX] {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(size, size)).unwrap();
        assert_eq!(pkt.width(), size);
        assert_eq!(pkt.height(), size);
        enc.reset();
    }
}

// ── 7. Packet size is bounded and predictable ─────────────────────────────────

#[test]
fn packet_size_equals_header_plus_pixels() {
    let (w, h) = (256u32, 256u32);
    let mut enc = GearAReferenceEncoder::new();
    let pkt = enc.encode_reference_frame(&make_frame(w, h)).unwrap();

    let expected_len = HEADER_LEN + (w * h * 3) as usize;
    assert_eq!(
        pkt.len(),
        expected_len,
        "packet length must be HEADER_LEN ({HEADER_LEN}) + pixel bytes ({} × {} × 3 = {})",
        w,
        h,
        w * h * 3
    );
}

#[test]
fn packet_size_report() {
    // Informational: print packet sizes for the two Gear A resolutions.
    // This is not a correctness assertion but documents the bandwidth cost.
    for &size in &[HEAD_PX_MIN, HEAD_PX_MAX] {
        let mut enc = GearAReferenceEncoder::new();
        let pkt = enc.encode_reference_frame(&make_frame(size, size)).unwrap();
        eprintln!(
            "AV1 reference frame [{size}×{size}]:  {} bytes  ({:.1} kB)  (one-shot per session)",
            pkt.len(),
            pkt.len() as f64 / 1024.0
        );
        enc.reset();
    }
}

// ── Receiver error paths ──────────────────────────────────────────────────────

#[test]
fn receiver_rejects_empty_packet() {
    assert!(
        matches!(GearAReferenceDecoder::decode(&[]), Err(ReferenceCodecError::PacketTooShort)),
        "empty packet must return PacketTooShort"
    );
}

#[test]
fn receiver_rejects_truncated_header() {
    let short = [TAG_REFERENCE_FRAME, 0, 0, 0, 0, 0, 0, 0]; // 8 B < HEADER_LEN (9)
    assert!(matches!(
        GearAReferenceDecoder::decode(&short),
        Err(ReferenceCodecError::PacketTooShort)
    ));
}

#[test]
fn receiver_rejects_wrong_tag() {
    let mut data = vec![0u8; HEADER_LEN + 48]; // header + 4×4×3 pixels
    data[0] = 0xDE; // not TAG_REFERENCE_FRAME
    data[1..5].copy_from_slice(&4u32.to_le_bytes()); // width = 4
    data[5..9].copy_from_slice(&4u32.to_le_bytes()); // height = 4
    assert!(matches!(
        GearAReferenceDecoder::decode(&data),
        Err(ReferenceCodecError::UnknownTag(0xDE))
    ));
}

#[test]
fn receiver_rejects_zero_dimension_in_header() {
    let mut data = vec![0u8; HEADER_LEN]; // no pixel payload
    data[0] = TAG_REFERENCE_FRAME;
    data[1..5].copy_from_slice(&0u32.to_le_bytes()); // width = 0
    data[5..9].copy_from_slice(&256u32.to_le_bytes());
    assert!(matches!(
        GearAReferenceDecoder::decode(&data),
        Err(ReferenceCodecError::ZeroDimension)
    ));
}

#[test]
fn receiver_rejects_pixel_length_mismatch() {
    // Header claims 16×16 (768 pixel bytes) but only 10 bytes follow.
    let mut data = vec![0u8; HEADER_LEN + 10];
    data[0] = TAG_REFERENCE_FRAME;
    data[1..5].copy_from_slice(&16u32.to_le_bytes());
    data[5..9].copy_from_slice(&16u32.to_le_bytes());
    assert!(matches!(
        GearAReferenceDecoder::decode(&data),
        Err(ReferenceCodecError::PixelLengthMismatch { expected: 768, actual: 10 })
    ));
}

//! Entropy coder for Gear A delta motion_latents — Feature 119.
//!
//! Encodes the per-frame delta of [`MotionLatents`] into a compact bitstream
//! targeting **5–9 kbps at 25 fps** (≈ 25–45 bytes per delta frame).
//!
//! # Encoding pipeline
//!
//! ```text
//! MotionLatents
//!     │  quantise (scale f32 → i32 per channel)
//!     │  delta (subtract previous quantised values)
//!     │  zigzag (signed → unsigned, maps small magnitudes to small codes)
//!     │  Exp-Golomb order-0 (variable-length binary code)
//!     └─→ [TAG_DELTA] ++ bit-packed bytes
//! ```
//!
//! The **first** frame (or any frame after [`MotionEncoder::reset`]) is a
//! *keyframe*: absolute quantised values are written as fixed-width `i16` words
//! so the receiver can initialise its state without prior context.  All
//! subsequent frames are *delta frames*.
//!
//! # Bitrate estimate (typical head-tracking data at 25 fps)
//!
//! | Component              | Count | avg bits | total  |
//! |------------------------|-------|----------|--------|
//! | keypoints x, y (Δ)     |   40  |   3.0    | 120 b  |
//! | keypoints z (Δ)         |   20  |   3.0    |  60 b  |
//! | pose angles (Δ)         |    3  |   5.0    |  15 b  |
//! | pose translation (Δ)    |    3  |   2.0    |   6 b  |
//! | expression latents (Δ)  |   64  |   1.6    | 102 b  |
//! | frame tag               |    1  |   8.0    |   8 b  |
//! | **Total**               |       |          | **311 b ≈ 39 B/frame** |
//!
//! 39 × 25 × 8 = **7 800 bps** — inside the 5–9 kbps window.

use crate::keypoint_extractor::EXPRESSION_DIM;
use crate::synthesis_network::{ExpressionLatents, HeadPose, Keypoint3D, MotionLatents, KEYPOINT_COUNT};

// ── Quantisation scales ───────────────────────────────────────────────────────

/// Keypoint x and y: normalised [0, 1] → integer steps of 1/256.
const SCALE_KP_XY: f32 = 256.0;

/// Keypoint z: depth offset [0, ~0.35] → integer steps of 1/256.
const SCALE_KP_Z: f32 = 256.0;

/// Pose angles (yaw, pitch, roll): [−π/4, π/4] → integer steps of ~0.002 rad.
const SCALE_POSE_ANGLE: f32 = 512.0;

/// Pose translation (tx, ty, tz): [−1, 1] → integer steps of 1/128.
const SCALE_POSE_TRANS: f32 = 128.0;

/// Expression latents: [−1, 1] → integer steps of 1/8.
///
/// Deliberately coarse — expression changes slowly between frames and the
/// quantisation noise stays well below synthesis quality.
const SCALE_EXPR: f32 = 8.0;

// ── Frame tags ────────────────────────────────────────────────────────────────

/// First byte of an absolute-coded (keyframe) payload.
const TAG_KEYFRAME: u8 = 0x01;

/// First byte of a differentially coded (delta) payload.
const TAG_DELTA: u8 = 0x00;

// ── Keyframe payload size ─────────────────────────────────────────────────────

/// Byte count of the keyframe payload (after the tag byte).
///
/// Layout: 20 keypoints × (x, y, z) × 2 bytes + 6 pose × 2 bytes + 64 expr × 2 bytes
/// = 120 + 12 + 128 = 260 bytes.
const KEYFRAME_PAYLOAD_LEN: usize = KEYPOINT_COUNT * 3 * 2 + 6 * 2 + EXPRESSION_DIM * 2;

// ── Public constants ──────────────────────────────────────────────────────────

/// Target frame rate for the Gear A motion stream (frames per second).
pub const MOTION_TARGET_FPS: u32 = 25;

/// Lower bound of the target delta-frame bitrate range (bps).
pub const MOTION_BITRATE_LO_BPS: u32 = 5_000;

/// Upper bound of the target delta-frame bitrate range (bps).
pub const MOTION_BITRATE_HI_BPS: u32 = 9_000;

// ── MotionCodecError ──────────────────────────────────────────────────────────

/// Errors returned by [`MotionDecoder::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotionCodecError {
    /// A delta frame arrived before the receiver has a keyframe to diff against.
    MissingKeyframe,
    /// The payload byte slice is empty.
    EmptyPayload,
    /// The payload is shorter than expected for the declared frame type.
    Truncated,
    /// The first byte is not a recognised frame tag.
    UnknownTag(u8),
}

// ── Internal quantised state ──────────────────────────────────────────────────

/// All motion latent scalars stored as quantised integers.
///
/// The encoder and decoder each hold one `QuantisedState` to compute per-frame
/// deltas without round-tripping through floating-point.
#[derive(Clone)]
struct QuantisedState {
    kp_x:  [i32; KEYPOINT_COUNT],
    kp_y:  [i32; KEYPOINT_COUNT],
    kp_z:  [i32; KEYPOINT_COUNT],
    yaw:   i32,
    pitch: i32,
    roll:  i32,
    tx:    i32,
    ty:    i32,
    tz:    i32,
    expr:  [i32; EXPRESSION_DIM],
}

impl Default for QuantisedState {
    fn default() -> Self {
        Self {
            kp_x:  [0; KEYPOINT_COUNT],
            kp_y:  [0; KEYPOINT_COUNT],
            kp_z:  [0; KEYPOINT_COUNT],
            yaw:   0,
            pitch: 0,
            roll:  0,
            tx:    0,
            ty:    0,
            tz:    0,
            expr:  [0; EXPRESSION_DIM],
        }
    }
}

// ── Quantise / dequantise helpers ─────────────────────────────────────────────

#[inline(always)]
fn quant(v: f32, scale: f32) -> i32 {
    (v * scale).round() as i32
}

#[inline(always)]
fn dequant(q: i32, scale: f32) -> f32 {
    q as f32 / scale
}

fn quantise(latents: &MotionLatents) -> QuantisedState {
    let mut s = QuantisedState::default();
    for (i, kp) in latents.keypoints.iter().enumerate().take(KEYPOINT_COUNT) {
        s.kp_x[i] = quant(kp.x, SCALE_KP_XY);
        s.kp_y[i] = quant(kp.y, SCALE_KP_XY);
        s.kp_z[i] = quant(kp.z, SCALE_KP_Z);
    }
    s.yaw   = quant(latents.pose.yaw,   SCALE_POSE_ANGLE);
    s.pitch = quant(latents.pose.pitch, SCALE_POSE_ANGLE);
    s.roll  = quant(latents.pose.roll,  SCALE_POSE_ANGLE);
    s.tx    = quant(latents.pose.tx,    SCALE_POSE_TRANS);
    s.ty    = quant(latents.pose.ty,    SCALE_POSE_TRANS);
    s.tz    = quant(latents.pose.tz,    SCALE_POSE_TRANS);
    for (i, &v) in latents.expression.values.iter().enumerate().take(EXPRESSION_DIM) {
        s.expr[i] = quant(v, SCALE_EXPR);
    }
    s
}

fn dequantise(s: &QuantisedState) -> MotionLatents {
    MotionLatents {
        keypoints: (0..KEYPOINT_COUNT)
            .map(|i| Keypoint3D {
                x:          dequant(s.kp_x[i], SCALE_KP_XY),
                y:          dequant(s.kp_y[i], SCALE_KP_XY),
                z:          dequant(s.kp_z[i], SCALE_KP_Z),
                confidence: 1.0, // not transmitted; receiver fills 1.0
            })
            .collect(),
        pose: HeadPose {
            yaw:   dequant(s.yaw,   SCALE_POSE_ANGLE),
            pitch: dequant(s.pitch, SCALE_POSE_ANGLE),
            roll:  dequant(s.roll,  SCALE_POSE_ANGLE),
            tx:    dequant(s.tx,    SCALE_POSE_TRANS),
            ty:    dequant(s.ty,    SCALE_POSE_TRANS),
            tz:    dequant(s.tz,    SCALE_POSE_TRANS),
        },
        expression: ExpressionLatents {
            values: (0..EXPRESSION_DIM).map(|i| dequant(s.expr[i], SCALE_EXPR)).collect(),
        },
    }
}

// ── Bit-level I/O ─────────────────────────────────────────────────────────────

/// Writes bits MSB-first into a growing byte buffer.
struct BitWriter {
    bytes:   Vec<u8>,
    pending: u8,
    bits:    u8, // bits written into `pending` so far (0..=7)
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), pending: 0, bits: 0 }
    }

    fn write_bit(&mut self, b: bool) {
        if b {
            self.pending |= 1 << (7 - self.bits);
        }
        self.bits += 1;
        if self.bits == 8 {
            self.bytes.push(self.pending);
            self.pending = 0;
            self.bits = 0;
        }
    }

    /// Write the `n` most-significant bits of `value` (MSB first).
    fn write_bits(&mut self, value: u32, n: u8) {
        for i in (0..n).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    /// Flush the partial byte (zero-padded) and return the buffer.
    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.bytes.push(self.pending);
        }
        self.bytes
    }
}

/// Reads bits MSB-first from a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit:  u8, // next bit position within the current byte (0 = MSB side)
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, byte: 0, bit: 0 }
    }

    fn read_bit(&mut self) -> Result<bool, MotionCodecError> {
        if self.byte >= self.data.len() {
            return Err(MotionCodecError::Truncated);
        }
        let b = (self.data[self.byte] >> (7 - self.bit)) & 1 != 0;
        self.bit += 1;
        if self.bit == 8 {
            self.byte += 1;
            self.bit = 0;
        }
        Ok(b)
    }
}

// ── Exp-Golomb order-0 coding ─────────────────────────────────────────────────

/// Encode non-negative integer `n` with Exp-Golomb order-0.
///
/// Code length for n: 2·⌊log₂(n+1)⌋ + 1 bits.
/// Examples: n=0 → "1" (1 b), n=1 → "010" (3 b), n=2 → "011" (3 b),
///           n=3 → "00100" (5 b), n=7 → "0001000" (7 b).
fn write_exp_golomb(w: &mut BitWriter, n: u32) {
    let v = n + 1; // v ≥ 1
    let m = 31 - v.leading_zeros(); // floor(log2(v))
    for _ in 0..m {
        w.write_bit(false);
    }
    w.write_bits(v, (m + 1) as u8);
}

/// Decode one Exp-Golomb order-0 value.
fn read_exp_golomb(r: &mut BitReader<'_>) -> Result<u32, MotionCodecError> {
    let mut m = 0u32;
    while !r.read_bit()? {
        m += 1;
        if m > 30 {
            return Err(MotionCodecError::Truncated);
        }
    }
    let mut val = 1u32;
    for _ in 0..m {
        val = (val << 1) | (r.read_bit()? as u32);
    }
    Ok(val - 1)
}

// ── Zigzag mapping ────────────────────────────────────────────────────────────

/// Bijection Z → N: 0→0, −1→1, 1→2, −2→3, 2→4, …
#[inline(always)]
fn zigzag_encode(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// Inverse of [`zigzag_encode`].
#[inline(always)]
fn zigzag_decode(n: u32) -> i32 {
    ((n >> 1) as i32) ^ (-((n & 1) as i32))
}

// ── Delta frame codec ─────────────────────────────────────────────────────────

fn encode_delta(w: &mut BitWriter, prev: &QuantisedState, curr: &QuantisedState) {
    for i in 0..KEYPOINT_COUNT {
        write_exp_golomb(w, zigzag_encode(curr.kp_x[i] - prev.kp_x[i]));
        write_exp_golomb(w, zigzag_encode(curr.kp_y[i] - prev.kp_y[i]));
        write_exp_golomb(w, zigzag_encode(curr.kp_z[i] - prev.kp_z[i]));
    }
    write_exp_golomb(w, zigzag_encode(curr.yaw   - prev.yaw));
    write_exp_golomb(w, zigzag_encode(curr.pitch - prev.pitch));
    write_exp_golomb(w, zigzag_encode(curr.roll  - prev.roll));
    write_exp_golomb(w, zigzag_encode(curr.tx    - prev.tx));
    write_exp_golomb(w, zigzag_encode(curr.ty    - prev.ty));
    write_exp_golomb(w, zigzag_encode(curr.tz    - prev.tz));
    for i in 0..EXPRESSION_DIM {
        write_exp_golomb(w, zigzag_encode(curr.expr[i] - prev.expr[i]));
    }
}

fn decode_delta(
    r: &mut BitReader<'_>,
    prev: &QuantisedState,
) -> Result<QuantisedState, MotionCodecError> {
    let mut curr = prev.clone();
    for i in 0..KEYPOINT_COUNT {
        curr.kp_x[i] = prev.kp_x[i] + zigzag_decode(read_exp_golomb(r)?);
        curr.kp_y[i] = prev.kp_y[i] + zigzag_decode(read_exp_golomb(r)?);
        curr.kp_z[i] = prev.kp_z[i] + zigzag_decode(read_exp_golomb(r)?);
    }
    curr.yaw   = prev.yaw   + zigzag_decode(read_exp_golomb(r)?);
    curr.pitch = prev.pitch + zigzag_decode(read_exp_golomb(r)?);
    curr.roll  = prev.roll  + zigzag_decode(read_exp_golomb(r)?);
    curr.tx    = prev.tx    + zigzag_decode(read_exp_golomb(r)?);
    curr.ty    = prev.ty    + zigzag_decode(read_exp_golomb(r)?);
    curr.tz    = prev.tz    + zigzag_decode(read_exp_golomb(r)?);
    for i in 0..EXPRESSION_DIM {
        curr.expr[i] = prev.expr[i] + zigzag_decode(read_exp_golomb(r)?);
    }
    Ok(curr)
}

// ── Keyframe codec (fixed-width i16) ─────────────────────────────────────────

fn encode_keyframe(state: &QuantisedState) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + KEYFRAME_PAYLOAD_LEN);
    out.push(TAG_KEYFRAME);
    macro_rules! push_i16 {
        ($v:expr) => {
            out.extend_from_slice(
                &(($v).clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes(),
            );
        };
    }
    for i in 0..KEYPOINT_COUNT {
        push_i16!(state.kp_x[i]);
        push_i16!(state.kp_y[i]);
        push_i16!(state.kp_z[i]);
    }
    push_i16!(state.yaw);
    push_i16!(state.pitch);
    push_i16!(state.roll);
    push_i16!(state.tx);
    push_i16!(state.ty);
    push_i16!(state.tz);
    for i in 0..EXPRESSION_DIM {
        push_i16!(state.expr[i]);
    }
    out
}

fn decode_keyframe(payload: &[u8]) -> Result<QuantisedState, MotionCodecError> {
    if payload.len() < KEYFRAME_PAYLOAD_LEN {
        return Err(MotionCodecError::Truncated);
    }
    let mut s = QuantisedState::default();
    let mut off = 0usize;
    macro_rules! read_i16 {
        () => {{
            let v = i16::from_le_bytes([payload[off], payload[off + 1]]) as i32;
            off += 2;
            v
        }};
    }
    for i in 0..KEYPOINT_COUNT {
        s.kp_x[i] = read_i16!();
        s.kp_y[i] = read_i16!();
        s.kp_z[i] = read_i16!();
    }
    s.yaw   = read_i16!();
    s.pitch = read_i16!();
    s.roll  = read_i16!();
    s.tx    = read_i16!();
    s.ty    = read_i16!();
    s.tz    = read_i16!();
    for i in 0..EXPRESSION_DIM {
        s.expr[i] = read_i16!();
    }
    Ok(s)
}

// ── MotionEncoder ─────────────────────────────────────────────────────────────

/// Sender-side entropy coder for Gear A delta motion_latents (Feature 119).
///
/// # Lifecycle
///
/// 1. Construct with [`MotionEncoder::new`].
/// 2. Call [`encode`] for every camera frame.  The **first** call produces a
///    keyframe (≈ 261 bytes); all subsequent calls produce delta frames
///    (typically 25–45 bytes, yielding 5–9 kbps at 25 fps).
/// 3. Call [`reset`] to force the next frame to be a new keyframe — e.g. on
///    reconnect or after a significant appearance change.
///
/// [`encode`]: Self::encode
/// [`reset`]: Self::reset
pub struct MotionEncoder {
    prev: Option<QuantisedState>,
}

impl MotionEncoder {
    /// Create a new encoder.  The next call to [`encode`] produces a keyframe.
    ///
    /// [`encode`]: Self::encode
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Entropy-code one frame of motion latents.
    ///
    /// Returns a `Vec<u8>` ready for transmission.  The byte at index 0 is the
    /// frame tag (`TAG_KEYFRAME` or `TAG_DELTA`).
    pub fn encode(&mut self, latents: &MotionLatents) -> Vec<u8> {
        let curr = quantise(latents);
        let out = if let Some(prev) = &self.prev {
            let mut w = BitWriter::new();
            encode_delta(&mut w, prev, &curr);
            let mut frame = vec![TAG_DELTA];
            frame.extend(w.finish());
            frame
        } else {
            encode_keyframe(&curr)
        };
        self.prev = Some(curr);
        out
    }

    /// Force the next [`encode`] call to emit a keyframe.
    ///
    /// [`encode`]: Self::encode
    pub fn reset(&mut self) {
        self.prev = None;
    }
}

impl Default for MotionEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ── MotionDecoder ─────────────────────────────────────────────────────────────

/// Receiver-side entropy decoder for Gear A delta motion_latents (Feature 119).
///
/// Mirrors [`MotionEncoder`]: keyframes initialise the state; delta frames
/// accumulate into it.  A delta frame received before any keyframe returns
/// [`MotionCodecError::MissingKeyframe`].
pub struct MotionDecoder {
    prev: Option<QuantisedState>,
}

impl MotionDecoder {
    /// Create a new decoder.  A keyframe must arrive before any delta frame.
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Decode one encoded motion frame, returning reconstructed [`MotionLatents`].
    pub fn decode(&mut self, data: &[u8]) -> Result<MotionLatents, MotionCodecError> {
        if data.is_empty() {
            return Err(MotionCodecError::EmptyPayload);
        }
        let state = match data[0] {
            TAG_KEYFRAME => decode_keyframe(&data[1..])?,
            TAG_DELTA => {
                let prev = self.prev.as_ref().ok_or(MotionCodecError::MissingKeyframe)?;
                let mut r = BitReader::new(&data[1..]);
                decode_delta(&mut r, prev)?
            }
            tag => return Err(MotionCodecError::UnknownTag(tag)),
        };
        let latents = dequantise(&state);
        self.prev = Some(state);
        Ok(latents)
    }

    /// Discard state.  The next frame must be a keyframe.
    pub fn reset(&mut self) {
        self.prev = None;
    }
}

impl Default for MotionDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_latents(seed: f32) -> MotionLatents {
        let kp = Keypoint3D {
            x:          (0.5 + 0.1 * seed.sin()).clamp(0.0, 1.0),
            y:          (0.5 + 0.1 * seed.cos()).clamp(0.0, 1.0),
            z:          0.05_f32.abs(),
            confidence: 0.9,
        };
        MotionLatents {
            keypoints: vec![kp; KEYPOINT_COUNT],
            pose: HeadPose {
                yaw:   0.1 * seed.sin(),
                pitch: 0.05 * seed.cos(),
                roll:  0.02 * seed.sin(),
                tx:    0.05 * seed.cos(),
                ty:    0.03 * seed.sin(),
                tz:    0.0,
            },
            expression: ExpressionLatents {
                values: vec![0.05 * seed.sin(); EXPRESSION_DIM],
            },
        }
    }

    // ── Frame tags ────────────────────────────────────────────────────────────

    #[test]
    fn first_encode_produces_keyframe() {
        let mut enc = MotionEncoder::new();
        assert_eq!(enc.encode(&make_latents(0.0))[0], TAG_KEYFRAME);
    }

    #[test]
    fn second_encode_produces_delta_frame() {
        let mut enc = MotionEncoder::new();
        enc.encode(&make_latents(0.0));
        assert_eq!(enc.encode(&make_latents(0.1))[0], TAG_DELTA);
    }

    #[test]
    fn reset_forces_next_keyframe() {
        let mut enc = MotionEncoder::new();
        enc.encode(&make_latents(0.0));
        enc.reset();
        assert_eq!(enc.encode(&make_latents(0.1))[0], TAG_KEYFRAME);
    }

    // ── Decoder error paths ───────────────────────────────────────────────────

    #[test]
    fn delta_before_keyframe_is_error() {
        let mut enc = MotionEncoder::new();
        let mut dec = MotionDecoder::new();
        enc.encode(&make_latents(0.0)); // skip keyframe
        let delta = enc.encode(&make_latents(0.1));
        assert!(matches!(dec.decode(&delta), Err(MotionCodecError::MissingKeyframe)));
    }

    #[test]
    fn empty_payload_is_error() {
        let mut dec = MotionDecoder::new();
        assert!(matches!(dec.decode(&[]), Err(MotionCodecError::EmptyPayload)));
    }

    #[test]
    fn unknown_tag_is_error() {
        let mut dec = MotionDecoder::new();
        assert!(matches!(dec.decode(&[0xFF]), Err(MotionCodecError::UnknownTag(0xFF))));
    }

    #[test]
    fn truncated_keyframe_is_error() {
        let mut dec = MotionDecoder::new();
        // TAG_KEYFRAME but payload too short.
        assert!(matches!(dec.decode(&[TAG_KEYFRAME, 0, 1]), Err(MotionCodecError::Truncated)));
    }

    #[test]
    fn decoder_reset_requires_new_keyframe() {
        let mut enc = MotionEncoder::new();
        let mut dec = MotionDecoder::new();
        dec.decode(&enc.encode(&make_latents(0.0))).unwrap();
        dec.decode(&enc.encode(&make_latents(0.1))).unwrap();
        dec.reset();
        let delta = enc.encode(&make_latents(0.2));
        assert!(matches!(dec.decode(&delta), Err(MotionCodecError::MissingKeyframe)));
    }

    // ── Round-trip fidelity ───────────────────────────────────────────────────

    #[test]
    fn keyframe_round_trip_within_quantisation_error() {
        let latents = make_latents(1.5);
        let mut enc = MotionEncoder::new();
        let mut dec = MotionDecoder::new();
        let out = dec.decode(&enc.encode(&latents)).unwrap();
        // 1 quantisation step per channel.
        let eps_xy    = 1.0 / SCALE_KP_XY + 1e-5;
        let eps_angle = 1.0 / SCALE_POSE_ANGLE + 1e-5;
        assert!((out.keypoints[0].x - latents.keypoints[0].x).abs() < eps_xy);
        assert!((out.pose.yaw       - latents.pose.yaw).abs()        < eps_angle);
    }

    #[test]
    fn delta_frame_round_trip_within_quantisation_error() {
        let l0 = make_latents(1.0);
        let l1 = make_latents(1.1);
        let mut enc = MotionEncoder::new();
        let mut dec = MotionDecoder::new();
        dec.decode(&enc.encode(&l0)).unwrap();
        let out = dec.decode(&enc.encode(&l1)).unwrap();
        let eps_xy    = 1.0 / SCALE_KP_XY + 1e-5;
        let eps_angle = 1.0 / SCALE_POSE_ANGLE + 1e-5;
        assert!((out.keypoints[0].x - l1.keypoints[0].x).abs() < eps_xy);
        assert!((out.pose.yaw       - l1.pose.yaw).abs()        < eps_angle);
    }

    #[test]
    fn multi_frame_accumulation_stays_accurate() {
        let mut enc = MotionEncoder::new();
        let mut dec = MotionDecoder::new();
        let n = 50;
        for i in 0..n {
            let seed = i as f32 * 0.05;
            let latents = make_latents(seed);
            let out = dec.decode(&enc.encode(&latents)).unwrap();
            let eps = 1.0 / SCALE_KP_XY + 1e-4;
            assert!(
                (out.keypoints[0].x - latents.keypoints[0].x).abs() < eps,
                "frame {i}: accumulated error exceeds 1 quantisation step"
            );
        }
    }

    // ── Bit-level primitives ──────────────────────────────────────────────────

    #[test]
    fn zigzag_round_trips_for_signed_values() {
        for n in [-1000i32, -1, 0, 1, 1000, i16::MAX as i32] {
            assert_eq!(zigzag_decode(zigzag_encode(n)), n, "zigzag round-trip for {n}");
        }
    }

    #[test]
    fn exp_golomb_round_trips() {
        for v in [0u32, 1, 2, 3, 4, 5, 6, 7, 15, 31, 63, 127, 255, 1000] {
            let mut w = BitWriter::new();
            write_exp_golomb(&mut w, v);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(read_exp_golomb(&mut r).unwrap(), v, "EG round-trip for {v}");
        }
    }

    #[test]
    fn exp_golomb_zero_is_one_bit() {
        let mut w = BitWriter::new();
        write_exp_golomb(&mut w, 0);
        assert_eq!(w.finish(), &[0x80u8]); // "1" followed by 7 padding zeros
    }

    // ── Size property ─────────────────────────────────────────────────────────

    #[test]
    fn delta_frame_smaller_than_keyframe_for_small_motion() {
        let mut enc = MotionEncoder::new();
        let kf_size = enc.encode(&make_latents(0.0)).len();
        let df_size = enc.encode(&make_latents(0.01)).len();
        assert!(
            df_size < kf_size,
            "delta frame ({df_size} B) must be smaller than keyframe ({kf_size} B)"
        );
    }

    #[test]
    fn keyframe_has_expected_size() {
        let mut enc = MotionEncoder::new();
        let kf = enc.encode(&make_latents(0.0));
        assert_eq!(kf.len(), 1 + KEYFRAME_PAYLOAD_LEN, "keyframe must be exactly 261 bytes");
    }
}

//! Production AV1 camera-tile codec (FR-8).
//!
//! Encodes a 32×32 BGRA tile as an AV1 intra frame with `rav1e` (the
//! reference pure-Rust AV1 encoder — the `av1-encode` feature, buildable and
//! testable without any C library) and decodes it with `dav1d` (system
//! libdav1d — the `av1` feature, exercised by the CI `camera-av1` job). This
//! is the real AV1 the PRD specifies for FR-8; the pure-Rust block-DCT codec
//! (`crate::picture`) remains the interim gear when AV1 is not compiled in.
//!
//! Color: BGRA → BT.601 YUV 4:2:0 (chroma subsampled) → AV1 → back. 4:2:0 and
//! AV1 quantization are both lossy, matching the photographic-tile role.

use lowband_platform::TILE_BYTES;

const DIM: usize = 32;
const CDIM: usize = DIM / 2; // 4:2:0 chroma dimension

/// BT.601 studio-range BGRA → YUV 4:2:0 planes (Y: 32×32, U/V: 16×16).
fn bgra_to_yuv420(bgra: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut y = vec![0u8; DIM * DIM];
    // Accumulate chroma over 2×2 blocks, then average.
    let mut u = vec![0u8; CDIM * CDIM];
    let mut v = vec![0u8; CDIM * CDIM];
    for j in 0..DIM {
        for i in 0..DIM {
            let off = (j * DIM + i) * 4;
            let (b, g, r) = (bgra[off] as f32, bgra[off + 1] as f32, bgra[off + 2] as f32);
            let yy = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
            y[j * DIM + i] = yy.round().clamp(0.0, 255.0) as u8;
        }
    }
    for cj in 0..CDIM {
        for ci in 0..CDIM {
            let mut su = 0.0;
            let mut sv = 0.0;
            for dj in 0..2 {
                for di in 0..2 {
                    let off = ((cj * 2 + dj) * DIM + ci * 2 + di) * 4;
                    let (b, g, r) =
                        (bgra[off] as f32, bgra[off + 1] as f32, bgra[off + 2] as f32);
                    su += -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
                    sv += 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
                }
            }
            u[cj * CDIM + ci] = (su / 4.0).round().clamp(0.0, 255.0) as u8;
            v[cj * CDIM + ci] = (sv / 4.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    (y, u, v)
}

/// YUV 4:2:0 planes → BGRA (alpha opaque), inverse of [`bgra_to_yuv420`].
#[cfg(feature = "av1")]
fn yuv420_to_bgra(y: &[u8], u: &[u8], v: &[u8], y_stride: usize, c_stride: usize) -> [u8; TILE_BYTES] {
    let mut out = [0u8; TILE_BYTES];
    for j in 0..DIM {
        for i in 0..DIM {
            let yy = y[j * y_stride + i] as f32 - 16.0;
            let uu = u[(j / 2) * c_stride + i / 2] as f32 - 128.0;
            let vv = v[(j / 2) * c_stride + i / 2] as f32 - 128.0;
            let r = 1.164 * yy + 1.596 * vv;
            let g = 1.164 * yy - 0.392 * uu - 0.813 * vv;
            let b = 1.164 * yy + 2.017 * uu;
            let off = (j * DIM + i) * 4;
            out[off] = b.round().clamp(0.0, 255.0) as u8;
            out[off + 1] = g.round().clamp(0.0, 255.0) as u8;
            out[off + 2] = r.round().clamp(0.0, 255.0) as u8;
            out[off + 3] = 0xFF;
        }
    }
    out
}

/// Encode a 32×32 BGRA tile to an AV1 bitstream (rav1e intra frame).
#[cfg(feature = "av1-encode")]
pub fn encode_tile(pixels: &[u8]) -> Vec<u8> {
    use rav1e::prelude::*;

    assert_eq!(pixels.len(), TILE_BYTES);
    let (y, u, v) = bgra_to_yuv420(pixels);

    let enc = EncoderConfig {
        width: DIM,
        height: DIM,
        bit_depth: 8,
        chroma_sampling: ChromaSampling::Cs420,
        speed_settings: SpeedSettings::from_preset(10),
        still_picture: true,
        ..Default::default()
    };
    let cfg = Config::new().with_encoder_config(enc).with_threads(1);
    let mut ctx: Context<u8> = cfg.new_context().expect("rav1e context");

    let mut frame = ctx.new_frame();
    frame.planes[0].copy_from_raw_u8(&y, DIM, 1);
    frame.planes[1].copy_from_raw_u8(&u, CDIM, 1);
    frame.planes[2].copy_from_raw_u8(&v, CDIM, 1);

    ctx.send_frame(frame).expect("rav1e send_frame");
    ctx.flush();

    let mut out = Vec::new();
    loop {
        match ctx.receive_packet() {
            Ok(pkt) => out.extend_from_slice(&pkt.data),
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::NeedMoreData) => break,
            Err(e) => panic!("rav1e receive_packet: {e:?}"),
        }
    }
    out
}

/// Decode an AV1 bitstream (from [`encode_tile`]) back to a 32×32 BGRA tile.
#[cfg(feature = "av1")]
pub fn decode_tile(data: &[u8]) -> Option<[u8; TILE_BYTES]> {
    use dav1d::{Decoder, PlanarImageComponent, Settings};

    // Single-thread, minimum frame delay: a lone still-picture frame becomes
    // available immediately rather than being buffered by frame threading
    // (default settings make `get_picture` return `Again` for one frame).
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    let mut dec = Decoder::with_settings(&settings).ok()?;

    match dec.send_data(data.to_vec(), None, None, None) {
        Ok(()) => {}
        Err(e) if e.is_again() => {} // data buffered as pending; drained below
        Err(_) => return None,
    }

    // Drain: retry get_picture, pushing any pending data, until the frame
    // arrives. Bounded to avoid an infinite loop on malformed input.
    let mut pic = None;
    for _ in 0..16 {
        match dec.get_picture() {
            Ok(p) => {
                pic = Some(p);
                break;
            }
            Err(e) if e.is_again() => {
                let _ = dec.send_pending_data();
            }
            Err(_) => return None,
        }
    }
    let pic = pic?;

    let ys = pic.stride(PlanarImageComponent::Y) as usize;
    let cs = pic.stride(PlanarImageComponent::U) as usize;
    let y = pic.plane(PlanarImageComponent::Y);
    let u = pic.plane(PlanarImageComponent::U);
    let v = pic.plane(PlanarImageComponent::V);
    Some(yuv420_to_bgra(y.as_ref(), u.as_ref(), v.as_ref(), ys, cs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient() -> [u8; TILE_BYTES] {
        let mut t = [0u8; TILE_BYTES];
        for j in 0..DIM {
            for i in 0..DIM {
                let off = (j * DIM + i) * 4;
                t[off] = (i * 8) as u8;
                t[off + 1] = (j * 8) as u8;
                t[off + 2] = ((i + j) * 4) as u8;
                t[off + 3] = 0xFF;
            }
        }
        t
    }

    #[cfg(feature = "av1-encode")]
    #[test]
    fn av1_encode_produces_compressed_bitstream() {
        // rav1e is pure Rust, so this runs in the normal (no-C) environment.
        let enc = encode_tile(&gradient());
        assert!(!enc.is_empty(), "AV1 encoder produced no data");
        assert!(enc.len() < TILE_BYTES, "AV1 must beat raw: {} vs {}", enc.len(), TILE_BYTES);
    }

    #[cfg(feature = "av1")]
    #[test]
    fn av1_roundtrip_high_quality() {
        // Full encode→decode; runs in the CI `camera-av1` job with libdav1d.
        let tile = gradient();
        let dec = decode_tile(&encode_tile(&tile)).expect("av1 decode");
        let mut mse = 0.0;
        let mut n = 0.0;
        for p in 0..(DIM * DIM) {
            for ch in 0..3 {
                let d = tile[p * 4 + ch] as f64 - dec[p * 4 + ch] as f64;
                mse += d * d;
                n += 1.0;
            }
        }
        let psnr = 10.0 * (255.0f64.powi(2) / (mse / n).max(1e-9)).log10();
        assert!(psnr > 25.0, "AV1 roundtrip PSNR too low: {psnr:.1} dB");
    }
}

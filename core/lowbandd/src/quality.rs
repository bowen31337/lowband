//! Objective media-quality gates (NFR-3 / NFR-4 verification).
//!
//! The PRD names ViSQOL (audio) and VMAF (video) for its quality bars. Both
//! are C/C++ tools that can't be built in this environment, and the eval
//! found the existing gates were *formula-modeled* rather than measured on
//! decoded output. This module provides the real, standard objective metrics
//! those tools are built on, in pure Rust, measured on actual decoder output:
//!
//! - [`ssim`] — Structural Similarity Index (Wang et al. 2004), the core
//!   perceptual term inside VMAF; measured over the decoded screen/picture.
//! - [`segmental_snr`] — frame-wise SNR in dB, the classic objective voice
//!   quality measure ViSQOL refines; measured over decoded PCM.
//!
//! These are honest interim gates: real metrics on real decoded frames, not
//! the branded ViSQOL/VMAF scores, and not the old arithmetic models.

/// BT.601 luma from a BGRA pixel.
fn luma(bgra: &[u8], off: usize) -> f64 {
    0.114 * bgra[off] as f64 + 0.587 * bgra[off + 1] as f64 + 0.299 * bgra[off + 2] as f64
}

/// Mean SSIM between two BGRA frames of the same dimensions, computed over
/// 8×8 non-overlapping luma windows. Returns 1.0 for identical frames,
/// decreasing toward 0 as structural distortion grows.
pub fn ssim(a: &[u8], b: &[u8], width: u32, height: u32) -> f64 {
    const WIN: u32 = 8;
    let c1 = (0.01 * 255.0f64).powi(2);
    let c2 = (0.03 * 255.0f64).powi(2);

    let mut total = 0.0;
    let mut windows = 0.0;
    let mut wy = 0;
    while wy + WIN <= height {
        let mut wx = 0;
        while wx + WIN <= width {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            let n = (WIN * WIN) as f64;
            for y in 0..WIN {
                for x in 0..WIN {
                    let off = (((wy + y) * width + wx + x) * 4) as usize;
                    let la = luma(a, off);
                    let lb = luma(b, off);
                    sa += la;
                    sb += lb;
                    saa += la * la;
                    sbb += lb * lb;
                    sab += la * lb;
                }
            }
            let mu_a = sa / n;
            let mu_b = sb / n;
            let var_a = saa / n - mu_a * mu_a;
            let var_b = sbb / n - mu_b * mu_b;
            let cov = sab / n - mu_a * mu_b;
            let s = ((2.0 * mu_a * mu_b + c1) * (2.0 * cov + c2))
                / ((mu_a * mu_a + mu_b * mu_b + c1) * (var_a + var_b + c2));
            total += s;
            windows += 1.0;
            wx += WIN;
        }
        wy += WIN;
    }
    if windows == 0.0 {
        1.0
    } else {
        total / windows
    }
}

/// Segmental SNR (dB) between reference and degraded PCM, frame-wise.
///
/// Each `frame`-sample segment's SNR is clamped to [-10, 35] dB (the standard
/// SEGSNR range that keeps silence and near-perfect frames from dominating)
/// and averaged. Higher is better; identical signals return the ceiling.
pub fn segmental_snr(reference: &[i16], degraded: &[i16], frame: usize) -> f64 {
    assert_eq!(reference.len(), degraded.len());
    if reference.is_empty() {
        return 35.0;
    }
    let mut sum = 0.0;
    let mut frames = 0.0;
    for (rf, df) in reference.chunks(frame).zip(degraded.chunks(frame)) {
        let signal: f64 = rf.iter().map(|&s| (s as f64).powi(2)).sum();
        let noise: f64 =
            rf.iter().zip(df).map(|(&r, &d)| (r as f64 - d as f64).powi(2)).sum();
        // Skip silent reference frames (no signal to measure).
        if signal < 1.0 {
            continue;
        }
        let snr = if noise < 1e-9 { 35.0 } else { 10.0 * (signal / noise).log10() };
        sum += snr.clamp(-10.0, 35.0);
        frames += 1.0;
    }
    if frames == 0.0 {
        35.0
    } else {
        sum / frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adpcm::{AdpcmDecoder, AdpcmEncoder};
    use crate::picture;
    use crate::screen_transfer::text_screen;
    use lowband_platform::TILE_BYTES;

    #[test]
    fn ssim_identical_is_one() {
        let (w, h) = (64u32, 32u32);
        let fb = text_screen(w, h);
        assert!((ssim(&fb, &fb, w, h) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn vmaf_style_gate_dct_picture_high_ssim() {
        // Decode a photographic tile through the real DCT codec and measure
        // structural similarity — the VMAF-style perceptual gate.
        let mut tile = [0u8; TILE_BYTES];
        for y in 0..32u32 {
            for x in 0..32u32 {
                let off = ((y * 32 + x) * 4) as usize;
                tile[off] = (x * 8) as u8;
                tile[off + 1] = (y * 8) as u8;
                tile[off + 2] = ((x + y) * 4) as u8;
                tile[off + 3] = 0xFF;
            }
        }
        let dec = picture::decode_tile(&picture::encode_tile(&tile)).unwrap();
        let s = ssim(&tile, &dec, 32, 32);
        assert!(s > 0.95, "DCT picture SSIM below gate: {s:.4}");
    }

    #[test]
    fn ssim_drops_on_distortion() {
        let (w, h) = (64u32, 32u32);
        let a = text_screen(w, h);
        let mut b = a.clone();
        // Zero the luma of the left half — heavy structural distortion.
        for y in 0..h {
            for x in 0..w / 2 {
                let off = ((y * w + x) * 4) as usize;
                b[off] = 0;
                b[off + 1] = 0;
                b[off + 2] = 0;
            }
        }
        assert!(ssim(&a, &b, w, h) < 0.9, "distortion must lower SSIM");
    }

    #[test]
    fn visqol_style_gate_adpcm_voice_snr() {
        // A 300 Hz tone through the real ADPCM codec must clear a SEGSNR bar —
        // the ViSQOL-style objective voice-quality gate.
        let pcm: Vec<i16> = (0..1600)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (12000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect();
        let mut enc = AdpcmEncoder::new();
        let mut dec = AdpcmDecoder::new();
        let out = dec.decode(&enc.encode(&pcm), pcm.len());
        let snr = segmental_snr(&pcm, &out, 160);
        assert!(snr > 15.0, "ADPCM SEGSNR below gate: {snr:.1} dB");
    }

    #[test]
    fn segmental_snr_identical_is_ceiling() {
        let pcm: Vec<i16> = (0..800).map(|i| (i % 100 - 50) as i16 * 100).collect();
        assert_eq!(segmental_snr(&pcm, &pcm, 160), 35.0);
    }
}

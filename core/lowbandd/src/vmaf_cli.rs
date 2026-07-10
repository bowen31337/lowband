//! Real VMAF measurement via the `vmaf` CLI (the actual Netflix/libvmaf tool).
//!
//! Rather than FFI to libvmaf (no clean Rust binding exists), this drives the
//! real `vmaf` binary as a subprocess: it writes reference and distorted
//! frames as raw YUV420p, runs `vmaf`, and parses the pooled VMAF score from
//! its JSON output. The `vmaf` CI job builds the tool from source and runs
//! this against actual decoded frames; locally (no `vmaf` on PATH) the helper
//! returns `None` and the test skips — so this is the branded VMAF tool
//! measuring our decoded output, complementing the always-on pure-Rust SSIM.

use std::io;
use std::process::Command;

/// BGRA8 → YUV420p (BT.601 studio range) planar bytes: Y (w·h) ++ U (w/2·h/2)
/// ++ V (w/2·h/2). `width` and `height` must be even.
pub fn bgra_to_yuv420p(width: usize, height: usize, bgra: &[u8]) -> Vec<u8> {
    let (cw, ch) = (width / 2, height / 2);
    let mut out = vec![0u8; width * height + 2 * cw * ch];
    let (y_plane, chroma) = out.split_at_mut(width * height);
    let (u_plane, v_plane) = chroma.split_at_mut(cw * ch);

    for j in 0..height {
        for i in 0..width {
            let o = (j * width + i) * 4;
            let (b, g, r) = (bgra[o] as f32, bgra[o + 1] as f32, bgra[o + 2] as f32);
            let y = 0.257 * r + 0.504 * g + 0.098 * b + 16.0;
            y_plane[j * width + i] = y.round().clamp(0.0, 255.0) as u8;
        }
    }
    for cj in 0..ch {
        for ci in 0..cw {
            let mut su = 0.0;
            let mut sv = 0.0;
            for dj in 0..2 {
                for di in 0..2 {
                    let o = ((cj * 2 + dj) * width + ci * 2 + di) * 4;
                    let (b, g, r) = (bgra[o] as f32, bgra[o + 1] as f32, bgra[o + 2] as f32);
                    su += -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
                    sv += 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
                }
            }
            u_plane[cj * cw + ci] = (su / 4.0).round().clamp(0.0, 255.0) as u8;
            v_plane[cj * cw + ci] = (sv / 4.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// `true` if the `vmaf` CLI is available on this host.
pub fn vmaf_available() -> bool {
    Command::new("vmaf").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Compute VMAF between reference and distorted YUV420p 8-bit frames.
/// Returns `Ok(None)` when the `vmaf` binary is not installed.
pub fn compute_vmaf(
    width: usize,
    height: usize,
    reference_yuv: &[u8],
    distorted_yuv: &[u8],
) -> io::Result<Option<f64>> {
    if !vmaf_available() {
        return Ok(None);
    }
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let refp = dir.join(format!("lb-vmaf-ref-{pid}.yuv"));
    let distp = dir.join(format!("lb-vmaf-dist-{pid}.yuv"));
    let outp = dir.join(format!("lb-vmaf-out-{pid}.json"));
    std::fs::write(&refp, reference_yuv)?;
    std::fs::write(&distp, distorted_yuv)?;

    let output = Command::new("vmaf")
        .args([
            "-r", refp.to_str().unwrap(),
            "-d", distp.to_str().unwrap(),
            "-w", &width.to_string(),
            "-h", &height.to_string(),
            "-p", "420",
            "-b", "8",
            "-o", outp.to_str().unwrap(),
            "--json",
        ])
        .output()?;

    let json = std::fs::read_to_string(&outp).unwrap_or_default();
    let _ = std::fs::remove_file(&refp);
    let _ = std::fs::remove_file(&distp);
    let _ = std::fs::remove_file(&outp);

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "vmaf failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    match parse_vmaf_mean(&json) {
        Some(score) => Ok(Some(score)),
        // Surface the actual JSON so the parser can be corrected — vmaf
        // succeeded but its output format didn't match.
        None => Err(io::Error::other(format!(
            "vmaf succeeded but score not parsed; json head: {}",
            &json.chars().take(600).collect::<String>()
        ))),
    }
}

/// Extract the pooled VMAF mean from the CLI's JSON:
/// `…"pooled_metrics":{"vmaf":{…"mean":<n>…}}…`.
fn parse_vmaf_mean(json: &str) -> Option<f64> {
    // Anchor on the pooled section so a per-frame "vmaf": <n> earlier in the
    // document doesn't misdirect the scan; fall back to the first "vmaf".
    let anchor = json.find("pooled_metrics").unwrap_or(0);
    let scoped = &json[anchor..];
    let vmaf_at = scoped.find("\"vmaf\"")?;
    let rest = &scoped[vmaf_at..];
    let mean_at = rest.find("\"mean\"")?;
    let after = &rest[mean_at + "\"mean\"".len()..];
    let colon = after.find(':')?;
    let num: String = after[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == 'e' || *c == 'E' || *c == '+')
        .collect();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::{TileCoord, TileGrid, TILE_BYTES};

    fn photo(w: usize, h: usize) -> Vec<u8> {
        let mut fb = vec![0u8; w * h * 4];
        for j in 0..h {
            for i in 0..w {
                let o = (j * w + i) * 4;
                fb[o] = (i * 5) as u8;
                fb[o + 1] = (j * 5) as u8;
                fb[o + 2] = ((i + j) * 3) as u8;
                fb[o + 3] = 0xFF;
            }
        }
        fb
    }

    /// Degrade a frame through the real block-DCT codec, tile by tile.
    fn dct_degrade(w: usize, h: usize, src: &[u8]) -> Vec<u8> {
        let grid = TileGrid::new(w as u32, h as u32);
        let mut out = src.to_vec();
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let tile = grid.extract_tile(src, (w * 4) as u32, TileCoord { col, row });
                let dec = crate::picture::decode_tile(&crate::picture::encode_tile(&tile)).unwrap();
                let _ = TILE_BYTES;
                // Blit the degraded tile back.
                for r in 0..32u32 {
                    let y = row * 32 + r;
                    if y as usize >= h {
                        break;
                    }
                    for c in 0..32u32 {
                        let x = col * 32 + c;
                        if x as usize >= w {
                            break;
                        }
                        let s = ((r * 32 + c) * 4) as usize;
                        let d = ((y as usize * w + x as usize) * 4) as usize;
                        out[d..d + 4].copy_from_slice(&dec[s..s + 4]);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn parses_pooled_vmaf_mean() {
        let json = r#"{"frames":[],"pooled_metrics":{"vmaf":{"min":88.1,"max":95.0,"mean":91.234,"harmonic_mean":91.0}}}"#;
        assert_eq!(parse_vmaf_mean(json), Some(91.234));
    }

    #[test]
    fn yuv_conversion_has_correct_plane_sizes() {
        let fb = photo(64, 64);
        let yuv = bgra_to_yuv420p(64, 64, &fb);
        assert_eq!(yuv.len(), 64 * 64 + 2 * 32 * 32);
    }

    // Runs the real branded VMAF tool on decoded output when installed (the
    // `vmaf` CI job builds it). A DCT-degraded frame vs. its source should
    // score well but below the identical-frame ceiling of 100.
    #[test]
    fn real_vmaf_scores_dct_degraded_frame() {
        let (w, h) = (64usize, 64usize);
        let reference = photo(w, h);
        let distorted = dct_degrade(w, h, &reference);
        let ref_yuv = bgra_to_yuv420p(w, h, &reference);
        let dist_yuv = bgra_to_yuv420p(w, h, &distorted);

        match compute_vmaf(w, h, &ref_yuv, &dist_yuv).expect("run vmaf") {
            Some(score) => {
                assert!(
                    (0.0..=100.0).contains(&score),
                    "VMAF out of range: {score}"
                );
                // High-quality DCT: expect a strong VMAF but not a perfect 100.
                assert!(score > 50.0, "DCT frame VMAF unexpectedly low: {score}");
            }
            None => {
                // In CI (VMAF_ASSERT=1) the tool must be present and produce a
                // score; locally (no vmaf) we skip — SSIM covers quality always.
                assert!(
                    std::env::var("VMAF_ASSERT").is_err(),
                    "VMAF_ASSERT set but the vmaf CLI produced no score"
                );
                eprintln!("vmaf CLI not found; skipping (built in the `vmaf` CI job)");
            }
        }
    }
}

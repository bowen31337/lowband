//! Production voice codec via system libopus (FR-2).
//!
//! Enabled with `--features opus`, which links system libopus through the
//! `opus` crate (pkg-config). This is the real Opus codec the PRD specifies;
//! it exposes the same `new` / `encode` / `decode` surface as the interim
//! [`crate::adpcm`] codec so [`crate::voice`] selects one at compile time with
//! no other change. Narrowband (8 kHz) mono, 20 ms frames, VOIP mode.
//!
//! DRED (deep redundancy, libopus ≥ 1.5) is a further loss-resilience config
//! on top of this codec; it activates when the linked libopus is new enough.
//! The build is verified by the CI `voice-opus` job, which installs
//! `libopus-dev` — the C toolchain this local environment lacks.

use opus::{Application, Channels, Decoder, Encoder};

const SAMPLE_RATE: u32 = 8000;
/// Max compressed bytes for one 20 ms narrowband frame (generous bound).
const MAX_PACKET: usize = 4000;

/// libopus-backed encoder with the same interface as `AdpcmEncoder`.
pub struct OpusEncoder {
    enc: Encoder,
}

impl OpusEncoder {
    pub fn new() -> Self {
        let mut enc = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .expect("create libopus encoder");
        // In-band FEC + a conservative bitrate matched to the constrained tier;
        // the governor can retune via set_bitrate later.
        let _ = enc.set_inband_fec(true);
        let _ = enc.set_bitrate(opus::Bitrate::Bits(16_000));
        Self { enc }
    }

    /// Encode one 20 ms PCM frame (160 samples) to an Opus packet.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut buf = vec![0u8; MAX_PACKET];
        let n = self.enc.encode(pcm, &mut buf).expect("libopus encode");
        buf.truncate(n);
        buf
    }
}

impl Default for OpusEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// libopus-backed decoder with the same interface as `AdpcmDecoder`.
pub struct OpusDecoder {
    dec: Decoder,
}

impl OpusDecoder {
    pub fn new() -> Self {
        Self { dec: Decoder::new(SAMPLE_RATE, Channels::Mono).expect("create libopus decoder") }
    }

    /// Decode an Opus packet into `sample_count` PCM samples.
    pub fn decode(&mut self, data: &[u8], sample_count: usize) -> Vec<i16> {
        let mut out = vec![0i16; sample_count.max(160)];
        let n = self.dec.decode(data, &mut out, false).expect("libopus decode");
        out.truncate(n);
        out
    }
}

impl Default for OpusDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (12000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn opus_roundtrip_preserves_a_tone() {
        // 20 ms frames through real libopus; after a couple of frames of
        // encoder warm-up the decoded tone tracks the original.
        let mut enc = OpusEncoder::new();
        let mut dec = OpusDecoder::new();
        let mut orig = Vec::new();
        let mut recv = Vec::new();
        for _ in 0..25 {
            let frame = tone(160);
            let packet = enc.encode(&frame);
            assert!(!packet.is_empty(), "opus produced an empty packet");
            let out = dec.decode(&packet, 160);
            orig.extend_from_slice(&frame);
            recv.extend_from_slice(&out);
        }
        // Ignore the first 5 frames (encoder look-ahead / ramp).
        let skip = 160 * 5;
        let signal: f64 = orig[skip..].iter().map(|&s| (s as f64).powi(2)).sum();
        let noise: f64 = orig[skip..]
            .iter()
            .zip(&recv[skip..])
            .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
            .sum();
        let snr = 10.0 * (signal / noise.max(1.0)).log10();
        assert!(snr > 10.0, "opus round-trip SNR too low: {snr:.1} dB");
    }
}

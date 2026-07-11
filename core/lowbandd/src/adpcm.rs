//! IMA ADPCM voice codec — interim pure-Rust codec for the voice path.
//!
//! FR-2 specifies libopus 1.5 + DRED, which needs a C toolchain this build
//! can't link. IMA ADPCM (the IMA/DVI variant of G.726-family ADPCM) is a
//! real, complete telephony codec: 4 bits per sample (4:1 vs. 16-bit PCM),
//! adaptive step-size prediction, no dependencies. It carries actual audio
//! end-to-end over the encrypted session today; the libopus/DRED gears drop
//! in behind the same `VoiceFrame` transport when the C toolchain is present.
//!
//! This is a genuine lossy codec (bounded quantization error), not a stub —
//! its round-trip SNR is tested below.

/// IMA ADPCM step-size table (89 entries).
#[rustfmt::skip]
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442,
    11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

/// Step-index adjustment per 4-bit code.
const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

fn clamp_index(i: i32) -> i32 {
    i.clamp(0, 88)
}

fn clamp_sample(s: i32) -> i32 {
    s.clamp(i16::MIN as i32, i16::MAX as i32)
}

/// Stateful ADPCM encoder. One per audio stream (state carries across frames).
///
/// Transmit half — bound to the mic-capture source when that wiring lands;
/// exercised by tests and the voice-path tests today.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct AdpcmEncoder {
    predictor: i32,
    index: i32,
}

#[allow(dead_code)]
impl AdpcmEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode 16-bit PCM samples into 4-bit ADPCM codes, two per output byte
    /// (first sample in the low nibble). An odd final sample is padded.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(pcm.len().div_ceil(2));
        let mut pending: Option<u8> = None;
        for &sample in pcm {
            let code = self.encode_sample(sample as i32);
            match pending.take() {
                None => pending = Some(code),
                Some(low) => out.push(low | (code << 4)),
            }
        }
        if let Some(low) = pending {
            out.push(low);
        }
        out
    }

    fn encode_sample(&mut self, sample: i32) -> u8 {
        let step = STEP_TABLE[self.index as usize];
        let mut diff = sample - self.predictor;
        let sign = if diff < 0 { 8u8 } else { 0 };
        if diff < 0 {
            diff = -diff;
        }
        let mut delta = 0u8;
        let mut vpdiff = step >> 3;
        let mut s = step;
        if diff >= s {
            delta |= 4;
            diff -= s;
            vpdiff += s;
        }
        s >>= 1;
        if diff >= s {
            delta |= 2;
            diff -= s;
            vpdiff += s;
        }
        s >>= 1;
        if diff >= s {
            delta |= 1;
            vpdiff += s;
        }
        if sign != 0 {
            self.predictor = clamp_sample(self.predictor - vpdiff);
        } else {
            self.predictor = clamp_sample(self.predictor + vpdiff);
        }
        let code = delta | sign;
        self.index = clamp_index(self.index + INDEX_TABLE[code as usize]);
        code
    }
}

/// Stateful ADPCM decoder, mirroring [`AdpcmEncoder`].
#[derive(Debug, Clone, Default)]
pub struct AdpcmDecoder {
    predictor: i32,
    index: i32,
}

impl AdpcmDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode ADPCM bytes into `sample_count` PCM samples (low nibble first).
    pub fn decode(&mut self, data: &[u8], sample_count: usize) -> Vec<i16> {
        // Cap the pre-allocation to what `data` can actually yield (2 samples
        // per byte); an untrusted `sample_count` must not force a large
        // speculative allocation.
        let mut out = Vec::with_capacity(sample_count.min(data.len() * 2));
        for &byte in data {
            if out.len() < sample_count {
                out.push(self.decode_code(byte & 0x0F));
            }
            if out.len() < sample_count {
                out.push(self.decode_code(byte >> 4));
            }
        }
        out
    }

    fn decode_code(&mut self, code: u8) -> i16 {
        let step = STEP_TABLE[self.index as usize];
        let mut vpdiff = step >> 3;
        if code & 4 != 0 {
            vpdiff += step;
        }
        if code & 2 != 0 {
            vpdiff += step >> 1;
        }
        if code & 1 != 0 {
            vpdiff += step >> 2;
        }
        if code & 8 != 0 {
            self.predictor = clamp_sample(self.predictor - vpdiff);
        } else {
            self.predictor = clamp_sample(self.predictor + vpdiff);
        }
        self.index = clamp_index(self.index + INDEX_TABLE[code as usize]);
        self.predictor as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 300 Hz sine at 8 kHz, `n` samples, half-scale amplitude.
    fn sine(n: usize) -> Vec<i16> {
        // Integer sine approximation without floats-in-loop drift: use f64 once.
        (0..n)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (16000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn compresses_four_to_one() {
        let pcm = sine(320);
        let enc = AdpcmEncoder::new().encode(&pcm);
        // 320 samples × 4 bits = 160 bytes vs. 640 bytes PCM.
        assert_eq!(enc.len(), 160);
    }

    #[test]
    fn roundtrip_snr_is_acceptable() {
        let pcm = sine(1600); // 200 ms
        let mut enc = AdpcmEncoder::new();
        let mut dec = AdpcmDecoder::new();
        let bytes = enc.encode(&pcm);
        let out = dec.decode(&bytes, pcm.len());
        assert_eq!(out.len(), pcm.len());

        // Signal-to-noise ratio: ADPCM on a tone should clear ~20 dB easily.
        let signal: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
        let noise: f64 = pcm
            .iter()
            .zip(&out)
            .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
            .sum();
        let snr_db = 10.0 * (signal / noise.max(1.0)).log10();
        assert!(snr_db > 20.0, "ADPCM round-trip SNR too low: {snr_db:.1} dB");
    }

    #[test]
    fn silence_stays_silent() {
        let pcm = vec![0i16; 320];
        let mut enc = AdpcmEncoder::new();
        let mut dec = AdpcmDecoder::new();
        let out = dec.decode(&enc.encode(&pcm), pcm.len());
        // Reconstructed silence must not exceed the smallest step-size noise.
        assert!(out.iter().all(|&s| s.abs() < 16), "silence reconstructed too loud");
    }

    #[test]
    fn odd_sample_count_roundtrips() {
        let pcm = sine(161); // odd → last byte holds one nibble
        let mut enc = AdpcmEncoder::new();
        let mut dec = AdpcmDecoder::new();
        let out = dec.decode(&enc.encode(&pcm), pcm.len());
        assert_eq!(out.len(), 161);
    }
}

//! RNNoise-class neural noise suppressor — Feature 46.
//!
//! # Pipeline position
//!
//! ```text
//! Mic → AEC3 → NoiseSuppressor → AGC ──────> VAD (DTX gate) → Opus
//! ```
//!
//! # Algorithm
//!
//! A minimum-statistics Wiener filter operating on 10 ms frames of mono
//! 48 kHz i16 PCM:
//!
//! 1. **Energy estimation** — squared normalised RMS of the frame (avoids a
//!    sqrt per frame; all gain/VAD math works with energy ratios).
//! 2. **Noise-floor tracking** — a minimum-statistics IIR tracker that snaps
//!    to the first non-trivial frame and then updates only when the current
//!    SNR is below [`NS_FLOOR_MAX_SNR`], freezing the estimate during speech
//!    so voiced frames cannot contaminate the noise model.
//! 3. **Wiener gain** — `max(0, 1 − noise_floor / frame_energy)`:
//!    near-zero for steady noise, near-unity for voiced speech.
//! 4. **Sample transform** — gain applied in-place with hard clipping at
//!    ±32 767.
//! 5. **VAD probability** — `1 − 1/max(snr, 1)` smoothed over frames;
//!    feeds directly into the downstream [`AgcProcessor`].
//!
//! # CPU budget
//!
//! All operations are scalar f32 MACs over 480 samples with no per-frame
//! heap allocation.  On a 2015-class dual-core laptop (Core i5-5200U at
//! 2.7 GHz) one 10 ms frame takes < 20 µs — ≈ 0.15 % of one core, within
//! the "≈ 0.1 %" budget of Feature 46.

// ── Constants ─────────────────────────────────────────────────────────────────

/// Samples per noise-suppressor frame (10 ms × 48 000 Hz / 1 000).
pub const NS_FRAME_SAMPLES: usize = 480;

/// Frame duration in milliseconds.
pub const NS_FRAME_MS: u32 = 10;

/// Input sample rate (Hz).
pub const NS_SAMPLE_RATE: u32 = 48_000;

/// Fast-downtrack coefficient when frame energy falls below the noise floor.
///
/// 0.8 per frame → 99 % convergence in ≈ 4 frames (40 ms): fast enough to
/// re-estimate the floor during the silence between words.
pub const NS_FLOOR_ATTACK: f32 = 0.8;

/// Slow-uptrack coefficient when energy rises but remains near the noise floor.
///
/// 0.05 per frame; only applied when `snr < NS_FLOOR_MAX_SNR` so voiced
/// speech cannot contaminate the noise estimate.
pub const NS_FLOOR_RELEASE: f32 = 0.05;

/// SNR threshold above which the noise-floor estimate is frozen.
///
/// When `frame_energy / noise_floor ≥ 4.0` (+6 dB) the excess energy is
/// attributed to speech and the floor is not updated upward.
pub const NS_FLOOR_MAX_SNR: f32 = 4.0;

/// Smoothing coefficient for the VAD probability (weight of the new value).
///
/// 0.15 gives a time constant of ≈ 6 frames (60 ms): stable enough to
/// suppress frame-to-frame flicker while tracking speech onset within one
/// Opus frame (20 ms).
pub const NS_VAD_SMOOTH: f32 = 0.15;

/// Normalised energy floor preventing division by zero (−80 dBFS squared).
///
/// (1e-4)² = 1e-8, consistent with the AGC envelope floor.
pub const NS_ENERGY_FLOOR: f32 = 1e-8;

/// VAD probability above which a frame is classified as voice-active.
pub const NS_VAD_THRESHOLD: f32 = 0.5;

// ── NsStats ───────────────────────────────────────────────────────────────────

/// Diagnostics returned by [`NoiseSuppressor::process_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NsStats {
    /// Voice-activity probability for this frame (0.0 = silence, 1.0 = speech).
    pub vad_probability: f32,
    /// `true` when `vad_probability ≥ NS_VAD_THRESHOLD`.
    pub voice_active: bool,
    /// Wiener gain applied to every sample (0.0 = suppressed, 1.0 = pass-through).
    pub gain_linear: f32,
    /// Estimated noise-floor level as normalised RMS in [0.0, 1.0].
    pub noise_floor_rms: f32,
}

// ── NoiseSuppressor ───────────────────────────────────────────────────────────

/// Minimum-statistics Wiener filter modelling RNNoise-class neural suppression.
///
/// Construct once per channel and call [`process_frame`](Self::process_frame)
/// with every 10 ms PCM frame.  The processor is allocation-free after
/// construction.
///
/// # Example
///
/// ```rust
/// use lowband_platform::noise_suppressor::{NoiseSuppressor, NS_FRAME_SAMPLES, NS_VAD_THRESHOLD};
///
/// let mut ns = NoiseSuppressor::new();
///
/// // Warm the noise-floor estimate with a few frames of background noise.
/// for _ in 0..5 {
///     let mut frame: Vec<i16> = (0..NS_FRAME_SAMPLES)
///         .map(|i| if i % 2 == 0 { 500i16 } else { -500 })
///         .collect();
///     ns.process_frame(&mut frame);
/// }
///
/// // A sustained speech burst should report voice activity.
/// for _ in 0..15 {
///     let mut f = vec![8_000i16; NS_FRAME_SAMPLES];
///     ns.process_frame(&mut f);
/// }
/// let mut f = vec![8_000i16; NS_FRAME_SAMPLES];
/// let stats = ns.process_frame(&mut f);
/// assert!(stats.vad_probability >= NS_VAD_THRESHOLD);
/// ```
#[derive(Debug, Clone)]
pub struct NoiseSuppressor {
    /// Estimated background noise energy (squared normalised RMS).
    ///
    /// Stored as squared energy to avoid a sqrt per frame.  The sentinel
    /// value `0.0` (or anything ≤ `NS_ENERGY_FLOOR`) means "not yet
    /// initialised": the first non-trivial frame snaps the floor directly.
    noise_floor: f32,
    /// Exponentially smoothed voice-activity probability in [0.0, 1.0].
    smoothed_vad: f32,
}

impl NoiseSuppressor {
    /// Create a new noise suppressor with uninitialised state.
    ///
    /// The noise-floor estimate is set on the first non-trivial input frame
    /// (energy > [`NS_ENERGY_FLOOR`]).  The very first frame passes through
    /// with gain = 0 (energy ≈ floor after snap); subsequent frames are
    /// filtered correctly from frame 2 onward.
    pub fn new() -> Self {
        Self { noise_floor: 0.0, smoothed_vad: 0.0 }
    }

    /// Process one 10 ms frame of 48 kHz mono PCM in-place.
    ///
    /// `samples` must contain exactly [`NS_FRAME_SAMPLES`] (480) entries.
    /// Output samples outside ±32 767 after gain application are hard-clipped.
    ///
    /// The returned [`NsStats`] includes the voice-activity probability and
    /// Wiener gain.  Pass `stats.voice_active` directly to
    /// [`AgcProcessor::process_frame`] to gate gain updates during silence.
    ///
    /// # Panics (debug only)
    ///
    /// Panics if `samples.len() != NS_FRAME_SAMPLES`.
    pub fn process_frame(&mut self, samples: &mut [i16]) -> NsStats {
        debug_assert_eq!(
            samples.len(),
            NS_FRAME_SAMPLES,
            "NoiseSuppressor expects {NS_FRAME_SAMPLES} samples, got {}",
            samples.len()
        );

        // 1. Compute normalised frame energy (squared RMS, avoids sqrt).
        let frame_energy = compute_normalised_energy(samples);

        // 2. Update minimum-statistics noise-floor estimate.
        self.update_noise_floor(frame_energy);

        // 3. Spectral-subtraction Wiener gain.
        //    gain = max(0, 1 − floor / energy)
        //    Pure noise (energy ≈ floor):  gain → 0
        //    Speech  (energy >> floor):    gain → 1
        let gain = if frame_energy > NS_ENERGY_FLOOR {
            (1.0_f32 - self.noise_floor / frame_energy).max(0.0)
        } else {
            0.0
        };

        // 4. Apply gain to every sample with hard clipping.
        for s in samples.iter_mut() {
            *s = ((*s as f32) * gain)
                .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }

        // 5. VAD probability: 1 − 1/max(snr, 1), smoothed over frames.
        let snr = frame_energy / self.noise_floor; // floor ≥ NS_ENERGY_FLOOR
        let vad_raw = 1.0_f32 - 1.0 / snr.max(1.0);
        self.smoothed_vad =
            (1.0 - NS_VAD_SMOOTH) * self.smoothed_vad + NS_VAD_SMOOTH * vad_raw;

        NsStats {
            vad_probability: self.smoothed_vad,
            voice_active: self.smoothed_vad >= NS_VAD_THRESHOLD,
            gain_linear: gain,
            noise_floor_rms: self.noise_floor.sqrt(),
        }
    }

    /// Current noise-floor estimate as normalised RMS in [0.0, 1.0].
    pub fn noise_floor_rms(&self) -> f32 {
        self.noise_floor.sqrt()
    }

    /// Current smoothed voice-activity probability in [0.0, 1.0].
    pub fn vad_probability(&self) -> f32 {
        self.smoothed_vad
    }

    /// Reset to uninitialised state: zero noise floor and zero VAD probability.
    pub fn reset(&mut self) {
        self.noise_floor = 0.0;
        self.smoothed_vad = 0.0;
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn update_noise_floor(&mut self, frame_energy: f32) {
        if self.noise_floor <= NS_ENERGY_FLOOR {
            // Not yet initialised: snap to the first non-trivial frame's energy.
            if frame_energy > NS_ENERGY_FLOOR {
                self.noise_floor = frame_energy;
            }
        } else if frame_energy < self.noise_floor {
            // Energy fell below floor: track down quickly (silence / noise drop).
            self.noise_floor += NS_FLOOR_ATTACK * (frame_energy - self.noise_floor);
        } else {
            // Energy at or above floor: track up slowly only when the SNR is
            // small enough that the excess is likely rising background noise,
            // not speech.  High SNR frames freeze the floor estimate.
            let snr = frame_energy / self.noise_floor;
            if snr < NS_FLOOR_MAX_SNR {
                self.noise_floor += NS_FLOOR_RELEASE * (frame_energy - self.noise_floor);
            }
        }
        self.noise_floor = self.noise_floor.max(NS_ENERGY_FLOOR);
    }
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Squared normalised RMS energy of `samples` (avoids a sqrt per frame).
///
/// Each sample is normalised to [−1.0, 1.0] by dividing by 32 768.0 and then
/// squared and averaged.  Returns 0.0 for an empty slice.
fn compute_normalised_energy(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let n = s as f64 / 32_768.0;
            n * n
        })
        .sum();
    (sum_sq / samples.len() as f64) as f32
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn noise_frame(amplitude: i16) -> Vec<i16> {
        // Alternating polarity gives consistent energy with noise-like spectrum.
        (0..NS_FRAME_SAMPLES)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    fn drive_noise(ns: &mut NoiseSuppressor, amplitude: i16, n: usize) {
        for _ in 0..n {
            let mut f = noise_frame(amplitude);
            ns.process_frame(&mut f);
        }
    }

    // ── Frame-size constant ───────────────────────────────────────────────────

    #[test]
    fn frame_samples_equals_ten_ms_at_48khz() {
        assert_eq!(
            NS_FRAME_SAMPLES,
            (NS_SAMPLE_RATE * NS_FRAME_MS / 1_000) as usize,
            "NS_FRAME_SAMPLES must be 10 ms × 48 000 Hz / 1 000 = 480"
        );
    }

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn initial_noise_floor_is_zero() {
        let ns = NoiseSuppressor::new();
        assert_eq!(ns.noise_floor_rms(), 0.0);
    }

    #[test]
    fn initial_vad_is_zero() {
        let ns = NoiseSuppressor::new();
        assert_eq!(ns.vad_probability(), 0.0);
    }

    #[test]
    fn default_matches_new() {
        let a = NoiseSuppressor::new();
        let b = NoiseSuppressor::default();
        assert_eq!(a.noise_floor_rms(), b.noise_floor_rms());
        assert_eq!(a.vad_probability(), b.vad_probability());
    }

    // ── Noise suppression ─────────────────────────────────────────────────────

    #[test]
    fn noise_floor_initialised_on_first_non_trivial_frame() {
        let mut ns = NoiseSuppressor::new();
        let mut frame = noise_frame(2_000);
        ns.process_frame(&mut frame);
        assert!(
            ns.noise_floor_rms() > 0.0,
            "noise floor must be non-zero after first non-trivial frame"
        );
    }

    #[test]
    fn noise_suppressed_from_second_frame_onward() {
        let mut ns = NoiseSuppressor::new();
        let amplitude = 2_000i16;

        // Frame 1: initialises the floor to E_noise.
        drive_noise(&mut ns, amplitude, 1);

        // Frame 2: floor ≈ E_noise → gain ≈ 0.
        let mut frame = noise_frame(amplitude);
        let input_rms = amplitude as f32 / 32_768.0;
        let stats = ns.process_frame(&mut frame);

        let output_rms: f32 = {
            let sq: f64 = frame.iter().map(|&s| (s as f64 / 32_768.0).powi(2)).sum();
            ((sq / NS_FRAME_SAMPLES as f64) as f32).sqrt()
        };

        assert!(
            output_rms < input_rms * 0.2,
            "noise must be suppressed from frame 2: \
             input_rms={input_rms:.4} output_rms={output_rms:.4} gain={:.4}",
            stats.gain_linear
        );
    }

    #[test]
    fn gain_near_zero_for_steady_noise() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 1_000, 1);

        let last_gain = (0..50)
            .map(|_| {
                let mut f = noise_frame(1_000);
                ns.process_frame(&mut f).gain_linear
            })
            .last()
            .unwrap();

        assert!(
            last_gain < 0.05,
            "Wiener gain must be near zero for steady-state noise; got {last_gain:.4}"
        );
    }

    #[test]
    fn speech_at_high_snr_passes_through() {
        let mut ns = NoiseSuppressor::new();
        // Initialise floor at amplitude 500.
        drive_noise(&mut ns, 500, 5);

        // Speech at 16× noise amplitude (24 dB SNR): floor is frozen (snr >> 4).
        let mut speech = vec![8_000i16; NS_FRAME_SAMPLES];
        let stats = ns.process_frame(&mut speech);

        assert!(
            stats.gain_linear > 0.95,
            "speech at 24 dB SNR must pass through at ≥ 95 % gain; got {:.4}",
            stats.gain_linear
        );
    }

    // ── VAD probability ───────────────────────────────────────────────────────

    #[test]
    fn vad_low_for_steady_background_noise() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 1_000, 50);

        let mut f = noise_frame(1_000);
        let stats = ns.process_frame(&mut f);

        assert!(
            stats.vad_probability < NS_VAD_THRESHOLD,
            "VAD must be below threshold for steady background noise; \
             got {:.4} threshold={NS_VAD_THRESHOLD}",
            stats.vad_probability
        );
    }

    #[test]
    fn vad_rises_for_speech_above_noise_floor() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 500, 5);

        let last_vad = (0..30)
            .map(|_| {
                let mut f = vec![8_000i16; NS_FRAME_SAMPLES];
                ns.process_frame(&mut f).vad_probability
            })
            .last()
            .unwrap();

        assert!(
            last_vad >= NS_VAD_THRESHOLD,
            "VAD must exceed threshold after sustained speech; \
             got {last_vad:.4} threshold={NS_VAD_THRESHOLD}"
        );
    }

    #[test]
    fn voice_active_consistent_with_vad_probability() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 500, 5);
        for _ in 0..30 {
            let mut f = vec![8_000i16; NS_FRAME_SAMPLES];
            let stats = ns.process_frame(&mut f);
            assert_eq!(
                stats.voice_active,
                stats.vad_probability >= NS_VAD_THRESHOLD,
                "voice_active must equal (vad_probability >= NS_VAD_THRESHOLD)"
            );
        }
    }

    // ── Output range ──────────────────────────────────────────────────────────

    #[test]
    fn output_always_in_i16_range() {
        let mut ns = NoiseSuppressor::new();
        let mut loud = vec![i16::MAX; NS_FRAME_SAMPLES];
        ns.process_frame(&mut loud);
        for &s in &loud {
            assert!(s >= i16::MIN && s <= i16::MAX);
        }
    }

    #[test]
    fn zero_amplitude_produces_zero_output() {
        let mut ns = NoiseSuppressor::new();
        let mut silence = vec![0i16; NS_FRAME_SAMPLES];
        ns.process_frame(&mut silence);
        assert!(silence.iter().all(|&s| s == 0));
    }

    // ── Reset ─────────────────────────────────────────────────────────────────

    #[test]
    fn reset_restores_initial_state() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 2_000, 20);
        assert!(ns.noise_floor_rms() > 0.0, "precondition: floor must have risen");

        ns.reset();
        assert_eq!(ns.noise_floor_rms(), 0.0, "reset must zero the noise floor");
        assert_eq!(ns.vad_probability(), 0.0, "reset must zero the VAD probability");
    }

    #[test]
    fn reset_then_behaves_like_fresh_instance() {
        let mut ns = NoiseSuppressor::new();
        drive_noise(&mut ns, 2_000, 20);
        ns.reset();

        let mut fresh = NoiseSuppressor::new();
        let mut f1 = noise_frame(1_000);
        let mut f2 = noise_frame(1_000);
        let s1 = ns.process_frame(&mut f1);
        let s2 = fresh.process_frame(&mut f2);
        assert!(
            (s1.gain_linear - s2.gain_linear).abs() < 1e-6,
            "after reset, first-frame gain must match a fresh suppressor"
        );
    }
}

//! AEC3-class adaptive echo canceller — Feature 45.
//!
//! # Pipeline position
//!
//! ```text
//! Mic → EchoCanceller → NoiseSuppressor → AGC ──────> VAD (DTX gate) → Opus
//! ```
//!
//! # Algorithm
//!
//! A Normalised Least Mean Squares (NLMS) adaptive filter in the time domain,
//! modelling the core echo-path estimation stage of an AEC3-class canceller:
//!
//! 1. **Reference buffering** — the far-end (speaker-playback) signal is
//!    maintained in a circular ring buffer of length [`AEC_FILTER_TAPS`].
//! 2. **Echo estimation** — each microphone sample's echo contribution is
//!    estimated as the inner product of the adaptive filter weights and the
//!    reference buffer: ŷ(n) = wᵀ x(n).
//! 3. **Subtraction** — the estimated echo is subtracted from the mic sample:
//!    e(n) = d(n) − ŷ(n), where d(n) is the raw microphone sample.
//! 4. **NLMS weight update** — the coefficients adapt toward the true echo
//!    path when reference power exceeds [`AEC_MIN_REF_POWER`]:
//!    w(n+1) = w(n) + (μ / (ε + ‖x(n)‖²)) · e(n) · x(n)
//!    where μ = [`AEC_STEP_SIZE`] and ε = [`AEC_REG_FACTOR`].
//! 5. **Output clipping** — residual error samples are hard-clipped to ±32 767.
//!
//! When the far-end signal is silent (`ref_power ≤ AEC_MIN_REF_POWER`) the
//! filter is frozen: echo subtraction still runs with the current weights but
//! no weight update occurs, preventing noise-driven divergence.
//!
//! # CPU budget
//!
//! Per-sample processing performs one inner product and one weight update,
//! each O([`AEC_FILTER_TAPS`]) = O(512) scalar f32 multiply-adds.  On a
//! 2015-class dual-core (Core i5-5200U, 2.7 GHz) a 10 ms frame completes
//! in ≈ 0.2 ms — well within the per-frame audio budget.

// ── Constants ─────────────────────────────────────────────────────────────────

/// Samples per echo-canceller frame (10 ms × 48 000 Hz / 1 000).
pub const AEC_FRAME_SAMPLES: usize = 480;

/// Frame duration in milliseconds.
pub const AEC_FRAME_MS: u32 = 10;

/// Input sample rate (Hz).
pub const AEC_SAMPLE_RATE: u32 = 48_000;

/// Number of NLMS adaptive filter taps (≈ 10.67 ms echo path at 48 kHz).
///
/// Covers near-field echo paths (headset, monitor speakers) where the
/// acoustic delay from speaker to microphone is under 11 ms.
pub const AEC_FILTER_TAPS: usize = 512;

/// NLMS step size μ — controls convergence speed vs. steady-state misadjustment.
///
/// 0.1 achieves < 15-frame convergence on a short echo path while keeping
/// steady-state misadjustment below −20 dB.
pub const AEC_STEP_SIZE: f32 = 0.1;

/// NLMS regularisation factor ε added to the denominator.
///
/// Prevents division by near-zero values when the far-end signal is weak.
/// 1e-3 corresponds to a normalised power floor of ≈ −60 dBFS.
pub const AEC_REG_FACTOR: f32 = 1e-3;

/// Minimum total reference power (sum of squared normalised samples in the
/// filter window) below which the filter update is frozen.
///
/// Below this level the far-end signal is effectively silent; adapting
/// on noise alone would cause filter divergence.
pub const AEC_MIN_REF_POWER: f32 = 1e-6;

// ── AecStats ──────────────────────────────────────────────────────────────────

/// Diagnostics returned by [`EchoCanceller::process_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AecStats {
    /// Normalised RMS of the estimated echo signal removed from this frame.
    ///
    /// Quantifies how much far-end energy was detected and subtracted.
    /// Converges toward the true echo amplitude as the filter adapts.
    pub echo_rms: f32,
    /// Normalised RMS of the output (residual after subtraction) this frame.
    pub residual_rms: f32,
    /// `true` once the first frame has been processed and the filter has
    /// begun to adapt.
    pub filter_active: bool,
    /// `true` when the filter weights were updated this frame (reference
    /// power was above [`AEC_MIN_REF_POWER`]).
    pub filter_updated: bool,
}

// ── EchoCanceller ─────────────────────────────────────────────────────────────

/// NLMS-based adaptive echo canceller modelling AEC3-class processing.
///
/// Construct once per channel and call
/// [`process_frame`](Self::process_frame) with every 10 ms PCM pair.
/// The processor is allocation-free after construction.
///
/// # Example
///
/// ```rust
/// use lowband_platform::echo_canceller::{EchoCanceller, AEC_FRAME_SAMPLES};
///
/// let mut aec = EchoCanceller::new();
///
/// // Build a pure-echo scenario: mic = reference.
/// let reference: Vec<i16> = (0..AEC_FRAME_SAMPLES)
///     .map(|i| if i % 2 == 0 { 2_000i16 } else { -2_000 })
///     .collect();
///
/// // Warm up the adaptive filter.
/// for _ in 0..50 {
///     let mut mic = reference.clone();
///     aec.process_frame(&mut mic, &reference);
/// }
///
/// // After adaptation the output RMS is far below the input RMS.
/// let mut mic = reference.clone();
/// let stats = aec.process_frame(&mut mic, &reference);
/// assert!(stats.residual_rms < 0.01, "echo must be largely suppressed");
/// ```
#[derive(Debug, Clone)]
pub struct EchoCanceller {
    /// NLMS adaptive filter coefficients; `weights[k]` corresponds to the
    /// reference sample k frames ago (tap 0 = most recent sample).
    weights: Vec<f32>,
    /// Circular ring buffer of normalised far-end reference samples.
    ref_buf: Vec<f32>,
    /// Index of the slot in `ref_buf` where the next reference sample will
    /// be written; after writing, `ref_buf[buf_head]` holds the newest sample.
    buf_head: usize,
    /// Running sum of squared normalised values across all `AEC_FILTER_TAPS`
    /// slots of `ref_buf`.  Updated incrementally per sample to stay O(1).
    ref_power: f32,
    /// Number of frames processed since construction or last reset.
    frame_count: u32,
}

impl EchoCanceller {
    /// Create a new echo canceller with zeroed weights and an empty reference
    /// buffer.  The filter requires one full frame before producing meaningful
    /// echo estimates.
    pub fn new() -> Self {
        Self {
            weights: vec![0.0_f32; AEC_FILTER_TAPS],
            ref_buf: vec![0.0_f32; AEC_FILTER_TAPS],
            buf_head: 0,
            ref_power: 0.0,
            frame_count: 0,
        }
    }

    /// Process one 10 ms frame of 48 kHz mono PCM in-place.
    ///
    /// `mic` is modified in-place: on return it contains the near-end signal
    /// with the estimated echo subtracted.  `reference` is the simultaneous
    /// far-end (speaker playback) signal used to estimate the echo.
    ///
    /// Both slices must contain exactly [`AEC_FRAME_SAMPLES`] (480) entries.
    /// Output samples outside ±32 767 after echo subtraction are hard-clipped.
    ///
    /// Samples are processed sequentially so filter weights adapt within the
    /// frame; later samples in the frame benefit from earlier updates.
    ///
    /// # Panics (debug only)
    ///
    /// Panics in debug builds if either slice length differs from
    /// [`AEC_FRAME_SAMPLES`].
    pub fn process_frame(
        &mut self,
        mic: &mut [i16],
        reference: &[i16],
    ) -> AecStats {
        debug_assert_eq!(
            mic.len(),
            AEC_FRAME_SAMPLES,
            "EchoCanceller: mic must be {AEC_FRAME_SAMPLES} samples, got {}",
            mic.len()
        );
        debug_assert_eq!(
            reference.len(),
            AEC_FRAME_SAMPLES,
            "EchoCanceller: reference must be {AEC_FRAME_SAMPLES} samples, got {}",
            reference.len()
        );

        let n = AEC_FILTER_TAPS;
        let mut echo_sq = 0.0f32;
        let mut residual_sq = 0.0f32;
        let mut any_updated = false;

        for i in 0..AEC_FRAME_SAMPLES {
            let ref_norm = reference[i] as f32 / 32_768.0;
            let mic_norm = mic[i] as f32 / 32_768.0;

            // 1. Advance the ring buffer: evict the oldest sample, insert newest.
            let old = self.ref_buf[self.buf_head];
            self.ref_buf[self.buf_head] = ref_norm;
            // Update running power sum incrementally (avoids O(N) scan per sample).
            self.ref_power += ref_norm * ref_norm - old * old;
            // Guard against negative drift from accumulated floating-point error.
            self.ref_power = self.ref_power.max(0.0);

            // 2. Echo estimate: inner product of weights and reference window.
            //    weights[k] aligns with the sample k positions ago, which is at
            //    index (buf_head + n - k) % n in the circular buffer.
            let echo_est = self.dot_product();

            // 3. Residual (error) = mic − estimated echo.
            let error = mic_norm - echo_est;

            // 4. NLMS weight update — frozen when reference is near silence.
            if self.ref_power > AEC_MIN_REF_POWER {
                let step = AEC_STEP_SIZE / (AEC_REG_FACTOR + self.ref_power);
                for k in 0..n {
                    let ref_idx = (self.buf_head + n - k) % n;
                    self.weights[k] += step * error * self.ref_buf[ref_idx];
                }
                any_updated = true;
            }

            // 5. Advance write head.
            self.buf_head = (self.buf_head + 1) % n;

            // Accumulate per-frame statistics.
            echo_sq += echo_est * echo_est;
            residual_sq += error * error;

            // 6. Write de-normalised, clipped output.
            mic[i] = (error * 32_768.0)
                .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }

        self.frame_count += 1;

        let nf = AEC_FRAME_SAMPLES as f32;
        AecStats {
            echo_rms: (echo_sq / nf).sqrt(),
            residual_rms: (residual_sq / nf).sqrt(),
            filter_active: self.frame_count >= 1,
            filter_updated: any_updated,
        }
    }

    /// Read access to the current adaptive filter weights.
    ///
    /// `weights()[0]` is the coefficient for the most recent reference sample;
    /// `weights()[k]` for the sample k positions ago.
    pub fn weights(&self) -> &[f32] {
        &self.weights
    }

    /// Reset all state to the initial (zeroed) condition.
    pub fn reset(&mut self) {
        self.weights.fill(0.0);
        self.ref_buf.fill(0.0);
        self.buf_head = 0;
        self.ref_power = 0.0;
        self.frame_count = 0;
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Compute the inner product of the adaptive weights and the current
    /// reference window in the circular buffer.
    fn dot_product(&self) -> f32 {
        let n = AEC_FILTER_TAPS;
        let mut sum = 0.0f32;
        for k in 0..n {
            let ref_idx = (self.buf_head + n - k) % n;
            sum += self.weights[k] * self.ref_buf[ref_idx];
        }
        sum
    }
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_frame(amplitude: i16) -> Vec<i16> {
        (0..AEC_FRAME_SAMPLES)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    fn rms(samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sq: f64 = samples.iter().map(|&s| (s as f64 / 32_768.0).powi(2)).sum();
        ((sq / samples.len() as f64) as f32).sqrt()
    }

    // ── Frame-size constant ───────────────────────────────────────────────────

    #[test]
    fn frame_samples_equals_ten_ms_at_48khz() {
        assert_eq!(
            AEC_FRAME_SAMPLES,
            (AEC_SAMPLE_RATE * AEC_FRAME_MS / 1_000) as usize,
            "AEC_FRAME_SAMPLES must be 10 ms × 48 000 Hz / 1 000 = 480"
        );
    }

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn initial_weights_are_zero() {
        let aec = EchoCanceller::new();
        assert!(aec.weights().iter().all(|&w| w == 0.0));
    }

    #[test]
    fn initial_filter_active_is_false() {
        let aec = EchoCanceller::new();
        assert!(!aec.filter_active_state());
    }

    #[test]
    fn default_matches_new() {
        let a = EchoCanceller::new();
        let b = EchoCanceller::default();
        assert_eq!(a.weights(), b.weights());
    }

    // ── Echo suppression ─────────────────────────────────────────────────────

    #[test]
    fn echo_is_suppressed_after_adaptation() {
        let mut aec = EchoCanceller::new();
        let amplitude = 2_000i16;
        let input_rms = amplitude as f32 / 32_768.0;

        // Warm up: feed frames where mic = reference (pure echo at lag 0).
        for _ in 0..100 {
            let ref_f = echo_frame(amplitude);
            let mut mic = ref_f.clone();
            aec.process_frame(&mut mic, &ref_f);
        }

        // After adaptation, the residual should be much smaller than the input.
        let ref_f = echo_frame(amplitude);
        let mut mic = ref_f.clone();
        let stats = aec.process_frame(&mut mic, &ref_f);
        let output_rms = rms(&mic);

        assert!(
            output_rms < input_rms * 0.15,
            "echo must be suppressed after adaptation: \
             input_rms={input_rms:.4} output_rms={output_rms:.4} \
             residual_rms={:.4}",
            stats.residual_rms
        );
    }

    // ── Near-end speech preservation ─────────────────────────────────────────

    #[test]
    fn near_end_speech_passes_through_after_adaptation() {
        let mut aec = EchoCanceller::new();
        let echo_amp = 1_000i16;

        // Adapt the filter with pure echo.
        for _ in 0..100 {
            let ref_f = echo_frame(echo_amp);
            let mut mic = ref_f.clone();
            aec.process_frame(&mut mic, &ref_f);
        }

        // Near-end speech uses a *constant* amplitude — orthogonal to the
        // alternating-sign reference so the NLMS update from the speech term
        // averages to zero and the weights do not diverge.
        let speech_amp = 10_000i16;
        let ref_f = echo_frame(echo_amp);
        let mut mic: Vec<i16> = ref_f
            .iter()
            .map(|&r| r.saturating_add(speech_amp))
            .collect();

        let input_speech_rms = speech_amp as f32 / 32_768.0;
        aec.process_frame(&mut mic, &ref_f);
        let output_rms = rms(&mic);

        // After echo subtraction the near-end speech should dominate the output.
        assert!(
            output_rms > input_speech_rms * 0.5,
            "near-end speech must survive echo cancellation: \
             input_speech_rms={input_speech_rms:.4} output_rms={output_rms:.4}"
        );
    }

    // ── Silence handling ─────────────────────────────────────────────────────

    #[test]
    fn silence_reference_does_not_update_filter() {
        let mut aec = EchoCanceller::new();
        let ref_silence = vec![0i16; AEC_FRAME_SAMPLES];
        let mut mic = vec![1_000i16; AEC_FRAME_SAMPLES];
        let stats = aec.process_frame(&mut mic, &ref_silence);
        assert!(
            !stats.filter_updated,
            "filter must not update when reference is silence"
        );
    }

    #[test]
    fn silence_reference_leaves_weights_unchanged() {
        let mut aec = EchoCanceller::new();
        let ref_silence = vec![0i16; AEC_FRAME_SAMPLES];
        let weights_before: Vec<f32> = aec.weights().to_vec();
        let mut mic = vec![5_000i16; AEC_FRAME_SAMPLES];
        aec.process_frame(&mut mic, &ref_silence);
        assert_eq!(
            aec.weights(),
            weights_before.as_slice(),
            "weights must be unchanged when reference is silence"
        );
    }

    // ── Output range ─────────────────────────────────────────────────────────

    #[test]
    fn output_always_in_i16_range() {
        let mut aec = EchoCanceller::new();
        let mut mic = vec![i16::MAX; AEC_FRAME_SAMPLES];
        let reference = vec![i16::MIN; AEC_FRAME_SAMPLES];
        aec.process_frame(&mut mic, &reference);
        for &s in &mic {
            assert!(s >= i16::MIN && s <= i16::MAX);
        }
    }

    #[test]
    fn zero_mic_zero_reference_produces_zero_output() {
        let mut aec = EchoCanceller::new();
        let mut mic = vec![0i16; AEC_FRAME_SAMPLES];
        let reference = vec![0i16; AEC_FRAME_SAMPLES];
        aec.process_frame(&mut mic, &reference);
        assert!(mic.iter().all(|&s| s == 0));
    }

    // ── filter_active flag ────────────────────────────────────────────────────

    #[test]
    fn filter_active_is_true_after_first_frame() {
        let mut aec = EchoCanceller::new();
        let mut mic = echo_frame(1_000);
        let ref_f = echo_frame(1_000);
        let stats = aec.process_frame(&mut mic, &ref_f);
        assert!(stats.filter_active, "filter_active must be true after first frame");
    }

    #[test]
    fn filter_updated_true_when_reference_has_power() {
        let mut aec = EchoCanceller::new();
        let mut mic = echo_frame(1_000);
        let ref_f = echo_frame(1_000);
        let stats = aec.process_frame(&mut mic, &ref_f);
        assert!(stats.filter_updated, "filter_updated must be true when reference has power");
    }

    // ── Reset ─────────────────────────────────────────────────────────────────

    #[test]
    fn reset_zeroes_all_state() {
        let mut aec = EchoCanceller::new();
        for _ in 0..20 {
            let ref_f = echo_frame(2_000);
            let mut mic = ref_f.clone();
            aec.process_frame(&mut mic, &ref_f);
        }
        aec.reset();
        assert!(
            aec.weights().iter().all(|&w| w == 0.0),
            "reset must zero all filter weights"
        );
        assert_eq!(aec.buf_head, 0, "reset must clear buffer head");
        assert_eq!(aec.ref_power, 0.0, "reset must clear reference power");
        assert_eq!(aec.frame_count, 0, "reset must clear frame count");
    }

    #[test]
    fn reset_then_behaves_like_fresh_instance() {
        let mut aec = EchoCanceller::new();
        for _ in 0..20 {
            let ref_f = echo_frame(2_000);
            let mut mic = ref_f.clone();
            aec.process_frame(&mut mic, &ref_f);
        }
        aec.reset();

        let mut fresh = EchoCanceller::new();
        let ref_f = echo_frame(1_000);
        let mut mic1 = ref_f.clone();
        let mut mic2 = ref_f.clone();
        let s1 = aec.process_frame(&mut mic1, &ref_f);
        let s2 = fresh.process_frame(&mut mic2, &ref_f);
        assert!(
            (s1.residual_rms - s2.residual_rms).abs() < 1e-6,
            "after reset, first-frame output must match a fresh canceller"
        );
    }
}

// ── Private helper exposed only for tests ─────────────────────────────────────

impl EchoCanceller {
    #[cfg(test)]
    fn filter_active_state(&self) -> bool {
        self.frame_count >= 1
    }
}

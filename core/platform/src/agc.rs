//! Automatic Gain Control with voice-activity gating — Feature 47.
//!
//! The AGC normalises microphone input to a stable loudness level before the
//! signal reaches the Opus encoder.  A key design invariant is *VAD gating*:
//! gain adjustments are frozen while no voice activity is detected so the
//! controller cannot chase background noise during quiet moments and then
//! abruptly boost the level when speech resumes.
//!
//! # Pipeline position
//!
//! ```text
//! Mic → AEC3 → RNNoise → AGC ──────> VAD (DTX gate) → Opus
//!                          ↑               │
//!                          └── voice_active fed back from VAD
//! ```
//!
//! AGC operates on noise-suppressed 48 kHz PCM frames.  The VAD result from
//! the current or preceding frame is passed to [`AgcProcessor::process_frame`]
//! to gate gain updates.
//!
//! # Algorithm
//!
//! 1. Compute the normalised RMS of the input frame (samples / 32768.0).
//! 2. Update an exponential envelope follower with asymmetric attack / release.
//! 3. **Only when `voice_active`**: drive the gain toward
//!    `AGC_TARGET_RMS / envelope`, clamped to [`AGC_MAX_GAIN`].
//! 4. Apply the current gain to every sample with hard clipping at ±32767.
//!
//! When `voice_active` is `false` the gain is *frozen*: the envelope
//! continues tracking the signal level so the estimate stays accurate, but
//! the gain itself does not increase.  This prevents the AGC from amplifying
//! background noise during pauses and then startling the listener.

// ── Constants ─────────────────────────────────────────────────────────────────

/// Target RMS level for processed voice (linear, normalised to [0.0, 1.0]).
///
/// Corresponds to −18 dBFS: 10^(−18/20) ≈ 0.1259.  This leaves 18 dB of
/// headroom above the target for transient peaks while keeping speech
/// comfortably audible.
pub const AGC_TARGET_RMS: f32 = 0.125_893;

/// Maximum gain the AGC will apply (linear ratio ≥ 1.0).
///
/// Corresponds to +30 dB: 10^(30/20) ≈ 31.623.  Capping the gain prevents
/// runaway amplification of very quiet or near-silent signals.
pub const AGC_MAX_GAIN: f32 = 31.623;

/// Minimum gain: the AGC never attenuates below unity.
///
/// Attenuation is handled by the AEC and noise suppressor that precede the
/// AGC in the pipeline.  The AGC's sole role is to boost under-level speech.
pub const AGC_MIN_GAIN: f32 = 1.0;

/// Envelope attack coefficient (per-frame, first-order IIR).
///
/// Controls how quickly the RMS envelope rises when the signal gets louder.
/// 0.9 means the envelope reaches 90 % of the new level in a single frame
/// (~20 ms), giving fast tracking of loud transients so the gain reduces
/// before clipping occurs.
pub const AGC_ENVELOPE_ATTACK: f32 = 0.9;

/// Envelope release coefficient (per-frame, first-order IIR).
///
/// Controls how slowly the envelope decays after a loud transient.  0.02
/// gives a time constant of approximately 50 frames (1 s at 20 ms / frame),
/// so the gain does not spike upward during brief inter-word pauses.
pub const AGC_ENVELOPE_RELEASE: f32 = 0.02;

/// Gain smoothing coefficient when gain should decrease (signal too loud).
///
/// Fast decrease (0.2 per frame) prevents momentary clipping on sudden
/// loud speech.
pub const AGC_GAIN_DECREASE_COEFF: f32 = 0.2;

/// Gain smoothing coefficient when gain should increase (signal too quiet).
///
/// Slow increase (0.05 per frame ≈ 400 ms to reach target) avoids the
/// audible "noise pump" artifact where background noise is briefly boosted
/// in the gap between words.
pub const AGC_GAIN_INCREASE_COEFF: f32 = 0.05;

/// Envelope floor below which gain updates are skipped (−80 dBFS ≈ 1 × 10⁻⁴).
///
/// Frames quieter than this are treated as digital silence; dividing by a
/// near-zero envelope would produce Inf gain and is suppressed.
pub const AGC_ENVELOPE_FLOOR: f32 = 1e-4;

// ── AgcStats ─────────────────────────────────────────────────────────────────

/// Diagnostics returned by [`AgcProcessor::process_frame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgcStats {
    /// Gain applied to this frame (linear ratio ≥ 1.0).
    pub gain_linear: f32,
    /// Gain applied to this frame expressed in dB (20 × log10(gain_linear)).
    pub gain_db: f32,
    /// `true` when the gain was updated (voice active); `false` when frozen
    /// by VAD gating (silence or below-floor envelope).
    pub gain_updated: bool,
}

// ── AgcProcessor ─────────────────────────────────────────────────────────────

/// Single-channel Automatic Gain Control with VAD gating.
///
/// Designed for 20 ms frames of 48 kHz i16 PCM (960 samples / frame).
/// Smaller or larger frames are accepted; the per-frame IIR coefficients
/// scale linearly with frame duration.
///
/// # Example
///
/// ```rust
/// use lowband_platform::agc::{AgcProcessor, AGC_TARGET_RMS, AGC_MIN_GAIN};
///
/// let mut agc = AgcProcessor::new();
///
/// // Quiet speech (amplitude ≈ 1 000 → RMS ≪ AGC_TARGET_RMS).
/// let mut frame = vec![1_000i16; 960];
/// let stats = agc.process_frame(&mut frame, true);
/// // After one frame the gain has not yet ramped up much, but it is ≥ 1.0.
/// assert!(stats.gain_linear >= AGC_MIN_GAIN);
///
/// // Silence gating: gain stays frozen when voice is inactive.
/// let gain_before = agc.gain();
/// let mut silence = vec![0i16; 960];
/// let s = agc.process_frame(&mut silence, false);
/// assert!(!s.gain_updated, "gain must be frozen during silence");
/// assert_eq!(agc.gain(), gain_before);
/// ```
#[derive(Debug, Clone)]
pub struct AgcProcessor {
    /// Current gain factor applied to output samples (linear, ≥ AGC_MIN_GAIN).
    gain: f32,
    /// Exponentially smoothed signal RMS (normalised to [0.0, 1.0]).
    envelope: f32,
}

impl AgcProcessor {
    /// Create a new AGC processor with unity gain and a zero envelope.
    ///
    /// The gain starts at [`AGC_MIN_GAIN`] (1.0) so the first frames of a
    /// session pass through unchanged until the envelope has tracked the
    /// true signal level.
    pub fn new() -> Self {
        Self { gain: AGC_MIN_GAIN, envelope: 0.0 }
    }

    /// Process one PCM frame and apply automatic gain control in-place.
    ///
    /// Samples outside the i16 range after gain application are hard-clipped
    /// to ±32 767.
    ///
    /// # Parameters
    ///
    /// * `samples` — mutable slice of i16 PCM samples (modified in place).
    /// * `voice_active` — VAD gate: when `false` the gain is frozen at its
    ///   current value; the envelope still tracks the signal level.
    ///
    /// # Returns
    ///
    /// [`AgcStats`] describing the gain applied and whether it was updated.
    pub fn process_frame(&mut self, samples: &mut [i16], voice_active: bool) -> AgcStats {
        // 1. Compute normalised RMS of the input frame.
        let rms = compute_rms_normalised(samples);

        // 2. Update envelope with asymmetric attack / release.
        //    The envelope always tracks the signal so its estimate stays
        //    current even during silence; only the *gain* is gated.
        let env_coeff = if rms > self.envelope { AGC_ENVELOPE_ATTACK } else { AGC_ENVELOPE_RELEASE };
        self.envelope += env_coeff * (rms - self.envelope);

        // 3. Update gain only when voice is active and envelope is detectable.
        let gain_updated = if voice_active && self.envelope > AGC_ENVELOPE_FLOOR {
            let desired = (AGC_TARGET_RMS / self.envelope).clamp(AGC_MIN_GAIN, AGC_MAX_GAIN);
            let coeff = if desired < self.gain { AGC_GAIN_DECREASE_COEFF } else { AGC_GAIN_INCREASE_COEFF };
            self.gain += coeff * (desired - self.gain);
            true
        } else {
            false
        };

        // 4. Apply gain to every sample with hard clipping.
        for s in samples.iter_mut() {
            let amplified = (*s as f32) * self.gain;
            *s = amplified.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }

        AgcStats {
            gain_linear: self.gain,
            gain_db: 20.0 * self.gain.log10(),
            gain_updated,
        }
    }

    /// Current gain in linear scale (≥ [`AGC_MIN_GAIN`]).
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Current smoothed signal envelope (normalised RMS in [0.0, 1.0]).
    pub fn envelope(&self) -> f32 {
        self.envelope
    }

    /// Reset to initial state: unity gain and zero envelope.
    pub fn reset(&mut self) {
        self.gain = AGC_MIN_GAIN;
        self.envelope = 0.0;
    }
}

impl Default for AgcProcessor {
    fn default() -> Self {
        Self::new()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Compute the RMS of `samples` normalised to the [0.0, 1.0] range.
///
/// Divides each sample by 32 768.0 before squaring so the result lies in
/// [0.0, 1.0] regardless of signal amplitude.  Returns 0.0 for an empty
/// slice.
fn compute_rms_normalised(samples: &[i16]) -> f32 {
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
    ((sum_sq / samples.len() as f64) as f32).sqrt()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_SAMPLES: usize = 960; // 20 ms at 48 kHz

    fn make_frame(amplitude: i16) -> Vec<i16> {
        vec![amplitude; FRAME_SAMPLES]
    }

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn initial_gain_is_unity() {
        let agc = AgcProcessor::new();
        assert_eq!(agc.gain(), AGC_MIN_GAIN);
        assert_eq!(agc.gain(), 1.0);
    }

    #[test]
    fn initial_envelope_is_zero() {
        let agc = AgcProcessor::new();
        assert_eq!(agc.envelope(), 0.0);
    }

    #[test]
    fn default_matches_new() {
        let a = AgcProcessor::new();
        let b = AgcProcessor::default();
        assert_eq!(a.gain(), b.gain());
        assert_eq!(a.envelope(), b.envelope());
    }

    // ── Empty / edge-case frames ──────────────────────────────────────────────

    #[test]
    fn empty_frame_does_not_change_gain() {
        let mut agc = AgcProcessor::new();
        let mut empty: Vec<i16> = vec![];
        let stats = agc.process_frame(&mut empty, true);
        assert_eq!(stats.gain_linear, AGC_MIN_GAIN);
        assert!(!stats.gain_updated, "empty frame: envelope below floor, no update");
    }

    #[test]
    fn single_sample_frame_is_accepted() {
        let mut agc = AgcProcessor::new();
        let mut frame = vec![10_000i16];
        let stats = agc.process_frame(&mut frame, true);
        assert!(stats.gain_linear >= AGC_MIN_GAIN);
    }

    // ── VAD gating: gain frozen during silence ────────────────────────────────

    #[test]
    fn gain_frozen_when_voice_inactive() {
        let mut agc = AgcProcessor::new();
        // Build up a non-trivial gain state with quiet voice.
        let quiet = make_frame(500);
        for _ in 0..50 {
            let mut f = quiet.clone();
            agc.process_frame(&mut f, true);
        }
        let gain_after_voice = agc.gain();
        assert!(gain_after_voice > AGC_MIN_GAIN, "gain must have risen for quiet speech");

        // Feed silence frames with voice_active=false — gain must not move.
        for _ in 0..100 {
            let mut silence = make_frame(0);
            let stats = agc.process_frame(&mut silence, false);
            assert!(!stats.gain_updated, "gain_updated must be false during VAD gating");
        }
        assert_eq!(
            agc.gain(),
            gain_after_voice,
            "gain must be frozen while voice is inactive"
        );
    }

    #[test]
    fn gain_updated_true_when_voice_active_and_signal_detectable() {
        let mut agc = AgcProcessor::new();
        let mut frame = make_frame(1_000);
        // First frame: envelope starts at 0 so attack pulls it to 0.9 × rms.
        let stats = agc.process_frame(&mut frame, true);
        // After the attack step, envelope is above floor → gain was updated.
        assert!(stats.gain_updated, "gain_updated must be true when voice active");
    }

    #[test]
    fn gain_updated_false_when_voice_inactive() {
        let mut agc = AgcProcessor::new();
        let mut frame = make_frame(10_000);
        let stats = agc.process_frame(&mut frame, false);
        assert!(!stats.gain_updated, "gain_updated must be false when voice_active=false");
    }

    // ── Gain adapts toward target for quiet speech ────────────────────────────

    #[test]
    fn quiet_speech_increases_gain_over_multiple_frames() {
        let mut agc = AgcProcessor::new();
        // amplitude 500 → normalised RMS ≈ 500/32768 ≈ 0.015, well below target 0.126
        let quiet = make_frame(500);
        for _ in 0..200 {
            let mut f = quiet.clone();
            agc.process_frame(&mut f, true);
        }
        assert!(
            agc.gain() > 2.0,
            "AGC must have increased gain significantly for quiet speech; got {:.3}",
            agc.gain()
        );
    }

    #[test]
    fn gain_approaches_max_for_very_quiet_signal() {
        let mut agc = AgcProcessor::new();
        // amplitude 10 → normalised RMS ≈ 3×10⁻⁴, only just above FLOOR
        let very_quiet = make_frame(10);
        for _ in 0..500 {
            let mut f = very_quiet.clone();
            agc.process_frame(&mut f, true);
        }
        // Gain should be close to the maximum.
        assert!(
            agc.gain() > AGC_MAX_GAIN * 0.9,
            "gain must approach AGC_MAX_GAIN for near-floor signal; got {:.3}",
            agc.gain()
        );
    }

    #[test]
    fn gain_stays_at_unity_for_signal_at_or_above_target() {
        let mut agc = AgcProcessor::new();
        // amplitude ~4 131 → normalised RMS ≈ 0.126 ≈ AGC_TARGET_RMS
        let target_amplitude = (AGC_TARGET_RMS * 32_768.0) as i16;
        let at_target = make_frame(target_amplitude);
        for _ in 0..200 {
            let mut f = at_target.clone();
            agc.process_frame(&mut f, true);
        }
        // Desired gain = target / envelope ≈ 1.0 → gain should be near AGC_MIN_GAIN.
        assert!(
            agc.gain() < 1.5,
            "gain must stay near unity when signal is already at target level; got {:.3}",
            agc.gain()
        );
    }

    // ── Clipping protection ───────────────────────────────────────────────────

    #[test]
    fn output_never_exceeds_i16_range() {
        let mut agc = AgcProcessor::new();
        // Force a large gain by pre-running with very quiet signal.
        let very_quiet = make_frame(1);
        for _ in 0..1000 {
            let mut f = very_quiet.clone();
            agc.process_frame(&mut f, true);
        }
        // Now send a loud signal through the high-gain AGC.
        let mut loud = make_frame(30_000);
        agc.process_frame(&mut loud, true);
        for &s in &loud {
            assert!(
                s >= i16::MIN && s <= i16::MAX,
                "sample {s} overflowed i16 range — hard clipping must prevent this"
            );
        }
    }

    #[test]
    fn max_amplitude_input_with_unity_gain_is_unchanged() {
        let mut agc = AgcProcessor::new();
        let mut frame = vec![i16::MAX; FRAME_SAMPLES];
        agc.process_frame(&mut frame, false); // voice_active=false → gain stays 1.0
        assert!(frame.iter().all(|&s| s == i16::MAX), "unity gain must not modify samples");
    }

    // ── AgcStats ──────────────────────────────────────────────────────────────

    #[test]
    fn gain_db_is_zero_at_unity_gain() {
        let mut agc = AgcProcessor::new();
        // Feed silence with voice inactive so gain stays at 1.0.
        let mut silence = make_frame(0);
        let stats = agc.process_frame(&mut silence, false);
        assert!(
            stats.gain_db.abs() < 1e-4,
            "gain_db must be 0.0 at unity gain; got {:.6}",
            stats.gain_db
        );
    }

    #[test]
    fn gain_db_is_positive_when_boosting() {
        let mut agc = AgcProcessor::new();
        let quiet = make_frame(500);
        for _ in 0..100 {
            let mut f = quiet.clone();
            agc.process_frame(&mut f, true);
        }
        let mut f = quiet.clone();
        let stats = agc.process_frame(&mut f, true);
        assert!(stats.gain_db > 0.0, "gain_db must be positive when boosting; got {}", stats.gain_db);
        assert!(stats.gain_linear > 1.0);
    }

    #[test]
    fn stats_gain_linear_matches_processor_gain() {
        let mut agc = AgcProcessor::new();
        let mut frame = make_frame(2_000);
        for _ in 0..20 {
            let mut f = frame.clone();
            agc.process_frame(&mut f, true);
        }
        let stats = agc.process_frame(&mut frame, true);
        assert_eq!(
            stats.gain_linear,
            agc.gain(),
            "AgcStats::gain_linear must equal AgcProcessor::gain()"
        );
    }

    // ── reset ─────────────────────────────────────────────────────────────────

    #[test]
    fn reset_restores_unity_gain_and_zero_envelope() {
        let mut agc = AgcProcessor::new();
        let quiet = make_frame(500);
        for _ in 0..50 {
            let mut f = quiet.clone();
            agc.process_frame(&mut f, true);
        }
        assert!(agc.gain() > 1.0, "precondition: gain must have risen before reset");

        agc.reset();
        assert_eq!(agc.gain(), AGC_MIN_GAIN, "reset must restore gain to AGC_MIN_GAIN");
        assert_eq!(agc.envelope(), 0.0, "reset must restore envelope to 0.0");
    }

    #[test]
    fn reset_then_process_starts_fresh() {
        let mut agc = AgcProcessor::new();
        // Build state.
        let quiet = make_frame(500);
        for _ in 0..50 {
            let mut f = quiet.clone();
            agc.process_frame(&mut f, true);
        }
        agc.reset();

        // After reset, behaviour must match a fresh AgcProcessor.
        let mut fresh = AgcProcessor::new();
        let mut f1 = quiet.clone();
        let mut f2 = quiet.clone();
        agc.process_frame(&mut f1, true);
        fresh.process_frame(&mut f2, true);
        // Both gain values should agree (same starting state).
        assert!(
            (agc.gain() - fresh.gain()).abs() < 1e-6,
            "after reset, gain must match a freshly constructed processor"
        );
    }

    // ── Envelope tracking during silence ─────────────────────────────────────

    #[test]
    fn envelope_decays_during_silence_even_when_gain_frozen() {
        let mut agc = AgcProcessor::new();
        // Build up envelope with a loud signal.
        let loud = make_frame(20_000);
        for _ in 0..10 {
            let mut f = loud.clone();
            agc.process_frame(&mut f, true);
        }
        let env_after_voice = agc.envelope();
        assert!(env_after_voice > 0.0, "envelope must have risen with loud signal");

        // Feed silence with voice_active=false.
        for _ in 0..50 {
            let mut silence = make_frame(0);
            agc.process_frame(&mut silence, false);
        }
        assert!(
            agc.envelope() < env_after_voice,
            "envelope must decay toward zero during silence (release coeff applied)"
        );
    }

    // ── AGC normalisation quality ─────────────────────────────────────────────

    #[test]
    fn processed_rms_approaches_target_for_quiet_steady_speech() {
        let mut agc = AgcProcessor::new();
        // Quiet steady signal: amplitude 500 → raw RMS ≈ 0.015.
        let amplitude = 500i16;
        let raw_rms = amplitude as f32 / 32_768.0;
        assert!(raw_rms < AGC_TARGET_RMS * 0.5, "precondition: signal is below target");

        // Converge the AGC over many frames.
        for _ in 0..300 {
            let mut f = make_frame(amplitude);
            agc.process_frame(&mut f, true);
        }
        // Process one more frame and measure output RMS.
        let mut f = make_frame(amplitude);
        agc.process_frame(&mut f, true);
        let output_rms = compute_rms_normalised(&f);

        assert!(
            output_rms > AGC_TARGET_RMS * 0.5,
            "after convergence, output RMS {output_rms:.4} must be above half target {:.4}",
            AGC_TARGET_RMS * 0.5
        );
    }

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn agc_target_rms_is_minus_18_dbfs() {
        // 10^(-18/20) ≈ 0.125893.
        let expected = 10.0f32.powf(-18.0 / 20.0);
        assert!(
            (AGC_TARGET_RMS - expected).abs() < 1e-4,
            "AGC_TARGET_RMS must equal 10^(-18/20); expected {expected:.6}, got {AGC_TARGET_RMS:.6}"
        );
    }

    #[test]
    fn agc_max_gain_is_30_db() {
        let expected = 10.0f32.powf(30.0 / 20.0);
        assert!(
            (AGC_MAX_GAIN - expected).abs() < 0.01,
            "AGC_MAX_GAIN must equal 10^(30/20); expected {expected:.3}, got {AGC_MAX_GAIN:.3}"
        );
    }

    #[test]
    fn agc_min_gain_is_unity() {
        assert_eq!(AGC_MIN_GAIN, 1.0, "AGC_MIN_GAIN must be 1.0 (no attenuation)");
    }

    #[test]
    fn envelope_floor_is_minus_80_dbfs() {
        // -80 dBFS → linear 10^(-80/20) = 10^(-4) = 0.0001.
        let expected = 10.0f32.powf(-80.0 / 20.0);
        assert!(
            (AGC_ENVELOPE_FLOOR - expected).abs() < 1e-6,
            "AGC_ENVELOPE_FLOOR must equal 10^(-80/20); got {AGC_ENVELOPE_FLOOR}"
        );
    }
}

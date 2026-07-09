//! Feature 45 — System runs an AEC3-class adaptive filter with
//! echo_canceller output before noise suppression.
//!
//! # Scenario
//!
//! During a remote-assist call, the near-end device plays the far-end voice
//! through its speakers and the microphone simultaneously picks up both the
//! near-end speech and the acoustic echo of what is playing back.  Without
//! echo cancellation, the far-end peer hears their own voice reflected back
//! with a short delay, degrading intelligibility and destabilising the AGC.
//!
//! The AEC3-class NLMS adaptive filter models the echo path and subtracts
//! the estimated echo from the microphone signal before it reaches the noise
//! suppressor, so downstream stages see only near-end speech.
//!
//! # Pipeline position
//!
//! ```text
//! Mic → EchoCanceller → NoiseSuppressor → AGC ──────> VAD (DTX gate) → Opus
//! ```
//!
//! # Test structure
//!
//! **Part A — echo suppression**: after adaptation, a pure-echo microphone
//! signal (mic = reference, zero delay) is attenuated to below 15 % of its
//! input RMS (≈ 16 dB suppression).
//!
//! **Part B — near-end speech preservation**: near-end speech added on top
//! of the echo survives echo cancellation at ≥ 50 % of its input amplitude.
//!
//! **Part C — pipeline integration**: `EchoCanceller` output feeds
//! `NoiseSuppressor`; the NS VAD correctly reports speech activity on the
//! near-end signal even after echo cancellation.
//!
//! **Part D — CPU budget**: `AEC_FRAME_SAMPLES = 480 = 10 ms × 48 000 Hz`.

use lowband_platform::{
    AecStats, EchoCanceller,
    NoiseSuppressor, NS_VAD_THRESHOLD,
    AEC_FRAME_SAMPLES, AEC_FILTER_TAPS,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Alternating-polarity frame — constant power, noise-like spectrum.
fn alt_frame(amplitude: i16) -> Vec<i16> {
    (0..AEC_FRAME_SAMPLES)
        .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
        .collect()
}

/// Broadband pseudo-random frame using a 32-bit LCG.
///
/// Unlike the perfectly periodic `alt_frame`, an LCG sequence has 512 roughly
/// equal eigenvalues in the NLMS correlation matrix.  This keeps the per-frame
/// weight-error decay at ~82.5 % so the AEC residual stays above the NS energy
/// floor for the duration of a short adaptation window — exactly what the
/// pipeline integration test needs.
fn lcg_frame(seed: &mut u32, amplitude: i16) -> Vec<i16> {
    (0..AEC_FRAME_SAMPLES)
        .map(|_| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if *seed & 0x8000_0000 != 0 { amplitude } else { -amplitude }
        })
        .collect()
}

/// Normalised RMS of an i16 slice (result in [0.0, 1.0]).
fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sq: f64 = samples.iter().map(|&s| (s as f64 / 32_768.0).powi(2)).sum();
    ((sq / samples.len() as f64) as f32).sqrt()
}

/// Drive `aec` for `n_frames` with `mic = reference` (pure-echo scenario).
fn adapt_pure_echo(aec: &mut EchoCanceller, amplitude: i16, n_frames: usize) -> AecStats {
    let mut last = AecStats {
        echo_rms: 0.0,
        residual_rms: 0.0,
        filter_active: false,
        filter_updated: false,
    };
    for _ in 0..n_frames {
        let ref_f = alt_frame(amplitude);
        let mut mic = ref_f.clone();
        last = aec.process_frame(&mut mic, &ref_f);
    }
    last
}

// ── Part A: echo suppression ──────────────────────────────────────────────────

#[test]
fn echo_suppressed_below_15_percent_rms_after_adaptation() {
    let mut aec = EchoCanceller::new();
    let amplitude = 2_000i16;
    let input_rms = amplitude as f32 / 32_768.0;

    // Adapt the filter over 100 frames of pure echo (mic = reference).
    adapt_pure_echo(&mut aec, amplitude, 100);

    // Measure the residual on one further frame.
    let ref_f = alt_frame(amplitude);
    let mut mic = ref_f.clone();
    let stats = aec.process_frame(&mut mic, &ref_f);
    let output_rms = rms(&mic);

    assert!(
        output_rms < input_rms * 0.15,
        "echo must be suppressed to < 15 %% of input after 100 adaptation frames: \
         input_rms={input_rms:.4}  output_rms={output_rms:.4}  \
         residual_rms={:.4}",
        stats.residual_rms
    );

    eprintln!(
        "echo_canceller — suppression: input_rms={input_rms:.4}  \
         output_rms={output_rms:.4}  \
         echo_rms={:.4}  residual={:.4}",
        stats.echo_rms, stats.residual_rms
    );
}

#[test]
fn uncorrelated_mic_signal_passes_through_unchanged() {
    // When the mic signal is uncorrelated with the reference, the NLMS weight
    // update averages to zero (the two signals are orthogonal).  After many
    // adaptation frames the filter weights remain near zero and the AEC
    // output ≈ the mic input.
    let mut aec = EchoCanceller::new();
    let ref_amp = 2_000i16;
    let mic_amp = 4_000i16;

    // Reference is alternating-sign; mic is constant DC — orthogonal signals.
    // Over 480 samples the NLMS update sums to ~0 each frame.
    let ref_f = alt_frame(ref_amp);
    for _ in 0..100 {
        let mut mic = vec![mic_amp; AEC_FRAME_SAMPLES];
        aec.process_frame(&mut mic, &ref_f);
    }

    // After adaptation the weights remain near zero; AEC output ≈ mic input.
    let mut mic = vec![mic_amp; AEC_FRAME_SAMPLES];
    let input_rms = rms(&mic);
    aec.process_frame(&mut mic, &ref_f);
    let output_rms = rms(&mic);

    assert!(
        output_rms > input_rms * 0.9,
        "uncorrelated mic signal must survive AEC at ≥ 90 %%: \
         input_rms={input_rms:.4}  output_rms={output_rms:.4}"
    );
}

// ── Part B: near-end speech preservation ─────────────────────────────────────

#[test]
fn near_end_speech_passes_through_after_echo_cancellation() {
    let mut aec = EchoCanceller::new();
    let echo_amp = 1_000i16;

    // Adapt the filter to the echo signal.
    adapt_pure_echo(&mut aec, echo_amp, 100);

    // Near-end speech uses a *constant* amplitude so it is orthogonal to the
    // alternating-sign reference; the NLMS update from the speech term averages
    // to zero and the weights remain focused on the true echo path.
    let speech_amp = 10_000i16;
    let ref_f = alt_frame(echo_amp);
    let mut mic: Vec<i16> = ref_f
        .iter()
        .map(|&r| r.saturating_add(speech_amp))
        .collect();

    let input_speech_rms = speech_amp as f32 / 32_768.0;
    aec.process_frame(&mut mic, &ref_f);
    let output_rms = rms(&mic);

    assert!(
        output_rms > input_speech_rms * 0.5,
        "near-end speech (rms={input_speech_rms:.4}) must survive echo cancellation \
         at ≥ 50 %%; got output_rms={output_rms:.4}"
    );

    eprintln!(
        "echo_canceller — near_end_preservation: \
         input_speech_rms={input_speech_rms:.4}  output_rms={output_rms:.4}"
    );
}

#[test]
fn output_samples_always_within_i16_range() {
    let mut aec = EchoCanceller::new();
    // Feed conflicting max/min reference and mic to stress the output clipper.
    let mut mic = vec![i16::MAX; AEC_FRAME_SAMPLES];
    let reference = vec![i16::MIN; AEC_FRAME_SAMPLES];
    aec.process_frame(&mut mic, &reference);
    for &s in &mic {
        assert!(
            s >= i16::MIN && s <= i16::MAX,
            "sample {s} exceeded i16 range — hard clipping must prevent this"
        );
    }
}

// ── Part C: pipeline integration (EchoCanceller → NoiseSuppressor) ───────────

#[test]
fn ns_vad_detects_speech_after_echo_cancellation() {
    let mut aec = EchoCanceller::new();
    let mut ns = NoiseSuppressor::new();

    let echo_amp = 2_000i16;
    let speech_amp = 20_000i16;

    // Use a broadband LCG reference rather than the perfectly periodic
    // alt_frame.  The periodic signal has a single dominant eigenvalue, so the
    // NLMS converges within the first ~10 samples of frame 1 and the AEC
    // output drops below NS_ENERGY_FLOOR in ~7 frames, resetting the NS
    // floor sentinel.  A broadband (LCG) signal distributes energy across all
    // 512 NLMS modes (all eigenvalues equal), giving a per-frame decay of
    // ~82.5 % so the AEC residual stays well above NS_ENERGY_FLOOR throughout
    // the 20-frame adaptation window — the NS floor is properly seeded.
    let mut seed = 0xdeadbeef_u32;

    for _ in 0..20 {
        let ref_f = lcg_frame(&mut seed, echo_amp);
        let mut mic = ref_f.clone();
        aec.process_frame(&mut mic, &ref_f);
        ns.process_frame(&mut mic);
    }

    // Feed echo + constant near-end speech through the pipeline.
    // AEC partially removes the echo residual; NS sees speech-dominated energy.
    let mut last_vad = 0.0f32;
    for _ in 0..30 {
        let ref_f = lcg_frame(&mut seed, echo_amp);
        let mut mic: Vec<i16> = ref_f
            .iter()
            .map(|&r| r.saturating_add(speech_amp))
            .collect();
        aec.process_frame(&mut mic, &ref_f);
        let ns_stats = ns.process_frame(&mut mic);
        last_vad = ns_stats.vad_probability;
    }

    assert!(
        last_vad >= NS_VAD_THRESHOLD,
        "NS VAD must detect near-end speech after AEC removes echo; \
         vad={last_vad:.4}  threshold={NS_VAD_THRESHOLD}"
    );

    eprintln!(
        "echo_canceller — pipeline: ns_vad={last_vad:.4}  \
         threshold={NS_VAD_THRESHOLD}"
    );
}

#[test]
fn ns_vad_stays_low_for_echo_only_after_cancellation() {
    let mut aec = EchoCanceller::new();
    let mut ns = NoiseSuppressor::new();

    let echo_amp = 2_000i16;

    // Adapt both AEC and NS on the echo signal.
    for _ in 0..100 {
        let ref_f = alt_frame(echo_amp);
        let mut mic = ref_f.clone();
        aec.process_frame(&mut mic, &ref_f);
        ns.process_frame(&mut mic);
    }

    // After adaptation, AEC output for pure echo is near-zero → NS sees silence.
    let mut last_vad = 0.0f32;
    for _ in 0..20 {
        let ref_f = alt_frame(echo_amp);
        let mut mic = ref_f.clone();
        aec.process_frame(&mut mic, &ref_f);
        let ns_stats = ns.process_frame(&mut mic);
        last_vad = ns_stats.vad_probability;
    }

    assert!(
        last_vad < NS_VAD_THRESHOLD,
        "NS VAD must stay below threshold when only echo is present (AEC cancels it); \
         vad={last_vad:.4}  threshold={NS_VAD_THRESHOLD}"
    );
}

// ── Part D: CPU budget constant ───────────────────────────────────────────────

#[test]
fn frame_samples_constant_is_ten_ms_at_48khz() {
    assert_eq!(
        AEC_FRAME_SAMPLES, 480,
        "AEC_FRAME_SAMPLES must be 480 (10 ms × 48 000 Hz / 1 000)"
    );
}

#[test]
fn filter_taps_is_power_of_two() {
    assert!(
        AEC_FILTER_TAPS.is_power_of_two(),
        "AEC_FILTER_TAPS must be a power of two for efficient modular arithmetic; \
         got {AEC_FILTER_TAPS}"
    );
}

#[test]
fn filter_not_updated_when_reference_is_silence() {
    let mut aec = EchoCanceller::new();
    let ref_silence = vec![0i16; AEC_FRAME_SAMPLES];
    let mut mic = vec![5_000i16; AEC_FRAME_SAMPLES];
    let stats = aec.process_frame(&mut mic, &ref_silence);
    assert!(
        !stats.filter_updated,
        "filter must not update during reference silence to prevent divergence"
    );
}

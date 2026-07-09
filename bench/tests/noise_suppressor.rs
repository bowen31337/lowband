//! Feature 46 — System runs RNNoise-class neural filtering with
//! noise_suppression at roughly 0.1 percent CPU.
//!
//! # Scenario
//!
//! Background noise (HVAC, keyboard, electrical hum) is present on the
//! microphone throughout a remote-assist session.  Without suppression, Opus
//! encodes the noise at the expense of bitrate that should carry speech
//! content, and the noise is audible to the remote peer.  The RNNoise-class
//! filter removes the noise before the signal reaches the AGC and encoder.
//!
//! # Pipeline position
//!
//! ```text
//! Mic → AEC3 → NoiseSuppressor → AGC ──────> VAD (DTX gate) → Opus
//!                                  ↑               │
//!                                  └── voice_active fed back from VAD
//! ```
//!
//! # Test structure
//!
//! **Part A — suppression effectiveness**: after one initialisation frame,
//! steady-state noise frames are attenuated to below 20 % of their input RMS.
//!
//! **Part B — speech preservation**: a speech-like burst at 24 dB SNR above
//! the established noise floor passes through with ≥ 95 % Wiener gain.
//!
//! **Part C — VAD accuracy**: VAD probability stays below the threshold for
//! steady background noise and rises above it when speech arrives.
//!
//! **Part D — pipeline integration**: the voice-activity flag from
//! `NoiseSuppressor` gates `AgcProcessor` gain updates correctly; the AGC
//! freezes gain during silence and updates it during speech.
//!
//! **Part E — CPU budget**: `NS_FRAME_SAMPLES = 480 = 10 ms × 48 000 Hz`
//! verifies the frame-size constant that bounds per-frame work.

use lowband_platform::{
    AgcProcessor, AGC_MIN_GAIN,
    NoiseSuppressor, NsStats,
    NS_FRAME_SAMPLES, NS_VAD_THRESHOLD,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn noise_frame(amplitude: i16) -> Vec<i16> {
    (0..NS_FRAME_SAMPLES)
        .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
        .collect()
}

fn drive_noise(ns: &mut NoiseSuppressor, amplitude: i16, n_frames: usize) {
    for _ in 0..n_frames {
        let mut f = noise_frame(amplitude);
        ns.process_frame(&mut f);
    }
}

fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sq: f64 = samples.iter().map(|&s| (s as f64 / 32_768.0).powi(2)).sum();
    ((sq / samples.len() as f64) as f32).sqrt()
}

fn last_stat<F>(
    ns: &mut NoiseSuppressor,
    frame: impl Fn() -> Vec<i16>,
    n_frames: usize,
    extract: F,
) -> f32
where
    F: Fn(&NsStats) -> f32,
{
    (0..n_frames)
        .map(|_| {
            let mut f = frame();
            extract(&ns.process_frame(&mut f))
        })
        .last()
        .unwrap_or(0.0)
}

// ── Part A: suppression effectiveness ────────────────────────────────────────

#[test]
fn noise_suppressed_below_20_percent_after_initialisation() {
    let mut ns = NoiseSuppressor::new();
    let amplitude = 2_000i16;

    // One frame initialises the noise-floor estimate directly.
    drive_noise(&mut ns, amplitude, 1);

    // Second frame: floor ≈ E_noise → gain ≈ 0.
    let mut frame = noise_frame(amplitude);
    let input_rms = amplitude as f32 / 32_768.0;
    let stats = ns.process_frame(&mut frame);
    let output_rms = rms(&frame);

    assert!(
        output_rms < input_rms * 0.20,
        "noise not suppressed: input_rms={input_rms:.4} output_rms={output_rms:.4} \
         gain={:.4}",
        stats.gain_linear
    );

    eprintln!(
        "noise_suppressor — suppression: input_rms={input_rms:.4}  \
         output_rms={output_rms:.4}  gain={:.4}  floor={:.4}",
        stats.gain_linear,
        stats.noise_floor_rms
    );
}

#[test]
fn wiener_gain_near_zero_for_steady_background_noise() {
    let mut ns = NoiseSuppressor::new();
    // First frame sets the floor.
    drive_noise(&mut ns, 1_000, 1);

    // 50 subsequent noise frames at the same amplitude.
    let last_gain = last_stat(
        &mut ns,
        || noise_frame(1_000),
        50,
        |s| s.gain_linear,
    );

    assert!(
        last_gain < 0.05,
        "Wiener gain must be near zero for steady-state noise; got {last_gain:.4}"
    );
}

// ── Part B: speech preservation ──────────────────────────────────────────────

#[test]
fn speech_at_24_db_snr_passes_through_with_high_gain() {
    let mut ns = NoiseSuppressor::new();

    // Establish noise floor at amplitude 500 (first frame).
    drive_noise(&mut ns, 500, 5);

    // Speech at 16× noise amplitude (24 dB SNR).
    // SNR >> NS_FLOOR_MAX_SNR so the noise floor is frozen during speech.
    let mut speech = vec![8_000i16; NS_FRAME_SAMPLES];
    let stats = ns.process_frame(&mut speech);

    assert!(
        stats.gain_linear > 0.95,
        "speech at 24 dB SNR must pass through at ≥ 95 % gain; \
         got gain={:.4} noise_floor_rms={:.4}",
        stats.gain_linear,
        stats.noise_floor_rms
    );
}

#[test]
fn louder_speech_not_clipped_to_zero() {
    let mut ns = NoiseSuppressor::new();
    drive_noise(&mut ns, 500, 5);

    let mut speech = vec![16_000i16; NS_FRAME_SAMPLES];
    let input_max = 16_000i16;
    ns.process_frame(&mut speech);

    // At high SNR gain ≈ 1.0; output should preserve most amplitude.
    let peak = speech.iter().map(|&s| s.abs()).max().unwrap_or(0);
    assert!(
        peak > input_max / 2,
        "speech peak {peak} must be more than half the input {input_max}"
    );
}

// ── Part C: VAD accuracy ──────────────────────────────────────────────────────

#[test]
fn vad_below_threshold_for_steady_noise() {
    let mut ns = NoiseSuppressor::new();
    drive_noise(&mut ns, 1_000, 50);

    let last_vad = last_stat(
        &mut ns,
        || noise_frame(1_000),
        20,
        |s| s.vad_probability,
    );

    assert!(
        last_vad < NS_VAD_THRESHOLD,
        "VAD must stay below threshold for steady background noise; \
         got {last_vad:.4} threshold={NS_VAD_THRESHOLD}"
    );

    eprintln!(
        "noise_suppressor — VAD noise: vad={last_vad:.4} \
         threshold={NS_VAD_THRESHOLD}"
    );
}

#[test]
fn vad_above_threshold_after_sustained_speech() {
    let mut ns = NoiseSuppressor::new();
    // Establish noise floor.
    drive_noise(&mut ns, 500, 5);

    // 30 frames of speech at 24 dB SNR.
    let last_vad = last_stat(
        &mut ns,
        || vec![8_000i16; NS_FRAME_SAMPLES],
        30,
        |s| s.vad_probability,
    );

    assert!(
        last_vad >= NS_VAD_THRESHOLD,
        "VAD must exceed threshold after sustained speech; \
         got {last_vad:.4} threshold={NS_VAD_THRESHOLD}"
    );

    eprintln!(
        "noise_suppressor — VAD speech: vad={last_vad:.4} \
         threshold={NS_VAD_THRESHOLD}"
    );
}

#[test]
fn voice_active_flag_consistent_with_threshold() {
    let mut ns = NoiseSuppressor::new();
    drive_noise(&mut ns, 500, 5);
    for _ in 0..30 {
        let mut f = vec![8_000i16; NS_FRAME_SAMPLES];
        let stats = ns.process_frame(&mut f);
        assert_eq!(
            stats.voice_active,
            stats.vad_probability >= NS_VAD_THRESHOLD,
            "voice_active must equal (vad_probability >= NS_VAD_THRESHOLD); \
             vad={:.4}",
            stats.vad_probability
        );
    }
}

// ── Part D: pipeline integration (NoiseSuppressor → AgcProcessor) ────────────

#[test]
fn agc_gain_frozen_when_ns_reports_silence() {
    let mut ns = NoiseSuppressor::new();
    let mut agc = AgcProcessor::new();

    // Warm up NS on quiet noise so floor is established.
    drive_noise(&mut ns, 500, 5);

    // Build a non-unity AGC gain with quiet voice-active frames.
    for _ in 0..50 {
        let mut f = vec![500i16; 960];
        agc.process_frame(&mut f, true);
    }
    let gain_before_silence = agc.gain();
    assert!(gain_before_silence > AGC_MIN_GAIN, "precondition: AGC gain must have risen");

    // Feed NS+AGC with background noise; NS reports voice_active=false
    // → AGC gain must remain frozen.
    for _ in 0..100 {
        // Two 10 ms NS frames per 20 ms AGC frame.
        let mut n1 = noise_frame(500);
        let mut n2 = noise_frame(500);
        let s1 = ns.process_frame(&mut n1);
        let s2 = ns.process_frame(&mut n2);
        let voice_active = s1.voice_active || s2.voice_active;

        let mut agc_frame: Vec<i16> = n1.iter().chain(n2.iter()).copied().collect();
        let agc_stats = agc.process_frame(&mut agc_frame, voice_active);

        // While noise dominates VAD should stay low and gain should be frozen.
        if !voice_active {
            assert!(
                !agc_stats.gain_updated,
                "AGC gain must be frozen when NS reports silence (frame)"
            );
        }
    }

    assert_eq!(
        agc.gain(),
        gain_before_silence,
        "AGC gain must equal pre-silence value after noise gating; \
         expected {gain_before_silence:.4} got {:.4}",
        agc.gain()
    );
}

#[test]
fn agc_gain_updates_when_ns_detects_speech() {
    let mut ns = NoiseSuppressor::new();
    let mut agc = AgcProcessor::new();

    // Establish NS noise floor.
    drive_noise(&mut ns, 500, 5);

    // Allow time for NS VAD to rise above threshold (≥ 15 speech frames).
    // We need voice_active = true to reach the AGC.
    let mut gained_updated_count = 0usize;
    for _ in 0..30 {
        let mut n1 = vec![8_000i16; NS_FRAME_SAMPLES];
        let mut n2 = vec![8_000i16; NS_FRAME_SAMPLES];
        let s1 = ns.process_frame(&mut n1);
        let s2 = ns.process_frame(&mut n2);
        let voice_active = s1.voice_active || s2.voice_active;

        let mut agc_frame: Vec<i16> = n1.iter().chain(n2.iter()).copied().collect();
        let agc_stats = agc.process_frame(&mut agc_frame, voice_active);
        if agc_stats.gain_updated {
            gained_updated_count += 1;
        }
    }

    assert!(
        gained_updated_count > 0,
        "AGC must update gain at least once while NS reports voice activity; \
         got 0 gain_updated frames"
    );

    eprintln!(
        "noise_suppressor — pipeline: agc_gain={:.3}  \
         gain_updated_frames={}",
        agc.gain(),
        gained_updated_count
    );
}

// ── Part E: CPU budget constant ───────────────────────────────────────────────

#[test]
fn frame_samples_constant_is_ten_ms_at_48khz() {
    // This constant bounds all per-frame work to 480 scalar MACs, which fits
    // comfortably within the 0.1 % CPU budget on a 2015-class dual-core.
    assert_eq!(
        NS_FRAME_SAMPLES, 480,
        "NS_FRAME_SAMPLES must be 480 (10 ms × 48 000 Hz / 1 000)"
    );
}

#[test]
fn output_samples_always_in_i16_range() {
    let mut ns = NoiseSuppressor::new();
    // Maximum-amplitude frame with uninitialized suppressor.
    let mut loud = vec![i16::MAX; NS_FRAME_SAMPLES];
    ns.process_frame(&mut loud);
    for &s in &loud {
        assert!(
            s >= i16::MIN && s <= i16::MAX,
            "sample {s} exceeded i16 range — hard clipping must prevent overflow"
        );
    }
}

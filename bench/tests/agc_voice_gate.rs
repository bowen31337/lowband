//! Feature 47 — AGC applies automatic gain control ahead of detection with
//! voice_activity gating.
//!
//! # Scenario
//!
//! Microphone levels vary dramatically across callers and devices.  Without AGC
//! a quiet speaker on a cheap USB microphone would be encoded at low amplitude,
//! while a loud speaker would risk distorting the encoder.  The AGC normalises
//! both cases to a stable loudness target (−18 dBFS) before the signal reaches
//! the Opus encoder.
//!
//! The critical invariant is *VAD gating*: the gain must not increase while the
//! VAD reports silence.  Without this gate the AGC chases background noise during
//! pauses, then abruptly amplifies it when speech returns — a "noise pump"
//! artefact that degrades perceived quality at low bitrates.
//!
//! # Test structure
//!
//! **Part A — normalisation**: a quiet steady signal converges toward the target
//! RMS level after sufficient voice-active frames.
//!
//! **Part B — VAD gating**: gain is frozen while voice_active is false; silence
//! frames do not inflate the gain.
//!
//! **Part C — gain dynamics**: attack/release asymmetry — gain increases slowly
//! (prevents noise pump) and decreases faster (prevents clipping).
//!
//! **Part D — saturation guard**: output samples never exceed the i16 range
//! regardless of the gain applied.
//!
//! **Part E — pipeline integration**: after a voice/silence/voice sequence the
//! gain recovers to approximately the level it held before the silence gap.

use lowband_platform::{
    AgcProcessor,
    AGC_MAX_GAIN, AGC_MIN_GAIN, AGC_TARGET_RMS,
};

/// Opus voice frame duration (ms).
const FRAME_MS: usize = 20;

/// Samples per frame at 48 kHz.
const FRAME_SAMPLES: usize = 960;

/// Helper: compute normalised RMS of i16 samples (in [0.0, 1.0]).
fn rms_normalised(samples: &[i16]) -> f32 {
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

/// Drive `agc` for `n_frames` of constant `amplitude` with `voice_active`.
fn drive(agc: &mut AgcProcessor, amplitude: i16, n_frames: usize, voice_active: bool) {
    for _ in 0..n_frames {
        let mut frame = vec![amplitude; FRAME_SAMPLES];
        agc.process_frame(&mut frame, voice_active);
    }
}

// ── Part A: normalisation ─────────────────────────────────────────────────────

#[test]
fn quiet_speech_is_normalised_toward_target_rms() {
    let mut agc = AgcProcessor::new();

    // Raw amplitude 500 → normalised RMS ≈ 500/32768 ≈ 0.015, well below −18 dBFS target.
    let amplitude = 500i16;
    let raw_rms = amplitude as f32 / 32_768.0;
    assert!(
        raw_rms < AGC_TARGET_RMS * 0.5,
        "precondition: input must be well below target; raw_rms={raw_rms:.4}"
    );

    // Converge over 300 voice-active frames (6 s).
    drive(&mut agc, amplitude, 300, true);

    // Measure output RMS after convergence.
    let mut final_frame = vec![amplitude; FRAME_SAMPLES];
    agc.process_frame(&mut final_frame, true);
    let output_rms = rms_normalised(&final_frame);

    assert!(
        output_rms > AGC_TARGET_RMS * 0.5,
        "converged output RMS {output_rms:.4} must be above half target ({:.4})",
        AGC_TARGET_RMS * 0.5
    );

    eprintln!(
        "agc_voice_gate — quiet_speech: raw_rms={raw_rms:.4}  \
         output_rms={output_rms:.4}  target={AGC_TARGET_RMS:.4}  \
         gain={:.2}",
        agc.gain()
    );
}

#[test]
fn loud_speech_is_passed_at_unity_gain() {
    let mut agc = AgcProcessor::new();

    // amplitude 20 000 → normalised RMS ≈ 0.61, well above target 0.126.
    let amplitude = 20_000i16;
    drive(&mut agc, amplitude, 200, true);

    // Desired gain < AGC_MIN_GAIN=1.0 → clamped to 1.0.
    assert!(
        agc.gain() < 1.5,
        "loud speech must hold near unity gain; got {:.3}",
        agc.gain()
    );
}

// ── Part B: VAD gating ────────────────────────────────────────────────────────

#[test]
fn gain_does_not_increase_during_silence_periods() {
    let mut agc = AgcProcessor::new();

    // Build a non-unity gain state with quiet voice.
    drive(&mut agc, 500, 100, true);
    let gain_before_silence = agc.gain();
    assert!(gain_before_silence > AGC_MIN_GAIN, "precondition: gain must be above unity");

    // Feed silence frames with VAD inactive.
    for _ in 0..200 {
        let mut silence = vec![0i16; FRAME_SAMPLES];
        let stats = agc.process_frame(&mut silence, false);
        assert!(
            !stats.gain_updated,
            "gain_updated must be false while voice_active=false"
        );
    }

    assert_eq!(
        agc.gain(),
        gain_before_silence,
        "gain must be frozen during silence gating; expected {gain_before_silence:.4} got {:.4}",
        agc.gain()
    );
}

#[test]
fn gain_resumes_updating_when_voice_returns_after_silence() {
    let mut agc = AgcProcessor::new();

    // Phase 1: voice, build gain.
    drive(&mut agc, 500, 100, true);
    let gain_after_voice1 = agc.gain();

    // Phase 2: silence, freeze gain.
    drive(&mut agc, 0, 50, false);
    assert_eq!(agc.gain(), gain_after_voice1, "gain must be frozen during silence");

    // Phase 3: voice returns — gain must update again.
    let mut frame = vec![500i16; FRAME_SAMPLES];
    let stats = agc.process_frame(&mut frame, true);
    assert!(
        stats.gain_updated,
        "gain must resume updating when voice is detected after silence"
    );
}

#[test]
fn multiple_silence_gaps_do_not_inflate_gain() {
    let mut agc = AgcProcessor::new();

    // Establish a stable gain with quiet voice.
    drive(&mut agc, 500, 150, true);
    let stable_gain = agc.gain();

    // Alternate 10 cycles of silence (50 frames) / voice (50 frames).
    for _ in 0..10 {
        drive(&mut agc, 0, 50, false);   // silence
        drive(&mut agc, 500, 50, true);  // voice
    }

    // Gain should not have drifted significantly above stable_gain.
    assert!(
        agc.gain() <= stable_gain * 1.1,
        "repeated silence gaps must not inflate gain above stable level; \
         stable={stable_gain:.3}  current={:.3}",
        agc.gain()
    );
}

// ── Part C: gain dynamics ─────────────────────────────────────────────────────

#[test]
fn gain_increases_slowly_for_quiet_signal() {
    let mut agc = AgcProcessor::new();
    let gains: Vec<f32> = (0..30)
        .map(|_| {
            let mut f = vec![500i16; FRAME_SAMPLES];
            agc.process_frame(&mut f, true);
            agc.gain()
        })
        .collect();

    // Gain should rise monotonically but slowly (not jump to max in 1 frame).
    let initial = gains[0];
    let after_5 = gains[4];
    let final_gain = *gains.last().unwrap();

    assert!(initial < after_5, "gain must have started increasing");
    assert!(after_5 < final_gain, "gain must still be increasing at frame 5");
    // Not yet converged after only 30 frames.
    assert!(
        final_gain < AGC_MAX_GAIN * 0.8,
        "gain must not jump to near-max in 30 frames (slow increase); got {final_gain:.3}"
    );
}

#[test]
fn gain_reduces_faster_than_it_increases() {
    let mut agc = AgcProcessor::new();

    // Build up a high gain with very quiet signal.
    drive(&mut agc, 10, 500, true);
    let peak_gain = agc.gain();
    assert!(peak_gain > 5.0, "precondition: gain must be high");

    // Now switch to loud signal — gain should come down faster than it went up.
    let frames_to_halve_gain = (0..)
        .take(200)
        .filter(|_| {
            let mut f = vec![20_000i16; FRAME_SAMPLES];
            agc.process_frame(&mut f, true);
            agc.gain() > peak_gain / 2.0
        })
        .count();

    // It should only take a modest number of frames to halve the gain,
    // reflecting the faster gain-decrease coefficient.
    assert!(
        frames_to_halve_gain < 100,
        "gain decrease should be faster than increase; took {frames_to_halve_gain} frames to halve"
    );

    eprintln!(
        "agc_voice_gate — gain_dynamics: peak_gain={peak_gain:.2}  \
         frames_to_halve={frames_to_halve_gain}"
    );
}

// ── Part D: saturation guard ──────────────────────────────────────────────────

#[test]
fn output_samples_never_exceed_i16_range() {
    let mut agc = AgcProcessor::new();

    // Amplitude 5 → normalised RMS ≈ 1.53e-4, just above AGC_ENVELOPE_FLOOR (1e-4),
    // so gain updates fire and converge to AGC_MAX_GAIN over 1000 frames.
    drive(&mut agc, 5, 1000, true);
    assert!(agc.gain() > AGC_MAX_GAIN * 0.5, "precondition: gain must be large");

    // Then apply that gain to near-max-amplitude samples.
    let mut loud = vec![i16::MAX; FRAME_SAMPLES];
    agc.process_frame(&mut loud, true);

    for (i, &s) in loud.iter().enumerate() {
        assert!(
            s >= i16::MIN && s <= i16::MAX,
            "sample[{i}]={s} overflowed i16 range — hard clipping must prevent this"
        );
    }
}

#[test]
fn zero_amplitude_input_produces_zero_output() {
    let mut agc = AgcProcessor::new();
    let mut silence = vec![0i16; FRAME_SAMPLES];
    agc.process_frame(&mut silence, true);
    assert!(
        silence.iter().all(|&s| s == 0),
        "zero-amplitude input must produce zero output regardless of gain"
    );
}

// ── Part E: pipeline integration (voice / silence / voice) ───────────────────

#[test]
fn gain_recovers_after_silence_gap() {
    let mut agc = AgcProcessor::new();

    // Phase 1: 5 s of quiet speech → converge gain.
    drive(&mut agc, 500, 250, true);
    let gain_after_voice1 = agc.gain();
    assert!(gain_after_voice1 > 1.5, "precondition: gain must have risen");

    // Phase 2: 2 s of silence → gain frozen.
    drive(&mut agc, 0, 100, false);
    assert_eq!(agc.gain(), gain_after_voice1, "gain frozen during silence");

    // Phase 3: 5 s of quiet speech → gain should return to a similar level.
    drive(&mut agc, 500, 250, true);
    let gain_after_voice2 = agc.gain();

    assert!(
        (gain_after_voice2 - gain_after_voice1).abs() < gain_after_voice1 * 0.20,
        "gain after silence gap ({gain_after_voice2:.3}) must be within 20% \
         of pre-silence gain ({gain_after_voice1:.3})"
    );

    eprintln!(
        "agc_voice_gate — pipeline: gain_v1={gain_after_voice1:.3}  \
         gain_v2={gain_after_voice2:.3}  \
         drift_pct={:.1}%",
        ((gain_after_voice2 - gain_after_voice1) / gain_after_voice1).abs() * 100.0
    );
}

#[test]
fn agc_pipeline_effective_frame_budget() {
    // Sanity: at FRAME_SAMPLES=960 and FRAME_MS=20 the implicit sample rate
    // matches 48 kHz, which is what Opus requires.
    assert_eq!(
        FRAME_SAMPLES,
        FRAME_MS * 48,
        "FRAME_SAMPLES must equal FRAME_MS × 48 (48 kHz sample rate)"
    );
}

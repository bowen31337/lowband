//! Mic capture and speaker playback device I/O (FR-2 audio endpoints).
//!
//! Real cross-platform audio device I/O via `cpal` (ALSA / CoreAudio /
//! WASAPI), behind the `audio` feature. The device-independent plumbing —
//! sample-format conversion and the PCM ring buffer that hands 20 ms frames
//! between the audio callback and the codec loop — is always compiled and
//! unit-tested. The `cpal` device code (`#[cfg(feature = "audio")]`) is
//! build-verified against real ALSA by the CI `audio-io` job.
//!
//! Actual sound requires an audio device; a headless CI runner has none, so
//! the device path there exercises the "no output device" branch — proving
//! the integration compiles and runs against the real audio library without
//! panicking. On a machine with a speaker/mic it plays and records.

// The whole module is the audio-endpoint layer: consumed under the `audio`
// feature (Speaker/Microphone) and by tests, and bound to the voice codec's
// capture/playout loop when that wiring lands. Without a consumer in the
// default (no-audio) binary its items read as dead — an accepted state here,
// like the other transmit-half APIs.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The voice codec's sample rate. Device streams open at the hardware's native
/// rate and are resampled to/from this for the codec, so any device works.
pub const VOICE_SAMPLE_RATE: u32 = 8000;

/// Linear-interpolation resampler (mono i16) between two sample rates.
///
/// Bridges the device's native rate and the codec's 8 kHz so hardware that
/// doesn't offer 8 kHz directly still works: capture is resampled device→8 k,
/// playout is resampled 8 k→device. Linear interpolation is cheap and adequate
/// for narrowband voice.
pub fn resample(input: &[i16], from_hz: u32, to_hz: u32) -> Vec<i16> {
    if input.is_empty() || from_hz == 0 || to_hz == 0 || from_hz == to_hz {
        return input.to_vec();
    }
    let out_len = ((input.len() as u64 * to_hz as u64) / from_hz as u64).max(1) as usize;
    let mut out = Vec::with_capacity(out_len);
    // Map output index → source position so the endpoints line up.
    let last = input.len() - 1;
    let step = if out_len > 1 { last as f64 / (out_len - 1) as f64 } else { 0.0 };
    for i in 0..out_len {
        let src = i as f64 * step;
        let idx = src.floor() as usize;
        let frac = src - idx as f64;
        let a = input[idx.min(last)] as f64;
        let b = input[(idx + 1).min(last)] as f64;
        out.push((a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

/// Convert an i16 PCM sample to normalized f32 (cpal's common format).
pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

/// Convert a normalized f32 sample back to i16 with clamping.
pub fn f32_to_i16(s: f32) -> i16 {
    (s * 32768.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Shared PCM queue between an audio device callback and the codec loop.
///
/// Mutex-guarded for clarity; a lock-free SPSC ring would replace it for
/// production real-time audio, but the interface is the same.
#[derive(Clone, Default)]
pub struct SharedPcm(Arc<Mutex<VecDeque<i16>>>);

impl SharedPcm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append captured/produced samples.
    pub fn push(&self, samples: &[i16]) {
        let mut q = self.0.lock().unwrap();
        q.extend(samples.iter().copied());
    }

    /// Fill `out` from the queue, padding with silence on underrun. Returns the
    /// number of real (non-silence) samples supplied.
    pub fn pop_into(&self, out: &mut [i16]) -> usize {
        let mut q = self.0.lock().unwrap();
        let mut n = 0;
        for slot in out.iter_mut() {
            match q.pop_front() {
                Some(s) => {
                    *slot = s;
                    n += 1;
                }
                None => *slot = 0,
            }
        }
        n
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Audio device errors.
#[derive(Debug)]
pub enum AudioError {
    /// No default output/input device on this host (e.g. headless runner).
    NoDevice,
    /// The audio backend rejected the stream configuration or failed.
    #[cfg(feature = "audio")]
    Backend(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoDevice => f.write_str("no audio device available"),
            #[cfg(feature = "audio")]
            AudioError::Backend(e) => write!(f, "audio backend: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

// ── Real device I/O (cpal) ─────────────────────────────────────────────────

/// Speaker playback: streams i16 PCM from a [`SharedPcm`] to the default
/// output device. Holds the live stream for its lifetime.
#[cfg(feature = "audio")]
pub struct Speaker {
    _stream: cpal::Stream,
}

#[cfg(feature = "audio")]
impl Speaker {
    /// Open the default output device and start playing samples pulled from
    /// `pcm`. Returns [`AudioError::NoDevice`] when the host has no output.
    ///
    /// Open the default output device at its **native** rate and return the
    /// stream plus that sample rate (so the caller resamples the codec's 8 kHz
    /// up to it). Works on any device, including 48-kHz-only hardware. The
    /// queue holds mono samples at the device rate; the callback replicates
    /// each across the device's channels.
    pub fn open(pcm: SharedPcm) -> Result<(Self, u32), AudioError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
        let supported =
            device.default_output_config().map_err(|e| AudioError::Backend(e.to_string()))?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let config = supported.config();

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| {
                    let frames = data.len() / channels.max(1);
                    let mut mono = vec![0i16; frames];
                    pcm.pop_into(&mut mono);
                    for (frame, &s) in data.chunks_mut(channels.max(1)).zip(mono.iter()) {
                        for ch in frame.iter_mut() {
                            *ch = i16_to_f32(s);
                        }
                    }
                },
                |err| eprintln!("lowbandd: speaker stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
        Ok((Self { _stream: stream }, rate))
    }
}

/// Microphone capture: streams i16 PCM from the default input device into a
/// [`SharedPcm`] for the voice encoder to consume.
#[cfg(feature = "audio")]
pub struct Microphone {
    _stream: cpal::Stream,
}

#[cfg(feature = "audio")]
impl Microphone {
    /// Open the default input device at its **native** rate and return the
    /// stream plus that sample rate (so the caller resamples down to the
    /// codec's 8 kHz). Downmixes the device's channels to mono i16.
    pub fn open(pcm: SharedPcm) -> Result<(Self, u32), AudioError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(AudioError::NoDevice)?;
        let supported =
            device.default_input_config().map_err(|e| AudioError::Backend(e.to_string()))?;
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let config = supported.config();

        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _| {
                    let mono: Vec<i16> = data
                        .chunks(channels.max(1))
                        .map(|frame| {
                            let avg = frame.iter().copied().sum::<f32>() / channels.max(1) as f32;
                            f32_to_i16(avg)
                        })
                        .collect();
                    pcm.push(&mono);
                },
                |err| eprintln!("lowbandd: microphone stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
        Ok((Self { _stream: stream }, rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_scales_length_and_preserves_endpoints() {
        // Downsample 48 kHz → 8 kHz: 6:1 length reduction.
        let input: Vec<i16> = (0..48).map(|i| (i * 100) as i16).collect();
        let down = resample(&input, 48_000, 8_000);
        assert_eq!(down.len(), 8);
        assert_eq!(down[0], input[0], "endpoint preserved");

        // Upsample 8 kHz → 48 kHz: 6× length.
        let up = resample(&down, 8_000, 48_000);
        assert_eq!(up.len(), 48);

        // Equal rates / empty are pass-through.
        assert_eq!(resample(&input, 8_000, 8_000), input);
        assert!(resample(&[], 48_000, 8_000).is_empty());
    }

    #[test]
    fn resampler_roundtrip_preserves_a_tone_shape() {
        // A 300 Hz tone at 48 kHz, down to 8 kHz and back, keeps its energy.
        let n = 480;
        let tone: Vec<i16> = (0..n)
            .map(|i| {
                (8000.0 * (2.0 * std::f64::consts::PI * 300.0 * i as f64 / 48000.0).sin()) as i16
            })
            .collect();
        let back = resample(&resample(&tone, 48_000, 8_000), 8_000, 48_000);
        let e_in: f64 = tone.iter().map(|&s| (s as f64).powi(2)).sum();
        let e_out: f64 = back.iter().map(|&s| (s as f64).powi(2)).sum();
        let ratio = e_out / e_in.max(1.0);
        assert!((0.5..2.0).contains(&ratio), "resample energy ratio off: {ratio:.2}");
    }

    #[test]
    fn sample_conversion_roundtrips() {
        for s in [i16::MIN, -1000, 0, 1234, i16::MAX] {
            let back = f32_to_i16(i16_to_f32(s));
            assert!((s as i32 - back as i32).abs() <= 1, "{s} -> {back}");
        }
    }

    #[test]
    fn ring_buffer_hands_off_and_underruns_to_silence() {
        let pcm = SharedPcm::new();
        pcm.push(&[1, 2, 3]);
        assert_eq!(pcm.len(), 3);

        let mut out = [0i16; 5];
        let real = pcm.pop_into(&mut out);
        assert_eq!(real, 3, "3 real samples supplied");
        assert_eq!(out, [1, 2, 3, 0, 0], "underrun padded with silence");
        assert!(pcm.is_empty());
    }

    // With the `audio` feature, opening the speaker exercises the real cpal /
    // ALSA path. On a machine with a speaker it plays; on a headless host the
    // backend reports either no device (`NoDevice`) or an unavailable card
    // (`Backend`, e.g. "cannot find card '0'"). All are a pass — the point is
    // the real cpal/ALSA integration compiles and runs to completion without
    // a spurious panic; actual playback needs audio hardware.
    #[cfg(feature = "audio")]
    #[test]
    fn speaker_open_runs_against_real_backend() {
        let pcm = SharedPcm::new();
        pcm.push(&vec![0i16; 8000]);
        // Any Ok/Err outcome is acceptable; we only require it not to panic.
        // Ok now yields (stream, native_sample_rate).
        let _ = Speaker::open(pcm);
    }
}

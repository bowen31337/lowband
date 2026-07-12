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
    /// Opens a mono 8 kHz stream to match the voice codec — the telephony rate
    /// most audio hardware supports, so no resampling is needed. (A device that
    /// rejects 8 kHz returns [`AudioError::Backend`]; a resampling layer for
    /// those is the follow-up.)
    pub fn open(pcm: SharedPcm) -> Result<Self, AudioError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
        let stream = device
            .build_output_stream(
                voice_config(),
                move |data: &mut [f32], _| {
                    // Mono: one sample per frame. Pull from the playout queue.
                    let mut mono = vec![0i16; data.len()];
                    pcm.pop_into(&mut mono);
                    for (out, &s) in data.iter_mut().zip(mono.iter()) {
                        *out = i16_to_f32(s);
                    }
                },
                |err| eprintln!("lowbandd: speaker stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
        Ok(Self { _stream: stream })
    }
}

/// The voice stream config: mono, 8 kHz — matched to the codec.
#[cfg(feature = "audio")]
pub const VOICE_SAMPLE_RATE: u32 = 8000;

#[cfg(feature = "audio")]
fn voice_config() -> cpal::StreamConfig {
    cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(VOICE_SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
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
    /// Opens a mono 8 kHz input stream matched to the voice codec.
    pub fn open(pcm: SharedPcm) -> Result<Self, AudioError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(AudioError::NoDevice)?;
        let stream = device
            .build_input_stream(
                voice_config(),
                move |data: &[f32], _| {
                    // Mono 8 kHz: one sample per frame → i16 into the queue.
                    let mono: Vec<i16> = data.iter().map(|&s| f32_to_i16(s)).collect();
                    pcm.push(&mono);
                },
                |err| eprintln!("lowbandd: microphone stream error: {err}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
        Ok(Self { _stream: stream })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _ = Speaker::open(pcm);
    }
}

//! Microphone capture broker — Feature 44.
//!
//! Brokers mono 48 kHz i16 PCM acquisition from the default microphone through
//! the correct OS audio API for each platform.
//!
//! # Pipeline position
//!
//! ```text
//! MicCaptureBroker → AEC3 → NoiseSuppressor → AGC → DTX gate → Opus
//! ```
//!
//! The broker produces [`MicFrame`] values at [`MIC_SAMPLE_RATE`] Hz.
//! Downstream stages ([`crate::noise_suppressor`], [`crate::agc`]) consume
//! these frames directly — both modules accept mono 48 kHz i16 PCM and are
//! dimension-agnostic in frame length.
//!
//! # Platform matrix
//!
//! | Platform | Backend              | Notes |
//! |----------|----------------------|-------|
//! | Windows  | WASAPI shared mode   | `IAudioClient` + `IAudioCaptureClient` via raw COM vtables |
//! | macOS    | CoreAudio AudioUnit  | `kAudioOutputUnitProperty_EnableIO` on input bus |
//! | Linux    | PipeWire `pw_stream` | Capture stream negotiated at 48 kHz S16 mono |
//!
//! # Usage
//!
//! ```no_run
//! use lowband_platform::mic_capture::{MicCaptureBroker, MicFrame, MIC_SAMPLE_RATE};
//!
//! let mut broker = MicCaptureBroker::open().expect("open mic");
//! assert_eq!(broker.sample_rate(), MIC_SAMPLE_RATE);
//!
//! loop {
//!     match broker.acquire_frame() {
//!         Ok(frame) => { /* hand frame.samples to AEC3 / NoiseSuppressor */ }
//!         Err(e) => eprintln!("mic: {e}"),
//!     }
//! }
//! ```

// ── Constants ─────────────────────────────────────────────────────────────────

/// Input sample rate for the audio encode pipeline (Hz).
///
/// All downstream stages — AEC3, [`crate::noise_suppressor`],
/// [`crate::agc`], and Opus — operate at this rate.  48 kHz is the Opus
/// native rate for the SILK/CELT hybrid modes used above the Survival tier;
/// no resampling is needed anywhere in the pipeline.
pub const MIC_SAMPLE_RATE: u32 = 48_000;

/// Number of audio channels captured from the microphone.
///
/// The entire encode pipeline is mono.  Stereo microphones are mixed to
/// mono by the OS capture API before samples reach this broker.
pub const MIC_CHANNELS: u16 = 1;

/// Duration of one [`MicFrame`] in milliseconds.
///
/// 10 ms matches [`crate::noise_suppressor::NS_FRAME_MS`], so each captured
/// frame can be handed directly to [`crate::noise_suppressor::NoiseSuppressor`]
/// without buffering or splitting.
pub const MIC_FRAME_MS: u32 = 10;

/// Number of i16 PCM samples per [`MicFrame`].
///
/// `MIC_SAMPLE_RATE / 1000 * MIC_FRAME_MS` = 48 000 / 1000 × 10 = 480.
/// Matches [`crate::noise_suppressor::NS_FRAME_SAMPLES`].
pub const MIC_FRAME_SAMPLES: usize = (MIC_SAMPLE_RATE / 1_000 * MIC_FRAME_MS) as usize;

// ── MicFrame ──────────────────────────────────────────────────────────────────

/// One 10 ms frame of mono 48 kHz i16 PCM from the microphone.
///
/// Contains exactly [`MIC_FRAME_SAMPLES`] (480) samples.  Samples are
/// little-endian signed 16-bit linear PCM in the range [−32 768, 32 767].
#[derive(Debug, Clone)]
pub struct MicFrame {
    /// Raw PCM samples — always [`MIC_FRAME_SAMPLES`] elements.
    pub samples: Vec<i16>,

    /// Monotonically increasing frame counter (starts at 0 when the broker
    /// opens).  Consecutive frames differ by exactly 1; a gap indicates a
    /// capture overrun.
    pub sequence: u64,
}

// ── MicCaptureError ───────────────────────────────────────────────────────────

/// Error returned when opening the microphone or acquiring a frame fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicCaptureError {
    /// The OS denied microphone access (permission not granted).
    PermissionDenied,

    /// No microphone is available or the selected device was disconnected.
    DeviceUnavailable,

    /// A transient buffer underrun; the caller should retry immediately.
    Underrun,

    /// An unexpected OS error; the broker should be closed and reopened.
    OsError(i32),
}

impl std::fmt::Display for MicCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied   => write!(f, "microphone permission denied"),
            Self::DeviceUnavailable  => write!(f, "no microphone device available"),
            Self::Underrun           => write!(f, "capture buffer underrun — retry"),
            Self::OsError(code)      => write!(f, "OS capture error (code {code})"),
        }
    }
}

// ── MicCaptureBroker ─────────────────────────────────────────────────────────

/// Platform microphone capture broker.
///
/// Wraps the correct OS API for the compile target and exposes a uniform
/// [`acquire_frame`](Self::acquire_frame) method.  Construct via
/// [`open`](Self::open).
pub struct MicCaptureBroker {
    inner: platform::Backend,
}

impl MicCaptureBroker {
    /// Open the default system microphone at [`MIC_SAMPLE_RATE`] Hz, mono,
    /// producing [`MIC_FRAME_SAMPLES`]-sample frames.
    ///
    /// Returns `Err(MicCaptureError::PermissionDenied)` when the OS has
    /// revoked or withheld microphone access.  On macOS the permission dialog
    /// is shown lazily by the OS when the stream is started; on Windows and
    /// Linux the error is returned synchronously if the right is not held.
    pub fn open() -> Result<Self, MicCaptureError> {
        Ok(Self { inner: platform::Backend::open()? })
    }

    /// Acquire the next available 10 ms PCM frame from the microphone.
    ///
    /// Blocks until a full [`MIC_FRAME_SAMPLES`]-sample frame is available in
    /// the OS capture buffer, then returns it.  Returns
    /// `Err(MicCaptureError::Underrun)` when the OS buffer was exhausted
    /// (caller should retry) and `Err(MicCaptureError::OsError)` on a
    /// hardware fault.
    pub fn acquire_frame(&mut self) -> Result<MicFrame, MicCaptureError> {
        self.inner.acquire_frame()
    }

    /// Return the sample rate this broker was opened at.
    ///
    /// Always [`MIC_SAMPLE_RATE`] (48 000 Hz).  Exposed so callers can assert
    /// the pipeline invariant without importing the constant directly.
    pub fn sample_rate(&self) -> u32 {
        MIC_SAMPLE_RATE
    }
}

// ── Platform backends ─────────────────────────────────────────────────────────
//
// Each platform module exposes:
//   pub(super) struct Backend
//   impl Backend {
//       pub(super) fn open() -> Result<Backend, MicCaptureError>
//       pub(super) fn acquire_frame(&mut self) -> Result<MicFrame, MicCaptureError>
//   }

// ── Windows — WASAPI shared mode ─────────────────────────────────────────────
//
// WASAPI (Windows Audio Session API) is the lowest-latency capture path on
// Windows that does not require an exclusive-mode driver.  Shared mode
// allows multiple applications to capture simultaneously (AEC3 running in
// the audio engine sees the same device).
//
// COM interfaces used (raw vtable navigation — windows-sys 0.59 does not
// expose AudioClient or CaptureClient):
//
//   MMDeviceEnumerator  → GetDefaultAudioEndpoint → IMMDevice
//   IMMDevice           → Activate(IID_IAudioClient) → IAudioClient
//   IAudioClient        → Initialize + GetService(IID_IAudioCaptureClient)
//                       → IAudioCaptureClient
//   IAudioCaptureClient → GetNextPacketSize + GetBuffer + ReleaseBuffer
//
// Vtable indices (coclass = IMMDeviceEnumerator, IID = {BCDE0395-…}):
//   IUnknown:               [0]=QI  [1]=AddRef  [2]=Release
//   IMMDeviceEnumerator:    [3]=EnumAudioEndpoints  [4]=GetDefaultAudioEndpoint
//                           [5]=GetDevice  [6]=RegisterNotificationClient
//                           [7]=UnregisterNotificationClient
//   IMMDevice:              [3]=Activate  [4]=OpenPropertyStore  [5]=GetId  [6]=GetState
//   IAudioClient:           [3]=Initialize [4]=GetBufferSize [5]=GetStreamLatency
//                           [6]=GetCurrentPadding [7]=IsFormatSupported
//                           [8]=GetMixFormat [9]=GetDevicePeriod [10]=Start
//                           [11]=Stop [12]=Reset [13]=SetEventHandle
//                           [14]=GetService
//   IAudioCaptureClient:    [3]=GetBuffer [4]=ReleaseBuffer [5]=GetNextPacketSize

#[cfg(target_os = "windows")]
mod platform {
    use super::{MicCaptureError, MicFrame, MIC_CHANNELS, MIC_FRAME_SAMPLES, MIC_SAMPLE_RATE};
    use std::ffi::c_void;

    // ── GUIDs ─────────────────────────────────────────────────────────────────

    #[repr(C)]
    struct Guid { d1: u32, d2: u16, d3: u16, d4: [u8; 8] }

    // CLSID_MMDeviceEnumerator = {BCDE0395-E52F-467C-8E3D-C4579291692E}
    const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid {
        d1: 0xBCDE0395, d2: 0xE52F, d3: 0x467C,
        d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    // IID_IMMDeviceEnumerator = {A95664D2-9614-4F35-A746-DE8DB63617E6}
    const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid {
        d1: 0xA95664D2, d2: 0x9614, d3: 0x4F35,
        d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
    };
    // IID_IAudioClient = {1CB9AD4C-DBFA-4C32-B178-C2F568A703B2}
    const IID_IAUDIO_CLIENT: Guid = Guid {
        d1: 0x1CB9AD4C, d2: 0xDBFA, d3: 0x4C32,
        d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
    };
    // IID_IAudioCaptureClient = {C8ADBD64-E71E-48A0-A4DE-185C395CD317}
    const IID_IAUDIO_CAPTURE_CLIENT: Guid = Guid {
        d1: 0xC8ADBD64, d2: 0xE71E, d3: 0x48A0,
        d4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
    };

    // WAVEFORMATEX for 48 kHz / 16-bit / mono PCM
    #[repr(C)]
    struct WaveFormatEx {
        w_format_tag:      u16,  // WAVE_FORMAT_PCM = 1
        n_channels:        u16,
        n_samples_per_sec: u32,
        n_avg_bytes_per_sec: u32,
        n_block_align:     u16,
        w_bits_per_sample: u16,
        cb_size:           u16,
    }

    // CoCreateInstance
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, co_init: u32) -> i32;
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }

    // CLSCTX_ALL = 0x17
    const CLSCTX_ALL: u32 = 0x17;
    // COINIT_MULTITHREADED = 0x0
    const COINIT_MULTITHREADED: u32 = 0x0;
    // eCapture = 1, eConsole = 0 (IMMDeviceEnumerator::GetDefaultAudioEndpoint)
    const E_CAPTURE: u32 = 1;
    const E_CONSOLE: u32 = 0;
    // AUDCLNT_SHAREMODE_SHARED = 0
    const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
    // AUDCLNT_STREAMFLAGS_LOOPBACK not used; plain capture
    // Buffer duration in 100-ns units: 10 ms = 100_000
    const BUFFER_DURATION_100NS: i64 = 100_000;

    unsafe fn com_release(obj: *mut c_void) {
        if obj.is_null() { return; }
        let vtbl = *(obj as *mut *mut usize);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*vtbl.add(2));
        release(obj);
    }

    pub(super) struct Backend {
        audio_client:   *mut c_void,
        capture_client: *mut c_void,
        sequence:       u64,
    }

    impl Backend {
        pub(super) fn open() -> Result<Self, MicCaptureError> {
            unsafe {
                // 1. Initialise COM (idempotent if already initialised on this thread).
                CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED);

                // 2. Create MMDeviceEnumerator.
                let mut enumerator: *mut c_void = std::ptr::null_mut();
                let hr = CoCreateInstance(
                    &CLSID_MM_DEVICE_ENUMERATOR,
                    std::ptr::null_mut(),
                    CLSCTX_ALL,
                    &IID_IMM_DEVICE_ENUMERATOR,
                    &mut enumerator,
                );
                if hr < 0 { return Err(MicCaptureError::OsError(hr)); }

                // 3. Get default capture device.
                let mut device: *mut c_void = std::ptr::null_mut();
                let vtbl = *(enumerator as *mut *mut usize);
                let get_default: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32 =
                    std::mem::transmute(*vtbl.add(4));
                let hr = get_default(enumerator, E_CAPTURE, E_CONSOLE, &mut device);
                com_release(enumerator);
                if hr < 0 {
                    return Err(if hr == 0x80070005u32 as i32 {
                        MicCaptureError::PermissionDenied
                    } else {
                        MicCaptureError::DeviceUnavailable
                    });
                }

                // 4. Activate IAudioClient.
                let mut audio_client: *mut c_void = std::ptr::null_mut();
                let vtbl = *(device as *mut *mut usize);
                let activate: unsafe extern "system" fn(
                    *mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void,
                ) -> i32 = std::mem::transmute(*vtbl.add(3));
                let hr = activate(
                    device,
                    &IID_IAUDIO_CLIENT,
                    CLSCTX_ALL,
                    std::ptr::null_mut(),
                    &mut audio_client,
                );
                com_release(device);
                if hr < 0 { return Err(MicCaptureError::OsError(hr)); }

                // 5. Initialize the stream: shared mode, 48 kHz, mono, 16-bit PCM.
                let fmt = WaveFormatEx {
                    w_format_tag:        1, // WAVE_FORMAT_PCM
                    n_channels:          MIC_CHANNELS,
                    n_samples_per_sec:   MIC_SAMPLE_RATE,
                    n_avg_bytes_per_sec: MIC_SAMPLE_RATE * 2,
                    n_block_align:       2,
                    w_bits_per_sample:   16,
                    cb_size:             0,
                };
                let vtbl = *(audio_client as *mut *mut usize);
                let initialize: unsafe extern "system" fn(
                    *mut c_void, u32, u32, i64, i64, *const WaveFormatEx, *const Guid,
                ) -> i32 = std::mem::transmute(*vtbl.add(3));
                let hr = initialize(
                    audio_client,
                    AUDCLNT_SHAREMODE_SHARED,
                    0,
                    BUFFER_DURATION_100NS,
                    0,
                    &fmt,
                    std::ptr::null(),
                );
                if hr < 0 {
                    com_release(audio_client);
                    return Err(MicCaptureError::OsError(hr));
                }

                // 6. Get IAudioCaptureClient.
                let mut capture_client: *mut c_void = std::ptr::null_mut();
                let get_service: unsafe extern "system" fn(
                    *mut c_void, *const Guid, *mut *mut c_void,
                ) -> i32 = std::mem::transmute(*vtbl.add(14));
                let hr = get_service(audio_client, &IID_IAUDIO_CAPTURE_CLIENT, &mut capture_client);
                if hr < 0 {
                    com_release(audio_client);
                    return Err(MicCaptureError::OsError(hr));
                }

                // 7. Start the stream.
                let start: unsafe extern "system" fn(*mut c_void) -> i32 =
                    std::mem::transmute(*vtbl.add(10));
                let hr = start(audio_client);
                if hr < 0 {
                    com_release(capture_client);
                    com_release(audio_client);
                    return Err(MicCaptureError::OsError(hr));
                }

                Ok(Self { audio_client, capture_client, sequence: 0 })
            }
        }

        pub(super) fn acquire_frame(&mut self) -> Result<MicFrame, MicCaptureError> {
            unsafe {
                // IAudioCaptureClient vtable:
                //   [3]=GetBuffer [4]=ReleaseBuffer [5]=GetNextPacketSize
                let vtbl = *(self.capture_client as *mut *mut usize);
                let get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 =
                    std::mem::transmute(*vtbl.add(5));
                let get_buffer: unsafe extern "system" fn(
                    *mut c_void, *mut *mut u8, *mut u32, *mut u32, *mut u64, *mut u64,
                ) -> i32 = std::mem::transmute(*vtbl.add(3));
                let release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> i32 =
                    std::mem::transmute(*vtbl.add(4));

                // Accumulate MIC_FRAME_SAMPLES samples across one or more OS packets.
                let mut out = Vec::with_capacity(MIC_FRAME_SAMPLES);

                while out.len() < MIC_FRAME_SAMPLES {
                    let mut packet_frames: u32 = 0;
                    let hr = get_next_packet_size(self.capture_client, &mut packet_frames);
                    if hr < 0 { return Err(MicCaptureError::OsError(hr)); }
                    if packet_frames == 0 {
                        return Err(MicCaptureError::Underrun);
                    }

                    let mut buf: *mut u8 = std::ptr::null_mut();
                    let mut frames_available: u32 = 0;
                    let mut flags: u32 = 0;
                    let hr = get_buffer(
                        self.capture_client,
                        &mut buf,
                        &mut frames_available,
                        &mut flags,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    if hr < 0 { return Err(MicCaptureError::OsError(hr)); }

                    let need = (MIC_FRAME_SAMPLES - out.len()).min(frames_available as usize);
                    // AUDCLNT_BUFFERFLAGS_SILENT = 0x2: fill with zeros instead of reading.
                    if flags & 0x2 != 0 {
                        out.extend(std::iter::repeat(0i16).take(need));
                    } else {
                        let samples = std::slice::from_raw_parts(buf as *const i16, frames_available as usize);
                        out.extend_from_slice(&samples[..need]);
                    }

                    release_buffer(self.capture_client, frames_available);
                }

                let seq = self.sequence;
                self.sequence += 1;
                Ok(MicFrame { samples: out, sequence: seq })
            }
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                if !self.audio_client.is_null() {
                    let vtbl = *(self.audio_client as *mut *mut usize);
                    let stop: unsafe extern "system" fn(*mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(11));
                    stop(self.audio_client);
                }
                com_release(self.capture_client);
                com_release(self.audio_client);
            }
        }
    }
}

// ── macOS — CoreAudio AudioUnit ───────────────────────────────────────────────
//
// The AUHAL (Audio Unit Hardware Abstraction Layer) component provides
// direct access to the default input device with minimal latency.  We
// request kAudioOutputUnitProperty_EnableIO on bus 1 (input) and disable
// bus 0 (output) to get a capture-only unit.
//
// CoreAudio uses a push model: the OS calls our render callback when a
// buffer is ready.  We forward samples to a ring buffer and drain it in
// `acquire_frame`.
//
// AudioComponent / AudioUnit symbols are in AudioToolbox.framework, which
// is available on all macOS versions supported by LowBand (12+).

#[cfg(target_os = "macos")]
mod platform {
    use super::{MicCaptureError, MicFrame, MIC_CHANNELS, MIC_FRAME_SAMPLES, MIC_SAMPLE_RATE};
    use std::sync::{Arc, Mutex};

    // AudioComponentDescription for kAudioUnitType_Output / kAudioUnitSubType_HALOutput
    #[repr(C)]
    struct AudioComponentDescription {
        component_type:         u32, // kAudioUnitType_Output = 'auou'
        component_sub_type:     u32, // kAudioUnitSubType_HALOutput = 'ahal'
        component_manufacturer: u32, // kAudioUnitManufacturer_Apple = 'appl'
        component_flags:        u32,
        component_flags_mask:   u32,
    }

    // AudioStreamBasicDescription (ASBD)
    #[repr(C)]
    struct AudioStreamBasicDescription {
        sample_rate:        f64,
        format_id:          u32, // kAudioFormatLinearPCM = 'lpcm'
        format_flags:       u32, // kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked = 0xC
        bytes_per_packet:   u32,
        frames_per_packet:  u32,
        bytes_per_frame:    u32,
        channels_per_frame: u32,
        bits_per_channel:   u32,
        reserved:           u32,
    }

    // AudioBufferList with a single buffer
    #[repr(C)]
    struct AudioBuffer {
        number_channels: u32,
        data_byte_size:  u32,
        data:            *mut std::ffi::c_void,
    }
    #[repr(C)]
    struct AudioBufferList {
        number_buffers: u32,
        buffers:        [AudioBuffer; 1],
    }

    #[repr(C)]
    struct AudioTimeStamp {
        sample_time: f64,
        host_time:   u64,
        rate_scalar: f64,
        word_clock_time: u64,
        smpte:       [u8; 24],
        flags:       u32,
        reserved:    u32,
    }

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioComponentFindNext(
            in_component: *mut std::ffi::c_void,
            in_desc: *const AudioComponentDescription,
        ) -> *mut std::ffi::c_void;
        fn AudioComponentInstanceNew(
            in_component: *mut std::ffi::c_void,
            out_instance: *mut *mut std::ffi::c_void,
        ) -> i32;
        fn AudioUnitSetProperty(
            in_unit: *mut std::ffi::c_void,
            in_id: u32,
            in_scope: u32,
            in_element: u32,
            in_data: *const std::ffi::c_void,
            in_data_size: u32,
        ) -> i32;
        fn AudioUnitInitialize(in_unit: *mut std::ffi::c_void) -> i32;
        fn AudioOutputUnitStart(in_unit: *mut std::ffi::c_void) -> i32;
        fn AudioOutputUnitStop(in_unit: *mut std::ffi::c_void) -> i32;
        fn AudioComponentInstanceDispose(in_instance: *mut std::ffi::c_void) -> i32;
    }

    // AudioUnit property selectors
    const K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO: u32 = 2011;
    const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
    const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
    // Scopes
    const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
    const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 0;
    // Element 1 = input bus
    const INPUT_BUS: u32 = 1;
    const OUTPUT_BUS: u32 = 0;
    // kAudioFormatLinearPCM
    const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6C70636D; // 'lpcm'
    // kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked
    const K_PCM_FLAGS: u32 = 0x0C;

    // AURenderCallbackStruct
    #[repr(C)]
    struct AURenderCallbackStruct {
        input_proc:        Option<unsafe extern "C" fn(
            ref_con:           *mut std::ffi::c_void,
            action_flags:      *mut u32,
            time_stamp:        *const AudioTimeStamp,
            bus_number:        u32,
            number_frames:     u32,
            io_data:           *mut AudioBufferList,
        ) -> i32>,
        input_proc_ref_con: *mut std::ffi::c_void,
    }

    // Ring buffer shared between the CoreAudio callback and acquire_frame.
    struct RingBuf {
        buf: Vec<i16>,
        head: usize,
        tail: usize,
        cap: usize,
    }
    impl RingBuf {
        fn new(capacity: usize) -> Self {
            Self { buf: vec![0i16; capacity], head: 0, tail: 0, cap: capacity }
        }
        fn push_slice(&mut self, data: &[i16]) {
            for &s in data {
                self.buf[self.tail] = s;
                self.tail = (self.tail + 1) % self.cap;
            }
        }
        fn available(&self) -> usize {
            (self.tail + self.cap - self.head) % self.cap
        }
        fn drain_into(&mut self, dst: &mut Vec<i16>, n: usize) {
            for _ in 0..n {
                dst.push(self.buf[self.head]);
                self.head = (self.head + 1) % self.cap;
            }
        }
    }

    unsafe extern "C" fn capture_callback(
        ref_con:       *mut std::ffi::c_void,
        _action_flags: *mut u32,
        _time_stamp:   *const AudioTimeStamp,
        _bus_number:   u32,
        number_frames: u32,
        io_data:       *mut AudioBufferList,
    ) -> i32 {
        let ring = &*(ref_con as *const Mutex<RingBuf>);
        let buf_ptr = (*io_data).buffers[0].data as *const i16;
        let samples = std::slice::from_raw_parts(buf_ptr, number_frames as usize);
        if let Ok(mut r) = ring.lock() {
            r.push_slice(samples);
        }
        0
    }

    pub(super) struct Backend {
        unit:     *mut std::ffi::c_void,
        ring:     Arc<Mutex<RingBuf>>,
        sequence: u64,
    }

    // Safety: the AudioUnit handle is not Send by default, but we access it
    // only from the thread that created it.  The ring buffer uses a Mutex.
    unsafe impl Send for Backend {}

    impl Backend {
        pub(super) fn open() -> Result<Self, MicCaptureError> {
            unsafe {
                let desc = AudioComponentDescription {
                    component_type:         0x61756F75, // 'auou'
                    component_sub_type:     0x6168616C, // 'ahal'
                    component_manufacturer: 0x6170706C, // 'appl'
                    component_flags:        0,
                    component_flags_mask:   0,
                };
                let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
                if comp.is_null() { return Err(MicCaptureError::DeviceUnavailable); }

                let mut unit: *mut std::ffi::c_void = std::ptr::null_mut();
                if AudioComponentInstanceNew(comp, &mut unit) != 0 {
                    return Err(MicCaptureError::DeviceUnavailable);
                }

                // Enable input (bus 1), disable output (bus 0).
                let one: u32 = 1;
                let zero: u32 = 0;
                AudioUnitSetProperty(
                    unit, K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
                    K_AUDIO_UNIT_SCOPE_INPUT, INPUT_BUS,
                    &one as *const u32 as *const _, 4,
                );
                AudioUnitSetProperty(
                    unit, K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
                    K_AUDIO_UNIT_SCOPE_OUTPUT, OUTPUT_BUS,
                    &zero as *const u32 as *const _, 4,
                );

                // Set stream format: 48 kHz / mono / S16.
                let asbd = AudioStreamBasicDescription {
                    sample_rate:        MIC_SAMPLE_RATE as f64,
                    format_id:          K_AUDIO_FORMAT_LINEAR_PCM,
                    format_flags:       K_PCM_FLAGS,
                    bytes_per_packet:   2,
                    frames_per_packet:  1,
                    bytes_per_frame:    2,
                    channels_per_frame: MIC_CHANNELS as u32,
                    bits_per_channel:   16,
                    reserved:           0,
                };
                let asbd_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
                if AudioUnitSetProperty(
                    unit, K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_OUTPUT, INPUT_BUS,
                    &asbd as *const _ as *const _, asbd_size,
                ) != 0 {
                    AudioComponentInstanceDispose(unit);
                    return Err(MicCaptureError::OsError(-1));
                }

                // Register render callback.
                let ring = Arc::new(Mutex::new(RingBuf::new(MIC_FRAME_SAMPLES * 16)));
                let ring_ptr: *mut std::ffi::c_void =
                    Arc::as_ptr(&ring) as *mut Mutex<RingBuf> as *mut std::ffi::c_void;
                let cb = AURenderCallbackStruct {
                    input_proc:        Some(capture_callback),
                    input_proc_ref_con: ring_ptr,
                };
                if AudioUnitSetProperty(
                    unit, K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                    K_AUDIO_UNIT_SCOPE_OUTPUT, INPUT_BUS,
                    &cb as *const _ as *const _,
                    std::mem::size_of::<AURenderCallbackStruct>() as u32,
                ) != 0 {
                    AudioComponentInstanceDispose(unit);
                    return Err(MicCaptureError::OsError(-2));
                }

                if AudioUnitInitialize(unit) != 0 || AudioOutputUnitStart(unit) != 0 {
                    AudioComponentInstanceDispose(unit);
                    return Err(MicCaptureError::OsError(-3));
                }

                Ok(Self { unit, ring, sequence: 0 })
            }
        }

        pub(super) fn acquire_frame(&mut self) -> Result<MicFrame, MicCaptureError> {
            let mut ring = self.ring.lock().map_err(|_| MicCaptureError::OsError(-10))?;
            if ring.available() < MIC_FRAME_SAMPLES {
                return Err(MicCaptureError::Underrun);
            }
            let mut samples = Vec::with_capacity(MIC_FRAME_SAMPLES);
            ring.drain_into(&mut samples, MIC_FRAME_SAMPLES);
            let seq = self.sequence;
            self.sequence += 1;
            Ok(MicFrame { samples, sequence: seq })
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                AudioOutputUnitStop(self.unit);
                AudioComponentInstanceDispose(self.unit);
            }
        }
    }
}

// ── Linux — PipeWire pw_stream (dlopen) ──────────────────────────────────────
//
// PipeWire replaces PulseAudio and JACK on modern Linux distributions.
// `pw_stream` is the recommended consumer API: we create a capture stream,
// negotiate 48 kHz / S16 / mono, and buffer incoming frames in a ring buffer
// drained by `acquire_frame`.
//
// The project builds against musl (fully static), but libpipewire ships
// only as a shared library.  We dlopen "libpipewire-0.3.so.0" at runtime
// so the binary links cleanly on every distro and degrades gracefully when
// PipeWire is absent (returns `DeviceUnavailable`).

#[cfg(target_os = "linux")]
mod platform {
    use super::{MicCaptureError, MicFrame, MIC_FRAME_SAMPLES};
    use std::ffi::c_void;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    // ── dlopen / dlsym ────────────────────────────────────────────────────────

    extern "C" {
        fn dlopen(filename: *const u8, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const u8) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }

    const RTLD_LAZY:  i32 = 1;
    const RTLD_LOCAL: i32 = 0;

    // ── libpipewire-0.3 function pointer types ────────────────────────────────

    type FnPwInit            = unsafe extern "C" fn(*mut i32, *mut *mut *mut u8);
    type FnPwMainLoopNew     = unsafe extern "C" fn(*const c_void) -> *mut c_void;
    type FnPwMainLoopGetLoop = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnPwMainLoopRun     = unsafe extern "C" fn(*mut c_void);
    type FnPwMainLoopQuit    = unsafe extern "C" fn(*mut c_void);
    type FnPwMainLoopDestroy = unsafe extern "C" fn(*mut c_void);
    type FnPwStreamNewSimple = unsafe extern "C" fn(
        *mut c_void, *const u8, *mut c_void, *const PwStreamEvents, *mut c_void,
    ) -> *mut c_void;
    type FnPwStreamConnect   = unsafe extern "C" fn(
        *mut c_void, i32, u32, u32, *const *mut c_void, u32,
    ) -> i32;
    type FnPwStreamDequeue   = unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer;
    type FnPwStreamQueue     = unsafe extern "C" fn(*mut c_void, *mut PwBuffer);
    type FnPwStreamDestroy   = unsafe extern "C" fn(*mut c_void);
    type FnPwPropertiesNew   = unsafe extern "C" fn(*const u8, *const u8, *const u8) -> *mut c_void;

    struct LibPw {
        _handle:               *mut c_void,
        pw_init:               FnPwInit,
        pw_main_loop_new:      FnPwMainLoopNew,
        pw_main_loop_get_loop: FnPwMainLoopGetLoop,
        pw_main_loop_run:      FnPwMainLoopRun,
        pw_main_loop_quit:     FnPwMainLoopQuit,
        pw_main_loop_destroy:  FnPwMainLoopDestroy,
        pw_stream_new_simple:  FnPwStreamNewSimple,
        pw_stream_connect:     FnPwStreamConnect,
        pw_stream_dequeue_buffer: FnPwStreamDequeue,
        pw_stream_queue_buffer:   FnPwStreamQueue,
        pw_stream_destroy:     FnPwStreamDestroy,
        pw_properties_new:     FnPwPropertiesNew,
    }
    unsafe impl Send for LibPw {}
    unsafe impl Sync for LibPw {}

    impl LibPw {
        fn load() -> Option<Self> {
            let handle = unsafe {
                dlopen(b"libpipewire-0.3.so.0\0".as_ptr(), RTLD_LAZY | RTLD_LOCAL)
            };
            if handle.is_null() { return None; }

            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let p = unsafe { dlsym(handle, concat!($name, "\0").as_bytes().as_ptr()) };
                    if p.is_null() { unsafe { dlclose(handle) }; return None; }
                    unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
                }};
            }

            Some(LibPw {
                _handle:                  handle,
                pw_init:                  sym!("pw_init",                    FnPwInit),
                pw_main_loop_new:         sym!("pw_main_loop_new",           FnPwMainLoopNew),
                pw_main_loop_get_loop:    sym!("pw_main_loop_get_loop",      FnPwMainLoopGetLoop),
                pw_main_loop_run:         sym!("pw_main_loop_run",           FnPwMainLoopRun),
                pw_main_loop_quit:        sym!("pw_main_loop_quit",          FnPwMainLoopQuit),
                pw_main_loop_destroy:     sym!("pw_main_loop_destroy",       FnPwMainLoopDestroy),
                pw_stream_new_simple:     sym!("pw_stream_new_simple",       FnPwStreamNewSimple),
                pw_stream_connect:        sym!("pw_stream_connect",          FnPwStreamConnect),
                pw_stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer",   FnPwStreamDequeue),
                pw_stream_queue_buffer:   sym!("pw_stream_queue_buffer",     FnPwStreamQueue),
                pw_stream_destroy:        sym!("pw_stream_destroy",          FnPwStreamDestroy),
                pw_properties_new:        sym!("pw_properties_new",          FnPwPropertiesNew),
            })
        }
    }

    // ── PipeWire ABI structs ──────────────────────────────────────────────────

    #[repr(C)]
    struct PwBuffer {
        buffer:    *mut SpaBuffer,
        user_data: *mut c_void,
        size:      u64,
        requested: u64,
    }
    #[repr(C)]
    struct SpaBuffer {
        n_metas: u32,
        n_datas: u32,
        metas:   *mut c_void,
        datas:   *mut SpaData,
    }
    #[repr(C)]
    struct SpaData {
        type_:     u32,
        flags:     u32,
        fd:        i64,
        mapoffset: i32,
        maxsize:   u32,
        data:      *mut c_void,
        chunk:     *mut SpaChunk,
    }
    #[repr(C)]
    struct SpaChunk {
        offset: u32,
        size:   u32,
        stride: i32,
        flags:  i32,
    }

    #[repr(C)]
    struct PwStreamEvents {
        version:       u32,
        destroy:       Option<unsafe extern "C" fn(*mut c_void)>,
        state_changed: Option<unsafe extern "C" fn(*mut c_void, u32, u32, *const u8)>,
        control_info:  Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
        io_changed:    Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
        param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
        add_buffer:    Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
        remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
        process:       Option<unsafe extern "C" fn(*mut c_void)>,
        drained:       Option<unsafe extern "C" fn(*mut c_void)>,
    }

    struct SharedState {
        ring: Mutex<(Vec<i16>, usize, usize)>, // (buf, head, tail)
        cond: Condvar,
        lib:  Arc<LibPw>,
    }

    struct StreamCtx {
        stream: *mut c_void,
        shared: Arc<SharedState>,
    }
    unsafe impl Send for StreamCtx {}

    unsafe extern "C" fn on_process(data: *mut c_void) {
        let ctx = &*(data as *const StreamCtx);
        let buf = (ctx.shared.lib.pw_stream_dequeue_buffer)(ctx.stream);
        if buf.is_null() { return; }
        let spa = (*buf).buffer;
        if spa.is_null() || (*spa).n_datas == 0 {
            (ctx.shared.lib.pw_stream_queue_buffer)(ctx.stream, buf);
            return;
        }
        let d = &*(*spa).datas;
        if d.data.is_null() || (*d.chunk).size == 0 {
            (ctx.shared.lib.pw_stream_queue_buffer)(ctx.stream, buf);
            return;
        }
        let n = (*d.chunk).size as usize / 2;
        let samples = std::slice::from_raw_parts(d.data as *const i16, n);
        if let Ok(mut guard) = ctx.shared.ring.lock() {
            let (ring_buf, _head, tail) = &mut *guard;
            let cap = ring_buf.len();
            for &s in samples {
                ring_buf[*tail] = s;
                *tail = (*tail + 1) % cap;
            }
        }
        ctx.shared.cond.notify_one();
        (ctx.shared.lib.pw_stream_queue_buffer)(ctx.stream, buf);
    }

    pub(super) struct Backend {
        main_loop: *mut c_void,
        shared:    Arc<SharedState>,
        _thread:   thread::JoinHandle<()>,
        sequence:  u64,
    }
    unsafe impl Send for Backend {}

    impl Backend {
        pub(super) fn open() -> Result<Self, MicCaptureError> {
            let lib = Arc::new(LibPw::load().ok_or(MicCaptureError::DeviceUnavailable)?);

            unsafe {
                (lib.pw_init)(std::ptr::null_mut(), std::ptr::null_mut());
                let main_loop = (lib.pw_main_loop_new)(std::ptr::null());
                if main_loop.is_null() { return Err(MicCaptureError::DeviceUnavailable); }
                let loop_ = (lib.pw_main_loop_get_loop)(main_loop);

                let shared = Arc::new(SharedState {
                    ring: Mutex::new((vec![0i16; MIC_FRAME_SAMPLES * 16], 0, 0)),
                    cond: Condvar::new(),
                    lib:  Arc::clone(&lib),
                });

                let props = (lib.pw_properties_new)(
                    b"media.role\0".as_ptr(),
                    b"Communication\0".as_ptr(),
                    std::ptr::null(),
                );

                let events = PwStreamEvents {
                    version:       2,
                    destroy:       None, state_changed: None, control_info: None,
                    io_changed:    None, param_changed: None,
                    add_buffer:    None, remove_buffer: None,
                    process:       Some(on_process),
                    drained:       None,
                };

                let ctx = Box::into_raw(Box::new(StreamCtx {
                    stream: std::ptr::null_mut(),
                    shared: Arc::clone(&shared),
                }));

                let stream = (lib.pw_stream_new_simple)(
                    loop_,
                    b"lowband-mic\0".as_ptr(),
                    props,
                    &events,
                    ctx as *mut c_void,
                );
                if stream.is_null() {
                    drop(Box::from_raw(ctx));
                    (lib.pw_main_loop_destroy)(main_loop);
                    return Err(MicCaptureError::DeviceUnavailable);
                }
                (*ctx).stream = stream;

                let hr = (lib.pw_stream_connect)(
                    stream,
                    1,        // PW_DIRECTION_INPUT
                    u32::MAX, // PW_ID_ANY
                    0x3,      // AUTOCONNECT | MAP_BUFFERS
                    std::ptr::null(),
                    0,
                );
                if hr < 0 {
                    (lib.pw_stream_destroy)(stream);
                    drop(Box::from_raw(ctx));
                    (lib.pw_main_loop_destroy)(main_loop);
                    return Err(MicCaptureError::OsError(hr));
                }

                let ml_ptr = main_loop as usize;
                let lib_clone = Arc::clone(&lib);
                let thread = thread::spawn(move || {
                    (lib_clone.pw_main_loop_run)(ml_ptr as *mut c_void);
                });

                Ok(Self { main_loop, shared, _thread: thread, sequence: 0 })
            }
        }

        pub(super) fn acquire_frame(&mut self) -> Result<MicFrame, MicCaptureError> {
            let mut guard = self.shared.ring.lock().map_err(|_| MicCaptureError::OsError(-1))?;
            loop {
                let (buf, head, tail) = &*guard;
                let available = (*tail + buf.len() - *head) % buf.len();
                if available >= MIC_FRAME_SAMPLES { break; }
                guard = self.shared.cond.wait(guard).map_err(|_| MicCaptureError::OsError(-2))?;
            }
            let (buf, head, _tail) = &mut *guard;
            let cap = buf.len();
            let mut samples = Vec::with_capacity(MIC_FRAME_SAMPLES);
            for _ in 0..MIC_FRAME_SAMPLES {
                samples.push(buf[*head]);
                *head = (*head + 1) % cap;
            }
            drop(guard);
            let seq = self.sequence;
            self.sequence += 1;
            Ok(MicFrame { samples, sequence: seq })
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe { (self.shared.lib.pw_main_loop_quit)(self.main_loop); }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn sample_rate_is_48khz() {
        assert_eq!(MIC_SAMPLE_RATE, 48_000,
            "Feature 44 requires 48 kHz; NS and Opus both expect this rate");
    }

    #[test]
    fn frame_samples_matches_sample_rate_and_duration() {
        let expected = (MIC_SAMPLE_RATE / 1_000 * MIC_FRAME_MS) as usize;
        assert_eq!(MIC_FRAME_SAMPLES, expected,
            "MIC_FRAME_SAMPLES must equal sample_rate/1000 × frame_ms");
    }

    #[test]
    fn frame_samples_matches_ns_frame_samples() {
        // MicCaptureBroker produces frames that feed NoiseSuppressor directly.
        // Both must agree on the frame length.
        assert_eq!(MIC_FRAME_SAMPLES, crate::noise_suppressor::NS_FRAME_SAMPLES,
            "mic and noise-suppressor frame sizes must agree for zero-copy handoff");
    }

    #[test]
    fn sample_rate_matches_ns_sample_rate() {
        assert_eq!(MIC_SAMPLE_RATE, crate::noise_suppressor::NS_SAMPLE_RATE,
            "mic and noise-suppressor sample rates must match");
    }

    #[test]
    fn mic_frame_ms_is_10() {
        assert_eq!(MIC_FRAME_MS, 10,
            "10 ms frames align with the NoiseSuppressor frame boundary");
    }

    #[test]
    fn mic_channels_is_mono() {
        assert_eq!(MIC_CHANNELS, 1, "encode pipeline is mono");
    }

    // ── MicFrame ──────────────────────────────────────────────────────────────

    #[test]
    fn mic_frame_samples_len_equals_constant() {
        let frame = MicFrame { samples: vec![0i16; MIC_FRAME_SAMPLES], sequence: 0 };
        assert_eq!(frame.samples.len(), MIC_FRAME_SAMPLES);
    }

    #[test]
    fn mic_frame_sequence_is_accessible() {
        let frame = MicFrame { samples: vec![0i16; MIC_FRAME_SAMPLES], sequence: 7 };
        assert_eq!(frame.sequence, 7);
    }

    // ── MicCaptureError ───────────────────────────────────────────────────────

    #[test]
    fn error_display_permission_denied() {
        let e = MicCaptureError::PermissionDenied;
        assert!(e.to_string().contains("permission"), "display must mention permission");
    }

    #[test]
    fn error_display_device_unavailable() {
        let e = MicCaptureError::DeviceUnavailable;
        assert!(e.to_string().contains("device") || e.to_string().contains("microphone"),
            "display must reference the device");
    }

    #[test]
    fn error_display_underrun() {
        let e = MicCaptureError::Underrun;
        assert!(e.to_string().to_lowercase().contains("underrun") ||
                e.to_string().contains("retry"),
            "display must mention underrun or retry");
    }

    #[test]
    fn error_display_os_error_includes_code() {
        let e = MicCaptureError::OsError(-42);
        assert!(e.to_string().contains("-42"), "display must include the OS error code");
    }

    #[test]
    fn error_variants_are_eq() {
        assert_eq!(MicCaptureError::PermissionDenied, MicCaptureError::PermissionDenied);
        assert_eq!(MicCaptureError::DeviceUnavailable, MicCaptureError::DeviceUnavailable);
        assert_eq!(MicCaptureError::Underrun, MicCaptureError::Underrun);
        assert_eq!(MicCaptureError::OsError(0), MicCaptureError::OsError(0));
        assert_ne!(MicCaptureError::OsError(1), MicCaptureError::OsError(2));
    }
}

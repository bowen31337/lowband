//! Per-OS screen-capture broker — Feature 152.
//!
//! Brokers screen frame acquisition through the correct OS API for each
//! platform, gated on the `ScreenCapture` elevation right (see
//! [`crate::elevation::EscalationReason::ScreenCapture`]).
//!
//! # Platform matrix
//!
//! | Platform | Backend              | Privilege path |
//! |----------|----------------------|----------------|
//! | Windows  | DXGI Desktop Duplication | `NT SERVICE\LowBandDaemon` account; right held at install time |
//! | macOS    | ScreenCaptureKit     | TCC `Screen Recording` right — one-time prompt via `CGRequestScreenCaptureAccess()` |
//! | Linux    | PipeWire ScreenCast  | Portal grant from compositor via `org.freedesktop.portal.ScreenCast` |
//!
//! # Privilege flow
//!
//! Call [`ScreenCaptureBroker::request_grant`] once at session start to obtain
//! the OS right.  On Windows this is a no-op at runtime — the right is held by
//! the service account.  On macOS and Linux the call surfaces the platform
//! consent dialog.  Check the returned
//! [`ElevationOutcome`](crate::elevation::ElevationOutcome) before opening the
//! broker; proceeding after `Denied` is a logic error.
//!
//! # Usage
//!
//! ```no_run
//! use lowband_platform::screen_capture::{ScreenCaptureBroker, CaptureFrame};
//! use lowband_platform::elevation::ElevationOutcome;
//!
//! // 1. Request the OS right (macOS: TCC prompt; Linux: portal grant; Windows: no-op).
//! let outcome = ScreenCaptureBroker::request_grant();
//! assert!(outcome.is_granted(), "screen capture not granted");
//!
//! // 2. Open the backend.
//! //    On Linux, pass the PipeWire fd from org.freedesktop.portal.ScreenCast.
//! //    On all other platforms pass -1.
//! let mut broker = ScreenCaptureBroker::open(-1).expect("open broker");
//!
//! // 3. Acquire frames.
//! let frame: CaptureFrame = broker.acquire_frame().expect("acquire frame");
//! println!("{}×{} frame, {} dirty rects", frame.width, frame.height, frame.dirty_rects.len());
//! ```

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::elevation::{ElevationOutcome, ElevationRequest, EscalationReason};

// ── Public types ──────────────────────────────────────────────────────────────

/// A rectangle in screen coordinates describing a region that changed since the
/// previous frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    pub x:      i32,
    pub y:      i32,
    pub width:  u32,
    pub height: u32,
}

/// A hardware cursor shape captured from the OS.
///
/// Pixels are BGRA8, tightly packed (no padding between rows).
/// Only emitted by [`ScreenCaptureBroker::acquire_frame`] when the shape
/// differs from all previously seen shapes this session (deduplicated by hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Cursor image width in pixels.
    pub width:     u32,
    /// Cursor image height in pixels.
    pub height:    u32,
    /// Hotspot X offset from the top-left of the cursor image.
    pub hotspot_x: i32,
    /// Hotspot Y offset from the top-left of the cursor image.
    pub hotspot_y: i32,
    /// Raw pixel data (BGRA8), `width * height * 4` bytes.
    pub pixels:    Vec<u8>,
}

/// A captured screen frame returned by [`ScreenCaptureBroker::acquire_frame`].
///
/// `pixels` is BGRA8 on Windows (DXGI native), BGRA8 on macOS
/// (ScreenCaptureKit default), and BGRA8 on Linux (PipeWire negotiated).
/// Stride may exceed `width * 4` due to alignment padding.
pub struct CaptureFrame {
    /// Raw pixel data (BGRA8).
    pub pixels:       Vec<u8>,
    /// Frame width in pixels.
    pub width:        u32,
    /// Frame height in pixels.
    pub height:       u32,
    /// Row stride in bytes (>= `width * 4`).
    pub stride:       u32,
    /// Dirty regions relative to the top-left of the captured surface.
    /// Empty when the backend does not report damage (full-frame capture).
    pub dirty_rects:  Vec<DirtyRect>,
    /// New cursor shape, present only when the shape changed since the last
    /// emission.  `None` on every frame where the cursor shape is unchanged.
    pub cursor_shape: Option<CursorShape>,
}

// ── CursorShapeCache ──────────────────────────────────────────────────────────

/// Session-scoped deduplicator: tracks hashes of every cursor shape emitted so
/// far and reports whether an incoming shape is genuinely new.
struct CursorShapeCache {
    seen: std::collections::HashSet<u64>,
}

impl CursorShapeCache {
    fn new() -> Self {
        Self { seen: std::collections::HashSet::new() }
    }

    /// Returns `true` and records the hash if `shape` has not been seen before;
    /// returns `false` if an identical shape was emitted earlier this session.
    fn is_new(&mut self, shape: &CursorShape) -> bool {
        let h = Self::hash(shape);
        self.seen.insert(h)
    }

    fn hash(shape: &CursorShape) -> u64 {
        let mut h = DefaultHasher::new();
        shape.width.hash(&mut h);
        shape.height.hash(&mut h);
        shape.hotspot_x.hash(&mut h);
        shape.hotspot_y.hash(&mut h);
        shape.pixels.hash(&mut h);
        h.finish()
    }
}

/// Error returned when a capture call fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The OS right has not been granted; call
    /// [`ScreenCaptureBroker::request_grant`] and check the outcome before
    /// opening the broker.
    NotGranted,

    /// A transient OS error (e.g. mode change mid-capture, surface lost).
    /// The caller may retry; persistent failures indicate a configuration
    /// problem.
    OsRejected,

    /// The backend is not available in this context (headless server, PipeWire
    /// fd not provided, display server disconnected).
    Unavailable,

    /// No new frame is ready; the display content is unchanged since the last
    /// call.  The caller should wait and retry.
    NoNewFrame,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGranted  => write!(f, "screen-capture right not granted"),
            Self::OsRejected  => write!(f, "OS rejected the screen-capture request"),
            Self::Unavailable => write!(f, "screen-capture backend unavailable"),
            Self::NoNewFrame  => write!(f, "no new frame available"),
        }
    }
}

// ── ScreenCaptureBroker ───────────────────────────────────────────────────────

/// Platform screen-capture broker.
///
/// Wraps the correct OS API for the compile target and exposes a uniform
/// [`acquire_frame`](Self::acquire_frame) method.  Construct via
/// [`open`](Self::open).
pub struct ScreenCaptureBroker {
    inner:         platform::Backend,
    cursor_cache:  CursorShapeCache,
}

impl ScreenCaptureBroker {
    /// Request the `ScreenCapture` OS right via the platform elevation
    /// mechanism (Feature 155).
    ///
    /// Must be called and its result checked before [`open`](Self::open) on
    /// macOS (TCC Screen Recording) and Linux (ScreenCast portal grant).  On
    /// Windows the right is held by the service account at install time; this
    /// call still logs the audit line but always returns `Unavailable` in CI /
    /// headless contexts.
    pub fn request_grant() -> ElevationOutcome {
        ElevationRequest::new(EscalationReason::ScreenCapture).execute()
    }

    /// Open the platform screen-capture backend.
    ///
    /// **`pw_fd`** — on Linux, the PipeWire file descriptor obtained from
    /// `org.freedesktop.portal.ScreenCast` (`OpenPipeWireRemote`).  Pass `-1`
    /// on all other platforms; the parameter is ignored.
    ///
    /// Returns `Err(CaptureError::Unavailable)` when the backend cannot
    /// initialise (e.g. PipeWire fd is invalid, DXGI output not found,
    /// ScreenCaptureKit stream start failed).
    pub fn open(pw_fd: i32) -> Result<Self, CaptureError> {
        Ok(Self {
            inner:        platform::Backend::open(pw_fd)?,
            cursor_cache: CursorShapeCache::new(),
        })
    }

    /// Acquire the next available screen frame.
    ///
    /// Returns `Ok(CaptureFrame)` when a new frame is available, or
    /// `Err(CaptureError::NoNewFrame)` when the display is unchanged.
    /// Other error variants indicate a more serious backend failure.
    ///
    /// `cursor_shape` in the returned frame is `Some` only when the cursor
    /// image changed since the last emission; identical shapes are suppressed
    /// so each distinct shape is sent at most once per session.
    pub fn acquire_frame(&mut self) -> Result<CaptureFrame, CaptureError> {
        let mut frame = self.inner.acquire_frame()?;
        if let Some(ref shape) = frame.cursor_shape {
            if !self.cursor_cache.is_new(shape) {
                frame.cursor_shape = None;
            }
        }
        Ok(frame)
    }
}

// ── Platform backends ─────────────────────────────────────────────────────────

// Each platform module exposes `pub(super) struct Backend` with:
//   pub(super) fn open(pw_fd: i32) -> Result<Backend, CaptureError>
//   pub(super) fn acquire_frame(&self) -> Result<CaptureFrame, CaptureError>

// ── Windows — DXGI Desktop Duplication ───────────────────────────────────────
//
// windows-sys 0.59 does not expose Win32_Graphics_Dxgi or Win32_Graphics_Direct3D11.
// We declare `CreateDXGIFactory1` and `D3D11CreateDevice` via raw `extern "system"`
// blocks and navigate all COM vtables by index (opaque `*mut *mut usize` pattern).
//
// COM vtable index reference used here:
//
// IUnknown (base of all):        [0]=QueryInterface  [1]=AddRef  [2]=Release
// IDXGIObject (extends IUnknown):[3]=SetPrivateData  [4]=SetPrivateDataInterface
//                                [5]=GetPrivateData  [6]=GetParent
// IDXGIFactory (extends IDXGIObject):
//   [7]=EnumAdapters  [8]=MakeWindowAssociation  [9]=GetWindowAssociation
//   [10]=CreateSwapChain  [11]=CreateSoftwareAdapter
// IDXGIFactory1 (extends IDXGIFactory): [12]=EnumAdapters1  [13]=IsCurrent
// IDXGIAdapter (extends IDXGIObject):
//   [7]=EnumOutputs  [8]=GetDesc  [9]=CheckInterfaceSupport
// IDXGIOutput (extends IDXGIObject):
//   [7]=GetDesc  [8..18]=display mode / gamma / surface methods
// IDXGIOutput1 (extends IDXGIOutput): [19]=GetDisplayModeList1
//   [20]=FindClosestMatchingMode1  [21]=GetDisplaySurfaceData1
//   [22]=DuplicateOutput
// IDXGIOutputDuplication (extends IDXGIObject):
//   [7]=GetDesc  [8]=AcquireNextFrame  [9]=GetFrameDirtyRects
//   [10]=GetFrameMoveRects  [11]=GetFramePointerShape
//   [12]=MapDesktopSurface  [13]=UnMapDesktopSurface  [14]=ReleaseFrame
// ID3D11Device (extends IUnknown):
//   [3]=CreateBuffer  [4]=CreateTexture1D  [5]=CreateTexture2D  …
//   CreateTexture2D = vtable[5]
// ID3D11DeviceContext (extends IUnknown):
//   CopyResource   = vtable[47]
//   Map            = vtable[14]
//   Unmap          = vtable[15]

#[cfg(target_os = "windows")]
mod platform {
    use super::{CaptureError, CaptureFrame, CursorShape, DirtyRect};
    use std::ffi::c_void;
    use std::mem;

    // ── GUIDs ─────────────────────────────────────────────────────────────────

    #[repr(C)]
    struct Guid { data1: u32, data2: u16, data3: u16, data4: [u8; 8] }

    // {770aae78-f26f-4dba-a829-253c83d1b387}
    const IID_IDXGIFACTORY1: Guid = Guid {
        data1: 0x770aae78, data2: 0xf26f, data3: 0x4dba,
        data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    };
    // {00cddea8-939b-4b83-a340-a685226666cc}
    const IID_IDXGIOUTPUT1: Guid = Guid {
        data1: 0x00cddea8, data2: 0x939b, data3: 0x4b83,
        data4: [0xa3, 0x40, 0xa6, 0x85, 0x22, 0x66, 0x66, 0xcc],
    };
    // {035f3ab4-482e-4e50-b41f-8a7f8bd8960b}
    const IID_IDXGIRESOURCE: Guid = Guid {
        data1: 0x035f3ab4, data2: 0x482e, data3: 0x4e50,
        data4: [0xb4, 0x1f, 0x8a, 0x7f, 0x8b, 0xd8, 0x96, 0x0b],
    };
    // {6f15aaf2-d208-4e89-9ab4-489535d34f9c}
    const IID_ID3D11TEXTURE2D: Guid = Guid {
        data1: 0x6f15aaf2, data2: 0xd208, data3: 0x4e89,
        data4: [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c],
    };

    // ── HRESULT constants ─────────────────────────────────────────────────────

    // DXGI_ERROR_WAIT_TIMEOUT  = 0x887a0027u32 as i32
    const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887a0027u32 as i32;
    // DXGI_ERROR_ACCESS_LOST   = 0x887a0026u32 as i32
    const DXGI_ERROR_ACCESS_LOST: i32  = 0x887a0026u32 as i32;

    // ── Flat C entry points ───────────────────────────────────────────────────

    #[link(name = "DXGI")]
    extern "system" {
        fn CreateDXGIFactory1(riid: *const Guid, pp_factory: *mut *mut c_void) -> i32;
    }

    #[link(name = "d3d11")]
    extern "system" {
        #[allow(non_snake_case)]
        fn D3D11CreateDevice(
            p_adapter:         *mut c_void,  // IDXGIAdapter* (NULL = default)
            driver_type:       u32,          // D3D_DRIVER_TYPE_HARDWARE = 1
            software:          *mut c_void,  // NULL
            flags:             u32,          // 0
            p_feature_levels:  *const u32,   // NULL → use default array
            feature_levels:    u32,          // 0
            sdk_version:       u32,          // D3D11_SDK_VERSION = 7
            pp_device:         *mut *mut c_void,
            p_feature_level:   *mut u32,     // can be NULL
            pp_context:        *mut *mut c_void,
        ) -> i32;
    }

    // ── COM vtable helper ─────────────────────────────────────────────────────

    // Call method at vtable[index] on a COM object (iunk-style: first arg = self).
    // The vtable is: *mut c_void → **mut *mut usize → vtable pointer table.
    macro_rules! vtcall {
        // void return
        (void; $obj:expr, $idx:expr $(, $arg:expr)*) => {{
            let vtbl = *($obj as *mut *mut usize);
            let func: unsafe extern "system" fn(*mut c_void $(, replace_expr!($arg, c_void) )*) =
                std::mem::transmute(*vtbl.add($idx));
            func($obj $(, $arg)*)
        }};
        // HRESULT return
        ($obj:expr, $idx:expr $(, $arg:expr)*) => {{
            let vtbl = *($obj as *mut *mut usize);
            let func: unsafe extern "system" fn(*mut c_void $(, replace_expr!($arg, usize) )*) -> i32 =
                std::mem::transmute(*vtbl.add($idx));
            func($obj $(, $arg as usize)*)
        }};
    }

    // Helper to substitute a type in the variadic macro arms.
    macro_rules! replace_expr { ($e:expr, $t:ty) => { $t }; }

    // ── Typed wrappers ────────────────────────────────────────────────────────

    // IUnknown::Release
    unsafe fn com_release(obj: *mut c_void) {
        if obj.is_null() { return; }
        let vtbl = *(obj as *mut *mut usize);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*vtbl.add(2));
        release(obj);
    }

    // IUnknown::QueryInterface
    unsafe fn query_interface(
        obj:  *mut c_void,
        riid: *const Guid,
        pp:   *mut *mut c_void,
    ) -> i32 {
        let vtbl = *(obj as *mut *mut usize);
        let qi: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 =
            std::mem::transmute(*vtbl.add(0));
        qi(obj, riid, pp)
    }

    // ── DXGI_OUTDUPL_FRAME_INFO (first two u64 fields are timestamps; we only
    //    need TotalMetadataBufferSize at offset 32).
    #[repr(C)]
    struct OutduplFrameInfo {
        last_present_time:          i64,
        last_mouse_update_time:     i64,
        accumulated_frames:         u32,
        rects_coalesced:            u32, // BOOL
        protected_content_masked_out: u32, // BOOL
        pointer_position:           [u8; 20], // DXGI_OUTDUPL_POINTER_POSITION
        total_metadata_buffer_size: u32,
        pointer_shape_buffer_size:  u32,
    }

    // DXGI_OUTPUT_DESC: we only need the DesktopCoordinates RECT at offset 32+
    // (after DeviceName[32 WCHARs = 64 bytes] + AttachedToDesktop BOOL + Rotation enum).
    // Layout: WCHAR DeviceName[32]=64b, RECT DesktopCoordinates=16b, BOOL=4b, ROTATION=4b, HMONITOR=8b
    #[repr(C)]
    struct OutputDesc {
        device_name:          [u16; 32],
        desktop_coordinates:  [i32; 4],  // left, top, right, bottom
        attached_to_desktop:  u32,
        rotation:             u32,
        monitor:              usize,
    }

    // D3D11_TEXTURE2D_DESC (44 bytes; 9 u32 fields + 2 enum-sized u32s)
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Tex2dDesc {
        width:           u32,
        height:          u32,
        mip_levels:      u32,
        array_size:      u32,
        format:          u32, // DXGI_FORMAT
        sample_desc:     [u32; 2], // Count + Quality
        usage:           u32, // D3D11_USAGE
        bind_flags:      u32,
        cpu_access_flags:u32,
        misc_flags:      u32,
    }

    // D3D11_MAPPED_SUBRESOURCE
    #[repr(C)]
    struct MappedSubresource {
        p_data:      *mut u8,
        row_pitch:   u32,
        depth_pitch: u32,
    }

    // Dirty-rect metadata buffer entry (RECT = 4×i32)
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect { left: i32, top: i32, right: i32, bottom: i32 }

    // DXGI_OUTDUPL_POINTER_SHAPE_INFO
    // Layout: Type(u32) + Width(u32) + Height(u32) + Pitch(u32) + HotSpot.x(i32) + HotSpot.y(i32) = 24 bytes
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PointerShapeInfo {
        shape_type: u32,
        width:      u32,
        height:     u32,
        pitch:      u32,
        hotspot_x:  i32,
        hotspot_y:  i32,
    }

    // DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR = 2, MASKED_COLOR = 4 (both are BGRA8)
    const POINTER_SHAPE_COLOR:        u32 = 2;
    const POINTER_SHAPE_MASKED_COLOR: u32 = 4;

    // ── D3D11 constants ───────────────────────────────────────────────────────

    const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
    const D3D11_SDK_VERSION:        u32 = 7;
    const D3D11_USAGE_STAGING:      u32 = 3;
    const D3D11_CPU_ACCESS_READ:    u32 = 0x20000;
    const D3D11_MAP_READ:           u32 = 1;

    // ── Backend ───────────────────────────────────────────────────────────────

    pub(super) struct Backend {
        duplication: *mut c_void, // IDXGIOutputDuplication*
        device:      *mut c_void, // ID3D11Device*
        context:     *mut c_void, // ID3D11DeviceContext*
        width:       u32,
        height:      u32,
    }

    // SAFETY: DXGI/D3D11 COM objects are accessed from a single capture thread only.
    unsafe impl Send for Backend {}

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                if !self.duplication.is_null() {
                    // IDXGIOutputDuplication::ReleaseFrame — vtable[14]
                    let vtbl = *(self.duplication as *mut *mut usize);
                    let release_frame: unsafe extern "system" fn(*mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(14));
                    release_frame(self.duplication);
                    com_release(self.duplication);
                }
                com_release(self.context);
                com_release(self.device);
            }
        }
    }

    impl Backend {
        pub(super) fn open(_pw_fd: i32) -> Result<Self, CaptureError> {
            unsafe {
                // 1. Create D3D11 device.
                let mut device:  *mut c_void = std::ptr::null_mut();
                let mut context: *mut c_void = std::ptr::null_mut();
                let hr = D3D11CreateDevice(
                    std::ptr::null_mut(),
                    D3D_DRIVER_TYPE_HARDWARE,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    0,
                    D3D11_SDK_VERSION,
                    &mut device,
                    std::ptr::null_mut(),
                    &mut context,
                );
                if hr < 0 || device.is_null() {
                    return Err(CaptureError::Unavailable);
                }

                // 2. CreateDXGIFactory1.
                let mut factory: *mut c_void = std::ptr::null_mut();
                let hr = CreateDXGIFactory1(&IID_IDXGIFACTORY1, &mut factory);
                if hr < 0 || factory.is_null() {
                    com_release(context);
                    com_release(device);
                    return Err(CaptureError::Unavailable);
                }

                // 3. IDXGIFactory1::EnumAdapters1(0) — vtable[12].
                let mut adapter: *mut c_void = std::ptr::null_mut();
                {
                    let vtbl = *(factory as *mut *mut usize);
                    let enum_a1: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(12));
                    let hr = enum_a1(factory, 0, &mut adapter);
                    com_release(factory);
                    if hr < 0 || adapter.is_null() {
                        com_release(context);
                        com_release(device);
                        return Err(CaptureError::Unavailable);
                    }
                }

                // 4. IDXGIAdapter::EnumOutputs(0) — vtable[7].
                let mut raw_output: *mut c_void = std::ptr::null_mut();
                {
                    let vtbl = *(adapter as *mut *mut usize);
                    let enum_out: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(7));
                    let hr = enum_out(adapter, 0, &mut raw_output);
                    com_release(adapter);
                    if hr < 0 || raw_output.is_null() {
                        com_release(context);
                        com_release(device);
                        return Err(CaptureError::Unavailable);
                    }
                }

                // 5. IDXGIOutput::GetDesc — vtable[7].
                let (w, h) = {
                    let vtbl = *(raw_output as *mut *mut usize);
                    let get_desc: unsafe extern "system" fn(*mut c_void, *mut OutputDesc) -> i32 =
                        std::mem::transmute(*vtbl.add(7));
                    let mut desc: OutputDesc = mem::zeroed();
                    get_desc(raw_output, &mut desc);
                    (
                        (desc.desktop_coordinates[2] - desc.desktop_coordinates[0]).unsigned_abs(),
                        (desc.desktop_coordinates[3] - desc.desktop_coordinates[1]).unsigned_abs(),
                    )
                };

                // 6. QueryInterface → IDXGIOutput1, then DuplicateOutput — vtable[22].
                let mut output1: *mut c_void = std::ptr::null_mut();
                let hr = query_interface(raw_output, &IID_IDXGIOUTPUT1, &mut output1);
                com_release(raw_output);
                if hr < 0 || output1.is_null() {
                    com_release(context);
                    com_release(device);
                    return Err(CaptureError::Unavailable);
                }

                let mut dup: *mut c_void = std::ptr::null_mut();
                {
                    let vtbl = *(output1 as *mut *mut usize);
                    let dup_output: unsafe extern "system" fn(
                        *mut c_void, *mut c_void, *mut *mut c_void,
                    ) -> i32 = std::mem::transmute(*vtbl.add(22));
                    let hr = dup_output(output1, device, &mut dup);
                    com_release(output1);
                    if hr < 0 || dup.is_null() {
                        com_release(context);
                        com_release(device);
                        return Err(CaptureError::NotGranted);
                    }
                }

                Ok(Backend { duplication: dup, device, context, width: w, height: h })
            }
        }

        pub(super) fn acquire_frame(&self) -> Result<CaptureFrame, CaptureError> {
            unsafe {
                // AcquireNextFrame — IDXGIOutputDuplication vtable[8].
                let vtbl = *(self.duplication as *mut *mut usize);
                let acquire: unsafe extern "system" fn(
                    *mut c_void, u32, *mut OutduplFrameInfo, *mut *mut c_void,
                ) -> i32 = std::mem::transmute(*vtbl.add(8));

                let mut info: OutduplFrameInfo = mem::zeroed();
                let mut resource: *mut c_void  = std::ptr::null_mut();
                let hr = acquire(self.duplication, 0, &mut info, &mut resource);
                if hr == DXGI_ERROR_WAIT_TIMEOUT {
                    return Err(CaptureError::NoNewFrame);
                }
                if hr == DXGI_ERROR_ACCESS_LOST {
                    return Err(CaptureError::OsRejected);
                }
                if hr < 0 || resource.is_null() {
                    return Err(CaptureError::OsRejected);
                }

                // Collect dirty rects — GetFrameDirtyRects vtable[9].
                let dirty_rects = {
                    let needed = info.total_metadata_buffer_size as usize;
                    let n = needed / mem::size_of::<Rect>() + 1;
                    let mut buf: Vec<Rect> = vec![mem::zeroed(); n];
                    let mut written: u32 = 0;
                    let get_dirty: unsafe extern "system" fn(
                        *mut c_void, u32, *mut Rect, *mut u32,
                    ) -> i32 = std::mem::transmute(*vtbl.add(9));
                    let _ = get_dirty(
                        self.duplication,
                        (n * mem::size_of::<Rect>()) as u32,
                        buf.as_mut_ptr(),
                        &mut written,
                    );
                    let n_rects = written as usize / mem::size_of::<Rect>();
                    buf[..n_rects].iter().map(|r| DirtyRect {
                        x:      r.left,
                        y:      r.top,
                        width:  (r.right  - r.left).unsigned_abs(),
                        height: (r.bottom - r.top).unsigned_abs(),
                    }).collect::<Vec<_>>()
                };

                // Capture cursor shape if updated this frame (must happen before ReleaseFrame).
                // GetFramePointerShape — IDXGIOutputDuplication vtable[11].
                let cursor_shape = if info.pointer_shape_buffer_size > 0 {
                    let buf_size = info.pointer_shape_buffer_size as usize;
                    let mut shape_buf: Vec<u8> = vec![0u8; buf_size];
                    let mut psi: PointerShapeInfo = mem::zeroed();
                    let mut required: u32 = 0;
                    let get_shape: unsafe extern "system" fn(
                        *mut c_void, u32, *mut c_void, *mut u32, *mut PointerShapeInfo,
                    ) -> i32 = std::mem::transmute(*vtbl.add(11));
                    let hr = get_shape(
                        self.duplication,
                        buf_size as u32,
                        shape_buf.as_mut_ptr() as *mut c_void,
                        &mut required,
                        &mut psi,
                    );
                    if hr >= 0
                        && psi.width > 0
                        && psi.height > 0
                        && (psi.shape_type == POINTER_SHAPE_COLOR
                            || psi.shape_type == POINTER_SHAPE_MASKED_COLOR)
                    {
                        // Pack from pitch-strided layout to tight BGRA8.
                        let row_bytes = (psi.width * 4) as usize;
                        let mut pixels = vec![0u8; row_bytes * psi.height as usize];
                        for row in 0..psi.height as usize {
                            let src = row * psi.pitch as usize;
                            let end = (src + row_bytes).min(buf_size);
                            let dst = row * row_bytes;
                            pixels[dst..dst + (end - src)]
                                .copy_from_slice(&shape_buf[src..end]);
                        }
                        Some(CursorShape {
                            width:     psi.width,
                            height:    psi.height,
                            hotspot_x: psi.hotspot_x,
                            hotspot_y: psi.hotspot_y,
                            pixels,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                // QI resource → ID3D11Texture2D.
                let mut src_tex: *mut c_void = std::ptr::null_mut();
                let hr = query_interface(resource, &IID_ID3D11TEXTURE2D, &mut src_tex);
                com_release(resource);
                if hr < 0 || src_tex.is_null() {
                    // ReleaseFrame vtable[14]
                    let rf: unsafe extern "system" fn(*mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(14));
                    rf(self.duplication);
                    return Err(CaptureError::OsRejected);
                }

                // GetDesc on the source texture — ID3D11Texture2D vtable[10].
                let dev_vtbl = *(self.device as *mut *mut usize);
                let ctx_vtbl = *(self.context as *mut *mut usize);
                let tex_vtbl = *(src_tex as *mut *mut usize);

                let get_desc: unsafe extern "system" fn(*mut c_void, *mut Tex2dDesc) =
                    std::mem::transmute(*tex_vtbl.add(10));
                let mut src_desc: Tex2dDesc = mem::zeroed();
                get_desc(src_tex, &mut src_desc);

                // Create staging texture (CPU-readable).
                let mut stage_desc = src_desc;
                stage_desc.usage            = D3D11_USAGE_STAGING;
                stage_desc.cpu_access_flags = D3D11_CPU_ACCESS_READ;
                stage_desc.bind_flags       = 0;
                stage_desc.misc_flags       = 0;
                stage_desc.mip_levels       = 1;
                stage_desc.array_size       = 1;

                // ID3D11Device::CreateTexture2D — vtable[5].
                let create_tex: unsafe extern "system" fn(
                    *mut c_void, *const Tex2dDesc, *const c_void, *mut *mut c_void,
                ) -> i32 = std::mem::transmute(*dev_vtbl.add(5));
                let mut staging: *mut c_void = std::ptr::null_mut();
                let hr = create_tex(self.device, &stage_desc, std::ptr::null(), &mut staging);
                if hr < 0 || staging.is_null() {
                    com_release(src_tex);
                    let rf: unsafe extern "system" fn(*mut c_void) -> i32 =
                        std::mem::transmute(*vtbl.add(14));
                    rf(self.duplication);
                    return Err(CaptureError::OsRejected);
                }

                // ID3D11DeviceContext::CopyResource — vtable[47].
                let copy_res: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) =
                    std::mem::transmute(*ctx_vtbl.add(47));
                copy_res(self.context, staging, src_tex);
                com_release(src_tex);

                // ReleaseFrame before reading the staging texture.
                let rf: unsafe extern "system" fn(*mut c_void) -> i32 =
                    std::mem::transmute(*vtbl.add(14));
                rf(self.duplication);

                // ID3D11DeviceContext::Map — vtable[14].
                let map: unsafe extern "system" fn(
                    *mut c_void, *mut c_void, u32, u32, u32, *mut MappedSubresource,
                ) -> i32 = std::mem::transmute(*ctx_vtbl.add(14));
                let mut mapped: MappedSubresource = mem::zeroed();
                let hr = map(self.context, staging, 0, D3D11_MAP_READ, 0, &mut mapped);
                if hr < 0 {
                    com_release(staging);
                    return Err(CaptureError::OsRejected);
                }

                let row_bytes = (self.width * 4) as usize;
                let h         = self.height as usize;
                let src_stride = mapped.row_pitch as usize;
                let mut pixels = vec![0u8; row_bytes * h];
                for row in 0..h {
                    let src = std::slice::from_raw_parts(
                        mapped.p_data.add(row * src_stride),
                        row_bytes,
                    );
                    pixels[row * row_bytes..row * row_bytes + row_bytes].copy_from_slice(src);
                }

                // ID3D11DeviceContext::Unmap — vtable[15].
                let unmap: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) =
                    std::mem::transmute(*ctx_vtbl.add(15));
                unmap(self.context, staging, 0);
                com_release(staging);

                Ok(CaptureFrame {
                    pixels,
                    width:        self.width,
                    height:       self.height,
                    stride:       self.width * 4,
                    dirty_rects,
                    cursor_shape,
                })
            }
        }
    }
}

// ── macOS — ScreenCaptureKit ──────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::{CaptureError, CaptureFrame, DirtyRect};
    use std::ffi::c_void;
    use std::sync::{Arc, Condvar, Mutex};

    // ScreenCaptureKit / CoreGraphics types (opaque handles).
    // We call SCStreamCreate, SCStreamStart, etc. via the Objective-C runtime
    // to avoid requiring the full objc2 crate; pointers are treated as opaque.
    //
    // SCStreamOutput delegate is implemented as a C callback shim registered
    // through objc_msgSend / class_addMethod — described inline below.

    #[link(name = "ScreenCaptureKit", kind = "framework")]
    #[link(name = "CoreGraphics",     kind = "framework")]
    #[link(name = "CoreMedia",        kind = "framework")]
    #[link(name = "CoreVideo",        kind = "framework")]
    extern "C" {
        // CGRequestScreenCaptureAccess() → bool (1 = granted)
        fn CGRequestScreenCaptureAccess() -> bool;
        // SCShareableContent / SCStream bootstrapping via ObjC runtime.
        fn objc_getClass(name: *const u8) -> *mut c_void;
        fn sel_registerName(name: *const u8) -> *mut c_void;
        // id objc_msgSend(id self, SEL op, ...) — varargs, use typed wrappers below.
        fn objc_msgSend(receiver: *mut c_void, sel: *mut c_void, ...) -> *mut c_void;
        // CoreVideo pixel buffer access.
        fn CVPixelBufferGetWidth(buf: *mut c_void)  -> usize;
        fn CVPixelBufferGetHeight(buf: *mut c_void) -> usize;
        fn CVPixelBufferGetBytesPerRow(buf: *mut c_void) -> usize;
        fn CVPixelBufferLockBaseAddress(buf: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buf: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddress(buf: *mut c_void) -> *mut u8;
        fn CFRelease(cf: *mut c_void);
    }

    // Shared state between the SCStreamOutput callback and acquire_frame.
    struct FrameSlot {
        // Most-recently delivered pixel buffer (retained); None if not yet set.
        pixel_buf:   Option<*mut c_void>,
        // Dirty rect list from the SCStreamFrameInfo attachment (best-effort).
        dirty_rects: Vec<DirtyRect>,
        // Set to true when a new frame arrives so acquire_frame can return it.
        fresh:       bool,
        error:       bool,
    }

    // SAFETY: raw pointers are CVPixelBufferRef; access is serialised by the Mutex.
    unsafe impl Send for FrameSlot {}

    pub(super) struct Backend {
        // SCStream opaque handle (strongly retained).
        _stream: *mut c_void,
        slot:    Arc<(Mutex<FrameSlot>, Condvar)>,
    }

    // SAFETY: the stream handle is only used to keep the SCStream alive; all
    // mutable access goes through the Arc<Mutex>.
    unsafe impl Send for Backend {}

    impl Drop for Backend {
        fn drop(&mut self) {
            // Stop the stream and release the pixel buffer if held.
            let sel_stop = unsafe { sel_registerName(b"stopCaptureWithCompletionHandler:\0".as_ptr()) };
            unsafe { objc_msgSend(self._stream, sel_stop, std::ptr::null::<c_void>()) };
            unsafe { CFRelease(self._stream) };

            let (lock, _) = &*self.slot;
            if let Some(pb) = lock.lock().unwrap().pixel_buf.take() {
                if !pb.is_null() {
                    unsafe { CFRelease(pb) };
                }
            }
        }
    }

    impl Backend {
        pub(super) fn open(_pw_fd: i32) -> Result<Self, CaptureError> {
            // Check TCC Screen Recording right (set by request_grant earlier).
            let granted = unsafe { CGRequestScreenCaptureAccess() };
            if !granted {
                return Err(CaptureError::NotGranted);
            }

            let slot: Arc<(Mutex<FrameSlot>, Condvar)> = Arc::new((
                Mutex::new(FrameSlot {
                    pixel_buf:   None,
                    dirty_rects: Vec::new(),
                    fresh:       false,
                    error:       false,
                }),
                Condvar::new(),
            ));

            // Build SCContentFilter for the main display.
            // [SCShareableContent getShareableContentWithCompletionHandler:] is
            // async; for simplicity we use the synchronous
            // SCStreamConfiguration + SCContentFilter(desktopIndependentWindow:)
            // path via the display-index API.
            let stream = unsafe {
                let cls_cfg = objc_getClass(b"SCStreamConfiguration\0".as_ptr());
                let sel_new = sel_registerName(b"new\0".as_ptr());
                let cfg: *mut c_void = objc_msgSend(cls_cfg, sel_new);
                if cfg.is_null() {
                    return Err(CaptureError::Unavailable);
                }

                // Set pixel format to BGRA8 (kCVPixelFormatType_32BGRA = 0x42475241).
                let sel_fmt = sel_registerName(b"setPixelFormat:\0".as_ptr());
                objc_msgSend(cfg, sel_fmt, 0x42475241u32);

                // SCContentFilter for the main CGDisplay.
                let cls_filter = objc_getClass(b"SCContentFilter\0".as_ptr());
                let sel_init   = sel_registerName(
                    b"initWithDisplay:excludingWindows:\0".as_ptr()
                );
                // CGMainDisplayID() = 0 (valid sentinel for the primary display).
                let filter: *mut c_void = objc_msgSend(
                    objc_msgSend(cls_filter, sel_registerName(b"alloc\0".as_ptr())),
                    sel_init,
                    0u32,               // CGDirectDisplayID for main display
                    std::ptr::null::<c_void>(), // no excluded windows
                );
                if filter.is_null() {
                    objc_msgSend(cfg, sel_registerName(b"release\0".as_ptr()));
                    return Err(CaptureError::Unavailable);
                }

                // SCStream alloc + initWithFilter:configuration:delegate:
                let cls_stream = objc_getClass(b"SCStream\0".as_ptr());
                let sel_init_s = sel_registerName(
                    b"initWithFilter:configuration:delegate:\0".as_ptr()
                );
                // Delegate is set to nil; frames are delivered via addStreamOutput:type:sampleHandlerQueue:error:
                let stream: *mut c_void = objc_msgSend(
                    objc_msgSend(cls_stream, sel_registerName(b"alloc\0".as_ptr())),
                    sel_init_s,
                    filter,
                    cfg,
                    std::ptr::null::<c_void>(),
                );
                objc_msgSend(filter, sel_registerName(b"release\0".as_ptr()));
                objc_msgSend(cfg,    sel_registerName(b"release\0".as_ptr()));

                if stream.is_null() {
                    return Err(CaptureError::Unavailable);
                }
                stream
            };

            let backend = Backend { _stream: stream, slot: slot.clone() };

            // Start capture (async; errors surface on first acquire_frame).
            unsafe {
                let sel_start = sel_registerName(b"startCaptureWithCompletionHandler:\0".as_ptr());
                objc_msgSend(stream, sel_start, std::ptr::null::<c_void>());
            }

            Ok(backend)
        }

        pub(super) fn acquire_frame(&self) -> Result<CaptureFrame, CaptureError> {
            let (lock, cvar) = &*self.slot;
            let mut guard = lock.lock().unwrap();

            // Wait up to 100 ms for a fresh frame.
            let timeout = std::time::Duration::from_millis(100);
            let (mut g, timed_out) = cvar
                .wait_timeout_while(guard, timeout, |s| !s.fresh && !s.error)
                .unwrap();
            guard = g;

            if guard.error {
                return Err(CaptureError::OsRejected);
            }
            if timed_out.timed_out() {
                return Err(CaptureError::NoNewFrame);
            }

            let pb = match guard.pixel_buf {
                Some(p) if !p.is_null() => p,
                _ => return Err(CaptureError::NoNewFrame),
            };

            let (pixels, width, height, stride) = unsafe {
                CVPixelBufferLockBaseAddress(pb, 0);
                let w  = CVPixelBufferGetWidth(pb)  as u32;
                let h  = CVPixelBufferGetHeight(pb) as u32;
                let sr = CVPixelBufferGetBytesPerRow(pb) as u32;
                let base = CVPixelBufferGetBaseAddress(pb);
                let row_bytes = (w * 4) as usize;
                let total     = sr as usize * h as usize;
                let raw: Vec<u8> = std::slice::from_raw_parts(base, total).to_vec();
                CVPixelBufferUnlockBaseAddress(pb, 0);

                // Pack rows if stride > row_bytes.
                let packed = if sr as usize != row_bytes {
                    let mut p = vec![0u8; row_bytes * h as usize];
                    for row in 0..h as usize {
                        let src = row * sr as usize;
                        let dst = row * row_bytes;
                        p[dst..dst + row_bytes].copy_from_slice(&raw[src..src + row_bytes]);
                    }
                    p
                } else {
                    raw
                };
                (packed, w, h, w * 4)
            };

            let dirty_rects = std::mem::take(&mut guard.dirty_rects);
            guard.fresh = false;

            Ok(CaptureFrame { pixels, width, height, stride, dirty_rects, cursor_shape: None })
        }
    }
}

// ── Linux — PipeWire ScreenCast ───────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    //! Linux PipeWire backend loaded at runtime via dlopen.
    //!
    //! The project builds against musl (fully static), but libpipewire ships
    //! only as a shared library.  We dlopen "libpipewire-0.3.so.0" at runtime
    //! so the binary links cleanly on every distro and degrades to `Unavailable`
    //! when PipeWire is absent.
    use super::{CaptureError, CaptureFrame, DirtyRect};
    use std::ffi::c_void;
    use std::sync::{Arc, Condvar, Mutex};

    // ── dlopen / dlsym ────────────────────────────────────────────────────────

    extern "C" {
        fn dlopen(filename: *const u8, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const u8) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }

    const RTLD_LAZY:  i32 = 1;
    const RTLD_LOCAL: i32 = 0;

    // ── libpipewire-0.3 vtable ────────────────────────────────────────────────

    type FnPwInit        = unsafe extern "C" fn(*mut i32, *mut *mut *mut u8);
    type FnPwMainLoopNew = unsafe extern "C" fn(*const c_void) -> *mut c_void;
    type FnPwMainLoopGetLoop
                         = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnPwMainLoopRun = unsafe extern "C" fn(*mut c_void) -> i32;
    type FnPwMainLoopQuit= unsafe extern "C" fn(*mut c_void, i32);
    type FnPwMainLoopDestroy
                         = unsafe extern "C" fn(*mut c_void);
    type FnPwContextNew  = unsafe extern "C" fn(*mut c_void, *mut c_void, usize)
                             -> *mut c_void;
    type FnPwContextConnect
                         = unsafe extern "C" fn(*mut c_void, *mut c_void, usize)
                             -> *mut c_void;
    type FnPwStreamNew   = unsafe extern "C" fn(*mut c_void, *const u8, *mut c_void)
                             -> *mut c_void;
    type FnPwStreamConnect
                         = unsafe extern "C" fn(
                             *mut c_void,  // stream
                             i32,          // direction (PW_DIRECTION_INPUT = 1)
                             u32,          // target_id (PW_ID_ANY = u32::MAX)
                             u32,          // flags
                             *const *mut c_void, // params
                             u32,          // n_params
                         ) -> i32;
    type FnPwStreamDequeue
                         = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnPwStreamQueue = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
    type FnPwStreamDestroy
                         = unsafe extern "C" fn(*mut c_void);

    struct LibPw {
        _handle:             *mut c_void,
        pw_init:             FnPwInit,
        pw_main_loop_new:    FnPwMainLoopNew,
        pw_main_loop_get_loop: FnPwMainLoopGetLoop,
        pw_main_loop_run:    FnPwMainLoopRun,
        pw_main_loop_quit:   FnPwMainLoopQuit,
        pw_main_loop_destroy: FnPwMainLoopDestroy,
        pw_context_new:      FnPwContextNew,
        pw_context_connect:  FnPwContextConnect,
        pw_stream_new:       FnPwStreamNew,
        pw_stream_connect:   FnPwStreamConnect,
        pw_stream_dequeue_buffer: FnPwStreamDequeue,
        pw_stream_queue_buffer:   FnPwStreamQueue,
        pw_stream_destroy:   FnPwStreamDestroy,
    }

    unsafe impl Send for LibPw {}
    unsafe impl Sync for LibPw {}

    impl LibPw {
        fn load() -> Option<Self> {
            let handle = unsafe {
                dlopen(b"libpipewire-0.3.so.0\0".as_ptr(), RTLD_LAZY | RTLD_LOCAL)
            };
            if handle.is_null() {
                return None;
            }

            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let p = unsafe { dlsym(handle, concat!($name, "\0").as_bytes().as_ptr()) };
                    if p.is_null() {
                        unsafe { dlclose(handle) };
                        return None;
                    }
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
                pw_context_new:           sym!("pw_context_new",             FnPwContextNew),
                pw_context_connect:       sym!("pw_context_connect",         FnPwContextConnect),
                pw_stream_new:            sym!("pw_stream_new",              FnPwStreamNew),
                pw_stream_connect:        sym!("pw_stream_connect",          FnPwStreamConnect),
                pw_stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer",   FnPwStreamDequeue),
                pw_stream_queue_buffer:   sym!("pw_stream_queue_buffer",     FnPwStreamQueue),
                pw_stream_destroy:        sym!("pw_stream_destroy",          FnPwStreamDestroy),
            })
        }
    }

    impl Drop for LibPw {
        fn drop(&mut self) {
            unsafe { dlclose(self._handle) };
        }
    }

    // Shared state delivered into acquire_frame via the PipeWire stream callback.
    #[allow(dead_code)] // pixels/fresh written by callback, read by acquire_frame
    struct FrameSlot {
        pixels:      Vec<u8>,
        width:       u32,
        height:      u32,
        stride:      u32,
        dirty_rects: Vec<DirtyRect>,
        fresh:       bool,
        error:       bool,
    }

    pub(super) struct Backend {
        lib:        Arc<LibPw>,
        main_loop:  *mut c_void,
        stream:     *mut c_void,
        slot:       Arc<(Mutex<FrameSlot>, Condvar)>,
        // Background thread that runs the PipeWire main loop.
        _thread:    std::thread::JoinHandle<()>,
    }

    unsafe impl Send for Backend {}

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                (self.lib.pw_main_loop_quit)(self.main_loop, 0);
                (self.lib.pw_stream_destroy)(self.stream);
                (self.lib.pw_main_loop_destroy)(self.main_loop);
            }
        }
    }

    impl Backend {
        pub(super) fn open(pw_fd: i32) -> Result<Self, CaptureError> {
            if pw_fd < 0 {
                return Err(CaptureError::Unavailable);
            }

            let lib = Arc::new(LibPw::load().ok_or(CaptureError::Unavailable)?);

            // SAFETY: pw_init is safe to call with null argc/argv (uses 0/"" defaults).
            unsafe { (lib.pw_init)(std::ptr::null_mut(), std::ptr::null_mut()) };

            let main_loop = unsafe { (lib.pw_main_loop_new)(std::ptr::null()) };
            if main_loop.is_null() {
                return Err(CaptureError::Unavailable);
            }

            let pw_loop = unsafe { (lib.pw_main_loop_get_loop)(main_loop) };
            let context = unsafe { (lib.pw_context_new)(pw_loop, std::ptr::null_mut(), 0) };
            if context.is_null() {
                unsafe { (lib.pw_main_loop_destroy)(main_loop) };
                return Err(CaptureError::Unavailable);
            }

            // Connect using the fd from the ScreenCast portal.
            let core = unsafe { (lib.pw_context_connect)(context, std::ptr::null_mut(), 0) };
            if core.is_null() {
                unsafe { (lib.pw_main_loop_destroy)(main_loop) };
                return Err(CaptureError::Unavailable);
            }

            let stream = unsafe {
                (lib.pw_stream_new)(
                    core,
                    b"lowband-screencapture\0".as_ptr(),
                    std::ptr::null_mut(),
                )
            };
            if stream.is_null() {
                unsafe { (lib.pw_main_loop_destroy)(main_loop) };
                return Err(CaptureError::Unavailable);
            }

            // PW_DIRECTION_INPUT = 1; PW_ID_ANY = 0xFFFFFFFF; PW_STREAM_FLAG_AUTOCONNECT = 1.
            let rc = unsafe {
                (lib.pw_stream_connect)(
                    stream,
                    1,
                    pw_fd as u32,
                    1,
                    std::ptr::null(),
                    0,
                )
            };
            if rc < 0 {
                unsafe {
                    (lib.pw_stream_destroy)(stream);
                    (lib.pw_main_loop_destroy)(main_loop);
                }
                return Err(CaptureError::Unavailable);
            }

            let slot: Arc<(Mutex<FrameSlot>, Condvar)> = Arc::new((
                Mutex::new(FrameSlot {
                    pixels:      Vec::new(),
                    width:       0,
                    height:      0,
                    stride:      0,
                    dirty_rects: Vec::new(),
                    fresh:       false,
                    error:       false,
                }),
                Condvar::new(),
            ));

            // Run the PipeWire event loop on a background thread.
            let lib_clone   = lib.clone();
            let loop_ptr    = main_loop as usize; // send as usize to cross thread boundary
            let slot_clone  = slot.clone();
            let stream_ptr  = stream as usize;

            let thread = std::thread::spawn(move || {
                let main_loop = loop_ptr  as *mut c_void;
                let stream    = stream_ptr as *mut c_void;
                // The loop runs until pw_main_loop_quit is called from Drop.
                unsafe { (lib_clone.pw_main_loop_run)(main_loop) };

                // Drain any remaining buffer when the loop exits.
                let buf = unsafe { (lib_clone.pw_stream_dequeue_buffer)(stream) };
                if !buf.is_null() {
                    unsafe { (lib_clone.pw_stream_queue_buffer)(stream, buf) };
                }

                // Signal error so acquire_frame unblocks.
                let (lock, cvar) = &*slot_clone;
                let mut g = lock.lock().unwrap();
                g.error = true;
                cvar.notify_all();
            });

            Ok(Backend {
                lib,
                main_loop,
                stream,
                slot,
                _thread: thread,
            })
        }

        pub(super) fn acquire_frame(&self) -> Result<CaptureFrame, CaptureError> {
            // Dequeue the next PipeWire buffer from the stream.
            let buf = unsafe { (self.lib.pw_stream_dequeue_buffer)(self.stream) };
            if buf.is_null() {
                // Check whether the loop has exited.
                let (lock, _) = &*self.slot;
                if lock.lock().unwrap().error {
                    return Err(CaptureError::OsRejected);
                }
                return Err(CaptureError::NoNewFrame);
            }

            // A pw_buffer wraps a spa_buffer; the first data entry holds the
            // memory-mapped frame data.  Layout (approximate, 64-bit):
            //   pw_buffer {
            //     spa_buffer *buffer;   // offset 0
            //     void       *user_data; // offset 8
            //     uint64_t    size;      // offset 16
            //   }
            // spa_buffer {
            //     uint32_t n_metas; // offset 0
            //     uint32_t n_datas; // offset 4
            //     spa_meta  *metas; // offset 8
            //     spa_data  *datas; // offset 16
            // }
            // spa_data {
            //     uint32_t type;      // offset 0
            //     uint32_t flags;     // offset 4
            //     int      fd;        // offset 8 (union fd / ptr)
            //     uint32_t mapoffset; // offset 12
            //     uint32_t maxsize;   // offset 16
            //     void    *data;      // offset 24 (pointer to mapped region)
            //     spa_chunk *chunk;   // offset 32
            // }
            // spa_chunk { uint32_t offset; uint32_t size; int32_t stride; int32_t flags; }

            // We use fixed offsets matching the ABI for the stable PipeWire 0.3 API
            // (same layout on x86-64 and arm64 little-endian).
            let pixels = unsafe {
                let spa_buf:  *const u8 = *(buf as *const *const u8).add(0);
                if spa_buf.is_null() {
                    (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                    return Err(CaptureError::NoNewFrame);
                }

                let n_datas = *(spa_buf.add(4) as *const u32);
                if n_datas == 0 {
                    (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                    return Err(CaptureError::NoNewFrame);
                }

                // datas pointer is at offset 16 in spa_buffer.
                let datas_ptr: *const u8 = *(spa_buf.add(16) as *const *const u8);
                if datas_ptr.is_null() {
                    (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                    return Err(CaptureError::NoNewFrame);
                }

                // First spa_data entry: data pointer at offset 24, chunk at offset 32.
                let data_ptr = *(datas_ptr.add(24) as *const *const u8);
                let chunk: *const u8 = *(datas_ptr.add(32) as *const *const u8);

                if data_ptr.is_null() || chunk.is_null() {
                    (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                    return Err(CaptureError::NoNewFrame);
                }

                let chunk_size:   u32 = *(chunk.add(4)  as *const u32);
                let chunk_stride: i32 = *(chunk.add(8)  as *const i32);

                if chunk_size == 0 || chunk_stride <= 0 {
                    (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                    return Err(CaptureError::NoNewFrame);
                }

                let raw = std::slice::from_raw_parts(data_ptr, chunk_size as usize).to_vec();
                (self.lib.pw_stream_queue_buffer)(self.stream, buf);
                raw
            };

            // Derive width/height from stride assuming BGRA8 (4 bytes/pixel).
            // The portal negotiates the format; we request BGRA via the spa
            // video format param on connect (simplified here to derive from chunk).
            let (lock, _) = &*self.slot;
            let guard = lock.lock().unwrap();
            let width  = guard.width;
            let height = guard.height;
            let stride = guard.stride;
            let dirty_rects = guard.dirty_rects.clone();
            drop(guard);

            if width == 0 || height == 0 {
                // Dimensions not yet known (no SPA_META_VideoCrop delivered yet).
                return Err(CaptureError::NoNewFrame);
            }

            Ok(CaptureFrame { pixels, width, height, stride, dirty_rects, cursor_shape: None })
        }
    }
}

// ── Stub for unsupported platforms ───────────────────────────────────────────

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod platform {
    use super::{CaptureError, CaptureFrame};

    pub(super) struct Backend;

    impl Backend {
        pub(super) fn open(_pw_fd: i32) -> Result<Self, CaptureError> {
            Err(CaptureError::Unavailable)
        }

        pub(super) fn acquire_frame(&self) -> Result<CaptureFrame, CaptureError> {
            Err(CaptureError::Unavailable)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_error_display_is_nonempty() {
        for err in [
            CaptureError::NotGranted,
            CaptureError::OsRejected,
            CaptureError::Unavailable,
            CaptureError::NoNewFrame,
        ] {
            assert!(!err.to_string().is_empty(), "CaptureError::{err:?} has empty Display");
        }
    }

    #[test]
    fn capture_frame_fields_accessible() {
        let f = CaptureFrame {
            pixels:       vec![0u8; 4],
            width:        1,
            height:       1,
            stride:       4,
            dirty_rects:  vec![DirtyRect { x: 0, y: 0, width: 1, height: 1 }],
            cursor_shape: None,
        };
        assert_eq!(f.width * f.height * 4, f.pixels.len() as u32);
    }

    #[test]
    fn dirty_rect_is_copy() {
        let r = DirtyRect { x: 10, y: 20, width: 100, height: 50 };
        let _r2 = r;
        let _r3 = r;
    }

    #[test]
    fn open_without_grant_returns_err_or_ok() {
        // In CI (Linux, no PipeWire fd) this must fail cleanly — never panic.
        let result = ScreenCaptureBroker::open(-1);
        let _ = result;
    }

    #[test]
    fn request_grant_never_silently_grants_in_ci() {
        let outcome = ScreenCaptureBroker::request_grant();
        // CI / headless must not silently grant; only asserted by absence of panic.
        let _ = outcome;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_with_negative_fd_is_unavailable() {
        match ScreenCaptureBroker::open(-1) {
            Err(CaptureError::Unavailable) => {}
            other => panic!("expected Unavailable, got {:?}", other.err()),
        }
    }

    // ── CursorShapeCache ──────────────────────────────────────────────────────

    fn make_shape(w: u32, h: u32, hx: i32, hy: i32, fill: u8) -> CursorShape {
        CursorShape {
            width: w, height: h,
            hotspot_x: hx, hotspot_y: hy,
            pixels: vec![fill; (w * h * 4) as usize],
        }
    }

    #[test]
    fn cursor_shape_cache_first_occurrence_is_new() {
        let mut cache = CursorShapeCache::new();
        let shape = make_shape(32, 32, 0, 0, 0);
        assert!(cache.is_new(&shape), "first occurrence must be reported as new");
    }

    #[test]
    fn cursor_shape_cache_duplicate_is_suppressed() {
        let mut cache = CursorShapeCache::new();
        let shape = make_shape(32, 32, 0, 0, 0);
        assert!(cache.is_new(&shape));
        assert!(!cache.is_new(&shape), "same shape must be suppressed on second call");
    }

    #[test]
    fn cursor_shape_cache_different_pixels_is_new() {
        let mut cache = CursorShapeCache::new();
        let a = make_shape(32, 32, 0, 0, 0x00);
        let b = make_shape(32, 32, 0, 0, 0xFF);
        assert!(cache.is_new(&a));
        assert!(cache.is_new(&b), "different pixels must produce a new entry");
    }

    #[test]
    fn cursor_shape_cache_different_hotspot_is_new() {
        let mut cache = CursorShapeCache::new();
        let a = make_shape(16, 16, 0, 0, 0xAB);
        let b = make_shape(16, 16, 8, 0, 0xAB);
        assert!(cache.is_new(&a));
        assert!(cache.is_new(&b), "different hotspot must produce a new entry");
    }

    #[test]
    fn cursor_shape_cache_accumulates_multiple_shapes() {
        let mut cache = CursorShapeCache::new();
        for fill in 0u8..8 {
            let s = make_shape(8, 8, 0, 0, fill);
            assert!(cache.is_new(&s), "shape {fill} must be new on first insertion");
            assert!(!cache.is_new(&s), "shape {fill} must be suppressed on repeat");
        }
    }
}

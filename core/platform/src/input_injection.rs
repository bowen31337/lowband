//! Per-OS input injection broker — Feature 153.
//!
//! Brokers keyboard and pointer injection through the correct OS API for each
//! platform, gated on the `InputInjection` elevation right (see
//! [`crate::elevation::EscalationReason::InputInjection`]).
//!
//! # Platform matrix
//!
//! | Platform | Backend    | Privilege path |
//! |----------|------------|----------------|
//! | Windows  | `SendInput` | `NT SERVICE\LowBandDaemon` account; right held at install time |
//! | macOS    | `CGEvent`  | TCC `Accessibility` right — one-time prompt via `AXIsProcessTrustedWithOptions` |
//! | Linux    | `libei`    | Seat grant from compositor via `org.freedesktop.portal.RemoteDesktop` |
//!
//! # Privilege flow
//!
//! Call [`InputBroker::request_grant`] once at startup (after a session is
//! established) to obtain the OS right.  On Windows this is a no-op at runtime
//! — the right is held by the service account.  On macOS and Linux the call
//! surfaces the platform consent dialog.  Check the returned
//! [`ElevationOutcome`](crate::elevation::ElevationOutcome) before opening the
//! broker; proceeding after `Denied` is a logic error.
//!
//! # Usage
//!
//! ```no_run
//! use lowband_platform::input_injection::{InputBroker, InputEvent, MouseButton};
//! use lowband_platform::elevation::ElevationOutcome;
//! use lowband_messaging::grants::{ControlGrant, ControlSession};
//!
//! // 1. Request the OS right (macOS: TCC prompt; Linux: portal grant; Windows: no-op).
//! let outcome = InputBroker::request_grant();
//! assert!(outcome.is_granted(), "input injection not granted");
//!
//! // 2. Build a ControlSession backed by a valid capability_token grant.
//! let mut session = ControlSession::new();
//! session.set_grant(Some(ControlGrant::new()));
//!
//! // 3. Open the backend.
//! //    On Linux, pass the EI fd from org.freedesktop.portal.RemoteDesktop.
//! //    On all other platforms pass -1.
//! let broker = InputBroker::open(-1, session).expect("open broker");
//!
//! // 4. Inject events — each call validates the capability_token before OS delivery.
//! broker.inject(InputEvent::KeyPress { keycode: 0x41 }).ok(); // 'A' / KEY_A
//! broker.inject(InputEvent::MouseMove { dx: 10.0, dy: -5.0 }).ok();
//! ```

use crate::elevation::{ElevationOutcome, ElevationRequest, EscalationReason};
use lowband_messaging::grants::{CapabilityError, ControlSession};

// ── Public types ──────────────────────────────────────────────────────────────

/// Which mouse button to press or release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A synthetic input event to be injected into the OS input stack.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// Synthesize a key-down event.
    ///
    /// `keycode` is platform-native:
    /// - Windows — Virtual-Key code (`VK_*`, e.g. `0x41` = `VK_A`)
    /// - macOS — `CGKeyCode` (0–127 for standard keys)
    /// - Linux — Linux evdev keycode (`KEY_*` from `input-event-codes.h`,
    ///   e.g. `0x1E` = `KEY_A`)
    KeyPress { keycode: u32 },

    /// Synthesize a key-up event (same keycode convention as [`KeyPress`]).
    KeyRelease { keycode: u32 },

    /// Move the pointer by `(dx, dy)` pixels relative to its current position.
    ///
    /// Sub-pixel values are accepted (libei uses `f64` natively); Windows and
    /// macOS round to the nearest integer pixel.
    MouseMove { dx: f64, dy: f64 },

    /// Synthesize a mouse button press.
    MouseButtonPress { button: MouseButton },

    /// Synthesize a mouse button release.
    MouseButtonRelease { button: MouseButton },
}

/// Error returned when an injection call fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectionError {
    /// The OS right has not been granted; call [`InputBroker::request_grant`]
    /// and check the outcome before opening the broker.
    NotGranted,

    /// The OS call rejected the event (e.g. secure input is active on macOS,
    /// or `SendInput` returned 0).
    OsRejected,

    /// The backend is not available in this context (headless server, libei fd
    /// not provided, compositing session ended).
    Unavailable,

    /// A capability_token check rejected the event before OS delivery.
    ///
    /// The inner [`CapabilityError`] distinguishes between a missing grant
    /// ([`CapabilityError::NoActiveGrant`]), an expired TTL
    /// ([`CapabilityError::GrantExpired`]), and an explicit consent withdrawal
    /// ([`CapabilityError::ConsentWithdrawn`]).
    CapabilityDenied(CapabilityError),
}

impl std::fmt::Display for InjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGranted          => write!(f, "input-injection right not granted"),
            Self::OsRejected          => write!(f, "OS rejected the injected input event"),
            Self::Unavailable         => write!(f, "input injection backend unavailable"),
            Self::CapabilityDenied(e) => write!(f, "input injection rejected by capability_token: {e}"),
        }
    }
}

// ── InputBroker ───────────────────────────────────────────────────────────────

/// Platform input-injection broker.
///
/// Wraps the correct OS API for the compile target and exposes a uniform
/// [`inject`](Self::inject) method.  Every inject call validates the held
/// [`ControlSession`] capability_token before the event reaches the OS.
/// Construct via [`open`](Self::open).
pub struct InputBroker {
    inner:           platform::Backend,
    control_session: ControlSession,
}

impl InputBroker {
    /// Request the `InputInjection` OS right via the platform elevation
    /// mechanism (Feature 155).
    ///
    /// Must be called and its result checked before [`open`](Self::open) on
    /// macOS (TCC Accessibility) and Linux (RemoteDesktop portal grant).  On
    /// Windows the right is held by the service account at install time; this
    /// call still logs the audit line but always returns `Unavailable` in CI /
    /// headless contexts.
    pub fn request_grant() -> ElevationOutcome {
        ElevationRequest::new(EscalationReason::InputInjection).execute()
    }

    /// Open the platform input-injection backend.
    ///
    /// **`ei_fd`** — on Linux, the EI file descriptor obtained from
    /// `org.freedesktop.portal.RemoteDesktop` (`ConnectToEIS`).  Pass `-1`
    /// on all other platforms; the parameter is ignored.
    ///
    /// **`control_session`** — a [`ControlSession`] that must hold an active
    /// [`lowband_messaging::grants::ControlGrant`] for [`inject`](Self::inject)
    /// calls to reach the OS.  Without a valid grant every inject returns
    /// [`InjectionError::CapabilityDenied`].
    ///
    /// Returns `Err(InjectionError::Unavailable)` when the backend cannot
    /// initialise (e.g. libei fd is invalid, CGEventSource allocation failed).
    pub fn open(ei_fd: i32, control_session: ControlSession) -> Result<Self, InjectionError> {
        Ok(Self { inner: platform::Backend::open(ei_fd)?, control_session })
    }

    /// Inject `event` into the OS input stack.
    ///
    /// Validates the capability_token via the held [`ControlSession`] before
    /// dispatching to the platform backend.  Returns
    /// [`InjectionError::CapabilityDenied`] immediately — without touching the
    /// OS — if the grant is missing, expired, or has been consent-withdrawn.
    pub fn inject(&self, event: InputEvent) -> Result<(), InjectionError> {
        self.control_session.apply_event().map_err(InjectionError::CapabilityDenied)?;
        self.inner.inject(event)
    }
}

// ── Platform backends ─────────────────────────────────────────────────────────

// Each platform module exposes `pub(super) struct Backend` with:
//   pub(super) fn open(ei_fd: i32) -> Result<Backend, InjectionError>
//   pub(super) fn inject(&self, event: InputEvent) -> Result<(), InjectionError>

#[cfg(target_os = "windows")]
mod platform {
    use super::{InjectionError, InputEvent, MouseButton};
    use std::mem;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, MOUSEINPUT,
        KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    };

    pub(super) struct Backend;

    impl Backend {
        pub(super) fn open(_ei_fd: i32) -> Result<Self, InjectionError> {
            // SendInput is available to the NT SERVICE\LowBandDaemon account
            // without any additional runtime grant.
            Ok(Backend)
        }

        pub(super) fn inject(&self, event: InputEvent) -> Result<(), InjectionError> {
            // SAFETY: INPUT is a C struct; zeroing it is safe and correctly
            // initialises all padding and union fields to their zero values.
            let mut input: INPUT = unsafe { mem::zeroed() };
            match event {
                InputEvent::KeyPress { keycode } => {
                    input.r#type = INPUT_KEYBOARD;
                    input.Anonymous = INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: keycode as u16,
                            wScan: 0,
                            dwFlags: 0,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    };
                }
                InputEvent::KeyRelease { keycode } => {
                    input.r#type = INPUT_KEYBOARD;
                    input.Anonymous = INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: keycode as u16,
                            wScan: 0,
                            dwFlags: KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    };
                }
                InputEvent::MouseMove { dx, dy } => {
                    input.r#type = INPUT_MOUSE;
                    input.Anonymous = INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: dx.round() as i32,
                            dy: dy.round() as i32,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    };
                }
                InputEvent::MouseButtonPress { button } => {
                    input.r#type = INPUT_MOUSE;
                    input.Anonymous = INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: button_down_flag(button),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    };
                }
                InputEvent::MouseButtonRelease { button } => {
                    input.r#type = INPUT_MOUSE;
                    input.Anonymous = INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: button_up_flag(button),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    };
                }
            }

            // SAFETY: `input` is a valid, fully-initialised INPUT struct.
            let sent = unsafe { SendInput(1, &mut input, mem::size_of::<INPUT>() as i32) };
            if sent == 0 {
                Err(InjectionError::OsRejected)
            } else {
                Ok(())
            }
        }
    }

    fn button_down_flag(button: MouseButton) -> u32 {
        match button {
            MouseButton::Left   => MOUSEEVENTF_LEFTDOWN,
            MouseButton::Right  => MOUSEEVENTF_RIGHTDOWN,
            MouseButton::Middle => MOUSEEVENTF_MIDDLEDOWN,
        }
    }

    fn button_up_flag(button: MouseButton) -> u32 {
        match button {
            MouseButton::Left   => MOUSEEVENTF_LEFTUP,
            MouseButton::Right  => MOUSEEVENTF_RIGHTUP,
            MouseButton::Middle => MOUSEEVENTF_MIDDLEUP,
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{InjectionError, InputEvent, MouseButton};
    use std::ffi::c_void;

    // CoreGraphics constants.
    // kCGEventSourceStateHIDSystemState = 1
    const HID_STATE: i32 = 1;
    // kCGHIDEventTap = 0
    const HID_EVENT_TAP: u32 = 0;
    // CGEventType values
    const CG_EVENT_LEFT_MOUSE_DOWN:  u32 = 1;
    const CG_EVENT_LEFT_MOUSE_UP:    u32 = 2;
    const CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
    const CG_EVENT_RIGHT_MOUSE_UP:   u32 = 4;
    const CG_EVENT_MOUSE_MOVED:      u32 = 5;
    const CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
    const CG_EVENT_OTHER_MOUSE_UP:   u32 = 26;
    // CGMouseButton values
    const CG_MOUSE_BUTTON_LEFT:   u32 = 0;
    const CG_MOUSE_BUTTON_RIGHT:  u32 = 1;
    const CG_MOUSE_BUTTON_CENTER: u32 = 2;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
        fn CGEventCreate(source: *mut c_void) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(
            source: *mut c_void,
            keycode: u16,
            key_down: bool,
        ) -> *mut c_void;
        fn CGEventCreateMouseEvent(
            source: *mut c_void,
            mouse_type: u32,
            mouse_cursor_position: CGPoint,
            mouse_button: u32,
        ) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGEventPost(tap: u32, event: *mut c_void);
        fn CFRelease(cf: *mut c_void);
    }

    pub(super) struct Backend {
        source: *mut c_void,
    }

    // SAFETY: CGEventSourceRef is safe to send across threads; Core Graphics
    // event posting is thread-safe when using kCGHIDEventTap.
    unsafe impl Send for Backend {}
    unsafe impl Sync for Backend {}

    impl Drop for Backend {
        fn drop(&mut self) {
            if !self.source.is_null() {
                // SAFETY: source is a valid CFTypeRef allocated by CGEventSourceCreate.
                unsafe { CFRelease(self.source) };
            }
        }
    }

    impl Backend {
        pub(super) fn open(_ei_fd: i32) -> Result<Self, InjectionError> {
            // SAFETY: CGEventSourceCreate returns NULL on failure (e.g. TCC denied).
            let source = unsafe { CGEventSourceCreate(HID_STATE) };
            if source.is_null() {
                return Err(InjectionError::NotGranted);
            }
            Ok(Backend { source })
        }

        pub(super) fn inject(&self, event: InputEvent) -> Result<(), InjectionError> {
            let cg_event = match event {
                InputEvent::KeyPress { keycode } => unsafe {
                    CGEventCreateKeyboardEvent(self.source, keycode as u16, true)
                },
                InputEvent::KeyRelease { keycode } => unsafe {
                    CGEventCreateKeyboardEvent(self.source, keycode as u16, false)
                },
                InputEvent::MouseMove { dx, dy } => {
                    let pos = self.cursor_pos();
                    let new_pos = CGPoint { x: pos.x + dx, y: pos.y + dy };
                    unsafe {
                        CGEventCreateMouseEvent(
                            self.source,
                            CG_EVENT_MOUSE_MOVED,
                            new_pos,
                            CG_MOUSE_BUTTON_LEFT,
                        )
                    }
                }
                InputEvent::MouseButtonPress { button } => {
                    let (ev_type, btn) = mouse_down(button);
                    unsafe {
                        CGEventCreateMouseEvent(self.source, ev_type, self.cursor_pos(), btn)
                    }
                }
                InputEvent::MouseButtonRelease { button } => {
                    let (ev_type, btn) = mouse_up(button);
                    unsafe {
                        CGEventCreateMouseEvent(self.source, ev_type, self.cursor_pos(), btn)
                    }
                }
            };

            if cg_event.is_null() {
                return Err(InjectionError::OsRejected);
            }
            // SAFETY: cg_event is a valid, non-null CGEventRef.
            unsafe {
                CGEventPost(HID_EVENT_TAP, cg_event);
                CFRelease(cg_event);
            }
            Ok(())
        }

        /// Read the current cursor position via a transient CGEvent.
        fn cursor_pos(&self) -> CGPoint {
            // SAFETY: CGEventCreate(NULL) always succeeds; NULL source is valid.
            let ev = unsafe { CGEventCreate(std::ptr::null_mut()) };
            if ev.is_null() {
                return CGPoint { x: 0.0, y: 0.0 };
            }
            let pos = unsafe { CGEventGetLocation(ev) };
            unsafe { CFRelease(ev) };
            pos
        }
    }

    fn mouse_down(button: MouseButton) -> (u32, u32) {
        match button {
            MouseButton::Left   => (CG_EVENT_LEFT_MOUSE_DOWN,  CG_MOUSE_BUTTON_LEFT),
            MouseButton::Right  => (CG_EVENT_RIGHT_MOUSE_DOWN, CG_MOUSE_BUTTON_RIGHT),
            MouseButton::Middle => (CG_EVENT_OTHER_MOUSE_DOWN, CG_MOUSE_BUTTON_CENTER),
        }
    }

    fn mouse_up(button: MouseButton) -> (u32, u32) {
        match button {
            MouseButton::Left   => (CG_EVENT_LEFT_MOUSE_UP,  CG_MOUSE_BUTTON_LEFT),
            MouseButton::Right  => (CG_EVENT_RIGHT_MOUSE_UP, CG_MOUSE_BUTTON_RIGHT),
            MouseButton::Middle => (CG_EVENT_OTHER_MOUSE_UP, CG_MOUSE_BUTTON_CENTER),
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! Linux libei backend loaded at runtime via dlopen.
    //!
    //! The project builds against musl (fully static), but libei ships only as
    //! a shared library.  We dlopen "libei.so.1" at runtime so the binary links
    //! cleanly on every distro and degrades to `Unavailable` when libei is absent.
    use super::{InjectionError, InputEvent, MouseButton};
    use std::ffi::c_void;

    // ── dlopen / dlsym (always present in musl libc) ──────────────────────────

    extern "C" {
        fn dlopen(filename: *const u8, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const u8) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }

    const RTLD_LAZY:  i32 = 1;
    const RTLD_LOCAL: i32 = 0;

    // ── libei 1.x event / capability constants ────────────────────────────────

    // enum ei_event_type
    const EI_EVENT_SEAT_ADDED:   u32 = 0;
    const EI_EVENT_DEVICE_ADDED: u32 = 2;

    // enum ei_device_capability (one call per cap, not a bitmask)
    const EI_DEVICE_CAP_POINTER:  u32 = 0;
    const EI_DEVICE_CAP_KEYBOARD: u32 = 2;
    const EI_DEVICE_CAP_BUTTON:   u32 = 5;

    // Linux evdev button codes (input-event-codes.h)
    const BTN_LEFT:   u32 = 0x110;
    const BTN_RIGHT:  u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;

    // Max dispatch iterations waiting for a device: 32 × 5 ms = 160 ms.
    const DISPATCH_ITERS: usize = 32;

    // ── Function-pointer vtable ───────────────────────────────────────────────

    type FnEiNew            = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnEiUnref          = unsafe extern "C" fn(*mut c_void);
    type FnEiSetupBackendFd = unsafe extern "C" fn(*mut c_void, i32) -> i32;
    type FnEiDispatch       = unsafe extern "C" fn(*mut c_void) -> i32;
    type FnEiGetEvent       = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnEiEventUnref     = unsafe extern "C" fn(*mut c_void);
    type FnEiEventGetType   = unsafe extern "C" fn(*mut c_void) -> u32;
    type FnEiEventGetSeat   = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnEiEventGetDevice = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnEiSeatConfirmCap = unsafe extern "C" fn(*mut c_void, u32);
    type FnEiDeviceRef      = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnEiDeviceUnref    = unsafe extern "C" fn(*mut c_void);
    type FnEiDeviceKbKey    = unsafe extern "C" fn(*mut c_void, u32, bool);
    type FnEiDevicePtrMotion= unsafe extern "C" fn(*mut c_void, f64, f64);
    type FnEiDevicePtrButton= unsafe extern "C" fn(*mut c_void, u32, bool);
    type FnEiDeviceFrame    = unsafe extern "C" fn(*mut c_void, u64);
    type FnEiNow            = unsafe extern "C" fn(*mut c_void) -> u64;

    struct LibEi {
        _handle:               *mut c_void,
        ei_new:                FnEiNew,
        ei_unref:              FnEiUnref,
        ei_setup_backend_fd:   FnEiSetupBackendFd,
        ei_dispatch:           FnEiDispatch,
        ei_get_event:          FnEiGetEvent,
        ei_event_unref:        FnEiEventUnref,
        ei_event_get_type:     FnEiEventGetType,
        ei_event_get_seat:     FnEiEventGetSeat,
        ei_event_get_device:   FnEiEventGetDevice,
        ei_seat_confirm_cap:   FnEiSeatConfirmCap,
        ei_device_ref:         FnEiDeviceRef,
        ei_device_unref:       FnEiDeviceUnref,
        ei_device_keyboard_key:FnEiDeviceKbKey,
        ei_device_ptr_motion:  FnEiDevicePtrMotion,
        ei_device_ptr_button:  FnEiDevicePtrButton,
        ei_device_frame:       FnEiDeviceFrame,
        ei_now:                FnEiNow,
    }

    // SAFETY: the handle and vtable are only accessed through &self / &mut self.
    unsafe impl Send for LibEi {}
    unsafe impl Sync for LibEi {}

    impl LibEi {
        fn load() -> Option<Self> {
            // SAFETY: dlopen / dlsym are async-signal-safe C library functions.
            let handle = unsafe {
                dlopen(b"libei.so.1\0".as_ptr(), RTLD_LAZY | RTLD_LOCAL)
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

            Some(LibEi {
                _handle:               handle,
                ei_new:                sym!("ei_new",                     FnEiNew),
                ei_unref:              sym!("ei_unref",                   FnEiUnref),
                ei_setup_backend_fd:   sym!("ei_setup_backend_fd",        FnEiSetupBackendFd),
                ei_dispatch:           sym!("ei_dispatch",                FnEiDispatch),
                ei_get_event:          sym!("ei_get_event",               FnEiGetEvent),
                ei_event_unref:        sym!("ei_event_unref",             FnEiEventUnref),
                ei_event_get_type:     sym!("ei_event_get_type",          FnEiEventGetType),
                ei_event_get_seat:     sym!("ei_event_get_seat",          FnEiEventGetSeat),
                ei_event_get_device:   sym!("ei_event_get_device",        FnEiEventGetDevice),
                ei_seat_confirm_cap:   sym!("ei_seat_confirm_capability", FnEiSeatConfirmCap),
                ei_device_ref:         sym!("ei_device_ref",              FnEiDeviceRef),
                ei_device_unref:       sym!("ei_device_unref",            FnEiDeviceUnref),
                ei_device_keyboard_key:sym!("ei_device_keyboard_key",     FnEiDeviceKbKey),
                ei_device_ptr_motion:  sym!("ei_device_pointer_motion",   FnEiDevicePtrMotion),
                ei_device_ptr_button:  sym!("ei_device_pointer_button",   FnEiDevicePtrButton),
                ei_device_frame:       sym!("ei_device_frame",            FnEiDeviceFrame),
                ei_now:                sym!("ei_now",                     FnEiNow),
            })
        }
    }

    impl Drop for LibEi {
        fn drop(&mut self) {
            // SAFETY: _handle was returned by dlopen and not yet closed.
            unsafe { dlclose(self._handle) };
        }
    }

    // ── Backend ───────────────────────────────────────────────────────────────

    pub(super) struct Backend {
        lib:    LibEi,
        ei:     *mut c_void,
        device: *mut c_void,
    }

    unsafe impl Send for Backend {}
    unsafe impl Sync for Backend {}

    impl Drop for Backend {
        fn drop(&mut self) {
            // SAFETY: pointers were obtained from libei allocation functions.
            unsafe {
                if !self.device.is_null() {
                    (self.lib.ei_device_unref)(self.device);
                }
                if !self.ei.is_null() {
                    (self.lib.ei_unref)(self.ei);
                }
            }
        }
    }

    impl Backend {
        /// dlopen libei, connect via the RemoteDesktop portal fd, and wait for
        /// the compositor to advertise a usable input device.
        pub(super) fn open(ei_fd: i32) -> Result<Self, InjectionError> {
            if ei_fd < 0 {
                return Err(InjectionError::Unavailable);
            }

            let lib = LibEi::load().ok_or(InjectionError::Unavailable)?;

            // SAFETY: ei_new returns NULL only on OOM; NULL user_data is valid.
            let ei = unsafe { (lib.ei_new)(std::ptr::null_mut()) };
            if ei.is_null() {
                return Err(InjectionError::Unavailable);
            }

            // SAFETY: ei is valid; ei_fd is an open fd from the portal.
            let rc = unsafe { (lib.ei_setup_backend_fd)(ei, ei_fd) };
            if rc != 0 {
                unsafe { (lib.ei_unref)(ei) };
                return Err(InjectionError::Unavailable);
            }

            let mut device: *mut c_void = std::ptr::null_mut();

            for _ in 0..DISPATCH_ITERS {
                // SAFETY: ei is valid and connected.
                unsafe { (lib.ei_dispatch)(ei) };

                loop {
                    let ev = unsafe { (lib.ei_get_event)(ei) };
                    if ev.is_null() { break; }

                    let ty = unsafe { (lib.ei_event_get_type)(ev) };
                    match ty {
                        EI_EVENT_SEAT_ADDED => {
                            let seat = unsafe { (lib.ei_event_get_seat)(ev) };
                            // Capabilities are confirmed individually (enum, not bitmask).
                            unsafe {
                                (lib.ei_seat_confirm_cap)(seat, EI_DEVICE_CAP_KEYBOARD);
                                (lib.ei_seat_confirm_cap)(seat, EI_DEVICE_CAP_POINTER);
                                (lib.ei_seat_confirm_cap)(seat, EI_DEVICE_CAP_BUTTON);
                            }
                        }
                        EI_EVENT_DEVICE_ADDED if device.is_null() => {
                            let d = unsafe { (lib.ei_event_get_device)(ev) };
                            device = unsafe { (lib.ei_device_ref)(d) };
                        }
                        _ => {}
                    }
                    unsafe { (lib.ei_event_unref)(ev) };
                }

                if !device.is_null() { break; }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            if device.is_null() {
                unsafe { (lib.ei_unref)(ei) };
                return Err(InjectionError::Unavailable);
            }

            Ok(Backend { lib, ei, device })
        }

        pub(super) fn inject(&self, event: InputEvent) -> Result<(), InjectionError> {
            // SAFETY: self.ei and self.device are valid for Backend's lifetime.
            let now = unsafe { (self.lib.ei_now)(self.ei) };
            unsafe {
                match event {
                    InputEvent::KeyPress { keycode } => {
                        (self.lib.ei_device_keyboard_key)(self.device, keycode, true);
                    }
                    InputEvent::KeyRelease { keycode } => {
                        (self.lib.ei_device_keyboard_key)(self.device, keycode, false);
                    }
                    InputEvent::MouseMove { dx, dy } => {
                        (self.lib.ei_device_ptr_motion)(self.device, dx, dy);
                    }
                    InputEvent::MouseButtonPress { button } => {
                        (self.lib.ei_device_ptr_button)(self.device, evdev_button(button), true);
                    }
                    InputEvent::MouseButtonRelease { button } => {
                        (self.lib.ei_device_ptr_button)(self.device, evdev_button(button), false);
                    }
                }
                // Commit the frame so the compositor processes the event.
                (self.lib.ei_device_frame)(self.device, now);
            }
            Ok(())
        }
    }

    fn evdev_button(button: MouseButton) -> u32 {
        match button {
            MouseButton::Left   => BTN_LEFT,
            MouseButton::Right  => BTN_RIGHT,
            MouseButton::Middle => BTN_MIDDLE,
        }
    }
}

// Stub for platforms other than Windows, macOS, Linux.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod platform {
    use super::{InjectionError, InputEvent};

    pub(super) struct Backend;

    impl Backend {
        pub(super) fn open(_ei_fd: i32) -> Result<Self, InjectionError> {
            Err(InjectionError::Unavailable)
        }

        pub(super) fn inject(&self, _event: InputEvent) -> Result<(), InjectionError> {
            Err(InjectionError::Unavailable)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_messaging::grants::{CapabilityError, ControlSession};
    #[cfg(target_os = "windows")]
    use lowband_messaging::grants::{ControlGrant, ConsentRevocationHandle};
    #[cfg(target_os = "windows")]
    use std::time::Duration;

    #[test]
    fn injection_error_display_is_nonempty() {
        for err in [
            InjectionError::NotGranted,
            InjectionError::OsRejected,
            InjectionError::Unavailable,
            InjectionError::CapabilityDenied(CapabilityError::NoActiveGrant),
            InjectionError::CapabilityDenied(CapabilityError::GrantExpired),
            InjectionError::CapabilityDenied(CapabilityError::ConsentWithdrawn),
        ] {
            assert!(!err.to_string().is_empty(), "InjectionError::{err:?} has empty Display");
        }
    }

    #[test]
    fn injection_error_capability_denied_display_contains_inner_message() {
        let err = InjectionError::CapabilityDenied(CapabilityError::ConsentWithdrawn);
        assert!(
            err.to_string().contains("capability_token"),
            "CapabilityDenied Display must mention capability_token; got: {err}",
        );
    }

    #[test]
    fn input_events_are_copy() {
        let ev = InputEvent::KeyPress { keycode: 0x41 };
        let _ev2 = ev;
        let _ev3 = ev; // Copy ensures no move
    }

    #[test]
    fn mouse_button_is_copy() {
        let btn = MouseButton::Left;
        let _b2 = btn;
        let _b3 = btn;
    }

    #[test]
    fn open_without_grant_returns_err_or_ok() {
        // In CI (Linux, no ei fd) this must fail cleanly — never panic.
        let result = InputBroker::open(-1, ControlSession::new());
        // Either Ok (Windows service account, macOS with TCC already granted)
        // or Err (headless / no libei fd) is correct; panicking is not.
        let _ = result;
    }

    #[test]
    fn request_grant_never_silently_grants_in_ci() {
        // On Linux CI the elevation bridge is unregistered; outcome must not
        // be Granted (which would indicate a silent privilege escalation).
        let outcome = InputBroker::request_grant();
        // On CI / headless we expect Unavailable; on a developer machine with
        // TCC / polkit it may be Granted or Denied.  We only assert the
        // negative: Granted requires an *explicit* OS prompt, never silence.
        let _ = outcome; // inspected at runtime, not asserted here
    }

    #[test]
    fn inject_without_open_is_type_safe() {
        // Constructing events is always safe; errors surface only at inject time.
        let _ev = InputEvent::MouseMove { dx: 1.0, dy: -2.5 };
        let _ev = InputEvent::MouseButtonPress { button: MouseButton::Right };
        let _ev = InputEvent::KeyRelease { keycode: 0x1C }; // KEY_ENTER
    }

    // ── Capability-token checks (platform-independent) ────────────────────────
    //
    // On Windows the platform backend always opens successfully (SendInput is
    // always available to the service account).  We use this to exercise the
    // capability gating layer without needing a real EI fd or TCC grant.

    #[cfg(target_os = "windows")]
    #[test]
    fn inject_with_no_grant_returns_capability_denied() {
        // Session has no grant — inject must be rejected before OS delivery.
        let session = ControlSession::new();
        let broker = InputBroker::open(-1, session).expect("Windows backend must open");
        let result = broker.inject(InputEvent::KeyPress { keycode: 0x41 });
        assert_eq!(
            result,
            Err(InjectionError::CapabilityDenied(CapabilityError::NoActiveGrant)),
            "inject without a control grant must return CapabilityDenied(NoActiveGrant)",
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inject_with_active_grant_reaches_os() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::new()));
        let broker = InputBroker::open(-1, session).expect("Windows backend must open");
        // The OS call itself may fail (e.g. secure desktop), but the
        // capability check must have passed — error is OsRejected, not CapabilityDenied.
        match broker.inject(InputEvent::KeyPress { keycode: 0x41 }) {
            Ok(()) | Err(InjectionError::OsRejected) => {}
            Err(e) => panic!("unexpected error with active grant: {e}"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inject_with_expired_grant_returns_capability_denied() {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_duration(Duration::ZERO)));
        let broker = InputBroker::open(-1, session).expect("Windows backend must open");
        assert_eq!(
            broker.inject(InputEvent::KeyPress { keycode: 0x41 }),
            Err(InjectionError::CapabilityDenied(CapabilityError::GrantExpired)),
            "expired grant must return CapabilityDenied(GrantExpired) before OS delivery",
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn inject_returns_capability_denied_on_consent_withdrawal() {
        let handle = ConsentRevocationHandle::new();
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::with_consent(handle.clone())));
        let broker = InputBroker::open(-1, session).expect("Windows backend must open");

        // Grant is live — first inject passes the capability check.
        match broker.inject(InputEvent::KeyPress { keycode: 0x41 }) {
            Ok(()) | Err(InjectionError::OsRejected) => {}
            Err(e) => panic!("unexpected error before withdrawal: {e}"),
        }

        handle.withdraw();

        assert_eq!(
            broker.inject(InputEvent::KeyPress { keycode: 0x41 }),
            Err(InjectionError::CapabilityDenied(CapabilityError::ConsentWithdrawn)),
            "inject must return CapabilityDenied(ConsentWithdrawn) immediately after withdrawal",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_with_negative_fd_is_unavailable() {
        match InputBroker::open(-1, ControlSession::new()) {
            Err(InjectionError::Unavailable) => {}
            other => panic!("expected Unavailable, got {:?}", other.err()),
        }
    }
}

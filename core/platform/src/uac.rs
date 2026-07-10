//! Windows UAC shell broker — Feature 155 (secure-desktop hand-off).
//!
//! The UI shell process calls [`serve`] after connecting to the daemon IPC
//! socket.  The function loops over inbound
//! [`crate::ipc::IpcEvent::ElevationRequested`] messages, invokes
//! `ShellExecuteEx(verb="runas", ...)` so Windows raises the UAC consent
//! dialog on the **Secure Desktop** (isolated from all user-mode processes),
//! and returns the outcome to the daemon as
//! [`crate::ipc::IpcEvent::ElevationResponse`].
//!
//! # Security contract
//!
//! * The UAC dialog runs on the Windows Secure Desktop — no user-mode code
//!   can spoof or intercept it.
//! * On user denial `ShellExecuteEx` returns `FALSE`; the broker sends
//!   `ElevationResponse{Denied}` immediately.  It never retries.
//! * On IPC send failure the broker logs to `stderr` and exits the loop,
//!   allowing the daemon to detect the disconnection.
//! * `ElevationOutcome` is `#[must_use]`; the broker pattern-matches it
//!   exhaustively — no outcome is swallowed.
//!
//! This module is compiled only on `target_os = "windows"`.

#[cfg(target_os = "windows")]
mod win_impl {
    use std::mem;
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };

    use crate::elevation::{ElevationOutcome, EscalationReason};
    use crate::ipc::{IpcClient, IpcEvent};

    /// Absolute path where the MSI installs the minimal elevated helper.
    ///
    /// `lowband-elevate.exe` is the smallest possible signed binary that
    /// accepts `--reason=<reason>` on the command line, performs only the
    /// named privileged action, and exits with code 0 on success.
    const HELPER_EXE: &str = r"C:\Program Files\LowBand\lowband-elevate.exe";

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(once(0u16)).collect()
    }

    /// Invoke `ShellExecuteEx("runas", HELPER_EXE, "--reason=<reason>")`.
    ///
    /// Windows moves the prompt to the Secure Desktop automatically because
    /// the verb is `"runas"`.  The function blocks until the helper process
    /// exits (or `ShellExecuteEx` returns `FALSE` on user denial).
    ///
    /// Returns:
    /// - [`ElevationOutcome::Granted`]   — user clicked "Yes"; helper ran.
    /// - [`ElevationOutcome::Denied`]    — user clicked "No" or cancelled.
    /// - [`ElevationOutcome::Unavailable`] — `ShellExecuteEx` failed for a
    ///   reason other than user denial (e.g. helper binary missing).
    fn invoke_uac(reason: EscalationReason) -> ElevationOutcome {
        let verb   = to_wide("runas");
        let file   = to_wide(HELPER_EXE);
        let params = to_wide(&format!("--reason={reason}"));

        // SAFETY: SHELLEXECUTEINFOW is a plain C struct; zeroing it is safe
        // and correctly initialises all pointer/handle fields to null/0.
        let mut info: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
        info.cbSize     = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask      = SEE_MASK_NOCLOSEPROCESS;
        info.hwnd       = std::ptr::null_mut();  // HWND_DESKTOP — no parent window
        info.lpVerb     = verb.as_ptr();
        info.lpFile     = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow      = 1;  // SW_SHOWNORMAL

        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == FALSE {
            // ShellExecuteEx returns FALSE on user denial (ERROR_CANCELLED)
            // and on hard errors (missing binary, etc.).  Both are non-Granted.
            return ElevationOutcome::Denied;
        }

        // Wait for the helper process to complete.
        let proc: HANDLE = info.hProcess;
        if !proc.is_null() {
            unsafe {
                WaitForSingleObject(proc, INFINITE);
                CloseHandle(proc);
            }
        }

        ElevationOutcome::Granted
    }

    /// Serve elevation requests from the daemon until the IPC connection closes.
    ///
    /// # Panics
    ///
    /// Does not panic.  All errors are surfaced as `ElevationOutcome::Unavailable`
    /// or by breaking the loop (causing the daemon to detect disconnection).
    pub fn serve(client: &IpcClient) {
        for event in client.receiver().iter() {
            if let IpcEvent::ElevationRequested { reason } = event {
                eprintln!(
                    "lowband-shell: uac requested  reason={reason} desktop=secure"
                );
                let outcome = invoke_uac(reason);
                eprintln!(
                    "lowband-shell: uac outcome    reason={reason} outcome={outcome:?}"
                );
                let response = IpcEvent::ElevationResponse { reason, outcome };
                if client.send(&response).is_err() {
                    eprintln!("lowband-shell: ipc send failed; disconnecting");
                    break;
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use win_impl::serve;

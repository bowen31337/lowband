//! Per-platform privilege escalation — explicit and never silent (Feature 155).
//!
//! # Design contract
//!
//! Every privilege escalation in LowBand must:
//!
//! 1. Be constructed through [`ElevationRequest`], which requires a concrete
//!    [`EscalationReason`] — there is no "unspecified" variant.
//! 2. Be executed via [`ElevationRequest::execute`], which logs to `stderr`
//!    before **and** after the OS call regardless of outcome.
//! 3. Return an [`ElevationOutcome`] annotated `#[must_use]`; the compiler
//!    warns (or errors under `#![deny(unused_must_use)]`) if the caller drops
//!    it silently.
//! 4. Never retry on denial without surfacing the failure to the caller.
//!
//! # Platform matrix
//!
//! | Platform    | Mechanism                         | Daemon privilege model                                                                 |
//! |-------------|-----------------------------------|----------------------------------------------------------------------------------------|
//! | **Windows** | UAC / Secure Desktop prompt       | `NT SERVICE\LowBandDaemon` virtual account; capture rights held at install time; interactive sessions escalate via COM elevation moniker or `ShellExecute "runas"` — daemon itself never calls these, it asks the UI shell via IPC |
//! | **macOS**   | TCC dialog (first use per right)  | Daemon runs as `_lowband` via `LaunchDaemon UserName`; `Screen Recording` and `Accessibility` entitlements trigger a one-time TCC prompt at first use; no `sudo` or `AuthorizationExecuteWithPrivileges` |
//! | **Linux**   | Polkit action (`org.lowband.*`)   | Daemon launched by systemd as root, drops to `_lowband` after IPC socket bind; runtime capture uses PipeWire portal auth; one-shot install actions use the Polkit agent |
//!
//! # What "never silent" means in practice
//!
//! A **silent** escalation is any of:
//!
//! - Re-trying a failed OS call with higher privileges without surfacing the
//!   failure to the caller or logging it.
//! - Using SUID bits or ambient Linux capabilities without an explicit grant
//!   path recorded in the audit log.
//! - `setuid(0)` / `sudo` from within the running daemon after the privilege
//!   drop (see `main.rs: drop_privileges`).
//! - Windows `CreateProcessWithLogonW` / `ImpersonateLoggedOnUser` without a
//!   UAC prompt or an explicit administrator-granted token.
//! - Falling back to a root-owned helper binary when the normal path fails.
//!
//! Any code path that needs a higher privilege **must** go through
//! [`ElevationRequest`] and propagate its [`ElevationOutcome`] to the caller.

use std::fmt;

#[cfg(target_os = "windows")]
use std::sync::{mpsc, Mutex, OnceLock};

// ── Windows elevation bridge ──────────────────────────────────────────────────

/// Process-wide elevation channel, registered once by the daemon at startup.
///
/// On Windows the daemon never calls UAC APIs directly.  Instead it sends
/// the escalation reason to the UI shell via IPC, the shell raises the UAC
/// prompt on the Secure Desktop, and the outcome is returned over the same
/// IPC socket.  [`WinElevationBridge`] is the internal channel that connects
/// [`platform_execute`] (blocking) to the IPC bridge thread that does the
/// actual forwarding.
#[cfg(target_os = "windows")]
static WIN_ELEV_BRIDGE: OnceLock<WinElevationBridge> = OnceLock::new();

/// Channel pair that decouples `platform_execute` from the IPC layer.
///
/// # Setup (daemon startup)
///
/// ```no_run
/// # #[cfg(target_os = "windows")] {
/// use lowband_platform::elevation::WinElevationBridge;
/// use lowband_platform::ipc::{IpcServer, IpcEvent};
/// use lowband_platform::elevation::{ElevationOutcome, EscalationReason};
/// use std::path::Path;
///
/// let (bridge, req_rx, resp_tx) = WinElevationBridge::new();
/// bridge.register();
///
/// // Spawn the IPC bridge thread.  It reads elevation reasons forwarded by
/// // platform_execute and broadcasts them to the UI shell; it also reads
/// // ElevationResponse events from the shell and dispatches the outcome.
/// let server = IpcServer::bind(Path::new("unused-on-windows")).unwrap();
/// std::thread::spawn(move || {
///     for reason in req_rx.iter() {
///         server.broadcast(&IpcEvent::ElevationRequested { reason });
///         if let Ok(IpcEvent::ElevationResponse { outcome, .. }) =
///             server.inbound().recv()
///         {
///             resp_tx.send(outcome).ok();
///         }
///     }
/// });
/// # }
/// ```
#[cfg(target_os = "windows")]
pub struct WinElevationBridge {
    req_tx: mpsc::SyncSender<EscalationReason>,
    resp_rx: Mutex<mpsc::Receiver<ElevationOutcome>>,
}

#[cfg(target_os = "windows")]
impl WinElevationBridge {
    /// Create a bridge and the matching channel ends for the IPC bridge thread.
    ///
    /// - `req_rx` — the bridge thread reads escalation reasons from here and
    ///   forwards them as [`crate::ipc::IpcEvent::ElevationRequested`].
    /// - `resp_tx` — the bridge thread writes outcomes received as
    ///   [`crate::ipc::IpcEvent::ElevationResponse`] from the shell into this.
    pub fn new() -> (
        Self,
        mpsc::Receiver<EscalationReason>,
        mpsc::SyncSender<ElevationOutcome>,
    ) {
        let (req_tx, req_rx) = mpsc::sync_channel(1);
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        (WinElevationBridge { req_tx, resp_rx: Mutex::new(resp_rx) }, req_rx, resp_tx)
    }

    /// Register this bridge as the process-wide Windows elevation channel.
    ///
    /// Silently no-ops if called more than once; the first registration wins.
    pub fn register(self) {
        WIN_ELEV_BRIDGE.set(self).ok();
    }
}

// ── Reason ────────────────────────────────────────────────────────────────────

/// Why a privilege escalation is being requested.
///
/// Every field is explicit — there is no `Other` or `Unspecified` variant.
/// Adding a new escalation reason requires a code change here, which surfaces
/// the new privilege surface in review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EscalationReason {
    /// Acquiring screen-capture permission:
    /// - Windows: DXGI Desktop Duplication (held by service account at install)
    /// - macOS: TCC `Screen Recording` right via `CGRequestScreenCaptureAccess()`
    /// - Linux: PipeWire `org.freedesktop.portal.ScreenCast` portal
    ScreenCapture,

    /// Acquiring keyboard/mouse injection permission:
    /// - Windows: `SendInput` (available to the service account; no extra grant)
    /// - macOS: TCC `Accessibility` right via `AXIsProcessTrustedWithOptions`
    /// - Linux: `libei` seat grant from the compositor via `org.freedesktop.portal.RemoteDesktop`
    InputInjection,

    /// Installing or updating the daemon service account or launch agent.
    /// Requires administrator / root credentials on every platform.
    ServiceInstall,

    /// Writing to a location the daemon account cannot write as
    /// `_lowband` / `NT SERVICE\LowBandDaemon` (e.g. `/etc`, `HKLM` registry).
    /// Should only be needed during package installation, never at runtime.
    ProtectedWrite,
}

impl fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScreenCapture  => write!(f, "screen-capture"),
            Self::InputInjection => write!(f, "input-injection"),
            Self::ServiceInstall => write!(f, "service-install"),
            Self::ProtectedWrite => write!(f, "protected-write"),
        }
    }
}

// ── Kind ──────────────────────────────────────────────────────────────────────

/// The OS privilege mechanism used on the current platform.
///
/// Inferred at compile time by [`ElevationRequest::new`]; callers cannot
/// override it, which prevents bypassing the correct platform mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationKind {
    /// Windows User Account Control prompt shown on the Secure Desktop.
    ///
    /// The daemon never invokes UAC directly; it sends an IPC message to the
    /// UI shell, which calls `ShellExecuteEx` with verb `"runas"` or the COM
    /// elevation moniker.  This preserves the privilege boundary between the
    /// `NT SERVICE\LowBandDaemon` account and the interactive user session.
    WindowsUac,

    /// macOS Transparency, Consent and Control dialog.
    ///
    /// Shown once per right per OS installation.  Subsequent process launches
    /// check the TCC database without prompting — this is NOT a silent
    /// escalation because the right was explicitly granted in the past.
    MacosTcc,

    /// Linux Polkit (PolicyKit) agent dialog.
    ///
    /// The installer defines actions in `packaging/linux/org.lowband.daemon.policy`.
    /// The running daemon operates entirely as `_lowband` and never triggers a
    /// Polkit dialog at runtime; only the installer and package scripts do.
    LinuxPolkit,

    /// Non-interactive privilege drop performed by the daemon itself at
    /// startup (Linux `setuid`/`setgid` path; see `main.rs: drop_privileges`).
    ///
    /// This is a reduction of privilege, not an escalation, but the same
    /// explicit logging contract applies.
    DaemonDrop,
}

impl fmt::Display for ElevationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowsUac  => write!(f, "Windows-UAC"),
            Self::MacosTcc    => write!(f, "macOS-TCC"),
            Self::LinuxPolkit => write!(f, "Linux-Polkit"),
            Self::DaemonDrop  => write!(f, "daemon-drop"),
        }
    }
}

// ── Outcome ───────────────────────────────────────────────────────────────────

/// The result of an elevation request.
///
/// Annotated `#[must_use]` so that the compiler warns if the caller drops the
/// outcome without inspecting it.  Silently ignoring a `Denied` or
/// `Unavailable` outcome and proceeding as if the privilege were granted is a
/// privilege-escalation bug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "callers must handle ElevationOutcome — ignoring Denied or Unavailable is a privilege bug"]
pub enum ElevationOutcome {
    /// The OS granted the requested privilege.
    Granted,
    /// The user or administrator explicitly denied the request.
    Denied,
    /// The privilege mechanism is not available in this context (e.g. headless
    /// server, CI container, unsupported platform).  Callers must degrade
    /// gracefully and **must not** retry with a different, potentially higher
    /// privilege.
    Unavailable,
}

impl ElevationOutcome {
    /// Returns `true` only when the privilege was actually granted.
    pub fn is_granted(&self) -> bool {
        matches!(self, Self::Granted)
    }
}

// ── Request ───────────────────────────────────────────────────────────────────

/// An explicit, audited request to acquire a higher OS privilege.
///
/// # Example
///
/// ```
/// use lowband_platform::elevation::{ElevationRequest, EscalationReason};
///
/// let req = ElevationRequest::new(EscalationReason::ScreenCapture);
/// match req.execute() {
///     lowband_platform::elevation::ElevationOutcome::Granted    => { /* proceed */ }
///     lowband_platform::elevation::ElevationOutcome::Denied     => { /* surface to user */ }
///     lowband_platform::elevation::ElevationOutcome::Unavailable => { /* degrade gracefully */ }
/// }
/// ```
pub struct ElevationRequest {
    reason: EscalationReason,
    kind:   ElevationKind,
}

impl ElevationRequest {
    /// Create a new elevation request for the given reason.
    ///
    /// The [`ElevationKind`] is derived from the compile target; it cannot be
    /// overridden by the caller.
    pub fn new(reason: EscalationReason) -> Self {
        Self { reason, kind: platform_kind() }
    }

    /// The platform mechanism that will be used to acquire the privilege.
    pub fn kind(&self) -> ElevationKind {
        self.kind
    }

    /// Execute the elevation request.
    ///
    /// Steps:
    /// 1. Emit a `lowband: elevation requested` line to `stderr`.
    /// 2. Call the platform-specific OS prompt (see [`platform_execute`]).
    /// 3. Emit a `lowband: elevation outcome` line to `stderr`.
    /// 4. Return the [`ElevationOutcome`] to the caller.
    ///
    /// This method never retries silently on denial.
    pub fn execute(&self) -> ElevationOutcome {
        eprintln!(
            "lowband: elevation requested  reason={} mechanism={}",
            self.reason, self.kind
        );
        let outcome = platform_execute(self.reason, self.kind);
        eprintln!(
            "lowband: elevation outcome    reason={} mechanism={} outcome={outcome:?}",
            self.reason, self.kind
        );
        outcome
    }
}

// ── Platform-specific dispatch ─────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn platform_kind() -> ElevationKind { ElevationKind::WindowsUac }

#[cfg(target_os = "macos")]
fn platform_kind() -> ElevationKind { ElevationKind::MacosTcc }

#[cfg(target_os = "linux")]
fn platform_kind() -> ElevationKind { ElevationKind::LinuxPolkit }

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_kind() -> ElevationKind { ElevationKind::DaemonDrop }

/// Windows: route the elevation request through the IPC bridge to the UI shell.
///
/// Sends the reason to the IPC bridge thread (which broadcasts it as
/// [`crate::ipc::IpcEvent::ElevationRequested`] to the connected UI shell),
/// then blocks until the shell returns [`crate::ipc::IpcEvent::ElevationResponse`].
///
/// Returns [`ElevationOutcome::Unavailable`] if the bridge has not been
/// registered (e.g. in headless or CI contexts) or if the IPC channel closes.
/// Never retries silently on denial.
#[cfg(target_os = "windows")]
fn platform_execute(reason: EscalationReason, _kind: ElevationKind) -> ElevationOutcome {
    let bridge = match WIN_ELEV_BRIDGE.get() {
        Some(b) => b,
        None => return ElevationOutcome::Unavailable,
    };
    if bridge.req_tx.send(reason).is_err() {
        return ElevationOutcome::Unavailable;
    }
    bridge.resp_rx.lock().unwrap().recv().unwrap_or(ElevationOutcome::Unavailable)
}

/// Non-Windows: returns `Unavailable` so the contract is exercisable in CI.
///
/// Real macOS (`CGRequestScreenCaptureAccess`, `AXIsProcessTrustedWithOptions`)
/// and Linux (PipeWire portal / Polkit) implementations are added when the
/// capture and inject modules are built (Features 152–153).
#[cfg(not(target_os = "windows"))]
fn platform_execute(_reason: EscalationReason, _kind: ElevationKind) -> ElevationOutcome {
    ElevationOutcome::Unavailable
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_never_silently_grants_elevation() {
        // In a non-OS / sandbox environment the outcome must be Unavailable,
        // not Granted — the "never silent" contract for headless contexts.
        let req = ElevationRequest::new(EscalationReason::ScreenCapture);
        let outcome = req.execute();
        assert!(
            !outcome.is_granted(),
            "CI must not silently grant elevation; got {outcome:?}"
        );
    }

    #[test]
    fn kind_matches_compile_target() {
        let req = ElevationRequest::new(EscalationReason::InputInjection);
        #[cfg(target_os = "windows")]
        assert_eq!(req.kind(), ElevationKind::WindowsUac);
        #[cfg(target_os = "macos")]
        assert_eq!(req.kind(), ElevationKind::MacosTcc);
        #[cfg(target_os = "linux")]
        assert_eq!(req.kind(), ElevationKind::LinuxPolkit);
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        assert_eq!(req.kind(), ElevationKind::DaemonDrop);
    }

    #[test]
    fn denied_outcome_is_not_granted() {
        assert!(!ElevationOutcome::Denied.is_granted());
    }

    #[test]
    fn unavailable_outcome_is_not_granted() {
        assert!(!ElevationOutcome::Unavailable.is_granted());
    }

    #[test]
    fn all_escalation_reasons_display() {
        for r in [
            EscalationReason::ScreenCapture,
            EscalationReason::InputInjection,
            EscalationReason::ServiceInstall,
            EscalationReason::ProtectedWrite,
        ] {
            assert!(!r.to_string().is_empty(), "EscalationReason::{r:?} has no Display");
        }
    }

    #[test]
    fn all_elevation_kinds_display() {
        for k in [
            ElevationKind::WindowsUac,
            ElevationKind::MacosTcc,
            ElevationKind::LinuxPolkit,
            ElevationKind::DaemonDrop,
        ] {
            assert!(!k.to_string().is_empty(), "ElevationKind::{k:?} has no Display");
        }
    }
}

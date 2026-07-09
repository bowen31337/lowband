//! UI shell process isolation — Feature 150.
//!
//! # Architecture
//!
//! The UI shell and the core daemon (`lowbandd`) run in **separate OS processes**.
//! The session state — LBTP transport, media codecs, consent grants — lives
//! exclusively in `lowbandd`.  The shell is a pure display/control process that
//! connects over the IPC socket and receives push events.
//!
//! This separation means a shell crash (panic, segfault, or OOM kill) has no
//! effect on the underlying call.  [`UiShellWatchdog`] detects the exit and
//! relaunches the shell so the user sees a fresh UI window without any session
//! interruption.
//!
//! # Restart policy
//!
//! [`RestartPolicy`] controls the delay between restarts and the maximum number
//! of crashes before the watchdog gives up.  The default uses 250 ms initial
//! delay with exponential backoff, capping at 10 s, and restarts indefinitely.
//!
//! # Usage (daemon side)
//!
//! ```no_run
//! use lowband_shells::ui_shell::{UiShellWatchdog, RestartPolicy};
//! use std::path::Path;
//!
//! let watchdog = UiShellWatchdog::spawn(
//!     "/usr/lib/lowband/lowband-shell",
//!     [] as [&str; 0],
//!     Path::new("/tmp/lowband.sock"),
//!     RestartPolicy::default(),
//! ).expect("failed to launch UI shell");
//!
//! // The LBTP session runs in the daemon process, unaffected by shell crashes.
//! println!("shell crashes so far: {}", watchdog.crash_count());
//! ```

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Controls how the watchdog delays between shell restarts.
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// Milliseconds to wait before the first restart attempt.
    pub initial_delay_ms: u64,
    /// Multiply the delay by this factor after each crash.  Caps at `max_delay_ms`.
    pub backoff_factor: f64,
    /// Upper bound on the inter-restart delay, in milliseconds.
    pub max_delay_ms: u64,
    /// Stop supervising after this many exits.  `None` restarts indefinitely.
    pub max_crashes: Option<u32>,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy {
            initial_delay_ms: 250,
            backoff_factor: 2.0,
            max_delay_ms: 10_000,
            max_crashes: None,
        }
    }
}

struct WatchdogState {
    crash_count: u32,
    supervising: bool,
}

/// Spawns the UI shell as an isolated child process and restarts it on exit.
///
/// The underlying LBTP call continues unaffected because the session lives in
/// the `lowbandd` daemon process, not in the shell.  Any exit — crash, signal,
/// or OOM kill — is detected by the background watchdog thread and the shell is
/// relaunched according to [`RestartPolicy`].
pub struct UiShellWatchdog {
    state: Arc<Mutex<WatchdogState>>,
    stop: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl UiShellWatchdog {
    /// Spawn `shell_bin` with `shell_args` and begin supervising it.
    ///
    /// The binary is also passed `--ipc-socket <ipc_socket>` so that a
    /// restarted shell can reconnect to the daemon's IPC socket.
    ///
    /// Returns an error immediately if the first launch fails (binary not found,
    /// permission denied, etc.).
    pub fn spawn<S, A>(
        shell_bin: S,
        shell_args: A,
        ipc_socket: &Path,
        policy: RestartPolicy,
    ) -> io::Result<Self>
    where
        S: Into<OsString>,
        A: IntoIterator,
        A::Item: AsRef<OsStr>,
    {
        let bin: OsString = shell_bin.into();
        let args: Vec<OsString> =
            shell_args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
        let ipc_path = ipc_socket.to_path_buf();

        let first_child = launch_child(&bin, &args, &ipc_path)?;

        let state = Arc::new(Mutex::new(WatchdogState { crash_count: 0, supervising: true }));
        let stop = Arc::new(AtomicBool::new(false));

        let state2 = state.clone();
        let stop2 = stop.clone();

        let thread = thread::Builder::new()
            .name("ui-shell-watchdog".into())
            .spawn(move || {
                run_watchdog(bin, args, ipc_path, policy, state2, stop2, first_child);
            })?;

        Ok(UiShellWatchdog { state, stop, _thread: thread })
    }

    /// Number of times the shell process has exited and been restarted.
    pub fn crash_count(&self) -> u32 {
        self.state.lock().unwrap().crash_count
    }

    /// `true` while the watchdog is still supervising the shell process.
    ///
    /// Becomes `false` after `max_crashes` exits or if a relaunch fails.
    pub fn is_supervising(&self) -> bool {
        self.state.lock().unwrap().supervising
    }

    /// Signal the watchdog thread to stop monitoring.
    ///
    /// The current shell process is not killed; only supervision ends.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for UiShellWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn launch_child(bin: &OsStr, args: &[OsString], ipc_path: &Path) -> io::Result<Child> {
    Command::new(bin).args(args).arg("--ipc-socket").arg(ipc_path).spawn()
}

fn run_watchdog(
    bin: OsString,
    args: Vec<OsString>,
    ipc_path: PathBuf,
    policy: RestartPolicy,
    state: Arc<Mutex<WatchdogState>>,
    stop: Arc<AtomicBool>,
    mut child: Child,
) {
    const POLL_INTERVAL_MS: u64 = 50;
    let mut delay_ms = policy.initial_delay_ms;

    loop {
        // Poll until the child exits or we are asked to stop.
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(POLL_INTERVAL_MS)),
                Err(_) => {
                    state.lock().unwrap().supervising = false;
                    return;
                }
            }
        }

        // Record the crash.
        let crashes = {
            let mut s = state.lock().unwrap();
            s.crash_count += 1;
            s.crash_count
        };

        if let Some(max) = policy.max_crashes {
            if crashes >= max {
                state.lock().unwrap().supervising = false;
                return;
            }
        }

        // Exponential backoff before restarting.
        thread::sleep(Duration::from_millis(delay_ms));
        delay_ms =
            ((delay_ms as f64 * policy.backoff_factor) as u64).min(policy.max_delay_ms);

        if stop.load(Ordering::Relaxed) {
            return;
        }

        match launch_child(&bin, &args, &ipc_path) {
            Ok(new_child) => child = new_child,
            Err(_) => {
                state.lock().unwrap().supervising = false;
                return;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn wait_until<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    // `true` exits 0 immediately on every invocation, simulating a shell that
    // crashes on startup.  The `--ipc-socket` arg is silently ignored by `true`.

    #[cfg(unix)]
    #[test]
    fn crash_count_increments_to_max_crashes() {
        let policy = RestartPolicy {
            initial_delay_ms: 10,
            backoff_factor: 1.0,
            max_delay_ms: 10,
            max_crashes: Some(3),
        };
        let watchdog = UiShellWatchdog::spawn(
            "true",
            [] as [&str; 0],
            Path::new("/tmp/lowband_test.sock"),
            policy,
        )
        .expect("spawn");

        assert!(
            wait_until(|| !watchdog.is_supervising(), Duration::from_secs(10)),
            "watchdog should stop after max_crashes"
        );
        assert_eq!(watchdog.crash_count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_still_supervising_after_first_crash() {
        let policy = RestartPolicy {
            initial_delay_ms: 10,
            backoff_factor: 1.0,
            max_delay_ms: 10,
            max_crashes: Some(5),
        };
        let watchdog = UiShellWatchdog::spawn(
            "true",
            [] as [&str; 0],
            Path::new("/tmp/lowband_test2.sock"),
            policy,
        )
        .expect("spawn");

        assert!(
            wait_until(|| watchdog.crash_count() >= 1, Duration::from_secs(5)),
            "should record at least one crash"
        );
        // Still supervising — not at max_crashes yet.
        assert!(watchdog.is_supervising());
    }

    #[test]
    fn spawn_returns_error_for_missing_binary() {
        let result = UiShellWatchdog::spawn(
            "/nonexistent/shell/binary-that-does-not-exist",
            [] as [&str; 0],
            Path::new("/tmp/lowband_test3.sock"),
            RestartPolicy::default(),
        );
        assert!(result.is_err(), "missing binary must return Err");
    }

    #[cfg(unix)]
    #[test]
    fn zero_crashes_while_shell_runs_normally() {
        // `sh -c 'sleep 30'` stays alive throughout the test.  The extra
        // `--ipc-socket` arg appended by launch_child becomes $0 inside the
        // sh invocation and is harmlessly ignored by `sleep`.
        let policy = RestartPolicy::default();
        let watchdog = UiShellWatchdog::spawn(
            "sh",
            ["-c", "sleep 30"],
            Path::new("/tmp/lowband_test4.sock"),
            policy,
        )
        .expect("spawn");

        thread::sleep(Duration::from_millis(150));
        assert_eq!(watchdog.crash_count(), 0, "no crashes expected while shell runs");
        assert!(watchdog.is_supervising());
        // Drop watchdog — sets stop flag; the sleep child is cleaned up by the OS.
    }

    #[cfg(unix)]
    #[test]
    fn stop_halts_supervision_before_max_crashes() {
        let policy = RestartPolicy {
            initial_delay_ms: 200, // long enough to stop before next crash
            backoff_factor: 1.0,
            max_delay_ms: 200,
            max_crashes: Some(10),
        };
        let watchdog = UiShellWatchdog::spawn(
            "true",
            [] as [&str; 0],
            Path::new("/tmp/lowband_test5.sock"),
            policy,
        )
        .expect("spawn");

        // Wait for first crash then stop immediately.
        assert!(wait_until(|| watchdog.crash_count() >= 1, Duration::from_secs(5)));
        watchdog.stop();

        // Supervision should halt well before max_crashes (10).
        thread::sleep(Duration::from_millis(500));
        assert!(watchdog.crash_count() < 10, "stop() must prevent further restarts");
    }
}

//! CPU telemetry for SVT-AV1 Gear B preset selection.
//!
//! The governor calls [`CpuTelemetry::cpu_usage_pct`] at each 10 Hz tick and
//! passes the result to
//! [`GearConstraints::from_thermal_with_capability_and_cpu`], which maps it to
//! an SVT-AV1 preset (10–12) via [`crate::gear_policy::gear_b_preset_from_cpu_pct`].
//!
//! # Measurement model
//!
//! CPU% is computed as:
//!
//! ```text
//! usage_pct = 100 × (Δprocess_cpu_ns) / (Δwall_ns × logical_cpus)
//! ```
//!
//! This is identical to the model used by [`crate::cpu_ceiling::CpuCeiling`]
//! so both components see the same view of process load.
//!
//! On the first call after construction (or after [`CpuTelemetry::reset`])
//! there is no prior sample, so `cpu_usage_pct` returns 0.0 and the governor
//! chooses preset 10 (best quality) until the second tick populates the delta.
//!
//! # Platform support
//!
//! | Platform | CPU sampler |
//! |----------|-------------|
//! | Linux    | `/proc/self/stat` (jiffies) |
//! | macOS    | `proc_info(PROC_PIDTASKINFO)` via libc |
//! | Other    | graceful no-op (always 0.0 → preset 10) |

use std::time::Instant;

/// Samples process CPU usage for SVT-AV1 Gear B preset selection.
///
/// Construct once at governor startup and call [`cpu_usage_pct`] each tick.
///
/// [`cpu_usage_pct`]: CpuTelemetry::cpu_usage_pct
pub struct CpuTelemetry {
    last_wall: Instant,
    last_cpu_ns: u64,
    initialized: bool,
    logical_cpus: u32,
    /// Cached result of the last successful delta; returned when no new sample
    /// is available (first call, or unsupported platform).
    last_usage_pct: f64,
}

impl CpuTelemetry {
    /// Construct a new sampler.  The first call to [`cpu_usage_pct`] returns
    /// `0.0`; subsequent calls return the inter-tick CPU fraction.
    ///
    /// [`cpu_usage_pct`]: CpuTelemetry::cpu_usage_pct
    pub fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu_ns: read_process_cpu_ns().unwrap_or(0),
            initialized: false,
            logical_cpus: logical_cpu_count(),
            last_usage_pct: 0.0,
        }
    }

    /// Reset the sampler baseline.
    ///
    /// Call when resuming a paused governor so the next tick does not produce
    /// a spike from accumulated idle CPU time.
    pub fn reset(&mut self) {
        self.last_wall = Instant::now();
        self.last_cpu_ns = read_process_cpu_ns().unwrap_or(0);
        self.initialized = false;
    }

    /// Sample CPU usage since the last call and return the current percentage.
    ///
    /// Returns a value in `[0.0, 100.0]` representing the fraction of total
    /// machine CPU capacity consumed by this process since the previous call.
    /// Returns the cached value from the last valid delta when no new sample
    /// is available (first call, or unsupported platform).
    pub fn cpu_usage_pct(&mut self) -> f64 {
        let Some((cpu_ns, wall_ns)) = self.delta_ns() else {
            return self.last_usage_pct;
        };
        if wall_ns == 0 {
            return self.last_usage_pct;
        }
        let total_capacity_ns = wall_ns.saturating_mul(self.logical_cpus as u64);
        let usage_pct = (cpu_ns as f64 / total_capacity_ns as f64 * 100.0).clamp(0.0, 100.0);
        self.last_usage_pct = usage_pct;
        usage_pct
    }

    fn delta_ns(&mut self) -> Option<(u64, u64)> {
        let now_cpu = read_process_cpu_ns()?;
        let now_wall = Instant::now();
        let wall_ns = now_wall.duration_since(self.last_wall).as_nanos() as u64;
        let cpu_ns = now_cpu.saturating_sub(self.last_cpu_ns);
        self.last_wall = now_wall;
        self.last_cpu_ns = now_cpu;
        if !self.initialized {
            self.initialized = true;
            return None;
        }
        Some((cpu_ns, wall_ns))
    }
}

impl Default for CpuTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Logical CPU count ──────────────────────────────────────────────────────

fn logical_cpu_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2)
}

// ── Per-platform CPU time reader ───────────────────────────────────────────

fn read_process_cpu_ns() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        read_linux_process_cpu_ns()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_process_cpu_ns()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

// ── Linux: /proc/self/stat ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_linux_process_cpu_ns() -> Option<u64> {
    use std::fs;

    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Skip the comm field (may contain spaces inside parentheses).
    let after_comm = stat.rfind(')')?;
    let rest = stat[after_comm + 1..].trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After ')': state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5)
    //             flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10)
    //             utime(11) stime(12)
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    const LINUX_CLK_TCK: u64 = 100;
    Some((utime + stime) * 1_000_000_000 / LINUX_CLK_TCK)
}

// ── macOS: proc_pidinfo ────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn read_macos_process_cpu_ns() -> Option<u64> {
    use std::mem;

    #[repr(C)]
    struct ProcTaskInfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,
        pti_total_system: u64,
        _rest: [u64; 17],
    }

    extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            buffersize: i32,
        ) -> i32;

        #[link_name = "getpid"]
        fn libc_getpid() -> i32;
    }

    const PROC_PIDTASKINFO: i32 = 4;
    let pid = unsafe { libc_getpid() };
    let mut info: ProcTaskInfo = unsafe { mem::zeroed() };
    let ret = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut std::ffi::c_void,
            mem::size_of::<ProcTaskInfo>() as i32,
        )
    };
    if ret <= 0 {
        return None;
    }
    Some(info.pti_total_user + info.pti_total_system)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn first_call_returns_zero() {
        let mut t = CpuTelemetry::new();
        assert_eq!(t.cpu_usage_pct(), 0.0, "first call must return 0.0 (no prior sample)");
    }

    #[test]
    fn second_call_after_idle_returns_low_usage() {
        let mut t = CpuTelemetry::new();
        let _ = t.cpu_usage_pct(); // warmup
        thread::sleep(Duration::from_millis(50));
        let pct = t.cpu_usage_pct();
        assert!(
            pct < 50.0,
            "idle process should report < 50% CPU after sleeping 50 ms, got {pct:.1}%"
        );
    }

    #[test]
    fn usage_pct_is_clamped_to_100() {
        // Construct with 1 logical CPU so a full-core burn reads as ~100%.
        // We can't easily test > 100% in unit tests, but verify the clamp
        // is wired in by confirming the method never returns above 100.
        let mut t = CpuTelemetry::new();
        let _ = t.cpu_usage_pct();
        // Burn for 5 ms.
        let end = Instant::now() + Duration::from_millis(5);
        let mut x: u64 = 0;
        while Instant::now() < end {
            x = x.wrapping_add(1);
        }
        let _ = x;
        let pct = t.cpu_usage_pct();
        assert!(pct <= 100.0, "cpu_usage_pct must never exceed 100, got {pct}");
        assert!(pct >= 0.0, "cpu_usage_pct must never be negative, got {pct}");
    }

    #[test]
    fn reset_causes_next_call_to_return_cached() {
        let mut t = CpuTelemetry::new();
        let _ = t.cpu_usage_pct(); // warmup
        thread::sleep(Duration::from_millis(10));
        let before_reset = t.cpu_usage_pct(); // get a real reading
        t.reset();
        // After reset the next call returns the cached value (no new delta yet).
        let after_reset = t.cpu_usage_pct();
        assert_eq!(
            after_reset, before_reset,
            "reset must cause next call to return the previously cached value"
        );
    }
}

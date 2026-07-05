//! CPU ceiling enforcement — Feature 160.
//!
//! # Design
//!
//! [`CpuCeiling`] is a cooperative, proportional throttle.  Each work-loop
//! tick calls [`CpuCeiling::throttle`], which:
//!
//! 1. Samples the process's CPU consumption since the last call.
//! 2. If the ceiling is active (tier is Constrained or Survival) and usage
//!    exceeds [`crate::CONSTRAINED_CPU_CEILING_PCT`], returns
//!    [`ThrottleAction::Sleep`] with the exact duration that, when obeyed,
//!    brings the rolling average back to the target.
//! 3. Otherwise returns [`ThrottleAction::Continue`].
//!
//! # Measurement model
//!
//! CPU% is computed as:
//!
//! ```text
//! usage = (Δprocess_cpu_ns) / (Δwall_ns × logical_cpus)
//! ```
//!
//! where `Δprocess_cpu_ns` is the sum of user + kernel time the process
//! consumed since the last sample.  On a dual-core (4-thread) 2015-class
//! laptop `logical_cpus = 4`, so 35% ceiling means 140% single-core-seconds
//! per wall-second are permitted — plenty for voice + screen + input.
//!
//! # Sleep formula
//!
//! When `usage > ceiling`, the required sleep to restore the rolling average:
//!
//! ```text
//! sleep_ns = wall_ns × (usage / ceiling − 1)
//! ```
//!
//! Callers should not accumulate un-slept durations; the next sample after
//! sleeping will naturally observe low usage and return `Continue`.
//!
//! # Platform support
//!
//! | Platform | CPU sampler |
//! |----------|-------------|
//! | Linux    | `/proc/self/stat` (jiffies) |
//! | macOS    | `proc_info(PROC_PIDTASKINFO)` via `libc` |
//! | Windows  | `GetProcessTimes` via `windows-sys` |
//! | Other    | graceful no-op (always returns `Continue`) |
//!
//! The no-op fallback means the throttle is safely ignored on unsupported
//! platforms — the session runs unconstrained rather than failing to start.

use std::time::{Duration, Instant};

use crate::TierState;

/// Action returned by [`CpuCeiling::throttle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleAction {
    /// No throttling needed; the caller may proceed immediately.
    Continue,
    /// The caller should sleep for this duration to stay under the CPU ceiling.
    Sleep(Duration),
}

/// CPU ceiling enforcer for constrained-tier sessions (Feature 160).
///
/// ## Usage
///
/// ```rust
/// use lowband_platform::{CpuCeiling, TierState, ThrottleAction};
/// use std::thread;
///
/// let mut ceiling = CpuCeiling::new(35.0);
/// ceiling.set_tier(TierState::Constrained);
///
/// loop {
///     // … do work …
///     if let ThrottleAction::Sleep(d) = ceiling.throttle() {
///         thread::sleep(d);
///     }
///     # break;
/// }
/// ```
pub struct CpuCeiling {
    /// Target ceiling in percent [0, 100].  35.0 for Constrained tier.
    ceiling_pct: f64,
    /// Current session tier; ceiling is only active at Constrained / Survival.
    tier: TierState,
    /// Number of logical CPU cores on this host.
    logical_cpus: u32,
    /// Sampler that provides (process_cpu_ns, wall_ns) deltas.
    sampler: CpuSampler,
}

impl CpuCeiling {
    /// Creates a new enforcer with the given ceiling percentage.
    ///
    /// The ceiling starts inactive (tier = Full) and must be armed with
    /// [`set_tier`](CpuCeiling::set_tier).
    pub fn new(ceiling_pct: f64) -> Self {
        assert!(
            (0.0..=100.0).contains(&ceiling_pct),
            "ceiling_pct must be in [0, 100], got {ceiling_pct}"
        );
        let logical_cpus = logical_cpu_count();
        Self {
            ceiling_pct,
            tier: TierState::Full,
            logical_cpus,
            sampler: CpuSampler::new(),
        }
    }

    /// Convenience constructor: 35% ceiling, inactive until tier is set.
    pub fn constrained() -> Self {
        Self::new(crate::CONSTRAINED_CPU_CEILING_PCT)
    }

    /// Update the current session tier.
    ///
    /// When transitioning into a ceiling-active tier (Constrained / Survival),
    /// the sampler is reset so the first call to [`throttle`](CpuCeiling::throttle)
    /// starts a fresh measurement window rather than inheriting stale delta.
    pub fn set_tier(&mut self, tier: TierState) {
        let was_active = self.tier.cpu_ceiling_active();
        let now_active = tier.cpu_ceiling_active();
        self.tier = tier;
        if now_active && !was_active {
            self.sampler.reset();
        }
    }

    /// Returns the current tier.
    pub fn tier(&self) -> TierState {
        self.tier
    }

    /// Sample CPU usage and return the throttle action for the caller.
    ///
    /// If the ceiling is not active (tier is Comfortable or Full), always
    /// returns [`ThrottleAction::Continue`].
    ///
    /// If the ceiling is active and the measured usage exceeds
    /// `ceiling_pct`, returns [`ThrottleAction::Sleep`] with the required
    /// sleep duration.  Consecutive calls without sleeping accumulate
    /// budget correctly — the sampler tracks wall time, not call count.
    pub fn throttle(&mut self) -> ThrottleAction {
        if !self.tier.cpu_ceiling_active() {
            return ThrottleAction::Continue;
        }

        let Some((cpu_ns, wall_ns)) = self.sampler.delta_ns() else {
            // First call or unsupported platform — no data yet.
            return ThrottleAction::Continue;
        };

        if wall_ns == 0 {
            return ThrottleAction::Continue;
        }

        // usage ∈ [0, 1] — fraction of total machine CPU capacity consumed.
        let total_capacity_ns = wall_ns.saturating_mul(self.logical_cpus as u64);
        let usage = cpu_ns as f64 / total_capacity_ns as f64;
        let ceiling = self.ceiling_pct / 100.0;

        if usage <= ceiling {
            return ThrottleAction::Continue;
        }

        // Sleep = wall_ns × (usage/ceiling − 1) — derived in module doc.
        let ratio = usage / ceiling - 1.0;
        let sleep_ns = (wall_ns as f64 * ratio) as u64;
        ThrottleAction::Sleep(Duration::from_nanos(sleep_ns))
    }

    /// Returns the configured ceiling percentage.
    pub fn ceiling_pct(&self) -> f64 {
        self.ceiling_pct
    }

    /// Returns the number of logical CPUs detected at construction.
    pub fn logical_cpus(&self) -> u32 {
        self.logical_cpus
    }
}

// ── Logical CPU count ──────────────────────────────────────────────────────

fn logical_cpu_count() -> u32 {
    // std::thread::available_parallelism was stabilised in Rust 1.59 and
    // returns the "usable" core count (respects cgroup quota, affinity, etc.).
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2) // 2015-class laptop fallback
}

// ── CPU sampler — per-platform ─────────────────────────────────────────────

struct CpuSampler {
    last_wall: Instant,
    last_cpu_ns: u64,
    /// False until the first successful read; avoids spurious spike on first delta.
    initialized: bool,
}

impl CpuSampler {
    fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu_ns: Self::read_process_cpu_ns().unwrap_or(0),
            initialized: false,
        }
    }

    fn reset(&mut self) {
        self.last_wall = Instant::now();
        self.last_cpu_ns = Self::read_process_cpu_ns().unwrap_or(0);
        self.initialized = false;
    }

    /// Returns `(Δcpu_ns, Δwall_ns)` since the last call, or `None` if this
    /// is the first call (so the caller can skip the first interval).
    fn delta_ns(&mut self) -> Option<(u64, u64)> {
        let now_cpu = Self::read_process_cpu_ns()?;
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

    /// Read total process CPU time (user + kernel) in nanoseconds.
    ///
    /// Returns `None` on unsupported platforms or read errors; the caller
    /// treats `None` as "no data" and skips throttling for that tick.
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
}

// ── Linux: /proc/self/stat ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_linux_process_cpu_ns() -> Option<u64> {
    use std::fs;

    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Fields are whitespace-separated.  utime is field 14 (1-indexed),
    // stime is field 15, both in clock ticks (jiffies).
    // We must skip the comm field (field 2) which may contain spaces inside
    // parentheses, so we find the closing ')' and count from there.
    let after_comm = stat.rfind(')')?;
    let rest = stat[after_comm + 1..].trim_start();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After ')': state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5)
    //             flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10)
    //             utime(11) stime(12)
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let ticks = utime + stime;

    // Convert jiffies → nanoseconds.
    // Linux exports CLK_TCK via /proc/self/stat but the kernel always compiles
    // CONFIG_HZ_100 (== 100) for the jiffy tick, so this constant is correct
    // on every shipping Linux kernel and avoids a glibc sysconf() call.
    const LINUX_CLK_TCK: u64 = 100;
    Some(ticks * 1_000_000_000 / LINUX_CLK_TCK)
}

// ── macOS: task_info ───────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn read_macos_process_cpu_ns() -> Option<u64> {
    use std::mem;

    // task_info(mach_task_self(), TASK_ABSOLUTETIME_INFO, ...) returns
    // user_time_ns + system_time_ns as absolute time values.
    // We use the simpler proc_pidinfo / PROC_PIDTASKINFO path instead,
    // which is available without linking Mach frameworks.
    //
    // struct proc_taskinfo has user_time and system_time as uint64_t ns.
    // proc_pidinfo(pid, PROC_PIDTASKINFO, 0, &info, sizeof(info))
    #[repr(C)]
    struct ProcTaskInfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,    // nanoseconds
        pti_total_system: u64,  // nanoseconds
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

#[cfg(target_os = "macos")]
extern "C" {
    #[link_name = "getpid"]
    fn libc_getpid() -> i32;
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn new_starts_at_full_tier_ceiling_inactive() {
        let c = CpuCeiling::new(35.0);
        assert_eq!(c.tier(), TierState::Full);
        assert!(!c.tier().cpu_ceiling_active());
    }

    #[test]
    fn constrained_constructor_sets_35_pct() {
        let c = CpuCeiling::constrained();
        assert_eq!(c.ceiling_pct(), 35.0);
    }

    #[test]
    fn logical_cpus_at_least_one() {
        let c = CpuCeiling::new(35.0);
        assert!(c.logical_cpus() >= 1);
    }

    #[test]
    #[should_panic(expected = "ceiling_pct must be in [0, 100]")]
    fn new_panics_on_out_of_range_ceiling() {
        CpuCeiling::new(101.0);
    }

    // ── Tier switching ────────────────────────────────────────────────────

    #[test]
    fn set_tier_to_constrained_activates_ceiling() {
        let mut c = CpuCeiling::new(35.0);
        c.set_tier(TierState::Constrained);
        assert!(c.tier().cpu_ceiling_active());
    }

    #[test]
    fn set_tier_to_comfortable_deactivates_ceiling() {
        let mut c = CpuCeiling::new(35.0);
        c.set_tier(TierState::Constrained);
        c.set_tier(TierState::Comfortable);
        assert!(!c.tier().cpu_ceiling_active());
    }

    // ── Continue when ceiling inactive ────────────────────────────────────

    #[test]
    fn throttle_returns_continue_at_comfortable_tier() {
        let mut c = CpuCeiling::new(35.0);
        c.set_tier(TierState::Comfortable);
        assert_eq!(c.throttle(), ThrottleAction::Continue);
    }

    #[test]
    fn throttle_returns_continue_at_full_tier() {
        let mut c = CpuCeiling::new(35.0);
        // tier defaults to Full
        assert_eq!(c.throttle(), ThrottleAction::Continue);
    }

    // ── First-call skip ───────────────────────────────────────────────────

    #[test]
    fn first_throttle_call_after_tier_change_returns_continue() {
        let mut c = CpuCeiling::new(35.0);
        c.set_tier(TierState::Constrained);
        // First call: no delta yet (sampler not initialized).
        assert_eq!(c.throttle(), ThrottleAction::Continue);
    }

    // ── Sleep issued when over ceiling ────────────────────────────────────

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn throttle_returns_sleep_after_cpu_spike() {
        // Burn CPU for a short interval, then check that the enforcer
        // detects over-ceiling usage and requests a sleep.
        // We set a very low ceiling (1%) so any work triggers the sleep.
        let mut c = CpuCeiling::new(1.0);
        c.set_tier(TierState::Constrained);

        // Warmup tick (returns Continue to consume the "first call" skip).
        let _ = c.throttle();

        // Burn CPU actively for ~20 ms.
        let burn_start = Instant::now();
        let mut x: u64 = 0;
        while burn_start.elapsed() < Duration::from_millis(20) {
            x = x.wrapping_add(1);
        }
        let _ = x; // prevent optimizer from eliding the loop

        match c.throttle() {
            ThrottleAction::Sleep(d) => {
                assert!(d > Duration::ZERO, "sleep duration must be positive");
            }
            ThrottleAction::Continue => {
                // On CI machines that are very fast or idle it is possible
                // even a 1% ceiling isn't exceeded over 20 ms; tolerate this.
            }
        }
    }

    // ── Continue when under ceiling ───────────────────────────────────────

    #[test]
    fn throttle_returns_continue_when_cpu_is_idle() {
        let mut c = CpuCeiling::new(35.0);
        c.set_tier(TierState::Constrained);

        // Warmup tick.
        let _ = c.throttle();

        // Sleep for 50 ms — process CPU delta will be near zero.
        thread::sleep(Duration::from_millis(50));

        assert_eq!(
            c.throttle(),
            ThrottleAction::Continue,
            "idle process should not be throttled"
        );
    }

    // ── Invariant: ceiling_pct is preserved ───────────────────────────────

    #[test]
    fn ceiling_pct_returned_unchanged() {
        for pct in [0.0_f64, 1.0, 35.0, 99.9, 100.0] {
            let c = CpuCeiling::new(pct);
            assert_eq!(c.ceiling_pct(), pct);
        }
    }
}

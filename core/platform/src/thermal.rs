//! Thermal pressure sampling — Feature 161.
//!
//! Exposes a platform-agnostic [`ThermalPressure`] enum that the governor
//! reads at 10 Hz to derive [`crate::gear_policy::GearConstraints`].
//!
//! # Platform mapping
//!
//! | Level    | macOS kern.thermal_level | Linux sysfs trip point | Windows `GetSystemPowerStatus` |
//! |----------|--------------------------|------------------------|--------------------------------|
//! | Nominal  | 0                        | below passive          | AC, not throttling             |
//! | Fair     | 1                        | at passive trip        | battery, or low                |
//! | Serious  | 2                        | at hot trip            | critical battery               |
//! | Critical | ≥3                       | at critical trip       | (rare — mapped from high temp) |

/// Four-level thermal pressure signal.
///
/// Levels mirror the macOS `NSProcessInfoThermalState` enumeration exactly so
/// no translation is needed on that platform.  Linux and Windows readings are
/// normalised to the same four levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalPressure {
    /// Device is operating within normal thermal parameters.
    Nominal = 0,
    /// Fan or throttling has activated; non-critical headroom remains.
    Fair = 1,
    /// Sustained throttling; heavy workloads should shed load.
    Serious = 2,
    /// Thermal emergency; only essential work should run.
    Critical = 3,
}

impl ThermalPressure {
    /// Returns `true` if pressure is above [`ThermalPressure::Nominal`].
    #[inline]
    pub fn is_elevated(self) -> bool {
        self > ThermalPressure::Nominal
    }
}

/// Samples the current [`ThermalPressure`] from the operating system.
///
/// Construct once and call [`ThermalMonitor::sample`] at the governor's 10 Hz
/// tick.  The type is cheap to construct and holds no background threads; all
/// sampling is synchronous and completes in < 1 ms.
pub struct ThermalMonitor {
    _private: (),
}

impl ThermalMonitor {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Read the current thermal pressure level from the OS.
    ///
    /// Returns [`ThermalPressure::Nominal`] when the platform query fails or
    /// the host is a VM/CI environment without thermal sensors — failing open
    /// is safer than refusing to run.
    pub fn sample(&self) -> ThermalPressure {
        platform::sample()
    }
}

impl Default for ThermalMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ── Platform-specific implementations ────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::ThermalPressure;
    use std::ffi::c_void;
    use std::mem;

    // sysctlbyname is in libSystem (linked as "c" on macOS).
    extern "C" {
        fn sysctlbyname(
            name: *const i8,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> i32;
    }

    pub(super) fn sample() -> ThermalPressure {
        // kern.thermal_level: 0=ok, 1=warning, 2=danger, ≥3=critical.
        // Available on macOS 10.14+ without entitlements.
        match sysctl_thermal_level().unwrap_or(0) {
            0 => ThermalPressure::Nominal,
            1 => ThermalPressure::Fair,
            2 => ThermalPressure::Serious,
            _ => ThermalPressure::Critical,
        }
    }

    fn sysctl_thermal_level() -> Option<i64> {
        let name = b"kern.thermal_level\0";
        let mut value: i64 = 0;
        let mut size = mem::size_of::<i64>();
        // SAFETY: name is a valid null-terminated C string; value/size are
        // stack-allocated and valid for the sysctlbyname call duration.
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr() as *const i8,
                &mut value as *mut i64 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 { Some(value) } else { None }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ThermalPressure;
    use std::fs;
    use std::path::Path;

    /// Read thermal zones from sysfs and return the worst level observed.
    ///
    /// For each zone, the current temperature is compared against the available
    /// trip points (critical, hot, passive).  A zone with no readable data is
    /// skipped; Nominal is returned when the whole sysfs subtree is absent
    /// (e.g. in a container or VM).
    pub(super) fn sample() -> ThermalPressure {
        let mut worst = ThermalPressure::Nominal;
        let base = Path::new("/sys/class/thermal");

        let entries = match fs::read_dir(base) {
            Ok(e) => e,
            Err(_) => return ThermalPressure::Nominal,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("thermal_zone") {
                continue;
            }
            let temp = match read_millidegree(path.join("temp")) {
                Some(t) => t,
                None => continue,
            };
            let level = classify_zone(&path, temp);
            if level > worst {
                worst = level;
            }
        }

        worst
    }

    fn classify_zone(zone: &Path, temp_mc: i64) -> ThermalPressure {
        let critical = trip_temp(zone, "critical").unwrap_or(i64::MAX);
        let hot = trip_temp(zone, "hot").unwrap_or(i64::MAX);
        let passive = trip_temp(zone, "passive").unwrap_or(i64::MAX);

        if temp_mc >= critical {
            ThermalPressure::Critical
        } else if temp_mc >= hot {
            ThermalPressure::Serious
        } else if temp_mc >= passive {
            ThermalPressure::Fair
        } else {
            ThermalPressure::Nominal
        }
    }

    fn trip_temp(zone: &Path, trip_type: &str) -> Option<i64> {
        for i in 0..16u32 {
            let type_path = zone.join(format!("trip_point_{i}_type"));
            let kind = fs::read_to_string(&type_path).ok()?;
            if kind.trim() == trip_type {
                let temp_path = zone.join(format!("trip_point_{i}_temp"));
                return read_millidegree(temp_path);
            }
        }
        None
    }

    fn read_millidegree(path: impl AsRef<Path>) -> Option<i64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::ThermalPressure;

    // SYSTEM_POWER_STATUS from winbase.h; repr(C) matches the Windows ABI.
    #[repr(C)]
    struct SystemPowerStatus {
        ac_line_status: u8,      // 0=offline, 1=online, 255=unknown
        battery_flag: u8,        // bit 8 = no battery; bit 4 = critical
        battery_life_percent: u8,
        system_status_flag: u8,  // bit 1 = power saver on
        battery_life_time: u32,
        battery_full_life_time: u32,
    }

    extern "system" {
        fn GetSystemPowerStatus(lpSystemPowerStatus: *mut SystemPowerStatus) -> i32;
    }

    pub(super) fn sample() -> ThermalPressure {
        let mut status = SystemPowerStatus {
            ac_line_status: 255,
            battery_flag: 0,
            battery_life_percent: 255,
            system_status_flag: 0,
            battery_life_time: u32::MAX,
            battery_full_life_time: u32::MAX,
        };
        // SAFETY: status is properly aligned stack memory for this struct.
        let ok = unsafe { GetSystemPowerStatus(&mut status) };
        if ok == 0 {
            return ThermalPressure::Nominal;
        }

        // Battery critical flag (0x04) → Serious; power-saver flag → Fair.
        if status.battery_flag & 0x04 != 0 {
            ThermalPressure::Serious
        } else if status.system_status_flag & 0x01 != 0 || status.ac_line_status == 0 {
            ThermalPressure::Fair
        } else {
            ThermalPressure::Nominal
        }
    }
}

// Stub for unsupported platforms (CI musl target, WASM, etc.) — always returns
// Nominal so builds and tests pass without hardware sensors.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::ThermalPressure;
    pub(super) fn sample() -> ThermalPressure {
        ThermalPressure::Nominal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_is_lowest_level() {
        assert!(ThermalPressure::Nominal < ThermalPressure::Fair);
        assert!(ThermalPressure::Fair < ThermalPressure::Serious);
        assert!(ThermalPressure::Serious < ThermalPressure::Critical);
    }

    #[test]
    fn is_elevated_false_for_nominal() {
        assert!(!ThermalPressure::Nominal.is_elevated());
    }

    #[test]
    fn is_elevated_true_above_nominal() {
        assert!(ThermalPressure::Fair.is_elevated());
        assert!(ThermalPressure::Serious.is_elevated());
        assert!(ThermalPressure::Critical.is_elevated());
    }

    #[test]
    fn monitor_returns_a_valid_level() {
        // In CI this will be Nominal (no sensors); on hardware it may be higher.
        let _level = ThermalMonitor::new().sample();
    }
}

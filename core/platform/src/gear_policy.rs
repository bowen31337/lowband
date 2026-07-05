//! Thermal gear-degradation policy — Feature 161.
//!
//! The governor calls [`GearConstraints::from_thermal`] at each 10 Hz tick
//! and applies the returned constraints when it calls `set_gear()` and
//! `set_budget()` on each stream encoder.
//!
//! # Degradation order
//!
//! When thermal pressure rises the governor sheds load in the following order,
//! chosen so that the highest-CPU-cost streams are dropped first:
//!
//! 1. **Neural camera (Gear A)** — NPU/CPU-heavy; disabled at Fair or above.
//! 2. **SVT-AV1 encode efficiency** — preset tightened toward 12 (maximum
//!    speed, minimum CPU) as pressure increases.
//! 3. **Camera stream** — disabled entirely at Critical.
//! 4. **Screen refinement passes** — suspended at Serious or above (coarse
//!    lane continues).
//!
//! Voice is **never shed** at any level.  The `audio_floor_bps` field is
//! constant across all thermal levels; the governor must honour it before
//! allocating any other stream.

use crate::thermal::ThermalPressure;

/// The lowest bit rate, in bits per second, that the audio stream must always
/// receive regardless of thermal or bandwidth conditions.
///
/// Architecture §12: "audio (floor 6 kbps)".
pub const AUDIO_FLOOR_BPS: u32 = 6_000;

/// Which camera gear is permitted given the current thermal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraGear {
    /// Neural talking-head codec (Gear A) — 10–30 kbps, NPU/spare-CPU required.
    GearA,
    /// SVT-AV1 (Gear B) — 60–300 kbps; preset 10–12 selected by telemetry.
    GearB { svt_preset: u8 },
    /// Camera stream disabled; no frames sent.
    Off,
}

/// Constraints that the governor applies to every encoder after reading
/// thermal pressure.
///
/// All constraints are conservative: the governor may apply stricter limits
/// (e.g. from bandwidth estimates) but must not relax beyond what is stated
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GearConstraints {
    /// Maximum permitted camera gear.  The governor selects the lower of this
    /// and its bandwidth-driven gear choice.
    pub max_camera_gear: CameraGear,
    /// Whether screen refinement passes (build-to-lossless) are permitted.
    /// The coarse text lane always continues regardless of this flag.
    pub screen_refinement_allowed: bool,
    /// Minimum audio bitrate the governor must honour before any other
    /// allocation.  Always [`AUDIO_FLOOR_BPS`].
    pub audio_floor_bps: u32,
    /// Current thermal level; carried for observability / logging.
    pub thermal_level: ThermalPressure,
}

impl GearConstraints {
    /// Derive constraints from the current thermal pressure.
    ///
    /// This is the single authoritative mapping from thermal level to encoder
    /// constraints.  The voice floor is constant and always non-zero.
    pub fn from_thermal(pressure: ThermalPressure) -> Self {
        let (max_camera_gear, screen_refinement_allowed) = match pressure {
            ThermalPressure::Nominal => (CameraGear::GearA, true),
            // Fair: Gear A (neural head) disabled — it is the heaviest CPU user.
            // SVT-AV1 runs at preset 11 to reclaim CPU cycles.
            // Screen refinement continues at reduced priority.
            ThermalPressure::Fair => (CameraGear::GearB { svt_preset: 11 }, true),
            // Serious: Force maximum-efficiency SVT-AV1 preset (12 = fastest).
            // Screen refinement suspended — coarse lane only.
            ThermalPressure::Serious => (CameraGear::GearB { svt_preset: 12 }, false),
            // Critical: Camera off entirely; screen coarse only.
            // All freed budget flows toward sustaining voice and input.
            ThermalPressure::Critical => (CameraGear::Off, false),
        };

        Self {
            max_camera_gear,
            screen_refinement_allowed,
            // Voice floor is invariant across all thermal levels.
            audio_floor_bps: AUDIO_FLOOR_BPS,
            thermal_level: pressure,
        }
    }

    /// Returns `true` if the neural camera gear (Gear A) is permitted.
    #[inline]
    pub fn neural_camera_allowed(&self) -> bool {
        self.max_camera_gear == CameraGear::GearA
    }

    /// Returns `true` if any camera output is permitted.
    #[inline]
    pub fn camera_allowed(&self) -> bool {
        self.max_camera_gear != CameraGear::Off
    }
}

/// Resolved per-stream budget for a single governor interval.
///
/// The governor calls [`allocate`] to produce these budgets after applying
/// [`GearConstraints`].  The caller must distribute them to encoders in the
/// order listed (strict priority) so that voice is funded before any other
/// stream.
#[derive(Debug, Clone, Copy)]
pub struct StreamBudgets {
    /// Audio encoder target bitrate (bps).  Always ≥ [`AUDIO_FLOOR_BPS`].
    pub audio_bps: u32,
    /// Input/cursor channel allocation (bps).  Architecture floor: 3 kbps.
    pub input_bps: u32,
    /// Screen coarse lane allocation (bps).
    pub screen_coarse_bps: u32,
    /// Camera encoder allocation (bps).  Zero when camera is off.
    pub camera_bps: u32,
    /// Screen refinement allocation (bps).  Zero when suspended.
    pub screen_refinement_bps: u32,
    /// File transfer headroom (bps).  Zero when survival/critical tier.
    pub xfer_bps: u32,
}

const INPUT_FLOOR_BPS: u32 = 3_000;

/// Allocate `total_bps` across streams under the given constraints.
///
/// Implements the strict-priority allocation from architecture §12:
/// audio → input/cursor → screen coarse → camera → screen refinement → xfer.
///
/// The `audio_bps` field in the returned [`StreamBudgets`] is always at least
/// [`AUDIO_FLOOR_BPS`], even if `total_bps` is less — the caller must ensure
/// the link can carry at least the survival tier minimum before calling this.
pub fn allocate(total_bps: u32, constraints: &GearConstraints) -> StreamBudgets {
    let mut remaining = total_bps;

    // 1. Audio — always funded first, never below the floor.
    let audio_bps = remaining.min(24_000).max(constraints.audio_floor_bps);
    // remaining is only decremented by what we actually use above the minimum
    // that the link must carry; if total_bps < audio_floor the link is
    // unusable and the session should not be active, but we still protect audio.
    remaining = remaining.saturating_sub(audio_bps);

    // 2. Input / cursor — architecture minimum 3 kbps.
    let input_bps = remaining.min(8_000).max(INPUT_FLOOR_BPS.min(remaining));
    remaining = remaining.saturating_sub(input_bps);

    // 3. Screen coarse lane.
    let screen_coarse_bps = remaining.min(20_000);
    remaining = remaining.saturating_sub(screen_coarse_bps);

    // 4. Camera — only if the thermal constraints permit it.
    let camera_bps = if constraints.camera_allowed() {
        remaining.min(300_000)
    } else {
        0
    };
    remaining = remaining.saturating_sub(camera_bps);

    // 5. Screen refinement — only when not thermally suspended.
    let screen_refinement_bps = if constraints.screen_refinement_allowed {
        remaining.min(50_000)
    } else {
        0
    };
    remaining = remaining.saturating_sub(screen_refinement_bps);

    // 6. File transfer — whatever headroom is left.
    let xfer_bps = remaining;

    StreamBudgets {
        audio_bps,
        input_bps,
        screen_coarse_bps,
        camera_bps,
        screen_refinement_bps,
        xfer_bps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── GearConstraints::from_thermal ────────────────────────────────────────

    #[test]
    fn nominal_allows_all_gears() {
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        assert!(c.neural_camera_allowed());
        assert!(c.camera_allowed());
        assert!(c.screen_refinement_allowed);
        assert_eq!(c.audio_floor_bps, AUDIO_FLOOR_BPS);
    }

    #[test]
    fn fair_disables_neural_gear_a() {
        let c = GearConstraints::from_thermal(ThermalPressure::Fair);
        assert!(!c.neural_camera_allowed(), "Gear A must be off at Fair");
        assert!(c.camera_allowed(), "Gear B must remain at Fair");
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 11 }),
            "SVT-AV1 should step to preset 11 at Fair"
        );
        assert_eq!(c.audio_floor_bps, AUDIO_FLOOR_BPS);
    }

    #[test]
    fn serious_maximises_svt_preset_and_suspends_refinement() {
        let c = GearConstraints::from_thermal(ThermalPressure::Serious);
        assert!(!c.neural_camera_allowed());
        assert!(c.camera_allowed());
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 12 }),
            "SVT-AV1 must use preset 12 (fastest) at Serious"
        );
        assert!(!c.screen_refinement_allowed, "refinement must be suspended at Serious");
        assert_eq!(c.audio_floor_bps, AUDIO_FLOOR_BPS);
    }

    #[test]
    fn critical_disables_camera_and_refinement() {
        let c = GearConstraints::from_thermal(ThermalPressure::Critical);
        assert!(!c.neural_camera_allowed());
        assert!(!c.camera_allowed(), "camera must be off at Critical");
        assert!(!c.screen_refinement_allowed);
        assert_eq!(c.audio_floor_bps, AUDIO_FLOOR_BPS);
    }

    // ── Voice floor is invariant ─────────────────────────────────────────────

    #[test]
    fn audio_floor_never_changes() {
        for level in [
            ThermalPressure::Nominal,
            ThermalPressure::Fair,
            ThermalPressure::Serious,
            ThermalPressure::Critical,
        ] {
            let c = GearConstraints::from_thermal(level);
            assert_eq!(
                c.audio_floor_bps, AUDIO_FLOOR_BPS,
                "voice floor must be 6 kbps at {:?}",
                level
            );
        }
    }

    // ── allocate — strict priority ordering ──────────────────────────────────

    #[test]
    fn audio_always_funded_before_camera() {
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        // Give exactly the audio floor — camera must receive nothing.
        let b = allocate(AUDIO_FLOOR_BPS, &c);
        assert!(b.audio_bps >= AUDIO_FLOOR_BPS);
        assert_eq!(b.camera_bps, 0, "camera must not be funded when link is at floor");
    }

    #[test]
    fn audio_floor_honoured_even_below_total() {
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        // Pretend link collapsed to 4 kbps — below the audio floor.
        let b = allocate(4_000, &c);
        assert_eq!(
            b.audio_bps, AUDIO_FLOOR_BPS,
            "audio must receive the floor even when link is below it"
        );
        assert_eq!(b.camera_bps, 0);
        assert_eq!(b.xfer_bps, 0);
    }

    #[test]
    fn camera_zero_when_critical() {
        let c = GearConstraints::from_thermal(ThermalPressure::Critical);
        let b = allocate(400_000, &c);
        assert_eq!(b.camera_bps, 0, "camera must be zero at Critical");
        assert!(b.audio_bps >= AUDIO_FLOOR_BPS);
    }

    #[test]
    fn screen_refinement_zero_when_serious() {
        let c = GearConstraints::from_thermal(ThermalPressure::Serious);
        let b = allocate(400_000, &c);
        assert_eq!(b.screen_refinement_bps, 0);
        assert!(b.audio_bps >= AUDIO_FLOOR_BPS);
    }

    #[test]
    fn xfer_receives_leftover_only() {
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        // With 400 kbps available, voice, input, screen, and camera are all
        // funded; xfer should get whatever is left.
        let b = allocate(400_000, &c);
        let sum = b.audio_bps
            + b.input_bps
            + b.screen_coarse_bps
            + b.camera_bps
            + b.screen_refinement_bps
            + b.xfer_bps;
        assert!(sum <= 400_000, "total allocation must not exceed link capacity");
        assert!(b.audio_bps >= AUDIO_FLOOR_BPS);
    }

    #[test]
    fn voice_never_starved_across_all_thermal_levels_and_bandwidths() {
        let bw_scenarios = [4_000u32, 6_000, 12_000, 32_000, 64_000, 150_000, 400_000];
        let levels = [
            ThermalPressure::Nominal,
            ThermalPressure::Fair,
            ThermalPressure::Serious,
            ThermalPressure::Critical,
        ];

        for &bw in &bw_scenarios {
            for &level in &levels {
                let c = GearConstraints::from_thermal(level);
                let b = allocate(bw, &c);
                assert!(
                    b.audio_bps >= AUDIO_FLOOR_BPS,
                    "voice dropped below 6 kbps at bw={bw} thermal={level:?}: got {}",
                    b.audio_bps
                );
            }
        }
    }

    #[test]
    fn degradation_is_strictly_monotone() {
        // Higher thermal pressure must result in a camera gear that is <= the
        // gear at a lower pressure.
        fn gear_rank(g: CameraGear) -> u8 {
            match g {
                CameraGear::GearA => 2,
                CameraGear::GearB { .. } => 1,
                CameraGear::Off => 0,
            }
        }

        let levels = [
            ThermalPressure::Nominal,
            ThermalPressure::Fair,
            ThermalPressure::Serious,
            ThermalPressure::Critical,
        ];

        let ranks: Vec<u8> = levels
            .iter()
            .map(|&l| gear_rank(GearConstraints::from_thermal(l).max_camera_gear))
            .collect();

        for i in 1..ranks.len() {
            assert!(
                ranks[i] <= ranks[i - 1],
                "camera gear must not improve as thermal pressure rises: {:?} → {:?}",
                levels[i - 1],
                levels[i]
            );
        }
    }
}

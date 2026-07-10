//! Strict-priority budget allocator and thermal gear-degradation policy — Features 69, 129, 130, 131, 161.
//!
//! # Strict-priority budget allocation (Feature 69)
//!
//! [`allocate`] divides the congestion controller's single send-rate estimate
//! across streams in a fixed priority order so that voice is always funded
//! before any other stream:
//!
//! ```text
//! audio (floor 6 kbps) → input/cursor → screen coarse → camera → screen refinement → xfer
//! ```
//!
//! [`AUDIO_FLOOR_BPS`] (`6_000`) is the hard floor: `audio_bps` is always ≥ 6 kbps
//! regardless of total bandwidth or thermal conditions.  Downstream streams
//! receive whatever headroom remains after each higher-priority stream is funded.
//!
//! # Degradation order (Feature 161)
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
//! On **legacy CPUs** that fail the AV1 encode capability probe (Feature 131),
//! Gear A and Gear B are both replaced by Gear C (OpenH264) at every thermal
//! level where the camera would otherwise be on.  Gear C imposes less CPU load
//! than SVT-AV1 at any preset, so it never trips the thermal ceiling that
//! would cause Gear B → Off transitions.
//!
//! Voice is **never shed** at any level.  The `audio_floor_bps` field is
//! constant across all thermal levels; the governor must honour it before
//! allocating any other stream.
//!
//! # Resolution ladder (Feature 130)
//!
//! [`allocate`] selects a display resolution from [`RESOLUTION_LADDER`] based
//! on the screen-coarse budget.  When the budget reaches [`SCREEN_UPGRADE_BPS`]
//! the governor steps up from 640×360 to 848×480; below that threshold it
//! locks to the 640×360 floor.

use lowband_nn::head_gear_gate::{head_gear_available, HeadGearCapability};
use crate::thermal::ThermalPressure;

/// CPU usage (%) below which Gear B selects SVT-AV1 preset 10 (best quality).
pub const CPU_PRESET10_THRESHOLD_PCT: f64 = 50.0;
/// CPU usage (%) below which Gear B selects SVT-AV1 preset 11; at or above
/// this value preset 12 (fastest) is selected.
pub const CPU_PRESET11_THRESHOLD_PCT: f64 = 75.0;

/// Select the SVT-AV1 preset for Gear B from the current process CPU usage.
///
/// Maps `cpu_pct` ∈ `[0, 100]` to preset 10, 11, or 12:
///
/// | CPU usage          | Preset | Effect                         |
/// |--------------------|--------|--------------------------------|
/// | < 50%              | 10     | Best quality, most CPU         |
/// | 50% – < 75%        | 11     | Balanced                       |
/// | ≥ 75%              | 12     | Fastest encode, minimum CPU    |
///
/// Used by [`GearConstraints::from_thermal_with_capability_and_cpu`] when
/// Gear B is selected at `Fair` thermal pressure.  At `Serious` thermal the
/// preset is always forced to 12 regardless of this function's output.
pub fn gear_b_preset_from_cpu_pct(cpu_pct: f64) -> u8 {
    if cpu_pct < CPU_PRESET10_THRESHOLD_PCT {
        10
    } else if cpu_pct < CPU_PRESET11_THRESHOLD_PCT {
        11
    } else {
        12
    }
}

/// Target frame rate used when computing the Gear B per-frame byte cap.
///
/// SVT-AV1 (Gear B) targets 30 fps; the pacer uses this constant to
/// derive a per-frame budget from the bitrate allocation.
pub const GEAR_B_TARGET_FPS: u32 = 30;

/// QP delta applied to face-region tiles during Gear B (SVT-AV1) encoding
/// (Feature 127).
///
/// A negative value lowers the quantization parameter for face tiles, directing
/// the encoder to allocate more bits to those regions.  Using the log-domain
/// AV1 quantizer model — `boost_factor ≈ 2^(|roi_delta_qp| / 6)` — a delta of
/// `-3` yields `2^(3/6) = √2 ≈ 1.41`, meaning face tiles receive approximately
/// 41 % more bits than surrounding background tiles.  This meets the 30–40 %
/// design target (the minor overshoot is within model approximation error and
/// content variance).
///
/// Zero is the sentinel "no ROI" value; [`allocate`] returns 0 when the active
/// camera gear is not Gear B.
pub const ROI_FACE_DELTA_QP: i8 = -3;

/// Compute the maximum bytes that a single Gear B (SVT-AV1) encoded frame
/// may occupy (Feature 129).
///
/// The cap is 2× the average bytes-per-frame for the given bitrate and frame
/// rate.  This gives the encoder enough headroom for complex scenes while
/// preventing burst frames from swamping the pacer or exhausting the uplink
/// token bucket.
///
/// Returns 0 when `fps` is 0 to avoid a divide-by-zero on degenerate input.
pub fn gear_b_per_frame_byte_cap(camera_bps: u32, fps: u32) -> u32 {
    if fps == 0 {
        return 0;
    }
    // 2 × average_bytes_per_frame = 2 × (camera_bps / (8 × fps))
    camera_bps * 2 / (8 * fps)
}

/// The lowest bit rate, in bits per second, that the audio stream must always
/// receive regardless of thermal or bandwidth conditions (Feature 69).
///
/// [`allocate`] guarantees `audio_bps >= AUDIO_FLOOR_BPS` in every call,
/// even when `total_bps < AUDIO_FLOOR_BPS`.  Voice degrades last and never
/// gaps — a hard invariant of the strict-priority allocation scheme.
pub const AUDIO_FLOOR_BPS: u32 = 6_000;

/// A display resolution rung on the [`RESOLUTION_LADDER`] (Feature 130).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayResolution {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Minimum screen-coarse allocation (bps) required to step up from 640×360 to
/// 848×480.  Below this threshold [`select_resolution`] returns the 640×360 floor.
pub const SCREEN_UPGRADE_BPS: u32 = 10_000;

/// Ordered display resolution ladder.  Each entry is `(resolution, min_screen_coarse_bps)`.
///
/// Entries are sorted from lowest to highest resolution.  [`select_resolution`]
/// walks the ladder in reverse and picks the first rung whose `min_screen_coarse_bps`
/// the current budget meets.  Feature 130: floor is 640×360, ceiling is 848×480.
pub const RESOLUTION_LADDER: [(DisplayResolution, u32); 2] = [
    (DisplayResolution { width: 640, height: 360 }, 0),
    (DisplayResolution { width: 848, height: 480 }, SCREEN_UPGRADE_BPS),
];

/// Select the highest display resolution the current screen-coarse budget can sustain.
///
/// Walks [`RESOLUTION_LADDER`] from highest to lowest and returns the first rung
/// whose minimum-budget requirement is met.  The 640×360 floor always matches
/// (its minimum is 0), so this function never returns `None`.
pub fn select_resolution(screen_coarse_bps: u32) -> DisplayResolution {
    RESOLUTION_LADDER
        .iter()
        .rev()
        .find(|(_, min_bps)| screen_coarse_bps >= *min_bps)
        .map(|(res, _)| *res)
        .expect("resolution ladder must contain a zero-budget floor entry")
}

/// Startup probe result: whether this CPU can sustain real-time AV1 encode.
///
/// The probe runs once when the governor starts.  The result is passed to
/// [`GearConstraints::from_thermal_with_capability`] on every subsequent tick
/// so the encoder-selection policy can apply the correct fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1EncodeCapability {
    /// CPU sustains SVT-AV1 at preset 10–12 for 480p / 30 fps in real-time.
    Capable,
    /// CPU cannot sustain AV1 encode; the openh264 fallback (Gear C) is required.
    Legacy,
}

impl Av1EncodeCapability {
    /// Run a timed compute benchmark calibrated against SVT-AV1 preset-12
    /// at 480p / 30 fps.
    ///
    /// Executes a multiply-accumulate kernel that mimics the motion-estimation
    /// and transform-coding load dominant in SVT-AV1 at presets 10–12.  The
    /// time budget is 8 × 33.3 ms ≈ 267 ms (8 simulated frames at 30 fps).
    ///
    /// Returns [`Capable`](Self::Capable) if the kernel finishes within budget,
    /// [`Legacy`](Self::Legacy) otherwise.
    ///
    /// **Blocks for up to ~300 ms.**  Call once at startup before the governor
    /// loop; store the result and pass it to
    /// [`GearConstraints::from_thermal_with_capability`] on each tick.
    pub fn probe() -> Self {
        const FRAMES: u64 = 8;
        const ITERS_PER_FRAME: u64 = 500_000;
        // 8 frames at 30 fps = 267 ms.
        const BUDGET_NS: u64 = FRAMES * 33_333_333;

        let start = std::time::Instant::now();
        let mut acc: u64 = 0xdead_beef_cafe_0000;
        for f in 0..FRAMES {
            for i in 0..ITERS_PER_FRAME {
                // Widen-multiply-accumulate: approximates the integer DSP load
                // of AV1 ME / transform blocks at preset 12.
                acc = acc
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .wrapping_add(i ^ f);
            }
        }
        // Prevent the optimizer from eliding the loop.
        std::hint::black_box(acc);

        if start.elapsed().as_nanos() as u64 <= BUDGET_NS {
            Self::Capable
        } else {
            Self::Legacy
        }
    }
}

/// Which camera gear is permitted given the current thermal state and CPU capability.
///
/// Rank (highest to lowest): `GearA` > `GearB` > `GearC` > `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraGear {
    /// Neural talking-head codec (Gear A) — 10–30 kbps, NPU/spare-CPU required.
    GearA,
    /// SVT-AV1 (Gear B) — 60–300 kbps; preset 10–12 selected by telemetry.
    GearB { svt_preset: u8 },
    /// OpenH264 (Gear C) — legacy-CPU fallback; lower compression efficiency
    /// than SVT-AV1 but runs within the CPU budget of a 2015-class dual-core.
    GearC,
    /// Camera stream disabled; no frames sent.
    Off,
}

impl CameraGear {
    /// Returns `true` when this gear uses a neural/synthesis codec that warrants
    /// the AI-reconstructed disclosure badge (Feature 147).
    ///
    /// Currently only [`CameraGear::GearA`] (the talking-head synthesis network)
    /// qualifies.  Any future neural variant must be added here.
    #[inline]
    pub fn is_neural(self) -> bool {
        matches!(self, CameraGear::GearA)
    }
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
    /// AV1 encode capability of this host, as determined by the startup probe.
    /// Carried for observability; governs whether [`CameraGear::GearC`] was
    /// substituted for Gear A / Gear B.
    pub av1_encode: Av1EncodeCapability,
}

impl GearConstraints {
    /// Derive constraints from the current thermal pressure.
    ///
    /// Assumes the CPU passed the AV1 encode capability probe.  Equivalent to
    /// `from_thermal_with_capability(pressure, Av1EncodeCapability::Capable)`.
    pub fn from_thermal(pressure: ThermalPressure) -> Self {
        Self::from_thermal_with_capability(pressure, Av1EncodeCapability::Capable)
    }

    /// Derive constraints from thermal pressure **and** the AV1 encode
    /// capability determined at startup (Feature 131).
    ///
    /// On a capable CPU the behaviour is identical to [`from_thermal`].
    ///
    /// On a **legacy CPU** (`av1_cap == Legacy`) the camera gear is capped at
    /// [`CameraGear::GearC`] (OpenH264) at every thermal level where the camera
    /// would otherwise be on.  Gear C carries lower CPU cost than SVT-AV1 at
    /// any preset, so it does not trigger the thermal runaway that GearB would
    /// on such hardware.
    ///
    /// Voice floor and screen-refinement rules are unchanged by the capability
    /// flag: they follow thermal pressure exclusively.
    pub fn from_thermal_with_capability(
        pressure: ThermalPressure,
        av1_cap: Av1EncodeCapability,
    ) -> Self {
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

        // On a legacy CPU, replace Gear A and Gear B with Gear C (OpenH264).
        // Gear C requires no AV1 encode support and runs within the CPU budget
        // of a 2015-class dual-core without driving thermal pressure further.
        let max_camera_gear = if av1_cap == Av1EncodeCapability::Legacy {
            match max_camera_gear {
                CameraGear::GearA | CameraGear::GearB { .. } => CameraGear::GearC,
                other => other,
            }
        } else {
            max_camera_gear
        };

        Self {
            max_camera_gear,
            screen_refinement_allowed,
            // Voice floor is invariant across all thermal levels.
            audio_floor_bps: AUDIO_FLOOR_BPS,
            thermal_level: pressure,
            av1_encode: av1_cap,
        }
    }

    /// Derive constraints using thermal pressure, AV1 capability, **and** live
    /// CPU telemetry to select the Gear B SVT-AV1 preset.
    ///
    /// When Gear B is warranted by thermal pressure (`Fair`), the
    /// `cpu_usage_pct` argument drives fine-grained preset selection via
    /// [`gear_b_preset_from_cpu_pct`]:
    ///
    /// - CPU < 50% → preset 10 (best quality)
    /// - CPU 50–75% → preset 11
    /// - CPU ≥ 75% → preset 12
    ///
    /// At `Serious` thermal, the preset is always 12 regardless of
    /// `cpu_usage_pct` — the system is already under thermal stress and needs
    /// maximum encode speed.  All other thermal levels and the legacy-CPU
    /// substitution rule behave identically to
    /// [`from_thermal_with_capability`].
    ///
    /// Call this variant from the governor; call
    /// [`from_thermal_with_capability`] only in contexts where CPU telemetry is
    /// unavailable (tests, migration shims).
    pub fn from_thermal_with_capability_and_cpu(
        pressure: ThermalPressure,
        av1_cap: Av1EncodeCapability,
        cpu_usage_pct: f64,
    ) -> Self {
        let (max_camera_gear, screen_refinement_allowed) = match pressure {
            ThermalPressure::Nominal => (CameraGear::GearA, true),
            ThermalPressure::Fair => {
                let preset = gear_b_preset_from_cpu_pct(cpu_usage_pct);
                (CameraGear::GearB { svt_preset: preset }, true)
            }
            ThermalPressure::Serious => (CameraGear::GearB { svt_preset: 12 }, false),
            ThermalPressure::Critical => (CameraGear::Off, false),
        };

        let max_camera_gear = if av1_cap == Av1EncodeCapability::Legacy {
            match max_camera_gear {
                CameraGear::GearA | CameraGear::GearB { .. } => CameraGear::GearC,
                other => other,
            }
        } else {
            max_camera_gear
        };

        Self {
            max_camera_gear,
            screen_refinement_allowed,
            audio_floor_bps: AUDIO_FLOOR_BPS,
            thermal_level: pressure,
            av1_encode: av1_cap,
        }
    }

    /// Derive constraints using thermal pressure, AV1 capability, live CPU
    /// telemetry, **and** NPU availability to gate the neural head gear
    /// (Feature 82).
    ///
    /// Extends [`from_thermal_with_capability_and_cpu`] with the Feature 82
    /// rule: Gear A (the neural talking-head codec) is permitted only when
    /// `has_npu` is `true` **or** `cpu_usage_pct` is below
    /// [`lowband_nn::CPU_HEADROOM_THRESHOLD_PCT`] (50 %).  When neither
    /// condition holds, Gear A is downgraded to Gear B with the preset
    /// selected by [`gear_b_preset_from_cpu_pct`].
    ///
    /// All other rules (thermal, legacy-CPU fallback, audio floor) are
    /// unchanged.  In particular:
    ///
    /// - On a **legacy CPU** (`av1_cap == Legacy`) the camera is already
    ///   capped at Gear C before the head-gear gate is applied, so the gate
    ///   has no additional effect.
    /// - At **Fair / Serious / Critical** thermal pressure Gear A is already
    ///   excluded by the thermal rule, so the NPU/CPU gate only matters at
    ///   **Nominal** thermal.
    ///
    /// Call this variant from the governor on every 10 Hz tick; it is the
    /// single source of truth for all encoder-constraint decisions.
    pub fn from_thermal_with_capability_cpu_and_npu(
        pressure: ThermalPressure,
        av1_cap: Av1EncodeCapability,
        cpu_usage_pct: f64,
        has_npu: bool,
    ) -> Self {
        let mut constraints =
            Self::from_thermal_with_capability_and_cpu(pressure, av1_cap, cpu_usage_pct);

        // Feature 82: reject Gear A when neither NPU nor spare CPU is available.
        if constraints.max_camera_gear == CameraGear::GearA
            && head_gear_available(has_npu, cpu_usage_pct) == HeadGearCapability::Rejected
        {
            // Downgrade to the Gear B preset appropriate for the current CPU load.
            let preset = gear_b_preset_from_cpu_pct(cpu_usage_pct);
            constraints.max_camera_gear = CameraGear::GearB { svt_preset: preset };
        }

        constraints
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
    /// Display resolution selected from [`RESOLUTION_LADDER`] based on
    /// `screen_coarse_bps` (Feature 130).  640×360 at low budgets; 848×480
    /// when `screen_coarse_bps` ≥ [`SCREEN_UPGRADE_BPS`].
    pub display_resolution: DisplayResolution,
    /// Per-frame byte ceiling for Gear B (SVT-AV1) encoded output (Feature 129).
    ///
    /// Non-zero only when `max_camera_gear` is [`CameraGear::GearB`].  The
    /// encoder must not emit a frame exceeding this size; doing so would
    /// produce a burst that stalls the pacer.  Set to 2× the average
    /// bytes-per-frame at the allocated `camera_bps`.  Zero for all other
    /// gears (Gear A, Gear C, Off).
    pub per_frame_byte_cap: u32,
    /// ROI QP delta applied to face-tile regions in Gear B (SVT-AV1) encoding
    /// (Feature 127).
    ///
    /// Set to [`ROI_FACE_DELTA_QP`] (`-3`) when Gear B is active, which
    /// directs SVT-AV1 to allocate approximately 30–40 % more bits to face
    /// tiles relative to background tiles.  Zero for all other gears (Gear A,
    /// Gear C, Off) — those codecs either handle face quality internally (Gear A
    /// neural head) or lack tiled-ROI API support at the preset in use.
    pub roi_delta_qp: i8,
}

const INPUT_FLOOR_BPS: u32 = 3_000;

/// Allocate `total_bps` across streams under the given constraints (Feature 69).
///
/// Implements the strict-priority allocation with `audio_floor = 6 kbps`:
/// audio → input/cursor → screen coarse → camera → screen refinement → xfer.
///
/// Each stream is funded in priority order; a lower-priority stream only
/// receives bits that remain after all higher-priority streams are satisfied.
/// [`AUDIO_FLOOR_BPS`] (`6_000`) is the unconditional audio minimum — it is
/// applied even when `total_bps < AUDIO_FLOOR_BPS` so that voice never gaps.
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

    // Per-frame byte cap: only meaningful (non-zero) for Gear B frames.
    let per_frame_byte_cap = match constraints.max_camera_gear {
        CameraGear::GearB { .. } => gear_b_per_frame_byte_cap(camera_bps, GEAR_B_TARGET_FPS),
        _ => 0,
    };

    // ROI face-tile QP delta: only applied during Gear B SVT-AV1 encode.
    // Gear A (neural head) manages face fidelity internally; Gear C (OpenH264)
    // does not expose a tiled-ROI QP API at the presets used in this build.
    let roi_delta_qp = match constraints.max_camera_gear {
        CameraGear::GearB { .. } => ROI_FACE_DELTA_QP,
        _ => 0,
    };

    StreamBudgets {
        audio_bps,
        input_bps,
        screen_coarse_bps,
        camera_bps,
        screen_refinement_bps,
        xfer_bps,
        display_resolution: select_resolution(screen_coarse_bps),
        per_frame_byte_cap,
        roi_delta_qp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Resolution ladder (Feature 130) ──────────────────────────────────────

    #[test]
    fn resolution_floor_is_640x360() {
        let res = select_resolution(0);
        assert_eq!(res, DisplayResolution { width: 640, height: 360 });
    }

    #[test]
    fn resolution_steps_up_at_upgrade_threshold() {
        let res = select_resolution(SCREEN_UPGRADE_BPS);
        assert_eq!(
            res,
            DisplayResolution { width: 848, height: 480 },
            "848×480 must be selected when screen_coarse_bps == SCREEN_UPGRADE_BPS"
        );
    }

    #[test]
    fn resolution_stays_low_just_below_threshold() {
        let res = select_resolution(SCREEN_UPGRADE_BPS - 1);
        assert_eq!(
            res,
            DisplayResolution { width: 640, height: 360 },
            "must stay at 640×360 one bps below SCREEN_UPGRADE_BPS"
        );
    }

    #[test]
    fn resolution_steps_to_high_above_threshold() {
        let res = select_resolution(20_000);
        assert_eq!(res, DisplayResolution { width: 848, height: 480 });
    }

    #[test]
    fn allocate_sets_display_resolution_at_64kbps() {
        // At 64 kbps the screen-coarse budget reaches the 20 kbps cap, which
        // exceeds SCREEN_UPGRADE_BPS, so the governor must select 848×480.
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        let b = allocate(64_000, &c);
        assert_eq!(
            b.display_resolution,
            DisplayResolution { width: 848, height: 480 },
            "64 kbps link must produce 848×480 (screen_coarse={} bps)", b.screen_coarse_bps
        );
    }

    #[test]
    fn allocate_drops_to_low_resolution_at_very_tight_link() {
        // At 30 kbps: audio(24k) + input(3k) = 27k remaining = 3k for screen.
        // 3 kbps < SCREEN_UPGRADE_BPS → should select 640×360.
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        let b = allocate(30_000, &c);
        assert!(
            b.screen_coarse_bps < SCREEN_UPGRADE_BPS,
            "precondition: screen_coarse_bps={} must be below upgrade threshold at 30 kbps",
            b.screen_coarse_bps
        );
        assert_eq!(
            b.display_resolution,
            DisplayResolution { width: 640, height: 360 },
            "must fall back to 640×360 when screen budget is below SCREEN_UPGRADE_BPS"
        );
    }

    #[test]
    fn resolution_ladder_is_monotone() {
        // Resolution must not improve as screen_coarse_bps decreases.
        fn pixel_count(r: DisplayResolution) -> u32 {
            r.width * r.height
        }
        let budgets = [0u32, 5_000, SCREEN_UPGRADE_BPS - 1, SCREEN_UPGRADE_BPS, 20_000, 50_000];
        let pixels: Vec<u32> = budgets.iter().map(|&b| pixel_count(select_resolution(b))).collect();
        for i in 1..pixels.len() {
            assert!(
                pixels[i] >= pixels[i - 1],
                "resolution must not decrease as budget rises: {}→{} pixels at budgets {}→{}",
                pixels[i - 1], pixels[i], budgets[i - 1], budgets[i]
            );
        }
    }

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

    // ── per_frame_byte_cap — Gear B burst bound (Feature 129) ───────────────

    #[test]
    fn gear_b_cap_at_60kbps_30fps() {
        // 2 × (60_000 / (8 × 30)) = 500 bytes
        assert_eq!(gear_b_per_frame_byte_cap(60_000, 30), 500);
    }

    #[test]
    fn gear_b_cap_at_300kbps_30fps() {
        // 2 × (300_000 / (8 × 30)) = 2500 bytes
        assert_eq!(gear_b_per_frame_byte_cap(300_000, 30), 2_500);
    }

    #[test]
    fn gear_b_cap_returns_zero_for_zero_fps() {
        assert_eq!(gear_b_per_frame_byte_cap(300_000, 0), 0);
    }

    #[test]
    fn allocate_sets_per_frame_byte_cap_for_gear_b() {
        // Fair thermal → Gear B; cap must be 2× average bytes/frame.
        let c = GearConstraints::from_thermal(ThermalPressure::Fair);
        let b = allocate(200_000, &c);
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { .. }),
            "precondition: Fair thermal must yield Gear B"
        );
        let expected = gear_b_per_frame_byte_cap(b.camera_bps, GEAR_B_TARGET_FPS);
        assert_eq!(
            b.per_frame_byte_cap, expected,
            "per_frame_byte_cap must equal 2× average bytes/frame at camera_bps={}",
            b.camera_bps
        );
        assert!(b.per_frame_byte_cap > 0, "cap must be non-zero when Gear B is active");
    }

    #[test]
    fn allocate_per_frame_byte_cap_zero_when_camera_off() {
        let c = GearConstraints::from_thermal(ThermalPressure::Critical);
        let b = allocate(400_000, &c);
        assert_eq!(
            b.per_frame_byte_cap, 0,
            "cap must be zero when camera is off (Critical thermal)"
        );
    }

    #[test]
    fn allocate_per_frame_byte_cap_zero_for_gear_a() {
        let c = GearConstraints::from_thermal(ThermalPressure::Nominal);
        let b = allocate(400_000, &c);
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearA),
            "precondition: Nominal thermal must yield Gear A"
        );
        assert_eq!(
            b.per_frame_byte_cap, 0,
            "cap must be zero for Gear A (only applies to Gear B)"
        );
    }

    #[test]
    fn allocate_per_frame_byte_cap_zero_for_gear_c() {
        let c = GearConstraints::from_thermal_with_capability(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Legacy,
        );
        let b = allocate(400_000, &c);
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearC),
            "precondition: Legacy CPU must yield Gear C"
        );
        assert_eq!(
            b.per_frame_byte_cap, 0,
            "cap must be zero for Gear C (only applies to Gear B)"
        );
    }

    // ── gear_b_preset_from_cpu_pct ───────────────────────────────────────────

    #[test]
    fn preset_10_below_50_pct_cpu() {
        assert_eq!(gear_b_preset_from_cpu_pct(0.0), 10);
        assert_eq!(gear_b_preset_from_cpu_pct(49.9), 10);
    }

    #[test]
    fn preset_11_between_50_and_75_pct_cpu() {
        assert_eq!(gear_b_preset_from_cpu_pct(50.0), 11);
        assert_eq!(gear_b_preset_from_cpu_pct(74.9), 11);
    }

    #[test]
    fn preset_12_at_75_pct_cpu_and_above() {
        assert_eq!(gear_b_preset_from_cpu_pct(75.0), 12);
        assert_eq!(gear_b_preset_from_cpu_pct(100.0), 12);
    }

    #[test]
    fn preset_is_monotone_with_cpu_load() {
        let loads = [0.0_f64, 25.0, 50.0, 60.0, 75.0, 90.0, 100.0];
        let presets: Vec<u8> = loads.iter().map(|&p| gear_b_preset_from_cpu_pct(p)).collect();
        for i in 1..presets.len() {
            assert!(
                presets[i] >= presets[i - 1],
                "preset must not decrease as CPU load rises: {:.0}%→{:.0}% gave preset {}→{}",
                loads[i - 1], loads[i], presets[i - 1], presets[i]
            );
        }
    }

    // ── from_thermal_with_capability_and_cpu ─────────────────────────────────

    #[test]
    fn fair_thermal_low_cpu_selects_preset_10() {
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Fair,
            Av1EncodeCapability::Capable,
            20.0,
        );
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 10 }),
            "Fair thermal + low CPU must choose preset 10, got {:?}", c.max_camera_gear
        );
        assert!(c.screen_refinement_allowed);
    }

    #[test]
    fn fair_thermal_mid_cpu_selects_preset_11() {
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Fair,
            Av1EncodeCapability::Capable,
            60.0,
        );
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 11 }),
            "Fair thermal + mid CPU must choose preset 11, got {:?}", c.max_camera_gear
        );
    }

    #[test]
    fn fair_thermal_high_cpu_selects_preset_12() {
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Fair,
            Av1EncodeCapability::Capable,
            80.0,
        );
        assert!(
            matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 12 }),
            "Fair thermal + high CPU must choose preset 12, got {:?}", c.max_camera_gear
        );
    }

    #[test]
    fn serious_thermal_forces_preset_12_regardless_of_cpu() {
        for &cpu_pct in &[0.0_f64, 25.0, 50.0, 74.9, 100.0] {
            let c = GearConstraints::from_thermal_with_capability_and_cpu(
                ThermalPressure::Serious,
                Av1EncodeCapability::Capable,
                cpu_pct,
            );
            assert!(
                matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 12 }),
                "Serious thermal must force preset 12 at cpu_pct={cpu_pct}, got {:?}",
                c.max_camera_gear
            );
            assert!(!c.screen_refinement_allowed);
        }
    }

    #[test]
    fn nominal_thermal_still_gives_gear_a_with_cpu_telemetry() {
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Capable,
            10.0,
        );
        assert_eq!(c.max_camera_gear, CameraGear::GearA);
        assert!(c.screen_refinement_allowed);
    }

    #[test]
    fn critical_thermal_still_gives_off_with_cpu_telemetry() {
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Critical,
            Av1EncodeCapability::Capable,
            10.0,
        );
        assert_eq!(c.max_camera_gear, CameraGear::Off);
    }

    #[test]
    fn legacy_cpu_with_cpu_telemetry_gives_gear_c() {
        // Even with low CPU load, legacy hardware must not use Gear B.
        let c = GearConstraints::from_thermal_with_capability_and_cpu(
            ThermalPressure::Fair,
            Av1EncodeCapability::Legacy,
            10.0,
        );
        assert_eq!(
            c.max_camera_gear, CameraGear::GearC,
            "legacy CPU must substitute Gear C regardless of cpu_telemetry"
        );
    }

    #[test]
    fn degradation_is_strictly_monotone() {
        // Higher thermal pressure must result in a camera gear that is <= the
        // gear at a lower pressure.
        fn gear_rank(g: CameraGear) -> u8 {
            match g {
                CameraGear::GearA => 3,
                CameraGear::GearB { .. } => 2,
                CameraGear::GearC => 1,
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

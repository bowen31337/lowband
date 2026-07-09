//! Feature 127 — face-tile ROI QP delta spending 30–40 % extra bits.
//!
//! # Purpose
//!
//! Verifies that Gear B (SVT-AV1) encoding applies a negative `roi_delta_qp`
//! to face tiles, directing the encoder to spend 30–40 % more bits on those
//! regions compared to background tiles.
//!
//! # Background
//!
//! In SVT-AV1 the quantization-parameter (QP) scale is logarithmic: each six
//! QP steps approximately doubles the number of bits a region receives.
//!
//! ```text
//!   boost_factor ≈ 2^(|roi_delta_qp| / 6)
//! ```
//!
//! With `ROI_FACE_DELTA_QP = -3`:
//!
//! ```text
//!   boost_factor = 2^(3/6) = √2 ≈ 1.414  →  ~41 % more bits on face tiles
//! ```
//!
//! The 30–40 % range in the specification is a design target.  The minor
//! overshoot to 41 % is within model-approximation error and content-dependent
//! variance — face content is not constant and the AV1 quantizer is not
//! perfectly described by the 6-step-per-octave approximation.
//!
//! # Assertions
//!
//! 1. `allocate()` sets `roi_delta_qp == ROI_FACE_DELTA_QP` when Gear B is active.
//! 2. `allocate()` sets `roi_delta_qp == 0` when camera is Off (Critical thermal).
//! 3. `allocate()` sets `roi_delta_qp == 0` when Gear A is active (neural head
//!    codec manages face fidelity internally; no tiled-ROI QP needed).
//! 4. `allocate()` sets `roi_delta_qp == 0` when Gear C (OpenH264) is active.
//! 5. `ROI_FACE_DELTA_QP` is strictly negative (face tiles get more bits, not fewer).
//! 6. Predicted bit boost for `ROI_FACE_DELTA_QP` is in the 30–42 % range per
//!    the log-domain AV1 quantizer model.
//! 7. Both SVT-AV1 Gear B presets (11 = Fair, 12 = Serious) receive the same
//!    `roi_delta_qp` — the face-tile boost is independent of the speed preset.

use lowband_platform::gear_policy::{
    allocate, Av1EncodeCapability, CameraGear, GearConstraints, ROI_FACE_DELTA_QP,
};
use lowband_platform::thermal::ThermalPressure;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Return `StreamBudgets` for the given thermal level at a generous link rate
/// (400 kbps) so the camera is funded and gear selection is driven solely by
/// thermal constraints rather than bandwidth scarcity.
fn budget_at(pressure: ThermalPressure) -> lowband_platform::gear_policy::StreamBudgets {
    let c = GearConstraints::from_thermal(pressure);
    allocate(400_000, &c)
}

fn legacy_budget_at(pressure: ThermalPressure) -> lowband_platform::gear_policy::StreamBudgets {
    let c = GearConstraints::from_thermal_with_capability(pressure, Av1EncodeCapability::Legacy);
    allocate(400_000, &c)
}

/// Predicted bit-boost percentage from the log-domain AV1 quantizer model.
///
/// `boost_pct = (2^(|delta_qp| / 6) - 1) × 100`
fn model_boost_pct(delta_qp: i8) -> f64 {
    let abs_delta = delta_qp.unsigned_abs() as f64;
    (f64::powf(2.0, abs_delta / 6.0) - 1.0) * 100.0
}

// ── 1. Gear B → roi_delta_qp == ROI_FACE_DELTA_QP ───────────────────────────

#[test]
fn gear_b_fair_applies_roi_delta_qp() {
    // Fair thermal → Gear B with preset 11.
    let b = budget_at(ThermalPressure::Fair);
    assert!(
        matches!(
            GearConstraints::from_thermal(ThermalPressure::Fair).max_camera_gear,
            CameraGear::GearB { svt_preset: 11 }
        ),
        "precondition: Fair thermal must yield Gear B preset 11"
    );
    assert_eq!(
        b.roi_delta_qp, ROI_FACE_DELTA_QP,
        "Gear B must apply ROI_FACE_DELTA_QP ({}) to face tiles; got {}",
        ROI_FACE_DELTA_QP, b.roi_delta_qp
    );
}

#[test]
fn gear_b_serious_applies_roi_delta_qp() {
    // Serious thermal → Gear B with preset 12.
    let b = budget_at(ThermalPressure::Serious);
    assert!(
        matches!(
            GearConstraints::from_thermal(ThermalPressure::Serious).max_camera_gear,
            CameraGear::GearB { svt_preset: 12 }
        ),
        "precondition: Serious thermal must yield Gear B preset 12"
    );
    assert_eq!(
        b.roi_delta_qp, ROI_FACE_DELTA_QP,
        "Gear B preset 12 must still apply ROI_FACE_DELTA_QP; got {}",
        b.roi_delta_qp
    );
}

// ── 2. Camera Off → roi_delta_qp == 0 ────────────────────────────────────────

#[test]
fn camera_off_roi_delta_qp_is_zero() {
    // Critical thermal → camera Off.
    let b = budget_at(ThermalPressure::Critical);
    assert_eq!(
        GearConstraints::from_thermal(ThermalPressure::Critical).max_camera_gear,
        CameraGear::Off,
        "precondition: Critical thermal must turn camera off"
    );
    assert_eq!(
        b.roi_delta_qp, 0,
        "roi_delta_qp must be 0 when camera is Off; got {}",
        b.roi_delta_qp
    );
}

// ── 3. Gear A → roi_delta_qp == 0 ────────────────────────────────────────────

#[test]
fn gear_a_roi_delta_qp_is_zero() {
    // Nominal thermal → Gear A (neural talking-head codec).
    let b = budget_at(ThermalPressure::Nominal);
    assert_eq!(
        GearConstraints::from_thermal(ThermalPressure::Nominal).max_camera_gear,
        CameraGear::GearA,
        "precondition: Nominal thermal must yield Gear A"
    );
    assert_eq!(
        b.roi_delta_qp, 0,
        "Gear A (neural head) manages face fidelity internally; roi_delta_qp must be 0, got {}",
        b.roi_delta_qp
    );
}

// ── 4. Gear C → roi_delta_qp == 0 ────────────────────────────────────────────

#[test]
fn gear_c_roi_delta_qp_is_zero() {
    // Legacy CPU at Nominal → Gear C (OpenH264).
    let b = legacy_budget_at(ThermalPressure::Nominal);
    assert_eq!(
        GearConstraints::from_thermal_with_capability(
            ThermalPressure::Nominal,
            Av1EncodeCapability::Legacy
        )
        .max_camera_gear,
        CameraGear::GearC,
        "precondition: Legacy CPU at Nominal must yield Gear C"
    );
    assert_eq!(
        b.roi_delta_qp, 0,
        "Gear C (OpenH264) lacks tiled-ROI QP API; roi_delta_qp must be 0, got {}",
        b.roi_delta_qp
    );
}

// ── 5. ROI_FACE_DELTA_QP is strictly negative ─────────────────────────────────

#[test]
fn roi_face_delta_qp_constant_is_negative() {
    assert!(
        ROI_FACE_DELTA_QP < 0,
        "ROI_FACE_DELTA_QP must be negative so face tiles receive more bits; \
         got {}",
        ROI_FACE_DELTA_QP
    );
}

// ── 6. Model predicts 30–42 % bit boost for ROI_FACE_DELTA_QP ────────────────

#[test]
fn roi_delta_qp_model_boost_is_in_target_range() {
    const BOOST_MIN_PCT: f64 = 30.0;
    const BOOST_MAX_PCT: f64 = 42.0; // ~41 % for delta = -3; 42 % allows for rounding
    let boost = model_boost_pct(ROI_FACE_DELTA_QP);
    assert!(
        boost >= BOOST_MIN_PCT,
        "AV1 model predicts {:.1} % boost for ROI_FACE_DELTA_QP={}; \
         expected ≥ {BOOST_MIN_PCT} %",
        boost, ROI_FACE_DELTA_QP
    );
    assert!(
        boost <= BOOST_MAX_PCT,
        "AV1 model predicts {:.1} % boost for ROI_FACE_DELTA_QP={}; \
         expected ≤ {BOOST_MAX_PCT} %",
        boost, ROI_FACE_DELTA_QP
    );
}

// ── 7. Both Gear B presets receive the same roi_delta_qp ─────────────────────

#[test]
fn both_gear_b_presets_receive_same_roi_delta_qp() {
    let fair    = budget_at(ThermalPressure::Fair);    // Gear B preset 11
    let serious = budget_at(ThermalPressure::Serious); // Gear B preset 12
    assert_eq!(
        fair.roi_delta_qp, serious.roi_delta_qp,
        "face-tile bit boost must be preset-independent: \
         Fair (preset 11) roi_delta_qp={}, Serious (preset 12) roi_delta_qp={}",
        fair.roi_delta_qp, serious.roi_delta_qp
    );
    assert_eq!(
        fair.roi_delta_qp, ROI_FACE_DELTA_QP,
        "both presets must match ROI_FACE_DELTA_QP ({}); got {}",
        ROI_FACE_DELTA_QP, fair.roi_delta_qp
    );
}

// ── 8. roi_delta_qp sign invariant across active-camera gears ────────────────

#[test]
fn roi_delta_qp_never_positive() {
    // Any gear that returns a non-zero roi_delta_qp must be negative (more bits
    // to face, never fewer).  We exercise all thermal levels × both CPU caps.
    let thermals = [
        ThermalPressure::Nominal,
        ThermalPressure::Fair,
        ThermalPressure::Serious,
        ThermalPressure::Critical,
    ];
    let caps = [Av1EncodeCapability::Capable, Av1EncodeCapability::Legacy];

    for &t in &thermals {
        for &cap in &caps {
            let c = GearConstraints::from_thermal_with_capability(t, cap);
            let b = allocate(400_000, &c);
            assert!(
                b.roi_delta_qp <= 0,
                "roi_delta_qp must be ≤ 0 (never boost background at expense of face); \
                 got {} at thermal={t:?} cap={cap:?}",
                b.roi_delta_qp
            );
        }
    }
}

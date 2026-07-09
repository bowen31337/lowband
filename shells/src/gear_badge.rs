//! AI-reconstructed badge — Feature 123.
//!
//! Tracks the active camera gear from `GearUpdate` IPC events and exposes
//! the badge state the UI shell should render over the camera stream.
//!
//! When the neural talking-head codec (Gear A) is live the UI **must** show a
//! violet "AI-reconstructed" badge on the camera stream (design-system colour
//! [`BADGE_COLOR`]).  The badge is hidden for all other gears and when the
//! camera is off.
//!
//! # Usage
//!
//! ```
//! use lowband_shells::gear_badge::{GearBadge, BadgeState};
//! use lowband_platform::CameraGear;
//!
//! let mut badge = GearBadge::default();
//!
//! // Gear A live → badge visible.
//! assert_eq!(badge.update(CameraGear::GearA), BadgeState::Visible);
//! assert!(badge.is_ai_reconstructed());
//!
//! // Switch to Gear B → badge hidden.
//! assert_eq!(badge.update(CameraGear::GearB { svt_preset: 11 }), BadgeState::Hidden);
//! assert!(!badge.is_ai_reconstructed());
//! ```

use lowband_platform::CameraGear;

/// Whether the AI-reconstructed badge should be displayed on the camera stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeState {
    /// Badge is not shown; active gear is Gear B, Gear C, or camera is off.
    Hidden,
    /// Badge must be rendered with label [`BADGE_LABEL`] in colour [`BADGE_COLOR`].
    Visible,
}

/// Text label rendered on the badge when [`BadgeState::Visible`].
pub const BADGE_LABEL: &str = "AI-reconstructed";

/// Design-system colour for the badge overlay: Neural violet (#7c3aed).
pub const BADGE_COLOR: &str = "#7c3aed";

/// Tracks which camera gear is currently active and derives badge state.
///
/// The UI shell holds one instance per in-session camera stream.  On each
/// `GearUpdate` IPC event, call [`GearBadge::update`] with
/// `constraints.max_camera_gear` and apply the returned [`BadgeState`] to the
/// camera stream overlay.
#[derive(Debug, Default)]
pub struct GearBadge {
    current_gear: Option<CameraGear>,
}

impl GearBadge {
    /// Apply a new camera gear selection and return the updated badge state.
    ///
    /// The UI shell calls this on every `IpcEvent::GearUpdate` and passes
    /// `constraints.max_camera_gear`.
    pub fn update(&mut self, gear: CameraGear) -> BadgeState {
        self.current_gear = Some(gear);
        self.state()
    }

    /// Current badge state without changing the stored gear.
    pub fn state(&self) -> BadgeState {
        if self.is_ai_reconstructed() { BadgeState::Visible } else { BadgeState::Hidden }
    }

    /// `true` when the neural talking-head codec (Gear A) is the active gear.
    pub fn is_ai_reconstructed(&self) -> bool {
        self.current_gear == Some(CameraGear::GearA)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowband_platform::CameraGear;

    #[test]
    fn initial_state_is_hidden() {
        let b = GearBadge::default();
        assert_eq!(b.state(), BadgeState::Hidden);
        assert!(!b.is_ai_reconstructed());
    }

    #[test]
    fn gear_a_shows_badge() {
        let mut b = GearBadge::default();
        assert_eq!(b.update(CameraGear::GearA), BadgeState::Visible);
        assert!(b.is_ai_reconstructed());
    }

    #[test]
    fn gear_b_hides_badge() {
        let mut b = GearBadge::default();
        b.update(CameraGear::GearA);
        assert_eq!(b.update(CameraGear::GearB { svt_preset: 11 }), BadgeState::Hidden);
        assert!(!b.is_ai_reconstructed());
    }

    #[test]
    fn gear_c_hides_badge() {
        let mut b = GearBadge::default();
        assert_eq!(b.update(CameraGear::GearC), BadgeState::Hidden);
        assert!(!b.is_ai_reconstructed());
    }

    #[test]
    fn camera_off_hides_badge() {
        let mut b = GearBadge::default();
        assert_eq!(b.update(CameraGear::Off), BadgeState::Hidden);
        assert!(!b.is_ai_reconstructed());
    }

    #[test]
    fn gear_b_any_preset_hides_badge() {
        let mut b = GearBadge::default();
        for preset in [10u8, 11, 12] {
            assert_eq!(
                b.update(CameraGear::GearB { svt_preset: preset }),
                BadgeState::Hidden,
                "Gear B preset {preset} must hide the badge"
            );
        }
    }

    #[test]
    fn transitions_gear_a_off_and_back() {
        let mut b = GearBadge::default();
        assert_eq!(b.update(CameraGear::GearA), BadgeState::Visible);
        assert_eq!(b.update(CameraGear::Off), BadgeState::Hidden);
        assert_eq!(b.update(CameraGear::GearA), BadgeState::Visible);
    }

    #[test]
    fn transitions_gear_a_to_b_and_back() {
        let mut b = GearBadge::default();
        assert_eq!(b.update(CameraGear::GearA), BadgeState::Visible);
        assert_eq!(b.update(CameraGear::GearB { svt_preset: 12 }), BadgeState::Hidden);
        assert_eq!(b.update(CameraGear::GearA), BadgeState::Visible);
    }

    #[test]
    fn badge_label_matches_spec() {
        assert_eq!(BADGE_LABEL, "AI-reconstructed");
    }

    #[test]
    fn badge_color_is_neural_violet() {
        assert_eq!(BADGE_COLOR, "#7c3aed");
    }
}

//! Session quality tier — emitted by the governor at 10 Hz (Feature 68).

/// Quality tier emitted by the governor each control interval.
///
/// Tiers are ordered from most-degraded to best quality.  The derived
/// [`Ord`] ordering reflects this: `Survival < Constrained < Comfortable < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TierState {
    /// Minimum viable session: voice only, all optional features shed.
    Survival,
    /// Degraded session: voice + legible screen + responsive input, reduced
    /// camera quality.  CPU ceiling of 35% applies (Feature 160).
    Constrained,
    /// Normal quality session: all features active at reduced fidelity.
    Comfortable,
    /// Full quality: all features at maximum fidelity.
    Full,
}

impl TierState {
    /// Returns `true` when the CPU ceiling must be enforced (Feature 160).
    ///
    /// The ceiling applies at `Constrained` and `Survival` so that the
    /// lowest tiers — where the link is already stressed — never over-commit
    /// CPU on the endpoint.
    #[inline]
    pub fn cpu_ceiling_active(self) -> bool {
        matches!(self, TierState::Constrained | TierState::Survival)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_survival_to_full() {
        assert!(TierState::Survival < TierState::Constrained);
        assert!(TierState::Constrained < TierState::Comfortable);
        assert!(TierState::Comfortable < TierState::Full);
    }

    #[test]
    fn cpu_ceiling_active_for_constrained_and_survival() {
        assert!(TierState::Survival.cpu_ceiling_active());
        assert!(TierState::Constrained.cpu_ceiling_active());
        assert!(!TierState::Comfortable.cpu_ceiling_active());
        assert!(!TierState::Full.cpu_ceiling_active());
    }
}

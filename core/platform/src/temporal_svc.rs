//! Temporal SVC T-layer assignment and congestion-drop controller — Feature §9.2.
//!
//! # Purpose
//!
//! For Gear B (SVT-AV1) the encoder is configured for temporal scalability.
//! Each frame is assigned to a temporal layer (T-layer); higher-layer frames
//! depend on lower-layer frames but lower-layer frames remain fully decodable
//! without the higher-layer frames.
//!
//! Under congestion the pacer withholds frames of the highest active T-layer,
//! reducing the effective send framerate while the decoder continues to decode
//! the surviving base-layer frames uninterrupted.  No IDR keyframe is emitted —
//! the rate reduction is decoder-transparent, avoiding the bitrate spike that a
//! keyframe would cause on a constrained link.
//!
//! # Layer patterns
//!
//! **L1T2** (period = 2 frames):
//!
//! | Frame mod 2 | Layer | Notes                               |
//! |-------------|-------|-------------------------------------|
//! | 0           | T0    | Base — always sent                  |
//! | 1           | T1    | Enhancement — dropped under overuse |
//!
//! Dropping T1 → 15 fps effective at 30 fps encode (50 % reduction).
//!
//! **L1T3** (period = 4 frames):
//!
//! | Frame mod 4 | Layer | Notes                                          |
//! |-------------|-------|------------------------------------------------|
//! | 0           | T0    | Base — always sent                             |
//! | 1           | T2    | High enhancement — first to be dropped         |
//! | 2           | T1    | Mid enhancement — dropped after T2             |
//! | 3           | T2    | High enhancement — dropped alongside frame 1   |
//!
//! Dropping T2 → 15 fps effective (T0 + T1, 50 % reduction).
//! Dropping T1 + T2 → 7.5 fps effective (T0 only, 75 % reduction).
//!
//! # Congestion integration
//!
//! [`TemporalSvcController::update`] is called each governor tick (10 Hz) with
//! a boolean `overuse` that is `true` when the delay-gradient estimator reports
//! [`BandwidthUsage::Overuse`](crate) or the loss-backstop controller fires.
//!
//! After [`OVERUSE_ESCALATE_TICKS`] consecutive overuse ticks the drop level
//! escalates one step.  After [`UNDERUSE_RELAX_TICKS`] consecutive non-overuse
//! ticks it relaxes one step.  This hysteresis prevents oscillation.

// ── Public constants ──────────────────────────────────────────────────────────

/// Consecutive governor ticks of overuse before escalating the drop level.
///
/// 3 × 100 ms/tick = 300 ms of confirmed overuse before widening the drop.
pub const OVERUSE_ESCALATE_TICKS: u32 = 3;

/// Consecutive governor ticks without overuse before relaxing the drop level.
///
/// 30 × 100 ms/tick = 3 s of clear conditions before recovering a T-layer.
pub const UNDERUSE_RELAX_TICKS: u32 = 30;

// ── TemporalSvcMode ──────────────────────────────────────────────────────────

/// Temporal SVC mode for Gear B (SVT-AV1) camera encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalSvcMode {
    /// 2 temporal layers.  Drop T1 → 50 % framerate reduction, decoder-transparent.
    L1T2,
    /// 3 temporal layers.  Drop T2 → 50 % reduction; drop T1+T2 → 75 % reduction.
    L1T3,
}

// ── TemporalLayerId ───────────────────────────────────────────────────────────

/// Temporal layer ID embedded in the LBTP frame header.
///
/// 0 = base layer (always forwarded); higher values = enhancement layers
/// (dropped under congestion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TemporalLayerId(pub u8);

/// Base layer — always sent, decoder requires it.
pub const T0: TemporalLayerId = TemporalLayerId(0);
/// Mid enhancement layer (used by L1T3).
pub const T1: TemporalLayerId = TemporalLayerId(1);
/// High enhancement layer (used by L1T3; the first to be dropped).
pub const T2: TemporalLayerId = TemporalLayerId(2);

/// Sentinel meaning "no frames are dropped" (drop_floor above all real layers).
const NO_DROP: u8 = u8::MAX;

// ── TemporalLayerAssigner ─────────────────────────────────────────────────────

/// Assigns temporal layer IDs to camera frames based on frame position.
///
/// Call [`next_layer`](Self::next_layer) once per encoded frame to get the layer
/// ID to embed in the LBTP frame header and pass to the drop decision.
///
/// ## Usage
///
/// ```
/// use lowband_platform::temporal_svc::{TemporalLayerAssigner, TemporalSvcMode, T0, T1};
///
/// let mut assigner = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
///
/// // L1T2 alternates T0 / T1.
/// assert_eq!(assigner.next_layer(), T0);
/// assert_eq!(assigner.next_layer(), T1);
/// assert_eq!(assigner.next_layer(), T0);
/// ```
#[derive(Debug)]
pub struct TemporalLayerAssigner {
    mode: TemporalSvcMode,
    frame_count: u64,
}

impl TemporalLayerAssigner {
    /// Create a new assigner for the given SVC mode, starting at frame 0.
    pub fn new(mode: TemporalSvcMode) -> Self {
        Self { mode, frame_count: 0 }
    }

    /// Advance to the next frame and return its temporal layer ID.
    ///
    /// The frame counter wraps at `u64::MAX` (at 30 fps this would take ~20
    /// billion years; the pattern period is preserved across the wrap).
    pub fn next_layer(&mut self) -> TemporalLayerId {
        let layer = layer_for(self.mode, self.frame_count);
        self.frame_count = self.frame_count.wrapping_add(1);
        layer
    }

    /// The SVC mode this assigner was created for.
    pub fn mode(&self) -> TemporalSvcMode {
        self.mode
    }

    /// Number of frames processed so far (the next call to `next_layer` returns
    /// the layer for frame `frame_count`).
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

/// Compute the temporal layer ID for frame `n` in the given SVC mode.
fn layer_for(mode: TemporalSvcMode, n: u64) -> TemporalLayerId {
    match mode {
        TemporalSvcMode::L1T2 => {
            // Period 2: T0, T1, T0, T1, …
            if n % 2 == 0 { T0 } else { T1 }
        }
        TemporalSvcMode::L1T3 => {
            // Period 4: T0, T2, T1, T2, …
            // Position 0 → T0 (base), 1 → T2 (high), 2 → T1 (mid), 3 → T2 (high)
            match n % 4 {
                0 => T0,
                2 => T1,
                _ => T2,
            }
        }
    }
}

// ── TemporalSvcController ─────────────────────────────────────────────────────

/// Congestion-driven T-layer drop controller for SVT-AV1 Gear B.
///
/// Integrates per-tick congestion signals into a temporal-layer drop policy and
/// per-frame drop decisions.
///
/// ## Usage
///
/// ```
/// use lowband_platform::temporal_svc::{TemporalSvcController, TemporalSvcMode, T0};
///
/// let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
///
/// // Governor tick: feed congestion signal.
/// ctrl.update(false); // no overuse
///
/// // Per encoded frame: get layer and drop decision.
/// let (layer, drop) = ctrl.next_frame();
/// assert_eq!(layer, T0);  // first frame is always T0
/// assert!(!drop);         // not dropping under normal conditions
/// ```
#[derive(Debug)]
pub struct TemporalSvcController {
    assigner: TemporalLayerAssigner,
    /// Frames with `layer_id.0 >= drop_floor` are withheld from the pacer.
    /// `NO_DROP` (u8::MAX) means all frames are forwarded.
    drop_floor: u8,
    overuse_ticks: u32,
    underuse_ticks: u32,
}

impl TemporalSvcController {
    /// Create a new controller for the given SVC mode with no active drops.
    pub fn new(mode: TemporalSvcMode) -> Self {
        Self {
            assigner: TemporalLayerAssigner::new(mode),
            drop_floor: NO_DROP,
            overuse_ticks: 0,
            underuse_ticks: 0,
        }
    }

    /// Update the drop policy from the current congestion state.
    ///
    /// Call once per governor tick (10 Hz nominal).
    ///
    /// Set `overuse = true` when:
    /// - the delay-gradient estimator returns `BandwidthUsage::Overuse`, **or**
    /// - the loss-backstop controller fired this tick.
    pub fn update(&mut self, overuse: bool) {
        if overuse {
            self.overuse_ticks += 1;
            self.underuse_ticks = 0;
            if self.overuse_ticks >= OVERUSE_ESCALATE_TICKS {
                self.escalate();
                self.overuse_ticks = 0;
            }
        } else {
            self.underuse_ticks += 1;
            self.overuse_ticks = 0;
            if self.underuse_ticks >= UNDERUSE_RELAX_TICKS {
                self.relax();
                self.underuse_ticks = 0;
            }
        }
    }

    /// Advance to the next encoded frame and return its layer ID and drop flag.
    ///
    /// Returns `(layer_id, drop)` where `drop = true` means the frame must be
    /// withheld from the pacer.  The internal frame counter always advances —
    /// even dropped frames preserve the temporal dependency structure for
    /// frames the decoder will receive.
    ///
    /// The base layer (T0) is **never** dropped regardless of congestion level.
    pub fn next_frame(&mut self) -> (TemporalLayerId, bool) {
        let layer = self.assigner.next_layer();
        let drop = layer.0 >= self.drop_floor;
        (layer, drop)
    }

    /// The current T-layer drop floor.
    ///
    /// Frames with `layer_id >= drop_floor()` are withheld from the pacer.
    /// [`TemporalLayerId(u8::MAX)`] means no frames are dropped.
    pub fn drop_floor(&self) -> TemporalLayerId {
        TemporalLayerId(self.drop_floor)
    }

    /// The fraction of encoded frames that will be forwarded to the pacer
    /// under the current drop policy.
    ///
    /// Returns a value in `(0.0, 1.0]`: 1.0 = all frames forwarded, 0.5 = half
    /// forwarded, 0.25 = base layer only for L1T3.
    pub fn active_frame_fraction(&self) -> f64 {
        match self.assigner.mode() {
            TemporalSvcMode::L1T2 => {
                if self.drop_floor <= T1.0 {
                    0.5 // only T0 sent (every other frame)
                } else {
                    1.0
                }
            }
            TemporalSvcMode::L1T3 => {
                if self.drop_floor <= T1.0 {
                    0.25 // only T0 sent (every 4th frame)
                } else if self.drop_floor <= T2.0 {
                    0.5 // T0 + T1 sent (every 2nd frame)
                } else {
                    1.0
                }
            }
        }
    }

    /// The SVC mode this controller was created for.
    pub fn mode(&self) -> TemporalSvcMode {
        self.assigner.mode()
    }

    fn escalate(&mut self) {
        self.drop_floor = match self.assigner.mode() {
            TemporalSvcMode::L1T2 => T1.0, // drop T1 (only enhancement layer)
            TemporalSvcMode::L1T3 => {
                if self.drop_floor == NO_DROP {
                    T2.0 // first escalation: drop T2 only
                } else {
                    T1.0 // second escalation: drop T1 and above
                }
            }
        };
    }

    fn relax(&mut self) {
        self.drop_floor = match self.assigner.mode() {
            TemporalSvcMode::L1T2 => NO_DROP, // fully recover
            TemporalSvcMode::L1T3 => {
                if self.drop_floor <= T1.0 {
                    T2.0 // partial recovery: restore T1, still drop T2
                } else {
                    NO_DROP // full recovery
                }
            }
        };
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── TemporalLayerAssigner: L1T2 pattern ──────────────────────────────────

    #[test]
    fn l1t2_pattern_alternates_t0_t1() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
        let layers: Vec<_> = (0..8).map(|_| a.next_layer()).collect();
        assert_eq!(layers, vec![T0, T1, T0, T1, T0, T1, T0, T1]);
    }

    #[test]
    fn l1t2_first_frame_is_t0() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
        assert_eq!(a.next_layer(), T0);
    }

    // ── TemporalLayerAssigner: L1T3 pattern ──────────────────────────────────

    #[test]
    fn l1t3_pattern_is_t0_t2_t1_t2() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
        let layers: Vec<_> = (0..8).map(|_| a.next_layer()).collect();
        assert_eq!(layers, vec![T0, T2, T1, T2, T0, T2, T1, T2]);
    }

    #[test]
    fn l1t3_first_frame_is_t0() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
        assert_eq!(a.next_layer(), T0);
    }

    #[test]
    fn l1t3_t0_appears_every_four_frames() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T3);
        for i in 0..40u64 {
            let layer = a.next_layer();
            if i % 4 == 0 {
                assert_eq!(layer, T0, "frame {i}: expected T0");
            } else {
                assert_ne!(layer, T0, "frame {i}: unexpected T0");
            }
        }
    }

    // ── TemporalLayerAssigner: frame counter ─────────────────────────────────

    #[test]
    fn frame_count_increments_each_call() {
        let mut a = TemporalLayerAssigner::new(TemporalSvcMode::L1T2);
        assert_eq!(a.frame_count(), 0);
        a.next_layer();
        assert_eq!(a.frame_count(), 1);
        a.next_layer();
        assert_eq!(a.frame_count(), 2);
    }

    // ── TemporalSvcController: initial state ─────────────────────────────────

    #[test]
    fn no_drops_at_startup() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..30 {
            let (_, drop) = ctrl.next_frame();
            assert!(!drop, "no frames should be dropped at startup (no overuse reported)");
        }
    }

    #[test]
    fn drop_floor_is_max_at_startup() {
        let ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        assert_eq!(ctrl.drop_floor(), TemporalLayerId(u8::MAX));
    }

    #[test]
    fn active_frame_fraction_is_one_at_startup() {
        let ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        assert!((ctrl.active_frame_fraction() - 1.0).abs() < f64::EPSILON);
    }

    // ── TemporalSvcController: L1T2 congestion escalation ────────────────────

    #[test]
    fn l1t2_no_drop_escalation_before_threshold() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..(OVERUSE_ESCALATE_TICKS - 1) {
            ctrl.update(true);
        }
        // Not yet at threshold — no escalation.
        assert_eq!(ctrl.drop_floor(), TemporalLayerId(u8::MAX));
    }

    #[test]
    fn l1t2_drops_t1_after_overuse_escalate_ticks() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T1, "L1T2 must drop T1 after {OVERUSE_ESCALATE_TICKS} overuse ticks");
    }

    #[test]
    fn l1t2_t0_never_dropped_under_overuse() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        // Force maximum congestion escalation.
        for _ in 0..OVERUSE_ESCALATE_TICKS * 10 {
            ctrl.update(true);
        }
        // Advance many frames — T0 must never be dropped.
        for _ in 0..100 {
            let (layer, drop) = ctrl.next_frame();
            if layer == T0 {
                assert!(!drop, "T0 (base layer) must never be dropped");
            }
        }
    }

    #[test]
    fn l1t2_active_fraction_halves_under_overuse() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert!(
            (ctrl.active_frame_fraction() - 0.5).abs() < f64::EPSILON,
            "L1T2 dropping T1 should yield 0.5 active frame fraction"
        );
    }

    // ── TemporalSvcController: L1T3 congestion escalation ────────────────────

    #[test]
    fn l1t3_first_escalation_drops_t2_only() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T2, "L1T3 first escalation must drop T2 only");
    }

    #[test]
    fn l1t3_second_escalation_drops_t1_and_above() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        // First escalation: drop T2.
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T2);
        // Second escalation: drop T1+T2.
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T1, "L1T3 second escalation must drop T1 and above");
    }

    #[test]
    fn l1t3_t0_never_dropped_under_any_escalation() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        // Saturate overuse to force maximum escalation.
        for _ in 0..OVERUSE_ESCALATE_TICKS * 10 {
            ctrl.update(true);
        }
        for _ in 0..200 {
            let (layer, drop) = ctrl.next_frame();
            if layer == T0 {
                assert!(!drop, "T0 (base layer) must never be dropped regardless of escalation");
            }
        }
    }

    #[test]
    fn l1t3_active_fraction_half_after_first_escalation() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert!(
            (ctrl.active_frame_fraction() - 0.5).abs() < f64::EPSILON,
            "L1T3 dropping T2 should yield 0.5 active frame fraction"
        );
    }

    #[test]
    fn l1t3_active_fraction_quarter_after_second_escalation() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        for _ in 0..(OVERUSE_ESCALATE_TICKS * 2) {
            ctrl.update(true);
        }
        assert!(
            (ctrl.active_frame_fraction() - 0.25).abs() < f64::EPSILON,
            "L1T3 dropping T1+T2 should yield 0.25 active frame fraction"
        );
    }

    // ── TemporalSvcController: recovery ──────────────────────────────────────

    #[test]
    fn l1t2_recovers_fully_after_underuse_relax_ticks() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T1, "must be dropping T1 before recovery");

        for _ in 0..UNDERUSE_RELAX_TICKS {
            ctrl.update(false);
        }
        assert_eq!(
            ctrl.drop_floor(),
            TemporalLayerId(u8::MAX),
            "L1T2 must fully recover after {UNDERUSE_RELAX_TICKS} non-overuse ticks"
        );
    }

    #[test]
    fn l1t3_partial_recovery_after_max_escalation() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        // Reach second escalation (drop T1+T2).
        for _ in 0..(OVERUSE_ESCALATE_TICKS * 2) {
            ctrl.update(true);
        }
        assert_eq!(ctrl.drop_floor(), T1);

        // First relax: should go from T1 → T2 (partial recovery).
        for _ in 0..UNDERUSE_RELAX_TICKS {
            ctrl.update(false);
        }
        assert_eq!(
            ctrl.drop_floor(),
            T2,
            "L1T3 must partially recover to dropping T2 only after first relax period"
        );

        // Second relax: full recovery.
        for _ in 0..UNDERUSE_RELAX_TICKS {
            ctrl.update(false);
        }
        assert_eq!(
            ctrl.drop_floor(),
            TemporalLayerId(u8::MAX),
            "L1T3 must fully recover after second relax period"
        );
    }

    #[test]
    fn no_recovery_before_relax_threshold() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        let floor_after_overuse = ctrl.drop_floor();
        for _ in 0..(UNDERUSE_RELAX_TICKS - 1) {
            ctrl.update(false);
        }
        assert_eq!(
            ctrl.drop_floor(),
            floor_after_overuse,
            "drop floor must not change before UNDERUSE_RELAX_TICKS non-overuse ticks"
        );
    }

    // ── TemporalSvcController: overuse counter resets on non-overuse ──────────

    #[test]
    fn overuse_counter_resets_on_non_overuse_tick() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        // Feed (ESCALATE - 1) overuse ticks then one non-overuse tick.
        for _ in 0..(OVERUSE_ESCALATE_TICKS - 1) {
            ctrl.update(true);
        }
        ctrl.update(false); // resets the counter
        // The next (ESCALATE - 1) ticks must not escalate.
        for _ in 0..(OVERUSE_ESCALATE_TICKS - 1) {
            ctrl.update(true);
        }
        assert_eq!(
            ctrl.drop_floor(),
            TemporalLayerId(u8::MAX),
            "overuse counter must reset on any non-overuse tick"
        );
    }

    // ── Decoder transparency: T0 sequence is contiguous ──────────────────────

    #[test]
    fn l1t2_t0_frames_are_contiguous_under_t1_drop() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T2);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        // Collect 20 frames; every forwarded frame must be T0.
        let sent: Vec<TemporalLayerId> = (0..20)
            .filter_map(|_| {
                let (layer, drop) = ctrl.next_frame();
                if drop { None } else { Some(layer) }
            })
            .collect();
        assert!(!sent.is_empty());
        for layer in &sent {
            assert_eq!(*layer, T0, "under L1T2 T1-drop only T0 frames must be forwarded");
        }
    }

    #[test]
    fn l1t3_sent_frames_are_decodable_subset_under_t2_drop() {
        let mut ctrl = TemporalSvcController::new(TemporalSvcMode::L1T3);
        for _ in 0..OVERUSE_ESCALATE_TICKS {
            ctrl.update(true);
        }
        // Only T0 and T1 should be forwarded; no T2.
        for _ in 0..40 {
            let (layer, drop) = ctrl.next_frame();
            if !drop {
                assert!(
                    layer <= T1,
                    "under L1T3 T2-drop only T0/T1 frames may be forwarded; got {layer:?}"
                );
            }
        }
    }
}

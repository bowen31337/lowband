//! Model warm pool — Feature 85.
//!
//! # Problem
//!
//! Cold-starting an ONNX model — allocating weights, creating an inference
//! session, setting up the execution-provider context — takes tens to hundreds
//! of milliseconds.  Without pre-warming, a gear switch to the neural
//! talking-head codec (Gear A) would delay the first encoded frame by that
//! full cold-start cost.
//!
//! # Solution
//!
//! The warm pool maintains one [`WarmEntry`] per [`ModelId`].  Before the
//! governor commits a switch to Gear A it checks [`WarmPool::is_gear_a_ready`]:
//! if every model Gear A needs is already [`WarmState::Warm`], the switch
//! completes with **zero additional latency**.  If any model is still
//! [`WarmState::Cold`] the governor pre-warms it in the background before
//! committing the switch.
//!
//! # Models per gear
//!
//! | Gear            | Neural models required                                        |
//! |-----------------|---------------------------------------------------------------|
//! | Gear A (neural) | [`ModelId::KeypointExtractor`], [`ModelId::SynthesisNetwork`] |
//! | Gear B (AV1)    | *(none — no neural models)*                                   |
//! | Gear C (H.264)  | *(none — no neural models)*                                   |
//!
//! Audio models (`NoiseSuppressor`, `NeuralVocoder`, `NeuralPlc`) are tracked
//! in the pool so they can be pre-warmed alongside camera gear models.
//!
//! # Thread safety
//!
//! [`WarmPool`] is `Send` but **not** `Sync`.  It is owned and mutated by a
//! single thread (the governor); sharing across threads requires external locking.

use crate::eval_card::ModelId;

// ── WarmState ─────────────────────────────────────────────────────────────────

/// Whether a model slot is ready for zero-latency inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmState {
    /// Model is not loaded.  The first inference will pay the full cold-start
    /// cost (session creation + weight load = tens to hundreds of ms).
    Cold,
    /// Model is pre-loaded and ready.  A gear switch to any codec that uses
    /// this model adds no cold-start latency beyond the inference itself.
    Warm,
}

// ── WarmEntry ─────────────────────────────────────────────────────────────────

/// A single slot in the warm pool for one [`ModelId`].
#[derive(Debug, Clone, Copy)]
pub struct WarmEntry {
    /// Which model this entry tracks.
    pub model_id: ModelId,
    /// Current warmth state.
    pub state: WarmState,
}

// ── Gear A model set ──────────────────────────────────────────────────────────

/// Models required by Gear A (neural talking-head codec).
///
/// [`WarmPool::is_gear_a_ready`] returns `true` when every model in this slice
/// is [`WarmState::Warm`], guaranteeing that a switch to Gear A adds no
/// cold-start latency.
pub const GEAR_A_MODELS: &[ModelId] = &[
    ModelId::KeypointExtractor,
    ModelId::SynthesisNetwork,
];

// ── WarmPool ──────────────────────────────────────────────────────────────────

const N_MODELS: usize = 5; // must match ModelId::ALL.len()

/// Pre-allocated warm pool that eliminates cold-start latency on gear switches.
///
/// The pool holds one [`WarmEntry`] per [`ModelId`] in [`ModelId::ALL`] order.
/// All entries start [`WarmState::Cold`]; the governor warms them proactively
/// between gear switches.  When every model a target gear needs is
/// [`WarmState::Warm`], the gear switch adds no cold-start latency.
///
/// ## Typical lifecycle
///
/// 1. `WarmPool::new()` at startup — all entries are `Cold`.
/// 2. Session idles on Gear B; runtime background-loads Gear A models.
/// 3. Governor calls `warm(ModelId::KeypointExtractor)` and
///    `warm(ModelId::SynthesisNetwork)` after each model loads successfully.
/// 4. Next governor tick: `is_gear_a_ready()` returns `true` — the switch
///    to Gear A commits with **zero cold-start latency**.
#[derive(Debug, Clone)]
pub struct WarmPool {
    entries: [WarmEntry; N_MODELS],
}

impl WarmPool {
    /// Create a new pool with all models in [`WarmState::Cold`].
    pub fn new() -> Self {
        Self {
            entries: [
                WarmEntry { model_id: ModelId::NoiseSuppressor,   state: WarmState::Cold },
                WarmEntry { model_id: ModelId::NeuralVocoder,     state: WarmState::Cold },
                WarmEntry { model_id: ModelId::NeuralPlc,         state: WarmState::Cold },
                WarmEntry { model_id: ModelId::KeypointExtractor, state: WarmState::Cold },
                WarmEntry { model_id: ModelId::SynthesisNetwork,  state: WarmState::Cold },
            ],
        }
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    /// Mark `model_id` as [`WarmState::Warm`].
    ///
    /// Call this after the background loader successfully creates an ONNX
    /// inference session and runs at least one warm-up inference.
    pub fn warm(&mut self, model_id: ModelId) {
        self.entry_mut(model_id).state = WarmState::Warm;
    }

    /// Mark `model_id` as [`WarmState::Cold`].
    ///
    /// Call this when the inference session is closed or evicted from memory
    /// (e.g. after a sustained period on Gear B with no Gear A frames needed).
    pub fn evict(&mut self, model_id: ModelId) {
        self.entry_mut(model_id).state = WarmState::Cold;
    }

    /// Mark every model in the registry as [`WarmState::Warm`].
    ///
    /// Intended for startup scenarios where all models are pre-loaded before
    /// the first governor tick, and for test fixtures.
    pub fn warm_all(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.state = WarmState::Warm;
        }
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    /// Return the current [`WarmState`] for `model_id`.
    pub fn state(&self, model_id: ModelId) -> WarmState {
        self.entry(model_id).state
    }

    /// Return `true` if `model_id` is [`WarmState::Warm`].
    pub fn is_warm(&self, model_id: ModelId) -> bool {
        self.state(model_id) == WarmState::Warm
    }

    /// Return `true` if `model_id` is [`WarmState::Cold`].
    pub fn is_cold(&self, model_id: ModelId) -> bool {
        self.state(model_id) == WarmState::Cold
    }

    /// Number of models currently in [`WarmState::Warm`].
    pub fn warm_count(&self) -> usize {
        self.entries.iter().filter(|e| e.state == WarmState::Warm).count()
    }

    /// Return `true` when every model required by Gear A is [`WarmState::Warm`].
    ///
    /// When this returns `true` the governor may commit a switch to the neural
    /// talking-head codec without any cold-start latency on the first frame.
    pub fn is_gear_a_ready(&self) -> bool {
        GEAR_A_MODELS.iter().all(|&id| self.is_warm(id))
    }

    /// Warm entries for the two models Gear A requires, in [`GEAR_A_MODELS`] order.
    ///
    /// Useful for diagnostic logging before a gear switch.
    pub fn gear_a_entries(&self) -> [WarmEntry; 2] {
        [
            *self.entry(ModelId::KeypointExtractor),
            *self.entry(ModelId::SynthesisNetwork),
        ]
    }

    /// All pool entries in [`ModelId::ALL`] order.
    pub fn entries(&self) -> &[WarmEntry; N_MODELS] {
        &self.entries
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn index(model_id: ModelId) -> usize {
        match model_id {
            ModelId::NoiseSuppressor   => 0,
            ModelId::NeuralVocoder     => 1,
            ModelId::NeuralPlc         => 2,
            ModelId::KeypointExtractor => 3,
            ModelId::SynthesisNetwork  => 4,
        }
    }

    fn entry(&self, model_id: ModelId) -> &WarmEntry {
        &self.entries[Self::index(model_id)]
    }

    fn entry_mut(&mut self, model_id: ModelId) -> &mut WarmEntry {
        &mut self.entries[Self::index(model_id)]
    }
}

impl Default for WarmPool {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Initial state ─────────────────────────────────────────────────────────

    #[test]
    fn new_pool_all_cold() {
        let pool = WarmPool::new();
        for &id in ModelId::ALL {
            assert_eq!(
                pool.state(id),
                WarmState::Cold,
                "{id:?} must start Cold"
            );
        }
    }

    #[test]
    fn new_pool_warm_count_is_zero() {
        let pool = WarmPool::new();
        assert_eq!(pool.warm_count(), 0, "no models warm at construction");
    }

    #[test]
    fn new_pool_gear_a_not_ready() {
        let pool = WarmPool::new();
        assert!(!pool.is_gear_a_ready(), "Gear A must not be ready when all models are cold");
    }

    // ── warm / evict ──────────────────────────────────────────────────────────

    #[test]
    fn warm_sets_model_to_warm() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);
        assert_eq!(pool.state(ModelId::KeypointExtractor), WarmState::Warm);
        assert!(pool.is_warm(ModelId::KeypointExtractor));
        assert!(!pool.is_cold(ModelId::KeypointExtractor));
    }

    #[test]
    fn warm_does_not_affect_other_models() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::NoiseSuppressor);
        for &id in ModelId::ALL {
            if id != ModelId::NoiseSuppressor {
                assert_eq!(
                    pool.state(id),
                    WarmState::Cold,
                    "warming NoiseSuppressor must not change {id:?}"
                );
            }
        }
    }

    #[test]
    fn evict_returns_model_to_cold() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::SynthesisNetwork);
        assert_eq!(pool.state(ModelId::SynthesisNetwork), WarmState::Warm);

        pool.evict(ModelId::SynthesisNetwork);
        assert_eq!(
            pool.state(ModelId::SynthesisNetwork),
            WarmState::Cold,
            "evict must return the model to Cold"
        );
    }

    #[test]
    fn warm_count_increments_per_warm_call() {
        let mut pool = WarmPool::new();
        assert_eq!(pool.warm_count(), 0);

        pool.warm(ModelId::NoiseSuppressor);
        assert_eq!(pool.warm_count(), 1);

        pool.warm(ModelId::NeuralVocoder);
        assert_eq!(pool.warm_count(), 2);

        pool.warm(ModelId::NeuralPlc);
        assert_eq!(pool.warm_count(), 3);
    }

    #[test]
    fn warm_count_decrements_on_evict() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);
        pool.warm(ModelId::SynthesisNetwork);
        assert_eq!(pool.warm_count(), 2);

        pool.evict(ModelId::KeypointExtractor);
        assert_eq!(pool.warm_count(), 1);

        pool.evict(ModelId::SynthesisNetwork);
        assert_eq!(pool.warm_count(), 0);
    }

    // ── warm_all ──────────────────────────────────────────────────────────────

    #[test]
    fn warm_all_sets_every_model_warm() {
        let mut pool = WarmPool::new();
        pool.warm_all();
        for &id in ModelId::ALL {
            assert_eq!(pool.state(id), WarmState::Warm, "{id:?} must be Warm after warm_all");
        }
    }

    #[test]
    fn warm_all_warm_count_equals_registry_length() {
        let mut pool = WarmPool::new();
        pool.warm_all();
        assert_eq!(pool.warm_count(), ModelId::ALL.len());
    }

    // ── is_gear_a_ready ───────────────────────────────────────────────────────

    #[test]
    fn gear_a_not_ready_with_only_keypoint_extractor_warm() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);
        assert!(
            !pool.is_gear_a_ready(),
            "Gear A must not be ready when SynthesisNetwork is still cold"
        );
    }

    #[test]
    fn gear_a_not_ready_with_only_synthesis_network_warm() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::SynthesisNetwork);
        assert!(
            !pool.is_gear_a_ready(),
            "Gear A must not be ready when KeypointExtractor is still cold"
        );
    }

    #[test]
    fn gear_a_ready_when_both_models_warm() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);
        pool.warm(ModelId::SynthesisNetwork);
        assert!(
            pool.is_gear_a_ready(),
            "Gear A must be ready when both KeypointExtractor and SynthesisNetwork are warm"
        );
    }

    #[test]
    fn gear_a_not_ready_after_evicting_one_model() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);
        pool.warm(ModelId::SynthesisNetwork);
        assert!(pool.is_gear_a_ready(), "precondition: Gear A must be ready");

        pool.evict(ModelId::SynthesisNetwork);
        assert!(
            !pool.is_gear_a_ready(),
            "evicting SynthesisNetwork must make Gear A not ready"
        );
    }

    #[test]
    fn gear_a_ready_when_all_models_warm() {
        let mut pool = WarmPool::new();
        pool.warm_all();
        assert!(pool.is_gear_a_ready());
    }

    // ── GEAR_A_MODELS constant ────────────────────────────────────────────────

    #[test]
    fn gear_a_models_contains_keypoint_extractor() {
        assert!(
            GEAR_A_MODELS.contains(&ModelId::KeypointExtractor),
            "GEAR_A_MODELS must include KeypointExtractor (Feature 118)"
        );
    }

    #[test]
    fn gear_a_models_contains_synthesis_network() {
        assert!(
            GEAR_A_MODELS.contains(&ModelId::SynthesisNetwork),
            "GEAR_A_MODELS must include SynthesisNetwork (Feature 120)"
        );
    }

    #[test]
    fn gear_a_models_has_exactly_two_entries() {
        assert_eq!(
            GEAR_A_MODELS.len(),
            2,
            "Gear A requires exactly two neural models: keypoint extractor + synthesis network"
        );
    }

    // ── gear_a_entries ────────────────────────────────────────────────────────

    #[test]
    fn gear_a_entries_reflect_current_state() {
        let mut pool = WarmPool::new();
        pool.warm(ModelId::KeypointExtractor);

        let entries = pool.gear_a_entries();
        assert_eq!(entries[0].model_id, ModelId::KeypointExtractor);
        assert_eq!(entries[0].state, WarmState::Warm);
        assert_eq!(entries[1].model_id, ModelId::SynthesisNetwork);
        assert_eq!(entries[1].state, WarmState::Cold);
    }

    // ── entries snapshot ──────────────────────────────────────────────────────

    #[test]
    fn entries_slice_has_one_per_model_id() {
        let pool = WarmPool::new();
        assert_eq!(pool.entries().len(), ModelId::ALL.len());
    }

    #[test]
    fn entries_model_ids_match_all_order() {
        let pool = WarmPool::new();
        for (entry, &id) in pool.entries().iter().zip(ModelId::ALL.iter()) {
            assert_eq!(entry.model_id, id, "entry order must match ModelId::ALL");
        }
    }

    // ── default ───────────────────────────────────────────────────────────────

    #[test]
    fn default_equals_new() {
        let a = WarmPool::new();
        let b = WarmPool::default();
        for &id in ModelId::ALL {
            assert_eq!(a.state(id), b.state(id));
        }
    }
}

//! Feature 85 — System keeps models ready with warm_pool entries so a gear
//! switch adds no cold-start latency.
//!
//! # What this test verifies
//!
//! 1. **Cold start at pool construction** — every model starts [`WarmState::Cold`]
//!    on a freshly created [`WarmPool`], so no model is falsely reported as ready.
//!
//! 2. **Incremental warm-up** — individual calls to [`WarmPool::warm`] transition
//!    exactly the named model to [`WarmState::Warm`] without affecting others.
//!
//! 3. **Gear A readiness gate** — [`WarmPool::is_gear_a_ready`] returns `false`
//!    until *both* [`ModelId::KeypointExtractor`] and [`ModelId::SynthesisNetwork`]
//!    are warm; the gate prevents a premature switch that would incur cold-start
//!    latency on the synthesis network, the slowest model in the registry.
//!
//! 4. **Zero-latency gear switch** — once `is_gear_a_ready()` is `true` the
//!    governor may commit the Gear A switch without any additional load time.
//!    The simulation verifies this end-to-end.
//!
//! 5. **Eviction clears readiness** — calling [`WarmPool::evict`] on either
//!    Gear A model makes `is_gear_a_ready()` return `false` again, preventing
//!    the governor from switching to Gear A with a partially-loaded model set.
//!
//! 6. **`warm_all` saturates the pool** — after [`WarmPool::warm_all`] every
//!    model is [`WarmState::Warm`] and the warm count equals the registry length.
//!
//! 7. **`GEAR_A_MODELS` correctness** — the constant names exactly the two
//!    models Gear A depends on: keypoint extractor (Feature 118) and synthesis
//!    network (Feature 120).
//!
//! 8. **Entries snapshot matches state** — [`WarmPool::entries`] and
//!    [`WarmPool::gear_a_entries`] reflect the live pool state rather than a
//!    cached snapshot.
//!
//! # Simulation
//!
//! A session on Gear B (SVT-AV1) runs 30 governor ticks.  During ticks 5 and
//! 10 the background loader reports that the keypoint extractor and synthesis
//! network have finished loading.  The test verifies that the governor cannot
//! commit a Gear A switch before tick 10, and can switch at tick 10 with zero
//! cold-start cost.

use lowband_nn::{
    warm_pool::{WarmPool, WarmState, GEAR_A_MODELS},
    ModelId,
};

// ── 1. Cold start at pool construction ───────────────────────────────────────

#[test]
fn new_pool_all_models_start_cold() {
    let pool = WarmPool::new();
    for &id in ModelId::ALL {
        assert_eq!(
            pool.state(id),
            WarmState::Cold,
            "{id:?} must be Cold immediately after WarmPool::new()"
        );
        assert!(pool.is_cold(id), "is_cold must agree with state() for {id:?}");
        assert!(!pool.is_warm(id), "is_warm must be false for a cold model ({id:?})");
    }
}

#[test]
fn new_pool_warm_count_is_zero() {
    let pool = WarmPool::new();
    assert_eq!(
        pool.warm_count(),
        0,
        "warm_count must be 0 at construction — no model has been pre-loaded"
    );
}

#[test]
fn new_pool_gear_a_not_ready() {
    let pool = WarmPool::new();
    assert!(
        !pool.is_gear_a_ready(),
        "is_gear_a_ready must be false when all models are cold — \
         switching to Gear A now would incur full cold-start latency"
    );
}

// ── 2. Incremental warm-up ────────────────────────────────────────────────────

#[test]
fn warm_sets_named_model_to_warm() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::NoiseSuppressor);

    assert_eq!(pool.state(ModelId::NoiseSuppressor), WarmState::Warm);
    assert!(pool.is_warm(ModelId::NoiseSuppressor));
    assert!(!pool.is_cold(ModelId::NoiseSuppressor));
}

#[test]
fn warm_does_not_affect_other_models() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::NeuralVocoder);

    for &id in ModelId::ALL {
        if id == ModelId::NeuralVocoder {
            continue;
        }
        assert_eq!(
            pool.state(id),
            WarmState::Cold,
            "warming NeuralVocoder must leave {id:?} untouched"
        );
    }
}

#[test]
fn warm_count_increments_for_each_unique_model_warmed() {
    let mut pool = WarmPool::new();
    let mut expected = 0;

    for &id in ModelId::ALL {
        pool.warm(id);
        expected += 1;
        assert_eq!(
            pool.warm_count(),
            expected,
            "warm_count must be {expected} after warming {id:?}"
        );
    }
}

#[test]
fn warming_already_warm_model_does_not_change_count() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::NeuralPlc);
    assert_eq!(pool.warm_count(), 1, "precondition");

    pool.warm(ModelId::NeuralPlc); // warm again
    assert_eq!(
        pool.warm_count(),
        1,
        "warming an already-warm model must not inflate warm_count"
    );
}

// ── 3. Gear A readiness gate ──────────────────────────────────────────────────

#[test]
fn gear_a_not_ready_with_only_keypoint_extractor_warm() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);

    assert!(
        !pool.is_gear_a_ready(),
        "Gear A must not be ready when SynthesisNetwork (Feature 120) is still cold"
    );
}

#[test]
fn gear_a_not_ready_with_only_synthesis_network_warm() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::SynthesisNetwork);

    assert!(
        !pool.is_gear_a_ready(),
        "Gear A must not be ready when KeypointExtractor (Feature 118) is still cold"
    );
}

#[test]
fn gear_a_ready_when_both_required_models_warm() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);
    pool.warm(ModelId::SynthesisNetwork);

    assert!(
        pool.is_gear_a_ready(),
        "Gear A must be ready once both KeypointExtractor and SynthesisNetwork are warm"
    );
}

#[test]
fn non_gear_a_models_warm_do_not_satisfy_gear_a_gate() {
    let mut pool = WarmPool::new();
    // Warm every audio model — still not enough for Gear A.
    pool.warm(ModelId::NoiseSuppressor);
    pool.warm(ModelId::NeuralVocoder);
    pool.warm(ModelId::NeuralPlc);

    assert!(
        !pool.is_gear_a_ready(),
        "audio models alone must not satisfy the Gear A readiness gate"
    );
}

// ── 4. Zero-latency gear switch simulation ────────────────────────────────────

#[test]
fn gear_switch_simulation_no_cold_start_when_ready() {
    // Simulate 30 governor ticks on Gear B.  Background loader warms models
    // during ticks 5 (KeypointExtractor) and 10 (SynthesisNetwork).
    // The governor must not attempt Gear A before both are warm.

    let mut pool = WarmPool::new();
    let mut gear_a_allowed_tick: Option<u32> = None;

    for tick in 0..30u32 {
        if tick == 5 {
            pool.warm(ModelId::KeypointExtractor);
        }
        if tick == 10 {
            pool.warm(ModelId::SynthesisNetwork);
        }

        if pool.is_gear_a_ready() && gear_a_allowed_tick.is_none() {
            gear_a_allowed_tick = Some(tick);
        }
    }

    // Gear A becomes ready at tick 10 (both models loaded).
    assert_eq!(
        gear_a_allowed_tick,
        Some(10),
        "Gear A switch must become allowed at tick 10, not before"
    );

    // At the allowed tick both Gear A models are warm: zero cold-start.
    assert!(pool.is_warm(ModelId::KeypointExtractor));
    assert!(pool.is_warm(ModelId::SynthesisNetwork));
}

#[test]
fn gear_a_switch_before_ready_would_cause_cold_start() {
    // Control case: show that switching before readiness means at least one
    // model is cold — the test documents the latency hazard that the warm pool
    // is designed to prevent.
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor); // only one of the two Gear A models

    assert!(!pool.is_gear_a_ready(), "pool is not ready");
    // SynthesisNetwork is still cold — switching now would cold-start it.
    assert!(
        pool.is_cold(ModelId::SynthesisNetwork),
        "SynthesisNetwork would cold-start if the gear switch were committed now"
    );
}

// ── 5. Eviction clears readiness ──────────────────────────────────────────────

#[test]
fn evicting_keypoint_extractor_breaks_gear_a_readiness() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);
    pool.warm(ModelId::SynthesisNetwork);
    assert!(pool.is_gear_a_ready(), "precondition: Gear A must be ready");

    pool.evict(ModelId::KeypointExtractor);
    assert!(
        !pool.is_gear_a_ready(),
        "evicting KeypointExtractor must make is_gear_a_ready() false"
    );
    assert_eq!(pool.state(ModelId::KeypointExtractor), WarmState::Cold);
}

#[test]
fn evicting_synthesis_network_breaks_gear_a_readiness() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);
    pool.warm(ModelId::SynthesisNetwork);
    assert!(pool.is_gear_a_ready(), "precondition: Gear A must be ready");

    pool.evict(ModelId::SynthesisNetwork);
    assert!(
        !pool.is_gear_a_ready(),
        "evicting SynthesisNetwork must make is_gear_a_ready() false"
    );
    assert_eq!(pool.state(ModelId::SynthesisNetwork), WarmState::Cold);
}

#[test]
fn evict_decrements_warm_count() {
    let mut pool = WarmPool::new();
    pool.warm_all();
    let all_warm = pool.warm_count();

    pool.evict(ModelId::NeuralVocoder);
    assert_eq!(
        pool.warm_count(),
        all_warm - 1,
        "evict must decrement warm_count by 1"
    );
}

#[test]
fn re_warm_after_evict_restores_readiness() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);
    pool.warm(ModelId::SynthesisNetwork);
    pool.evict(ModelId::SynthesisNetwork);
    assert!(!pool.is_gear_a_ready(), "precondition: not ready after evict");

    pool.warm(ModelId::SynthesisNetwork);
    assert!(
        pool.is_gear_a_ready(),
        "re-warming SynthesisNetwork must restore Gear A readiness"
    );
}

// ── 6. warm_all saturates the pool ───────────────────────────────────────────

#[test]
fn warm_all_makes_every_model_warm() {
    let mut pool = WarmPool::new();
    pool.warm_all();

    for &id in ModelId::ALL {
        assert_eq!(
            pool.state(id),
            WarmState::Warm,
            "{id:?} must be Warm after warm_all()"
        );
    }
}

#[test]
fn warm_all_warm_count_equals_registry_size() {
    let mut pool = WarmPool::new();
    pool.warm_all();
    assert_eq!(
        pool.warm_count(),
        ModelId::ALL.len(),
        "after warm_all() warm_count must equal the number of ModelId variants"
    );
}

#[test]
fn warm_all_makes_gear_a_ready() {
    let mut pool = WarmPool::new();
    pool.warm_all();
    assert!(pool.is_gear_a_ready(), "warm_all must make Gear A ready");
}

// ── 7. GEAR_A_MODELS correctness ─────────────────────────────────────────────

#[test]
fn gear_a_models_contains_keypoint_extractor() {
    assert!(
        GEAR_A_MODELS.contains(&ModelId::KeypointExtractor),
        "GEAR_A_MODELS must include KeypointExtractor — Feature 118 requires it for Gear A"
    );
}

#[test]
fn gear_a_models_contains_synthesis_network() {
    assert!(
        GEAR_A_MODELS.contains(&ModelId::SynthesisNetwork),
        "GEAR_A_MODELS must include SynthesisNetwork — Feature 120 requires it for Gear A"
    );
}

#[test]
fn gear_a_models_has_exactly_two_entries() {
    assert_eq!(
        GEAR_A_MODELS.len(),
        2,
        "Gear A requires exactly two neural models: keypoint extractor + synthesis network; \
         found {} in GEAR_A_MODELS",
        GEAR_A_MODELS.len()
    );
}

#[test]
fn gear_a_models_are_all_neural_video_models() {
    // Gear A models must be the video inference models, not audio models.
    let audio_models = [
        ModelId::NoiseSuppressor,
        ModelId::NeuralVocoder,
        ModelId::NeuralPlc,
    ];
    for &audio_id in &audio_models {
        assert!(
            !GEAR_A_MODELS.contains(&audio_id),
            "audio model {audio_id:?} must not be in GEAR_A_MODELS — \
             Gear A readiness is gated only on video inference models"
        );
    }
}

// ── 8. Entries snapshot ───────────────────────────────────────────────────────

#[test]
fn entries_returns_one_slot_per_model_id() {
    let pool = WarmPool::new();
    assert_eq!(
        pool.entries().len(),
        ModelId::ALL.len(),
        "entries() must return one WarmEntry per ModelId variant"
    );
}

#[test]
fn entries_model_ids_follow_all_order() {
    let pool = WarmPool::new();
    for (entry, &expected_id) in pool.entries().iter().zip(ModelId::ALL.iter()) {
        assert_eq!(
            entry.model_id,
            expected_id,
            "entries() order must match ModelId::ALL order"
        );
    }
}

#[test]
fn gear_a_entries_reflect_live_state() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);

    let entries = pool.gear_a_entries();
    assert_eq!(entries[0].model_id, ModelId::KeypointExtractor);
    assert_eq!(entries[0].state, WarmState::Warm, "KeypointExtractor entry must show Warm");
    assert_eq!(entries[1].model_id, ModelId::SynthesisNetwork);
    assert_eq!(entries[1].state, WarmState::Cold, "SynthesisNetwork entry must show Cold");
}

#[test]
fn entries_state_matches_is_warm_for_all_models() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::NoiseSuppressor);
    pool.warm(ModelId::SynthesisNetwork);

    for entry in pool.entries() {
        let expected = pool.state(entry.model_id);
        assert_eq!(
            entry.state,
            expected,
            "entries()[{:?}].state must match state({:?})",
            entry.model_id,
            entry.model_id
        );
    }
}

// ── Default / Clone ───────────────────────────────────────────────────────────

#[test]
fn default_pool_equals_new() {
    let a = WarmPool::new();
    let b = WarmPool::default();
    for &id in ModelId::ALL {
        assert_eq!(
            a.state(id),
            b.state(id),
            "WarmPool::default() must equal WarmPool::new() for {id:?}"
        );
    }
}

#[test]
fn clone_pool_is_independent() {
    let mut pool = WarmPool::new();
    pool.warm(ModelId::KeypointExtractor);

    let mut clone = pool.clone();
    clone.warm(ModelId::SynthesisNetwork);

    // The original must not see the mutation made to the clone.
    assert!(
        !pool.is_warm(ModelId::SynthesisNetwork),
        "mutating a cloned pool must not affect the original"
    );
    assert!(clone.is_warm(ModelId::SynthesisNetwork));
}

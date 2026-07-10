//! Feature 81 — System guards the transport loop with model_watchdog
//! supervision so a stalled model never stalls it.
//!
//! # What this test verifies
//!
//! 1. **Fast inference passes through** — an inference closure that returns
//!    immediately yields `Ok(result)` with the correct value.
//!
//! 2. **Stalled inference fires the watchdog** — a closure that sleeps past
//!    the deadline yields `Err(InferenceTimeout)`.
//!
//! 3. **Timeout carries the correct model_id** — the `InferenceTimeout` error
//!    identifies which model stalled.
//!
//! 4. **Transport loop recovers after a stall** — the watchdog is not left in
//!    a broken state; subsequent fast inferences on the same instance succeed.
//!
//! 5. **Deadline does not exceed one governor tick** — `INFERENCE_DEADLINE_MS`
//!    is < 100 ms so a stall is always detected within a single 10 Hz
//!    governor interval.
//!
//! 6. **Deadline exceeds the slowest model p50** — `INFERENCE_DEADLINE_MS`
//!    is above the synthesis network's 18 ms p50, preventing false positives
//!    under normal operating conditions.
//!
//! 7. **Watchdog returns promptly after the deadline fires** — timing the
//!    watchdog call confirms that the caller is unblocked within 2× the
//!    configured deadline, not after the stalled thread eventually finishes.
//!
//! 8. **`ModelWatchdog::new()` uses the production constant** — verifies that
//!    the default constructor is wired to `INFERENCE_DEADLINE_MS`.

use std::time::{Duration, Instant};

use lowband_nn::{
    model_watchdog::{InferenceTimeout, ModelWatchdog, INFERENCE_DEADLINE_MS},
    ModelId,
};

/// Short deadline used in all integration tests to keep the suite fast.
/// Must be long enough that fast closures always complete; short enough
/// that the stall tests don't make the suite slow.
const TEST_DEADLINE_MS: u64 = 30;

fn test_watchdog() -> ModelWatchdog {
    ModelWatchdog::with_deadline(Duration::from_millis(TEST_DEADLINE_MS))
}

// ── 1. Fast inference passes through ─────────────────────────────────────────

#[test]
fn fast_u32_inference_returns_ok() {
    let wdog = test_watchdog();
    let result = wdog.run(ModelId::NoiseSuppressor, || 7u32);
    assert_eq!(result, Ok(7u32), "immediate inference must return Ok with the correct value");
}

#[test]
fn fast_vec_inference_result_preserved() {
    let wdog = test_watchdog();
    let expected = vec![10u8, 20, 30];
    let result = wdog.run(ModelId::NeuralVocoder, || vec![10u8, 20, 30]);
    assert_eq!(result, Ok(expected), "inference result must be forwarded through the channel");
}

#[test]
fn fast_bool_inference_returns_ok() {
    let wdog = test_watchdog();
    assert_eq!(wdog.run(ModelId::NeuralPlc, || true), Ok(true));
    assert_eq!(wdog.run(ModelId::NeuralPlc, || false), Ok(false));
}

#[test]
fn multiple_sequential_fast_inferences_succeed() {
    let wdog = test_watchdog();
    for i in 0u32..5 {
        let result = wdog.run(ModelId::NeuralVocoder, move || i * i);
        assert_eq!(result, Ok(i * i), "inference {i} must succeed");
    }
}

// ── 2. Stalled inference fires the watchdog ───────────────────────────────────

#[test]
fn stalled_inference_returns_inference_timeout() {
    let wdog = test_watchdog();
    let result = wdog.run(ModelId::KeypointExtractor, || {
        std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        0u32
    });
    assert!(
        matches!(result, Err(InferenceTimeout { .. })),
        "a stalled model must return Err(InferenceTimeout); got {result:?}"
    );
}

// ── 3. Timeout carries the correct model_id ───────────────────────────────────

#[test]
fn timeout_error_carries_model_id_noise_suppressor() {
    let wdog = test_watchdog();
    let Err(err) = wdog.run(ModelId::NoiseSuppressor, || {
        std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
    }) else {
        panic!("expected Err(InferenceTimeout)");
    };
    assert_eq!(
        err.model_id,
        ModelId::NoiseSuppressor,
        "timeout must carry the model_id passed to run()"
    );
}

#[test]
fn timeout_error_carries_model_id_synthesis_network() {
    let wdog = test_watchdog();
    let Err(err) = wdog.run(ModelId::SynthesisNetwork, || {
        std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
    }) else {
        panic!("expected Err(InferenceTimeout)");
    };
    assert_eq!(err.model_id, ModelId::SynthesisNetwork);
}

#[test]
fn timeout_error_carries_configured_deadline_ms() {
    let wdog = test_watchdog();
    let Err(err) = wdog.run(ModelId::NeuralPlc, || {
        std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
    }) else {
        panic!("expected Err(InferenceTimeout)");
    };
    assert_eq!(
        err.deadline_ms, TEST_DEADLINE_MS,
        "InferenceTimeout.deadline_ms must equal the watchdog's configured deadline"
    );
}

// ── 4. Transport loop recovers after a stall ──────────────────────────────────

#[test]
fn transport_loop_continues_after_stall() {
    // Simulate one stalled model inference followed by a fast one.
    // The watchdog must remain fully functional after a timeout.
    let wdog = test_watchdog();

    // Stall: triggers the watchdog.
    let timeout_result = wdog.run(ModelId::SynthesisNetwork, || {
        std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
    });
    assert!(
        matches!(timeout_result, Err(InferenceTimeout { .. })),
        "first (stalled) call must return InferenceTimeout"
    );

    // Recovery: the watchdog must not be in a broken state.
    let fast_result = wdog.run(ModelId::NoiseSuppressor, || 42u32);
    assert_eq!(
        fast_result,
        Ok(42u32),
        "watchdog must remain functional after a timeout — the transport loop \
         must be able to continue with subsequent inferences"
    );
}

#[test]
fn multiple_stalls_then_recovery() {
    let wdog = test_watchdog();

    for _ in 0..2 {
        let _ = wdog.run(ModelId::KeypointExtractor, || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        });
    }

    // Must still work after multiple stalls.
    let result = wdog.run(ModelId::NeuralVocoder, || 123u32);
    assert_eq!(result, Ok(123u32), "watchdog must remain functional after multiple timeouts");
}

// ── 5. Deadline does not exceed one governor tick ─────────────────────────────

#[test]
fn inference_deadline_ms_is_below_one_governor_tick() {
    // The governor tick period is 100 ms.  A stall must be detected within
    // one tick so the transport loop never loses more than one interval.
    const GOVERNOR_TICK_MS: u64 = 100;
    assert!(
        INFERENCE_DEADLINE_MS < GOVERNOR_TICK_MS,
        "INFERENCE_DEADLINE_MS ({INFERENCE_DEADLINE_MS} ms) must be strictly less than \
         one governor tick ({GOVERNOR_TICK_MS} ms)"
    );
}

// ── 6. Deadline exceeds the slowest model p50 ─────────────────────────────────

#[test]
fn inference_deadline_ms_exceeds_synthesis_network_p50() {
    // The synthesis network is the slowest model in the eval-card registry:
    // p50 = 18 ms on the 2015-class reference CPU.  The deadline must be
    // above this value so normal-speed inferences are never timed out.
    let synthesis_p50_ms = lowband_nn::eval_card(lowband_nn::ModelId::SynthesisNetwork)
        .inference_p50_ms;
    assert!(
        INFERENCE_DEADLINE_MS as f64 > synthesis_p50_ms,
        "INFERENCE_DEADLINE_MS ({INFERENCE_DEADLINE_MS} ms) must exceed the synthesis \
         network p50 ({synthesis_p50_ms} ms) to avoid spurious timeouts"
    );
}

// ── 7. Watchdog returns promptly after the deadline fires ─────────────────────

#[test]
fn watchdog_unblocks_caller_within_2x_deadline() {
    // The caller must regain control within ≤ 2× the configured deadline.
    // Allowing 2× gives one full deadline of scheduling slack while still
    // guaranteeing the transport loop is not held for the stalled model's
    // full execution time.
    let wdog = test_watchdog();
    let start = Instant::now();

    let _ = wdog.run(ModelId::NoiseSuppressor, || {
        std::thread::sleep(Duration::from_secs(10)); // simulate a severe stall
    });

    let elapsed = start.elapsed();
    let upper_bound = Duration::from_millis(TEST_DEADLINE_MS * 2);
    assert!(
        elapsed < upper_bound,
        "watchdog must unblock the caller within {upper_bound:?}; elapsed: {elapsed:?}"
    );
}

// ── 8. ModelWatchdog::new() uses the production constant ─────────────────────

#[test]
fn new_watchdog_deadline_matches_production_constant() {
    let wdog = ModelWatchdog::new();
    assert_eq!(
        wdog.deadline(),
        Duration::from_millis(INFERENCE_DEADLINE_MS),
        "ModelWatchdog::new() must configure the deadline from INFERENCE_DEADLINE_MS"
    );
}

#[test]
fn default_watchdog_deadline_matches_new() {
    let a = ModelWatchdog::new();
    let b = ModelWatchdog::default();
    assert_eq!(
        a.deadline(),
        b.deadline(),
        "ModelWatchdog::default() must equal ModelWatchdog::new()"
    );
}

#[test]
fn with_deadline_overrides_production_constant() {
    let custom = Duration::from_millis(77);
    let wdog = ModelWatchdog::with_deadline(custom);
    assert_eq!(
        wdog.deadline(),
        custom,
        "with_deadline must set the specified duration instead of the production constant"
    );
}

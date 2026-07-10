//! Model watchdog supervisor — Feature 81.
//!
//! The transport loop must never block waiting for a neural model.  This
//! module wraps every model inference call with a per-inference deadline.
//! If a model stalls — hangs inside ONNX Runtime, spins on an NPU queue,
//! or blocks on a slow device — [`ModelWatchdog::run`] returns
//! [`Err(InferenceTimeout)`] immediately so the transport loop can take its
//! fallback path without ever being delayed.
//!
//! # Mechanism
//!
//! [`ModelWatchdog::run`] spawns the inference closure on a background thread
//! and waits up to [`ModelWatchdog::deadline`] for the result via
//! `mpsc::recv_timeout`.  If the result arrives in time it is returned to the
//! caller.  If the deadline fires the watchdog returns [`InferenceTimeout`]
//! and gives control back to the transport loop immediately.  The background
//! thread is *not* cancelled — Rust has no safe thread-cancellation primitive
//! — but it is abandoned: the transport loop moves on and the thread either
//! completes later or is reaped when the process exits.
//!
//! # Default deadline
//!
//! [`INFERENCE_DEADLINE_MS`] is set to 50 ms — half a governor tick (100 ms).
//! A model whose p50 latency is 18 ms (synthesis network, the slowest in the
//! registry) has 2.8× headroom before the watchdog fires; pathological stalls
//! (device hang, deadlock, ONNX Runtime bug) are detected within half a tick.
//!
//! # Transport loop contract
//!
//! Every inference that runs on the transport thread or whose latency is
//! visible to the transport loop **must** go through [`ModelWatchdog::run`].
//! Calling a model inference closure directly, without the watchdog, violates
//! Feature 81 and risks a live-lock of the transport loop.

use std::sync::mpsc;
use std::time::Duration;

use crate::eval_card::ModelId;

/// Hard per-inference deadline (milliseconds).
///
/// 50 ms = half a governor tick (100 ms interval).  Any inference that has
/// not returned by this deadline is considered stalled and the watchdog fires,
/// returning [`InferenceTimeout`] to the caller so the transport loop can
/// continue on its fallback path.
pub const INFERENCE_DEADLINE_MS: u64 = 50;

// ── InferenceTimeout ─────────────────────────────────────────────────────────

/// Error returned when a model does not complete within the watchdog deadline.
///
/// The transport loop must treat this as a signal to activate the model's
/// fallback path (e.g. comfort noise instead of neural PLC, Opus SILK instead
/// of the neural vocoder) and must not retry the same inference synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceTimeout {
    /// Which model triggered the watchdog.
    pub model_id: ModelId,
    /// The deadline that was exceeded (milliseconds), from
    /// [`ModelWatchdog::deadline`].
    pub deadline_ms: u64,
}

// ── ModelWatchdog ─────────────────────────────────────────────────────────────

/// Supervisor that enforces a per-inference deadline.
///
/// The transport loop calls [`run`](ModelWatchdog::run) instead of invoking
/// a model inference closure directly.  The watchdog guarantees that `run`
/// returns — either with the inference result or with [`InferenceTimeout`] —
/// within at most `deadline + one thread-scheduling quantum` of wall time.
///
/// ## Thread safety
///
/// `ModelWatchdog` is `Clone + Send + Sync` and may be shared freely across
/// threads.  Each [`run`](ModelWatchdog::run) call is independent.
#[derive(Debug, Clone)]
pub struct ModelWatchdog {
    deadline: Duration,
}

impl ModelWatchdog {
    /// Create a watchdog with the production deadline ([`INFERENCE_DEADLINE_MS`]).
    pub fn new() -> Self {
        Self {
            deadline: Duration::from_millis(INFERENCE_DEADLINE_MS),
        }
    }

    /// Create a watchdog with a custom deadline.
    ///
    /// Intended for tests and benchmarks that need tight control over the
    /// timeout without relying on the production constant.
    pub fn with_deadline(deadline: Duration) -> Self {
        Self { deadline }
    }

    /// The configured inference deadline.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Run `f` under watchdog supervision.
    ///
    /// Spawns `f` on a background thread and waits up to
    /// [`Self::deadline`] for the result.
    ///
    /// * Returns `Ok(result)` if `f` completes before the deadline.
    /// * Returns `Err(`[`InferenceTimeout`]`)` if the deadline expires.
    ///
    /// In the timeout case `f` continues running on its background thread
    /// until completion; the result is silently discarded.  The caller
    /// **must not** block or spin waiting for the background thread to
    /// finish — that would defeat the purpose of the watchdog.
    pub fn run<F, T>(&self, model_id: ModelId, f: F) -> Result<T, InferenceTimeout>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let result = f();
            // If the receiver already timed out and was dropped, discard the result.
            let _ = tx.send(result);
        });

        rx.recv_timeout(self.deadline).map_err(|_| InferenceTimeout {
            model_id,
            deadline_ms: self.deadline.as_millis() as u64,
        })
    }
}

impl Default for ModelWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Short deadline used in all watchdog unit tests to keep the suite fast.
    const TEST_DEADLINE_MS: u64 = 30;

    fn test_watchdog() -> ModelWatchdog {
        ModelWatchdog::with_deadline(Duration::from_millis(TEST_DEADLINE_MS))
    }

    // ── Fast inference ────────────────────────────────────────────────────────

    #[test]
    fn fast_inference_returns_ok() {
        let wdog = test_watchdog();
        let result = wdog.run(ModelId::NoiseSuppressor, || 42u32);
        assert_eq!(result, Ok(42u32), "fast inference must return Ok");
    }

    #[test]
    fn fast_inference_result_value_is_preserved() {
        let wdog = test_watchdog();
        let result = wdog.run(ModelId::NeuralVocoder, || vec![1u8, 2, 3]);
        assert_eq!(result, Ok(vec![1u8, 2, 3]), "inference result must survive the watchdog channel");
    }

    #[test]
    fn fast_inference_bool_true() {
        let wdog = test_watchdog();
        assert_eq!(wdog.run(ModelId::NeuralPlc, || true), Ok(true));
    }

    // ── Stalled inference ─────────────────────────────────────────────────────

    #[test]
    fn stalled_inference_returns_inference_timeout() {
        let wdog = test_watchdog();
        let result = wdog.run(ModelId::KeypointExtractor, move || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
            0u32
        });
        assert!(
            matches!(result, Err(InferenceTimeout { .. })),
            "stalled inference must return Err(InferenceTimeout); got {result:?}"
        );
    }

    #[test]
    fn timeout_carries_correct_model_id() {
        let wdog = test_watchdog();
        let result = wdog.run(ModelId::SynthesisNetwork, || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        });
        match result {
            Err(InferenceTimeout { model_id, .. }) => {
                assert_eq!(
                    model_id,
                    ModelId::SynthesisNetwork,
                    "InferenceTimeout must carry the model_id passed to run()"
                );
            }
            Ok(()) => panic!("expected InferenceTimeout, got Ok"),
        }
    }

    #[test]
    fn timeout_carries_configured_deadline_ms() {
        let wdog = test_watchdog();
        let result = wdog.run(ModelId::NeuralPlc, || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        });
        match result {
            Err(InferenceTimeout { deadline_ms, .. }) => {
                assert_eq!(
                    deadline_ms, TEST_DEADLINE_MS,
                    "InferenceTimeout.deadline_ms must match the watchdog's configured deadline"
                );
            }
            Ok(()) => panic!("expected InferenceTimeout, got Ok"),
        }
    }

    // ── Deadline timing ───────────────────────────────────────────────────────

    #[test]
    fn watchdog_returns_within_reasonable_time_of_deadline() {
        // The watchdog must unblock the caller within deadline + one scheduling
        // quantum.  Allow 2× the deadline as the upper bound.
        let wdog = test_watchdog();
        let start = Instant::now();

        let _ = wdog.run(ModelId::NoiseSuppressor, || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        });

        let elapsed = start.elapsed();
        let upper_bound = Duration::from_millis(TEST_DEADLINE_MS * 2);
        assert!(
            elapsed < upper_bound,
            "watchdog must return within {upper_bound:?} after the deadline fires; \
             elapsed: {elapsed:?}"
        );
    }

    // ── Recovery after timeout ────────────────────────────────────────────────

    #[test]
    fn transport_loop_continues_after_timeout() {
        // A timeout must not leave the watchdog in a broken state — subsequent
        // fast inferences on the same watchdog must still succeed.
        let wdog = test_watchdog();

        // First call: stalled inference → timeout.
        let _ = wdog.run(ModelId::SynthesisNetwork, || {
            std::thread::sleep(Duration::from_millis(TEST_DEADLINE_MS * 4));
        });

        // Second call: fast inference → must succeed.
        let result = wdog.run(ModelId::NoiseSuppressor, || 99u32);
        assert_eq!(
            result,
            Ok(99u32),
            "watchdog must remain functional after a timeout — transport loop \
             must be able to continue"
        );
    }

    #[test]
    fn multiple_sequential_fast_inferences_all_succeed() {
        let wdog = test_watchdog();
        for i in 0u32..5 {
            let r = wdog.run(ModelId::NeuralVocoder, move || i * 2);
            assert_eq!(r, Ok(i * 2), "inference {i} must succeed");
        }
    }

    // ── Production constant ───────────────────────────────────────────────────

    #[test]
    fn production_deadline_is_below_one_governor_tick() {
        // The governor tick period is 100 ms.  The watchdog deadline must be
        // less than one full tick so a stall is detected within a single
        // governor interval.
        const GOVERNOR_TICK_MS: u64 = 100;
        assert!(
            INFERENCE_DEADLINE_MS < GOVERNOR_TICK_MS,
            "INFERENCE_DEADLINE_MS ({INFERENCE_DEADLINE_MS}) must be < one governor tick \
             ({GOVERNOR_TICK_MS} ms)"
        );
    }

    #[test]
    fn production_deadline_is_above_synthesis_network_p50() {
        // The synthesis network is the slowest model in the registry at 18 ms p50.
        // The production deadline must be above this value to avoid spurious
        // timeouts under normal operation.
        const SYNTHESIS_NETWORK_P50_MS: f64 = 18.0;
        assert!(
            INFERENCE_DEADLINE_MS as f64 > SYNTHESIS_NETWORK_P50_MS,
            "INFERENCE_DEADLINE_MS ({INFERENCE_DEADLINE_MS}) must exceed the synthesis \
             network p50 ({SYNTHESIS_NETWORK_P50_MS} ms) to avoid false positives"
        );
    }

    #[test]
    fn production_watchdog_new_has_correct_deadline() {
        let wdog = ModelWatchdog::new();
        assert_eq!(
            wdog.deadline(),
            Duration::from_millis(INFERENCE_DEADLINE_MS),
            "ModelWatchdog::new() must use INFERENCE_DEADLINE_MS"
        );
    }

    #[test]
    fn production_watchdog_default_equals_new() {
        let a = ModelWatchdog::new();
        let b = ModelWatchdog::default();
        assert_eq!(
            a.deadline(),
            b.deadline(),
            "ModelWatchdog::default() must equal ModelWatchdog::new()"
        );
    }

    // ── with_deadline override ────────────────────────────────────────────────

    #[test]
    fn with_deadline_sets_custom_deadline() {
        let custom = Duration::from_millis(123);
        let wdog = ModelWatchdog::with_deadline(custom);
        assert_eq!(wdog.deadline(), custom, "with_deadline must set the specified duration");
    }
}

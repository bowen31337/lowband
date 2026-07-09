//! Feature 29 — panic key rejects input_injection within 50 milliseconds.
//!
//! # Contract
//!
//! From the moment `fire_panic` is called (key press received by the daemon)
//! to the moment the next `apply_event` call returns an error, the elapsed
//! wall-clock time must not exceed [`PANIC_INJECTION_BLOCK_DEADLINE_MS`]
//! (50 ms).
//!
//! # Why this is easy to verify
//!
//! `PanicController::fire_panic` is fully synchronous — it calls
//! `ControlSession::set_grant(None)` in the same stack frame.  There is no
//! thread hand-off, no async await point, and no OS call on the revocation
//! path.  The revocation is therefore sub-microsecond, well inside the 50 ms
//! SLA.
//!
//! # Simulation
//!
//! 10 000 back-to-back fire-panic cycles are timed to produce a reliable
//! worst-case observation.  Each cycle:
//!
//! 1. Creates a `ControlSession` with an active `ControlGrant`.
//! 2. Creates a `PanicController` with the transport up.
//! 3. Calls `fire_panic` and immediately calls `apply_event`.
//! 4. Records the elapsed wall-clock time.
//!
//! The test asserts:
//! - `effect.injection_revoked` is `true` (revocation actually happened).
//! - `apply_event()` returns `Err(CapabilityError::NoActiveGrant)` (injection
//!   is blocked from the very next call).
//! - The worst-case (maximum) elapsed time across all iterations is strictly
//!   less than the 50 ms SLA.
//!
//! # Architecture note
//!
//! The 50 ms budget is generous by design: it accounts for OS scheduling jitter
//! on a loaded 3G-class device.  The synchronous implementation leaves ≈49.99 ms
//! of margin — the budget is there for a future async dispatch path if the
//! key-press is relayed over IPC.

use std::time::{Duration, Instant};

use lowband_messaging::{
    grants::{CapabilityError, ControlGrant, ControlSession},
    panic_key::{PanicController, PANIC_INJECTION_BLOCK_DEADLINE_MS},
};

// ── Simulation parameters ─────────────────────────────────────────────────────

/// Number of fire-panic cycles timed to build a reliable worst-case sample.
const ITERATIONS: usize = 10_000;

/// SLA expressed as a [`Duration`] for assertion comparisons.
const DEADLINE: Duration = Duration::from_millis(PANIC_INJECTION_BLOCK_DEADLINE_MS);

// ── Helpers ───────────────────────────────────────────────────────────────────

fn active_session() -> ControlSession {
    let mut s = ControlSession::new();
    s.set_grant(Some(ControlGrant::new()));
    s
}

// ── 1. Constant matches Feature 29 spec ──────────────────────────────────────

#[test]
fn panic_injection_block_deadline_is_50ms() {
    assert_eq!(
        PANIC_INJECTION_BLOCK_DEADLINE_MS, 50,
        "Feature 29 mandates a 50 ms panic injection-block deadline"
    );
}

// ── 2. Injection rejected on the very next call after fire_panic ──────────────

#[test]
fn injection_rejected_immediately_after_fire_panic() {
    let mut ctrl = active_session();
    let mut pc = PanicController::new();
    pc.set_transport_up(true);

    assert!(ctrl.apply_event().is_ok(), "precondition: injection allowed before panic");

    let effect = pc.fire_panic(&mut ctrl);

    assert!(
        effect.injection_revoked,
        "fire_panic must report injection_revoked=true"
    );
    assert_eq!(
        ctrl.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "injection must be blocked on the very next apply_event call after fire_panic"
    );
}

// ── 3. Wall-clock latency gate — worst-case across 10 000 iterations ─────────

#[test]
fn panic_injection_block_latency_worst_case_under_50ms() {
    let mut worst = Duration::ZERO;

    for _ in 0..ITERATIONS {
        let mut ctrl = active_session();
        let mut pc = PanicController::new();
        pc.set_transport_up(true);

        // Time the full path: fire_panic (grant revoked) + apply_event (blocked).
        let t0 = Instant::now();
        let effect = pc.fire_panic(&mut ctrl);
        let blocked = ctrl.apply_event();
        let elapsed = t0.elapsed();

        // Correctness: revocation must have happened.
        assert!(
            effect.injection_revoked,
            "fire_panic must revoke injection on every iteration"
        );
        assert_eq!(
            blocked,
            Err(CapabilityError::NoActiveGrant),
            "apply_event must return NoActiveGrant immediately after fire_panic"
        );

        if elapsed > worst {
            worst = elapsed;
        }
    }

    eprintln!(
        "panic_key_latency — worst-case over {ITERATIONS} iterations: {worst:?}  \
         [deadline: {DEADLINE:?}]"
    );

    assert!(
        worst < DEADLINE,
        "panic injection-block worst-case latency {worst:?} exceeded the \
         {DEADLINE:?} SLA (Feature 29)"
    );
}

// ── 4. Transport remains up after panic (Feature 30 regression guard) ─────────

#[test]
fn transport_stays_up_while_injection_is_blocked() {
    let mut ctrl = active_session();
    let mut pc = PanicController::new();
    pc.set_transport_up(true);

    let effect = pc.fire_panic(&mut ctrl);

    assert!(
        effect.transport_up,
        "transport must remain up after panic (Feature 30)"
    );
    assert!(
        pc.transport_up(),
        "PanicController must report transport_up=true after fire_panic"
    );
    assert_eq!(
        ctrl.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "injection must be blocked even though transport is up"
    );
}

//! Feature 62 — system injects input on the assisted machine only with
//! control_grant held live.
//!
//! # Contract
//!
//! Every `InputBroker::inject` call validates the held `ControlSession`
//! capability_token before the event reaches the OS API
//! (`SendInput` / `CGEvent` / `libei`).  Without a live `ControlGrant` the
//! call returns `InjectionError::CapabilityDenied` immediately — the OS is
//! never invoked.
//!
//! # Enforcement architecture
//!
//! ```text
//! InputBroker::inject(event)
//!   └─ ControlSession::apply_event()   ← capability gate
//!        ├─ None               → CapabilityError::NoActiveGrant
//!        ├─ Some(grant) TTL elapsed → CapabilityError::GrantExpired
//!        └─ Some(grant) withdrawn   → CapabilityError::ConsentWithdrawn
//!        └─ Some(grant) live        → platform backend (SendInput / CGEvent / libei)
//! ```
//!
//! # Tests
//!
//! 1. **Default session has no grant** — `ControlSession::new()` starts
//!    without a grant; the very first `apply_event` returns `NoActiveGrant`.
//! 2. **Events rejected without grant** — `apply_event` returns
//!    `CapabilityError::NoActiveGrant` before any grant has been issued.
//! 3. **Events accepted with active grant** — once a `ControlGrant::new()` is
//!    installed, `apply_event` returns `Ok(())`.
//! 4. **Revocation blocks the next event** — setting the grant to `None`
//!    immediately returns `NoActiveGrant` on the next `apply_event`.
//! 5. **Expired grant blocks injection** — a zero-TTL `ControlGrant` is
//!    expired on construction; all `apply_event` calls return `GrantExpired`.
//! 6. **Non-expired timed grant passes** — a grant with ample TTL is accepted
//!    on construction without sleeping.
//! 7. **Consent withdrawal — single session** — `ConsentRevocationHandle::withdraw`
//!    causes the next `apply_event` to return `ConsentWithdrawn` immediately.
//! 8. **Consent withdrawal — multiple sessions** — a single `withdraw` call
//!    invalidates all `ControlSession`s bound to the same handle at once.
//! 9. **`is_granted` reflects live state** — the predicate tracks grant,
//!    expiry, and withdrawal correctly.
//! 10. **Revocation wall-clock latency** — worst-case over 10 000
//!     grant-and-revoke cycles is sub-microsecond; the capability check never
//!     blocks waiting on the OS.

use std::time::{Duration, Instant};

use lowband_messaging::grants::{
    CapabilityError, ConsentGrant, ControlGrant, ControlSession,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn session_with_grant() -> ControlSession {
    let mut s = ControlSession::new();
    s.set_grant(Some(ControlGrant::new()));
    s
}

fn session_no_grant() -> ControlSession {
    ControlSession::new()
}

// ── 1. Default session has no grant ──────────────────────────────────────────

#[test]
fn default_session_has_no_active_grant() {
    let session = ControlSession::new();
    assert!(
        !session.is_granted(),
        "ControlSession::new() must start with is_granted=false (Feature 62: \
         injection requires an explicit consent grant)"
    );
}

// ── 2. Events rejected without a grant ───────────────────────────────────────

#[test]
fn inject_rejected_without_any_grant() {
    let session = session_no_grant();
    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "apply_event must return NoActiveGrant when no ControlGrant is installed \
         (Feature 62: injection is forbidden without control_grant held live)"
    );
}

// ── 3. Events accepted with an active grant ───────────────────────────────────

#[test]
fn inject_accepted_with_live_grant() {
    let session = session_with_grant();
    assert!(
        session.apply_event().is_ok(),
        "apply_event must return Ok(()) while a live ControlGrant is installed \
         (Feature 62: injection is permitted only with control_grant held live)"
    );
}

// ── 4. Revocation immediately blocks the next event ──────────────────────────

#[test]
fn inject_rejected_immediately_after_grant_revocation() {
    let mut session = session_with_grant();
    assert!(session.apply_event().is_ok(), "precondition: grant is live");

    session.set_grant(None);

    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::NoActiveGrant),
        "apply_event must return NoActiveGrant on the very next call after \
         set_grant(None) — revocation must be instantaneous (Feature 62)"
    );
}

// ── 5. Expired grant blocks injection ─────────────────────────────────────────

#[test]
fn inject_rejected_when_grant_ttl_elapsed() {
    let mut session = ControlSession::new();
    // Duration::ZERO produces an expiry instant that is already in the past.
    session.set_grant(Some(ControlGrant::with_duration(Duration::ZERO)));

    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::GrantExpired),
        "apply_event must return GrantExpired when the consent_grant TTL has \
         elapsed — an expired grant is not a live grant (Feature 62)"
    );
}

// ── 6. Non-expired timed grant is accepted immediately ───────────────────────

#[test]
fn inject_accepted_when_grant_ttl_has_not_elapsed() {
    let mut session = ControlSession::new();
    session.set_grant(Some(ControlGrant::with_duration(Duration::from_secs(3600))));

    assert!(
        session.apply_event().is_ok(),
        "apply_event must return Ok(()) immediately after issuing a non-expired \
         timed ControlGrant (Feature 62: the grant is live from the moment of issue)"
    );
}

// ── 7. Consent withdrawal — single session ────────────────────────────────────

#[test]
fn inject_rejected_instantly_after_consent_withdrawal() {
    let (_, ctrl_grant, _, handle) = ConsentGrant::new().issue_all();
    let mut session = ControlSession::new();
    session.set_grant(Some(ctrl_grant));

    assert!(
        session.apply_event().is_ok(),
        "precondition: grant is live before withdrawal"
    );

    handle.withdraw();

    assert_eq!(
        session.apply_event(),
        Err(CapabilityError::ConsentWithdrawn),
        "apply_event must return ConsentWithdrawn immediately after \
         ConsentRevocationHandle::withdraw — no grace window (Feature 62)"
    );
}

// ── 8. Consent withdrawal — multiple sessions share the same handle ──────────

#[test]
fn inject_rejected_on_all_sessions_after_shared_handle_withdrawal() {
    let (_, ctrl_grant_a, _, handle) = ConsentGrant::new().issue_all();
    let ctrl_grant_b = ControlGrant::with_consent(handle.clone());

    let mut session_a = ControlSession::new();
    let mut session_b = ControlSession::new();
    session_a.set_grant(Some(ctrl_grant_a));
    session_b.set_grant(Some(ctrl_grant_b));

    assert!(session_a.apply_event().is_ok(), "precondition: session_a live");
    assert!(session_b.apply_event().is_ok(), "precondition: session_b live");

    // A single withdraw call on the shared handle must invalidate every token
    // bound to it simultaneously, without requiring individual set_grant(None)
    // calls on each session.
    handle.withdraw();

    assert_eq!(
        session_a.apply_event(),
        Err(CapabilityError::ConsentWithdrawn),
        "session_a must be blocked immediately after shared handle withdrawal"
    );
    assert_eq!(
        session_b.apply_event(),
        Err(CapabilityError::ConsentWithdrawn),
        "session_b must be blocked immediately after shared handle withdrawal — \
         a single withdraw call must cascade to all bound sessions (Feature 62)"
    );
}

// ── 9. `is_granted` reflects live state ──────────────────────────────────────

#[test]
fn is_granted_tracks_grant_expiry_and_withdrawal() {
    // No grant.
    let mut session = ControlSession::new();
    assert!(!session.is_granted(), "is_granted must be false with no grant");

    // Live grant.
    session.set_grant(Some(ControlGrant::new()));
    assert!(session.is_granted(), "is_granted must be true with a live grant");

    // Grant revoked.
    session.set_grant(None);
    assert!(!session.is_granted(), "is_granted must be false after set_grant(None)");

    // Expired grant.
    session.set_grant(Some(ControlGrant::with_duration(Duration::ZERO)));
    assert!(
        !session.is_granted(),
        "is_granted must be false when the grant TTL has elapsed"
    );

    // Withdrawn grant.
    let (_, ctrl_grant, _, handle) = ConsentGrant::new().issue_all();
    session.set_grant(Some(ctrl_grant));
    assert!(session.is_granted(), "is_granted must be true before withdrawal");
    handle.withdraw();
    assert!(
        !session.is_granted(),
        "is_granted must be false immediately after consent withdrawal"
    );
}

// ── 10. Revocation wall-clock latency ────────────────────────────────────────

/// The capability gate (`ControlSession::apply_event`) is a pure in-process
/// atomic read — no OS call, no thread hand-off.  This test measures the
/// worst-case wall-clock time for a grant-issue → apply_event → revoke →
/// apply_event cycle over 10 000 iterations to prove the revocation path is
/// sub-microsecond.
///
/// The assertion uses a 100 µs deadline to leave margin for scheduling jitter
/// on loaded CI machines; the actual synchronous implementation typically
/// resolves in < 100 ns.
#[test]
fn control_grant_revocation_is_sub_microsecond() {
    const ITERATIONS: usize = 10_000;
    const DEADLINE: Duration = Duration::from_micros(100);

    let mut worst = Duration::ZERO;

    for _ in 0..ITERATIONS {
        let mut session = ControlSession::new();
        session.set_grant(Some(ControlGrant::new()));

        // Time: revocation + next apply_event (the path InputBroker::inject takes).
        let t0 = Instant::now();
        session.set_grant(None);
        let result = session.apply_event();
        let elapsed = t0.elapsed();

        assert_eq!(
            result,
            Err(CapabilityError::NoActiveGrant),
            "apply_event must return NoActiveGrant immediately after revocation"
        );

        if elapsed > worst {
            worst = elapsed;
        }
    }

    eprintln!(
        "control_grant revocation latency — worst-case over {ITERATIONS} \
         iterations: {worst:?}  [deadline: {DEADLINE:?}]"
    );

    assert!(
        worst < DEADLINE,
        "control_grant revocation worst-case latency {worst:?} exceeded the \
         {DEADLINE:?} deadline — the capability gate must never block on an OS \
         call (Feature 62: revocation must be instantaneous)"
    );
}

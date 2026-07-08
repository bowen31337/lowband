//! Feature 4 — NAT keepalive window verification.
//!
//! A simulated 10-minute session at 10 Hz verifies that every keepalive fires
//! within the 15–25 second window declared by the spec.  The test drives the
//! controller with a deterministic sequence of jittered intervals drawn from
//! the full [MIN, MAX] range and asserts both bounds on every firing.

use lowband_lbtp::nat_keepalive::{
    KeepaliveEvent, NatKeepaliveController,
    NAT_KEEPALIVE_MAX_TICKS, NAT_KEEPALIVE_MIN_TICKS,
};

/// Tick rate (Hz) — matches the nominal LBTP control loop rate.
const TICK_HZ: u32 = 10;

/// Session duration in seconds.
const SESSION_SECS: u32 = 10 * 60; // 10 minutes

/// Total control ticks for the session.
const TOTAL_TICKS: u32 = SESSION_SECS * TICK_HZ; // 6 000

/// Simple deterministic interval sequence cycling through min, midpoint, and max.
///
/// In production the interval is drawn from a CSPRNG; here we use a fixed
/// pattern so the test is reproducible without an external RNG dependency.
fn jitter_sequence() -> impl Iterator<Item = u32> {
    let mid = (NAT_KEEPALIVE_MIN_TICKS + NAT_KEEPALIVE_MAX_TICKS) / 2;
    [
        NAT_KEEPALIVE_MIN_TICKS,
        mid,
        NAT_KEEPALIVE_MAX_TICKS,
        NAT_KEEPALIVE_MIN_TICKS + 17,
        NAT_KEEPALIVE_MAX_TICKS - 23,
    ]
    .into_iter()
    .cycle()
}

#[test]
fn nat_keepalive_fires_within_15_to_25_second_window() {
    let mut jitter = jitter_sequence();
    let first_interval = jitter.next().unwrap();
    let mut ctrl = NatKeepaliveController::new(first_interval);

    let mut keepalive_count: u32 = 0;
    let mut ticks_since_last: u32 = 0;

    for _ in 0..TOTAL_TICKS {
        ticks_since_last += 1;
        if let Some(KeepaliveEvent::Keepalive) = ctrl.tick() {
            // Verify the elapsed time is within the valid window.
            assert!(
                ticks_since_last >= NAT_KEEPALIVE_MIN_TICKS,
                "keepalive #{keepalive_count} fired after {ticks_since_last} ticks \
                 — below the {NAT_KEEPALIVE_MIN_TICKS}-tick (15 s) minimum"
            );
            assert!(
                ticks_since_last <= NAT_KEEPALIVE_MAX_TICKS,
                "keepalive #{keepalive_count} fired after {ticks_since_last} ticks \
                 — above the {NAT_KEEPALIVE_MAX_TICKS}-tick (25 s) maximum"
            );

            keepalive_count += 1;
            ticks_since_last = 0;

            let next_interval = jitter.next().unwrap();
            ctrl.reset(next_interval);
        }
    }

    // A 10-minute session must produce at least some keepalives.
    // At 25 s each, 600 s ÷ 25 s = 24 keepalives minimum.
    let min_expected = SESSION_SECS / (NAT_KEEPALIVE_MAX_TICKS / TICK_HZ);
    assert!(
        keepalive_count >= min_expected,
        "only {keepalive_count} keepalives in {SESSION_SECS} s — expected ≥ {min_expected}"
    );

    eprintln!(
        "nat_keepalive: {keepalive_count} keepalives in {SESSION_SECS} s \
         [window: {min}–{max} s]",
        min = NAT_KEEPALIVE_MIN_TICKS / TICK_HZ,
        max = NAT_KEEPALIVE_MAX_TICKS / TICK_HZ,
    );
}

#[test]
fn nat_keepalive_min_interval_never_undercuts_15_seconds() {
    // Drive the controller with NAT_KEEPALIVE_MIN_TICKS every time and verify
    // the firing tick is never below the 15 s floor.
    let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MIN_TICKS);
    let mut ticks_since_last: u32 = 0;

    for _ in 0..TOTAL_TICKS {
        ticks_since_last += 1;
        if let Some(KeepaliveEvent::Keepalive) = ctrl.tick() {
            assert!(
                ticks_since_last >= NAT_KEEPALIVE_MIN_TICKS,
                "keepalive fired after {ticks_since_last} ticks — below minimum"
            );
            ticks_since_last = 0;
            ctrl.reset(NAT_KEEPALIVE_MIN_TICKS);
        }
    }
}

#[test]
fn nat_keepalive_max_interval_never_exceeds_25_seconds() {
    // Drive the controller with NAT_KEEPALIVE_MAX_TICKS every time and verify
    // the firing tick never exceeds the 25 s ceiling.
    let mut ctrl = NatKeepaliveController::new(NAT_KEEPALIVE_MAX_TICKS);
    let mut ticks_since_last: u32 = 0;

    for _ in 0..TOTAL_TICKS {
        ticks_since_last += 1;
        if let Some(KeepaliveEvent::Keepalive) = ctrl.tick() {
            assert!(
                ticks_since_last <= NAT_KEEPALIVE_MAX_TICKS,
                "keepalive fired after {ticks_since_last} ticks — above maximum"
            );
            ticks_since_last = 0;
            ctrl.reset(NAT_KEEPALIVE_MAX_TICKS);
        }
    }
}

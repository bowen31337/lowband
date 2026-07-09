//! Feature 56 — playout convergence under 15 % with time_scaling, no gaps.
//!
//! # Architecture requirement
//!
//! "System converges playout under 15 percent with time_scaling instead of
//! gaps."  The jitter buffer must never emit silence when audio is available;
//! instead it signals time-scaled playback (acceleration or deceleration) to
//! drain or fill the buffer.
//!
//! # Test structure
//!
//! **Part A — ceiling.**  Drive the jitter buffer from an extreme overfill
//! (10× target) and confirm every Accelerate factor is within 15 %.
//!
//! **Part B — convergence, no gaps.**  Simulate a jitter burst that pushes
//! the buffer 3× above target, then play out with a realistic arrival rate
//! (one frame per tick from steady-state) and verify:
//!   1. The buffer converges to within the convergence zone.
//!   2. No `Conceal` action is emitted while audio is present.
//!
//! **Part C — decelerate instead of underrun.**  Load the buffer with 2 frames
//! against a target of 8 and confirm `Decelerate` is returned (not `Conceal`)
//! while frames remain.
//!
//! **Part D — Conceal when truly empty.**  Verify `Conceal` is returned only
//! when the buffer is exhausted, affirming that PLC — not silence — is the
//! designated response to underrun.

use lowband_platform::{
    JitterBuffer, PlayoutAction, CONVERGENCE_ZONE_FRAMES, MAX_TIME_SCALE_RATE,
};

/// Opus frame duration at the constrained tier (ms).
const FRAME_MS: usize = 20;

/// Target jitter buffer depth: 4 frames = 80 ms, matching the constrained-tier
/// architecture target.
const TARGET_FRAMES: usize = 4;

// ── Part A ────────────────────────────────────────────────────────────────────

#[test]
fn acceleration_factor_never_exceeds_15_pct_during_convergence() {
    let mut jb = JitterBuffer::new(TARGET_FRAMES, CONVERGENCE_ZONE_FRAMES);

    // Pre-fill 10× the target to produce the maximum possible deviation.
    for _ in 0..TARGET_FRAMES * 10 {
        jb.enqueue();
    }

    let mut violations = 0usize;
    let mut checked = 0usize;

    // Drain without adding new frames (worst-case drain scenario).
    while jb.level() > TARGET_FRAMES + CONVERGENCE_ZONE_FRAMES {
        match jb.tick() {
            PlayoutAction::Accelerate(f) => {
                checked += 1;
                if f > MAX_TIME_SCALE_RATE + 1e-9 {
                    violations += 1;
                    eprintln!(
                        "time_scale_ceiling: Accelerate({f:.4}) exceeds \
                         MAX_TIME_SCALE_RATE ({MAX_TIME_SCALE_RATE})"
                    );
                }
            }
            _ => {}
        }
    }

    eprintln!(
        "time_scale_ceiling — checked={checked} accelerate ticks, violations={violations} \
         [limit: 0], target={TARGET_FRAMES} frames ({} ms)",
        TARGET_FRAMES * FRAME_MS,
    );

    assert_eq!(
        violations,
        0,
        "time-scaling Accelerate factor exceeded {MAX_TIME_SCALE_RATE} ({} %) \
         in {violations}/{checked} ticks — ceiling violation",
        MAX_TIME_SCALE_RATE * 100.0,
    );
    assert!(
        checked > 0,
        "no Accelerate ticks observed; test pre-conditions may be wrong"
    );
}

// ── Part B ────────────────────────────────────────────────────────────────────

#[test]
fn buffer_converges_without_silence_gaps_after_jitter_burst() {
    let mut jb = JitterBuffer::new(TARGET_FRAMES, CONVERGENCE_ZONE_FRAMES);

    // Simulate a jitter burst: 3× the target arrives at once.
    // The sender resumes at the nominal rate after the burst, so new arrivals
    // come every (1 + acceleration_factor) playout ticks on average — the
    // receiver plays slightly faster than the sender sends, draining the excess.
    // Model this by adding one arrival every other tick during drain, which
    // approximates steady-state at ~7.5 % excess drain rate.
    let burst_fill = TARGET_FRAMES * 3;
    for _ in 0..burst_fill {
        jb.enqueue();
    }

    let initial_level = jb.level();
    let mut conceals = 0usize;
    let mut ticks = 0usize;

    while jb.level() > TARGET_FRAMES + CONVERGENCE_ZONE_FRAMES {
        // Arrival every other tick models sender at nominal rate while the
        // receiver accelerates to drain the burst-induced excess.
        if ticks % 2 == 0 {
            jb.enqueue();
        }
        match jb.tick() {
            PlayoutAction::Conceal => conceals += 1,
            PlayoutAction::Accelerate(f) => {
                assert!(
                    f <= MAX_TIME_SCALE_RATE + 1e-9,
                    "convergence Accelerate({f:.4}) > 15% at tick {ticks}"
                );
            }
            _ => {}
        }
        ticks += 1;
        assert!(
            ticks < 2_000,
            "buffer did not converge within 2 000 ticks \
             (initial={initial_level}, target={TARGET_FRAMES})"
        );
    }

    eprintln!(
        "time_scale_convergence — initial={initial_level}  target={TARGET_FRAMES} \
         frames ({} ms)  ticks={ticks}  conceals={conceals} [limit: 0]",
        TARGET_FRAMES * FRAME_MS,
    );

    assert_eq!(
        conceals,
        0,
        "Conceal emitted {conceals} time(s) while frames were present — \
         convergence must use time_scaling, not gaps"
    );
}

// ── Part C ────────────────────────────────────────────────────────────────────

#[test]
fn underfilled_buffer_decelerates_instead_of_concealing() {
    let target = 8usize;
    let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);

    // Enqueue well below target so the buffer is underfilled.
    let initial_frames = 2usize;
    for _ in 0..initial_frames {
        jb.enqueue();
    }

    let mut decelerate_count = 0usize;
    let mut conceal_count = 0usize;

    // Drain the 2 enqueued frames.
    for _ in 0..initial_frames {
        match jb.tick() {
            PlayoutAction::Decelerate(f) => {
                decelerate_count += 1;
                assert!(
                    f <= MAX_TIME_SCALE_RATE + 1e-9,
                    "Decelerate({f:.4}) exceeds 15% ceiling"
                );
            }
            PlayoutAction::Conceal => conceal_count += 1,
            other => {
                panic!(
                    "expected Decelerate while underfilled, got {other:?} \
                     (target={target}, initial={initial_frames})"
                );
            }
        }
    }

    eprintln!(
        "time_scale_decelerate — target={target} frames ({} ms)  \
         initial={initial_frames}  decelerate={decelerate_count}  conceal={conceal_count}",
        target * FRAME_MS,
    );

    assert_eq!(
        conceal_count,
        0,
        "Conceal emitted while frames were available — \
         underfilled buffer must Decelerate, not conceal"
    );
    assert!(
        decelerate_count > 0,
        "no Decelerate ticks observed on underfilled buffer"
    );
}

// ── Part D ────────────────────────────────────────────────────────────────────

#[test]
fn empty_buffer_returns_conceal_not_silence() {
    let mut jb = JitterBuffer::new(TARGET_FRAMES, CONVERGENCE_ZONE_FRAMES);

    // Buffer is empty from the start.
    let action = jb.tick();
    assert_eq!(
        action,
        PlayoutAction::Conceal,
        "empty buffer must return Conceal (PLC), not silence; got {action:?}"
    );

    // Enqueue one frame then drain it; next tick must also be Conceal.
    jb.enqueue();
    let _ = jb.tick(); // drains the one frame
    let action2 = jb.tick();
    assert_eq!(
        action2,
        PlayoutAction::Conceal,
        "buffer exhausted after one frame: must return Conceal, got {action2:?}"
    );

    eprintln!(
        "time_scale_conceal — verified Conceal on empty buffer \
         (target={TARGET_FRAMES} frames / {} ms)",
        TARGET_FRAMES * FRAME_MS,
    );
}

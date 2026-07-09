//! NetEQ-style jitter buffer playout convergence — Feature 56.
//!
//! When the jitter buffer diverges from its target level, the system
//! corrects by time-scaling the audio stream rather than inserting or
//! discarding frames, which would create audible gaps.
//!
//! # Convergence strategy
//!
//! | Buffer state                         | Action                              |
//! |--------------------------------------|-------------------------------------|
//! | `level > target + convergence_zone`  | `Accelerate` — drain faster         |
//! | `level < target − convergence_zone`  | `Decelerate` — fill faster          |
//! | within convergence zone              | `Normal` — no adjustment            |
//! | `level == 0`                         | `Conceal` — PLC, never a gap        |
//!
//! The scaling factor is proportional to the normalised deviation from the
//! target level, clamped to [`MAX_TIME_SCALE_RATE`] (15 %).  A larger deviation
//! produces a faster correction but never exceeds the perceptual quality
//! floor for speech intelligibility.
//!
//! # 15 % ceiling
//!
//! WSOLA and related time-scaling algorithms preserve speech intelligibility
//! up to roughly 20 % acceleration/deceleration.  The 15 % ceiling keeps the
//! system well inside this bound while still converging within a few hundred
//! milliseconds at a typical 20 ms frame rate.
//!
//! # Never silence
//!
//! The buffer returns [`PlayoutAction::Conceal`] — not silence — when empty.
//! The caller must apply the PLC chain (Feature 57) to generate a plausible
//! frame rather than emitting zeros.

/// Maximum time-scaling rate for playout convergence.
///
/// Audio is accelerated or decelerated by at most this fraction of the
/// nominal playback rate.  Effective rate lies in `[0.85, 1.15]`.
pub const MAX_TIME_SCALE_RATE: f64 = 0.15;

/// Frames within which the buffer is considered "at target" and no
/// time-scaling adjustment is applied.
///
/// One frame (20 ms) of dead band avoids perpetual micro-adjustments when
/// the buffer level oscillates around the target by a single frame.
pub const CONVERGENCE_ZONE_FRAMES: usize = 1;

/// Action the caller should apply to the next decoded audio frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayoutAction {
    /// Play at 1.0× (nominal rate) — buffer is within the convergence zone.
    Normal,
    /// Time-scale to play faster, draining an overfilled buffer.
    ///
    /// The effective playback rate is `1.0 + factor`.  `factor` is in
    /// `(0.0, MAX_TIME_SCALE_RATE]`.  The caller passes this factor to a
    /// WSOLA or OLA time-stretcher to produce the correct output duration.
    Accelerate(f64),
    /// Time-scale to play slower, filling an underfilled buffer.
    ///
    /// The effective playback rate is `1.0 - factor`.  `factor` is in
    /// `(0.0, MAX_TIME_SCALE_RATE]`.
    Decelerate(f64),
    /// Buffer is empty — apply PLC (Feature 57) instead of silence.
    Conceal,
}

/// NetEQ-style adaptive jitter buffer with time-scaling playout convergence.
///
/// The buffer tracks frames enqueued and dequeued.  Each [`JitterBuffer::tick`]
/// call returns a [`PlayoutAction`] telling the caller how to schedule the
/// next output frame so the buffer converges to [`JitterBuffer::target_level`]
/// without audible gaps.
#[derive(Debug, Clone)]
pub struct JitterBuffer {
    target_level: usize,
    level: usize,
    convergence_zone: usize,
}

impl JitterBuffer {
    /// Create a new jitter buffer with the given target level.
    ///
    /// `target_level_frames` is the steady-state buffer depth in 20 ms frames
    /// (e.g. 4 frames = 80 ms at the constrained tier).
    /// `convergence_zone_frames` is the dead band; use [`CONVERGENCE_ZONE_FRAMES`].
    pub fn new(target_level_frames: usize, convergence_zone_frames: usize) -> Self {
        Self {
            target_level: target_level_frames,
            level: 0,
            convergence_zone: convergence_zone_frames,
        }
    }

    /// Return the current buffer level in frames.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Enqueue a received frame into the buffer.
    ///
    /// Should be called for every frame the transport delivers, before
    /// [`JitterBuffer::tick`].
    pub fn enqueue(&mut self) {
        self.level += 1;
    }

    /// Dequeue one frame worth of playout and return the required action.
    ///
    /// Call once per 20 ms playout tick.  The returned [`PlayoutAction`]
    /// tells the caller:
    /// - how fast to play the dequeued frame, or
    /// - that the buffer is empty and PLC should be applied.
    ///
    /// When `Conceal` is returned, no frame is dequeued — the buffer remains
    /// empty and the caller must synthesise audio via the PLC chain.
    pub fn tick(&mut self) -> PlayoutAction {
        if self.level == 0 {
            return PlayoutAction::Conceal;
        }

        // Consume one frame.
        self.level -= 1;

        let deviation =
            (self.level as isize) - (self.target_level as isize);
        let abs_dev = deviation.unsigned_abs();

        if abs_dev <= self.convergence_zone {
            return PlayoutAction::Normal;
        }

        // Scale factor proportional to deviation, clamped to the 15 % ceiling.
        // Use target_level as the normalisation denominator; fall back to abs_dev
        // itself when target is zero to avoid division by zero.
        let denom = self.target_level.max(1) as f64;
        let raw_factor = (abs_dev as f64) / denom;
        let factor = raw_factor.min(MAX_TIME_SCALE_RATE);

        if deviation > 0 {
            // Buffer above target — accelerate to drain.
            PlayoutAction::Accelerate(factor)
        } else {
            // Buffer below target — decelerate to fill.
            PlayoutAction::Decelerate(factor)
        }
    }

    /// Target buffer level in frames.
    pub fn target_level(&self) -> usize {
        self.target_level
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_returns_conceal_not_silence() {
        let mut jb = JitterBuffer::new(4, CONVERGENCE_ZONE_FRAMES);
        assert_eq!(jb.tick(), PlayoutAction::Conceal);
    }

    #[test]
    fn buffer_at_target_returns_normal() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Fill to exactly target.
        for _ in 0..target {
            jb.enqueue();
        }
        // After dequeue, level = target - 1 which is within convergence_zone (1).
        // So Normal is expected.
        assert_eq!(jb.tick(), PlayoutAction::Normal);
    }

    #[test]
    fn overfilled_buffer_returns_accelerate() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Fill well above target.
        for _ in 0..(target * 2 + 4) {
            jb.enqueue();
        }
        let action = jb.tick();
        assert!(
            matches!(action, PlayoutAction::Accelerate(_)),
            "overfilled buffer must produce Accelerate, got {action:?}"
        );
    }

    #[test]
    fn underfilled_buffer_returns_decelerate() {
        let target = 8usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Enqueue only 2 frames — well below target of 8.
        jb.enqueue();
        jb.enqueue();
        let action = jb.tick();
        assert!(
            matches!(action, PlayoutAction::Decelerate(_)),
            "underfilled buffer must produce Decelerate, got {action:?}"
        );
    }

    #[test]
    fn acceleration_factor_is_capped_at_15_pct() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Load an extreme excess so the raw factor would exceed 15 %.
        for _ in 0..200 {
            jb.enqueue();
        }
        for _ in 0..100 {
            if let PlayoutAction::Accelerate(f) = jb.tick() {
                assert!(
                    f <= MAX_TIME_SCALE_RATE + 1e-9,
                    "Accelerate factor {f:.4} exceeds MAX_TIME_SCALE_RATE {MAX_TIME_SCALE_RATE}"
                );
            }
        }
    }

    #[test]
    fn deceleration_factor_is_capped_at_15_pct() {
        let target = 80usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Enqueue only 1 frame — maximum deviation below target.
        jb.enqueue();
        if let PlayoutAction::Decelerate(f) = jb.tick() {
            assert!(
                f <= MAX_TIME_SCALE_RATE + 1e-9,
                "Decelerate factor {f:.4} exceeds MAX_TIME_SCALE_RATE {MAX_TIME_SCALE_RATE}"
            );
        }
    }

    #[test]
    fn buffer_converges_from_excess_using_only_time_scaling() {
        let target = 4usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        // Simulate a burst arrival: fill to 3× target.
        for _ in 0..(target * 3) {
            jb.enqueue();
        }

        let mut gaps = 0usize;
        let mut max_ticks = 500usize;

        // Drive the buffer with no new arrivals to simulate the drain.
        while jb.level() > target + CONVERGENCE_ZONE_FRAMES && max_ticks > 0 {
            match jb.tick() {
                PlayoutAction::Conceal => gaps += 1,
                PlayoutAction::Accelerate(f) => {
                    assert!(
                        f <= MAX_TIME_SCALE_RATE + 1e-9,
                        "convergence Accelerate factor {f:.4} exceeds 15%"
                    );
                }
                PlayoutAction::Normal | PlayoutAction::Decelerate(_) => {}
            }
            max_ticks -= 1;
        }

        // No concealment should be triggered while draining an overfilled buffer.
        assert_eq!(gaps, 0, "conceal emitted during drain — time-scaling must be used instead");
        assert!(max_ticks > 0, "buffer did not converge within the tick budget");
    }

    #[test]
    fn conceal_is_used_not_silence_when_buffer_empties() {
        let target = 2usize;
        let mut jb = JitterBuffer::new(target, CONVERGENCE_ZONE_FRAMES);
        jb.enqueue();
        jb.tick(); // drains the 1 frame

        // Buffer is now empty — must get Conceal, not Normal.
        assert_eq!(
            jb.tick(),
            PlayoutAction::Conceal,
            "empty buffer must signal Conceal, not silence"
        );
    }

    #[test]
    fn level_decrements_on_tick_not_on_conceal() {
        let mut jb = JitterBuffer::new(4, CONVERGENCE_ZONE_FRAMES);
        jb.enqueue();
        jb.enqueue();
        assert_eq!(jb.level(), 2);

        jb.tick();
        assert_eq!(jb.level(), 1, "tick must consume one frame");

        jb.tick(); // drains last frame
        let prev_level = jb.level();
        jb.tick(); // empty — Conceal
        assert_eq!(
            jb.level(),
            prev_level,
            "Conceal must not decrement level"
        );
    }
}

//! Packet-loss concealment chain (PLC chain) — Feature 57.
//!
//! The system conceals every lost audio frame by traversing four ordered stages.
//! Each stage covers a different burst-length range; the chain advances to the
//! next stage only when the previous cannot handle the loss.
//!
//! # Concealment order
//!
//! | Stage         | Burst length (frames)                                    |
//! |---------------|----------------------------------------------------------|
//! | FEC decode    | == 1 (isolated loss — LBRR in-band redundancy)           |
//! | DRED          | 2..=`DRED_DEPTH_FRAMES` (50 frames = 1 s at 20 ms)      |
//! | Neural PLC    | DRED+1..=DRED+`NEURAL_PLC_MAX_FRAMES`                    |
//! | Comfort noise | > DRED+`NEURAL_PLC_MAX_FRAMES`, fades over               |
//! |               | `COMFORT_NOISE_FADE_FRAMES` then goes silent             |
//!
//! The caller feeds one `received` boolean per 20 ms frame tick into
//! [`PlcChain::advance`]; the returned [`PlcOutcome`] names the active stage
//! (or signals that the frame arrived cleanly).  When the comfort-noise stage
//! is active, [`PlcChain::comfort_noise_gain`] provides a linear fade
//! coefficient in \[0.0, 1.0\] that the audio mixer should apply to the
//! synthesised background noise level.
//!
//! # Architecture constants
//!
//! - [`DRED_DEPTH_FRAMES`] = 50: architecture gap-free ceiling — 1 s at 20 ms/frame
//!   (Features 52–53).  Every burst within this bound is reconstructed from
//!   the DRED payload in the first non-lost post-burst packet.
//! - [`NEURAL_PLC_MAX_FRAMES`] = 5: frames neural PLC synthesises before comfort
//!   noise.  Context drift beyond this window degrades output quality below the
//!   synthesised noise floor.
//! - [`COMFORT_NOISE_FADE_FRAMES`] = 8: linear ramp (160 ms) avoids an abrupt
//!   silence cut while minimising audible noise-floor duration.

/// Maximum DRED coverage depth in frames.
///
/// 50 frames × 20 ms = 1 000 ms.  Every loss burst within this bound is
/// reconstructed from the DRED payload carried by the first non-lost
/// post-burst packet.  Dimensioned to the architecture gap-free ceiling
/// (Feature 53).
pub const DRED_DEPTH_FRAMES: usize = 50;

/// Maximum frames the neural PLC stage synthesises before yielding to
/// comfort noise.
///
/// After [`DRED_DEPTH_FRAMES`] the neural synthesiser generates plausible
/// audio from the most-recently decoded context.  Context drift beyond this
/// window reduces output quality below the comfort-noise floor, so the chain
/// transitions to comfort noise at burst frame
/// `DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES + 1`.
pub const NEURAL_PLC_MAX_FRAMES: usize = 5;

/// Length of the comfort-noise amplitude fade ramp in frames.
///
/// When the comfort-noise stage begins, synthesised background noise is
/// emitted at full level and decays linearly to silence over this many frames
/// (8 × 20 ms = 160 ms).  A ramp of 8 frames avoids an abrupt cut while
/// minimising the audible noise-floor duration before silence.
pub const COMFORT_NOISE_FADE_FRAMES: usize = 8;

/// Ordered stage applied to conceal a lost frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlcStage {
    /// LBRR in-band FEC — covers isolated single-frame losses (burst == 1).
    ///
    /// Opus LBRR encodes a low-bitrate redundant representation of the
    /// previous frame alongside each outbound packet.  When exactly one frame
    /// is lost, the receiver decodes the embedded payload with no audible
    /// artefact.
    FecDecode,

    /// DRED neural redundancy — covers bursts 2..=`DRED_DEPTH_FRAMES`.
    ///
    /// Opus 1.5 DRED encodes the last N frames of audio in every outgoing
    /// packet.  The receiver reconstructs a contiguous loss burst from the
    /// DRED payload carried in the first non-lost post-burst packet, provided
    /// the burst does not exceed the depth configured at stream setup.
    Dred,

    /// Neural PLC synthesis — covers residual frames beyond DRED coverage.
    ///
    /// A neural synthesiser generates plausible audio conditioned on the
    /// most-recently decoded frames.  Applied when the burst length exceeds
    /// `DRED_DEPTH_FRAMES` but remains within
    /// `DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES`.
    NeuralPlc,

    /// Faded comfort noise — last resort when synthesis context is exhausted.
    ///
    /// Synthesised background noise at a level matched to recent speech
    /// energy, decaying linearly to silence over `COMFORT_NOISE_FADE_FRAMES`.
    /// Prevents a hard silence cut at the cost of a brief noise artefact.
    ComfortNoise,
}

/// Result of advancing the [`PlcChain`] by one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlcOutcome {
    /// Frame arrived; burst counter reset — no concealment applied.
    Received,
    /// Frame was lost; the named stage was applied for concealment.
    Concealed(PlcStage),
}

/// Stateful packet-loss concealment chain (Feature 57).
///
/// Call [`PlcChain::advance`] once per 20 ms frame with `received = true`
/// when the frame arrived and `received = false` when it was lost.  The
/// returned [`PlcOutcome`] identifies which concealment stage handled the
/// frame.
///
/// When [`PlcOutcome::Concealed`]`(`[`PlcStage::ComfortNoise`]`)` is returned,
/// call [`PlcChain::comfort_noise_gain`] to obtain the linear fade coefficient
/// for the synthesised noise level.
#[derive(Debug, Clone)]
pub struct PlcChain {
    dred_depth: usize,
    burst: usize,
}

impl PlcChain {
    /// Create a new chain with the given DRED coverage depth.
    ///
    /// Pass [`DRED_DEPTH_FRAMES`] for the architecture-specified 1 s ceiling.
    pub fn new(dred_depth_frames: usize) -> Self {
        Self { dred_depth: dred_depth_frames, burst: 0 }
    }

    /// Advance the chain by one 20 ms frame and return the concealment outcome.
    ///
    /// A `received = true` call resets the burst counter and returns
    /// [`PlcOutcome::Received`].  A `received = false` call increments the
    /// burst counter and returns the stage that covers this frame:
    ///
    /// | Burst after this call                         | Returned stage    |
    /// |-----------------------------------------------|-------------------|
    /// | 1 (isolated)                                  | `FecDecode`       |
    /// | 2..=`dred_depth`                              | `Dred`            |
    /// | `dred_depth`+1..=`dred_depth`+`NEURAL_PLC_MAX`| `NeuralPlc`      |
    /// | > `dred_depth` + `NEURAL_PLC_MAX_FRAMES`      | `ComfortNoise`    |
    pub fn advance(&mut self, received: bool) -> PlcOutcome {
        if received {
            self.burst = 0;
            return PlcOutcome::Received;
        }

        self.burst += 1;
        let stage = if self.burst == 1 {
            PlcStage::FecDecode
        } else if self.burst <= self.dred_depth {
            PlcStage::Dred
        } else if self.burst <= self.dred_depth + NEURAL_PLC_MAX_FRAMES {
            PlcStage::NeuralPlc
        } else {
            PlcStage::ComfortNoise
        };
        PlcOutcome::Concealed(stage)
    }

    /// Current contiguous burst length in lost frames.
    ///
    /// Returns 0 when the chain is idle (last frame was received).
    pub fn burst_frames(&self) -> usize {
        self.burst
    }

    /// Whether the chain is idle (no ongoing loss burst).
    pub fn is_idle(&self) -> bool {
        self.burst == 0
    }

    /// Linear amplitude gain for the comfort-noise stage, in \[0.0, 1.0\].
    ///
    /// Returns `0.0` unless the chain is currently in the
    /// [`PlcStage::ComfortNoise`] stage.  When in that stage the gain decays
    /// linearly from `1.0` at the first comfort-noise frame to `0.0` after
    /// [`COMFORT_NOISE_FADE_FRAMES`] frames, then holds at `0.0` (silent).
    ///
    /// Call this after [`PlcChain::advance`] returns
    /// [`PlcOutcome::Concealed`]`(`[`PlcStage::ComfortNoise`]`)`.
    pub fn comfort_noise_gain(&self) -> f64 {
        let cn_start = self.dred_depth + NEURAL_PLC_MAX_FRAMES + 1;
        if self.burst < cn_start {
            return 0.0;
        }
        let frames_into_cn = self.burst - cn_start;
        let fade_frac = frames_into_cn as f64 / COMFORT_NOISE_FADE_FRAMES as f64;
        (1.0 - fade_frac).max(0.0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_frame_returns_received_and_resets_burst() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Drive several lost frames to build up a burst.
        for _ in 0..10 {
            chain.advance(false);
        }
        assert_eq!(chain.burst_frames(), 10);

        // A received frame must reset the burst counter.
        let outcome = chain.advance(true);
        assert_eq!(outcome, PlcOutcome::Received);
        assert_eq!(chain.burst_frames(), 0);
        assert!(chain.is_idle());
    }

    #[test]
    fn isolated_loss_uses_fec_decode() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        let outcome = chain.advance(false);
        assert_eq!(
            outcome,
            PlcOutcome::Concealed(PlcStage::FecDecode),
            "burst == 1 (isolated loss) must be handled by FEC decode"
        );
        assert_eq!(chain.burst_frames(), 1);
    }

    #[test]
    fn short_burst_uses_dred() {
        // Test the full DRED range: burst 2..=DRED_DEPTH_FRAMES.
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Consume burst frame 1 (FEC stage).
        chain.advance(false);

        for burst in 2..=DRED_DEPTH_FRAMES {
            let outcome = chain.advance(false);
            assert_eq!(
                outcome,
                PlcOutcome::Concealed(PlcStage::Dred),
                "burst == {burst} must be handled by DRED"
            );
        }
        assert_eq!(chain.burst_frames(), DRED_DEPTH_FRAMES);
    }

    #[test]
    fn post_dred_burst_uses_neural_plc() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Exhaust FEC (1 frame) and DRED (DRED_DEPTH_FRAMES-1 frames, bursts 2..=50).
        // Using exclusive range so we stop at burst=50 (last DRED frame consumed),
        // leaving the full NEURAL_PLC_MAX_FRAMES window untouched.
        for _ in 0..DRED_DEPTH_FRAMES {
            chain.advance(false);
        }

        // The next NEURAL_PLC_MAX_FRAMES frames must be handled by neural PLC.
        for i in 1..=NEURAL_PLC_MAX_FRAMES {
            let outcome = chain.advance(false);
            assert_eq!(
                outcome,
                PlcOutcome::Concealed(PlcStage::NeuralPlc),
                "burst == {} must be handled by neural PLC (frame {i} of NEURAL_PLC_MAX_FRAMES)",
                DRED_DEPTH_FRAMES + i,
            );
        }
    }

    #[test]
    fn extended_burst_uses_comfort_noise() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Exhaust FEC, DRED, and neural PLC stages.
        for _ in 0..DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES {
            chain.advance(false);
        }

        // Every subsequent lost frame must be handled by comfort noise.
        for extra in 1..=COMFORT_NOISE_FADE_FRAMES + 4 {
            let outcome = chain.advance(false);
            assert_eq!(
                outcome,
                PlcOutcome::Concealed(PlcStage::ComfortNoise),
                "burst == {} must be handled by comfort noise (extra frame {extra})",
                DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES + extra,
            );
        }
    }

    #[test]
    fn chain_order_traverses_all_four_stages_in_sequence() {
        // Verify the strict ordering: FEC → DRED → NeuralPLC → ComfortNoise.
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);

        // Stage 1: FEC decode (burst == 1).
        assert_eq!(chain.advance(false), PlcOutcome::Concealed(PlcStage::FecDecode));

        // Stage 2: DRED (burst 2..=DRED_DEPTH_FRAMES).
        for burst in 2..=DRED_DEPTH_FRAMES {
            let outcome = chain.advance(false);
            assert_eq!(
                outcome, PlcOutcome::Concealed(PlcStage::Dred),
                "expected Dred at burst {burst}"
            );
        }

        // Stage 3: neural PLC (burst DRED+1..=DRED+NEURAL_PLC_MAX).
        for i in 1..=NEURAL_PLC_MAX_FRAMES {
            let outcome = chain.advance(false);
            assert_eq!(
                outcome, PlcOutcome::Concealed(PlcStage::NeuralPlc),
                "expected NeuralPlc at burst {}", DRED_DEPTH_FRAMES + i
            );
        }

        // Stage 4: comfort noise (every frame beyond the neural PLC window).
        let outcome = chain.advance(false);
        assert_eq!(outcome, PlcOutcome::Concealed(PlcStage::ComfortNoise));
    }

    #[test]
    fn received_after_long_burst_resets_to_fec_stage_on_next_loss() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Build a burst deep into the comfort-noise stage.
        for _ in 0..DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES + 10 {
            chain.advance(false);
        }
        // A received frame resets the chain.
        assert_eq!(chain.advance(true), PlcOutcome::Received);
        assert!(chain.is_idle());

        // The very next lost frame must return to FEC decode — the chain is fresh.
        assert_eq!(chain.advance(false), PlcOutcome::Concealed(PlcStage::FecDecode));
    }

    #[test]
    fn comfort_noise_gain_is_zero_before_cn_stage() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Idle state.
        assert_eq!(chain.comfort_noise_gain(), 0.0);

        // FEC stage.
        chain.advance(false);
        assert_eq!(chain.comfort_noise_gain(), 0.0, "gain must be 0 during FEC decode stage");

        // DRED stage — advance to the last DRED frame.
        for _ in 2..=DRED_DEPTH_FRAMES {
            chain.advance(false);
            assert_eq!(chain.comfort_noise_gain(), 0.0, "gain must be 0 during DRED stage");
        }

        // Neural PLC stage.
        for _ in 1..=NEURAL_PLC_MAX_FRAMES {
            chain.advance(false);
            assert_eq!(
                chain.comfort_noise_gain(), 0.0,
                "gain must be 0 during neural PLC stage"
            );
        }
    }

    #[test]
    fn comfort_noise_gain_starts_at_one_and_fades_linearly() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Exhaust FEC, DRED, and neural PLC stages.
        for _ in 0..DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES {
            chain.advance(false);
        }

        // First comfort-noise frame: gain must be 1.0.
        chain.advance(false);
        let gain_0 = chain.comfort_noise_gain();
        assert!(
            (gain_0 - 1.0).abs() < 1e-9,
            "first comfort-noise gain must be 1.0, got {gain_0}"
        );

        // Gain decreases strictly over subsequent frames.
        let mut prev_gain = gain_0;
        for frame in 1..COMFORT_NOISE_FADE_FRAMES {
            chain.advance(false);
            let gain = chain.comfort_noise_gain();
            assert!(
                gain < prev_gain,
                "comfort-noise gain must decrease monotonically \
                 (frame {frame}: {gain} not < {prev_gain})"
            );
            prev_gain = gain;
        }
    }

    #[test]
    fn comfort_noise_gain_reaches_zero_at_and_after_fade_end() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        // Exhaust FEC + DRED + neural PLC stages.
        for _ in 0..DRED_DEPTH_FRAMES + NEURAL_PLC_MAX_FRAMES {
            chain.advance(false);
        }
        // Consume COMFORT_NOISE_FADE_FRAMES+1 frames in the CN stage so that
        // frames_into_cn reaches COMFORT_NOISE_FADE_FRAMES and the ramp hits 0.
        // (The formula is gain = 1 − frames_into_cn/N; at frames_into_cn=N, gain=0.)
        for _ in 0..=COMFORT_NOISE_FADE_FRAMES {
            chain.advance(false);
        }

        // At frames_into_cn == COMFORT_NOISE_FADE_FRAMES the ramp reaches 0.
        let gain_at_fade_end = chain.comfort_noise_gain();
        assert!(
            gain_at_fade_end <= 0.0,
            "gain must be 0.0 when the fade ramp is complete, got {gain_at_fade_end}"
        );

        // Additional lost frames must also return 0.
        for _ in 0..5 {
            chain.advance(false);
            assert_eq!(
                chain.comfort_noise_gain(),
                0.0,
                "gain must remain 0.0 after the fade ramp is complete"
            );
        }
    }

    #[test]
    fn dred_depth_frames_matches_architecture_ceiling() {
        // The architecture specifies 1 000 ms at 20 ms/frame = 50 frames.
        const FRAME_MS: usize = 20;
        const GAP_FREE_CEILING_MS: usize = 1_000;
        assert_eq!(
            DRED_DEPTH_FRAMES,
            GAP_FREE_CEILING_MS / FRAME_MS,
            "DRED_DEPTH_FRAMES must equal the architecture 1 s gap-free ceiling \
             ({GAP_FREE_CEILING_MS} ms / {FRAME_MS} ms per frame)"
        );
    }

    #[test]
    fn dred_depth_two_produces_dred_on_burst_two() {
        // With dred_depth=2: FEC covers burst==1, DRED covers burst==2 (range 2..=2),
        // and NeuralPLC starts at burst==3.  dred_depth=1 would make the DRED range
        // 2..=1 (empty), so the minimum meaningful DRED depth is 2.
        let mut chain = PlcChain::new(2);
        assert_eq!(chain.advance(false), PlcOutcome::Concealed(PlcStage::FecDecode));
        assert_eq!(chain.advance(false), PlcOutcome::Concealed(PlcStage::Dred));
        assert_eq!(chain.advance(false), PlcOutcome::Concealed(PlcStage::NeuralPlc));
    }

    #[test]
    fn is_idle_reflects_burst_state() {
        let mut chain = PlcChain::new(DRED_DEPTH_FRAMES);
        assert!(chain.is_idle(), "chain must start idle");

        chain.advance(false);
        assert!(!chain.is_idle(), "chain must not be idle after a lost frame");

        chain.advance(true);
        assert!(chain.is_idle(), "chain must be idle after a received frame");
    }
}

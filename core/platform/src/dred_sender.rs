//! DRED neural redundancy sender — Feature 52.
//!
//! Opus 1.5 DRED (Deep REDundancy for Opus) encodes the last `depth` frames
//! of audio into every outgoing packet.  When a receiver encounters a loss
//! burst, it reconstructs the missing frames from the DRED payload carried in
//! the first non-lost post-burst packet, up to a coverage ceiling of `depth`
//! frames.
//!
//! # Coverage and the 1-second ceiling
//!
//! At [`MAX_DRED_DEPTH_FRAMES`] (50 frames × 20 ms = 1 000 ms) every loss
//! burst within the architecture ceiling is fully reconstructed by the
//! receiver with no audible gap, satisfying the "zero voice_gaps" criterion
//! (Feature 169).  Loss bursts longer than the current depth fall through to
//! the neural PLC stage of the plc_chain (Feature 57).
//!
//! # Bitrate overhead
//!
//! Each redundant frame adds approximately [`DRED_BITS_PER_FRAME`] bits to
//! every outgoing packet.  The Opus 1.5 DRED encoder targets ≈ 800 bps per
//! second of coverage; at 20 ms / 50 pps that is 16 bits per depth frame
//! per packet, or [`DRED_OVERHEAD_BPS_PER_FRAME`] = 800 bps per frame of
//! depth.  At the architecture maximum (50 frames) the overhead is:
//!
//! ```text
//! 50 frames × 800 bps/frame = 40 000 bps (40 kbps)
//! ```
//!
//! The governor (Feature 53) updates the depth at each 10 Hz tick via
//! [`DredSender::set_depth`], using the Gilbert-Elliott burst estimate to
//! match overhead to actual channel conditions rather than holding it at
//! the worst-case ceiling.
//!
//! # Relationship to plc_chain
//!
//! The receiver concealment chain (`plc_chain`, Feature 57) relies on the
//! DRED depth configured here.  For the chain's DRED stage to recover a
//! burst the sender must have embedded at least `burst_frames` frames of
//! history into the first surviving post-burst packet.  The minimum depth
//! required to cover a given burst length is computed by
//! [`dred_depth_from_burst_ms`].

/// Opus frame duration in milliseconds at the constrained-assist tier.
///
/// All DRED depth calculations use this frame period.  Matches the
/// 20 ms default Opus frame size set at the constrained tier.
pub const DRED_FRAME_DURATION_MS: usize = 20;

/// Architecture gap-free ceiling — maximum DRED depth in frames.
///
/// 50 frames × 20 ms = 1 000 ms.  Every loss burst within this bound is
/// reconstructed by the receiver from the DRED payload in the first
/// non-lost post-burst packet.  Mirrors [`crate::plc_chain::DRED_DEPTH_FRAMES`].
pub const MAX_DRED_DEPTH_FRAMES: usize = 1_000 / DRED_FRAME_DURATION_MS; // 50

/// Minimum DRED depth in frames; 0 disables DRED entirely.
pub const MIN_DRED_DEPTH_FRAMES: usize = 0;

/// Approximate bits added to each packet per redundant DRED depth frame.
///
/// Derived from the Opus 1.5 DRED target of ≈ 800 bps per second of
/// coverage.  At 50 frames/s (20 ms frame period): 800 / 50 = 16 bits.
pub const DRED_BITS_PER_FRAME: usize = 16;

/// Bitrate overhead per depth frame at the 20 ms Opus packet cadence (bps).
///
/// `DRED_BITS_PER_FRAME × 1_000 / DRED_FRAME_DURATION_MS`
/// = 16 bits × 50 pps = 800 bps per frame of depth.
pub const DRED_OVERHEAD_BPS_PER_FRAME: u32 = 800;

/// Stateful DRED redundancy sender (Feature 52).
///
/// The audio encode loop calls [`DredSender::depth_frames`] each time it
/// encodes a packet to read the current DRED embedding depth.  The governor
/// calls [`DredSender::set_depth`] at 10 Hz to adjust the depth from the
/// Gilbert-Elliott burst estimate (Feature 53).
///
/// # Example
///
/// ```rust
/// use lowband_platform::dred_sender::{DredSender, MAX_DRED_DEPTH_FRAMES};
///
/// // Start at the architecture ceiling for maximum gap-free coverage.
/// let mut sender = DredSender::new(MAX_DRED_DEPTH_FRAMES);
/// assert_eq!(sender.depth_frames(), 50);
/// assert_eq!(sender.depth_ms(), 1_000);
///
/// // Governor downgrades depth to save bandwidth when channel is clean.
/// sender.set_depth(10);
/// assert_eq!(sender.depth_frames(), 10);
/// assert_eq!(sender.depth_ms(), 200);
/// ```
#[derive(Debug, Clone)]
pub struct DredSender {
    depth_frames: usize,
}

impl DredSender {
    /// Create a new sender with the given DRED depth.
    ///
    /// `depth_frames` is clamped to `[MIN_DRED_DEPTH_FRAMES,
    /// MAX_DRED_DEPTH_FRAMES]`.  Pass [`MAX_DRED_DEPTH_FRAMES`] to start
    /// at the architecture gap-free ceiling.
    pub fn new(depth_frames: usize) -> Self {
        Self { depth_frames: depth_frames.clamp(MIN_DRED_DEPTH_FRAMES, MAX_DRED_DEPTH_FRAMES) }
    }

    /// Update the DRED embedding depth.
    ///
    /// The new value is clamped to `[MIN_DRED_DEPTH_FRAMES,
    /// MAX_DRED_DEPTH_FRAMES]`.  The change takes effect on the next
    /// encode call; no reframing or renegotiation is required.
    pub fn set_depth(&mut self, depth_frames: usize) {
        self.depth_frames =
            depth_frames.clamp(MIN_DRED_DEPTH_FRAMES, MAX_DRED_DEPTH_FRAMES);
    }

    /// Current DRED embedding depth in frames.
    ///
    /// Returns 0 when DRED is disabled.
    pub fn depth_frames(&self) -> usize {
        self.depth_frames
    }

    /// Current DRED coverage in milliseconds (`depth_frames × DRED_FRAME_DURATION_MS`).
    pub fn depth_ms(&self) -> usize {
        self.depth_frames * DRED_FRAME_DURATION_MS
    }

    /// Whether DRED embedding is currently active (depth > 0).
    pub fn is_active(&self) -> bool {
        self.depth_frames > 0
    }

    /// Estimated bitrate overhead from DRED at the current depth (bps).
    ///
    /// Computed as `depth_frames × DRED_OVERHEAD_BPS_PER_FRAME`.  This
    /// overhead must be subtracted from the audio bitrate budget before
    /// configuring the Opus encoder target; the governor uses it when
    /// allocating the per-stream budget.
    pub fn overhead_bps(&self) -> u32 {
        self.depth_frames as u32 * DRED_OVERHEAD_BPS_PER_FRAME
    }
}

/// Minimum DRED depth in frames required to cover a loss burst of `burst_ms`.
///
/// Returns `ceil(burst_ms / DRED_FRAME_DURATION_MS)` clamped to
/// `[MIN_DRED_DEPTH_FRAMES, MAX_DRED_DEPTH_FRAMES]`.  The governor (Feature 53)
/// calls this with the Gilbert-Elliott mean burst length to derive the minimum
/// depth needed to reconstruct expected bursts without over-provisioning overhead
/// for clean channels.
///
/// # Examples
///
/// ```rust
/// use lowband_platform::dred_sender::{
///     dred_depth_from_burst_ms, DRED_FRAME_DURATION_MS, MAX_DRED_DEPTH_FRAMES,
/// };
///
/// // 1 frame (20 ms) covers a single-frame burst (FEC handles this too).
/// assert_eq!(dred_depth_from_burst_ms(20.0), 1);
///
/// // 3 frames covers a 60 ms burst exactly.
/// assert_eq!(dred_depth_from_burst_ms(60.0), 3);
///
/// // Ceiling: 1 000 ms requires the full 50-frame depth.
/// assert_eq!(dred_depth_from_burst_ms(1_000.0), MAX_DRED_DEPTH_FRAMES);
///
/// // Bursts longer than 1 s are still clamped — DRED cannot cover them.
/// assert_eq!(dred_depth_from_burst_ms(2_000.0), MAX_DRED_DEPTH_FRAMES);
/// ```
pub fn dred_depth_from_burst_ms(burst_ms: f64) -> usize {
    if burst_ms <= 0.0 {
        return MIN_DRED_DEPTH_FRAMES;
    }
    let frames = (burst_ms / DRED_FRAME_DURATION_MS as f64).ceil() as usize;
    frames.clamp(MIN_DRED_DEPTH_FRAMES, MAX_DRED_DEPTH_FRAMES)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plc_chain::DRED_DEPTH_FRAMES;

    // ── Architecture constants ─────────────────────────────────────────────────

    #[test]
    fn max_depth_frames_matches_plc_chain_constant() {
        assert_eq!(
            MAX_DRED_DEPTH_FRAMES,
            DRED_DEPTH_FRAMES,
            "DredSender ceiling must match the plc_chain DRED_DEPTH_FRAMES architecture constant"
        );
    }

    #[test]
    fn max_depth_covers_one_second() {
        assert_eq!(
            MAX_DRED_DEPTH_FRAMES * DRED_FRAME_DURATION_MS,
            1_000,
            "MAX_DRED_DEPTH_FRAMES × DRED_FRAME_DURATION_MS must equal 1 000 ms"
        );
    }

    #[test]
    fn overhead_bps_per_frame_matches_formula() {
        let expected = DRED_BITS_PER_FRAME as u32 * (1_000 / DRED_FRAME_DURATION_MS as u32);
        assert_eq!(
            DRED_OVERHEAD_BPS_PER_FRAME,
            expected,
            "DRED_OVERHEAD_BPS_PER_FRAME must equal DRED_BITS_PER_FRAME × pps"
        );
    }

    // ── DredSender::new ────────────────────────────────────────────────────────

    #[test]
    fn new_at_max_depth() {
        let s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        assert_eq!(s.depth_frames(), MAX_DRED_DEPTH_FRAMES);
    }

    #[test]
    fn new_at_zero_depth_is_disabled() {
        let s = DredSender::new(0);
        assert_eq!(s.depth_frames(), 0);
        assert!(!s.is_active());
    }

    #[test]
    fn new_clamps_above_max() {
        let s = DredSender::new(MAX_DRED_DEPTH_FRAMES + 100);
        assert_eq!(
            s.depth_frames(),
            MAX_DRED_DEPTH_FRAMES,
            "depth above MAX must be clamped to MAX_DRED_DEPTH_FRAMES"
        );
    }

    // ── DredSender::set_depth ──────────────────────────────────────────────────

    #[test]
    fn set_depth_updates_within_bounds() {
        let mut s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        s.set_depth(10);
        assert_eq!(s.depth_frames(), 10);
    }

    #[test]
    fn set_depth_clamps_above_max() {
        let mut s = DredSender::new(5);
        s.set_depth(MAX_DRED_DEPTH_FRAMES + 1);
        assert_eq!(
            s.depth_frames(),
            MAX_DRED_DEPTH_FRAMES,
            "set_depth above MAX must clamp to MAX_DRED_DEPTH_FRAMES"
        );
    }

    #[test]
    fn set_depth_to_zero_disables_dred() {
        let mut s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        s.set_depth(0);
        assert_eq!(s.depth_frames(), 0);
        assert!(!s.is_active(), "DRED must be inactive when depth is 0");
        assert_eq!(s.overhead_bps(), 0, "overhead must be 0 when DRED is disabled");
    }

    // ── DredSender::depth_ms ──────────────────────────────────────────────────

    #[test]
    fn depth_ms_scales_with_depth_frames() {
        for frames in [0, 1, 5, 10, 25, 50] {
            let s = DredSender::new(frames);
            assert_eq!(
                s.depth_ms(),
                frames * DRED_FRAME_DURATION_MS,
                "depth_ms must be depth_frames × DRED_FRAME_DURATION_MS (frames={frames})"
            );
        }
    }

    #[test]
    fn depth_ms_at_max_is_one_second() {
        let s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        assert_eq!(
            s.depth_ms(),
            1_000,
            "max-depth sender must cover exactly 1 000 ms"
        );
    }

    // ── DredSender::overhead_bps ──────────────────────────────────────────────

    #[test]
    fn overhead_bps_at_max_depth_is_bounded() {
        let s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        let overhead = s.overhead_bps();
        // 50 frames × 800 bps/frame = 40 000 bps.
        assert_eq!(overhead, 40_000, "overhead at max depth must be 40 kbps");
    }

    #[test]
    fn overhead_bps_is_linear_in_depth() {
        let s10 = DredSender::new(10);
        let s20 = DredSender::new(20);
        assert_eq!(
            s20.overhead_bps(),
            2 * s10.overhead_bps(),
            "DRED overhead must scale linearly with depth"
        );
    }

    #[test]
    fn overhead_bps_zero_when_disabled() {
        let s = DredSender::new(0);
        assert_eq!(s.overhead_bps(), 0);
    }

    // ── DredSender::is_active ─────────────────────────────────────────────────

    #[test]
    fn is_active_true_when_depth_positive() {
        for frames in [1, 2, 10, MAX_DRED_DEPTH_FRAMES] {
            let s = DredSender::new(frames);
            assert!(s.is_active(), "is_active must be true for depth {frames}");
        }
    }

    // ── dred_depth_from_burst_ms ──────────────────────────────────────────────

    #[test]
    fn depth_from_burst_zero_is_zero() {
        assert_eq!(dred_depth_from_burst_ms(0.0), 0);
    }

    #[test]
    fn depth_from_burst_negative_is_zero() {
        assert_eq!(dred_depth_from_burst_ms(-100.0), 0);
    }

    #[test]
    fn depth_from_burst_exact_frame_multiples() {
        for frames in 1usize..=5 {
            let burst_ms = (frames * DRED_FRAME_DURATION_MS) as f64;
            assert_eq!(
                dred_depth_from_burst_ms(burst_ms),
                frames,
                "burst_ms={burst_ms} must require exactly {frames} depth frame(s)"
            );
        }
    }

    #[test]
    fn depth_from_burst_rounds_up_non_multiple() {
        // 30 ms burst → ceil(30/20) = 2 frames.
        assert_eq!(dred_depth_from_burst_ms(30.0), 2);
        // 1 ms burst → ceil(1/20) = 1 frame.
        assert_eq!(dred_depth_from_burst_ms(1.0), 1);
        // 21 ms burst → ceil(21/20) = 2 frames.
        assert_eq!(dred_depth_from_burst_ms(21.0), 2);
    }

    #[test]
    fn depth_from_burst_1000ms_reaches_architecture_ceiling() {
        assert_eq!(
            dred_depth_from_burst_ms(1_000.0),
            MAX_DRED_DEPTH_FRAMES,
            "1 000 ms burst requires exactly MAX_DRED_DEPTH_FRAMES"
        );
    }

    #[test]
    fn depth_from_burst_above_ceiling_clamps() {
        assert_eq!(
            dred_depth_from_burst_ms(2_000.0),
            MAX_DRED_DEPTH_FRAMES,
            "burst > 1 000 ms must still clamp to MAX_DRED_DEPTH_FRAMES"
        );
        assert_eq!(
            dred_depth_from_burst_ms(10_000.0),
            MAX_DRED_DEPTH_FRAMES,
            "extreme burst must still clamp to MAX_DRED_DEPTH_FRAMES"
        );
    }

    #[test]
    fn depth_from_burst_covers_multi_hundred_ms_bursts() {
        // Architecture scenario: multi-hundred-millisecond loss bursts are
        // the target for DRED (Feature 52 description).
        let burst_200ms = dred_depth_from_burst_ms(200.0);
        let burst_500ms = dred_depth_from_burst_ms(500.0);
        let burst_800ms = dred_depth_from_burst_ms(800.0);

        assert!(
            burst_200ms <= MAX_DRED_DEPTH_FRAMES,
            "200 ms burst depth {burst_200ms} must fit within MAX_DRED_DEPTH_FRAMES"
        );
        assert!(
            burst_500ms <= MAX_DRED_DEPTH_FRAMES,
            "500 ms burst depth {burst_500ms} must fit within MAX_DRED_DEPTH_FRAMES"
        );
        assert!(
            burst_800ms <= MAX_DRED_DEPTH_FRAMES,
            "800 ms burst depth {burst_800ms} must fit within MAX_DRED_DEPTH_FRAMES"
        );

        // Depths must increase monotonically with burst length.
        assert!(
            burst_200ms <= burst_500ms,
            "longer burst must require at least as much DRED depth"
        );
        assert!(
            burst_500ms <= burst_800ms,
            "longer burst must require at least as much DRED depth"
        );
    }

    #[test]
    fn sender_at_max_depth_covers_all_architecture_bursts() {
        // A DredSender at MAX_DRED_DEPTH_FRAMES covers every burst that the
        // plc_chain DRED stage can handle, including the 1 s worst case.
        let s = DredSender::new(MAX_DRED_DEPTH_FRAMES);
        for burst_ms in [20.0_f64, 60.0, 200.0, 500.0, 1_000.0] {
            let required_depth = dred_depth_from_burst_ms(burst_ms);
            assert!(
                s.depth_frames() >= required_depth,
                "sender depth {} must cover {burst_ms} ms burst (requires {required_depth} frames)",
                s.depth_frames()
            );
        }
    }
}

//! LBRR in-band FEC — Feature 51.
//!
//! Opus SILK LBRR (Low Bit-Rate Redundancy) embeds a compressed copy of the
//! previous SILK frame alongside every outgoing packet.  When the receiver
//! detects an isolated single-frame loss (burst == 1), it decodes the embedded
//! payload with `decode_fec = true` to reconstruct the lost audio with no
//! audible artefact.
//!
//! # Sender side
//!
//! [`LbrrEncoder`] tracks whether LBRR embedding is active and the current
//! `packet_loss_perc` hint.  The governor calls
//! [`LbrrEncoder::set_loss_rate`] at each 10 Hz tick from the
//! [`GilbertElliottEstimator`]'s `loss_rate()`.  When the measured rate meets
//! or exceeds [`LBRR_ENABLE_THRESHOLD`] the encoder enables LBRR via
//! `OPUS_SET_INBAND_FEC(1)` and updates `OPUS_SET_PACKET_LOSS_PERC`; on a
//! clean channel it disables LBRR to reclaim the [`LBRR_OVERHEAD_BPS`]
//! overhead for the audio bitrate.
//!
//! # Receiver side
//!
//! [`LbrrDecoder`] tracks isolated-loss events surfaced by the plc_chain
//! (Feature 57).  When [`PlcStage::FecDecode`] fires (burst == 1), the
//! receive loop calls [`LbrrDecoder::mark_isolated_loss`].  On the
//! immediately following packet it calls [`LbrrDecoder::consume_fec_pending`];
//! `true` means the Opus decoder must be called with `decode_fec = true` to
//! reconstruct the lost frame from the LBRR payload embedded in that packet.
//!
//! # Coverage
//!
//! LBRR covers only isolated (burst == 1) losses.  Longer bursts are handed
//! to the DRED (Feature 52) and neural-PLC stages of the plc_chain; the two
//! mechanisms are non-overlapping.
//!
//! # Bitrate overhead
//!
//! At the constrained-tier Opus SILK target of 16–24 kbps, LBRR adds
//! approximately [`LBRR_OVERHEAD_BPS`] per second.  The governor subtracts
//! this from the audio budget when [`LbrrEncoder::is_enabled`] returns `true`.
//!
//! [`GilbertElliottEstimator`]: lowband_lbtp::fec::GilbertElliottEstimator
//! [`PlcStage::FecDecode`]: crate::plc_chain::PlcStage::FecDecode

/// Opus frame duration in milliseconds at the constrained-assist tier.
///
/// Matches the 20 ms Opus frame size used at the constrained tier.
pub const LBRR_FRAME_DURATION_MS: usize = 20;

/// Approximate bitrate overhead added by LBRR when enabled (bps).
///
/// Opus SILK LBRR embeds a compressed copy of the previous SILK frame.  At
/// the constrained-tier target of 16–24 kbps this redundant payload costs
/// approximately 2 kbps (40 bits per packet × 50 pps = 2 000 bps).  The
/// governor subtracts this from the audio budget when LBRR is active.
pub const LBRR_OVERHEAD_BPS: u32 = 2_000;

/// Loss-rate fraction below which LBRR is disabled to reclaim overhead.
///
/// On a clean channel (measured loss < this threshold) LBRR is turned off
/// and the [`LBRR_OVERHEAD_BPS`] overhead is returned to the audio bitrate
/// budget.  Above the threshold every packet carries redundancy for the
/// previous frame, recovering any isolated loss without a voice gap.
///
/// 0.001 (0.1 %) sits below the 5 % GE channel target so that any real-world
/// loss rate activates LBRR, while thermal noise below one loss per thousand
/// packets does not pay the overhead.
pub const LBRR_ENABLE_THRESHOLD: f64 = 0.001;

/// Stateful LBRR encoder policy (Feature 51).
///
/// The audio encode loop reads [`LbrrEncoder::is_enabled`] each tick to
/// decide whether to pass `OPUS_SET_INBAND_FEC(1)` to the Opus encoder, and
/// reads [`LbrrEncoder::packet_loss_perc`] for `OPUS_SET_PACKET_LOSS_PERC`.
/// The governor calls [`LbrrEncoder::set_loss_rate`] at 10 Hz with the
/// current [`GilbertElliottEstimator::loss_rate`].
///
/// # Example
///
/// ```rust
/// use lowband_platform::lbrr_fec::{LbrrEncoder, LBRR_OVERHEAD_BPS};
///
/// let mut enc = LbrrEncoder::new();
/// assert!(!enc.is_enabled());
/// assert_eq!(enc.overhead_bps(), 0);
///
/// // Governor detects 5 % loss → enable LBRR.
/// enc.set_loss_rate(0.05);
/// assert!(enc.is_enabled());
/// assert_eq!(enc.packet_loss_perc(), 5);
/// assert_eq!(enc.overhead_bps(), LBRR_OVERHEAD_BPS);
///
/// // Clean channel → LBRR disabled, overhead reclaimed.
/// enc.set_loss_rate(0.0);
/// assert!(!enc.is_enabled());
/// assert_eq!(enc.overhead_bps(), 0);
/// ```
///
/// [`GilbertElliottEstimator::loss_rate`]: lowband_lbtp::fec::GilbertElliottEstimator::loss_rate
#[derive(Debug, Clone)]
pub struct LbrrEncoder {
    enabled: bool,
    packet_loss_perc: u8,
}

impl LbrrEncoder {
    /// Create a new encoder with LBRR disabled.
    ///
    /// The governor must call [`set_loss_rate`] at the first 10 Hz tick
    /// before the first packet is encoded so that the loss hint is current.
    ///
    /// [`set_loss_rate`]: LbrrEncoder::set_loss_rate
    pub fn new() -> Self {
        Self { enabled: false, packet_loss_perc: 0 }
    }

    /// Update LBRR state from the measured link loss rate.
    ///
    /// `loss_rate` is a fraction in `[0.0, 1.0]`.  Values above
    /// [`LBRR_ENABLE_THRESHOLD`] enable LBRR; `packet_loss_perc` is set to
    /// `(loss_rate × 100).round()` clamped to `[1, 100]`.  Values at or
    /// below the threshold disable LBRR and set `packet_loss_perc` to zero.
    pub fn set_loss_rate(&mut self, loss_rate: f64) {
        let clamped = loss_rate.clamp(0.0, 1.0);
        if clamped >= LBRR_ENABLE_THRESHOLD {
            self.enabled = true;
            // Round to nearest integer percentage, floor-clamped to 1 so the
            // Opus encoder always receives a nonzero hint when LBRR is active.
            self.packet_loss_perc = ((clamped * 100.0).round() as u8).max(1);
        } else {
            self.enabled = false;
            self.packet_loss_perc = 0;
        }
    }

    /// Whether LBRR in-band FEC is currently active.
    ///
    /// When `true`, configure the Opus encoder with
    /// `OPUS_SET_INBAND_FEC(1)` and
    /// `OPUS_SET_PACKET_LOSS_PERC(self.packet_loss_perc())`.
    /// When `false`, configure with `OPUS_SET_INBAND_FEC(0)`.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Packet loss percentage for `OPUS_SET_PACKET_LOSS_PERC` (range 0–100).
    ///
    /// `0` when LBRR is disabled; otherwise the measured loss rate rounded
    /// to the nearest integer percentage (minimum 1 when enabled).  The Opus
    /// SILK encoder uses this hint to tune the redundancy bitrate.
    pub fn packet_loss_perc(&self) -> u8 {
        self.packet_loss_perc
    }

    /// Estimated bitrate overhead from LBRR at the current state (bps).
    ///
    /// Returns [`LBRR_OVERHEAD_BPS`] when enabled, `0` when disabled.  The
    /// governor subtracts this from the audio bitrate budget before setting
    /// the Opus encoder target rate.
    pub fn overhead_bps(&self) -> u32 {
        if self.enabled { LBRR_OVERHEAD_BPS } else { 0 }
    }
}

impl Default for LbrrEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Stateful LBRR receiver policy (Feature 51).
///
/// When the plc_chain (Feature 57) returns [`PlcStage::FecDecode`] for a
/// burst-1 loss, the receive loop calls [`LbrrDecoder::mark_isolated_loss`].
/// On the next arriving packet it calls [`LbrrDecoder::consume_fec_pending`];
/// `true` means the Opus decoder must use `decode_fec = true` to reconstruct
/// the previously lost frame from the LBRR payload embedded in that packet.
///
/// The FEC flag is consumed on the first call regardless of the return value;
/// only the packet *immediately following* the loss carries the LBRR payload.
///
/// # Example
///
/// ```rust
/// use lowband_platform::lbrr_fec::LbrrDecoder;
///
/// let mut dec = LbrrDecoder::new();
/// assert!(!dec.is_fec_pending());
///
/// // plc_chain signals an isolated loss (burst == 1, PlcStage::FecDecode).
/// dec.mark_isolated_loss();
/// assert!(dec.is_fec_pending());
///
/// // Next packet: FEC-decode it to recover the lost frame.
/// assert!(dec.consume_fec_pending());
/// assert!(!dec.is_fec_pending()); // flag consumed
///
/// // Subsequent packet: no preceding loss, normal decode.
/// assert!(!dec.consume_fec_pending());
/// ```
///
/// [`PlcStage::FecDecode`]: crate::plc_chain::PlcStage::FecDecode
#[derive(Debug, Clone, Default)]
pub struct LbrrDecoder {
    fec_pending: bool,
}

impl LbrrDecoder {
    /// Create a new decoder with no pending FEC.
    pub fn new() -> Self {
        Self { fec_pending: false }
    }

    /// Signal that an isolated single-frame loss (burst == 1) occurred.
    ///
    /// Called when the plc_chain returns [`PlcStage::FecDecode`].  The next
    /// packet that arrives must be decoded with `decode_fec = true` to recover
    /// the lost frame from its embedded LBRR payload.
    ///
    /// [`PlcStage::FecDecode`]: crate::plc_chain::PlcStage::FecDecode
    pub fn mark_isolated_loss(&mut self) {
        self.fec_pending = true;
    }

    /// Returns `true` if the current packet must be decoded with `decode_fec = true`.
    ///
    /// Consumes the pending flag — the LBRR payload is present only in the
    /// packet immediately following the loss.  Call once per arriving packet;
    /// the flag is cleared regardless of the return value.
    pub fn consume_fec_pending(&mut self) -> bool {
        let pending = self.fec_pending;
        self.fec_pending = false;
        pending
    }

    /// Whether an FEC decode is pending for the next arriving packet.
    ///
    /// Non-consuming inspection.  Use [`consume_fec_pending`] when actually
    /// processing the next packet.
    ///
    /// [`consume_fec_pending`]: LbrrDecoder::consume_fec_pending
    pub fn is_fec_pending(&self) -> bool {
        self.fec_pending
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn frame_duration_matches_constrained_tier() {
        assert_eq!(LBRR_FRAME_DURATION_MS, 20);
    }

    #[test]
    fn overhead_bps_fits_below_16kbps_floor() {
        // LBRR overhead must leave headroom within the constrained-tier
        // 16 kbps audio floor (Feature 48).
        const AUDIO_FLOOR_BPS: u32 = 16_000;
        assert!(
            LBRR_OVERHEAD_BPS < AUDIO_FLOOR_BPS,
            "LBRR_OVERHEAD_BPS {LBRR_OVERHEAD_BPS} must be < audio floor {AUDIO_FLOOR_BPS} bps"
        );
    }

    #[test]
    fn enable_threshold_activates_on_5pct_ge_loss() {
        // The canonical 5 % GE channel must always trigger LBRR.
        assert!(
            LBRR_ENABLE_THRESHOLD < 0.05,
            "LBRR_ENABLE_THRESHOLD {LBRR_ENABLE_THRESHOLD} must be below the \
             5% GE loss target so that the canonical channel enables LBRR"
        );
    }

    // ── LbrrEncoder::new ──────────────────────────────────────────────────────

    #[test]
    fn encoder_starts_disabled() {
        let enc = LbrrEncoder::new();
        assert!(!enc.is_enabled());
        assert_eq!(enc.packet_loss_perc(), 0);
        assert_eq!(enc.overhead_bps(), 0);
    }

    // ── LbrrEncoder::set_loss_rate ────────────────────────────────────────────

    #[test]
    fn loss_rate_above_threshold_enables_lbrr() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(0.05);
        assert!(enc.is_enabled(), "5% loss must enable LBRR");
        assert_eq!(enc.packet_loss_perc(), 5);
        assert_eq!(enc.overhead_bps(), LBRR_OVERHEAD_BPS);
    }

    #[test]
    fn loss_rate_zero_disables_lbrr() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(0.05);
        enc.set_loss_rate(0.0);
        assert!(!enc.is_enabled(), "zero loss must disable LBRR");
        assert_eq!(enc.packet_loss_perc(), 0);
        assert_eq!(enc.overhead_bps(), 0);
    }

    #[test]
    fn loss_rate_below_threshold_disables_lbrr() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(0.05); // enable first
        enc.set_loss_rate(LBRR_ENABLE_THRESHOLD / 2.0);
        assert!(!enc.is_enabled(), "loss below threshold must disable LBRR");
        assert_eq!(enc.overhead_bps(), 0);
    }

    #[test]
    fn loss_rate_at_threshold_enables_lbrr() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(LBRR_ENABLE_THRESHOLD);
        assert!(enc.is_enabled(), "loss exactly at threshold must enable LBRR");
        assert!(enc.packet_loss_perc() >= 1, "packet_loss_perc must be at least 1 when enabled");
    }

    #[test]
    fn loss_rate_clamped_above_one() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(2.0); // absurd value
        assert_eq!(enc.packet_loss_perc(), 100, "packet_loss_perc must clamp to 100");
    }

    #[test]
    fn packet_loss_perc_rounds_to_nearest_integer() {
        let cases: &[(f64, u8)] = &[
            (0.01,  1),  // 1.0 → 1
            (0.05,  5),  // 5.0 → 5
            (0.10, 10),  // 10.0 → 10
            (0.999, 100), // 99.9 → 100 (rounds up)
        ];
        for &(loss_rate, expected_pct) in cases {
            let mut enc = LbrrEncoder::new();
            enc.set_loss_rate(loss_rate);
            assert_eq!(
                enc.packet_loss_perc(),
                expected_pct,
                "loss_rate={loss_rate}: expected packet_loss_perc={expected_pct}, \
                 got {}",
                enc.packet_loss_perc(),
            );
        }
    }

    // ── LbrrEncoder overhead ──────────────────────────────────────────────────

    #[test]
    fn overhead_is_lbrr_overhead_bps_when_enabled() {
        let mut enc = LbrrEncoder::new();
        enc.set_loss_rate(0.05);
        assert_eq!(enc.overhead_bps(), LBRR_OVERHEAD_BPS);
    }

    #[test]
    fn overhead_is_zero_when_disabled() {
        let enc = LbrrEncoder::new();
        assert_eq!(enc.overhead_bps(), 0);
    }

    // ── LbrrDecoder::new ──────────────────────────────────────────────────────

    #[test]
    fn decoder_starts_with_no_fec_pending() {
        let dec = LbrrDecoder::new();
        assert!(!dec.is_fec_pending());
    }

    // ── LbrrDecoder::mark_isolated_loss ──────────────────────────────────────

    #[test]
    fn mark_isolated_loss_sets_fec_pending() {
        let mut dec = LbrrDecoder::new();
        dec.mark_isolated_loss();
        assert!(dec.is_fec_pending());
    }

    // ── LbrrDecoder::consume_fec_pending ─────────────────────────────────────

    #[test]
    fn consume_returns_true_after_isolated_loss() {
        let mut dec = LbrrDecoder::new();
        dec.mark_isolated_loss();
        assert!(dec.consume_fec_pending());
    }

    #[test]
    fn consume_clears_fec_pending_flag() {
        let mut dec = LbrrDecoder::new();
        dec.mark_isolated_loss();
        let _ = dec.consume_fec_pending();
        assert!(!dec.is_fec_pending(), "FEC flag must clear after consume");
        assert!(!dec.consume_fec_pending(), "second consume must return false");
    }

    #[test]
    fn consume_without_prior_loss_returns_false() {
        let mut dec = LbrrDecoder::new();
        assert!(!dec.consume_fec_pending());
    }

    // ── Interaction: isolated loss recovery sequence ──────────────────────────

    #[test]
    fn isolated_loss_triggers_fec_on_immediately_following_packet_only() {
        let mut dec = LbrrDecoder::new();

        dec.mark_isolated_loss();

        // The packet immediately after the loss must use FEC.
        assert!(
            dec.consume_fec_pending(),
            "first packet after isolated loss must be FEC-decoded"
        );

        // The packet after that carries no LBRR payload for the already-recovered frame.
        assert!(
            !dec.consume_fec_pending(),
            "second packet after isolated loss must NOT be FEC-decoded"
        );
    }

    #[test]
    fn two_isolated_losses_each_trigger_exactly_one_fec_decode() {
        let mut dec = LbrrDecoder::new();

        // First isolated loss.
        dec.mark_isolated_loss();
        assert!(dec.consume_fec_pending(), "first loss: FEC on next packet");
        assert!(!dec.consume_fec_pending(), "first loss: no FEC on subsequent packet");

        // Second isolated loss after several clean packets.
        dec.mark_isolated_loss();
        assert!(dec.consume_fec_pending(), "second loss: FEC on next packet");
        assert!(!dec.consume_fec_pending(), "second loss: no FEC on subsequent packet");
    }

    #[test]
    fn isolated_loss_recovery_trace_fec_count_matches_loss_count() {
        // Simulate a receiver: 5 isolated losses in 100 packets (every 20th
        // packet lost, isolated — no consecutive losses).  Each loss marks FEC
        // pending; the immediately following received packet consumes it.
        // Total FEC decodes must equal total isolated losses.
        let mut dec = LbrrDecoder::new();
        let mut isolated_losses: usize = 0;
        let mut fec_decodes: usize = 0;

        // 0-indexed trace: losses at i = 0, 20, 40, 60, 80 (every 20th).
        // The last loss is at i=80; packets 81-99 follow, so the FEC is always consumed.
        for i in 0u32..100 {
            let is_loss = i % 20 == 0;

            if is_loss {
                isolated_losses += 1;
                dec.mark_isolated_loss();
            } else if dec.consume_fec_pending() {
                fec_decodes += 1;
            }
        }

        assert_eq!(
            fec_decodes,
            isolated_losses,
            "FEC decode count {fec_decodes} must equal isolated loss count {isolated_losses}"
        );
    }
}

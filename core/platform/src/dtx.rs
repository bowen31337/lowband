//! Opus DTX (Discontinuous Transmission) — Feature 54.
//!
//! During silence periods Opus DTX replaces continuous frame transmission with
//! periodic SID (Silence Insertion Descriptor) comfort-noise updates.  Between
//! SID packets nothing is sent, dropping the effective audio bitrate from the
//! normal 16–24 kbps down to [`DTX_SILENCE_BPS`] (100 bps) — a 160–240×
//! reduction at near-zero cost to perceived quality.
//!
//! # Sender side
//!
//! [`DtxEncoder`] is the governor's DTX state machine.  The audio encode loop
//! calls [`DtxEncoder::observe_vad`] once per 20 ms frame with the current
//! voice-activity decision.  The returned [`DtxAction`] tells the loop whether
//! to transmit a full Opus frame, emit a SID comfort-noise update, or suppress
//! this frame entirely.
//!
//! A hangover of [`DTX_HANGOVER_FRAMES`] (8 frames = 160 ms) prevents
//! premature DTX entry on brief pauses: the encoder stays in voice mode for
//! eight consecutive silent frames before switching to silence mode.  Voice
//! activity immediately exits silence mode with no delay.
//!
//! [`DtxEncoder::effective_audio_bps`] returns the bitrate the network
//! actually consumes — [`DTX_SILENCE_BPS`] during silence, `voice_bps` during
//! voice activity — so the governor can derive accurate uplink budget
//! accounting.
//!
//! # Receiver side
//!
//! [`DtxReceiver`] tracks whether the remote sender is in a DTX silence
//! period.  The receive loop calls [`DtxReceiver::observe_packet`] for every
//! arriving packet, setting `is_sid = true` when the Opus TOC byte identifies
//! a comfort-noise frame.  When no packet arrives for a 20 ms playout slot,
//! the caller calls [`DtxReceiver::tick_no_packet`]: `true` means the slot
//! was DTX-suppressed and the caller should generate comfort noise locally;
//! `false` means the gap may be a real packet loss that the PLC chain should
//! handle.
//!
//! # Bitrate arithmetic
//!
//! ```text
//! SID packet: DTX_SID_BYTES bytes every DTX_SID_INTERVAL_FRAMES × 20 ms
//!           = 5 bytes × 8 bits / (20 × 0.020 s)
//!           = 40 bits / 0.400 s
//!           = 100 bps   (DTX_SILENCE_BPS)
//! ```
//!
//! At a typical voice bitrate of 24 kbps, DTX saves 23 900 bps — 99.6 % of
//! the audio budget — during silence.

// ── Constants ────────────────────────────────────────────────────────────────

/// Number of 20 ms frames between consecutive SID comfort-noise updates.
///
/// During a DTX silence period Opus emits one SID packet per interval;
/// all other frames carry zero payload bytes.  20 frames × 20 ms = 400 ms.
pub const DTX_SID_INTERVAL_FRAMES: usize = 20;

/// Payload bytes per SID (Silence Insertion Descriptor) comfort-noise packet.
///
/// A SILK-mode CN frame encodes the spectral envelope of background noise in
/// approximately 5 bytes.  This constant is used to compute the effective
/// audio bitrate during silence.
pub const DTX_SID_BYTES: usize = 5;

/// Effective audio bitrate during a DTX silence period (bps).
///
/// Derived from one [`DTX_SID_BYTES`]-byte packet every
/// [`DTX_SID_INTERVAL_FRAMES`] × 20 ms:
///
/// ```text
/// (5 × 8 bits) × 1 000 ms/s  /  (20 frames × 20 ms/frame)
/// = 40 000  /  400
/// = 100 bps
/// ```
pub const DTX_SILENCE_BPS: u32 =
    DTX_SID_BYTES as u32 * 8 * 1_000 / (DTX_SID_INTERVAL_FRAMES as u32 * 20);

/// Frames of hangover before the encoder enters silence mode.
///
/// After the VAD ceases detecting voice, the encoder continues transmitting
/// full frames for this many frames before switching to DTX silence.  8 frames
/// (160 ms) avoids premature silence insertion on brief pauses (inhalations,
/// thinking gaps) while still activating DTX within 160 ms of true silence.
pub const DTX_HANGOVER_FRAMES: usize = 8;

// ── DtxState ──────────────────────────────────────────────────────────────────

/// Current DTX transmission state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtxState {
    /// Voice activity detected; full Opus frames are transmitted at the
    /// configured voice bitrate.  Also active during the hangover period.
    Voice,
    /// Silence period active; only periodic SID comfort-noise frames are
    /// sent.  Effective bitrate is [`DTX_SILENCE_BPS`].
    Silence,
}

// ── DtxAction ─────────────────────────────────────────────────────────────────

/// Frame-level transmit action returned by [`DtxEncoder::observe_vad`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtxAction {
    /// Voice active or in hangover: transmit a full Opus frame at the
    /// configured voice bitrate.
    Voice,
    /// Silence period + SID interval: transmit a comfort-noise update packet.
    /// The Opus encoder must be called with DTX enabled so it produces a SID
    /// frame rather than a full voice frame.
    Sid,
    /// Silence period between SID updates: suppress this frame entirely.
    /// No bytes are sent to the network for this 20 ms slot.
    Suppress,
}

// ── DtxEncoder ────────────────────────────────────────────────────────────────

/// Sender-side DTX state machine (Feature 54).
///
/// Call [`DtxEncoder::observe_vad`] once per 20 ms frame with the current
/// voice-activity detection result.  The returned [`DtxAction`] tells the
/// audio encode loop what to transmit for that frame.
///
/// # Hangover
///
/// After the VAD detects silence, the encoder stays in [`DtxState::Voice`]
/// for [`DTX_HANGOVER_FRAMES`] consecutive silent frames before transitioning
/// to [`DtxState::Silence`].  Voice activity immediately cancels the hangover
/// and returns to [`DtxState::Voice`].
///
/// # SID cadence
///
/// On the first frame of silence and every [`DTX_SID_INTERVAL_FRAMES`] frames
/// thereafter, [`DtxAction::Sid`] is returned so the encoder emits a
/// comfort-noise update.  All other silence frames return [`DtxAction::Suppress`].
///
/// # Bitrate accounting
///
/// Use [`DtxEncoder::effective_audio_bps`] to obtain the bitrate the uplink
/// actually consumes at the current DTX state for governor budget calculations.
///
/// # Example
///
/// ```rust
/// use lowband_platform::dtx::{DtxEncoder, DtxAction, DtxState, DTX_SILENCE_BPS,
///                              DTX_HANGOVER_FRAMES};
///
/// let mut enc = DtxEncoder::new();
/// assert_eq!(enc.state(), DtxState::Voice);
///
/// // Hangover: full frames continue for DTX_HANGOVER_FRAMES silent ticks.
/// for _ in 0..DTX_HANGOVER_FRAMES {
///     let action = enc.observe_vad(false);
///     assert_eq!(action, DtxAction::Voice);
/// }
/// assert_eq!(enc.state(), DtxState::Voice); // still in hangover state
///
/// // One more silent frame past hangover → first SID.
/// let action = enc.observe_vad(false);
/// assert_eq!(action, DtxAction::Sid);
/// assert_eq!(enc.state(), DtxState::Silence);
///
/// // Subsequent silence frames → suppress.
/// let action = enc.observe_vad(false);
/// assert_eq!(action, DtxAction::Suppress);
///
/// // Effective bitrate drops to near zero during silence.
/// assert_eq!(enc.effective_audio_bps(24_000), DTX_SILENCE_BPS);
///
/// // Voice activity → immediate return to Voice.
/// let action = enc.observe_vad(true);
/// assert_eq!(action, DtxAction::Voice);
/// assert_eq!(enc.state(), DtxState::Voice);
/// assert_eq!(enc.effective_audio_bps(24_000), 24_000);
/// ```
#[derive(Debug, Clone)]
pub struct DtxEncoder {
    state: DtxState,
    hangover_remaining: usize,
    /// Counts frames since we *entered* the Silence state (not since last SID).
    silence_frames: usize,
}

impl DtxEncoder {
    /// Create a new encoder starting in [`DtxState::Voice`].
    ///
    /// The encoder begins in Voice to avoid suppressing audio before the VAD
    /// has observed enough context to make a reliable silence decision.
    pub fn new() -> Self {
        Self {
            state: DtxState::Voice,
            hangover_remaining: DTX_HANGOVER_FRAMES,
            silence_frames: 0,
        }
    }

    /// Advance the DTX state machine and return the transmit action for this frame.
    ///
    /// `voice_active = true`  → voice detected; resets hangover counter,
    ///                          returns to [`DtxState::Voice`], returns
    ///                          [`DtxAction::Voice`].
    ///
    /// `voice_active = false` → silence detected; decrements hangover counter.
    ///                          While hangover > 0: returns [`DtxAction::Voice`].
    ///                          Once hangover reaches 0: transitions to
    ///                          [`DtxState::Silence`]; returns [`DtxAction::Sid`]
    ///                          on the first and every [`DTX_SID_INTERVAL_FRAMES`]
    ///                          frames, otherwise [`DtxAction::Suppress`].
    pub fn observe_vad(&mut self, voice_active: bool) -> DtxAction {
        if voice_active {
            self.state = DtxState::Voice;
            self.hangover_remaining = DTX_HANGOVER_FRAMES;
            self.silence_frames = 0;
            return DtxAction::Voice;
        }

        // Silent frame.
        if self.hangover_remaining > 0 {
            self.hangover_remaining -= 1;
            return DtxAction::Voice;
        }

        // Past hangover → silence mode.
        self.state = DtxState::Silence;
        let idx = self.silence_frames;
        self.silence_frames += 1;

        if idx % DTX_SID_INTERVAL_FRAMES == 0 {
            DtxAction::Sid
        } else {
            DtxAction::Suppress
        }
    }

    /// Current DTX state.
    pub fn state(&self) -> DtxState {
        self.state
    }

    /// Whether the encoder is currently suppressing voice transmission.
    pub fn is_silence(&self) -> bool {
        self.state == DtxState::Silence
    }

    /// Effective audio bitrate given the current DTX state (bps).
    ///
    /// Returns [`DTX_SILENCE_BPS`] during silence, `voice_bps` during voice
    /// activity or hangover.  The governor uses this for uplink budget
    /// accounting so that silence periods do not over-reserve bandwidth.
    pub fn effective_audio_bps(&self, voice_bps: u32) -> u32 {
        match self.state {
            DtxState::Voice => voice_bps,
            DtxState::Silence => DTX_SILENCE_BPS,
        }
    }

    /// Bitrate saved by DTX compared to continuous voice transmission (bps).
    ///
    /// Zero when voice is active or in hangover.  Positive during silence:
    /// `voice_bps − DTX_SILENCE_BPS`.  The governor may reallocate these
    /// savings to other streams (screen, camera) during a silence period.
    pub fn savings_bps(&self, voice_bps: u32) -> u32 {
        voice_bps.saturating_sub(self.effective_audio_bps(voice_bps))
    }

    /// Number of frames the encoder has been in silence mode (since last voice).
    ///
    /// Returns `0` when in [`DtxState::Voice`].  Used for observability.
    pub fn silence_frame_count(&self) -> usize {
        self.silence_frames
    }
}

impl Default for DtxEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ── DtxReceiver ───────────────────────────────────────────────────────────────

/// Receiver-side DTX comfort-noise manager (Feature 54).
///
/// The receive loop calls [`DtxReceiver::observe_packet`] for every incoming
/// Opus packet, identifying comfort-noise (SID) frames by the CN bit in the
/// Opus TOC byte.  When a 20 ms playout slot has no arriving packet, the loop
/// calls [`DtxReceiver::tick_no_packet`] to distinguish DTX-suppressed slots
/// (which need local comfort-noise synthesis) from genuine packet losses
/// (which the PLC chain handles).
///
/// # Example
///
/// ```rust
/// use lowband_platform::dtx::{DtxReceiver, DtxState};
///
/// let mut rx = DtxReceiver::new();
/// assert_eq!(rx.state(), DtxState::Voice);
///
/// // A SID packet arrives → enter silence; subsequent no-packet slots need CN.
/// rx.observe_packet(true);
/// assert_eq!(rx.state(), DtxState::Silence);
/// assert!(rx.tick_no_packet(), "DTX-suppressed slot must request comfort noise");
///
/// // A normal voice frame arrives → exit silence immediately.
/// rx.observe_packet(false);
/// assert_eq!(rx.state(), DtxState::Voice);
/// assert!(
///     !rx.tick_no_packet(),
///     "missing slot after voice packet is a loss, not DTX"
/// );
/// ```
#[derive(Debug, Clone)]
pub struct DtxReceiver {
    state: DtxState,
}

impl DtxReceiver {
    /// Create a new receiver in [`DtxState::Voice`].
    pub fn new() -> Self {
        Self { state: DtxState::Voice }
    }

    /// Observe an incoming Opus packet and update DTX state.
    ///
    /// `is_sid = true`  → comfort-noise / SID frame: enter [`DtxState::Silence`]
    ///                     and update local CN parameters.
    /// `is_sid = false` → normal voice frame: exit silence, enter
    ///                     [`DtxState::Voice`] immediately.
    pub fn observe_packet(&mut self, is_sid: bool) {
        self.state = if is_sid {
            DtxState::Silence
        } else {
            DtxState::Voice
        };
    }

    /// Call when no Opus packet arrived for a 20 ms playout slot.
    ///
    /// Returns `true` when the receiver is in [`DtxState::Silence`], meaning
    /// the slot was DTX-suppressed by the remote sender and the caller should
    /// synthesise a comfort-noise frame for playout.
    ///
    /// Returns `false` when the receiver is in [`DtxState::Voice`], meaning
    /// the missing slot is a genuine packet loss that the PLC chain should
    /// handle (LBRR FEC, DRED, neural PLC, or comfort-noise fade).
    pub fn tick_no_packet(&self) -> bool {
        self.state == DtxState::Silence
    }

    /// Whether the receiver is currently tracking a DTX silence period.
    pub fn is_in_silence(&self) -> bool {
        self.state == DtxState::Silence
    }

    /// Current DTX state.
    pub fn state(&self) -> DtxState {
        self.state
    }
}

impl Default for DtxReceiver {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ─────────────────────────────────────────────────────────────

    #[test]
    fn dtx_silence_bps_is_100() {
        // (5 bytes × 8 bits × 1000 ms/s) / (20 frames × 20 ms/frame) = 100 bps.
        assert_eq!(DTX_SILENCE_BPS, 100);
    }

    #[test]
    fn silence_bps_is_far_below_voice_floor() {
        // The audio floor is 6 kbps (Feature 48 / gear_policy::AUDIO_FLOOR_BPS).
        // DTX silence must be dramatically lower to constitute near-zero cost.
        const AUDIO_FLOOR_BPS: u32 = 6_000;
        assert!(
            DTX_SILENCE_BPS < AUDIO_FLOOR_BPS / 10,
            "DTX_SILENCE_BPS {DTX_SILENCE_BPS} must be < 10% of audio floor {AUDIO_FLOOR_BPS}"
        );
    }

    #[test]
    fn sid_interval_and_bytes_define_silence_bps() {
        // Verify the constant derivation formula.
        let expected = DTX_SID_BYTES as u32 * 8 * 1_000
            / (DTX_SID_INTERVAL_FRAMES as u32 * 20);
        assert_eq!(
            DTX_SILENCE_BPS, expected,
            "DTX_SILENCE_BPS must equal the SID-packet-rate formula"
        );
    }

    // ── DtxEncoder — initial state ─────────────────────────────────────────────

    #[test]
    fn encoder_starts_in_voice_state() {
        let enc = DtxEncoder::new();
        assert_eq!(enc.state(), DtxState::Voice);
        assert!(!enc.is_silence());
        assert_eq!(enc.silence_frame_count(), 0);
    }

    #[test]
    fn encoder_voice_bps_returned_when_in_voice() {
        let enc = DtxEncoder::new();
        assert_eq!(enc.effective_audio_bps(24_000), 24_000);
        assert_eq!(enc.savings_bps(24_000), 0);
    }

    // ── DtxEncoder — hangover ─────────────────────────────────────────────────

    #[test]
    fn hangover_keeps_voice_action_for_dtx_hangover_frames() {
        let mut enc = DtxEncoder::new();
        for i in 0..DTX_HANGOVER_FRAMES {
            let action = enc.observe_vad(false);
            assert_eq!(
                action,
                DtxAction::Voice,
                "frame {i}: hangover must keep Voice action"
            );
            assert_eq!(enc.state(), DtxState::Voice, "state must remain Voice during hangover");
        }
    }

    #[test]
    fn first_frame_past_hangover_is_sid() {
        let mut enc = DtxEncoder::new();
        // Exhaust the hangover.
        for _ in 0..DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        // The next silent frame is the first in silence mode → SID.
        let action = enc.observe_vad(false);
        assert_eq!(action, DtxAction::Sid, "first frame past hangover must produce a SID");
        assert_eq!(enc.state(), DtxState::Silence);
    }

    #[test]
    fn voice_resets_hangover() {
        let mut enc = DtxEncoder::new();
        // Advance partway through hangover.
        for _ in 0..(DTX_HANGOVER_FRAMES / 2) {
            enc.observe_vad(false);
        }
        // Voice activity resets.
        let action = enc.observe_vad(true);
        assert_eq!(action, DtxAction::Voice);
        assert_eq!(enc.state(), DtxState::Voice);

        // Hangover must restart: DTX_HANGOVER_FRAMES silent frames required again.
        for i in 0..DTX_HANGOVER_FRAMES {
            let action = enc.observe_vad(false);
            assert_eq!(
                action,
                DtxAction::Voice,
                "frame {i}: hangover must restart after voice activity"
            );
        }
    }

    // ── DtxEncoder — silence mode ─────────────────────────────────────────────

    #[test]
    fn silence_frames_between_sids_are_suppress() {
        let mut enc = DtxEncoder::new();
        // Enter silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        // Now in silence; the hangover+1 frame was the first SID.
        // The next DTX_SID_INTERVAL_FRAMES - 1 frames must be Suppress.
        for i in 1..DTX_SID_INTERVAL_FRAMES {
            let action = enc.observe_vad(false);
            assert_eq!(
                action,
                DtxAction::Suppress,
                "silence frame {i} must be Suppress (before next SID interval)"
            );
        }
    }

    #[test]
    fn sid_repeats_every_dtx_sid_interval_frames() {
        let mut enc = DtxEncoder::new();
        // Enter silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        // First SID already emitted. Exhaust one full interval of Suppress frames.
        for _ in 1..DTX_SID_INTERVAL_FRAMES {
            enc.observe_vad(false);
        }
        // The frame at silence_frames == DTX_SID_INTERVAL_FRAMES must be the next SID.
        let action = enc.observe_vad(false);
        assert_eq!(action, DtxAction::Sid, "SID must repeat every DTX_SID_INTERVAL_FRAMES");
    }

    #[test]
    fn sid_count_over_long_silence() {
        let mut enc = DtxEncoder::new();
        // Enter silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        // Drive 5 full SID intervals (5 × 20 = 100 frames) beyond the first SID.
        let expected_sids = 5;
        let mut sid_count = 1usize; // counted the entrance SID above

        for _ in 0..DTX_SID_INTERVAL_FRAMES * expected_sids {
            if enc.observe_vad(false) == DtxAction::Sid {
                sid_count += 1;
            }
        }

        // Should have seen 6 SIDs total: 1 initial + 5 interval SIDs.
        assert_eq!(
            sid_count,
            expected_sids + 1,
            "expected {} SIDs over {} silence frames; got {sid_count}",
            expected_sids + 1,
            DTX_SID_INTERVAL_FRAMES * expected_sids,
        );
    }

    #[test]
    fn voice_in_silence_returns_to_voice_immediately() {
        let mut enc = DtxEncoder::new();
        // Enter silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        assert_eq!(enc.state(), DtxState::Silence);

        // One voice frame → immediate Voice.
        let action = enc.observe_vad(true);
        assert_eq!(action, DtxAction::Voice);
        assert_eq!(enc.state(), DtxState::Voice);
        assert_eq!(enc.silence_frame_count(), 0, "silence counter must reset on voice");
        assert_eq!(enc.effective_audio_bps(24_000), 24_000);
    }

    // ── DtxEncoder — effective_audio_bps ─────────────────────────────────────

    #[test]
    fn effective_bps_is_silence_bps_in_silence_state() {
        let mut enc = DtxEncoder::new();
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        assert_eq!(enc.state(), DtxState::Silence);
        assert_eq!(enc.effective_audio_bps(24_000), DTX_SILENCE_BPS);
        assert_eq!(enc.effective_audio_bps(16_000), DTX_SILENCE_BPS);
    }

    #[test]
    fn savings_bps_equals_voice_minus_silence_bps() {
        let mut enc = DtxEncoder::new();
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        let voice_bps = 24_000u32;
        assert_eq!(enc.savings_bps(voice_bps), voice_bps - DTX_SILENCE_BPS);
    }

    #[test]
    fn savings_bps_zero_in_voice_state() {
        let enc = DtxEncoder::new();
        assert_eq!(enc.savings_bps(24_000), 0);
        assert_eq!(enc.savings_bps(0), 0);
    }

    // ── DtxReceiver — initial state ───────────────────────────────────────────

    #[test]
    fn receiver_starts_in_voice_state() {
        let rx = DtxReceiver::new();
        assert_eq!(rx.state(), DtxState::Voice);
        assert!(!rx.is_in_silence());
    }

    #[test]
    fn no_packet_in_voice_state_is_not_dtx() {
        let rx = DtxReceiver::new();
        assert!(
            !rx.tick_no_packet(),
            "missing packet in Voice state is a loss, not DTX suppression"
        );
    }

    // ── DtxReceiver — observe_packet ──────────────────────────────────────────

    #[test]
    fn sid_packet_enters_silence_state() {
        let mut rx = DtxReceiver::new();
        rx.observe_packet(true);
        assert_eq!(rx.state(), DtxState::Silence);
        assert!(rx.is_in_silence());
    }

    #[test]
    fn voice_packet_exits_silence_state() {
        let mut rx = DtxReceiver::new();
        rx.observe_packet(true); // enter silence
        rx.observe_packet(false); // exit via voice
        assert_eq!(rx.state(), DtxState::Voice);
        assert!(!rx.is_in_silence());
    }

    #[test]
    fn no_packet_in_silence_state_needs_comfort_noise() {
        let mut rx = DtxReceiver::new();
        rx.observe_packet(true);
        assert!(
            rx.tick_no_packet(),
            "missing packet in Silence state must request local comfort-noise synthesis"
        );
    }

    #[test]
    fn no_packet_after_exit_from_silence_is_loss() {
        let mut rx = DtxReceiver::new();
        rx.observe_packet(true); // silence
        rx.observe_packet(false); // voice → exit silence
        assert!(
            !rx.tick_no_packet(),
            "missing packet after returning to Voice is a loss, not DTX"
        );
    }

    // ── Integration: sender/receiver round-trip ───────────────────────────────

    #[test]
    fn sender_sid_frames_receiver_tracks_silence_period() {
        let mut enc = DtxEncoder::new();
        let mut rx = DtxReceiver::new();
        let voice_bps: u32 = 24_000;

        // Silence on the sender side (hang-over + entry).
        for _ in 0..=DTX_HANGOVER_FRAMES {
            let action = enc.observe_vad(false);
            // During hangover, transmit voice frames to receiver.
            if action == DtxAction::Voice {
                rx.observe_packet(false);
            }
        }
        // First silence frame produced DtxAction::Sid — tell receiver.
        rx.observe_packet(true);
        assert_eq!(rx.state(), DtxState::Silence);

        // DTX-suppressed frames: receiver generates CN locally.
        for _ in 1..DTX_SID_INTERVAL_FRAMES {
            let action = enc.observe_vad(false);
            assert_eq!(action, DtxAction::Suppress);
            assert!(
                rx.tick_no_packet(),
                "receiver must synthesise CN for each DTX-suppressed slot"
            );
        }

        // SID update arrives.
        let action = enc.observe_vad(false);
        assert_eq!(action, DtxAction::Sid);
        rx.observe_packet(true);
        assert_eq!(rx.state(), DtxState::Silence, "receiver stays in Silence after SID update");

        // Verify bitrate savings on the sender.
        let savings = enc.savings_bps(voice_bps);
        assert!(
            savings > voice_bps * 99 / 100,
            "DTX must save >99% of voice bitrate during silence; saved {savings} bps of {voice_bps}"
        );
    }

    #[test]
    fn voice_resumes_after_silence_both_sides() {
        let mut enc = DtxEncoder::new();
        let mut rx = DtxReceiver::new();

        // Drive both into silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        rx.observe_packet(true);
        assert_eq!(rx.state(), DtxState::Silence);

        // Voice resumes.
        let action = enc.observe_vad(true);
        assert_eq!(action, DtxAction::Voice);
        assert_eq!(enc.state(), DtxState::Voice);
        rx.observe_packet(false);
        assert_eq!(rx.state(), DtxState::Voice, "receiver must exit silence when voice arrives");
    }

    #[test]
    fn bitrate_accounting_silence_vs_voice_differs_by_savings() {
        let mut enc = DtxEncoder::new();
        let voice_bps = 16_000u32;

        // Voice state.
        assert_eq!(enc.effective_audio_bps(voice_bps), voice_bps);

        // Enter silence.
        for _ in 0..=DTX_HANGOVER_FRAMES {
            enc.observe_vad(false);
        }
        let silence_bps = enc.effective_audio_bps(voice_bps);
        assert_eq!(silence_bps, DTX_SILENCE_BPS);
        assert!(
            voice_bps / silence_bps >= 100,
            "silence bitrate must be ≤1% of voice bitrate; ratio={}/{}",
            voice_bps,
            silence_bps
        );
    }
}

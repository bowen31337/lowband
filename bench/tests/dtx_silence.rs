//! Feature 54 — DTX silence costs near-zero bitrate with comfort_noise updates.
//!
//! # Scenario
//!
//! During a voice call a significant fraction of time is silence (speaker turns,
//! pauses, listening).  Without DTX, the encoder sends a full Opus frame every
//! 20 ms at 16–24 kbps even when there is no speech, wasting uplink bandwidth.
//!
//! With DTX enabled:
//! - The VAD detects silence after a [`DTX_HANGOVER_FRAMES`]-frame hangover.
//! - The encoder emits one SID comfort-noise update every
//!   [`DTX_SID_INTERVAL_FRAMES`] (400 ms); all other slots are suppressed.
//! - The receiver generates comfort noise locally from the most recent SID.
//! - Effective audio bitrate during silence: [`DTX_SILENCE_BPS`] (100 bps).
//!
//! # Test structure
//!
//! **Part A — bitrate savings**: a 10-second call with 50 % silence.  Confirm
//! that the average bitrate is well below the continuous-voice rate and that the
//! effective bitrate during silence is exactly [`DTX_SILENCE_BPS`].
//!
//! **Part B — SID cadence**: drive the encoder through a sustained silence and
//! confirm that exactly one SID packet is produced per [`DTX_SID_INTERVAL_FRAMES`].
//!
//! **Part C — receiver comfort-noise synthesis**: verify the receiver generates
//! CN for each DTX-suppressed slot and never triggers CN for voice slots or
//! after returning to voice.
//!
//! **Part D — hangover correctness**: confirm no silence suppression occurs
//! before the full hangover window expires, preventing premature DTX on brief
//! pauses.

use lowband_platform::{
    DtxAction, DtxEncoder, DtxReceiver, DtxState,
    DTX_HANGOVER_FRAMES, DTX_SID_INTERVAL_FRAMES, DTX_SILENCE_BPS,
};

/// Voice bitrate used in all tests (typical constrained-assist tier).
const VOICE_BPS: u32 = 24_000;

/// Opus frame duration (ms).
const FRAME_MS: usize = 20;

/// Helper: advance `enc` through the hangover and into silence.
/// Returns the encoder having just produced its first `DtxAction::Sid`.
fn enter_silence(enc: &mut DtxEncoder) {
    for _ in 0..DTX_HANGOVER_FRAMES {
        let a = enc.observe_vad(false);
        assert_eq!(a, DtxAction::Voice, "action must be Voice during hangover");
    }
    let a = enc.observe_vad(false);
    assert_eq!(a, DtxAction::Sid, "first frame past hangover must be Sid");
    assert_eq!(enc.state(), DtxState::Silence);
}

// ── Part A: bitrate savings ───────────────────────────────────────────────────

#[test]
fn average_bitrate_over_50_pct_silence_call_is_below_voice_rate() {
    // Simulate a 10-second call (500 frames at 20 ms) with alternating
    // 2-second voice / 2-second silence (100 frames each, 5 cycles).
    let total_frames: usize = 500;
    let segment_frames: usize = 100; // 2 s at 20 ms/frame

    let mut enc = DtxEncoder::new();
    let mut total_bytes: usize = 0;
    let mut in_voice_segment = true;
    let mut segment_frame = 0usize;
    let bytes_per_voice_frame = VOICE_BPS as usize / (8 * 1000 / FRAME_MS); // 60 bytes

    for _ in 0..total_frames {
        // Flip segment.
        if segment_frame == segment_frames {
            in_voice_segment = !in_voice_segment;
            segment_frame = 0;
        }
        let voice_active = in_voice_segment;
        let action = enc.observe_vad(voice_active);
        total_bytes += match action {
            DtxAction::Voice => bytes_per_voice_frame,
            DtxAction::Sid => lowband_platform::DTX_SID_BYTES,
            DtxAction::Suppress => 0,
        };
        segment_frame += 1;
    }

    // Average bitrate = total_bytes × 8 / (total_frames × 20ms / 1000)
    let elapsed_s = total_frames * FRAME_MS; // in ms
    let avg_bps = (total_bytes * 8 * 1_000) / elapsed_s;

    // With 50% silence the average should be well below the voice rate.
    let expected_max_avg = VOICE_BPS * 70 / 100; // 70% of voice rate is a generous ceiling
    assert!(
        avg_bps as u32 <= expected_max_avg,
        "average bitrate with 50% silence must be ≤70% of voice rate; \
         got {avg_bps} bps (voice rate: {VOICE_BPS} bps)"
    );

    eprintln!(
        "dtx_silence — total_frames={total_frames}  avg_bps={avg_bps}  \
         voice_bps={VOICE_BPS}  silence_bps={DTX_SILENCE_BPS}  \
         savings={:.1}%",
        (1.0 - avg_bps as f64 / VOICE_BPS as f64) * 100.0
    );
}

#[test]
fn effective_audio_bps_is_silence_bps_during_silence() {
    let mut enc = DtxEncoder::new();
    enter_silence(&mut enc);
    assert_eq!(
        enc.effective_audio_bps(VOICE_BPS),
        DTX_SILENCE_BPS,
        "effective bitrate must be DTX_SILENCE_BPS during silence"
    );
}

#[test]
fn savings_exceed_99_pct_of_voice_rate_during_silence() {
    let mut enc = DtxEncoder::new();
    enter_silence(&mut enc);
    let saved = enc.savings_bps(VOICE_BPS);
    let pct_saved = saved as f64 / VOICE_BPS as f64 * 100.0;
    assert!(
        pct_saved > 99.0,
        "DTX must save >99% of voice bitrate during silence; saved {pct_saved:.2}% ({saved} bps)"
    );
}

// ── Part B: SID cadence ───────────────────────────────────────────────────────

#[test]
fn sid_count_matches_expected_for_n_silence_intervals() {
    let n_intervals = 10usize;
    let mut enc = DtxEncoder::new();
    enter_silence(&mut enc); // emits the first SID (silence frame 0)

    let mut sid_count = 1usize; // account for the entry SID

    // Drive n_intervals - 1 more full intervals (each interval = 20 frames).
    for _ in 0..((n_intervals - 1) * DTX_SID_INTERVAL_FRAMES) {
        if enc.observe_vad(false) == DtxAction::Sid {
            sid_count += 1;
        }
    }

    assert_eq!(
        sid_count, n_intervals,
        "expected {n_intervals} SIDs over {n_intervals} silence intervals; got {sid_count}"
    );
}

#[test]
fn suppress_count_between_two_sids_equals_interval_minus_one() {
    let mut enc = DtxEncoder::new();
    enter_silence(&mut enc); // first SID at silence frame 0

    let mut suppress_count = 0usize;
    // Advance until the next SID (silence frames 1..DTX_SID_INTERVAL_FRAMES-1 are Suppress).
    for _ in 1..DTX_SID_INTERVAL_FRAMES {
        let action = enc.observe_vad(false);
        if action == DtxAction::Suppress {
            suppress_count += 1;
        }
    }
    let next = enc.observe_vad(false);
    assert_eq!(next, DtxAction::Sid, "next frame must be SID");
    assert_eq!(
        suppress_count,
        DTX_SID_INTERVAL_FRAMES - 1,
        "must have {} Suppress frames between consecutive SIDs; got {suppress_count}",
        DTX_SID_INTERVAL_FRAMES - 1,
    );
}

// ── Part C: receiver comfort-noise synthesis ──────────────────────────────────

#[test]
fn receiver_synthesises_cn_for_every_dtx_suppressed_slot() {
    let mut enc = DtxEncoder::new();
    let mut rx = DtxReceiver::new();

    // Hangover phase: transmit voice frames to receiver.
    for _ in 0..DTX_HANGOVER_FRAMES {
        enc.observe_vad(false);
        rx.observe_packet(false); // voice frame
    }
    // Entry SID.
    enc.observe_vad(false);
    rx.observe_packet(true); // SID → receiver enters silence
    assert_eq!(rx.state(), DtxState::Silence);

    // DTX-suppressed slots: receiver must synthesise CN.
    let mut cn_slots = 0usize;
    let total_suppressed = DTX_SID_INTERVAL_FRAMES - 1;
    for _ in 1..DTX_SID_INTERVAL_FRAMES {
        let action = enc.observe_vad(false);
        assert_eq!(action, DtxAction::Suppress);
        if rx.tick_no_packet() {
            cn_slots += 1;
        }
    }
    assert_eq!(
        cn_slots, total_suppressed,
        "receiver must synthesise CN for every suppressed slot ({total_suppressed}); got {cn_slots}"
    );
}

#[test]
fn receiver_does_not_synthesise_cn_for_voice_slots() {
    let rx = DtxReceiver::new(); // starts in Voice
    assert!(
        !rx.tick_no_packet(),
        "missing slot in Voice state must NOT request CN synthesis (it is a loss)"
    );
}

#[test]
fn receiver_exits_cn_on_voice_packet() {
    let mut rx = DtxReceiver::new();
    rx.observe_packet(true); // SID → silence
    assert!(rx.tick_no_packet(), "in silence: missing slot needs CN");

    rx.observe_packet(false); // voice → exit silence
    assert_eq!(rx.state(), DtxState::Voice);
    assert!(
        !rx.tick_no_packet(),
        "after returning to Voice: missing slot is a loss, not DTX"
    );
}

// ── Part D: hangover correctness ──────────────────────────────────────────────

#[test]
fn no_suppression_before_full_hangover_expires() {
    let mut enc = DtxEncoder::new();
    // Every frame during the hangover must return Voice action.
    for i in 0..DTX_HANGOVER_FRAMES {
        let action = enc.observe_vad(false);
        assert_eq!(
            action,
            DtxAction::Voice,
            "frame {i} (of {DTX_HANGOVER_FRAMES}): hangover must prevent DTX suppression"
        );
        assert_eq!(
            enc.state(),
            DtxState::Voice,
            "state must remain Voice during hangover (frame {i})"
        );
    }
}

#[test]
fn brief_silence_followed_by_voice_never_enters_dtx() {
    let mut enc = DtxEncoder::new();
    // Drive half the hangover then restore voice — must never enter DTX.
    for _ in 0..(DTX_HANGOVER_FRAMES / 2) {
        let a = enc.observe_vad(false);
        assert_eq!(a, DtxAction::Voice);
    }
    // Voice returns before hangover exhausted.
    let a = enc.observe_vad(true);
    assert_eq!(a, DtxAction::Voice);
    assert_eq!(enc.state(), DtxState::Voice);
    // Confirm we still produce Voice for many more frames.
    for _ in 0..50 {
        let a = enc.observe_vad(true);
        assert_eq!(a, DtxAction::Voice);
        assert_eq!(enc.state(), DtxState::Voice);
    }
}

#[test]
fn voice_in_middle_of_hangover_restarts_full_hangover_on_next_silence() {
    let mut enc = DtxEncoder::new();
    // Advance partway through hangover.
    for _ in 0..(DTX_HANGOVER_FRAMES / 2) {
        enc.observe_vad(false);
    }
    enc.observe_vad(true); // voice resets hangover

    // Now silence again: must wait the full hangover before entering DTX.
    for i in 0..DTX_HANGOVER_FRAMES {
        let a = enc.observe_vad(false);
        assert_eq!(
            a,
            DtxAction::Voice,
            "restarted hangover frame {i}: must stay in Voice"
        );
    }
    // Past the new hangover → enters silence.
    let a = enc.observe_vad(false);
    assert_eq!(a, DtxAction::Sid, "must enter DTX silence after restarted hangover");
}

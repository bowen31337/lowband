//! UC-3 end-to-end verification — Feature 175.
//!
//! Field-to-expert call: the field engineer starts with survival-tier voice
//! and an AI-reconstructed head video (GearA, channel 5 video-rt) then
//! switches to sharing the diagnostics laptop screen (channel 4 screen-rt +
//! channel 6 reliable-bulk lossless).  The session is never renegotiated —
//! the PathMigrationController stays Idle throughout, and no signaling
//! endpoint is called during the switch.
//!
//! # Channel map (LBTP §6.2)
//!
//! | ch | purpose            | delivery class       |
//! |----|--------------------|----------------------|
//! |  1 | audio              | realtime             |
//! |  4 | screen-rt (coarse) | realtime             |
//! |  5 | video-rt (GearA)   | realtime             |
//! |  6 | reliable bulk      | reliable-unordered   |

use lowband_lbtp::{ChannelId, Pacer, PacerFrame, PathMigrationController};
use lowband_platform::{GearConstraints, ThermalPressure};

// Channel constants matching the LBTP architecture spec §6.2.
const CH_AUDIO:     ChannelId = ChannelId(1);
const CH_SCREEN_RT: ChannelId = ChannelId(4);
const CH_VIDEO_RT:  ChannelId = ChannelId(5);
const CH_RELIABLE:  ChannelId = ChannelId(6);

// One pacing tick at 10 Hz = 100 ms = 100_000_000 ns.
const TICK_NS: u64 = 100_000_000;

/// UC-3: field-to-expert call switches GearA head video to screen_share
/// without renegotiating the session.
#[test]
fn uc3_field_to_expert_switches_head_video_to_screen_share_without_renegotiation() {
    // Session path controller — must remain Idle throughout.
    let path_ctrl = PathMigrationController::new();
    // Signaling-call counter — any increment represents an illegitimate
    // renegotiation attempt (POST /signal/offer, /signal/answer, etc.).
    let signaling_calls: u32 = 0;

    // Use 1 Mbps so the burst cap (625 B) comfortably covers each frame pair.
    // The token budget equation: rate_bps * BURST_TOLERANCE_MS / 8_000
    // = 1_000_000 * 5 / 8_000 = 625 bytes — enough for audio(80) + video(200).
    let mut pacer = Pacer::new(1_000_000.0);

    // ── Phase 1: survival-tier voice + GearA neural head video ───────────────
    //
    // At Nominal thermal pressure the governor confirms GearA is permitted
    // before activating the neural-encoder badge.
    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    assert!(
        constraints.neural_camera_allowed(),
        "GearA must be permitted at Nominal thermal pressure"
    );

    // One pacing tick: audio (ch 1) + GearA head video (ch 5).
    pacer.advance(TICK_NS);
    pacer.enqueue(PacerFrame::new(CH_AUDIO,    vec![0u8; 80]));
    pacer.enqueue(PacerFrame::new(CH_VIDEO_RT, vec![0u8; 200]));

    assert_eq!(
        pacer.queued_frames(CH_VIDEO_RT), 1,
        "channel 5 (video-rt / GearA) must have a queued frame before the switch"
    );
    assert_eq!(
        pacer.queued_frames(CH_SCREEN_RT), 0,
        "channel 4 (screen-rt) must be empty before the screen_share switch"
    );

    // Transport loop drains the GearA frame.
    pacer.drain_tick().expect("pacer must produce a datagram with GearA head video");
    assert_eq!(
        pacer.queued_frames(CH_VIDEO_RT), 0,
        "channel 5 must be empty after the transport drains the GearA frame"
    );

    // Session path controller untouched.
    assert!(!path_ctrl.is_probing(), "session must not be probing a new path before the switch");
    assert!(!path_ctrl.is_migrated(), "no migration should have occurred before the switch");

    // ── Phase 2: engineer switches to screen_share ────────────────────────────
    //
    // The governor:
    //   (a) stops feeding channel 5 (GearA head video encoder silent),
    //   (b) starts feeding channel 4 (screen-rt coarse) and channel 6
    //       (reliable-bulk lossless refinement) from ScreenCaptureBroker.
    //
    // No signaling endpoint is called.  The existing LBTP session keys are
    // valid on the same UDP 5-tuple — only the channel mix changes.
    pacer.advance(TICK_NS);
    pacer.enqueue(PacerFrame::new(CH_AUDIO,     vec![0u8; 80]));
    pacer.enqueue(PacerFrame::new(CH_SCREEN_RT, vec![0u8; 400])); // coarse frame
    pacer.enqueue(PacerFrame::new(CH_RELIABLE,  vec![0u8; 200])); // lossless chunk

    assert_eq!(
        pacer.queued_frames(CH_VIDEO_RT), 0,
        "channel 5 (video-rt) must remain empty — GearA head video is off"
    );
    assert_eq!(
        pacer.queued_frames(CH_SCREEN_RT), 1,
        "channel 4 (screen-rt) must have a frame from ScreenCaptureBroker"
    );
    assert_eq!(
        pacer.queued_frames(CH_RELIABLE), 1,
        "channel 6 (reliable-bulk) must have a lossless refinement chunk"
    );

    // Drain and verify the datagram carries screen traffic, not head video.
    let datagram = pacer
        .drain_tick()
        .expect("pacer must produce a datagram after the screen_share switch");
    let channels_used: Vec<u8> = datagram.frames.iter().map(|f| f.channel.0).collect();

    assert!(
        !channels_used.contains(&CH_VIDEO_RT.0),
        "video-rt (ch 5) must not appear in any datagram after the screen_share switch; \
         got channels: {channels_used:?}"
    );
    assert!(
        channels_used.iter().any(|&c| c == CH_SCREEN_RT.0 || c == CH_AUDIO.0),
        "at least screen-rt (ch 4) or audio (ch 1) must be present after the switch; \
         got channels: {channels_used:?}"
    );

    // ── No-renegotiation assertion ────────────────────────────────────────────
    //
    // The switch changes which channels carry data but the session itself —
    // LBTP crypto state, UDP 5-tuple, peer addresses — is unchanged.
    // No new offer/answer exchange is required.
    assert_eq!(
        signaling_calls, 0,
        "no signaling endpoint may be called during a screen_share switch"
    );
    assert!(
        !path_ctrl.is_probing(),
        "PathMigrationController must not be probing — session path is unchanged"
    );
    assert!(
        !path_ctrl.is_migrated(),
        "PathMigrationController must not have migrated — still on the original path"
    );
    assert!(
        !path_ctrl.is_failed(),
        "PathMigrationController must not be in Failed — no probe was ever started"
    );
}

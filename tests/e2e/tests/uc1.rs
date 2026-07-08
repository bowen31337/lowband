//! UC-1 end-to-end verification — Feature 173.
//!
//! A technician (Tan) fixes a remote config over an emulated 3G link, then
//! pushes a 40 MB installer to Ana's machine.  Mid-transfer, Ana's dongle
//! re-attaches on a new IP address; the session migrates via
//! path_challenge/response and the transfer completes without losing a byte.
//!
//! # Playwright-style scenario
//!
//! | Step | Action | Key observable |
//! |------|--------|----------------|
//! | 1 | Bimodal 3G OWD pattern observed | `cellular_mode` activates after `CELLULAR_ENTRY_TICKS` |
//! | 2 | γ widened to resist RAN-scheduler spikes | `gamma_multiplier() == CELLULAR_GAMMA_MULTIPLIER` (2×) |
//! | 3 | Tan fixes config: audio + cursor + screen-rt over 200 kbps | `audio_gap_ticks == 0`; cursor drains every tick |
//! | 4 | Installer push begins; first 20 MB pre-migration | Xfer held when voice pending; headroom gate respected |
//! | 5 | Ana's dongle re-attaches on new IP | `PathMigrationController` → `Migrated` |
//! | 6 | Remaining 20 MB transfers post-migration | All 40 MB delivered; scheduler queue empty |
//!
//! # 3G link parameters
//!
//! 200 kbps upload (EDGE/HSPA minimum for a functional session).
//! Pacing tick: 5 ms → token budget = 200 000 bps / 8 / 200 ticks/s = 125 B/tick.
//! Governor interval: 100 ms (10 Hz).
//!
//! # Cellular-mode detection (Feature 14)
//!
//! The BimodalDetector needs OWD_WINDOW_SIZE (30) samples with a spike fraction
//! in [10 %, 45 %] and a spread ≥ 15 ms.  We use a 20 % spike rate
//! (BASE = 80 ms, SPIKE = 250 ms — same values as the unit tests in
//! cellular.rs).  200 warm-up samples, then CELLULAR_ENTRY_TICKS (20) ticks.
//!
//! # Installer size math
//!
//! INSTALLER_BYTES = 40 × 1 024 × 1 024 = 41 943 040 bytes.
//! Frame size: 1 024 bytes ≤ MAX_DATAGRAM_XFER_BYTES (1 181).
//! Frame count: 41 943 040 / 1 024 = 40 960 frames (exact).
//! Headroom per half: INSTALLER_BYTES / 2 = 20 971 520 bytes.
//! Each frame occupies one datagram (slot = 1 026 B; two slots = 2 052 > 1 181).
//! Ticks per half: 20 480 tick() calls drain 20 480 frames × 1 024 B = 20 971 520 B exactly.
//!
//! # IP migration (Feature 12)
//!
//! Ana's dongle re-attaches mid-transfer (after half the frames are sent).
//! PathMigrationController issues a PATH_CHALLENGE; the peer echoes it in the
//! same control tick (zero-latency model, correct for a unit-level e2e test
//! that doesn't simulate network RTT).  The xfer scheduler's VecDeque is
//! unchanged by the address switch — no frames are lost or re-queued.

use lowband_lbtp::{
    CellularModeController, ChannelId, MigrationEvent, Pacer, PacerFrame,
    PathMigrationController, PathResponseFrame,
    CELLULAR_ENTRY_TICKS, CELLULAR_GAMMA_MULTIPLIER,
};
use lowband_platform::{allocate, GearConstraints, ThermalPressure, AUDIO_FLOOR_BPS};
use lowband_xfer::{BulkTransferScheduler, PacerDemand, TickResult, XferFrame};

// Channel constants (LBTP §6.2).
const CH_AUDIO:     ChannelId = ChannelId(1);
const CH_CURSOR:    ChannelId = ChannelId(2);
const CH_SCREEN_RT: ChannelId = ChannelId(4);

// 3G uplink parameters.
const G3_BPS:         f64 = 200_000.0;
const TICK_NS:        u64 = 5_000_000;  // 5 ms
const TICKS_PER_SEC:  u64 = 200;        // 1_000_000_000 / TICK_NS

// Bimodal OWD emulation: 80 ms baseline, 250 ms RAN spike (> 1.5 × base).
const BASE_OWD_US:  u32 = 80_000;
const SPIKE_OWD_US: u32 = 250_000;

// Installer: 40 MB in 1 024-byte frames (exact division).
const INSTALLER_BYTES:  usize = 40 * 1024 * 1024;      // 41 943 040
const FRAME_DATA_BYTES: usize = 1_024;
const N_FRAMES:         usize = INSTALLER_BYTES / FRAME_DATA_BYTES; // 40 960

// Headroom per governor half-interval: drains exactly half the installer.
const HEADROOM_PER_HALF: usize = INSTALLER_BYTES / 2;  // 20 971 520

// IP-migration challenge token (fixed for determinism).
const MIGRATION_TOKEN: [u8; 8] = [0x3A, 0x1B, 0x4C, 0xAA, 0x5D, 0x0F, 0x6E, 0x71];

fn bytes_per_tick(bps: u32) -> usize {
    (bps as u64 / 8 / TICKS_PER_SEC) as usize
}

/// UC-1: technician fixes config over 3G and pushes a 40 MB installer that
/// survives an IP migration mid-transfer.
#[test]
fn uc1_technician_fixes_config_over_3g_and_pushes_40mb_installer_with_ip_migration() {
    // ── Phase 1: Cellular 3G detection ───────────────────────────────────────
    //
    // Feed a 20% bimodal spike pattern to the CellularModeController.
    // After CELLULAR_ENTRY_TICKS (20) consecutive bimodal ticks the controller
    // enters cellular mode and widens γ to 2×.  This prevents the
    // delay-gradient congestion controller from misreading RAN scheduling
    // quanta as overuse and cutting the rate needlessly.

    let mut cellular = CellularModeController::new();

    for _ in 0..200 {
        for _ in 0..4 {
            cellular.observe_owd(BASE_OWD_US);
        }
        cellular.observe_owd(SPIKE_OWD_US); // 20% spike rate — bimodal signature
    }
    for _ in 0..CELLULAR_ENTRY_TICKS {
        cellular.tick();
    }

    assert!(
        cellular.is_active(),
        "cellular_mode must activate on a 3G bimodal OWD pattern (20% spike rate, \
         spread {BASE_OWD_US}–{SPIKE_OWD_US} µs)"
    );
    assert!(
        (cellular.gamma_multiplier() - CELLULAR_GAMMA_MULTIPLIER).abs() < f64::EPSILON,
        "γ must be widened to {CELLULAR_GAMMA_MULTIPLIER}× in cellular mode; \
         got {:.2}",
        cellular.gamma_multiplier()
    );
    assert!(
        !cellular.can_increase(0.5),
        "rate increase must be gated when OWD trend is positive in cellular mode"
    );
    assert!(
        cellular.can_increase(-0.1),
        "rate increase must be permitted when OWD trend is non-positive (queue draining)"
    );

    // ── Phase 2: Config fix — voice + cursor + screen over 200 kbps 3G ───────
    //
    // Tan edits the config file on Ana's machine via remote control.
    // Audio channel (ch 1) carries voice; cursor (ch 2) carries pointer deltas;
    // screen-rt (ch 4) delivers pixel-crisp screen text.
    //
    // allocate(200_000, Nominal) distributes:
    //   audio    → 24 kbps (15 B/tick)
    //   input    →  8 kbps (cursor carries 4 B per tick)
    //   screen   → 20 kbps (12 B/tick)
    //   camera   → remaining 148 kbps (not relevant here)
    //   xfer     → 0 kbps (fully consumed by media)
    //
    // Per tick budget: 200 kbps / 8 / 200 Hz = 125 B.
    // Media demand: 15 + 4 + 12 = 31 B << 125 B → every frame drains same tick.

    let constraints = GearConstraints::from_thermal(ThermalPressure::Nominal);
    let budgets = allocate(200_000, &constraints);

    assert!(
        budgets.audio_bps >= AUDIO_FLOOR_BPS,
        "voice floor must be honoured at 200 kbps 3G: got {} bps, floor {} bps",
        budgets.audio_bps,
        AUDIO_FLOOR_BPS
    );

    let audio_bytes  = bytes_per_tick(budgets.audio_bps).max(1);
    let screen_bytes = bytes_per_tick(budgets.screen_coarse_bps).max(1);
    const CURSOR_BYTES: usize = 4; // compact pointer-delta event

    let mut pacer = Pacer::new(G3_BPS);
    let mut audio_gap_ticks = 0u32;

    // 40 × 5 ms ticks = 200 ms of remote config editing.
    for _ in 0..40 {
        pacer.enqueue(PacerFrame::new(CH_AUDIO,     vec![0u8; audio_bytes]));
        pacer.enqueue(PacerFrame::new(CH_CURSOR,    vec![0u8; CURSOR_BYTES]));
        pacer.enqueue(PacerFrame::new(CH_SCREEN_RT, vec![0u8; screen_bytes]));
        pacer.advance(TICK_NS);
        while pacer.dequeue().is_some() {}

        if pacer.queued_frames(CH_AUDIO) > 0 {
            audio_gap_ticks += 1;
        }
    }

    assert_eq!(
        audio_gap_ticks, 0,
        "voice must not gap during config fix over 3G: \
         {audio_gap_ticks} audio_gap tick(s) detected in 40 ticks"
    );
    assert_eq!(
        pacer.queued_frames(CH_CURSOR), 0,
        "cursor must drain every tick so the remote pointer stays fluid"
    );
    assert_eq!(
        pacer.queued_frames(CH_SCREEN_RT), 0,
        "screen-rt must drain every tick — pixel-crisp text on 3G"
    );

    // ── Phase 3: Installer push — first 20 MB pre-migration ──────────────────
    //
    // The BulkTransferScheduler enforces two pacing gates:
    //
    //   Feature 111 (priority gate): xfer is held while voice or input bytes
    //   are pending in the pacer.  Even one byte of pending audio blocks all
    //   xfer datagrams in that tick.
    //
    //   Feature 110 (headroom gate): xfer may only use the byte budget granted
    //   by the governor.  Once exhausted, xfer waits for the next interval.
    //
    // We verify the priority gate with a single held tick, then drain the
    // first 20 MB (20 480 frames) in a tight loop under full headroom.

    let mut scheduler = BulkTransferScheduler::new();

    for esi in 0u32..(N_FRAMES as u32) {
        scheduler.enqueue(XferFrame::new(vec![0u8; FRAME_DATA_BYTES], 1, esi));
    }
    assert_eq!(
        scheduler.queued_frames(), N_FRAMES,
        "all {N_FRAMES} installer frames ({} MB) must be queued before transfer",
        INSTALLER_BYTES / (1024 * 1024)
    );

    // Verify priority gate: voice pending → xfer held; headroom unchanged.
    scheduler.set_headroom(HEADROOM_PER_HALF);
    let priority_held = scheduler.tick(PacerDemand { voice_pending: 1_200, input_pending: 0 });
    assert!(
        matches!(priority_held, TickResult::HeldForPriority),
        "xfer must be held while voice bytes are pending (Feature 111)"
    );
    assert_eq!(
        scheduler.headroom_remaining(), HEADROOM_PER_HALF,
        "headroom must not decrease while xfer is held for priority"
    );
    assert_eq!(
        scheduler.queued_frames(), N_FRAMES,
        "no frames must be consumed during a priority hold"
    );

    // Drain first half: 20 480 ticks × 1 024 B/frame = 20 971 520 B = 20 MB.
    let mut bytes_sent: usize = 0;
    loop {
        match scheduler.tick(PacerDemand::default()) {
            TickResult::SendAggregated(agg) => bytes_sent += agg.data_bytes(),
            TickResult::HeldForHeadroom | TickResult::Idle => break,
            TickResult::HeldForPriority => panic!(
                "unexpected HeldForPriority during installer push with no pacer demand"
            ),
        }
    }

    assert_eq!(
        bytes_sent, HEADROOM_PER_HALF,
        "exactly 20 MB (first governor headroom interval) should drain pre-migration; \
         got {bytes_sent} bytes"
    );
    assert_eq!(
        scheduler.queued_frames(),
        N_FRAMES / 2,
        "half the installer frames must remain in queue before IP migration"
    );

    // ── Phase 4: IP migration — Ana's dongle re-attaches on a new IP ─────────
    //
    // The transport detects a local-address change (dongle re-attach) and calls
    // PathMigrationController::start() with a freshly generated token.  The
    // challenge is sent to the candidate remote address on the new UDP 5-tuple.
    //
    // When the peer echoes the token in a PATH_RESPONSE the controller commits
    // to the new path.  The existing session keys remain valid — no renegotiation,
    // no new Noise-IK handshake.  The xfer scheduler's VecDeque is untouched;
    // it resumes on the next governor interval exactly where it paused.

    let mut path_ctrl = PathMigrationController::new();
    let challenge = path_ctrl.start(MIGRATION_TOKEN);
    assert_eq!(
        challenge.token, MIGRATION_TOKEN,
        "PATH_CHALLENGE must carry the migration token verbatim"
    );
    assert!(path_ctrl.is_probing(), "controller must be Probing after start()");

    // Peer echoes the token — migration completes.
    let migration_result = path_ctrl.on_response(&PathResponseFrame { token: MIGRATION_TOKEN });
    assert_eq!(
        migration_result,
        Some(MigrationEvent::Migrated),
        "migration must complete when the peer echoes the correct token"
    );
    assert!(path_ctrl.is_migrated(),  "controller must be Migrated after on_response");
    assert!(!path_ctrl.is_probing(), "probe must be inactive after successful migration");
    assert!(!path_ctrl.is_failed(),  "migration must not be marked Failed");

    // xfer queue is unaffected by the address change.
    assert_eq!(
        scheduler.queued_frames(),
        N_FRAMES / 2,
        "ip_migration must not discard or duplicate queued installer frames"
    );

    // ── Phase 5: Installer push — remaining 20 MB post-migration ─────────────
    //
    // The governor grants a fresh headroom budget on the new path.  The
    // scheduler resumes from the frame at the head of the queue — no rewind,
    // no retransmission at the session layer.

    scheduler.set_headroom(HEADROOM_PER_HALF);
    loop {
        match scheduler.tick(PacerDemand::default()) {
            TickResult::SendAggregated(agg) => bytes_sent += agg.data_bytes(),
            TickResult::HeldForHeadroom | TickResult::Idle => break,
            TickResult::HeldForPriority => panic!(
                "unexpected HeldForPriority during post-migration installer push"
            ),
        }
    }

    // ── Final assertions ──────────────────────────────────────────────────────

    eprintln!(
        "UC-1 result: audio_gap_ticks={audio_gap_ticks} [limit: 0], \
         installer_bytes_delivered={bytes_sent} [target: {INSTALLER_BYTES}], \
         frames_sent={} [target: {N_FRAMES}], \
         migration=Migrated, cellular_mode={}",
        bytes_sent / FRAME_DATA_BYTES,
        cellular.is_active(),
    );

    assert_eq!(
        bytes_sent, INSTALLER_BYTES,
        "all 40 MB of the installer must be delivered across the IP migration; \
         delivered {bytes_sent} / {INSTALLER_BYTES} bytes"
    );
    assert_eq!(
        scheduler.queued_frames(), 0,
        "xfer scheduler must be empty after the full installer transfer"
    );
    assert!(
        path_ctrl.is_migrated(),
        "session must remain on the migrated path at transfer completion"
    );
}

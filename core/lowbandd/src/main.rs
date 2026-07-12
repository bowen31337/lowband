//! `lowbandd` — LowBand core daemon.
//!
//! Runs as a least-privilege system service (macOS: `_lowband` via LaunchDaemon
//! `UserName`; Windows: `NT SERVICE\LowBandDaemon` virtual account; Linux:
//! `--drop-privs-to _lowband` after IPC socket bind).  The UI shell connects
//! over the IPC socket and receives push events from the governor loop.
//!
//! # Privilege model
//!
//! The daemon holds device-capture entitlements (screen, mic, camera, input
//! injection) but deliberately drops every other right at startup.  The UI shell
//! runs under the interactive user account with only `network.client` — it never
//! touches hardware directly.  Communication is exclusively over the IPC socket.
//!
//! # Governor loop
//!
//! Runs at 10 Hz.  Each tick samples thermal pressure and CPU usage, derives the
//! session tier and encoder gear constraints, allocates per-stream bandwidths, and
//! broadcasts three events to every connected UI shell:
//! [`TierUpdate`](lowband_platform::ipc::IpcEvent::TierUpdate),
//! [`StreamBudget`](lowband_platform::ipc::IpcEvent::StreamBudget), and
//! [`GearUpdate`](lowband_platform::ipc::IpcEvent::GearUpdate).

mod adpcm;
mod ai_label;
mod dataplane;
mod file_transfer;
mod inbound;
// Mesh group calls (FR-14): the daemon-side full-mesh fan-out over a signaling
// room roster; always compiled (establishment + budget + mixer are pure Rust).
mod mesh;
mod quality_indicator;
// Verification-only harness (NFR-4 OCR gate); compiled for tests, like the
// bench gates.
#[cfg(test)]
mod ocr;
// Production voice codec (system libopus); the interim ADPCM codec is used
// when this feature is off.
#[cfg(feature = "opus")]
mod opus_codec;
// Production AV1 camera codec (rav1e encode / dav1d decode); the interim
// block-DCT codec is used when these features are off.
#[cfg(feature = "av1-encode")]
mod av1_codec;
// Mic/speaker device I/O plumbing (device-independent parts always compiled;
// the cpal device code is behind the `audio` feature).
mod audio_io;
// Full-duplex voice loop (mic ↔ speaker over the E2EE session), behind `audio`.
#[cfg(feature = "audio")]
mod voice_loop;
// Full-duplex mesh group voice loop (multi-peer mix), behind `audio`.
#[cfg(feature = "audio")]
mod mesh_voice;
// ONNX neural-inference runtime (pure-Rust tract), behind the `onnx` feature.
#[cfg(feature = "onnx")]
mod neural;
// Trained neural voice gear (PCA autoencoder → ONNX → tract runtime).
#[cfg(feature = "onnx")]
mod neural_codec;
// Neural training pipeline: backprop-trained nonlinear MLP autoencoder.
#[cfg(feature = "onnx")]
mod neural_train;
// AI head-video gear: keypoints → neural synthesis → AI-labeled frame.
#[cfg(feature = "onnx")]
mod neural_head;
mod picture;
// Verification-only quality gates (SSIM / segmental SNR); compiled for tests.
#[cfg(test)]
mod quality;
// Real branded VMAF via the vmaf CLI subprocess; verification harness, tests
// only (the `vmaf` CI job builds the tool; locally it skips).
#[cfg(test)]
mod vmaf_cli;
mod screen_transfer;
mod session;
mod stun;
mod voice;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lowband_platform::ipc::{IpcEvent, IpcServer};
use lowband_platform::{
    allocate, CpuCeiling, GearConstraints, ThermalMonitor, ThermalPressure, ThrottleAction,
    TierState,
};

/// How the daemon establishes its peer session at startup.
#[derive(Clone)]
enum SessionMode {
    /// Governor-only; no peer session (default, backward compatible).
    None,
    /// Create a code and wait for a peer (technician side).
    Host { signaling: String },
    /// Join an existing code (assisted side).
    Join { signaling: String, code: String },
    /// Create a mesh room and host a group call of up to `size` participants.
    MeshHost { signaling: String, id: String, size: usize },
    /// Join an existing mesh room by code.
    MeshJoin { signaling: String, code: String, id: String, size: usize },
}

// ── Shutdown flag ─────────────────────────────────────────────────────────────

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

// SAFETY: only performs an atomic store — async-signal-safe per POSIX.
#[cfg(unix)]
extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

#[cfg(unix)]
fn install_signal_handlers() {
    extern "C" {
        // Returns the previous handler; we ignore it.
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    unsafe {
        signal(2, on_signal);  // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Config {
    ipc_socket: PathBuf,
    data_dir: PathBuf,
    /// Starting link bandwidth estimate (bps).  The network governor will refine
    /// this at runtime; 400 kbps is a conservative initial allocation.
    link_bps: u32,
    /// Linux only: POSIX user name to drop privileges to after socket bind.
    #[cfg(target_os = "linux")]
    drop_to: Option<String>,
    /// How to establish the peer session at startup.
    session_mode: SessionMode,
    /// Optional STUN server for server-reflexive candidate gathering.
    stun_server: Option<std::net::SocketAddr>,
}

/// Default mesh party size when `--room-size` is omitted (the FR-14 max).
fn mesh_default_size() -> usize {
    lowband_signaling::MESH_MAX_PARTICIPANTS
}

/// A participant id unique to this process when `--room-id` is omitted.
fn default_participant_id() -> String {
    format!("peer-{}", std::process::id())
}

fn parse_args() -> Config {
    let mut ipc_socket = PathBuf::from("/tmp/lowband.sock");
    let mut data_dir = PathBuf::from("/var/lib/lowband");
    let mut link_bps: u32 = 400_000;
    #[cfg(target_os = "linux")]
    let mut drop_to: Option<String> = None;
    let mut signaling: Option<String> = None;
    let mut host = false;
    let mut join_code: Option<String> = None;
    let mut stun_server: Option<std::net::SocketAddr> = None;
    // Mesh group call (FR-14): `--room` hosts, `--room-join <code>` joins;
    // `--room-id` names this participant, `--room-size` bounds the party.
    let mut room_host = false;
    let mut room_join: Option<String> = None;
    let mut room_id: Option<String> = None;
    let mut room_size: usize = mesh_default_size();

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--ipc-socket" => {
                if let Some(v) = args.next() {
                    ipc_socket = PathBuf::from(v);
                }
            }
            "--data-dir" => {
                if let Some(v) = args.next() {
                    data_dir = PathBuf::from(v);
                }
            }
            "--link-bps" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<u32>() {
                        link_bps = n;
                    }
                }
            }
            #[cfg(target_os = "linux")]
            "--drop-privs-to" => {
                if let Some(v) = args.next() {
                    drop_to = Some(v);
                }
            }
            // `--signaling <addr>` names the rendezvous server; `--host` creates
            // a code, `--join <code>` enters one. Absent these, the daemon runs
            // the governor only (unchanged default).
            "--signaling" => signaling = args.next(),
            "--host" => host = true,
            "--join" => join_code = args.next(),
            "--stun" => stun_server = args.next().and_then(|s| s.parse().ok()),
            "--room" => room_host = true,
            "--room-join" => room_join = args.next(),
            "--room-id" => room_id = args.next(),
            "--room-size" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        room_size = n.clamp(2, lowband_signaling::MESH_MAX_PARTICIPANTS);
                    }
                }
            }
            _ => {}
        }
    }

    // Mesh modes take precedence when a room flag is present; otherwise fall
    // back to the 1:1 host/join selection (unchanged default when neither set).
    let session_mode = match (signaling, host, join_code, room_host, room_join) {
        (Some(sig), _, _, _, Some(code)) => SessionMode::MeshJoin {
            signaling: sig,
            code,
            id: room_id.unwrap_or_else(default_participant_id),
            size: room_size,
        },
        (Some(sig), _, _, true, None) => SessionMode::MeshHost {
            signaling: sig,
            id: room_id.unwrap_or_else(default_participant_id),
            size: room_size,
        },
        (Some(sig), _, Some(code), _, _) => SessionMode::Join { signaling: sig, code },
        (Some(sig), true, None, _, _) => SessionMode::Host { signaling: sig },
        _ => SessionMode::None,
    };

    Config {
        ipc_socket,
        data_dir,
        link_bps,
        #[cfg(target_os = "linux")]
        drop_to,
        session_mode,
        stun_server,
    }
}

/// Run the unified inbound router on an established session, in the
/// background, until shutdown. Received control messages and file-transfer
/// progress are logged; inbound files land under `data_dir/inbox.*`.
/// (Superseded by the full-duplex voice loop under `--features audio`.)
#[cfg_attr(feature = "audio", allow(dead_code))]
fn spawn_session_worker(mut session: lowband_crypto::SecureSession, data_dir: PathBuf) {
    use lowband_messaging::clipboard::ClipboardGrant;

    thread::spawn(move || {
        // A short read timeout keeps the loop responsive to shutdown; recv
        // returns a transient session error on timeout, which we ignore.
        let _ = session.set_read_timeout(Some(Duration::from_secs(1)));
        let inbox = data_dir.join("inbox.bin");
        let resume = data_dir.join("inbox.resume");
        let mut router =
            inbound::InboundRouter::new(file_transfer::FileReceiver::new(inbox, resume));
        // The daemon accepts clipboard content by default for this session;
        // scoped consent toggling arrives with the consent-UX wiring.
        router.clipboard.set_grant(Some(ClipboardGrant::new()));

        while !SHUTDOWN.load(Ordering::Relaxed) {
            match router.recv_and_handle(&mut session) {
                Ok(handled) => eprintln!("lowbandd: inbound {handled:?}"),
                // Timeout / transient socket error — poll again.
                Err(file_transfer::XferError::Session(_)) => continue,
                Err(e) => eprintln!("lowbandd: inbound frame error: {e}"),
            }
        }
    });
}

/// Establish the peer session (blocking) before the governor loop starts.
/// Logs the secure-channel outcome; media plumbing over the channel is a
/// later milestone, so the session is held for the process lifetime.
fn establish_peer_session(
    mode: &SessionMode,
    stun_server: Option<std::net::SocketAddr>,
) -> Option<lowband_crypto::SecureSession> {
    use lowband_crypto::StaticKeypair;
    use lowband_signaling::SignalingClient;

    let timeout = Duration::from_secs(30);
    let static_key = StaticKeypair::generate();

    let (signaling, is_host, code) = match mode {
        SessionMode::None => return None,
        SessionMode::Host { signaling } => (signaling.clone(), true, None),
        SessionMode::Join { signaling, code } => (signaling.clone(), false, Some(code.clone())),
        // Mesh modes are established by `establish_mesh_session`, not here.
        SessionMode::MeshHost { .. } | SessionMode::MeshJoin { .. } => return None,
    };

    let host_header = signaling.clone();
    let client = match SignalingClient::connect(&signaling, host_header) {
        Ok(c) => c.with_timeout(timeout),
        Err(e) => {
            eprintln!("lowbandd: signaling connect failed: {e}");
            return None;
        }
    };

    let result = if is_host {
        session::establish_host(&client, &static_key, timeout, stun_server, |code| {
            eprintln!("lowbandd: hosting session — join code: {code}");
        })
        .map(|(_code, s)| s)
    } else {
        session::establish_join(
            &client,
            code.as_deref().unwrap_or(""),
            &static_key,
            timeout,
            stun_server,
        )
    };

    match result {
        Ok(s) => {
            let peer = s.remote_static_pubkey();
            let peer_hex: String = peer.iter().take(4).map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "lowbandd: secure channel established (initiator={}, peer_key={peer_hex}…)",
                s.is_initiator()
            );
            Some(s)
        }
        Err(e) => {
            eprintln!("lowbandd: session establishment failed: {e}");
            None
        }
    }
}

/// Establish a mesh group call (FR-14) if a room mode was selected: create or
/// join the signaling room, then fan out a full mesh of E2EE sessions to every
/// other participant. Returns this node's [`mesh::MeshSession`] on success.
fn establish_mesh_session(
    mode: &SessionMode,
    stun_server: Option<std::net::SocketAddr>,
) -> Option<mesh::MeshSession> {
    use lowband_crypto::StaticKeypair;
    use lowband_signaling::SignalingClient;

    let _ = stun_server; // reflexive mesh candidates: reuses --stun once ICE lands.
    let timeout = Duration::from_secs(30);

    let (signaling, is_host, code, id, size) = match mode {
        SessionMode::MeshHost { signaling, id, size } => {
            (signaling.clone(), true, None, id.clone(), *size)
        }
        SessionMode::MeshJoin { signaling, code, id, size } => {
            (signaling.clone(), false, Some(code.clone()), id.clone(), *size)
        }
        _ => return None,
    };

    let client = match SignalingClient::connect(&signaling, signaling.clone()) {
        Ok(c) => c.with_timeout(timeout),
        Err(e) => {
            eprintln!("lowbandd: signaling connect failed: {e}");
            return None;
        }
    };

    // The host mints the room code (and reads it to the party); joiners are
    // handed the code out of band.
    let code = if is_host {
        match client.create_room() {
            Ok(c) => {
                eprintln!("lowbandd: hosting mesh room (size {size}) — room code: {c}");
                c
            }
            Err(e) => {
                eprintln!("lowbandd: mesh room creation failed: {e}");
                return None;
            }
        }
    } else {
        code.unwrap_or_default()
    };

    let static_key = StaticKeypair::generate();
    match mesh::establish_mesh(&client, &code, &id, &static_key, size, timeout) {
        Ok(m) => {
            let budget = mesh::per_peer_budget(400_000, m.peers.len());
            eprintln!(
                "lowbandd: mesh established as '{}' — {} peer(s), {} bps/peer uplink",
                m.me,
                m.peers.len(),
                budget
            );
            for p in &m.peers {
                let k: String = p.session.remote_static_pubkey().iter().take(4)
                    .map(|b| format!("{b:02x}")).collect();
                eprintln!("lowbandd:   peer '{}' (key={k}…)", p.id);
            }
            Some(m)
        }
        Err(e) => {
            eprintln!("lowbandd: mesh establishment failed: {e}");
            None
        }
    }
}

/// Run the inbound router on every mesh peer session — one worker thread per
/// peer, each dispatching that peer's control/media frames. (The full-duplex
/// mesh voice mix runs under `--features audio`.)
#[cfg_attr(feature = "audio", allow(dead_code))]
fn spawn_mesh_worker(mesh: mesh::MeshSession, data_dir: PathBuf) {
    use lowband_messaging::clipboard::ClipboardGrant;

    for peer in mesh.peers {
        let data_dir = data_dir.clone();
        thread::spawn(move || {
            let mut session = peer.session;
            let _ = session.set_read_timeout(Some(Duration::from_secs(1)));
            // Per-peer inbox so concurrent transfers from different peers don't
            // collide.
            let inbox = data_dir.join(format!("inbox-{}.bin", sanitize(&peer.id)));
            let resume = data_dir.join(format!("inbox-{}.resume", sanitize(&peer.id)));
            let mut router =
                inbound::InboundRouter::new(file_transfer::FileReceiver::new(inbox, resume));
            router.clipboard.set_grant(Some(ClipboardGrant::new()));

            while !SHUTDOWN.load(Ordering::Relaxed) {
                match router.recv_and_handle(&mut session) {
                    Ok(handled) => eprintln!("lowbandd: [{}] inbound {handled:?}", peer.id),
                    Err(file_transfer::XferError::Session(_)) => continue,
                    Err(e) => eprintln!("lowbandd: [{}] inbound frame error: {e}", peer.id),
                }
            }
        });
    }
}

/// Filesystem-safe form of a participant id for per-peer inbox filenames.
#[cfg_attr(feature = "audio", allow(dead_code))]
fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

// ── Privilege drop (Linux) ────────────────────────────────────────────────────

/// Parse `name:password:uid:gid:...` lines from `/etc/passwd` to find uid+gid.
#[cfg(target_os = "linux")]
fn lookup_passwd(name: &str) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let mut cols = line.splitn(7, ':');
        let login = cols.next()?;
        if login != name {
            continue;
        }
        cols.next(); // password
        let uid: u32 = cols.next()?.parse().ok()?;
        let gid: u32 = cols.next()?.parse().ok()?;
        return Some((uid, gid));
    }
    None
}

/// Drop to `username` after the IPC socket has been bound.
///
/// Sequence: setgid → setgroups([]) → setuid → verify.  The order matters:
/// setuid must come last because it removes the ability to call setgid.
/// setgroups([]) clears supplementary groups so no residual rights survive.
#[cfg(target_os = "linux")]
fn drop_privileges(username: &str) -> Result<(), String> {
    let (uid, gid) =
        lookup_passwd(username).ok_or_else(|| format!("user '{username}' not in /etc/passwd"))?;

    extern "C" {
        #[link_name = "setgid"]
        fn c_setgid(gid: u32) -> i32;
        #[link_name = "setgroups"]
        fn c_setgroups(size: usize, list: *const u32) -> i32;
        #[link_name = "setuid"]
        fn c_setuid(uid: u32) -> i32;
        #[link_name = "getuid"]
        fn c_getuid() -> u32;
    }

    unsafe {
        if c_setgid(gid) != 0 {
            return Err(format!("setgid({gid}) failed"));
        }
        // Clear supplementary groups: no residual group rights survive.
        if c_setgroups(0, std::ptr::null()) != 0 {
            return Err("setgroups([]) failed".into());
        }
        if c_setuid(uid) != 0 {
            return Err(format!("setuid({uid}) failed"));
        }
        if c_getuid() != uid {
            return Err("post-drop UID verification failed — still root?".into());
        }
    }

    eprintln!("lowbandd: dropped to uid={uid} gid={gid} ({username})");
    Ok(())
}

// ── CPU snapshot ──────────────────────────────────────────────────────────────

struct CpuSnapshot {
    cpu_ns: u64,
    wall: Instant,
}

impl CpuSnapshot {
    fn now() -> Self {
        Self { cpu_ns: proc_cpu_ns(), wall: Instant::now() }
    }

    /// Fraction of total machine CPU consumed since this snapshot was taken.
    fn pct_since(&self, logical_cpus: u32) -> f32 {
        let delta_cpu = proc_cpu_ns().saturating_sub(self.cpu_ns);
        let delta_wall = Instant::now().duration_since(self.wall).as_nanos() as u64;
        if delta_wall == 0 || logical_cpus == 0 {
            return 0.0;
        }
        let capacity = delta_wall.saturating_mul(logical_cpus as u64);
        ((delta_cpu as f64 / capacity as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

/// Total process CPU time (user + kernel) in nanoseconds; 0 on unsupported platforms.
#[cfg(target_os = "linux")]
fn proc_cpu_ns() -> u64 {
    (|| -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after = stat.rfind(')')? + 1;
        let fields: Vec<&str> = stat[after..].split_whitespace().collect();
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        // Jiffies → ns: Linux CLK_TCK is always 100 on shipping kernels.
        Some((utime + stime) * 1_000_000_000 / 100)
    })()
    .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn proc_cpu_ns() -> u64 {
    0
}

// ── Tier derivation ───────────────────────────────────────────────────────────

/// Map thermal pressure and daemon CPU load to a session quality tier.
///
/// Thermal is the primary signal — Critical always forces Survival regardless of
/// CPU. CPU load (this process) is a secondary gate: if the daemon itself is
/// burning more than the Constrained ceiling even at Nominal thermal, the session
/// degrades to protect the endpoint.
fn derive_tier(thermal: ThermalPressure, cpu_pct: f32) -> TierState {
    match thermal {
        ThermalPressure::Critical => TierState::Survival,
        ThermalPressure::Serious => TierState::Constrained,
        ThermalPressure::Fair => {
            if cpu_pct > 35.0 { TierState::Constrained } else { TierState::Comfortable }
        }
        ThermalPressure::Nominal => {
            if cpu_pct > 35.0 {
                TierState::Constrained
            } else if cpu_pct > 20.0 {
                TierState::Comfortable
            } else {
                TierState::Full
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[cfg(not(unix))]
fn main() {
    eprintln!("lowbandd: Windows named-pipe IPC not yet implemented");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    let cfg = parse_args();

    // Bind the IPC socket before dropping privileges.  The socket path
    // (/tmp/lowband.sock) is world-writable; the file is created by the
    // daemon and is owned by the daemon user after the drop.
    let server = IpcServer::bind(&cfg.ipc_socket).unwrap_or_else(|e| {
        eprintln!("lowbandd: bind {}: {e}", cfg.ipc_socket.display());
        std::process::exit(1);
    });

    eprintln!("lowbandd: IPC socket bound at {}", cfg.ipc_socket.display());

    // Linux: drop to the least-privilege system account after socket bind.
    // macOS: launchd already ran the daemon as _lowband via the plist UserName key.
    // Windows: the SCM started the service as NT SERVICE\LowBandDaemon.
    #[cfg(target_os = "linux")]
    if let Some(ref user) = cfg.drop_to {
        drop_privileges(user).unwrap_or_else(|e| {
            eprintln!("lowbandd: privilege drop failed: {e}");
            std::process::exit(1);
        });
    }

    install_signal_handlers();

    // Establish the peer session if requested. With `--features audio` the
    // daemon runs the full-duplex voice loop (mic ↔ speaker over the E2EE
    // session); otherwise it runs the receive-only inbound router (chat,
    // clipboard, panic, file transfer over the encrypted channel).
    if let Some(session) = establish_peer_session(&cfg.session_mode, cfg.stun_server) {
        #[cfg(feature = "audio")]
        {
            let data_dir = cfg.data_dir.clone();
            thread::spawn(move || {
                if let Err(e) = voice_loop::run(session, data_dir) {
                    eprintln!("lowbandd: voice loop error: {e}");
                }
            });
        }
        #[cfg(not(feature = "audio"))]
        spawn_session_worker(session, cfg.data_dir.clone());
    }

    // Mesh group call (FR-14): establish a full mesh over the room roster and
    // run a per-peer worker for each pairwise E2EE session.
    if let Some(mesh) = establish_mesh_session(&cfg.session_mode, cfg.stun_server) {
        #[cfg(feature = "audio")]
        {
            let data_dir = cfg.data_dir.clone();
            thread::spawn(move || {
                if let Err(e) = mesh_voice::run(mesh, data_dir) {
                    eprintln!("lowbandd: mesh voice loop error: {e}");
                }
            });
        }
        #[cfg(not(feature = "audio"))]
        spawn_mesh_worker(mesh, cfg.data_dir.clone());
    }

    let thermal_mon = ThermalMonitor::new();
    let mut cpu_ceiling = CpuCeiling::constrained();
    let logical_cpus = cpu_ceiling.logical_cpus();

    let tick = Duration::from_millis(100); // 10 Hz
    let mut snap = CpuSnapshot::now();
    // FR-11 live quality indicator: fed the governor's own per-tick state so it
    // can never drift from what the governor decided. Logged once a second.
    let mut quality = quality_indicator::QualityIndicator::new();
    let mut tick_count: u32 = 0;

    eprintln!(
        "lowbandd: governor running (data_dir={}, link_bps={})",
        cfg.data_dir.display(),
        cfg.link_bps,
    );

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            eprintln!("lowbandd: received shutdown signal");
            break;
        }

        let thermal = thermal_mon.sample();
        let cpu_pct = snap.pct_since(logical_cpus);
        snap = CpuSnapshot::now();

        let tier = derive_tier(thermal, cpu_pct);
        cpu_ceiling.set_tier(tier);

        let constraints = GearConstraints::from_thermal(thermal);
        let budgets = allocate(cfg.link_bps, &constraints);

        // Update the honest quality indicator from this tick's governor state.
        let (rtt_ms, loss_pct) = (0, 0.0);
        quality.update(tier, &budgets, rtt_ms, loss_pct);
        tick_count += 1;
        if tick_count % 10 == 0 {
            if let Some(line) = quality.line() {
                eprintln!("lowbandd: {line}");
            }
        }

        server.broadcast(&IpcEvent::TierUpdate { tier, cpu_percent: cpu_pct, thermal });
        server.broadcast(&IpcEvent::StreamBudget { budgets, rtt_ms, loss_pct });
        server.broadcast(&IpcEvent::GearUpdate { constraints });

        // Honour the CPU ceiling: if the daemon is over budget, sleep the
        // throttle duration (capped at one tick) rather than the full tick.
        let sleep = match cpu_ceiling.throttle() {
            ThrottleAction::Sleep(d) => d.min(tick),
            ThrottleAction::Continue => tick,
        };
        thread::sleep(sleep);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_critical_thermal_is_always_survival() {
        for cpu in [0.0_f32, 20.0, 50.0, 100.0] {
            assert_eq!(derive_tier(ThermalPressure::Critical, cpu), TierState::Survival);
        }
    }

    #[test]
    fn tier_serious_thermal_is_always_constrained() {
        for cpu in [0.0_f32, 20.0, 50.0] {
            assert_eq!(derive_tier(ThermalPressure::Serious, cpu), TierState::Constrained);
        }
    }

    #[test]
    fn tier_nominal_low_cpu_is_full() {
        assert_eq!(derive_tier(ThermalPressure::Nominal, 5.0), TierState::Full);
    }

    #[test]
    fn tier_nominal_high_cpu_is_constrained() {
        assert_eq!(derive_tier(ThermalPressure::Nominal, 40.0), TierState::Constrained);
    }

    #[test]
    fn tier_fair_high_cpu_is_constrained() {
        assert_eq!(derive_tier(ThermalPressure::Fair, 40.0), TierState::Constrained);
    }

    #[test]
    fn tier_fair_low_cpu_is_comfortable() {
        assert_eq!(derive_tier(ThermalPressure::Fair, 10.0), TierState::Comfortable);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lookup_passwd_root_uid_zero() {
        // root is always uid=0 gid=0 on Linux.
        let (uid, gid) = lookup_passwd("root").expect("root must exist");
        assert_eq!(uid, 0);
        assert_eq!(gid, 0);
    }

    #[test]
    fn cpu_snapshot_pct_is_clamped() {
        let snap = CpuSnapshot::now();
        // Sleep long enough to accumulate wall time, then measure.
        thread::sleep(Duration::from_millis(10));
        let pct = snap.pct_since(4);
        assert!((0.0..=100.0).contains(&pct));
    }
}

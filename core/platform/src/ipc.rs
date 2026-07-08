//! IPC socket — Feature 157.
//!
//! Connects `lowbandd` to its per-platform UI shells over a domain socket
//! (Unix) or named pipe (Windows) using a FlatBuffer wire format described in
//! `proto/ipc.fbs`.
//!
//! # Wire framing
//!
//! Every message is prefixed with a 4-byte little-endian `body_len` field:
//!
//! ```text
//! ┌─────────────────┬────────────┬──────────────────────────────┐
//! │ body_len (u32LE)│ kind (u8)  │  FlatBuffer table (body_len-1 bytes) │
//! └─────────────────┴────────────┴──────────────────────────────┘
//! ```
//!
//! `kind` values:  `0` = [`IpcEvent::TierUpdate`],
//!                 `1` = [`IpcEvent::StreamBudget`],
//!                 `2` = [`IpcEvent::GearUpdate`],
//!                 `3` = [`IpcEvent::ElevationRequested`],
//!                 `4` = [`IpcEvent::ElevationResponse`].
//!
//! # Bidirectional flow
//!
//! Kinds 0–2 are daemon → UI shell only (pushed by the governor).
//! Kinds 3–4 implement the Windows UAC hand-off round-trip:
//!
//! ```text
//! Daemon                          UI Shell
//!   │  ElevationRequested{reason}   │
//!   │ ─────────────────────────────►│  ShellExecuteEx("runas", ...)
//!   │                               │  ↳ UAC prompt on Secure Desktop
//!   │  ElevationResponse{outcome}   │
//!   │ ◄─────────────────────────────│
//! ```
//!
//! The daemon sends [`IpcEvent::ElevationRequested`] via
//! [`IpcServer::broadcast`].  The UI shell receives it via
//! [`IpcClient::receiver`], invokes the UAC, then sends back
//! [`IpcEvent::ElevationResponse`] via [`IpcClient::send`].  The daemon reads
//! the response from [`IpcServer::inbound`].
//!
//! # Platform transports
//!
//! | Platform    | Transport                        |
//! |-------------|----------------------------------|
//! | Linux/macOS | Unix domain socket (`/tmp/lowband.sock`) |
//! | Windows     | Named pipe (`\\.\pipe\lowband-ipc`) |
//!
//! # Usage — daemon side
//!
//! ```no_run
//! use std::path::Path;
//! use lowband_platform::ipc::{IpcServer, IpcEvent};
//! use lowband_platform::{TierState, ThermalPressure, StreamBudgets, GearConstraints};
//!
//! let server = IpcServer::bind(Path::new("/tmp/lowband.sock")).unwrap();
//! // …governor tick…
//! server.broadcast(&IpcEvent::TierUpdate {
//!     tier: TierState::Comfortable,
//!     cpu_percent: 22.5,
//!     thermal: ThermalPressure::Nominal,
//! });
//! // …read elevation responses from the UI shell…
//! if let Ok(event) = server.inbound().try_recv() {
//!     // handle IpcEvent::ElevationResponse{..}
//! }
//! ```
//!
//! # Usage — UI shell side
//!
//! ```no_run
//! use std::path::Path;
//! use lowband_platform::ipc::{IpcClient, IpcEvent};
//! use lowband_platform::elevation::{ElevationOutcome};
//!
//! let client = IpcClient::connect(Path::new("/tmp/lowband.sock")).unwrap();
//! for event in client.receiver().iter() {
//!     match event {
//!         IpcEvent::TierUpdate { tier, .. } => println!("tier → {:?}", tier),
//!         IpcEvent::ElevationRequested { reason } => {
//!             // invoke UAC, then respond
//!             let outcome = ElevationOutcome::Granted; // placeholder
//!             client.send(&IpcEvent::ElevationResponse { reason, outcome }).ok();
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use flatbuffers::FlatBufferBuilder;

use crate::elevation::{ElevationOutcome, EscalationReason};
use crate::gear_policy::{CameraGear, GearConstraints, StreamBudgets};
use crate::thermal::ThermalPressure;
use crate::tier::TierState;

// ── Unix-only socket types ────────────────────────────────────────────────────
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

// ── Kind tags ─────────────────────────────────────────────────────────────────
const KIND_TIER: u8 = 0;
const KIND_BUDGET: u8 = 1;
const KIND_GEAR: u8 = 2;
/// Daemon → UI shell: please raise a UAC prompt on the Windows Secure Desktop.
const KIND_ELEV_REQ: u8 = 3;
/// UI shell → Daemon: UAC prompt outcome.
const KIND_ELEV_RESP: u8 = 4;

// ── Public event enum ─────────────────────────────────────────────────────────

/// Events exchanged between `lowbandd` and connected UI shells.
#[derive(Debug, Clone)]
pub enum IpcEvent {
    /// Quality tier changed or CPU/thermal snapshot available.
    TierUpdate {
        tier: TierState,
        cpu_percent: f32,
        thermal: ThermalPressure,
    },
    /// Per-stream bandwidth allocation from the current governor tick.
    /// Also carries network measurements for the UI quality bar.
    StreamBudget {
        budgets: StreamBudgets,
        rtt_ms: u32,
        loss_pct: f32,
    },
    /// Encoder gear constraints derived from the latest thermal reading.
    GearUpdate { constraints: GearConstraints },

    /// Daemon → UI shell: the daemon requires a privilege escalation.
    ///
    /// The UI shell must invoke `ShellExecuteEx(verb="runas", ...)` on the
    /// Windows Secure Desktop and reply with [`IpcEvent::ElevationResponse`].
    /// The daemon's `platform_execute` blocks until the response arrives.
    ElevationRequested { reason: EscalationReason },

    /// UI shell → Daemon: the UAC prompt has completed with `outcome`.
    ///
    /// Must be sent in response to every [`IpcEvent::ElevationRequested`]
    /// message — even on cancellation (`Denied`) or when the shell is headless
    /// (`Unavailable`).  Never swallowed silently.
    ElevationResponse { reason: EscalationReason, outcome: ElevationOutcome },
}

// ── FlatBuffer helpers ─────────────────────────────────────────────────────────

// VOffsetT for the Nth field in a FlatBuffers table: 4 + 2*N.
#[inline(always)]
fn voff(n: u16) -> flatbuffers::VOffsetT {
    4 + 2 * n
}

fn tier_to_u8(t: TierState) -> u8 {
    match t {
        TierState::Survival => 0,
        TierState::Constrained => 1,
        TierState::Comfortable => 2,
        TierState::Full => 3,
    }
}

fn tier_from_u8(v: u8) -> TierState {
    match v {
        1 => TierState::Constrained,
        2 => TierState::Comfortable,
        3 => TierState::Full,
        _ => TierState::Survival,
    }
}

fn thermal_to_u8(t: ThermalPressure) -> u8 {
    match t {
        ThermalPressure::Nominal => 0,
        ThermalPressure::Fair => 1,
        ThermalPressure::Serious => 2,
        ThermalPressure::Critical => 3,
    }
}

fn thermal_from_u8(v: u8) -> ThermalPressure {
    match v {
        1 => ThermalPressure::Fair,
        2 => ThermalPressure::Serious,
        3 => ThermalPressure::Critical,
        _ => ThermalPressure::Nominal,
    }
}

fn camera_gear_to_pair(g: CameraGear) -> (u8, u8) {
    match g {
        CameraGear::GearA => (0, 0),
        CameraGear::GearB { svt_preset } => (1, svt_preset),
        CameraGear::Off => (2, 0),
        CameraGear::GearC => (3, 0),
    }
}

fn camera_gear_from_pair(kind: u8, preset: u8) -> CameraGear {
    match kind {
        0 => CameraGear::GearA,
        1 => CameraGear::GearB { svt_preset: preset },
        3 => CameraGear::GearC,
        _ => CameraGear::Off,
    }
}

fn av1_cap_to_u8(cap: crate::gear_policy::Av1EncodeCapability) -> u8 {
    use crate::gear_policy::Av1EncodeCapability;
    match cap {
        Av1EncodeCapability::Capable => 0,
        Av1EncodeCapability::Legacy => 1,
    }
}

fn av1_cap_from_u8(v: u8) -> crate::gear_policy::Av1EncodeCapability {
    use crate::gear_policy::Av1EncodeCapability;
    match v {
        1 => Av1EncodeCapability::Legacy,
        _ => Av1EncodeCapability::Capable,
    }
}

fn reason_to_u8(r: EscalationReason) -> u8 {
    match r {
        EscalationReason::ScreenCapture  => 0,
        EscalationReason::InputInjection => 1,
        EscalationReason::ServiceInstall => 2,
        EscalationReason::ProtectedWrite => 3,
    }
}

fn reason_from_u8(v: u8) -> EscalationReason {
    match v {
        1 => EscalationReason::InputInjection,
        2 => EscalationReason::ServiceInstall,
        3 => EscalationReason::ProtectedWrite,
        _ => EscalationReason::ScreenCapture,
    }
}

fn outcome_to_u8(o: &ElevationOutcome) -> u8 {
    match o {
        ElevationOutcome::Granted     => 0,
        ElevationOutcome::Denied      => 1,
        ElevationOutcome::Unavailable => 2,
    }
}

fn outcome_from_u8(v: u8) -> ElevationOutcome {
    match v {
        0 => ElevationOutcome::Granted,
        1 => ElevationOutcome::Denied,
        _ => ElevationOutcome::Unavailable,
    }
}

// ── Encoding ──────────────────────────────────────────────────────────────────

fn encode_tier(tier: TierState, cpu_percent: f32, thermal: ThermalPressure) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(64);
    let start = fbb.start_table();
    fbb.push_slot::<u8>(voff(0), tier_to_u8(tier), 0);
    fbb.push_slot::<f32>(voff(1), cpu_percent, 0.0_f32);
    fbb.push_slot::<u8>(voff(2), thermal_to_u8(thermal), 0);
    let o = fbb.end_table(start);
    let root: flatbuffers::WIPOffset<flatbuffers::ForwardsUOffset<()>> =
        flatbuffers::WIPOffset::new(o.value());
    fbb.finish(root, None);
    fbb.finished_data().to_vec()
}

fn encode_budget(budgets: &StreamBudgets, rtt_ms: u32, loss_pct: f32) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(64);
    let start = fbb.start_table();
    fbb.push_slot::<u32>(voff(0), budgets.audio_bps, 0);
    fbb.push_slot::<u32>(voff(1), budgets.input_bps, 0);
    fbb.push_slot::<u32>(voff(2), budgets.screen_coarse_bps, 0);
    fbb.push_slot::<u32>(voff(3), budgets.camera_bps, 0);
    fbb.push_slot::<u32>(voff(4), budgets.screen_refinement_bps, 0);
    fbb.push_slot::<u32>(voff(5), budgets.xfer_bps, 0);
    fbb.push_slot::<u32>(voff(6), rtt_ms, 0);
    fbb.push_slot::<f32>(voff(7), loss_pct, 0.0_f32);
    let o = fbb.end_table(start);
    let root: flatbuffers::WIPOffset<flatbuffers::ForwardsUOffset<()>> =
        flatbuffers::WIPOffset::new(o.value());
    fbb.finish(root, None);
    fbb.finished_data().to_vec()
}

fn encode_gear(constraints: &GearConstraints) -> Vec<u8> {
    let (cam_kind, svt_preset) = camera_gear_to_pair(constraints.max_camera_gear);
    let mut fbb = FlatBufferBuilder::with_capacity(48);
    let start = fbb.start_table();
    fbb.push_slot::<u8>(voff(0), cam_kind, 0);
    fbb.push_slot::<u8>(voff(1), svt_preset, 0);
    fbb.push_slot::<bool>(voff(2), constraints.screen_refinement_allowed, false);
    fbb.push_slot::<u8>(voff(3), thermal_to_u8(constraints.thermal_level), 0);
    fbb.push_slot::<u8>(voff(4), av1_cap_to_u8(constraints.av1_encode), 0);
    let o = fbb.end_table(start);
    let root: flatbuffers::WIPOffset<flatbuffers::ForwardsUOffset<()>> =
        flatbuffers::WIPOffset::new(o.value());
    fbb.finish(root, None);
    fbb.finished_data().to_vec()
}

fn encode_elev_req(reason: EscalationReason) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(16);
    let start = fbb.start_table();
    fbb.push_slot::<u8>(voff(0), reason_to_u8(reason), 0);
    let o = fbb.end_table(start);
    let root: flatbuffers::WIPOffset<flatbuffers::ForwardsUOffset<()>> =
        flatbuffers::WIPOffset::new(o.value());
    fbb.finish(root, None);
    fbb.finished_data().to_vec()
}

fn encode_elev_resp(reason: EscalationReason, outcome: &ElevationOutcome) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(16);
    let start = fbb.start_table();
    fbb.push_slot::<u8>(voff(0), reason_to_u8(reason), 0);
    fbb.push_slot::<u8>(voff(1), outcome_to_u8(outcome), 0);
    let o = fbb.end_table(start);
    let root: flatbuffers::WIPOffset<flatbuffers::ForwardsUOffset<()>> =
        flatbuffers::WIPOffset::new(o.value());
    fbb.finish(root, None);
    fbb.finished_data().to_vec()
}

/// Serialize `event` into a length-prefixed wire frame.
pub fn encode_event(event: &IpcEvent) -> Vec<u8> {
    let (kind, fb) = match event {
        IpcEvent::TierUpdate { tier, cpu_percent, thermal } => {
            (KIND_TIER, encode_tier(*tier, *cpu_percent, *thermal))
        }
        IpcEvent::StreamBudget { budgets, rtt_ms, loss_pct } => {
            (KIND_BUDGET, encode_budget(budgets, *rtt_ms, *loss_pct))
        }
        IpcEvent::GearUpdate { constraints } => (KIND_GEAR, encode_gear(constraints)),
        IpcEvent::ElevationRequested { reason } => (KIND_ELEV_REQ, encode_elev_req(*reason)),
        IpcEvent::ElevationResponse { reason, outcome } => {
            (KIND_ELEV_RESP, encode_elev_resp(*reason, outcome))
        }
    };

    let body_len = (1u32 + fb.len() as u32).to_le_bytes();
    let mut frame = Vec::with_capacity(4 + 1 + fb.len());
    frame.extend_from_slice(&body_len);
    frame.push(kind);
    frame.extend_from_slice(&fb);
    frame
}

// ── Decoding (manual FlatBuffers reader) ──────────────────────────────────────
//
// Implements the FlatBuffers table reading spec without relying on flatc-
// generated code.  The format is:
//   buf[0..4]        — u32 LE forward offset from position 0 to the root table
//   buf[root..]      — table: i32 LE backward offset to vtable (soffset_t)
//   buf[vtable..]    — u16 vtable_size, u16 obj_size, then u16 field offsets

struct FbTable<'a> {
    buf: &'a [u8],
    obj: usize,
    vtab: usize,
}

impl<'a> FbTable<'a> {
    fn from_bytes(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < 8 {
            return None;
        }
        let root_off = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
        let obj = root_off;
        if obj + 4 > buf.len() {
            return None;
        }
        let soffset = i32::from_le_bytes(buf[obj..obj + 4].try_into().ok()?);
        let vtab = (obj as i64 - soffset as i64) as usize;
        if vtab + 4 > buf.len() {
            return None;
        }
        Some(FbTable { buf, obj, vtab })
    }

    fn field_pos(&self, idx: usize) -> Option<usize> {
        let vt_size = u16::from_le_bytes(
            self.buf[self.vtab..self.vtab + 2].try_into().ok()?,
        ) as usize;
        let slot = 4 + idx * 2;
        if slot + 2 > vt_size {
            return None;
        }
        let off =
            u16::from_le_bytes(self.buf[self.vtab + slot..self.vtab + slot + 2].try_into().ok()?)
                as usize;
        if off == 0 { None } else { Some(self.obj + off) }
    }

    fn u8_at(&self, idx: usize) -> u8 {
        self.field_pos(idx)
            .filter(|&p| p < self.buf.len())
            .map(|p| self.buf[p])
            .unwrap_or(0)
    }

    fn u32_at(&self, idx: usize) -> u32 {
        self.field_pos(idx)
            .filter(|&p| p + 4 <= self.buf.len())
            .map(|p| u32::from_le_bytes(self.buf[p..p + 4].try_into().unwrap()))
            .unwrap_or(0)
    }

    fn f32_at(&self, idx: usize) -> f32 {
        self.field_pos(idx)
            .filter(|&p| p + 4 <= self.buf.len())
            .map(|p| f32::from_le_bytes(self.buf[p..p + 4].try_into().unwrap()))
            .unwrap_or(0.0)
    }

    fn bool_at(&self, idx: usize) -> bool {
        self.u8_at(idx) != 0
    }
}

/// Decode a frame body (kind byte already stripped; `fb` is the FlatBuffer bytes).
fn decode_fb(kind: u8, fb: &[u8]) -> Option<IpcEvent> {
    let t = FbTable::from_bytes(fb)?;
    match kind {
        KIND_TIER => Some(IpcEvent::TierUpdate {
            tier: tier_from_u8(t.u8_at(0)),
            cpu_percent: t.f32_at(1),
            thermal: thermal_from_u8(t.u8_at(2)),
        }),
        KIND_BUDGET => {
            let budgets = StreamBudgets {
                audio_bps: t.u32_at(0),
                input_bps: t.u32_at(1),
                screen_coarse_bps: t.u32_at(2),
                camera_bps: t.u32_at(3),
                screen_refinement_bps: t.u32_at(4),
                xfer_bps: t.u32_at(5),
            };
            Some(IpcEvent::StreamBudget { budgets, rtt_ms: t.u32_at(6), loss_pct: t.f32_at(7) })
        }
        KIND_GEAR => {
            let cam = camera_gear_from_pair(t.u8_at(0), t.u8_at(1));
            let constraints = GearConstraints {
                max_camera_gear: cam,
                screen_refinement_allowed: t.bool_at(2),
                audio_floor_bps: crate::gear_policy::AUDIO_FLOOR_BPS,
                thermal_level: thermal_from_u8(t.u8_at(3)),
                av1_encode: av1_cap_from_u8(t.u8_at(4)),
            };
            Some(IpcEvent::GearUpdate { constraints })
        }
        KIND_ELEV_REQ => Some(IpcEvent::ElevationRequested {
            reason: reason_from_u8(t.u8_at(0)),
        }),
        KIND_ELEV_RESP => Some(IpcEvent::ElevationResponse {
            reason: reason_from_u8(t.u8_at(0)),
            outcome: outcome_from_u8(t.u8_at(1)),
        }),
        _ => None,
    }
}

/// Parse one length-prefixed frame from `buf`.
/// Returns `Some(event)` on success.  `buf` must contain exactly one frame
/// (i.e. be the body read after stripping the 4-byte length prefix).
pub fn decode_frame(frame_body: &[u8]) -> Option<IpcEvent> {
    if frame_body.is_empty() {
        return None;
    }
    let kind = frame_body[0];
    decode_fb(kind, &frame_body[1..])
}

// ── Socket I/O helpers (platform-agnostic) ────────────────────────────────────

fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let body_len = u32::from_le_bytes(len_buf) as usize;
    if body_len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "zero-length frame"));
    }
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body)?;
    Ok(body)
}

fn write_loop<W: Write>(mut stream: W, rx: mpsc::Receiver<Vec<u8>>) {
    for frame in rx {
        if stream.write_all(&frame).is_err() {
            break;
        }
    }
}

fn read_loop<R: Read>(mut stream: R, tx: mpsc::Sender<IpcEvent>) {
    loop {
        match read_frame(&mut stream) {
            Ok(body) => {
                if let Some(event) = decode_frame(&body) {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

// ── IpcServer ─────────────────────────────────────────────────────────────────

/// Unix-domain socket server.  Binds to a path and fans out [`IpcEvent`]s to
/// every connected UI shell.
///
/// The accept loop runs on a background thread.  Each connected client also
/// gets its own write thread so a slow shell never blocks the daemon, and a
/// read thread so the shell can send back [`IpcEvent::ElevationResponse`].
#[cfg(unix)]
pub struct IpcServer {
    clients: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>>,
    inbound: mpsc::Receiver<IpcEvent>,
    _accept: JoinHandle<()>,
}

#[cfg(unix)]
impl IpcServer {
    /// Bind to `path`, removing any stale socket file first.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        let clients: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let clients2 = clients.clone();
        let (inbound_tx, inbound_rx) = mpsc::channel::<IpcEvent>();

        let accept = thread::Builder::new()
            .name("ipc-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let stream_write =
                                stream.try_clone().expect("clone ipc stream for write");
                            let (tx, rx) = mpsc::sync_channel(32);
                            clients2.lock().unwrap().push(tx);
                            thread::Builder::new()
                                .name("ipc-write".into())
                                .spawn(move || write_loop(stream_write, rx))
                                .ok();
                            let itx = inbound_tx.clone();
                            thread::Builder::new()
                                .name("ipc-read".into())
                                .spawn(move || read_loop(stream, itx))
                                .ok();
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(IpcServer { clients, inbound: inbound_rx, _accept: accept })
    }

    /// Serialize `event` and deliver it to every connected UI shell.
    /// Clients that have disconnected are removed silently; clients with a full
    /// send buffer are kept (they will catch up or be dropped on the next call).
    pub fn broadcast(&self, event: &IpcEvent) {
        let frame = encode_event(event);
        let mut guard = self.clients.lock().unwrap();
        guard.retain(|tx| match tx.try_send(frame.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => true,
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    /// Number of currently connected UI shells.
    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Inbound event receiver.
    ///
    /// Events sent by connected UI shells arrive here.  Currently only
    /// [`IpcEvent::ElevationResponse`] is sent shell → daemon; all other
    /// event kinds are daemon → shell only.
    pub fn inbound(&self) -> &mpsc::Receiver<IpcEvent> {
        &self.inbound
    }
}

// ── IpcClient ─────────────────────────────────────────────────────────────────

/// Unix-domain socket client used by UI shells to receive daemon events
/// and send responses (e.g. [`IpcEvent::ElevationResponse`]).
#[cfg(unix)]
pub struct IpcClient {
    rx: mpsc::Receiver<IpcEvent>,
    frame_tx: mpsc::SyncSender<Vec<u8>>,
    _read: JoinHandle<()>,
    _write: JoinHandle<()>,
}

#[cfg(unix)]
impl IpcClient {
    /// Connect to a running [`IpcServer`] at `path`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let stream_write = stream.try_clone()?;
        let (event_tx, event_rx) = mpsc::channel::<IpcEvent>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(32);
        let read = thread::Builder::new()
            .name("ipc-read".into())
            .spawn(move || read_loop(stream, event_tx))?;
        let write = thread::Builder::new()
            .name("ipc-write".into())
            .spawn(move || write_loop(stream_write, frame_rx))?;
        Ok(IpcClient { rx: event_rx, frame_tx, _read: read, _write: write })
    }

    /// Blocking iterator over incoming [`IpcEvent`]s from the daemon.
    /// The iterator ends when the server closes the connection.
    pub fn receiver(&self) -> &mpsc::Receiver<IpcEvent> {
        &self.rx
    }

    /// Send an event to the daemon.
    ///
    /// Used by the UI shell to return [`IpcEvent::ElevationResponse`] after a
    /// UAC prompt.  Returns an error if the write channel has been closed.
    pub fn send(&self, event: &IpcEvent) -> io::Result<()> {
        let frame = encode_event(event);
        self.frame_tx.send(frame).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ipc client write channel closed")
        })
    }
}

// ── Windows named-pipe transport ──────────────────────────────────────────────
//
// The daemon creates a named pipe server; the UI shell opens the client end.
// The pipe is full-duplex and byte-stream, matching the Unix socket semantics.
// Security: the pipe is created with the default DACL (grants access to the
// creating process's SID and the local Administrators group).  In production
// the MSI restricts the DACL to NT SERVICE\LowBandDaemon + the current user.

#[cfg(target_os = "windows")]
pub const WIN_PIPE_NAME: &str = r"\\.\pipe\lowband-ipc";

#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(std::iter::once(0u16)).collect()
}

/// Windows named-pipe IPC server.
///
/// Mirrors the Unix [`IpcServer`] API exactly so daemon code is transport-agnostic.
#[cfg(target_os = "windows")]
pub struct IpcServer {
    clients: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>>,
    inbound: mpsc::Receiver<IpcEvent>,
    _accept: JoinHandle<()>,
}

#[cfg(target_os = "windows")]
impl IpcServer {
    /// Create the named pipe server and start accepting connections.
    ///
    /// `_path` is ignored on Windows; the pipe name is always
    /// [`WIN_PIPE_NAME`].  The parameter exists so call sites compile on all
    /// platforms without a `#[cfg]` guard.
    pub fn bind(_path: &Path) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_ACCESS_DUPLEX, PIPE_READMODE_BYTE,
            PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        };

        let clients: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let clients2 = clients.clone();
        let (inbound_tx, inbound_rx) = mpsc::channel::<IpcEvent>();

        let accept = thread::Builder::new()
            .name("ipc-accept-win".into())
            .spawn(move || {
                let pipe_name = to_wide(WIN_PIPE_NAME);
                loop {
                    let handle = unsafe {
                        CreateNamedPipeW(
                            pipe_name.as_ptr(),
                            PIPE_ACCESS_DUPLEX,
                            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                            PIPE_UNLIMITED_INSTANCES,
                            4096,
                            4096,
                            0,
                            std::ptr::null(),
                        )
                    };
                    if handle == INVALID_HANDLE_VALUE {
                        break;
                    }
                    // Block until a UI shell connects.
                    unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };

                    // Wrap as std::fs::File so we can use the generic I/O loops.
                    let file_read = unsafe {
                        std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle)
                    };
                    let file_write = file_read.try_clone().expect("clone pipe handle for write");

                    let (tx, rx) = mpsc::sync_channel(32);
                    clients2.lock().unwrap().push(tx);
                    thread::Builder::new()
                        .name("ipc-write-win".into())
                        .spawn(move || write_loop(file_write, rx))
                        .ok();
                    let itx = inbound_tx.clone();
                    thread::Builder::new()
                        .name("ipc-read-win".into())
                        .spawn(move || read_loop(file_read, itx))
                        .ok();
                }
            })?;

        Ok(IpcServer { clients, inbound: inbound_rx, _accept: accept })
    }

    pub fn broadcast(&self, event: &IpcEvent) {
        let frame = encode_event(event);
        let mut guard = self.clients.lock().unwrap();
        guard.retain(|tx| match tx.try_send(frame.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => true,
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Inbound events sent by connected UI shells (e.g. [`IpcEvent::ElevationResponse`]).
    pub fn inbound(&self) -> &mpsc::Receiver<IpcEvent> {
        &self.inbound
    }
}

/// Windows named-pipe IPC client.
///
/// Mirrors the Unix [`IpcClient`] API exactly.
#[cfg(target_os = "windows")]
pub struct IpcClient {
    rx: mpsc::Receiver<IpcEvent>,
    frame_tx: mpsc::SyncSender<Vec<u8>>,
    _read: JoinHandle<()>,
    _write: JoinHandle<()>,
}

#[cfg(target_os = "windows")]
impl IpcClient {
    /// Open the client end of the LowBand named pipe.
    ///
    /// `_path` is ignored on Windows; see [`WIN_PIPE_NAME`].
    pub fn connect(_path: &Path) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, OPEN_EXISTING,
        };
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

        let pipe_name = to_wide(WIN_PIPE_NAME);
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let file_read = unsafe {
            std::fs::File::from_raw_handle(handle as std::os::windows::io::RawHandle)
        };
        let file_write = file_read.try_clone()?;

        let (event_tx, event_rx) = mpsc::channel::<IpcEvent>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(32);
        let read = thread::Builder::new()
            .name("ipc-read-win".into())
            .spawn(move || read_loop(file_read, event_tx))?;
        let write = thread::Builder::new()
            .name("ipc-write-win".into())
            .spawn(move || write_loop(file_write, frame_rx))?;
        Ok(IpcClient { rx: event_rx, frame_tx, _read: read, _write: write })
    }

    pub fn receiver(&self) -> &mpsc::Receiver<IpcEvent> {
        &self.rx
    }

    pub fn send(&self, event: &IpcEvent) -> io::Result<()> {
        let frame = encode_event(event);
        self.frame_tx.send(frame).map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ipc client write channel closed")
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elevation::{ElevationOutcome, EscalationReason};
    use crate::gear_policy::{GearConstraints, AUDIO_FLOOR_BPS};
    use crate::thermal::ThermalPressure;
    use crate::tier::TierState;

    fn roundtrip(event: IpcEvent) -> IpcEvent {
        let frame = encode_event(&event);
        // frame = [4-byte len][kind][flatbuffer]
        let body_len = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        let body = &frame[4..4 + body_len];
        decode_frame(body).expect("decode failed")
    }

    #[test]
    fn tier_update_roundtrip() {
        let ev = roundtrip(IpcEvent::TierUpdate {
            tier: TierState::Constrained,
            cpu_percent: 34.5,
            thermal: ThermalPressure::Fair,
        });
        if let IpcEvent::TierUpdate { tier, cpu_percent, thermal } = ev {
            assert_eq!(tier, TierState::Constrained);
            assert!((cpu_percent - 34.5).abs() < 1e-4);
            assert_eq!(thermal, ThermalPressure::Fair);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn stream_budget_roundtrip() {
        let budgets = StreamBudgets {
            audio_bps: 24_000,
            input_bps: 8_000,
            screen_coarse_bps: 20_000,
            camera_bps: 150_000,
            screen_refinement_bps: 30_000,
            xfer_bps: 10_000,
        };
        let ev = roundtrip(IpcEvent::StreamBudget {
            budgets,
            rtt_ms: 42,
            loss_pct: 0.5,
        });
        if let IpcEvent::StreamBudget { budgets: b, rtt_ms, loss_pct } = ev {
            assert_eq!(b.audio_bps, 24_000);
            assert_eq!(b.camera_bps, 150_000);
            assert_eq!(rtt_ms, 42);
            assert!((loss_pct - 0.5).abs() < 1e-4);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn gear_update_roundtrip() {
        let constraints =
            GearConstraints::from_thermal(ThermalPressure::Serious);
        let ev = roundtrip(IpcEvent::GearUpdate { constraints });
        if let IpcEvent::GearUpdate { constraints: c } = ev {
            assert!(matches!(c.max_camera_gear, CameraGear::GearB { svt_preset: 12 }));
            assert!(!c.screen_refinement_allowed);
            assert_eq!(c.thermal_level, ThermalPressure::Serious);
            assert_eq!(c.audio_floor_bps, AUDIO_FLOOR_BPS);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn gear_off_roundtrip() {
        let constraints = GearConstraints::from_thermal(ThermalPressure::Critical);
        let ev = roundtrip(IpcEvent::GearUpdate { constraints });
        if let IpcEvent::GearUpdate { constraints: c } = ev {
            assert_eq!(c.max_camera_gear, CameraGear::Off);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn all_tier_states_survive_roundtrip() {
        for tier in [
            TierState::Survival,
            TierState::Constrained,
            TierState::Comfortable,
            TierState::Full,
        ] {
            let ev = roundtrip(IpcEvent::TierUpdate {
                tier,
                cpu_percent: 0.0,
                thermal: ThermalPressure::Nominal,
            });
            if let IpcEvent::TierUpdate { tier: got, .. } = ev {
                assert_eq!(got, tier);
            } else {
                panic!("wrong variant for {:?}", tier);
            }
        }
    }

    #[test]
    fn all_thermal_levels_survive_roundtrip() {
        for thermal in [
            ThermalPressure::Nominal,
            ThermalPressure::Fair,
            ThermalPressure::Serious,
            ThermalPressure::Critical,
        ] {
            let ev = roundtrip(IpcEvent::TierUpdate {
                tier: TierState::Full,
                cpu_percent: 0.0,
                thermal,
            });
            if let IpcEvent::TierUpdate { thermal: got, .. } = ev {
                assert_eq!(got, thermal);
            } else {
                panic!("wrong variant");
            }
        }
    }

    // ── Elevation event roundtrips ────────────────────────────────────────────

    #[test]
    fn elevation_requested_all_reasons_roundtrip() {
        for reason in [
            EscalationReason::ScreenCapture,
            EscalationReason::InputInjection,
            EscalationReason::ServiceInstall,
            EscalationReason::ProtectedWrite,
        ] {
            let ev = roundtrip(IpcEvent::ElevationRequested { reason });
            if let IpcEvent::ElevationRequested { reason: got } = ev {
                assert_eq!(got, reason, "ElevationRequested reason mismatch for {reason:?}");
            } else {
                panic!("wrong variant for ElevationRequested({reason:?})");
            }
        }
    }

    #[test]
    fn elevation_response_all_outcomes_roundtrip() {
        for outcome in [
            ElevationOutcome::Granted,
            ElevationOutcome::Denied,
            ElevationOutcome::Unavailable,
        ] {
            let ev = roundtrip(IpcEvent::ElevationResponse {
                reason: EscalationReason::ServiceInstall,
                outcome: outcome.clone(),
            });
            if let IpcEvent::ElevationResponse { reason, outcome: got } = ev {
                assert_eq!(reason, EscalationReason::ServiceInstall);
                assert_eq!(got, outcome, "ElevationResponse outcome mismatch");
            } else {
                panic!("wrong variant for ElevationResponse(outcome={outcome:?})");
            }
        }
    }

    // Verify the denied path: an ElevationResponse with Denied must not be
    // mistaken for Granted after a wire roundtrip.
    #[test]
    fn elevation_denied_is_not_granted_after_roundtrip() {
        let ev = roundtrip(IpcEvent::ElevationResponse {
            reason: EscalationReason::ScreenCapture,
            outcome: ElevationOutcome::Denied,
        });
        if let IpcEvent::ElevationResponse { outcome, .. } = ev {
            assert!(!outcome.is_granted(), "Denied must not be Granted after roundtrip");
        } else {
            panic!("wrong variant");
        }
    }

    // ── Unix socket integration tests ─────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn server_client_exchange() {
        use std::time::Duration;

        let path = std::path::PathBuf::from(format!(
            "/tmp/lowband_ipc_test_{}.sock",
            std::process::id()
        ));
        let server = IpcServer::bind(&path).expect("bind");

        std::thread::sleep(Duration::from_millis(20));

        let client = IpcClient::connect(&path).expect("connect");

        std::thread::sleep(Duration::from_millis(20));

        server.broadcast(&IpcEvent::TierUpdate {
            tier: TierState::Full,
            cpu_percent: 10.0,
            thermal: ThermalPressure::Nominal,
        });

        let ev = client
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("no event received");

        if let IpcEvent::TierUpdate { tier, .. } = ev {
            assert_eq!(tier, TierState::Full);
        } else {
            panic!("wrong event variant: {:?}", ev);
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Full UAC hand-off roundtrip over a Unix socket:
    ///
    ///   daemon broadcasts ElevationRequested
    ///   → shell receives it
    ///   → shell sends back ElevationResponse{Granted}
    ///   → daemon reads it from inbound()
    #[cfg(unix)]
    #[test]
    fn elevation_handoff_roundtrip_over_socket() {
        use std::time::Duration;

        let path = std::path::PathBuf::from(format!(
            "/tmp/lowband_ipc_elev_test_{}.sock",
            std::process::id()
        ));
        let server = IpcServer::bind(&path).expect("bind");

        std::thread::sleep(Duration::from_millis(20));

        let client = IpcClient::connect(&path).expect("connect");

        std::thread::sleep(Duration::from_millis(20));

        // Daemon side: broadcast the elevation request.
        server.broadcast(&IpcEvent::ElevationRequested {
            reason: EscalationReason::ScreenCapture,
        });

        // Shell side: receive the request.
        let req = client
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("shell did not receive ElevationRequested");
        assert!(
            matches!(req, IpcEvent::ElevationRequested { reason: EscalationReason::ScreenCapture }),
            "unexpected event: {req:?}"
        );

        // Shell side: send back the response (simulating UAC Granted).
        client
            .send(&IpcEvent::ElevationResponse {
                reason: EscalationReason::ScreenCapture,
                outcome: ElevationOutcome::Granted,
            })
            .expect("shell send failed");

        // Daemon side: read the response from inbound().
        let resp = server
            .inbound()
            .recv_timeout(Duration::from_secs(1))
            .expect("daemon did not receive ElevationResponse");
        if let IpcEvent::ElevationResponse { reason, outcome } = resp {
            assert_eq!(reason, EscalationReason::ScreenCapture);
            assert!(outcome.is_granted(), "expected Granted, got {outcome:?}");
        } else {
            panic!("unexpected inbound event: {resp:?}");
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Verify that ElevationResponse{Denied} reaches the daemon correctly —
    /// the "never silent" property must hold through the socket layer.
    #[cfg(unix)]
    #[test]
    fn elevation_denial_propagates_through_socket() {
        use std::time::Duration;

        let path = std::path::PathBuf::from(format!(
            "/tmp/lowband_ipc_denied_test_{}.sock",
            std::process::id()
        ));
        let server = IpcServer::bind(&path).expect("bind");
        std::thread::sleep(Duration::from_millis(20));
        let client = IpcClient::connect(&path).expect("connect");
        std::thread::sleep(Duration::from_millis(20));

        server.broadcast(&IpcEvent::ElevationRequested {
            reason: EscalationReason::ProtectedWrite,
        });

        client.receiver().recv_timeout(Duration::from_secs(1)).expect("recv req");

        client
            .send(&IpcEvent::ElevationResponse {
                reason: EscalationReason::ProtectedWrite,
                outcome: ElevationOutcome::Denied,
            })
            .expect("send resp");

        let resp = server
            .inbound()
            .recv_timeout(Duration::from_secs(1))
            .expect("daemon recv resp");
        if let IpcEvent::ElevationResponse { outcome, .. } = resp {
            assert!(
                !outcome.is_granted(),
                "Denied must not be reported as Granted to the daemon"
            );
        } else {
            panic!("unexpected inbound event: {resp:?}");
        }

        let _ = std::fs::remove_file(&path);
    }
}

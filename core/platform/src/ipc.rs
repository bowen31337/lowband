//! IPC socket — Feature 157.
//!
//! Connects `lowbandd` to its per-platform UI shells over a Unix domain socket
//! using a FlatBuffer wire format described in `proto/ipc.fbs`.
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
//!                 `2` = [`IpcEvent::GearUpdate`].
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
//! ```
//!
//! # Usage — UI shell side
//!
//! ```no_run
//! use std::path::Path;
//! use lowband_platform::ipc::{IpcClient, IpcEvent};
//!
//! let client = IpcClient::connect(Path::new("/tmp/lowband.sock")).unwrap();
//! for event in client.receiver() {
//!     match event {
//!         IpcEvent::TierUpdate { tier, .. } => println!("tier → {:?}", tier),
//!         _ => {}
//!     }
//! }
//! ```

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use flatbuffers::FlatBufferBuilder;

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

// ── Public event enum ─────────────────────────────────────────────────────────

/// Events that `lowbandd` pushes to connected UI shells.
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
    }
}

fn camera_gear_from_pair(kind: u8, preset: u8) -> CameraGear {
    match kind {
        0 => CameraGear::GearA,
        1 => CameraGear::GearB { svt_preset: preset },
        _ => CameraGear::Off,
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
            };
            Some(IpcEvent::GearUpdate { constraints })
        }
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

// ── Socket I/O helpers ────────────────────────────────────────────────────────

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

#[cfg(unix)]
fn write_loop(mut stream: UnixStream, rx: mpsc::Receiver<Vec<u8>>) {
    for frame in rx {
        if stream.write_all(&frame).is_err() {
            break;
        }
    }
}

#[cfg(unix)]
fn read_loop(mut stream: UnixStream, tx: mpsc::Sender<IpcEvent>) {
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
/// gets its own write thread so a slow shell never blocks the daemon.
#[cfg(unix)]
pub struct IpcServer {
    clients: Arc<Mutex<Vec<mpsc::SyncSender<Vec<u8>>>>>,
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

        let accept = thread::Builder::new()
            .name("ipc-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let (tx, rx) = mpsc::sync_channel(32);
                            clients2.lock().unwrap().push(tx);
                            thread::Builder::new()
                                .name("ipc-write".into())
                                .spawn(move || write_loop(stream, rx))
                                .ok();
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(IpcServer { clients, _accept: accept })
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
}

// ── IpcClient ─────────────────────────────────────────────────────────────────

/// Unix-domain socket client used by UI shells to receive daemon events.
#[cfg(unix)]
pub struct IpcClient {
    rx: mpsc::Receiver<IpcEvent>,
    _read: JoinHandle<()>,
}

#[cfg(unix)]
impl IpcClient {
    /// Connect to a running [`IpcServer`] at `path`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let (tx, rx) = mpsc::channel();
        let read = thread::Builder::new()
            .name("ipc-read".into())
            .spawn(move || read_loop(stream, tx))?;
        Ok(IpcClient { rx, _read: read })
    }

    /// Blocking iterator over incoming [`IpcEvent`]s.
    /// The iterator ends when the server closes the connection.
    pub fn receiver(&self) -> &mpsc::Receiver<IpcEvent> {
        &self.rx
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    #[cfg(unix)]
    #[test]
    fn server_client_exchange() {
        use std::time::Duration;

        let path = std::path::PathBuf::from(format!(
            "/tmp/lowband_ipc_test_{}.sock",
            std::process::id()
        ));
        let server = IpcServer::bind(&path).expect("bind");

        // Give the accept thread a moment to start.
        std::thread::sleep(Duration::from_millis(20));

        let client = IpcClient::connect(&path).expect("connect");

        // Give the client write-thread time to register with the server.
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
}

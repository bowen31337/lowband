//! Mesh group rendezvous (FR-14) — up to four participants per room.
//!
//! The 1:1 rendezvous (`/signal/session` + offer/answer) is strictly
//! two-party. A group call is a full mesh: every participant connects
//! directly to every other, so each needs the roster — the other
//! participants' identity keys and transport candidates. This module brokers
//! that roster; the peers still run the same pairwise Noise-IK
//! ([`SecureSession`](../../lowband_crypto/index.html)) over the addresses
//! they discover here, so no media touches the server.
//!
//! Routes (wired in [`crate::router`]):
//! - `POST /signal/room` → create a room, returns `{"room_code": "..."}`
//! - `POST /signal/room/join` → register `{room_code, participant_id, pubkey}`
//! - `POST /signal/room/candidate` → publish `{room_code, participant_id, candidate}`
//! - `GET  /signal/room/:code` → the roster `{"participants":[{id,pubkey,candidates}]}`

use axum::{extract::{Path, State}, http::StatusCode, response::Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{unix_now, AppState, ROOM_TTL_SECS};

/// Maximum participants in one mesh room (PRD FR-14: "group calls up to 4").
pub const MESH_MAX_PARTICIPANTS: usize = 4;

impl AppState {
    /// Create a room with the given code and a fresh TTL.
    pub(crate) fn create_room(&self, code: &str) {
        let expires = unix_now() + ROOM_TTL_SECS;
        self.rooms.insert(code.as_bytes(), &expires.to_le_bytes()).ok();
    }

    /// `true` if `code` names a live (non-expired) room.
    pub(crate) fn room_live(&self, code: &str) -> bool {
        match self.rooms.get(code.as_bytes()).ok().flatten() {
            Some(v) if v.len() >= 8 => {
                let exp = i64::from_le_bytes(v[..8].try_into().unwrap());
                if exp > unix_now() {
                    true
                } else {
                    self.rooms.remove(code.as_bytes()).ok();
                    false
                }
            }
            _ => false,
        }
    }

    /// Current participant ids in `code`, in id order.
    pub(crate) fn room_participants(&self, code: &str) -> Vec<String> {
        let prefix = format!("{code}:");
        self.room_members
            .scan_prefix(prefix.as_bytes())
            .filter_map(|r| r.ok())
            .filter_map(|(k, _)| {
                String::from_utf8(k.to_vec()).ok().and_then(|s| {
                    s.strip_prefix(&prefix).map(str::to_string)
                })
            })
            .collect()
    }

    /// Register a participant. Returns `false` when the room is already full.
    pub(crate) fn add_participant(&self, code: &str, pid: &str, pubkey_hex: &str) -> bool {
        // Idempotent re-join is allowed; only a genuinely new id counts toward
        // the cap.
        let key = format!("{code}:{pid}");
        let is_new = self.room_members.get(key.as_bytes()).ok().flatten().is_none();
        if is_new && self.room_participants(code).len() >= MESH_MAX_PARTICIPANTS {
            return false;
        }
        self.room_members.insert(key.as_bytes(), pubkey_hex.as_bytes()).ok();
        true
    }

    fn participant_pubkey(&self, code: &str, pid: &str) -> Option<String> {
        let key = format!("{code}:{pid}");
        self.room_members
            .get(key.as_bytes())
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v.to_vec()).ok())
    }

    fn participant_candidates(&self, code: &str, pid: &str) -> Vec<String> {
        let prefix = format!("{code}:{pid}:");
        self.room_candidates
            .scan_prefix(prefix.as_bytes())
            .filter_map(|r| r.ok())
            .filter_map(|(_, v)| String::from_utf8(v.to_vec()).ok())
            .collect()
    }

    pub(crate) fn add_room_candidate(&self, code: &str, pid: &str, seq: u64, cand: &str) {
        let key = format!("{code}:{pid}:{seq:016x}");
        self.room_candidates.insert(key.as_bytes(), cand.as_bytes()).ok();
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────

pub(crate) async fn post_room(State(st): State<AppState>) -> (StatusCode, Json<Value>) {
    let code = crate::gen_code();
    st.create_room(&code);
    (StatusCode::CREATED, Json(json!({ "room_code": code })))
}

#[derive(Deserialize)]
pub(crate) struct RoomJoinBody {
    room_code: String,
    participant_id: String,
    /// Hex-encoded static public key.
    pubkey: String,
}

pub(crate) async fn post_room_join(
    State(st): State<AppState>,
    Json(b): Json<RoomJoinBody>,
) -> StatusCode {
    if !st.room_live(&b.room_code) {
        return StatusCode::NOT_FOUND;
    }
    if b.participant_id.is_empty() || b.participant_id.len() > 64 {
        return StatusCode::BAD_REQUEST;
    }
    if st.add_participant(&b.room_code, &b.participant_id, &b.pubkey) {
        StatusCode::OK
    } else {
        // Room is at MESH_MAX_PARTICIPANTS.
        StatusCode::CONFLICT
    }
}

#[derive(Deserialize)]
pub(crate) struct RoomCandidateBody {
    room_code: String,
    participant_id: String,
    candidate: String,
}

pub(crate) async fn post_room_candidate(
    State(st): State<AppState>,
    Json(b): Json<RoomCandidateBody>,
) -> StatusCode {
    if !st.room_live(&b.room_code) {
        return StatusCode::NOT_FOUND;
    }
    // Only a registered participant may publish candidates.
    if st.participant_pubkey(&b.room_code, &b.participant_id).is_none() {
        return StatusCode::FORBIDDEN;
    }
    let seq = crate::next_candidate_seq();
    st.add_room_candidate(&b.room_code, &b.participant_id, seq, &b.candidate);
    StatusCode::ACCEPTED
}

pub(crate) async fn get_room(
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    if !st.room_live(&code) {
        return Err(StatusCode::NOT_FOUND);
    }
    let participants: Vec<Value> = st
        .room_participants(&code)
        .into_iter()
        .map(|pid| {
            json!({
                "id": pid,
                "pubkey": st.participant_pubkey(&code, &pid).unwrap_or_default(),
                "candidates": st.participant_candidates(&code, &pid),
            })
        })
        .collect();
    Ok(Json(json!({ "room_code": code, "participants": participants })))
}

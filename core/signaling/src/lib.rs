//! LowBand stateless signaling rendezvous service.
//!
//! Registers 9-digit session codes, brokers offer/answer/ICE exchanges, and
//! mints short-lived TURN credentials.  Holds no media — it exits the path
//! the moment the peers connect directly.

pub mod client;
pub mod room;
pub use client::{ClientError, JoinInfo, RoomParticipant, RoomRoster, SignalingClient, TurnCredential};
pub use room::MESH_MAX_PARTICIPANTS;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

const SESSION_TTL: Duration = Duration::from_secs(300);
const TURN_TTL_SECS: u64 = 86_400;
/// Mesh room lifetime in seconds (matches the 1:1 session TTL).
const ROOM_TTL_SECS: i64 = 300;

// ── session_codes store ───────────────────────────────────────────────────────
//
// Each entry encodes: expires_at (i64 LE) || created_at (i64 LE) || responder_pubkey
// This mirrors the session_codes table schema (code PK, responder_pubkey,
// created_at, expires_at).

#[derive(Clone)]
pub struct AppState {
    session_codes: sled::Tree,
    ice_candidates: sled::Tree,
    offers: sled::Tree,
    answers: sled::Tree,
    // Mesh group rooms (FR-14): `rooms` holds code → expires_at; `room_members`
    // holds `{code}:{participant_id}` → static pubkey; `room_candidates` holds
    // `{code}:{participant_id}:{seq}` → candidate.
    rooms: sled::Tree,
    room_members: sled::Tree,
    room_candidates: sled::Tree,
    turn_urls: Arc<Vec<String>>,
    turn_secret: Arc<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// Creates a temporary in-memory store (used by tests).
    pub fn new() -> Self {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .expect("in-memory sled failed");
        Self {
            session_codes: db.open_tree("session_codes").expect("open tree"),
            ice_candidates: db.open_tree("ice_candidates").expect("open tree"),
            offers: db.open_tree("offers").expect("open tree"),
            answers: db.open_tree("answers").expect("open tree"),
            rooms: db.open_tree("rooms").expect("open tree"),
            room_members: db.open_tree("room_members").expect("open tree"),
            room_candidates: db.open_tree("room_candidates").expect("open tree"),
            turn_urls: Arc::new(vec!["turn:turn.example.com:3478".into()]),
            turn_secret: Arc::new("test-secret".into()),
        }
    }

    /// Opens (or creates) a file-backed store at `path`.
    pub fn open(path: &str, turn_urls: Vec<String>, turn_secret: String) -> sled::Result<Self> {
        let db = sled::open(path)?;
        Ok(Self {
            session_codes: db.open_tree("session_codes")?,
            ice_candidates: db.open_tree("ice_candidates")?,
            offers: db.open_tree("offers")?,
            answers: db.open_tree("answers")?,
            rooms: db.open_tree("rooms")?,
            room_members: db.open_tree("room_members")?,
            room_candidates: db.open_tree("room_candidates")?,
            turn_urls: Arc::new(turn_urls),
            turn_secret: Arc::new(turn_secret),
        })
    }

    /// Returns all pending ICE candidates for `session_code` in insertion order.
    pub fn pending_candidates(&self, session_code: &str) -> Vec<String> {
        let prefix = format!("{session_code}:");
        self.ice_candidates
            .scan_prefix(prefix.as_bytes())
            .filter_map(|r| r.ok())
            .filter_map(|(_, v)| String::from_utf8(v.to_vec()).ok())
            .collect()
    }

    /// Returns the pending SDP offer for `session_code`, if one has been posted.
    pub fn pending_offer(&self, session_code: &str) -> Option<String> {
        self.offers
            .get(session_code.as_bytes())
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v.to_vec()).ok())
    }

    /// Returns the pending SDP answer for `session_code`, if one has been posted.
    pub fn pending_answer(&self, session_code: &str) -> Option<String> {
        self.answers
            .get(session_code.as_bytes())
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v.to_vec()).ok())
    }

    /// Inserts a session that is already expired. Used only in tests.
    #[doc(hidden)]
    pub fn insert_expired_session(&self, code: &str) {
        let entry = encode_entry(0, 0, &[]);
        self.session_codes.insert(code.as_bytes(), entry).unwrap();
    }
}

fn encode_entry(created_at: i64, expires_at: i64, pubkey: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + pubkey.len());
    v.extend_from_slice(&expires_at.to_le_bytes());
    v.extend_from_slice(&created_at.to_le_bytes());
    v.extend_from_slice(pubkey);
    v
}

fn decode_expires_at(bytes: &[u8]) -> Option<i64> {
    bytes.get(..8).map(|b| i64::from_le_bytes(b.try_into().unwrap()))
}

// ── Session code generation ────────────────────────────────────────────────────

static CODE_SEQ: AtomicU64 = AtomicU64::new(100_000_000);
static CAND_SEQ: AtomicU64 = AtomicU64::new(0);

fn gen_code() -> String {
    format!("{:09}", CODE_SEQ.fetch_add(1, Ordering::Relaxed) % 1_000_000_000)
}

/// Monotonic sequence for candidate ordering (shared by 1:1 and mesh).
fn next_candidate_seq() -> u64 {
    CAND_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/signal/session", post(post_session))
        .route("/signal/join/:code", get(get_join))
        .route("/signal/offer", post(post_offer))
        .route("/signal/answer", post(post_answer))
        .route("/signal/answer/:code", get(get_answer))
        .route("/signal/candidate", post(post_candidate))
        .route("/signal/turn", post(post_turn))
        .route("/signal/connected", post(post_connected))
        // Mesh group rendezvous (FR-14).
        .route("/signal/room", post(room::post_room))
        .route("/signal/room/join", post(room::post_room_join))
        .route("/signal/room/candidate", post(room::post_room_candidate))
        .route("/signal/room/:code", get(room::get_room))
        .with_state(state)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SessionResp {
    session_code: String,
}

async fn post_session(State(st): State<AppState>) -> (StatusCode, Json<SessionResp>) {
    let code = gen_code();
    let now = unix_now();
    let expires_at = now + SESSION_TTL.as_secs() as i64;
    let entry = encode_entry(now, expires_at, &[]);
    st.session_codes.insert(code.as_bytes(), entry).unwrap();
    (StatusCode::CREATED, Json(SessionResp { session_code: code }))
}

async fn get_join(
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let now = unix_now();
    match st.session_codes.get(code.as_bytes()).unwrap() {
        Some(bytes) => match decode_expires_at(&bytes) {
            Some(exp) if exp > now => {
                let offer = st.pending_offer(&code);
                let candidates = st.pending_candidates(&code);
                Ok(Json(serde_json::json!({
                    "session_code": code,
                    "offer": offer,
                    "candidates": candidates,
                })))
            }
            _ => {
                st.session_codes.remove(code.as_bytes()).ok();
                Err(StatusCode::NOT_FOUND)
            }
        },
        None => Err(StatusCode::NOT_FOUND),
    }
}

// Called by the offering peer (technician) to poll for the joiner's answer.
// Returns `{"answer": null}` until the joiner has posted one, then the SDP.
// This completes the rendezvous loop — `post_answer` stores it, this reads it.
async fn get_answer(
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    check_code(&st, &code)?;
    Ok(Json(serde_json::json!({ "answer": st.pending_answer(&code) })))
}

#[derive(Deserialize)]
struct CodedBody {
    session_code: String,
}

#[derive(Deserialize)]
struct OfferBody {
    session_code: String,
    sdp: String,
}

#[derive(Deserialize)]
struct AnswerBody {
    session_code: String,
    sdp: Option<String>,
}

#[derive(Deserialize)]
struct CandidateBody {
    session_code: String,
    candidate: String,
}

async fn post_offer(
    State(st): State<AppState>,
    Json(b): Json<OfferBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    st.offers
        .insert(b.session_code.as_bytes(), b.sdp.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn post_answer(
    State(st): State<AppState>,
    Json(b): Json<AnswerBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    if let Some(sdp) = b.sdp {
        st.answers
            .insert(b.session_code.as_bytes(), sdp.as_bytes())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(StatusCode::OK)
}

async fn post_candidate(
    State(st): State<AppState>,
    Json(b): Json<CandidateBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    let seq = CAND_SEQ.fetch_add(1, Ordering::Relaxed);
    let key = format!("{}:{:016x}", b.session_code, seq);
    st.ice_candidates
        .insert(key.as_bytes(), b.candidate.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::ACCEPTED)
}

fn check_code(st: &AppState, code: &str) -> Result<(), StatusCode> {
    let now = unix_now();
    match st.session_codes.get(code.as_bytes()).unwrap() {
        Some(bytes) => match decode_expires_at(&bytes) {
            Some(exp) if exp > now => Ok(()),
            _ => Err(StatusCode::NOT_FOUND),
        },
        None => Err(StatusCode::NOT_FOUND),
    }
}

// Called by either peer once a direct LBTP connection is established.
// Evicts the session so the signaling service is no longer in the path.
async fn post_connected(
    State(st): State<AppState>,
    Json(b): Json<CodedBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    st.session_codes.remove(b.session_code.as_bytes()).ok();
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct TurnResp {
    turn_credential: TurnCred,
}

#[derive(Serialize)]
struct TurnCred {
    urls: Vec<String>,
    username: String,
    credential: String,
    ttl_secs: u64,
}

async fn post_turn(State(st): State<AppState>) -> (StatusCode, Json<TurnResp>) {
    let expires_at = unix_now() + TURN_TTL_SECS as i64;
    let username = format!("{}:lowband", expires_at);

    // coturn REST API: credential = base64(HMAC-SHA256(shared_secret, username))
    let mut mac = Hmac::<Sha256>::new_from_slice(st.turn_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    let credential = B64.encode(mac.finalize().into_bytes());

    (
        StatusCode::OK,
        Json(TurnResp {
            turn_credential: TurnCred {
                urls: (*st.turn_urls).clone(),
                username,
                credential,
                ttl_secs: TURN_TTL_SECS,
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use base64::engine::general_purpose::STANDARD as B64;
    use hmac::{Hmac, Mac};
    use http_body_util::BodyExt as _;
    use sha2::Sha256;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn post_turn_returns_200_with_valid_credential() {
        let secret = "test-secret";
        let state = AppState::new();
        // AppState::new() uses "test-secret" and "turn:turn.example.com:3478"
        let app = router(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/signal/turn")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let cred = &v["turn_credential"];

        // username must be "<unix_timestamp>:lowband"
        let username = cred["username"].as_str().unwrap();
        let parts: Vec<&str> = username.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1], "lowband");
        let expires: i64 = parts[0].parse().unwrap();
        let now = unix_now();
        assert!(expires > now, "credential must not already be expired");
        assert!(expires <= now + TURN_TTL_SECS as i64 + 2);

        // credential must be valid HMAC-SHA256(secret, username) in base64
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(username.as_bytes());
        let expected = B64.encode(mac.finalize().into_bytes());
        assert_eq!(cred["credential"].as_str().unwrap(), expected);

        assert_eq!(cred["ttl_secs"].as_u64().unwrap(), TURN_TTL_SECS);
        assert!(cred["urls"].as_array().unwrap().len() > 0);
    }
}

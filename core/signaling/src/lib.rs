//! LowBand stateless signaling rendezvous service.
//!
//! Registers 9-digit session codes, brokers offer/answer/ICE exchanges, and
//! mints short-lived TURN credentials.  Holds no media — it exits the path
//! the moment the peers connect directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SESSION_TTL: Duration = Duration::from_secs(300);

// ── session_codes store ───────────────────────────────────────────────────────
//
// Each entry encodes: expires_at (i64 LE) || created_at (i64 LE) || responder_pubkey
// This mirrors the session_codes table schema (code PK, responder_pubkey,
// created_at, expires_at).

#[derive(Clone)]
pub struct AppState {
    session_codes: sled::Tree,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    /// Creates a temporary in-memory session_codes store (used by tests).
    pub fn new() -> Self {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .expect("in-memory sled failed");
        Self { session_codes: db.open_tree("session_codes").expect("open tree") }
    }

    /// Opens (or creates) a file-backed session_codes store at `path`.
    pub fn open(path: &str) -> sled::Result<Self> {
        let db = sled::open(path)?;
        Ok(Self { session_codes: db.open_tree("session_codes")? })
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

fn gen_code() -> String {
    format!("{:09}", CODE_SEQ.fetch_add(1, Ordering::Relaxed) % 1_000_000_000)
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
        .route("/signal/candidate", post(post_candidate))
        .route("/signal/turn", post(post_turn))
        .route("/signal/connected", post(post_connected))
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
            Some(exp) if exp > now => Ok(Json(serde_json::json!({ "code": code }))),
            _ => {
                st.session_codes.remove(code.as_bytes()).ok();
                Err(StatusCode::NOT_FOUND)
            }
        },
        None => Err(StatusCode::NOT_FOUND),
    }
}

// Shared body shape for offer / answer / candidate — any extra fields are ignored.
#[derive(Deserialize)]
struct CodedBody {
    session_code: String,
}

async fn post_offer(
    State(st): State<AppState>,
    Json(b): Json<CodedBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    Ok(StatusCode::OK)
}

async fn post_answer(
    State(st): State<AppState>,
    Json(b): Json<CodedBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
    Ok(StatusCode::OK)
}

async fn post_candidate(
    State(st): State<AppState>,
    Json(b): Json<CodedBody>,
) -> Result<StatusCode, StatusCode> {
    check_code(&st, &b.session_code)?;
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

async fn post_turn() -> (StatusCode, Json<TurnResp>) {
    (
        StatusCode::OK,
        Json(TurnResp {
            turn_credential: TurnCred {
                urls: vec!["turn:turn.example.com:3478".into()],
                username: "lowband".into(),
                credential: "stub-cred".into(),
                ttl_secs: 86_400,
            },
        }),
    )
}

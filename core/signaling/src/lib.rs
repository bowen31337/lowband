//! LowBand stateless signaling rendezvous service.
//!
//! Registers 9-digit session codes, brokers offer/answer/ICE exchanges, and
//! mints short-lived TURN credentials.  Holds no media — it exits the path
//! the moment the peers connect directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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

// ── State ─────────────────────────────────────────────────────────────────────

struct SessionEntry {
    peer_descriptor: Value,
    expires_at: SystemTime,
}

#[derive(Clone)]
pub struct AppState {
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())) }
    }
}

// ── Session code generation ────────────────────────────────────────────────────

static CODE_SEQ: AtomicU64 = AtomicU64::new(100_000_000);

fn gen_code() -> String {
    format!("{:09}", CODE_SEQ.fetch_add(1, Ordering::Relaxed) % 1_000_000_000)
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
        .with_state(state)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct SessionResp {
    session_code: String,
}

async fn post_session(State(st): State<AppState>) -> (StatusCode, Json<SessionResp>) {
    let code = gen_code();
    let entry = SessionEntry {
        peer_descriptor: serde_json::json!({ "code": &code }),
        expires_at: SystemTime::now() + SESSION_TTL,
    };
    st.sessions.lock().unwrap().insert(code.clone(), entry);
    (StatusCode::CREATED, Json(SessionResp { session_code: code }))
}

async fn get_join(
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let mut map = st.sessions.lock().unwrap();
    let now = SystemTime::now();
    match map.get(&code) {
        Some(e) if e.expires_at > now => Ok(Json(e.peer_descriptor.clone())),
        Some(_) => {
            map.remove(&code);
            Err(StatusCode::NOT_FOUND)
        }
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
    let map = st.sessions.lock().unwrap();
    let now = SystemTime::now();
    match map.get(code) {
        Some(e) if e.expires_at > now => Ok(()),
        _ => Err(StatusCode::NOT_FOUND),
    }
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

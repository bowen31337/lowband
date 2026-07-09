//! Integration tests for the signaling rendezvous service.
//!
//! Each test drives the in-process router via `tower::ServiceExt::oneshot`,
//! so no network socket is needed.  The shared `AppState` (wrapped in `Arc`)
//! is visible across cloned router instances within the same test.

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use http_body_util::BodyExt;
use lowband_signaling::{router, AppState};
use serde_json::Value;
use tower::ServiceExt;

// ── Helpers shared across test modules ───────────────────────────────────────

async fn create_session(app: axum::Router) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    collect_json(resp.into_body()).await["session_code"]
        .as_str()
        .unwrap()
        .to_string()
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn make_app() -> axum::Router {
    router(AppState::new())
}

async fn collect_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn json_body(v: Value) -> Body {
    Body::from(serde_json::to_vec(&v).unwrap())
}

// ── POST /signal/session ──────────────────────────────────────────────────────

#[tokio::test]
async fn post_session_returns_201_with_9_digit_code() {
    let app = make_app();
    let req = Request::builder()
        .method("POST")
        .uri("/signal/session")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = collect_json(resp.into_body()).await;
    let code = v["session_code"].as_str().unwrap();
    assert_eq!(code.len(), 9, "session_code must be exactly 9 characters");
    assert!(
        code.chars().all(|c| c.is_ascii_digit()),
        "session_code must be all digits, got: {code}"
    );
}

// ── GET /signal/join/{code} ───────────────────────────────────────────────────

#[tokio::test]
async fn get_join_expired_code_returns_404() {
    let state = AppState::new();
    let app = router(state.clone());
    state.insert_expired_session("111111111");

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/signal/join/111111111")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_join_happy_path_returns_200_with_peer_descriptor() {
    let app = make_app();

    // Create a session to get a valid code.
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/signal/session").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v = collect_json(resp.into_body()).await;
    let code = v["session_code"].as_str().unwrap().to_string();

    // Join with the valid code.
    let resp2 = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/signal/join/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let desc = collect_json(resp2.into_body()).await;
    assert!(desc.is_object(), "peer descriptor must be a JSON object");
}

#[tokio::test]
async fn get_join_unknown_code_returns_404() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/signal/join/000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/offer ────────────────────────────────────────────────────────

#[tokio::test]
async fn post_offer_happy_path_returns_200() {
    let app = make_app();
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/signal/session").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = collect_json(resp.into_body()).await["session_code"].as_str().unwrap().to_string();

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/offer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": code, "sdp": "v=0\r\n" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

#[tokio::test]
async fn post_offer_bad_code_returns_404() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/offer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "000000000", "sdp": "v=0\r\n" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/offer — expired ─────────────────────────────────────────────

#[tokio::test]
async fn post_offer_expired_code_returns_404() {
    let state = AppState::new();
    let app = router(state.clone());
    state.insert_expired_session("111111111");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/offer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "111111111", "sdp": "v=0\r\n" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/answer ───────────────────────────────────────────────────────

#[tokio::test]
async fn post_answer_happy_path_returns_200() {
    let app = make_app();
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/signal/session").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = collect_json(resp.into_body()).await["session_code"].as_str().unwrap().to_string();

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/answer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": code, "sdp": "v=0\r\n" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

#[tokio::test]
async fn post_answer_bad_code_returns_404() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/answer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "000000000", "sdp": "v=0\r\n" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/candidate ────────────────────────────────────────────────────

#[tokio::test]
async fn post_candidate_happy_path_returns_202() {
    let app = make_app();
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/signal/session").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = collect_json(resp.into_body()).await["session_code"].as_str().unwrap().to_string();

    let resp2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/candidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(
                    serde_json::json!({ "session_code": code, "candidate": "candidate:1 1 UDP 2130706431 192.0.2.1 54400 typ host" }),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn post_candidate_bad_code_returns_404() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/candidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "000000000", "candidate": "..." })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/answer — expired ────────────────────────────────────────────

#[tokio::test]
async fn post_answer_expired_code_returns_404() {
    let state = AppState::new();
    let app = router(state.clone());
    state.insert_expired_session("111111111");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/answer")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "111111111" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/candidate — expired ─────────────────────────────────────────

#[tokio::test]
async fn post_candidate_expired_code_returns_404() {
    let state = AppState::new();
    let app = router(state.clone());
    state.insert_expired_session("111111111");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/candidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "111111111", "candidate": "..." })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── POST /signal/connected — expired ─────────────────────────────────────────

#[tokio::test]
async fn post_connected_expired_code_returns_404() {
    let state = AppState::new();
    let app = router(state.clone());
    state.insert_expired_session("111111111");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/connected")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": "111111111" })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── ICE relay verification ────────────────────────────────────────────────────

#[tokio::test]
async fn post_candidate_relays_candidate_to_ice_store() {
    let state = AppState::new();
    let app = router(state.clone());
    let code = create_session(app.clone()).await;

    let candidate_str = "candidate:1 1 UDP 2130706431 192.0.2.1 54400 typ host";
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/candidate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": code, "candidate": candidate_str })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let pending = state.pending_candidates(&code);
    assert_eq!(pending.len(), 1, "exactly one candidate must be queued");
    assert_eq!(pending[0], candidate_str);
}

#[tokio::test]
async fn post_candidate_trickle_multiple_candidates_all_relayed() {
    let state = AppState::new();
    let app = router(state.clone());
    let code = create_session(app.clone()).await;

    let candidates = [
        "candidate:1 1 UDP 2130706431 192.0.2.1 54400 typ host",
        "candidate:2 1 UDP 1694498815 203.0.113.5 54400 typ srflx raddr 192.0.2.1 rport 54400",
        "candidate:3 1 UDP 41885439 192.0.2.2 54400 typ relay raddr 192.0.2.2 rport 54400",
    ];
    for c in &candidates {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/signal/candidate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(json_body(serde_json::json!({ "session_code": code, "candidate": c })))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "each trickled candidate must be accepted");
    }

    let pending = state.pending_candidates(&code);
    assert_eq!(pending.len(), candidates.len(), "all three candidates must be queued");
    for c in &candidates {
        assert!(pending.iter().any(|p| p == c), "missing candidate: {c}");
    }
}

// ── POST /signal/connected ────────────────────────────────────────────────────

#[tokio::test]
async fn post_connected_drops_session_from_path() {
    let app = make_app();
    // Create a session.
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/signal/session").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let code = collect_json(resp.into_body()).await["session_code"].as_str().unwrap().to_string();

    // Signal that peers connected directly — service must drop the session.
    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/connected")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json_body(serde_json::json!({ "session_code": code })))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // Session must now be gone — any further signaling returns 404.
    let resp3 = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/signal/join/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::NOT_FOUND, "session must be evicted after /connected");
}

#[tokio::test]
async fn service_has_no_media_relay_endpoint() {
    // The signaling service must never forward media.  Any request to a
    // media-path URI must be rejected (404 from the router, not a relay).
    let app = make_app();
    for uri in &["/signal/media", "/media", "/relay"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "media relay endpoint {uri} must not exist"
        );
    }
}

// ── POST /signal/turn ─────────────────────────────────────────────────────────

#[tokio::test]
async fn post_turn_returns_200_with_turn_credential() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signal/turn")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = collect_json(resp.into_body()).await;
    assert!(
        v["turn_credential"].is_object(),
        "response must contain a turn_credential object"
    );
    let cred = &v["turn_credential"];
    assert!(cred["urls"].is_array(), "turn_credential must have urls array");
    assert!(cred["username"].is_string(), "turn_credential must have username");
    assert!(cred["credential"].is_string(), "turn_credential must have credential");
}

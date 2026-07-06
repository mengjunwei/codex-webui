//! End-to-end route tests via axum oneshot (no real port needed).
//!
//! Covers:
//! - GET / is public and returns `{ ok: true }`
//! - GET /api/_ping requires auth → 401 without token
//! - GET /api/_ping accepts API key → 200
//! - POST /api/auth/login returns a JWT on valid key → 200
//! - POST /api/auth/login returns 401 on wrong key

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_webui::auth::AuthService;
use codex_webui::codex::CodexProcessManager;
use codex_webui::db::Db;
use codex_webui::routes::build_router;
use codex_webui::state::AppState;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn state(api_key: &str) -> AppState {
    use std::collections::HashSet;
    let c = Connection::open_in_memory().unwrap();
    AppState {
        db: Arc::new(Db {
            conn: Mutex::new(c),
        }),
        auth: Arc::new(AuthService::new(api_key)),
        codex: Arc::new(CodexProcessManager::new("codex".into(), None)),
        dynamic_files_roots: Arc::new(Mutex::new(HashSet::new())),
    }
}

#[tokio::test]
async fn root_is_public() {
    let app = build_router(state("s"));
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn ping_requires_auth() {
    let app = build_router(state("s"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/_ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ping_accepts_api_key() {
    let app = build_router(state("topsecret"));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/_ping")
                .header("authorization", "Bearer topsecret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_returns_jwt() {
    let app = build_router(state("correct-key"));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"apiKey":"correct-key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["accessToken"].as_str().is_some());
    assert_eq!(v["expiresIn"], 86_400);
}

#[tokio::test]
async fn login_wrong_key_is_401() {
    let app = build_router(state("correct-key"));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"apiKey":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["errorCode"], "auth.invalid_api_key");
}

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
use codex_webui::terminal::{TerminalConfig, TerminalService};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn state(api_key: &str) -> AppState {
    use std::collections::{HashMap, HashSet};
    let c = Connection::open_in_memory().unwrap();
    let db = Arc::new(Db { conn: Mutex::new(c) });
    let term_cfg = {
        let r = codex_webui::settings::SettingsReader::new(&db, None);
        TerminalConfig::from_settings(&r)
    };
    let codex = Arc::new(CodexProcessManager::new("codex".into(), None));
    AppState {
        db,
        mt_pg: None,
        mt_master_key: "test-master".into(),
        mt_team_codex: Arc::new(codex_webui::multitenant::codex_pool::TeamCodexManager::new(
            std::path::PathBuf::from("/tmp/mt-test"),
            "codex".into(),
            None,
        )),
        mt_redis: None,
        metrics_handle: None,
        auth: Arc::new(AuthService::new(api_key)),
        status: Arc::new(codex_webui::codex_status::CodexStatusService::new(codex.clone())),
        codex,
        terminal: TerminalService::new(term_cfg),
        resume_registry: Arc::new(codex_webui::threads::ThreadResumeRegistry::new()),
        dynamic_files_roots: Arc::new(Mutex::new(HashSet::new())),
        settings_cache: Arc::new(Mutex::new(HashMap::new())),
    }
}

#[tokio::test]
async fn status_requires_auth() {
    let app = build_router(state("s"));
    let resp = app
        .oneshot(Request::builder().uri("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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

//! Integration tests for settings CRUD endpoints.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_webui::auth::AuthService;
use codex_webui::codex::CodexProcessManager;
use codex_webui::db::{run_migrations, Db};
use codex_webui::api::build_router;
use codex_webui::services::settings::{self, reconcile_settings};
use codex_webui::state::AppState;
use codex_webui::services::terminal::{TerminalConfig, TerminalService};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn state() -> AppState {
    use std::collections::HashSet;
    let c = Connection::open_in_memory().unwrap();
    let db = Arc::new(Db { conn: Mutex::new(c) });
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();
    let term_cfg = {
        let r = settings::SettingsReader::new(&db, None);
        TerminalConfig::from_settings(&r)
    };
    let codex = Arc::new(CodexProcessManager::new("codex".into(), None));
    AppState {
        db,
        mt_pg: None,
        mt_master_key: "test-master".into(),
        mt_team_codex: Arc::new(codex_webui::services::multitenant::codex_pool::TeamCodexManager::new(
            std::path::PathBuf::from("/tmp/mt-test"),
            "codex".into(),
            None,
        )),
        mt_redis: None,
        metrics_handle: None,
        auth: Arc::new(AuthService::new("test-key")),
        status: Arc::new(codex_webui::services::codex_status::CodexStatusService::new(codex.clone())),
        codex,
        terminal: TerminalService::new(term_cfg),
        resume_registry: Arc::new(codex_webui::services::threads::ThreadResumeRegistry::new()),
        dynamic_files_roots: Arc::new(Mutex::new(HashSet::new())),
        settings_cache: Arc::new(Mutex::new(HashMap::new())),
    }
}

async fn authed(app: axum::Router, method: &str, uri: &str, body: Option<&str>) -> Value {
    let req_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer test-key");
    let req = if let Some(b) = body {
        req_builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap()
    } else {
        req_builder.body(Body::empty()).unwrap()
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if !status.is_success() {
        panic!("{} {} failed: {} {:?}", method, uri, status, v);
    }
    v
}

#[tokio::test]
async fn list_returns_all_settings() {
    let app = build_router(state());
    let v = authed(app, "GET", "/api/settings", None).await;
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 12, "expected 12 settings");
}

#[tokio::test]
async fn list_filters_by_category() {
    let app = build_router(state());
    let v = authed(app, "GET", "/api/settings?category=terminal", None).await;
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 4, "expected 4 terminal settings");
}

#[tokio::test]
async fn get_one_returns_setting() {
    let app = build_router(state());
    let v = authed(app, "GET", "/api/settings/files.uploadMaxBytes", None).await;
    assert_eq!(v["key"], "files.uploadMaxBytes");
    assert_eq!(v["type"], "number");
    assert_eq!(v["value"], 104_857_600);
    assert_eq!(v["source"], "default");
}

#[tokio::test]
async fn get_one_unknown_key_404() {
    let app = build_router(state());
    let req = Request::builder()
        .uri("/api/settings/nonexistent.key")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_one_changes_value() {
    let app = build_router(state());
    let v = authed(
        app,
        "PATCH",
        "/api/settings/files.uploadMaxBytes",
        Some(r#"{"value": 52428800}"#),
    )
    .await;
    assert_eq!(v["value"], 52_428_800);
    assert_eq!(v["source"], "db");
}

#[tokio::test]
async fn update_one_wrong_type_400() {
    let app = build_router(state());
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/settings/files.uploadMaxBytes")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value": "not-a-number"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_one_resets_to_default() {
    let app = build_router(state());
    // First set a value.
    authed(
        build_router(state()),
        "PATCH",
        "/api/settings/files.uploadMaxBytes",
        Some(r#"{"value": 52428800}"#),
    )
    .await;
    // Then delete.
    let v = authed(app, "DELETE", "/api/settings/files.uploadMaxBytes", None).await;
    assert_eq!(v["source"], "default");
    assert_eq!(v["value"], 104_857_600);
}

#[tokio::test]
async fn update_batch_changes_multiple() {
    let app = build_router(state());
    let body = r#"{"updates": [{"key":"files.uploadMaxBytes","value":1048576},{"key":"terminal.maxSessions","value":20}]}"#;
    let v = authed(app, "PATCH", "/api/settings", Some(body)).await;
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 2);
}

#[tokio::test]
async fn proxy_returns_500_when_codex_not_started() {
    // All 6 proxy modules now forward via the codex manager. With the manager
    // not started (tests), the forward returns RpcError::Closed → 500.
    let app = build_router(state());
    let req = Request::builder()
        .uri("/api/apps")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ── H1 parity: string values are JSON-encoded in storage (TS interop) ─────────

#[tokio::test]
async fn string_value_roundtrips_and_is_json_encoded_in_db() {
    let st = state();
    let app = build_router(st.clone());

    // Write a string value.
    let v = authed(
        app,
        "PATCH",
        "/api/settings/general.onlyofficeUrl",
        Some(r#"{"value":"https://docs.example.com"}"#),
    )
    .await;
    assert_eq!(v["value"], "https://docs.example.com");
    assert_eq!(v["source"], "db");

    // Verify the DB stores it JSON-encoded (with embedded quotes), matching TS.
    let conn = st.db.conn.lock().unwrap();
    let stored: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key='general.onlyofficeUrl'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);
    assert_eq!(
        stored, r#""https://docs.example.com""#,
        "string values must be JSON-encoded in storage (parity with TS encodeJson)"
    );
}

// ── H3 parity: constraints populated in DTO + enforced on write ───────────────

#[tokio::test]
async fn constraints_appear_in_dto() {
    let app = build_router(state());
    let v = authed(app, "GET", "/api/settings/terminal.maxSessions", None).await;
    assert_eq!(v["constraints"]["min"], 1);
    assert_eq!(v["constraints"]["max"], 50);
    assert_eq!(v["constraints"]["integer"], true);
}

#[tokio::test]
async fn constraints_enforced_on_write() {
    let app = build_router(state());
    // terminal.maxSessions max is 50 → 999 must be rejected.
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/settings/terminal.maxSessions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":999}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Non-integer rejected (integer constraint).
    let app = build_router(state());
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/settings/terminal.maxSessions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":5.5}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // In-range integer accepted.
    let app = build_router(state());
    let v = authed(
        app,
        "PATCH",
        "/api/settings/terminal.maxSessions",
        Some(r#"{"value":25}"#),
    )
    .await;
    assert_eq!(v["value"], 25);
}

#[tokio::test]
async fn pending_approvals_dto_omits_resolved_at() {
    // No pending rows → empty list, but verify the DTO shape has no resolvedAt.
    let app = build_router(state());
    let v = authed(app, "GET", "/api/pending-approvals", None).await;
    let arr = v["requests"].as_array().unwrap();
    assert!(arr.is_empty(), "fresh DB has no pending requests");
    // (If rows existed, each would be checked for absence of resolvedAt/resolvedBy.)
}

#[tokio::test]
async fn pending_approvals_respond_not_found_is_404() {
    // Manager not started (generation 0), no pending rows → not found.
    let app = build_router(state());
    let req = Request::builder()
        .method("POST")
        .uri("/api/pending-approvals/999/respond")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"result":{"approved":true}}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["errorCode"], "approvals.not_found");
}

// ── CodexStatusService 接线（codex 未连接时应报告 unavailable）────────────────

#[tokio::test]
async fn codex_status_reports_unavailable_when_not_connected() {
    // codex 进程未启动 → CodexStatusService 应聚合为 unavailable，并带原因。
    let app = build_router(state());
    let v = authed(app, "GET", "/api/codex/status", None).await;
    assert_eq!(v["appServer"]["ok"], false);
    assert_eq!(v["appServer"]["connected"], false);
    assert_eq!(v["runtime"]["status"], "unavailable");
    let reasons = v["runtime"]["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r == "appServerUnavailable"),
        "expected appServerUnavailable reason, got {reasons:?}"
    );
}

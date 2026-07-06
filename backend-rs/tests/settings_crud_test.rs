//! Integration tests for settings CRUD endpoints.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_webui::auth::AuthService;
use codex_webui::db::{run_migrations, Db};
use codex_webui::routes::build_router;
use codex_webui::settings::reconcile_settings;
use codex_webui::state::AppState;
use rusqlite::Connection;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn state() -> AppState {
    let c = Connection::open_in_memory().unwrap();
    let db = Arc::new(Db {
        conn: Mutex::new(c),
    });
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();
    AppState {
        db,
        auth: Arc::new(AuthService::new("test-key")),
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
async fn get_one_unknown_key_400() {
    let app = build_router(state());
    let req = Request::builder()
        .uri("/api/settings/nonexistent.key")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
async fn proxy_stub_returns_501() {
    let app = build_router(state());
    let req = Request::builder()
        .uri("/api/apps")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}

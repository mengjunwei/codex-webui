//! Health / probe endpoints.
//!
//! - `GET /`        — public root health (parity with `AppController`).
//! - `GET /api/_ping` — protected probe (Phase 0 only; replaced by real endpoints in Phase 1+).

use axum::Json;
use serde_json::{json, Value};

pub async fn root() -> Json<Value> {
    Json(json!({ "ok": true }))
}

pub async fn ping() -> Json<Value> {
    Json(json!({ "ok": true }))
}

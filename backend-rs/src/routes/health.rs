//! Health / probe endpoints.
//!
//! - `GET /api/status` — protected health (parity with TS `AppController`).
//! - `GET /api/_ping`  — protected probe (Phase 0 internal).

use axum::Json;
use serde_json::{json, Value};

pub async fn ping() -> Json<Value> {
    Json(json!({ "ok": true }))
}

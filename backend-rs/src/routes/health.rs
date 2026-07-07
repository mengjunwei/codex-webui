//! 健康 / 探针端点。
//!
//! - `GET /api/status` — 受保护的健康检查(与 TS `AppController` 对齐)。
//! - `GET /api/_ping`  — 受保护的探针(Phase 0 内部使用)。

use crate::error::Json;
use serde_json::{json, Value};

pub async fn ping() -> Json<Value> {
    Json(json!({ "ok": true }))
}

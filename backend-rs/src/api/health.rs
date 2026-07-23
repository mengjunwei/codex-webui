//! 健康 / 探针端点。
//!
//! - `GET /api/status` — 受保护的健康检查(与 TS `AppController` 对齐)。
//! - `GET /api/_ping`  — 受保护的探针(Phase 0 内部使用)。

use crate::error::Json;
use serde_json::{json, Value};

/// 健康检查 / 探针。返回 `{ "ok": true }`。
///
/// 同时绑定到 `GET /api/status` 与 `GET /api/_ping`（此处文档只登记前者）。
#[utoipa::path(
    get,
    path = "/api/status",
    tag = "system",
    responses(
        (status = 200, description = "服务存活", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn ping() -> Json<Value> {
    Json(json!({ "ok": true }))
}

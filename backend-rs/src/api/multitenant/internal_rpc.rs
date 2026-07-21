//! 内网 RPC server(M4):把本节点的 codex 调用能力暴露给其它节点转发。
//!
//! 每个 backend 节点同时跑内网 RPC server 与 HTTP API(单二进制双角色)。
//! 独立 axum app,监听 `INTERNAL_RPC_HOST:INTERNAL_RPC_PORT`,路由 `/internal/*`。
//! 请求头 `x-internal-token` 校验(security.internal_rpc_token;启动期 config 强制 ≥32 字节,
//! 空值无法启动,故 check_internal_token 内空 token 分支实际不可达)。
//!
//! 处理来自其它节点的 codex 调用转发(turn/start/approve/fork 等),
//! threads 元数据双写由主节点在 PG 完成(共享库)。

use crate::error::{AppError, ErrorCode};
use crate::services::multitenant::replication::RolloutChunk;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
struct ThreadStartReq {
    #[serde(rename = "teamId")]
    #[allow(dead_code)]
    team_id: String,
    #[serde(rename = "createdBy")]
    #[allow(dead_code)]
    created_by: String,
    params: Value,
}

#[derive(Deserialize)]
struct TurnStartReq {
    #[serde(rename = "threadId")]
    thread_id: String,
    #[serde(rename = "teamId")]
    #[allow(dead_code)]
    team_id: String,
    params: Value,
}

#[derive(Deserialize)]
struct EvictReq {
    #[serde(rename = "teamId")]
    #[allow(dead_code)]
    team_id: String,
}

/// 纯函数:校验 x-internal-token(恒定时间比较)。供 layer 与单测复用。
fn check_internal_token(expected: &[u8], headers: &axum::http::HeaderMap) -> Result<(), AppError> {
    if expected.is_empty() {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "internal token not configured".into(),
            None,
        ));
    }
    let got = headers
        .get("x-internal-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .as_bytes();
    // 恒定时间比较:防时序攻击。
    if got.len() != expected.len() || !bool::from(got.ct_eq(expected)) {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "invalid internal token".into(),
            None,
        ));
    }
    Ok(())
}

/// axum middleware:整层强制 x-internal-token 校验。
pub async fn require_internal_token_layer(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    check_internal_token(state.internal_token.as_bytes(), &headers)?;
    Ok(next.run(req).await)
}

/// 构建 worker 内网 RPC router(独立监听端口,与前端 axum 分离)。
/// 整层挂 require_internal_token_layer,所有 /internal/* 路由强制 token 校验。
pub fn build_internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/thread/start", post(thread_start))
        .route("/internal/thread/invoke", post(thread_invoke))
        .route("/internal/turn/start", post(turn_start))
        .route("/internal/evict", post(evict))
        .route("/internal/approval/respond", post(approval_respond))
        .route("/internal/replicate", post(replicate_receive))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_internal_token_layer,
        ))
        .with_state(state)
}

async fn thread_start(
    State(state): State<AppState>,
    Json(req): Json<ThreadStartReq>,
) -> Result<Json<Value>, AppError> {
    metrics::counter!("internal_thread_start_total").increment(1);
    // 对齐 mt_create_thread 的本地参数:这两个参数确保 codex 持久化 rollout。
    let mut params = req.params;
    if let Value::Object(ref mut m) = params {
        m.entry("experimentalRawEvents".to_string()).or_insert(Value::Bool(false));
        m.entry("persistExtendedHistory".to_string()).or_insert(Value::Bool(true));
    }
    let resp = state
        .codex
        .request("thread/start", Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?;
    // PG 缓存响应供任意 owner 后续 thread/resume 复用(集群:副本转发 RPC 到 owner)。
    let thread_id = resp
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .or_else(|| resp.get("threadId").and_then(Value::as_str))
        .or_else(|| resp.get("id").and_then(Value::as_str));
    if let Some(tid) = thread_id {
        let _ = crate::services::multitenant::resume_cache::put_cached_resume(
            &state.db, tid, &resp,
        )
        .await;
    }
    Ok(Json(resp))
}

async fn turn_start(
    State(state): State<AppState>,
    Json(req): Json<TurnStartReq>,
) -> Result<Json<Value>, AppError> {
    metrics::counter!("internal_turn_start_total").increment(1);
    let mut params = req.params;
    if let Value::Object(ref mut m) = params {
        m.entry("threadId").or_insert(Value::String(req.thread_id.clone()));
    }
    let resp = state
        .codex
        .request("turn/start", Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex turn/start: {e}")))?;
    Ok(Json(resp))
}

async fn evict(
    State(_state): State<AppState>,
    Json(_req): Json<EvictReq>,
) -> Result<StatusCode, AppError> {
    // 单进程统一代理模式下 codex key 由全局 auth.json 管理,无需 per-team evict。
    // 保留 handler + 路由以兼容旧副本的转发请求,Task 3 一并清理。
    Ok(StatusCode::NO_CONTENT)
}

/// 副本:接收主节点推送的 rollout 增量,写入本地全局 CODEX_HOME(per-session 文件)。
async fn replicate_receive(
    State(state): State<AppState>,
    Json(chunk): Json<RolloutChunk>,
) -> Result<StatusCode, AppError> {
    crate::services::multitenant::replication::receive_rollout(&chunk, &state.codex_home).await?;
    metrics::counter!("replication_received_total").increment(1);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ApprovalRespondReq {
    #[serde(rename = "teamId")]
    #[allow(dead_code)]
    team_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    approved: bool,
    result: Option<Value>,
}

/// 响应审批(其它节点转发而来):把决定回传到该 team 的 codex 进程。
async fn approval_respond(
    State(state): State<AppState>,
    Json(req): Json<ApprovalRespondReq>,
) -> Result<StatusCode, AppError> {
    metrics::counter!("internal_approval_respond_total").increment(1);
    let id_val = parse_req_id(&req.request_id);
    let ok = if let Some(client) = state.codex.client().await {
        if req.approved {
            client
                .respond_to_server_request(
                    id_val,
                    req.result.unwrap_or(Value::Object(Default::default())),
                )
                .is_ok()
        } else {
            client
                .respond_to_server_request_with_error(id_val, -32000, "denied by user")
                .is_ok()
        }
    } else {
        false
    };
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::internal("respond to codex failed".into()))
    }
}

fn parse_req_id(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        Value::Number(serde_json::Number::from(n))
    } else {
        Value::String(s.to_string())
    }
}

#[derive(Deserialize)]
struct InvokeReq {
    #[serde(rename = "teamId")]
    #[allow(dead_code)]
    team_id: String,
    #[serde(rename = "threadId")]
    thread_id: String,
    method: String,
    params: Option<Value>,
}

/// 通用 codex 会话方法执行(fork/rollback/resume 等,其它节点转发而来)。
///
/// thread/resume:读 PG cache 作兜底(codex 失败时返回),不短路 —— 仍调 codex
/// 确保 owner 进程内存持有 thread(进程重启后需重新加载)。
async fn thread_invoke(
    State(state): State<AppState>,
    Json(req): Json<InvokeReq>,
) -> Result<Json<Value>, AppError> {
    let cache_fallback: Option<Value> = if req.method == "thread/resume" {
        crate::services::multitenant::resume_cache::get_cached_resume(&state.db, &req.thread_id).await
    } else {
        None
    };
    let mut params = req.params.unwrap_or(Value::Object(Default::default()));
    if let Value::Object(ref mut m) = params {
        m.entry("threadId").or_insert(Value::String(req.thread_id.clone()));
        if req.method == "thread/resume" {
            m.entry("persistExtendedHistory".to_string()).or_insert(Value::Bool(true));
        }
    }
    let resp = match state
        .codex
        .request(&req.method, Some(params))
        .await
    {
        Ok(v) => v,
        Err(crate::codex::jsonrpc::RpcError::ServerError { code: -32600, .. })
            if req.method == "thread/resume" && cache_fallback.is_some() =>
        {
            tracing::warn!(thread_id = %req.thread_id, "internal thread/resume -32600, serving cached fallback");
            cache_fallback.unwrap()
        }
        Err(e) => return Err(AppError::internal(format!("codex {}: {e}", req.method))),
    };
    if req.method == "thread/resume" {
        let _ = crate::services::multitenant::resume_cache::put_cached_resume(
            &state.db, &req.thread_id, &resp,
        )
        .await;
    }
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn mk_headers(token: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert("x-internal-token", t.parse().unwrap());
        }
        h
    }

    const EXPECTED: &[u8] = b"0123456789abcdef0123456789abcdef"; // 32 字节

    #[test]
    fn check_internal_token_correct_passes() {
        let h = mk_headers(Some(std::str::from_utf8(EXPECTED).unwrap()));
        assert!(check_internal_token(EXPECTED, &h).is_ok());
    }

    #[test]
    fn check_internal_token_wrong_rejected() {
        // 等长但内容不同,验证不是仅比长度
        let h = mk_headers(Some("9999456789abcdef0123456789abcdef"));
        assert!(check_internal_token(EXPECTED, &h).is_err());
    }

    #[test]
    fn check_internal_token_missing_header_rejected() {
        let h = mk_headers(None);
        assert!(check_internal_token(EXPECTED, &h).is_err());
    }

    #[test]
    fn check_internal_token_empty_expected_rejected() {
        let h = mk_headers(Some("anything"));
        assert!(check_internal_token(b"", &h).is_err());
    }
}

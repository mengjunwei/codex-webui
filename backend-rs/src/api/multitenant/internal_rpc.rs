//! worker 内网 RPC server(M4):暴露 codex 调用给 ingress 转发。
//!
//! 独立 axum app,监听 `INTERNAL_RPC_HOST:INTERNAL_RPC_PORT`,路由 `/internal/*`。
//! 请求头 `x-internal-token` 校验(`WORKER_RPC_TOKEN`;未配置则不校验 —— 仅限内网部署)。
//!
//! worker 只负责跑 codex 并返回结果;threads 元数据双写由 ingress 在 PG 完成(共享库)。

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

#[derive(Deserialize)]
struct ThreadStartReq {
    #[serde(rename = "teamId")]
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
    team_id: String,
    params: Value,
}

#[derive(Deserialize)]
struct EvictReq {
    #[serde(rename = "teamId")]
    team_id: String,
}

async fn require_internal_token(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<(), AppError> {
    if let Some(tok) = &state.internal_token {
        let got = headers
            .get("x-internal-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if got != tok {
            return Err(AppError::business(
                ErrorCode::HttpForbidden,
                StatusCode::FORBIDDEN,
                "invalid internal token".into(),
                None,
            ));
        }
    }
    Ok(())
}

/// 构建 worker 内网 RPC router(独立监听端口,与前端 axum 分离)。
pub fn build_internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/thread/start", post(thread_start))
        .route("/internal/thread/invoke", post(thread_invoke))
        .route("/internal/turn/start", post(turn_start))
        .route("/internal/evict", post(evict))
        .route("/internal/approval/respond", post(approval_respond))
        .route("/internal/replicate", post(replicate_receive))
        .with_state(state)
}

async fn thread_start(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ThreadStartReq>,
) -> Result<Json<Value>, AppError> {
    require_internal_token(&state, &headers).await?;
    metrics::counter!("internal_thread_start_total").increment(1);
    let lease = state
        .mt_team_codex
        .client_for(&req.team_id, &state.db, &state.mt_master_key)
        .await?;
    let resp = lease
        .client()
        .request("thread/start", Some(req.params))
        .await
        .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?;
    Ok(Json(resp))
}

async fn turn_start(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<TurnStartReq>,
) -> Result<Json<Value>, AppError> {
    require_internal_token(&state, &headers).await?;
    metrics::counter!("internal_turn_start_total").increment(1);
    let lease = state
        .mt_team_codex
        .client_for(&req.team_id, &state.db, &state.mt_master_key)
        .await?;
    let mut params = req.params;
    if let Value::Object(ref mut m) = params {
        m.entry("threadId").or_insert(Value::String(req.thread_id.clone()));
    }
    let resp = lease
        .client()
        .request("turn/start", Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex turn/start: {e}")))?;
    Ok(Json(resp))
}

async fn evict(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<EvictReq>,
) -> Result<StatusCode, AppError> {
    require_internal_token(&state, &headers).await?;
    state.mt_team_codex.evict(&req.team_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// 副本:接收主节点推送的 rollout 增量,写入本地全局 CODEX_HOME(per-session 文件)。
async fn replicate_receive(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(chunk): Json<RolloutChunk>,
) -> Result<StatusCode, AppError> {
    require_internal_token(&state, &headers).await?;
    crate::services::multitenant::replication::receive_rollout(&chunk, &state.codex_home).await?;
    metrics::counter!("replication_received_total").increment(1);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ApprovalRespondReq {
    #[serde(rename = "teamId")]
    team_id: String,
    #[serde(rename = "requestId")]
    request_id: String,
    approved: bool,
    result: Option<Value>,
}

/// 响应审批(ingress 转发而来):把决定回传到该 team 的 codex 进程。
async fn approval_respond(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ApprovalRespondReq>,
) -> Result<StatusCode, AppError> {
    require_internal_token(&state, &headers).await?;
    metrics::counter!("internal_approval_respond_total").increment(1);
    let lease = state
        .mt_team_codex
        .client_for(&req.team_id, &state.db, &state.mt_master_key)
        .await?;
    let id_val = parse_req_id(&req.request_id);
    let ok = if req.approved {
        lease
            .client()
            .respond_to_server_request(
                id_val,
                req.result.unwrap_or(Value::Object(Default::default())),
            )
            .is_ok()
    } else {
        lease
            .client()
            .respond_to_server_request_with_error(id_val, -32000, "denied by user")
            .is_ok()
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
    team_id: String,
    #[serde(rename = "threadId")]
    thread_id: String,
    method: String,
    params: Option<Value>,
}

/// 通用 codex 会话方法执行(fork/rollback/resume 等,ingress 转发而来)。
async fn thread_invoke(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<InvokeReq>,
) -> Result<Json<Value>, AppError> {
    require_internal_token(&state, &headers).await?;
    let lease = state
        .mt_team_codex
        .client_for(&req.team_id, &state.db, &state.mt_master_key)
        .await?;
    let mut params = req.params.unwrap_or(Value::Object(Default::default()));
    if let Value::Object(ref mut m) = params {
        m.entry("threadId").or_insert(Value::String(req.thread_id.clone()));
    }
    let resp = lease
        .client()
        .request(&req.method, Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex {}: {e}", req.method)))?;
    Ok(Json(resp))
}

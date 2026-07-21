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
use crate::services::multitenant::replication::{safe_join, RolloutChunk};
use crate::services::workspace::file_sync::{ChangeType, FileChange};
use crate::services::workspace::{ensure_thread_workspace, thread_workspace_path};
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
        .route("/internal/approval/respond", post(approval_respond))
        .route("/internal/replicate", post(replicate_receive))
        .route("/internal/filesync", post(receive_files))
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
    // C1:系统 thread_id(入口节点预生成,放 params.threadId)—— 用于 session_replicas /
    // sticky / active_rollout 的 key(全系统统一用该 id)。
    let sys_thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .map(String::from);
    let resp = state
        .codex
        .request("thread/start", Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?;

    // C1:target 侧自登记。转发场景下 codex 进程跑在本节点,本节点 local_node_id()=target,
    //    在此登记 primary_node=target 才正确(入口节点不再无条件登记,避免把 primary 登记成入口)。
    //    codex thread 已成功启动,登记失败改 best-effort(不中断 RPC,否则入口侧重试生成新 id 留孤儿)。
    if let Some(tid) = &sys_thread_id {
        if let Err(e) = crate::services::multitenant::replication::get_or_assign(
            &state.db,
            tid,
            state.cluster.as_ref(),
        )
        .await
        {
            tracing::error!(error = %e, thread_id = %tid, "target-side get_or_assign failed (best-effort)");
        }
        // sticky 共享 Redis,在 target 侧绑定 → 后续 turn 经入口路由时 sticky 命中回到本节点。
        if let Err(e) = state.sticky.bind(tid, &state.node_id, 3600).await {
            tracing::error!(error = %e, thread_id = %tid, "target-side sticky.bind failed (best-effort)");
        }
    } else {
        tracing::warn!("internal thread_start missing params.threadId, skip target-side registration");
    }

    // PG 缓存响应供任意 owner 后续 thread/resume 复用 —— key 用系统 thread_id
    // (全系统 resume 查询按系统 thread_id;codex 尊重 threadId 时 codex_tid==sys_thread_id)。
    if let Some(tid) = &sys_thread_id {
        let _ = crate::services::multitenant::resume_cache::put_cached_resume(
            &state.db, tid, &resp,
        )
        .await;
    }

    // C1:关联 rollout 文件 + 复制增量到副本(active_rollout key = 系统 thread_id)。
    //    find_rollout 用系统 thread_id 查找(C2 会改为 codex 响应 tid 以兼容 codex 忽略 threadId 的场景)。
    if let Some(tid) = &sys_thread_id {
        if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
            &state.codex_home,
            tid,
        )
        .await
        {
            state.active_rollout.lock().await.insert(tid.clone(), p);
        }
        let _ = crate::services::multitenant::replication::replicate_thread_rollout(
            &state.db,
            tid,
            &state.codex_home,
            state.cluster.as_ref(),
            state.mt_redis.as_ref(),
            &state.worker_rpc,
            &state.active_rollout,
            &state.local_offsets,
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

/// 副本:接收主节点推送的 rollout 增量,写入本地全局 CODEX_HOME(per-session 文件)。
async fn replicate_receive(
    State(state): State<AppState>,
    Json(chunk): Json<RolloutChunk>,
) -> Result<StatusCode, AppError> {
    crate::services::multitenant::replication::receive_rollout(&chunk, &state.codex_home).await?;
    metrics::counter!("replication_received_total").increment(1);
    Ok(StatusCode::NO_CONTENT)
}

/// 副本:接收主节点推送的 per-thread workspace 文件变更,写入本地 thread workspace。
///
/// 每条变更:ensure_thread_workspace(首次接收建目录)→ safe_join 校验 relative_path
/// 防穿越 → Create/Modify 覆盖写(parent 目录 create_dir_all)→ Delete 删文件。
async fn receive_files(
    State(state): State<AppState>,
    Json(changes): Json<Vec<FileChange>>,
) -> Result<StatusCode, AppError> {
    for change in changes {
        // 确保 thread workspace 存在(副本首次接收)。
        let _ = ensure_thread_workspace(&state, &change.thread_id).await;
        let ws = thread_workspace_path(&state.workspace_root, &change.thread_id);
        // 路径安全:过 replication::safe_join 校验相对路径(防穿越)。
        let path = safe_join(&ws, &change.relative_path).await?;
        match change.change_type {
            ChangeType::Create | ChangeType::Modify => {
                if let Some(content) = &change.content {
                    if let Some(p) = path.parent() {
                        tokio::fs::create_dir_all(p)
                            .await
                            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
                    }
                    tokio::fs::write(&path, content)
                        .await
                        .map_err(|e| AppError::internal(format!("write {}: {e}", path.display())))?;
                }
            }
            ChangeType::Delete => {
                if path.exists() {
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
        }
        metrics::counter!("filesync_received_total").increment(1);
    }
    Ok(StatusCode::OK)
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

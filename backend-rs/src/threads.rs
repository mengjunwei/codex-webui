//! Threads + turns REST 端点 — codex app-server 代理。
//!
//! 与 `threads.controller.ts` + `threads.service.ts` 对齐。
//!
//! 简单代理:list / loaded-list / read / resume / archive / unarchive /
//! compact / fork / rollback / name-set / interrupt / start-thread。
//! start-turn / steer:结构化输入校验 + 工作区路径安全边界(`mention`/`localImage`/
//! 文本内联 @mention 路径经 `files::resolve_safe_path` 校验,对齐
//! FilesService.resolveSafePath / ChatUploadService.resolveStoredUploadPath)。

use crate::codex::RpcError;
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
};
use crate::error::Json;
use serde::Deserialize;
use serde_json::{json, Value};

fn map_rpc(e: RpcError) -> AppError {
    AppError::internal(format!("codex: {e}"))
}
fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

// ── POST /threads(创建)───────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateThreadBody {
    pub model: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: Option<Value>,
}

#[utoipa::path(
    post,
    path = "/api/threads",
    tag = "threads",
    request_body = CreateThreadBody,
    responses(
        (status = 201, description = "线程已创建（codex thread/start 透传）", body = ThreadStartResponse),
        (status = 400, description = "参数非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn create_thread(
    State(state): State<AppState>,
    Json(body): Json<CreateThreadBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let mut params = serde_json::Map::new();
    if let Some(m) = body.model {
        params.insert("model".into(), Value::String(m));
    }
    if let Some(c) = body.cwd {
        params.insert("cwd".into(), Value::String(c));
    }
    if let Some(a) = body.approval_policy {
        params.insert("approvalPolicy".into(), a);
    }
    params.insert("experimentalRawEvents".into(), Value::Bool(false));
    params.insert("persistExtendedHistory".into(), Value::Bool(true));
    let result = state
        .codex
        .request("thread/start", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    // H6：标记已 resume（对齐 TS resumeRegistry.markResumed + cacheResponse）。
    if let Some(thread) = result.get("thread").and_then(|t| t.get("id")).and_then(Value::as_str) {
        state.resume_registry.mark_resumed(thread, result.clone());
    }
    Ok((StatusCode::CREATED, Json(result)))
}

// ── GET /threads(列表)─────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListThreadsQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    pub archived: Option<String>,
    #[serde(rename = "searchTerm")]
    pub search_term: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "sortKey")]
    pub sort_key: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/threads",
    tag = "threads",
    params(ListThreadsQuery),
    responses(
        (status = 200, description = "线程列表（codex thread/list 透传）", body = ThreadListResponse),
        (status = 400, description = "limit/sortKey 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn list_threads(
    State(state): State<AppState>,
    Query(q): Query<ListThreadsQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = parse_positive_limit(q.limit.as_deref())?;
    if let Some(sk) = q.sort_key.as_deref() {
        if sk != "created_at" && sk != "updated_at" {
            return Err(bad_request(
                ErrorCode::ThreadsInvalidSortKey,
                "sortKey must be created_at or updated_at",
            ));
        }
    }
    let mut params = serde_json::Map::new();
    if let Some(c) = q.cursor {
        params.insert("cursor".into(), Value::String(c));
    }
    if let Some(l) = limit {
        params.insert("limit".into(), Value::Number(l.into()));
    }
    if q.archived.as_deref() == Some("true") {
        params.insert("archived".into(), Value::Bool(true));
    }
    if let Some(s) = q.search_term {
        params.insert("searchTerm".into(), Value::String(s));
    }
    if let Some(c) = q.cwd {
        params.insert("cwd".into(), Value::String(c));
    }
    if let Some(sk) = q.sort_key {
        params.insert("sortKey".into(), Value::String(sk));
    }
    // 空数组 = 所有 provider(与 TS controller 注释对齐)。
    params.insert("modelProviders".into(), Value::Array(vec![]));
    let result = state
        .codex
        .request("thread/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ── GET /threads/loaded ──────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LoadedQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/threads/loaded",
    tag = "threads",
    params(LoadedQuery),
    responses(
        (status = 200, description = "已加载线程列表（codex thread/loaded/list 透传）", body = ThreadLoadedListResponse),
        (status = 400, description = "limit 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn list_loaded_threads(
    State(state): State<AppState>,
    Query(q): Query<LoadedQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = parse_positive_limit(q.limit.as_deref())?;
    let mut params = serde_json::Map::new();
    if let Some(c) = q.cursor {
        params.insert("cursor".into(), Value::String(c));
    }
    if let Some(l) = limit {
        params.insert("limit".into(), Value::Number(l.into()));
    }
    let result = state
        .codex
        .request("thread/loaded/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ── GET /threads/:threadId(读取)───────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReadQuery {
    #[serde(rename = "includeTurns")]
    pub include_turns: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/threads/{threadId}",
    tag = "threads",
    params(
        ("threadId" = String, Path, description = "线程 ID"),
        ReadQuery,
    ),
    responses(
        (status = 200, description = "线程详情（codex thread/read 透传；未具现化时 turns 为空）", body = ThreadReadResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<Value>, AppError> {
    let include_turns = q.include_turns.as_deref() == Some("true");
    let result = state
        .codex
        .request(
            "thread/read",
            Some(json!({ "threadId": thread_id, "includeTurns": include_turns })),
        )
        .await;
    match result {
        Ok(v) => Ok(Json(v)),
        Err(e) if include_turns && is_not_materialized(&e) => {
            // 不带 turns 重试;未具现化的线程本来也没有 turns。
            let retry = state
                .codex
                .request(
                    "thread/read",
                    Some(json!({ "threadId": thread_id, "includeTurns": false })),
                )
                .await
                .map_err(map_rpc)?;
            let mut out = retry;
            if let Value::Object(ref mut m) = out {
                m.get_mut("thread")
                    .and_then(|t| t.as_object_mut())
                    .map(|t| t.insert("turns".into(), Value::Array(vec![])));
            }
            Ok(Json(out))
        }
        Err(e) => Err(map_rpc(e)),
    }
}

/// 与 `thread-errors.ts:isNotMaterializedError` 对齐 — 消息包含
/// "not materialized"(不区分大小写)。
fn is_not_materialized(e: &RpcError) -> bool {
    match e {
        RpcError::ServerError { message, .. } => {
            message.to_ascii_lowercase().contains("not materialized")
        }
        _ => false,
    }
}

// ── POST /threads/:threadId/resume ───────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/resume",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 201, description = "线程已恢复（codex thread/resume 透传；含并发去重缓存）", body = ThreadStartResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn resume_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    // ensure_resumed：缓存命中 / 并发 in-flight 去重（对齐 TS ensureResumed），
    // 避免并发或重复请求对非幂等的 thread/resume 多次调用。
    let codex = state.codex.clone();
    let (result, from_cache) = state
        .resume_registry
        .ensure_resumed(&thread_id, move |tid| async move {
            codex
                .request("thread/resume", Some(json!({ "threadId": tid, "persistExtendedHistory": true })))
                .await
                .map_err(map_rpc)
        })
        .await?;
    // 缓存命中（已 resume 过）→ readAsResume 重读最新 thread；新 RPC 则直接返回。
    if from_cache {
        let merged = read_as_resume(&state, &thread_id, result).await;
        Ok((StatusCode::CREATED, Json(merged)))
    } else {
        Ok((StatusCode::CREATED, Json(result)))
    }
}

/// readAsResume（对齐 TS `readAsResume`）：缓存命中时，重新 `thread/read` 取最新
/// `thread` 字段（含最新 turns）合并进缓存响应；resolved settings（model/approvalPolicy
/// 等顶层字段）保留缓存的值。读取失败则回退缓存，保证至少返回有效响应。
async fn read_as_resume(state: &AppState, thread_id: &str, cached: Value) -> Value {
    let fresh_opt: Option<Value> = match state
        .codex
        .request("thread/read", Some(json!({ "threadId": thread_id, "includeTurns": true })))
        .await
    {
        Ok(f) => Some(f),
        Err(e) if is_not_materialized(&e) => {
            match state
                .codex
                .request("thread/read", Some(json!({ "threadId": thread_id, "includeTurns": false })))
                .await
            {
                Ok(mut f) => {
                    if let Value::Object(ref mut m) = f {
                        m.get_mut("thread")
                            .and_then(|t| t.as_object_mut())
                            .map(|t| t.insert("turns".into(), Value::Array(vec![])));
                    }
                    Some(f)
                }
                Err(_) => None,
            }
        }
        Err(_) => None,
    };
    match fresh_opt {
        Some(fresh) => {
            let mut merged = cached;
            if let (Some(fresh_thread), Value::Object(ref mut m)) =
                (fresh.get("thread").cloned(), &mut merged)
            {
                m.insert("thread".into(), fresh_thread);
            }
            merged
        }
        None => cached,
    }
}

// ── POST /threads/:threadId/turns(开始 turn)──────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct StartTurnBody {
    pub input: Value,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/turns",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    request_body = StartTurnBody,
    responses(
        (status = 201, description = "turn 已启动（codex turn/start 透传）", body = TurnStartResponse),
        (status = 400, description = "input/model/effort 非法或 mention 路径越界", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn start_turn(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(body): Json<StartTurnBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let input = validate_turn_input(&body.input, &state).await?;
    let mut params = serde_json::Map::new();
    params.insert("threadId".into(), Value::String(thread_id));
    params.insert("input".into(), input);
    if let Some(m) = body.model.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        params.insert("model".into(), Value::String(m.into()));
    } else if body.model.is_some() {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidModel,
            "model must be a non-empty string",
        ));
    }
    if let Some(eff) = body.effort.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !REASONING_EFFORT_VALUES.contains(&eff) {
            return Err(bad_request(
                ErrorCode::ThreadsInvalidEffort,
                "Invalid reasoning effort",
            ));
        }
        params.insert("effort".into(), Value::String(eff.into()));
    } else if body.effort.is_some() {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidEffort,
            "Invalid reasoning effort",
        ));
    }
    let result = state
        .codex
        .request("turn/start", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok((StatusCode::CREATED, Json(result)))
}

// ── POST /threads/:threadId/turns/:turnId/steer ──────────────────────────────

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/turns/{turnId}/steer",
    tag = "threads",
    params(
        ("threadId" = String, Path, description = "线程 ID"),
        ("turnId" = String, Path, description = "目标 turn ID"),
    ),
    request_body = StartTurnBody,
    responses(
        (status = 201, description = "turn 已转向（codex turn/steer 透传）", body = TurnSteerResponse),
        (status = 400, description = "input 非法或 mention 路径越界", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn steer_turn(
    State(state): State<AppState>,
    Path((thread_id, turn_id)): Path<(String, String)>,
    Json(body): Json<StartTurnBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let input = validate_turn_input(&body.input, &state).await?;
    let params = json!({
        "threadId": thread_id,
        "expectedTurnId": turn_id,
        "input": input,
    });
    let result = state
        .codex
        .request("turn/steer", Some(params))
        .await
        .map_err(map_rpc)?;
    Ok((StatusCode::CREATED, Json(result)))
}

// ── POST /threads/:threadId/turns/:turnId/interrupt ──────────────────────────

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/turns/{turnId}/interrupt",
    tag = "threads",
    params(
        ("threadId" = String, Path, description = "线程 ID"),
        ("turnId" = String, Path, description = "要中断的 turn ID"),
    ),
    responses(
        (status = 201, description = "turn 已中断（codex turn/interrupt 透传）", body = crate::files::OkResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn interrupt_turn(
    State(state): State<AppState>,
    Path((thread_id, turn_id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    state
        .codex
        .request("turn/interrupt", Some(json!({ "threadId": thread_id, "turnId": turn_id })))
        .await
        .map_err(map_rpc)?;
    Ok((StatusCode::CREATED, Json(json!({ "ok": true }))))
}

// ── 归档 / 取消归档 / 压缩 / 分叉 / 回滚 / 重命名 ───────────────────

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/archive",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 204, description = "已归档"),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn archive_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("thread/archive", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    // H6：归档时清理 resume 注册表（对齐 TS resumeRegistry.forget）。
    state.resume_registry.forget(&thread_id);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/unarchive",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 201, description = "已取消归档（codex thread/unarchive 透传）", body = ThreadUnarchiveResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn unarchive_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let result = state
        .codex
        .request("thread/unarchive", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    Ok((StatusCode::CREATED, Json(result)))
}

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/compact",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 204, description = "压缩已启动"),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn compact_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("thread/compact/start", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/fork",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 201, description = "已分叉出新线程（codex thread/fork 透传）", body = ThreadStartResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn fork_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let result = state
        .codex
        .request(
            "thread/fork",
            Some(json!({ "threadId": thread_id, "persistExtendedHistory": true })),
        )
        .await
        .map_err(map_rpc)?;
    // H6：标记新 fork 的线程为已 resume（对齐 TS resumeRegistry.markResumed + cacheResponse）。
    if let Some(thread) = result.get("thread").and_then(|t| t.get("id")).and_then(Value::as_str) {
        state.resume_registry.mark_resumed(thread, result.clone());
    }
    Ok((StatusCode::CREATED, Json(result)))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RollbackBody {
    #[serde(rename = "numTurns")]
    pub num_turns: i64,
}

#[utoipa::path(
    post,
    path = "/api/threads/{threadId}/rollback",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    request_body = RollbackBody,
    responses(
        (status = 201, description = "已回滚（codex thread/rollback 透传）", body = ThreadRollbackResponse),
        (status = 400, description = "numTurns 非正整数", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn rollback_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if body.num_turns < 1 {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidRollbackTurns,
            "numTurns must be a positive integer",
        ));
    }
    let result = state
        .codex
        .request(
            "thread/rollback",
            Some(json!({ "threadId": thread_id, "numTurns": body.num_turns })),
        )
        .await
        .map_err(map_rpc)?;
    Ok((StatusCode::CREATED, Json(result)))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetNameBody {
    pub name: String,
}

#[utoipa::path(
    patch,
    path = "/api/threads/{threadId}/name",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    request_body = SetNameBody,
    responses(
        (status = 204, description = "名称已设置"),
        (status = 400, description = "name 为空", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn set_thread_name(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(body): Json<SetNameBody>,
) -> Result<StatusCode, AppError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidName,
            "name must be a non-empty string",
        ));
    }
    state
        .codex
        .request("thread/name/set", Some(json!({ "threadId": thread_id, "name": name })))
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── 输入校验(结构化 + 工作区路径安全)──────────────────────

const USER_INPUT_TYPES: &[&str] = &["text", "image", "localImage", "skill", "mention"];
const REASONING_EFFORT_VALUES: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// H6：线程 resume 注册表（按 generation 去重，对齐 TS ThreadResumeRegistryService）。
/// 线程 resume 注册表：缓存最近一次 resume/start/fork 的响应，按 generation 去重。
/// codex 重启（generation 变化）时通过 `advance_generation()` 清空缓存（对齐 TS
/// resumeRegistry 在 appServerReady 时按 generation 重建）。
///
/// ## 三个并发难题与对应解决方案
///
/// 1. **H7**：旧实现中条目无 generation，advance 与 auto-resume 跨任务调度顺序
///    无保证 → 旧 generation 的陈旧缓存可能命中新 generation 的 resume。
///    **修复**：条目绑定写入时的 generation；读侧按当前 generation 过滤。
///
/// 2. **TS 对齐**：并发 resume 同线程会触发非幂等的 `thread/resume` RPC 多次。
///    **修复**：per-key 锁槽（std Mutex 取槽 + tokio Mutex 跨 await），保证
///    同线程串行；不同线程并发安全。
///
/// 3. **T7**：锁槽无限增长。**修复**：`reap_inflight_slot` 仅在 `strong_count == 1`
///    时移除 —— 调用方先 drop 自己的 Arc clone，再检查 strong_count。
///
/// ## 数据布局
///
/// - `generation`：当前 generation（启动时为 0）。
/// - `entries`：HashMap<thread_id, (generation, response)>。
/// - `inflight`：HashMap<thread_id, Arc<tokio::Mutex<()>>>。
#[derive(Debug, Default)]
pub struct ThreadResumeRegistry {
    generation: std::sync::Mutex<u64>,
    /// 条目携带写入时的 generation；读侧按当前 generation 过滤，根除
    /// advance_generation 与 auto-resume 跨任务调度的时序竞态（H7）。
    entries: std::sync::Mutex<std::collections::HashMap<String, (u64, serde_json::Value)>>,
    /// per-key in-flight 锁槽：并发 resume 串行化（对齐 TS resumeRegistry.inFlight）。
    /// HashMap 用 std Mutex（取槽短暂），每个槽是 tokio Mutex（跨 RPC await 持有）。
    inflight: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl ThreadResumeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次 resume/start/fork 的响应（缓存供后续重复调用复用）。
    /// 条目绑定当前 generation，跨 generation 读侧自动失效。
    pub fn mark_resumed(&self, thread_id: &str, response: serde_json::Value) {
        let g = *self.generation.lock().unwrap();
        self.entries
            .lock()
            .unwrap()
            .insert(thread_id.to_string(), (g, response));
    }

    /// 返回缓存响应（仅当条目 generation 与当前 generation 一致时命中）。
    pub fn get_cached(&self, thread_id: &str) -> Option<serde_json::Value> {
        let g = *self.generation.lock().unwrap();
        self.entries
            .lock()
            .unwrap()
            .get(thread_id)
            .filter(|(gen, _)| *gen == g)
            .map(|(_, v)| v.clone())
    }

    pub fn forget(&self, thread_id: &str) {
        self.entries.lock().unwrap().remove(thread_id);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// generation 推进：generation 变化时清空缓存响应。
    /// 注意：不再清空 inflight —— clear 会打断进行中的 resume，使 per-key 互斥出现破洞；
    /// 孤立锁槽改由 ensure_resumed 释放 guard 后按 strong_count 回收。
    /// （即使本调用未被及时调度，get_cached 的 generation 过滤也能保证不命中陈旧缓存。）
    pub fn advance_generation(&self, new_generation: u64) {
        let mut g = self.generation.lock().unwrap();
        if *g != new_generation {
            *g = new_generation;
            self.entries.lock().unwrap().clear();
        }
    }

    /// ensure_resumed：缓存命中直接返回；否则在 per-key 锁内串行化并发 resume，
    /// 锁内重检缓存（前一个 in-flight 可能已完成），仍未命中才执行 RPC 并写缓存。
    /// 返回 `(响应, 是否来自缓存)`。对齐 TS `resumeRegistry` 的 inFlight + resumed 去重，
    /// 避免对非幂等的 `thread/resume` 并发重复调用。
    ///
    /// ## 完整流程
    ///
    /// 1. **快路径**：`get_cached` 命中（generation 过滤）→ 直接返回 `(v, true)`。
    /// 2. **取锁槽**：std Mutex 短持有获取或创建 per-key tokio Mutex。
    /// 3. **异步加锁**：`lock.await` —— 此处跨 await，tokio Mutex 不会被 worker 线程独占。
    /// 4. **锁内重检**：可能前一个 in-flight 已完成 → 命中后释放锁槽再返回。
    /// 5. **执行 RPC**：失败路径也必须释放锁槽 + reap（否则泄漏）。
    /// 6. **写缓存**：成功后 `mark_resumed`。
    /// 7. **释放 + reap**：先 drop 自己的 guard 与 Arc clone，再 `reap_inflight_slot`
    ///    —— 检查 `strong_count == 1` 时移除锁槽。
    ///
    /// ## 为什么 `reap_inflight_slot` 在 drop 之后
    ///
    /// 若 drop 之前检查 strong_count，本地 Arc + HashMap 里的 Arc 至少 2，
    /// 永远不会被回收。drop 之后本表成为唯一持有者才能正确判定孤立。
    pub async fn ensure_resumed<F, Fut>(
        &self,
        thread_id: &str,
        f: F,
    ) -> Result<(serde_json::Value, bool), AppError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<serde_json::Value, AppError>>,
    {
        // 快路径：缓存命中（未获取锁槽，无回收义务）。
        if let Some(v) = self.get_cached(thread_id) {
            return Ok((v, true));
        }
        // per-key 锁槽（std Mutex 短暂持有取槽，tokio Mutex 跨 RPC await）。
        let key = thread_id.to_string();
        let lock = {
            let mut guards = self.inflight.lock().unwrap();
            guards
                .entry(key.clone())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // T7：锁内重检命中 / RPC 失败 / 成功 三条路径都要回收锁槽，否则并发命中（最常见）
        // 与失败路径泄漏。提取 reap_inflight_slot，先 drop 自己的 guard + Arc clone 再检查 strong_count。
        if let Some(v) = self.get_cached(thread_id) {
            drop(_guard);
            drop(lock);
            self.reap_inflight_slot(&key);
            return Ok((v, true));
        }
        let result = match f(key.clone()).await {
            Ok(r) => r,
            Err(e) => {
                drop(_guard);
                drop(lock);
                self.reap_inflight_slot(&key);
                return Err(e);
            }
        };
        self.mark_resumed(&key, result.clone());
        drop(_guard);
        drop(lock);
        self.reap_inflight_slot(&key);
        Ok((result, false))
    }
}

impl ThreadResumeRegistry {
    /// 回收孤立的 in-flight 锁槽：仅当本表是唯一持有者（strong_count==1）时移除。
    /// 调用前必须已 drop 调用方自己的 Arc clone，否则计数恒 ≥2。
    fn reap_inflight_slot(&self, key: &str) {
        let mut guards = self.inflight.lock().unwrap();
        if let Some(arc) = guards.get(key) {
            if std::sync::Arc::strong_count(arc) == 1 {
                guards.remove(key);
            }
        }
    }
}

/// 校验可辨识的 UserInput 数组。返回已校验的数组。
/// mention/localImage/文本内联 @mention 路径经 `files::resolve_safe_path` 做
/// 工作区安全边界校验（对齐 FilesService.resolveSafePath /
/// ChatUploadService.resolveStoredUploadPath）。
/// H5：校验 discriminated UserInput 数组。
/// H4：mention / localImage 路径经 resolve_safe_path 校验。
/// 提取文本中内联的绝对路径 @mention（对齐 TS `validateInlineTextMentions` 的正则
/// `/(^|\s)@(\/(?:\\ |[^\s])+)/g`）：前导为行首或空白，路径以 `/` 开头，
/// 其中 `\ ` 转义为真实空格。返回去重后的路径列表（含前导 `/`）。
///
/// ## 字符级扫描（而非正则）
///
/// Rust 的 `regex` crate 不支持"反斜杠转义空格"的灵活分组；改为手动字符扫描：
///
/// - 前导条件：`i == 0` 或 `chars[i - 1]` 是空白。
/// - 起始：`@` + `/`。
/// - 路径字符：跳过 `\ `（写入真实空格），遇其他空白终止。
///
/// 去重通过 `HashSet` 跟踪，确保同一路径多次出现也只返回一次。
fn extract_inline_mentions(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while i < n {
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if at_boundary && chars[i] == '@' && i + 1 < n && chars[i + 1] == '/' {
            let mut j = i + 1; // 从前导 '/' 开始捕获
            let mut path = String::new();
            while j < n {
                if chars[j] == '\\' && j + 1 < n && chars[j + 1] == ' ' {
                    path.push(' '); // `\ ` → 真实空格
                    j += 2;
                    continue;
                }
                if chars[j].is_whitespace() {
                    break;
                }
                path.push(chars[j]);
                j += 1;
            }
            i = j;
            if !path.is_empty() && seen.insert(path.clone()) {
                out.push(path);
            }
        } else {
            i += 1;
        }
    }
    out
}

async fn validate_turn_input(input: &Value, state: &AppState) -> Result<Value, AppError> {
    let arr = input.as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
        bad_request(
            ErrorCode::ThreadsInvalidInput,
            "input must be a non-empty array",
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        out.push(validate_input_item(item, i, state).await?);
    }
    Ok(Value::Array(out))
}

async fn validate_input_item(item: &Value, i: usize, state: &AppState) -> Result<Value, AppError> {
    let obj = item.as_object().ok_or_else(|| {
        bad_request_params(
            ErrorCode::ThreadsInvalidInputItem,
            format!("input[{i}] must be an object"),
            i,
        )
    })?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    match ty {
        "text" => {
            let text = req_string(obj, "text", i)?;
            // H7：校验文本中内联的绝对 @mention 路径（对齐 TS validateInlineTextMentions）。
            // 前端会把文件 mention 内联为 @/abs/path，后端仍是路径访问的安全边界。
            for mention in extract_inline_mentions(&text) {
                crate::files::resolve_safe_path(state, &mention)
                    .await
                    .map_err(|e| {
                        AppError::business(
                            ErrorCode::FilesPathOutsideWorkspace,
                            StatusCode::BAD_REQUEST,
                            format!("inline mention path validation failed: {e}"),
                            Some({
                                let mut p = std::collections::BTreeMap::new();
                                p.insert("index".to_string(), serde_json::Value::Number(i.into()));
                                p
                            }),
                        )
                    })?;
            }
            // H5：校验 text_elements（对齐 TS validateTextElements）——
            // 非数组拒绝；每个元素必须是对象；byteRange 必填且结构/范围有效；
            // placeholder 规整为 string|null。输出规整化后的数组。
            let text_elements = match obj.get("text_elements") {
                None | Some(Value::Null) => Value::Array(vec![]),
                Some(Value::Array(arr)) => {
                    let text_byte_len = text.len();
                    let mut out = Vec::with_capacity(arr.len());
                    for (ei, elem) in arr.iter().enumerate() {
                        let elem_obj = elem.as_object().ok_or_else(|| {
                            bad_request_params(
                                ErrorCode::ThreadsInvalidInputField,
                                format!("input[{i}].text_elements[{ei}] must be an object"),
                                i,
                            )
                        })?;
                        let br = elem_obj.get("byteRange").ok_or_else(|| {
                            bad_request_params(
                                ErrorCode::ThreadsInvalidInputField,
                                format!("input[{i}].text_elements[{ei}].byteRange is required"),
                                i,
                            )
                        })?;
                        let br_obj = br.as_object().ok_or_else(|| {
                            bad_request_params(
                                ErrorCode::ThreadsInvalidInputField,
                                format!("input[{i}].text_elements[{ei}].byteRange is invalid"),
                                i,
                            )
                        })?;
                        let start = br_obj.get("start").and_then(Value::as_i64);
                        let end = br_obj.get("end").and_then(Value::as_i64);
                        match (start, end) {
                            (Some(s), Some(e))
                                if s >= 0 && e >= s && (e as usize) <= text_byte_len => {}
                            _ => {
                                return Err(bad_request_params(
                                    ErrorCode::ThreadsInvalidInputField,
                                    format!("input[{i}].text_elements[{ei}].byteRange is invalid"),
                                    i,
                                ));
                            }
                        }
                        let placeholder = match elem_obj.get("placeholder") {
                            Some(Value::String(s)) => Value::String(s.clone()),
                            Some(Value::Null) | None => Value::Null,
                            Some(_) => {
                                return Err(bad_request_params(
                                    ErrorCode::ThreadsInvalidInputField,
                                    format!(
                                        "input[{i}].text_elements[{ei}].placeholder must be a string or null"
                                    ),
                                    i,
                                ));
                            }
                        };
                        let mut m = serde_json::Map::new();
                        m.insert("byteRange".into(), br.clone());
                        m.insert("placeholder".into(), placeholder);
                        out.push(Value::Object(m));
                    }
                    Value::Array(out)
                }
                Some(_) => {
                    return Err(bad_request_params(
                        ErrorCode::ThreadsInvalidInputField,
                        format!("input[{i}].text_elements must be an array"),
                        i,
                    ));
                }
            };
            Ok(json!({ "type": "text", "text": text, "text_elements": text_elements }))
        }
        "image" => {
            let url = req_string_trimmed(obj, "url", i)?;
            if !is_http_url(&url) {
                return Err(bad_request_params(
                    ErrorCode::ThreadsInvalidInputUrl,
                    format!("input[{i}].url must use http or https"),
                    i,
                ));
            }
            Ok(json!({ "type": "image", "url": url }))
        }
        "localImage" => {
            // path 必须是非空字符串（对齐 TS validateLocalImageInput）。
            let path = match obj
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(p) => p.to_string(),
                None => {
                    return Err(bad_request_params(
                        ErrorCode::ChatImagePathRequired,
                        format!("input[{i}].path is required for localImage"),
                        i,
                    ));
                }
            };
            // 校验路径位于 chat 上传根目录内（对齐 TS ChatUploadService.resolveStoredUploadPath），
            // 仅允许引用通过 /chat/upload 上传的图片，而非整个工作区。
            crate::chat::resolve_stored_upload_path(&path).await?;
            Ok(json!({ "type": "localImage", "path": path }))
        }
        "skill" => {
            let name = req_string_trimmed(obj, "name", i)?;
            let path = req_string_trimmed(obj, "path", i)?;
            Ok(json!({ "type": "skill", "name": name, "path": path }))
        }
        "mention" => {
            let name = req_string_trimmed(obj, "name", i)?;
            let path = req_string_trimmed(obj, "path", i)?;
            // H4 修复：验证 mention 路径存在且在工作区内（对齐
            // FilesService.resolveSafePath，受 workspace-root 安全约束保护）。
            crate::files::resolve_safe_path(state, &path)
                .await
                .map_err(|e| {
                    AppError::business(
                        ErrorCode::FilesPathOutsideWorkspace,
                        StatusCode::BAD_REQUEST,
                        format!("mention path validation failed: {e}"),
                        Some({
                            let mut p = std::collections::BTreeMap::new();
                            p.insert("index".to_string(), serde_json::Value::Number(i.into()));
                            p
                        }),
                    )
                })?;
            Ok(json!({ "type": "mention", "name": name, "path": path }))
        }
        _ => Err(bad_request_params(
            ErrorCode::ThreadsInvalidInputType,
            format!("input[{i}].type must be one of {}", USER_INPUT_TYPES.join(", ")),
            i,
        )),
    }
}

fn req_string(obj: &serde_json::Map<String, Value>, field: &str, i: usize) -> Result<String, AppError> {
    obj.get(field)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| {
            bad_request_params(
                ErrorCode::ThreadsInvalidInputField,
                format!("input[{i}].{field} must be a string"),
                i,
            )
        })
}

fn req_string_trimmed(obj: &serde_json::Map<String, Value>, field: &str, i: usize) -> Result<String, AppError> {
    let s = req_string(obj, field, i)?;
    let t = s.trim();
    if t.is_empty() {
        return Err(bad_request_params(
            ErrorCode::ThreadsInvalidInputField,
            format!("input[{i}].{field} must be a non-empty string"),
            i,
        ));
    }
    Ok(t.to_string())
}

fn bad_request_params(code: ErrorCode, msg: String, index: usize) -> AppError {
    let mut params = std::collections::BTreeMap::new();
    params.insert("index".into(), Value::Number(index.into()));
    AppError::business(code, StatusCode::BAD_REQUEST, msg, Some(params))
}

fn is_http_url(s: &str) -> bool {
    // 完整 URL 解析校验（对齐 TS new URL(url)），拒绝 javascript:、data:、畸形 URL 等。
    matches!(url::Url::parse(s), Ok(u) if u.scheme() == "http" || u.scheme() == "https")
}

fn parse_positive_limit(value: Option<&str>) -> Result<Option<i64>, AppError> {
    match value.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => Err(bad_request(
                ErrorCode::ThreadsInvalidLimit,
                "limit must be a positive number",
            )),
        },
    }
}

// ── 文档响应 DTO（仅供 OpenAPI/Swagger 展示响应字段；handler 运行时仍返回
//    Json<serde_json::Value>，字段对齐 codex app-server 透传的 JSON 结构，
//    参考 TS codex/dto/v2/{thread,turn,responses}.dto.ts）──────────────────────
//
// 复杂的可辨识联合（ThreadItem 16 路 / ThreadStatus / SessionSource /
// ApprovalPolicy / SandboxPolicy）一律用 serde_json::Value 透传不展开，避免
// 自引用递归导致的 schema 无限展开（栈溢出）。

/// 线程创建时捕获的可选 Git 元数据（对齐 TS GitInfoDto；字段全可空）。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitInfoDto {
    /// Git commit SHA
    pub sha: Option<String>,
    /// 分支名
    pub branch: Option<String>,
    /// 远程仓库地址
    pub origin_url: Option<String>,
}

/// 单个 turn（对齐 TS TurnDto）。
/// `items` 为 16 路 ThreadItem 可辨识联合，结构极复杂，以 serde_json::Value
/// 透传不展开；`error` 为 TurnErrorDto，同样以 Value 透传。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnDto {
    /// turn ID
    pub id: String,
    /// turn 内的消息/动作项（command/fileChange/mcpToolCall 等）
    pub items: Vec<serde_json::Value>,
    /// turn 状态：completed/interrupted/failed/inProgress
    pub status: String,
    /// 错误详情（失败时）
    pub error: Option<serde_json::Value>,
    /// 开始时间
    pub started_at: Option<i64>,
    /// 完成时间
    pub completed_at: Option<i64>,
    /// 耗时（毫秒）
    pub duration_ms: Option<i64>,
}

/// 线程详情（对齐 TS ThreadDto）。
/// `status`（ThreadStatus）/ `source`（SessionSource）为复杂 oneOf，以 Value 透传。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDto {
    /// 线程 ID
    pub id: String,
    /// fork 来源线程 ID（无则 null）
    pub forked_from_id: Option<String>,
    /// 预览摘要
    pub preview: String,
    /// 是否临时线程
    pub ephemeral: bool,
    /// 模型 provider
    pub model_provider: String,
    /// 创建时间（Unix 毫秒）
    pub created_at: i64,
    /// 更新时间（Unix 毫秒）
    pub updated_at: i64,
    /// 线程状态（含 active 的等待标志）
    pub status: serde_json::Value,
    /// 关联路径
    pub path: Option<String>,
    /// 工作目录
    pub cwd: String,
    /// codex CLI 版本
    pub cli_version: String,
    /// 会话来源
    pub source: serde_json::Value,
    /// agent 昵称
    pub agent_nickname: Option<String>,
    /// agent 角色
    pub agent_role: Option<String>,
    /// git 信息
    pub git_info: Option<GitInfoDto>,
    /// 线程名
    pub name: Option<String>,
    /// turn 列表
    pub turns: Vec<TurnDto>,
}

/// thread/start / thread/resume / thread/fork 响应（对齐 TS ThreadStartResponseDto）。
/// `approval_policy` / `sandbox` 为复杂联合，以 Value 透传。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    /// 线程详情
    pub thread: ThreadDto,
    /// 实际使用的模型
    pub model: String,
    /// 模型 provider
    pub model_provider: String,
    /// 服务层级 fast/flex
    pub service_tier: Option<String>,
    /// 工作目录
    pub cwd: String,
    /// 审批策略
    pub approval_policy: serde_json::Value,
    /// 审批者 user/guardian_subagent
    pub approvals_reviewer: String,
    /// 沙箱策略
    pub sandbox: serde_json::Value,
    /// 推理强度
    pub reasoning_effort: Option<String>,
}

/// thread/list 响应（对齐 TS ThreadListResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    /// 线程列表
    pub data: Vec<ThreadDto>,
    /// 分页游标（无更多数据为 null）
    pub next_cursor: Option<String>,
}

/// thread/loaded/list 响应（对齐 TS ThreadLoadedListResponseDto；data 为线程 ID 列表）。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLoadedListResponse {
    /// 已加载线程 ID 列表
    pub data: Vec<String>,
    /// 分页游标（无更多数据为 null）
    pub next_cursor: Option<String>,
}

/// thread/read 响应（对齐 TS ThreadReadResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ThreadReadResponse {
    /// 线程详情
    pub thread: ThreadDto,
}

/// turn/start 响应（对齐 TS TurnStartResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct TurnStartResponse {
    /// turn 详情
    pub turn: TurnDto,
}

/// turn/steer 响应（对齐 TS TurnSteerResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    /// 目标 turn ID
    pub turn_id: String,
}

/// thread/unarchive 响应（对齐 TS ThreadUnarchiveResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ThreadUnarchiveResponse {
    /// 线程详情
    pub thread: ThreadDto,
}

/// thread/rollback 响应（对齐 TS ThreadRollbackResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ThreadRollbackResponse {
    /// 线程详情
    pub thread: ThreadDto,
}

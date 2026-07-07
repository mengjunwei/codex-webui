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
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

fn map_rpc(e: RpcError) -> AppError {
    AppError::internal(format!("codex: {e}"))
}
fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

// ── POST /threads(创建)───────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateThreadBody {
    pub model: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: Option<Value>,
}

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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct LoadedQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
}

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

#[derive(Deserialize)]
pub struct ReadQuery {
    #[serde(rename = "includeTurns")]
    pub include_turns: Option<String>,
}

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

pub async fn resume_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    // H6：若本 generation 已 resume 过该线程，命中缓存（不再重发非幂等的 thread/resume，
    // 对齐 TS resumeRegistry.ensureResumed）。但为避免返回陈旧的 turns，命中时走
    // readAsResume：重新 thread/read 取最新 thread 合并进缓存（对齐 TS readAsResume）。
    if let Some(cached) = state.resume_registry.get_cached(&thread_id) {
        let merged = read_as_resume(&state, &thread_id, cached).await;
        return Ok((StatusCode::CREATED, Json(merged)));
    }
    let result = state
        .codex
        .request("thread/resume", Some(json!({ "threadId": thread_id, "persistExtendedHistory": true })))
        .await
        .map_err(map_rpc)?;
    state.resume_registry.mark_resumed(&thread_id, result.clone());
    Ok((StatusCode::CREATED, Json(result)))
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

#[derive(Deserialize)]
pub struct StartTurnBody {
    pub input: Value,
    pub model: Option<String>,
    pub effort: Option<String>,
}

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

#[derive(Deserialize)]
pub struct RollbackBody {
    #[serde(rename = "numTurns")]
    pub num_turns: i64,
}

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

#[derive(Deserialize)]
pub struct SetNameBody {
    pub name: String,
}

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
#[derive(Debug, Default)]
pub struct ThreadResumeRegistry {
    generation: std::sync::Mutex<u64>,
    entries: std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
}

impl ThreadResumeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次 resume/start/fork 的响应（缓存供后续重复调用复用）。
    pub fn mark_resumed(&self, thread_id: &str, response: serde_json::Value) {
        self.entries
            .lock()
            .unwrap()
            .insert(thread_id.to_string(), response);
    }

    /// 返回缓存响应（若当前 generation 已 resume 过该线程）。
    pub fn get_cached(&self, thread_id: &str) -> Option<serde_json::Value> {
        self.entries.lock().unwrap().get(thread_id).cloned()
    }

    pub fn forget(&self, thread_id: &str) {
        self.entries.lock().unwrap().remove(thread_id);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    /// generation 推进：generation 变化时清空所有缓存的 resume 响应。
    pub fn advance_generation(&self, new_generation: u64) {
        let mut g = self.generation.lock().unwrap();
        if *g != new_generation {
            *g = new_generation;
            self.entries.lock().unwrap().clear();
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
    s.starts_with("http://") || s.starts_with("https://")
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

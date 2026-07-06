//! Threads + turns REST endpoints — codex app-server proxies.
//!
//! Parity with `threads.controller.ts` + `threads.service.ts`.
//!
//! Simple proxies: list / loaded-list / read / resume / archive / unarchive /
//! compact / fork / rollback / name-set / interrupt / start-thread.
//! start-turn / steer: structural input validation (the workspace path-resolution
//! security boundary for `mention`/`localImage` types is deferred until
//! FilesService/ChatUploadService land — Phase 3c/4).

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

// ── POST /threads (create) ───────────────────────────────────────────────────

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
) -> Result<Json<Value>, AppError> {
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
    Ok(Json(result))
}

// ── GET /threads (list) ──────────────────────────────────────────────────────

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
    // Empty array = all providers (parity with TS controller comment).
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

// ── GET /threads/:threadId (read) ────────────────────────────────────────────

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
            // Retry without turns; unmaterialized thread has no turns anyway.
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

/// Parity with `thread-errors.ts:isNotMaterializedError` — message contains
/// "not materialized" (case-insensitive).
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
) -> Result<Json<Value>, AppError> {
    // NOTE: TS uses a resume-registry to dedupe per generation + cache. Deferred.
    let result = state
        .codex
        .request("thread/resume", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ── POST /threads/:threadId/turns (start turn) ───────────────────────────────

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
) -> Result<Json<Value>, AppError> {
    let input = validate_turn_input(&body.input)?;
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
    Ok(Json(result))
}

// ── POST /threads/:threadId/turns/:turnId/steer ──────────────────────────────

pub async fn steer_turn(
    State(state): State<AppState>,
    Path((thread_id, turn_id)): Path<(String, String)>,
    Json(body): Json<StartTurnBody>,
) -> Result<Json<Value>, AppError> {
    let input = validate_turn_input(&body.input)?;
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
    Ok(Json(result))
}

// ── POST /threads/:threadId/turns/:turnId/interrupt ──────────────────────────

pub async fn interrupt_turn(
    State(state): State<AppState>,
    Path((thread_id, turn_id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    state
        .codex
        .request("turn/interrupt", Some(json!({ "threadId": thread_id, "turnId": turn_id })))
        .await
        .map_err(map_rpc)?;
    Ok(Json(json!({ "ok": true })))
}

// ── archive / unarchive / compact / fork / rollback / name ───────────────────

pub async fn archive_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("thread/archive", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unarchive_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let result = state
        .codex
        .request("thread/unarchive", Some(json!({ "threadId": thread_id })))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
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
) -> Result<Json<Value>, AppError> {
    let result = state
        .codex
        .request(
            "thread/fork",
            Some(json!({ "threadId": thread_id, "persistExtendedHistory": true })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
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
) -> Result<Json<Value>, AppError> {
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
    Ok(Json(result))
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

// ── input validation (structural; path resolution deferred) ──────────────────

const USER_INPUT_TYPES: &[&str] = &["text", "image", "localImage", "skill", "mention"];
const REASONING_EFFORT_VALUES: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// Validate the discriminated UserInput array. Returns the validated array.
/// NOTE: `mention`/`localImage` paths are passed through WITHOUT workspace-root
/// resolution — the security boundary (FilesService.resolveSafePath /
/// ChatUploadService.resolveStoredUploadPath) is deferred until those land.
fn validate_turn_input(input: &Value) -> Result<Value, AppError> {
    let arr = input.as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
        bad_request(
            ErrorCode::ThreadsInvalidInput,
            "input must be a non-empty array",
        )
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        out.push(validate_input_item(item, i)?);
    }
    Ok(Value::Array(out))
}

fn validate_input_item(item: &Value, i: usize) -> Result<Value, AppError> {
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
            Ok(json!({ "type": "text", "text": text }))
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
            let path = req_string_trimmed(obj, "path", i)?;
            // resolveStoredUploadPath deferred (ChatUploadService, Phase 4).
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
            // resolveSafePath deferred (FilesService, Phase 3c).
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

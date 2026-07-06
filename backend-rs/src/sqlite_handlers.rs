//! SQLite read endpoints for Phase 2 modules that don't depend on the
//! codex JSON-RPC client (Phase 1): token-usage, turn-diff, turn-errors,
//! pending-approvals.
//!
//! Write paths for these modules are event-driven (subscribed to codex
//! notifications) and will be added in Phase 1.

use crate::error::AppError;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

// ── token-usage ──────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct BreakdownDto {
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: i64,
    #[serde(rename = "cachedInputTokens")]
    pub cached_input_tokens: i64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: i64,
    #[serde(rename = "reasoningOutputTokens")]
    pub reasoning_output_tokens: i64,
}

#[derive(Serialize, Clone)]
pub struct TurnUsageDto {
    #[serde(rename = "modelContextWindow")]
    pub model_context_window: Option<i64>,
    pub total: BreakdownDto,
    pub last: BreakdownDto,
}

#[derive(Serialize, Clone)]
pub struct TurnTokenUsageDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub usage: TurnUsageDto,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct ThreadTokenUsageResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub turns: Vec<TurnTokenUsageDto>,
    pub latest: Option<TurnTokenUsageDto>,
}

pub async fn read_token_usage(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTokenUsageResponse>, AppError> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|e| AppError::internal(format!("db lock: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT turn_id, total_tokens, input_tokens, cached_input_tokens, \
             output_tokens, reasoning_output_tokens, \
             last_total_tokens, last_input_tokens, last_cached_input_tokens, \
             last_output_tokens, last_reasoning_output_tokens, \
             model_context_window, updated_at \
             FROM token_usage_snapshots WHERE thread_id = ?1 ORDER BY updated_at",
        )
        .map_err(|e| AppError::internal(format!("prepare: {e}")))?;

    let turns = stmt
        .query_map([&thread_id], |r| {
            Ok(TurnTokenUsageDto {
                turn_id: r.get(0)?,
                usage: TurnUsageDto {
                    model_context_window: r.get(11)?,
                    total: BreakdownDto {
                        total_tokens: r.get(1)?,
                        input_tokens: r.get(2)?,
                        cached_input_tokens: r.get(3)?,
                        output_tokens: r.get(4)?,
                        reasoning_output_tokens: r.get(5)?,
                    },
                    last: BreakdownDto {
                        total_tokens: r.get(6)?,
                        input_tokens: r.get(7)?,
                        cached_input_tokens: r.get(8)?,
                        output_tokens: r.get(9)?,
                        reasoning_output_tokens: r.get(10)?,
                    },
                },
                updated_at: r.get(12)?,
            })
        })
        .map_err(|e| AppError::internal(format!("query: {e}")))?
        .filter_map(|t| t.ok())
        .collect::<Vec<_>>();

    Ok(Json(ThreadTokenUsageResponse {
        thread_id,
        latest: turns.last().cloned(),
        turns,
    }))
}

// ── turn-diff ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct TurnDiffDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub diff: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct ThreadTurnDiffsResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub turns: Vec<TurnDiffDto>,
}

pub async fn read_turn_diffs(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTurnDiffsResponse>, AppError> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|e| AppError::internal(format!("db lock: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT turn_id, diff, updated_at FROM turn_diffs \
             WHERE thread_id = ?1 ORDER BY updated_at",
        )
        .map_err(|e| AppError::internal(format!("prepare: {e}")))?;

    let turns = stmt
        .query_map([&thread_id], |r| {
            Ok(TurnDiffDto {
                turn_id: r.get(0)?,
                diff: r.get(1)?,
                updated_at: r.get(2)?,
            })
        })
        .map_err(|e| AppError::internal(format!("query: {e}")))?
        .filter_map(|t| t.ok())
        .collect();

    Ok(Json(ThreadTurnDiffsResponse {
        thread_id,
        turns,
    }))
}

// ── turn-errors ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct TurnErrorDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub message: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

#[derive(Serialize)]
pub struct ThreadTurnErrorsResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub errors: Vec<TurnErrorDto>,
}

pub async fn read_turn_errors(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTurnErrorsResponse>, AppError> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|e| AppError::internal(format!("db lock: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT turn_id, message, created_at FROM turn_errors \
             WHERE thread_id = ?1 ORDER BY created_at",
        )
        .map_err(|e| AppError::internal(format!("prepare: {e}")))?;

    let errors = stmt
        .query_map([&thread_id], |r| {
            Ok(TurnErrorDto {
                turn_id: r.get(0)?,
                message: r.get(1)?,
                created_at: r.get(2)?,
            })
        })
        .map_err(|e| AppError::internal(format!("query: {e}")))?
        .filter_map(|t| t.ok())
        .collect();

    Ok(Json(ThreadTurnErrorsResponse {
        thread_id,
        errors,
    }))
}

// ── pending-approvals (list) ─────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PendingServerRequestDto {
    pub generation: i64,
    #[serde(rename = "requestId")]
    pub request_id: String,
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
    #[serde(rename = "itemId")]
    pub item_id: Option<String>,
    pub method: String,
    pub params: serde_json::Value,
    pub status: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    // NOTE: resolvedAt + resolvedBy intentionally omitted — parity with
    // pending-approvals.dto.ts / toDto (TS never serializes them).
}

#[derive(Serialize)]
pub struct ListPendingResponse {
    pub requests: Vec<PendingServerRequestDto>,
}

#[derive(Deserialize)]
pub struct PendingQuery {
    #[serde(rename = "threadIds")]
    pub thread_ids: Option<String>, // comma-separated
}

pub async fn list_pending(
    State(state): State<AppState>,
    Query(q): Query<PendingQuery>,
) -> Result<Json<ListPendingResponse>, AppError> {
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|e| AppError::internal(format!("db lock: {e}")))?;

    let requests = match q.thread_ids.as_deref().filter(|s| !s.is_empty()) {
        Some(ids) => {
            let v: Vec<String> = ids.split(',').map(|s| s.trim().to_string()).collect();
            let placeholders = std::iter::repeat("?")
                .take(v.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT generation, request_id, thread_id, turn_id, item_id, method, \
                 params_json, status, created_at, updated_at, resolved_at \
                 FROM pending_server_requests \
                 WHERE status = 'pending' AND thread_id IN ({}) \
                 ORDER BY created_at",
                placeholders
            );
            let v_refs: Vec<&dyn rusqlite::ToSql> =
                v.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::internal(format!("prepare: {e}")))?;
            let rows: Vec<PendingServerRequestDto> = stmt
                .query_map(v_refs.as_slice(), parse_pending_row)
                .map_err(|e| AppError::internal(format!("query: {e}")))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        }
        None => {
            let mut stmt = conn
                .prepare(
                    "SELECT generation, request_id, thread_id, turn_id, item_id, method, \
                     params_json, status, created_at, updated_at, resolved_at \
                     FROM pending_server_requests \
                     WHERE status = 'pending' \
                     ORDER BY created_at",
                )
                .map_err(|e| AppError::internal(format!("prepare: {e}")))?;
            let rows: Vec<PendingServerRequestDto> = stmt
                .query_map([], parse_pending_row)
                .map_err(|e| AppError::internal(format!("query: {e}")))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        }
    };

    Ok(Json(ListPendingResponse { requests }))
}

fn parse_pending_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingServerRequestDto> {
    let params_json: String = r.get(6)?;
    let params: serde_json::Value =
        serde_json::from_str(&params_json).unwrap_or(serde_json::Value::Object(Default::default()));
    Ok(PendingServerRequestDto {
        generation: r.get(0)?,
        request_id: r.get(1)?,
        thread_id: r.get(2)?,
        turn_id: r.get(3)?,
        item_id: r.get(4)?,
        method: r.get(5)?,
        params,
        status: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}
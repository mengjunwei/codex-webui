//! Phase 2 中不依赖 codex JSON-RPC 客户端（Phase 1）的 SQLite 只读端点：
//! token-usage、turn-diff、turn-errors、pending-approvals。
//!
//! 这些模块的写入路径是事件驱动的（订阅 codex 通知），将在 Phase 1 中补充。
//!
//! 数据访问全部走 SeaORM（PG/MySQL 多方言）。`AppState.db` 为
//! `sea_orm::DatabaseConnection`，复合主键查找用元组，CAS 更新在事务内完成
//! （保持"转发失败回滚"的原语义）。

use crate::error::{AppError, ErrorCode, Json};
use crate::services::multitenant::now_ms;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
};
use sea_orm::{
    entity::prelude::*, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TransactionError,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::db::entity::pending_server_request::{
    ActiveModel as PendingServerRequestActiveModel, Column as PendingServerRequestColumn,
    Entity as PendingServerRequestEntity, Model as PendingServerRequestModel,
};
use crate::db::entity::token_usage_snapshot::{
    Column as TokenUsageColumn, Entity as TokenUsageEntity, Model as TokenUsageModel,
};
use crate::db::entity::turn_diff::{
    Column as TurnDiffColumn, Entity as TurnDiffEntity, Model as TurnDiffModel,
};
use crate::db::entity::turn_error::{
    Column as TurnErrorColumn, Entity as TurnErrorEntity, Model as TurnErrorModel,
};

// ── token 用量 ──────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, utoipa::ToSchema)]
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

#[derive(Serialize, Clone, utoipa::ToSchema)]
pub struct TurnUsageDto {
    #[serde(rename = "modelContextWindow")]
    pub model_context_window: Option<i64>,
    pub total: BreakdownDto,
    pub last: BreakdownDto,
}

#[derive(Serialize, Clone, utoipa::ToSchema)]
pub struct TurnTokenUsageDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub usage: TurnUsageDto,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ThreadTokenUsageResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub turns: Vec<TurnTokenUsageDto>,
    pub latest: Option<TurnTokenUsageDto>,
}

/// 把 entity Model 投影为对外 DTO（不动 raw_payload 字段）。
fn token_usage_to_dto(m: TokenUsageModel) -> TurnTokenUsageDto {
    TurnTokenUsageDto {
        turn_id: m.turn_id,
        usage: TurnUsageDto {
            model_context_window: m.model_context_window,
            total: BreakdownDto {
                total_tokens: m.total_tokens,
                input_tokens: m.input_tokens,
                cached_input_tokens: m.cached_input_tokens,
                output_tokens: m.output_tokens,
                reasoning_output_tokens: m.reasoning_output_tokens,
            },
            last: BreakdownDto {
                total_tokens: m.last_total_tokens,
                input_tokens: m.last_input_tokens,
                cached_input_tokens: m.last_cached_input_tokens,
                output_tokens: m.last_output_tokens,
                reasoning_output_tokens: m.last_reasoning_output_tokens,
            },
        },
        updated_at: m.updated_at,
    }
}

#[utoipa::path(
    get,
    path = "/api/threads/{threadId}/token-usage",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 200, description = "线程下所有 turn 的 token 用量", body = ThreadTokenUsageResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_token_usage(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTokenUsageResponse>, AppError> {
    let turns: Vec<TurnTokenUsageDto> = TokenUsageEntity::find()
        .filter(TokenUsageColumn::ThreadId.eq(thread_id.clone()))
        .order_by_asc(TokenUsageColumn::UpdatedAt)
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("query token_usage_snapshots: {e}")))?
        .into_iter()
        .map(token_usage_to_dto)
        .collect();

    Ok(Json(ThreadTokenUsageResponse {
        thread_id,
        latest: turns.last().cloned(),
        turns,
    }))
}

/// H10 修复：新增仅读取最新一条 token usage 的端点（对齐 TS
/// `TokenUsageService.readLatestThreadUsage`），比全量查询更高效。
#[utoipa::path(
    get,
    path = "/api/threads/{threadId}/token-usage/latest",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 200, description = "最新一条 token 用量（无则 null）", body = Option<TurnTokenUsageDto>),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_latest_token_usage(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Option<TurnTokenUsageDto>>, AppError> {
    let result = TokenUsageEntity::find()
        .filter(TokenUsageColumn::ThreadId.eq(thread_id.clone()))
        .order_by_desc(TokenUsageColumn::UpdatedAt)
        .one(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("query latest token_usage: {e}")))?
        .map(token_usage_to_dto);

    Ok(Json(result))
}

// ── turn 差异 ────────────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
pub struct TurnDiffDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub diff: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ThreadTurnDiffsResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub turns: Vec<TurnDiffDto>,
}

#[utoipa::path(
    get,
    path = "/api/threads/{threadId}/turn-diffs",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 200, description = "线程下所有 turn 的差异", body = ThreadTurnDiffsResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_turn_diffs(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTurnDiffsResponse>, AppError> {
    let turns: Vec<TurnDiffDto> = TurnDiffEntity::find()
        .filter(TurnDiffColumn::ThreadId.eq(thread_id.clone()))
        .order_by_asc(TurnDiffColumn::UpdatedAt)
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("query turn_diffs: {e}")))?
        .into_iter()
        .map(|m: TurnDiffModel| TurnDiffDto {
            turn_id: m.turn_id,
            diff: m.diff,
            updated_at: m.updated_at,
        })
        .collect();

    Ok(Json(ThreadTurnDiffsResponse { thread_id, turns }))
}

// ── turn 错误 ────────────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
pub struct TurnErrorDto {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub message: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ThreadTurnErrorsResponse {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub errors: Vec<TurnErrorDto>,
}

#[utoipa::path(
    get,
    path = "/api/threads/{threadId}/turn-errors",
    tag = "threads",
    params(("threadId" = String, Path, description = "线程 ID")),
    responses(
        (status = 200, description = "线程下所有 turn 的错误", body = ThreadTurnErrorsResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_turn_errors(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadTurnErrorsResponse>, AppError> {
    let errors: Vec<TurnErrorDto> = TurnErrorEntity::find()
        .filter(TurnErrorColumn::ThreadId.eq(thread_id.clone()))
        .order_by_asc(TurnErrorColumn::CreatedAt)
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("query turn_errors: {e}")))?
        .into_iter()
        .map(|m: TurnErrorModel| TurnErrorDto {
            turn_id: m.turn_id,
            message: m.message,
            created_at: m.created_at,
        })
        .collect();

    Ok(Json(ThreadTurnErrorsResponse { thread_id, errors }))
}

// ── 待处理审批（列表）─────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
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
    // 注意：resolvedAt 与 resolvedBy 故意省略 —— 与
    // pending-approvals.dto.ts / toDto 保持一致（TS 端从不序列化这两个字段）。
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ListPendingResponse {
    pub requests: Vec<PendingServerRequestDto>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PendingQuery {
    #[serde(rename = "threadIds")]
    pub thread_ids: Option<String>, // 以逗号分隔
}

/// 把 entity Model 投影为对外 DTO：解析 params_json。
fn pending_model_to_dto(m: PendingServerRequestModel) -> PendingServerRequestDto {
    let params: serde_json::Value = serde_json::from_str(&m.params_json)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    PendingServerRequestDto {
        generation: m.generation,
        request_id: m.request_id,
        thread_id: m.thread_id,
        turn_id: m.turn_id,
        item_id: m.item_id,
        method: m.method,
        params,
        status: m.status,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

#[utoipa::path(
    get,
    path = "/api/pending-approvals",
    tag = "approvals",
    params(PendingQuery),
    responses(
        (status = 200, description = "待处理审批列表", body = ListPendingResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn list_pending(
    State(state): State<AppState>,
    Query(q): Query<PendingQuery>,
) -> Result<Json<ListPendingResponse>, AppError> {
    // 过滤空/纯空白条目；全空时退化为"无过滤"（对齐 TS .filter(Boolean)）。
    let ids: Vec<String> = q
        .thread_ids
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut query = PendingServerRequestEntity::find()
        .filter(PendingServerRequestColumn::Status.eq("pending".to_string()));
    if !ids.is_empty() {
        query = query.filter(PendingServerRequestColumn::ThreadId.is_in(ids));
    }
    let rows = query
        .order_by_asc(PendingServerRequestColumn::CreatedAt)
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("query pending_server_requests: {e}")))?
        .into_iter()
        .map(pending_model_to_dto)
        .collect();

    Ok(Json(ListPendingResponse { requests: rows }))
}

// ── 响应待处理请求：POST /pending-approvals/:requestId/respond ──────────────

/// `respond_to_request` 的请求体（仅用于 OpenAPI 文档；handler 实际用原始
/// `serde_json::Value` 提取，此处给出结构化 schema 供前端参考）。
#[derive(Deserialize, utoipa::ToSchema)]
pub struct RespondRequestBody {
    /// 审批结果（任意 JSON：approve/deny 的具体结构由 codex 上游决定）。
    pub result: serde_json::Value,
    /// 可选：发起响应的客户端标识（记录到 resolved_by）。
    #[serde(rename = "clientId")]
    pub client_id: Option<String>,
}

/// `respond_to_request` 事务闭包内部错误：转发失败（保留回滚语义）+ CAS 冲突。
#[derive(Debug)]
enum RespondTxnError {
    /// CAS 未命中（rows_affected != 1），并发场景下其他请求已处理。
    AlreadyHandled,
    /// 转发到 app-server 失败 —— 事务必须回滚以保留 pending 状态。
    Forward(String),
    /// DB 层错误（向上传递 DbErr 方便外层包装）。
    Db(DbErr),
}

impl std::fmt::Display for RespondTxnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHandled => write!(f, "pending approval already handled"),
            Self::Forward(e) => write!(f, "respond forward: {e}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RespondTxnError {}

impl From<DbErr> for RespondTxnError {
    fn from(e: DbErr) -> Self {
        Self::Db(e)
    }
}

#[utoipa::path(
    post,
    path = "/api/pending-approvals/{requestId}/respond",
    tag = "approvals",
    params(("requestId" = String, Path, description = "待处理请求 ID")),
    request_body = RespondRequestBody,
    responses(
        (status = 200, description = "已响应，返回更新后的请求", body = PendingServerRequestDto),
        (status = 400, description = "缺少 result 字段", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "请求不存在", body = crate::error::ErrorResponse),
        (status = 409, description = "请求已响应/已处理/服务端未连接", body = crate::error::ErrorResponse),
    )
)]
pub async fn respond_to_request(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<PendingServerRequestDto>, AppError> {
    let generation = state.codex.generation() as i64;
    let now = now_ms();

    // H5：显式校验 result 存在性（TS 端使用 hasOwnProperty('result')）。
    // 注意：{"result": null} 视为存在（转发 null），仅当字段缺失时报 400。
    let has_result = body
        .as_object()
        .map(|o| o.contains_key("result"))
        .unwrap_or(false);
    if !has_result {
        return Err(AppError::business(
            ErrorCode::ApprovalsResultRequired,
            StatusCode::BAD_REQUEST,
            "result is required".into(),
            None,
        ));
    }
    let result = body.get("result").cloned().unwrap_or(serde_json::Value::Null);
    let client_id = body
        .get("clientId")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.to_string());

    // 1. 预先查询（读阶段，可独立提交一次 SELECT）。SeaORM 的 .await 不再持有
    //    MutexGuard，故可直接在同一 handler 中向下推进。
    let existing: Option<PendingServerRequestModel> = PendingServerRequestEntity::find_by_id((
        generation,
        request_id.clone(),
    ))
    .one(&state.db)
    .await
    .map_err(|e| AppError::internal(format!("query pending: {e}")))?;

    let existing = existing.ok_or_else(|| {
        AppError::business(
            ErrorCode::ApprovalsNotFound,
            StatusCode::NOT_FOUND,
            "Pending request not found".into(),
            None,
        )
    })?;
    if existing.status != "pending" {
        return Err(AppError::business(
            ErrorCode::ApprovalsAlreadyResolved,
            StatusCode::CONFLICT,
            "Pending request has already been resolved".into(),
            None,
        ));
    }

    // 2. 获取客户端（在事务外提前 await，避免在事务闭包内持有锁）。
    let client = state.codex.client().await.ok_or_else(|| {
        AppError::business(
            ErrorCode::ApprovalsServerNotConnected,
            StatusCode::CONFLICT,
            "Codex app-server is not connected".into(),
            None,
        )
    })?;

    // 3. 事务：CAS 更新 + 转发。转发失败回滚（保持原 sqlite 语义）。
    //    通过自定义错误类型区分 AlreadyHandled / Forward，事务外再映射回 AppError。
    let txn_result: Result<(), TransactionError<RespondTxnError>> = state
        .db
        .transaction(|txn| {
            // client 是 Arc,clone 廉价;closure 需 'static-friendly
            let client = client.clone();
            let request_id = request_id.clone();
            let client_id = client_id.clone();
            let id_value = parse_request_id_value(&request_id);
            Box::pin(async move {
                let mut am: PendingServerRequestActiveModel = existing.into();
                am.status = Set("resolved".to_string());
                am.resolved_by = Set(client_id.clone());
                am.resolved_at = Set(Some(now));
                am.updated_at = Set(now);

                // CAS：仅在 status='pending' 时才更新；用复合主键定位行。
                let update_res = PendingServerRequestEntity::update_many()
                    .set(am)
                    .filter(PendingServerRequestColumn::Generation.eq(generation))
                    .filter(PendingServerRequestColumn::RequestId.eq(request_id.clone()))
                    .filter(PendingServerRequestColumn::Status.eq("pending".to_string()))
                    .exec(txn)
                    .await?;

                if update_res.rows_affected != 1 {
                    // 不 commit → 自动回滚。
                    return Err(RespondTxnError::AlreadyHandled);
                }

                // 在事务内转发到 app-server（失败时事务回滚，状态保留 pending）。
                if let Err(e) = client.respond_to_server_request(id_value, result) {
                    return Err(RespondTxnError::Forward(e.to_string()));
                }
                Ok(())
            })
        })
        .await;

    match txn_result {
        Ok(()) => {}
        Err(TransactionError::Transaction(RespondTxnError::AlreadyHandled)) => {
            return Err(AppError::business(
                ErrorCode::ApprovalsAlreadyHandled,
                StatusCode::CONFLICT,
                "Pending approval was already handled".into(),
                None,
            ));
        }
        Err(TransactionError::Transaction(RespondTxnError::Forward(e))) => {
            return Err(AppError::internal(format!("respond forward: {e}")));
        }
        Err(TransactionError::Transaction(RespondTxnError::Db(e))) => {
            return Err(AppError::internal(format!("cas update: {e}")));
        }
        Err(TransactionError::Connection(e)) => {
            return Err(AppError::internal(format!("txn begin: {e}")));
        }
    }

    // 4. 重新查询以构造 DTO。
    let updated: PendingServerRequestModel = PendingServerRequestEntity::find_by_id((
        generation,
        request_id.clone(),
    ))
    .one(&state.db)
    .await
    .map_err(|e| AppError::internal(format!("requery pending: {e}")))?
    .ok_or_else(|| {
        AppError::business(
            ErrorCode::ApprovalsNotFound,
            StatusCode::NOT_FOUND,
            "Pending request not found".into(),
            None,
        )
    })?;

    Ok(Json(pending_model_to_dto(updated)))
}

/// 将存储的 requestId（字符串）还原为 JSON Value，
/// 在用于 JSON-RPC 响应关联时保留其原本的数字/字符串类型。
fn parse_request_id_value(s: &str) -> serde_json::Value {
    if let Ok(n) = s.parse::<u64>() {
        serde_json::Value::Number(n.into())
    } else {
        serde_json::Value::String(s.into())
    }
}
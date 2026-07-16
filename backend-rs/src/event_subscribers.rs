//! 事件驱动的 DB 写入路径。
//!
//! 订阅 CodexProcessManager 的 notification / server-request /
//! lifecycle 广播并持久化数据 —— 与 TS 服务对齐：
//! - token-usage：`thread/tokenUsage/updated` → upsert token_usage_snapshots
//! - turn-diff：`turn/diff/updated`（缓冲）+ `turn/completed`（刷写）→ turn_diffs
//! - turn-errors：`error`（willRetry=false）+ `turn/completed`（status=failed）→ turn_errors
//! - pending-approvals：server-request → 记录；`serverRequest/resolved` → 标记；
//!   lifecycle Restarting/Unavailable → 按代次过期；启动 → 全部过期。
//!
//! 每个订阅者都是一个独立的 tokio 任务。在启动时调用一次 `spawn_all` 即可。
//!
//! 数据库访问统一走 SeaORM 1.1(`sea_orm::DatabaseConnection`,PG/MySQL 多方言)。

use crate::codex::{CodexProcessManager, LifecycleEvent};
use crate::entity::{
    pending_server_request::{
        ActiveModel as PendingActive, Column as PendingColumn, Entity as PendingEntity,
    },
    token_usage_snapshot::{ActiveModel as TokenUsageActive, Entity as TokenUsageEntity},
    turn_diff::{ActiveModel as TurnDiffActive, Entity as TurnDiffEntity},
    turn_error::{ActiveModel as TurnErrorActive, Entity as TurnErrorEntity},
};
use anyhow::Result;
use sea_orm::{
    entity::prelude::*, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// 启动所有事件订阅者。同时会在启动时过期陈旧的待处理请求
/// （与 PendingApprovalsService.onModuleInit 对齐）。
pub fn spawn_all(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    // 启动时过期所有 pending 任务;不阻断后续订阅者启动。
    let db_for_expire = db.clone();
    tokio::spawn(async move {
        if let Err(e) = expire_all_pending(&db_for_expire).await {
            tracing::warn!("startup expire-all-pending failed: {e}");
        }
    });
    spawn_token_usage(db.clone(), codex.clone());
    spawn_turn_diff(db.clone(), codex.clone());
    spawn_turn_errors(db.clone(), codex.clone());
    // M1 修复：pending_record 已移至 realtime.rs，与 WS emit 合并处理
    // （原来是分开的订阅者 → 存在 TOCTOU：emit 可能在 DB 记录之前到达）。
    spawn_pending_resolved(db.clone(), codex.clone());
    spawn_pending_expire(db.clone(), codex.clone());
}

// ── token 用量 ──────────────────────────────────────────────────────────────

fn spawn_token_usage(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    tokio::spawn(async move {
        let mut rx = codex.subscribe_notifications();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if msg.get("method").and_then(Value::as_str)
                        != Some("thread/tokenUsage/updated")
                    {
                        continue;
                    }
                    let Some(params) = msg.get("params") else {
                        continue;
                    };
                    let (Some(thread_id), Some(turn_id)) = (
                        params.get("threadId").and_then(Value::as_str),
                        params.get("turnId").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    let Some(usage) = params.get("tokenUsage") else {
                        continue;
                    };
                    if let Err(e) = upsert_token_usage(&db, thread_id, turn_id, usage).await {
                        tracing::warn!(
                            "token usage upsert failed (thread={thread_id} turn={turn_id}): {e}"
                        );
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("token-usage subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// 用量字段读取辅助:从可选 JSON 对象中按 key 取 i64,缺省 0。
fn read_i64(o: Option<&Value>, k: &str) -> i64 {
    o.and_then(|v| v.get(k)).and_then(Value::as_i64).unwrap_or(0)
}

/// upsert:SeaORM 不依赖方言特定 upsert,统一采用
/// "先 find_by_id(复合主键),存在则 update,不存在则 insert"模式。
async fn upsert_token_usage(
    db: &DatabaseConnection,
    thread_id: &str,
    turn_id: &str,
    usage: &Value,
) -> Result<()> {
    let total = usage.get("total");
    let last = usage.get("last");
    let model_ctx = usage.get("modelContextWindow").and_then(Value::as_i64);
    let raw = serde_json::to_string(usage).unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();

    // 复合主键 (thread_id, turn_id) 用元组查。
    let existing = TokenUsageEntity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .map_err(|e| anyhow::anyhow!("find token_usage: {e}"))?;

    if let Some(model) = existing {
        let mut am: TokenUsageActive = model.into();
        am.total_tokens = Set(read_i64(total, "totalTokens"));
        am.input_tokens = Set(read_i64(total, "inputTokens"));
        am.cached_input_tokens = Set(read_i64(total, "cachedInputTokens"));
        am.output_tokens = Set(read_i64(total, "outputTokens"));
        am.reasoning_output_tokens = Set(read_i64(total, "reasoningOutputTokens"));
        am.last_total_tokens = Set(read_i64(last, "totalTokens"));
        am.last_input_tokens = Set(read_i64(last, "inputTokens"));
        am.last_cached_input_tokens = Set(read_i64(last, "cachedInputTokens"));
        am.last_output_tokens = Set(read_i64(last, "outputTokens"));
        am.last_reasoning_output_tokens = Set(read_i64(last, "reasoningOutputTokens"));
        am.model_context_window = Set(model_ctx);
        am.raw_payload = Set(raw);
        am.updated_at = Set(now);
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update token_usage: {e}"))?;
    } else {
        let am = TokenUsageActive {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            total_tokens: Set(read_i64(total, "totalTokens")),
            input_tokens: Set(read_i64(total, "inputTokens")),
            cached_input_tokens: Set(read_i64(total, "cachedInputTokens")),
            output_tokens: Set(read_i64(total, "outputTokens")),
            reasoning_output_tokens: Set(read_i64(total, "reasoningOutputTokens")),
            last_total_tokens: Set(read_i64(last, "totalTokens")),
            last_input_tokens: Set(read_i64(last, "inputTokens")),
            last_cached_input_tokens: Set(read_i64(last, "cachedInputTokens")),
            last_output_tokens: Set(read_i64(last, "outputTokens")),
            last_reasoning_output_tokens: Set(read_i64(last, "reasoningOutputTokens")),
            model_context_window: Set(model_ctx),
            raw_payload: Set(raw),
            updated_at: Set(now),
        };
        am.insert(db)
            .await
            .map_err(|e| anyhow::anyhow!("insert token_usage: {e}"))?;
    }
    Ok(())
}

// ── turn 差异（内存缓冲 + turn/completed 时刷写）───────────────────────────────

fn spawn_turn_diff(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    tokio::spawn(async move {
        let mut rx = codex.subscribe_notifications();
        // 归本任务所有的缓冲区：turnKey → (threadId, turnId, diff)。
        let mut buffer: HashMap<String, (String, String, String)> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    let params = msg.get("params");
                    match method {
                        "turn/diff/updated" => {
                            if let Some(p) = params {
                                let thread_id = p.get("threadId").and_then(Value::as_str);
                                let turn_id = p.get("turnId").and_then(Value::as_str);
                                let diff = p.get("diff").and_then(Value::as_str);
                                if let (Some(t), Some(u), Some(d)) = (thread_id, turn_id, diff) {
                                    buffer.insert(format!("{t}:{u}"), (t.into(), u.into(), d.into()));
                                }
                            }
                        }
                        "turn/completed" => {
                            if let Some(p) = params {
                                let thread_id = p.get("threadId").and_then(Value::as_str);
                                let turn_id = p
                                    .get("turn")
                                    .and_then(|t| t.get("id"))
                                    .and_then(Value::as_str);
                                if let (Some(t), Some(u)) = (thread_id, turn_id) {
                                    if let Some((tid, uid, diff)) = buffer.remove(&format!("{t}:{u}")) {
                                        if let Err(e) = persist_turn_diff(&db, &tid, &uid, &diff).await {
                                            tracing::warn!("turn diff persist failed: {e}");
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // T9：Lagged 可能丢失 turn/completed，buffer 条目会孤立泄漏；flush 全部落盘后清空。
                    tracing::warn!("turn-diff subscriber lagged {n}, flushing {} buffered diffs", buffer.len());
                    for (tid, uid, diff) in buffer.values() {
                        if let Err(e) = persist_turn_diff(&db, tid, uid, diff).await {
                            tracing::warn!("turn-diff lag flush failed: {e}");
                        }
                    }
                    buffer.clear();
                }
                Err(RecvError::Closed) => break,
            }
        }
        // H9 修复：优雅退出时刷写所有仍在内存中的 diff。
        // 当 codex 管理器关闭时 broadcast 通道关闭，循环 break，
        // 转储全部已缓冲的 diff 到数据库（对齐 TS onModuleDestroy flushAll）。
        for (tid, uid, diff) in buffer.values() {
            if let Err(e) = persist_turn_diff(&db, tid, uid, diff).await {
                tracing::warn!("turn-diff graceful flush failed: {e}");
            }
        }
        tracing::info!("turn-diff subscriber exiting (flushed {} buffered diffs)", buffer.len());
    });
}

async fn persist_turn_diff(
    db: &DatabaseConnection,
    thread_id: &str,
    turn_id: &str,
    diff: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    // 复合主键 (thread_id, turn_id) upsert。
    let existing = TurnDiffEntity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .map_err(|e| anyhow::anyhow!("find turn_diff: {e}"))?;

    if let Some(model) = existing {
        let mut am: TurnDiffActive = model.into();
        am.diff = Set(diff.to_string());
        am.updated_at = Set(now);
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update turn_diff: {e}"))?;
    } else {
        let am = TurnDiffActive {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            diff: Set(diff.to_string()),
            updated_at: Set(now),
        };
        am.insert(db)
            .await
            .map_err(|e| anyhow::anyhow!("insert turn_diff: {e}"))?;
    }
    Ok(())
}

// ── turn 错误 ────────────────────────────────────────────────────────────────

fn spawn_turn_errors(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    tokio::spawn(async move {
        let mut rx = codex.subscribe_notifications();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    let Some(params) = msg.get("params") else {
                        continue;
                    };
                    match method {
                        "error" => {
                            // 仅处理带 threadId+turnId 的最终错误（willRetry 为假）。
                            if params.get("willRetry").and_then(Value::as_bool).unwrap_or(false) {
                                continue;
                            }
                            let thread_id = params.get("threadId").and_then(Value::as_str);
                            let turn_id = params.get("turnId").and_then(Value::as_str);
                            let message = params
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown error");
                            if let (Some(t), Some(u)) = (thread_id, turn_id) {
                                if let Err(e) = upsert_turn_error(&db, t, u, message).await {
                                    tracing::warn!("turn error persist failed: {e}");
                                }
                            }
                        }
                        "turn/completed" => {
                            let thread_id = params.get("threadId").and_then(Value::as_str);
                            let turn = params.get("turn");
                            let turn_id = turn.and_then(|t| t.get("id")).and_then(Value::as_str);
                            let status = turn.and_then(|t| t.get("status")).and_then(Value::as_str);
                            let message = turn
                                .and_then(|t| t.get("error"))
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str);
                            if let (Some(t), Some(u), Some("failed"), Some(m)) =
                                (thread_id, turn_id, status, message)
                            {
                                if let Err(e) = upsert_turn_error(&db, t, u, m).await {
                                    tracing::warn!("turn error persist failed: {e}");
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("turn-errors subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

async fn upsert_turn_error(
    db: &DatabaseConnection,
    thread_id: &str,
    turn_id: &str,
    message: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let existing = TurnErrorEntity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .map_err(|e| anyhow::anyhow!("find turn_error: {e}"))?;

    if let Some(model) = existing {
        let mut am: TurnErrorActive = model.into();
        am.message = Set(message.to_string());
        am.created_at = Set(now);
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update turn_error: {e}"))?;
    } else {
        let am = TurnErrorActive {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            message: Set(message.to_string()),
            created_at: Set(now),
        };
        am.insert(db)
            .await
            .map_err(|e| anyhow::anyhow!("insert turn_error: {e}"))?;
    }
    Ok(())
}

// ── 待处理审批：记录 server 请求 ───────────────────────────────────────────────
// M1 修复：记录+emit 现在在 realtime.rs::spawn_server_request_record_and_emit 中完成，
// 以保证 DB 记录在 WS 投递之前完成（防止 TOCTOU：客户端对尚未记录的请求作出响应 → 404）。
// record_server_request 从那里被调用。

pub async fn record_server_request(
    db: &DatabaseConnection,
    codex: &CodexProcessManager,
    req: &Value,
) -> Result<()> {
    let params = req.get("params");
    let thread_id = params
        .and_then(|p| p.get("threadId"))
        .and_then(Value::as_str);
    let id = req.get("id");
    let method = req.get("method").and_then(Value::as_str);

    // 缺少 threadId / id / method 时跳过（对齐：TS 端返回 null）。
    let (Some(thread_id), Some(_), Some(method)) = (thread_id, id, method) else {
        return Ok(());
    };

    let turn_id = params
        .and_then(|p| p.get("turnId"))
        .and_then(Value::as_str);
    let item_id = params
        .and_then(|p| p.get("itemId"))
        .and_then(Value::as_str);
    let params_json = serde_json::to_string(params.unwrap_or(&Value::Null)).unwrap_or_default();
    let generation = codex.generation() as i64;
    let request_id = id_to_string(id.unwrap());
    let now = chrono::Utc::now().timestamp_millis();

    // 复合主键 (generation, request_id) upsert。
    // ON CONFLICT 等价语义:同一代次同一请求重发时,
    // 把 thread/turn/item/method/params 重置回 pending,清空 resolved_*。
    let existing = PendingEntity::find_by_id((generation, request_id.clone()))
        .one(db)
        .await
        .map_err(|e| anyhow::anyhow!("find pending_server_request: {e}"))?;

    if let Some(model) = existing {
        let mut am: PendingActive = model.into();
        am.thread_id = Set(thread_id.to_string());
        am.turn_id = Set(turn_id.map(|s| s.to_string()));
        am.item_id = Set(item_id.map(|s| s.to_string()));
        am.method = Set(method.to_string());
        am.params_json = Set(params_json);
        am.status = Set("pending".to_string());
        am.resolved_by = Set(None);
        am.updated_at = Set(now);
        am.resolved_at = Set(None);
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update pending_server_request: {e}"))?;
    } else {
        let am = PendingActive {
            generation: Set(generation),
            request_id: Set(request_id),
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.map(|s| s.to_string())),
            item_id: Set(item_id.map(|s| s.to_string())),
            method: Set(method.to_string()),
            params_json: Set(params_json),
            status: Set("pending".to_string()),
            resolved_by: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            resolved_at: Set(None),
        };
        am.insert(db)
            .await
            .map_err(|e| anyhow::anyhow!("insert pending_server_request: {e}"))?;
    }
    Ok(())
}

/// 将 JSON id Value 转换为其 DB 字符串形式。
fn id_to_string(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ── 待处理审批：serverRequest/resolved → 标记为已解决 ───────────────────────────

fn spawn_pending_resolved(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    tokio::spawn(async move {
        let mut rx = codex.subscribe_notifications();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if msg.get("method").and_then(Value::as_str) != Some("serverRequest/resolved") {
                        continue;
                    }
                    let Some(params) = msg.get("params") else { continue };
                    let Some(request_id) = params.get("requestId") else { continue };
                    let generation = codex.generation() as i64;
                    let now = chrono::Utc::now().timestamp_millis();
                    let req_str = id_to_string(request_id);
                    if let Err(e) =
                        mark_pending_resolved(&db, generation, &req_str, now).await
                    {
                        tracing::warn!("pending-resolved update failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("pending-resolved subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// 把 (generation, request_id) 对应行标记为 resolved。
/// 不存在时静默忽略(对齐原 UPDATE 行为)。
async fn mark_pending_resolved(
    db: &DatabaseConnection,
    generation: i64,
    request_id: &str,
    now: i64,
) -> Result<()> {
    let existing = PendingEntity::find_by_id((generation, request_id.to_string()))
        .one(db)
        .await
        .map_err(|e| anyhow::anyhow!("find pending_server_request: {e}"))?;

    if let Some(model) = existing {
        let mut am: PendingActive = model.into();
        am.status = Set("resolved".to_string());
        am.updated_at = Set(now);
        am.resolved_at = Set(Some(now));
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update pending_server_request resolved: {e}"))?;
    }
    Ok(())
}

// ── 待处理审批：在生命周期事件时过期 ───────────────────────────────────────────

fn spawn_pending_expire(db: DatabaseConnection, codex: Arc<CodexProcessManager>) {
    tokio::spawn(async move {
        let mut rx = codex.subscribe_lifecycle();
        loop {
            match rx.recv().await {
                Ok(event) => match event {
                    LifecycleEvent::Restarting { generation, .. }
                    | LifecycleEvent::Unavailable { generation, .. } => {
                        if let Err(e) = expire_generation(&db, generation as i64).await {
                            tracing::warn!("expire generation {generation} failed: {e}");
                        }
                    }
                    LifecycleEvent::Ready { .. } => {}
                },
                Err(RecvError::Lagged(n)) => tracing::warn!("pending-expire subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// 把指定 generation 下所有 status='pending' 的请求批量过期。
/// 按 (generation, status) 过滤后逐条 update;set 与单条 mark 一致。
async fn expire_generation(db: &DatabaseConnection, generation: i64) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let rows = PendingEntity::find()
        .filter(PendingColumn::Generation.eq(generation))
        .filter(PendingColumn::Status.eq("pending"))
        .all(db)
        .await
        .map_err(|e| anyhow::anyhow!("find pending by generation: {e}"))?;
    for model in rows {
        let mut am: PendingActive = model.into();
        am.status = Set("expired".to_string());
        am.updated_at = Set(now);
        am.resolved_at = Set(Some(now));
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update pending expired: {e}"))?;
    }
    Ok(())
}

/// 启动时把全部 status='pending' 的请求批量过期。
async fn expire_all_pending(db: &DatabaseConnection) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let rows = PendingEntity::find()
        .filter(PendingColumn::Status.eq("pending"))
        .all(db)
        .await
        .map_err(|e| anyhow::anyhow!("find all pending: {e}"))?;
    for model in rows {
        let mut am: PendingActive = model.into();
        am.status = Set("expired".to_string());
        am.updated_at = Set(now);
        am.resolved_at = Set(Some(now));
        am.update(db)
            .await
            .map_err(|e| anyhow::anyhow!("update all pending expired: {e}"))?;
    }
    tracing::debug!("expired stale pending requests: startup");
    Ok(())
}
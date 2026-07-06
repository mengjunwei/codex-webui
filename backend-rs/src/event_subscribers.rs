//! Event-driven DB write paths.
//!
//! Subscribes to the CodexProcessManager's notification / server-request /
//! lifecycle broadcasts and persists data — parity with the TS services:
//! - token-usage: `thread/tokenUsage/updated` → upsert token_usage_snapshots
//! - turn-diff: `turn/diff/updated` (buffer) + `turn/completed` (flush) → turn_diffs
//! - turn-errors: `error` (willRetry=false) + `turn/completed` (status=failed) → turn_errors
//! - pending-approvals: server-request → record; `serverRequest/resolved` → mark;
//!   lifecycle Restarting/Unavailable → expire generation; startup → expire all.
//!
//! Each subscriber is a detached tokio task. Call `spawn_all` once at startup.

use crate::codex::{CodexProcessManager, LifecycleEvent};
use crate::db::Db;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Spawn all event subscribers. Also expires stale pending requests on boot
/// (parity with PendingApprovalsService.onModuleInit).
pub fn spawn_all(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    if let Err(e) = expire_all_pending(&db, "WebUI restarted") {
        tracing::warn!("startup expire-all-pending failed: {e}");
    }
    spawn_token_usage(db.clone(), codex.clone());
    spawn_turn_diff(db.clone(), codex.clone());
    spawn_turn_errors(db.clone(), codex.clone());
    spawn_pending_record(db.clone(), codex.clone());
    spawn_pending_resolved(db.clone(), codex.clone());
    spawn_pending_expire(db.clone(), codex.clone());
}

// ── token-usage ──────────────────────────────────────────────────────────────

fn spawn_token_usage(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_notifications();
    tokio::spawn(async move {
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
                    if let Err(e) = upsert_token_usage(&db, thread_id, turn_id, usage) {
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

fn upsert_token_usage(db: &Db, thread_id: &str, turn_id: &str, usage: &Value) -> Result<()> {
    let n = |o: Option<&Value>, k: &str| -> i64 {
        o.and_then(|v| v.get(k)).and_then(Value::as_i64).unwrap_or(0)
    };
    let total = usage.get("total");
    let last = usage.get("last");
    let model_ctx = usage.get("modelContextWindow").and_then(Value::as_i64);
    let raw = serde_json::to_string(usage).unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();

    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "INSERT INTO token_usage_snapshots \
         (thread_id, turn_id, total_tokens, input_tokens, cached_input_tokens, output_tokens, \
          reasoning_output_tokens, last_total_tokens, last_input_tokens, last_cached_input_tokens, \
          last_output_tokens, last_reasoning_output_tokens, model_context_window, raw_payload, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) \
         ON CONFLICT(thread_id, turn_id) DO UPDATE SET \
          total_tokens=excluded.total_tokens, input_tokens=excluded.input_tokens, \
          cached_input_tokens=excluded.cached_input_tokens, output_tokens=excluded.output_tokens, \
          reasoning_output_tokens=excluded.reasoning_output_tokens, \
          last_total_tokens=excluded.last_total_tokens, last_input_tokens=excluded.last_input_tokens, \
          last_cached_input_tokens=excluded.last_cached_input_tokens, \
          last_output_tokens=excluded.last_output_tokens, \
          last_reasoning_output_tokens=excluded.last_reasoning_output_tokens, \
          model_context_window=excluded.model_context_window, raw_payload=excluded.raw_payload, \
          updated_at=excluded.updated_at",
        rusqlite::params![
            thread_id,
            turn_id,
            n(total, "totalTokens"),
            n(total, "inputTokens"),
            n(total, "cachedInputTokens"),
            n(total, "outputTokens"),
            n(total, "reasoningOutputTokens"),
            n(last, "totalTokens"),
            n(last, "inputTokens"),
            n(last, "cachedInputTokens"),
            n(last, "outputTokens"),
            n(last, "reasoningOutputTokens"),
            model_ctx,
            raw,
            now,
        ],
    )?;
    Ok(())
}

// ── turn-diff (in-memory buffer + flush on turn/completed) ───────────────────

fn spawn_turn_diff(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_notifications();
    tokio::spawn(async move {
        // Buffer owned by this task: turnKey → (threadId, turnId, diff).
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
                                        if let Err(e) = persist_turn_diff(&db, &tid, &uid, &diff) {
                                            tracing::warn!("turn diff persist failed: {e}");
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("turn-diff subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn persist_turn_diff(db: &Db, thread_id: &str, turn_id: &str, diff: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "INSERT INTO turn_diffs (thread_id, turn_id, diff, updated_at) \
         VALUES (?1,?2,?3,?4) \
         ON CONFLICT(thread_id, turn_id) DO UPDATE SET diff=excluded.diff, updated_at=excluded.updated_at",
        rusqlite::params![thread_id, turn_id, diff, now],
    )?;
    Ok(())
}

// ── turn-errors ──────────────────────────────────────────────────────────────

fn spawn_turn_errors(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_notifications();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    let Some(params) = msg.get("params") else {
                        continue;
                    };
                    match method {
                        "error" => {
                            // Only final errors (willRetry falsy) with threadId+turnId.
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
                                if let Err(e) = upsert_turn_error(&db, t, u, message) {
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
                                if let Err(e) = upsert_turn_error(&db, t, u, m) {
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

fn upsert_turn_error(db: &Db, thread_id: &str, turn_id: &str, message: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "INSERT INTO turn_errors (thread_id, turn_id, message, created_at) \
         VALUES (?1,?2,?3,?4) \
         ON CONFLICT(thread_id, turn_id) DO UPDATE SET message=excluded.message, created_at=excluded.created_at",
        rusqlite::params![thread_id, turn_id, message, now],
    )?;
    Ok(())
}

// ── pending-approvals: record server requests ───────────────────────────────

fn spawn_pending_record(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_server_requests();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(req) => {
                    if let Err(e) = record_server_request(&db, &codex, &req) {
                        tracing::warn!("record server request failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("pending-record subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn record_server_request(db: &Db, codex: &CodexProcessManager, req: &Value) -> Result<()> {
    let params = req.get("params");
    let thread_id = params
        .and_then(|p| p.get("threadId"))
        .and_then(Value::as_str);
    let id = req.get("id");
    let method = req.get("method").and_then(Value::as_str);

    // Skip if missing threadId / id / method (parity: TS returns null).
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

    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "INSERT INTO pending_server_requests \
         (generation, request_id, thread_id, turn_id, item_id, method, params_json, status, \
          resolved_by, created_at, updated_at, resolved_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',NULL,?8,?9,NULL) \
         ON CONFLICT(generation, request_id) DO UPDATE SET \
          thread_id=excluded.thread_id, turn_id=excluded.turn_id, item_id=excluded.item_id, \
          method=excluded.method, params_json=excluded.params_json, status='pending', \
          updated_at=excluded.updated_at, resolved_at=NULL, resolved_by=NULL",
        rusqlite::params![
            generation,
            request_id,
            thread_id,
            turn_id,
            item_id,
            method,
            params_json,
            now,
            now,
        ],
    )?;
    Ok(())
}

/// Convert a JSON id Value to its DB string form.
fn id_to_string(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ── pending-approvals: serverRequest/resolved → mark resolved ────────────────

fn spawn_pending_resolved(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_notifications();
    tokio::spawn(async move {
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
                    let conn = match db.conn.lock() {
                        Ok(c) => c,
                        Err(e) => { tracing::warn!("pending-resolved db lock: {e}"); continue; }
                    };
                    let _ = conn.execute(
                        "UPDATE pending_server_requests \
                         SET status='resolved', updated_at=?1, resolved_at=?2 \
                         WHERE generation=?3 AND request_id=?4",
                        rusqlite::params![now, now, generation, req_str],
                    );
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("pending-resolved subscriber lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

// ── pending-approvals: expire on lifecycle ───────────────────────────────────

fn spawn_pending_expire(db: Arc<Db>, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_lifecycle();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => match event {
                    LifecycleEvent::Restarting { generation, .. }
                    | LifecycleEvent::Unavailable { generation, .. } => {
                        if let Err(e) = expire_generation(&db, generation as i64) {
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

fn expire_generation(db: &Db, generation: i64) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "UPDATE pending_server_requests SET status='expired', updated_at=?1, resolved_at=?2 \
         WHERE generation=?3 AND status='pending'",
        rusqlite::params![now, now, generation],
    )?;
    Ok(())
}

fn expire_all_pending(db: &Db, _reason: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
    conn.execute(
        "UPDATE pending_server_requests SET status='expired', updated_at=?1, resolved_at=?2 \
         WHERE status='pending'",
        rusqlite::params![now, now],
    )?;
    tracing::debug!("expired stale pending requests: startup");
    Ok(())
}

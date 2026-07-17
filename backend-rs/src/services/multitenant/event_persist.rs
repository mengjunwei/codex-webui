//! team 事件持久化(M4/M3):订阅 codex:events,把 team 维度的事件落 PG。
//!
//! **双保险**(设计 §177):codex 的 server_request(审批)持久化到 pending_server_requests,
//! 前端重连可拉取未处理项,绝不丢;turn 错误落 turn_errors(team_id 隔离)。
//! team_id 从 thread_id 反查 threads 表(内存缓存降低 DB 压力)。

use crate::db::entities::thread::Entity as ThreadEntity;
use crate::db::entity::pending_server_request as psr;
use crate::db::entity::turn_error;
use crate::error::AppError;
use crate::services::multitenant::event_bus::EventBus;
use crate::services::multitenant::now_ms;
use sea_orm::ActiveModelTrait;
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 启动 team 事件持久化 task(订阅 codex:events)。
pub fn spawn_team_event_persistor(bus: Arc<dyn EventBus>, db: DatabaseConnection) {
    tokio::spawn(async move {
        let mut rx = match bus.subscribe("codex:events").await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(error = %e, "persistor subscribe codex:events failed");
                return;
            }
        };
        let cache: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
        tracing::info!("team event persistor started");
        while let Ok(payload) = rx.recv().await {
            let msg: Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Err(e) = handle_event(&db, &msg, &cache).await {
                tracing::warn!(error = %e, "team event persist failed (non-fatal)");
            }
        }
    });
}

async fn handle_event(
    db: &DatabaseConnection,
    msg: &Value,
    cache: &Mutex<HashMap<String, String>>,
) -> Result<(), AppError> {
    let params = msg.get("params");
    let thread_id = params.and_then(|p| p.get("threadId")).and_then(Value::as_str);
    let Some(tid) = thread_id else { return Ok(()); };
    let team_id = match resolve_team(db, tid, cache).await? {
        Some(t) => t,
        None => return Ok(()),
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    // server_request(带 id)→ 审批持久化(双保险)。
    if msg.get("id").is_some() && !method.is_empty() {
        persist_server_request(db, &team_id, msg).await;
    }
    // turn 错误(error 通知)。
    if method == "error" {
        let message = params
            .and_then(|p| p.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str);
        if let Some(m) = message {
            let turn_id = params
                .and_then(|p| p.get("turnId"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            upsert_turn_error(db, &team_id, tid, turn_id, m).await;
        }
    }
    // token 用量 → token_usage_snapshots(team_id)+ 月配额累加(last.totalTokens 增量)。
    if method == "thread/tokenUsage/updated" {
        if let Some(usage) = params.and_then(|p| p.get("tokenUsage")) {
            let turn_id = params
                .and_then(|p| p.get("turnId"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let last_total = usage
                .get("last")
                .and_then(|l| l.get("totalTokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            upsert_token_usage(db, &team_id, tid, turn_id, usage).await;
            if last_total > 0 {
                let _ = crate::services::multitenant::quota::incr_tokens(db, &team_id, last_total)
                    .await;
            }
        }
    }
    Ok(())
}

async fn resolve_team(
    db: &DatabaseConnection,
    thread_id: &str,
    cache: &Mutex<HashMap<String, String>>,
) -> Result<Option<String>, AppError> {
    if let Some(t) = cache.lock().await.get(thread_id).map(|s| s.clone()) {
        return Ok(Some(t));
    }
    let row = ThreadEntity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("persistor query thread: {e}")))?;
    let team = row.map(|r| r.team_id);
    if let Some(ref t) = team {
        cache.lock().await.insert(thread_id.to_string(), t.clone());
    }
    Ok(team)
}

async fn persist_server_request(db: &DatabaseConnection, team_id: &str, msg: &Value) {
    let now = now_ms();
    let request_id = id_to_string(msg.get("id").unwrap_or(&Value::Null));
    let generation = team_generation(team_id);
    let params = msg.get("params");
    let thread_id = params
        .and_then(|p| p.get("threadId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let turn_id = params
        .and_then(|p| p.get("turnId"))
        .and_then(Value::as_str)
        .map(String::from);
    let item_id = params
        .and_then(|p| p.get("itemId"))
        .and_then(Value::as_str)
        .map(String::from);
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params_json = params.map(|p| p.to_string()).unwrap_or_default();

    let existing = psr::Entity::find_by_id((generation, request_id.clone()))
        .one(db)
        .await
        .ok()
        .flatten();
    let team_id = team_id.to_string();
    if let Some(model) = existing {
        let mut am: psr::ActiveModel = model.into();
        am.team_id = Set(Some(team_id));
        am.thread_id = Set(thread_id);
        am.turn_id = Set(turn_id);
        am.item_id = Set(item_id);
        am.method = Set(method);
        am.params_json = Set(params_json);
        am.status = Set("pending".to_string());
        am.resolved_by = Set(None);
        am.resolved_at = Set(None);
        am.updated_at = Set(now);
        let _ = am.update(db).await;
    } else {
        let am = psr::ActiveModel {
            generation: Set(generation),
            request_id: Set(request_id),
            team_id: Set(Some(team_id)),
            thread_id: Set(thread_id),
            turn_id: Set(turn_id),
            item_id: Set(item_id),
            method: Set(method),
            params_json: Set(params_json),
            status: Set("pending".to_string()),
            resolved_by: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            resolved_at: Set(None),
        };
        let _ = am.insert(db).await;
    }
}

async fn upsert_turn_error(
    db: &DatabaseConnection,
    team_id: &str,
    thread_id: &str,
    turn_id: &str,
    message: &str,
) {
    let now = now_ms();
    let existing = turn_error::Entity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .ok()
        .flatten();
    if let Some(model) = existing {
        let mut am: turn_error::ActiveModel = model.into();
        am.team_id = Set(Some(team_id.to_string()));
        am.message = Set(message.to_string());
        am.created_at = Set(now);
        let _ = am.update(db).await;
    } else {
        let am = turn_error::ActiveModel {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            team_id: Set(Some(team_id.to_string())),
            message: Set(message.to_string()),
            created_at: Set(now),
        };
        let _ = am.insert(db).await;
    }
}

/// 用量字段读取:从可选 JSON 对象按 key 取 i64,缺省 0。
fn read_i64(o: Option<&Value>, k: &str) -> i64 {
    o.and_then(|v| v.get(k)).and_then(Value::as_i64).unwrap_or(0)
}

/// upsert token_usage_snapshots(team_id 隔离;字段对齐 codex tokenUsage)。
async fn upsert_token_usage(
    db: &DatabaseConnection,
    team_id: &str,
    thread_id: &str,
    turn_id: &str,
    usage: &Value,
) {
    use crate::db::entity::token_usage_snapshot::{ActiveModel as TusActive, Entity as TusEntity};
    let total = usage.get("total");
    let last = usage.get("last");
    let model_ctx = usage.get("modelContextWindow").and_then(Value::as_i64);
    let raw = serde_json::to_string(usage).unwrap_or_default();
    let now = now_ms();

    let existing = TusEntity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .ok()
        .flatten();
    let team = team_id.to_string();
    if let Some(model) = existing {
        let mut am: TusActive = model.into();
        am.team_id = Set(Some(team));
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
        let _ = am.update(db).await;
    } else {
        let am = TusActive {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            team_id: Set(Some(team)),
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
        let _ = am.insert(db).await;
    }
}

/// JSON Value id → 字符串(数字/字符串/其他)。
pub fn id_to_string(id: &Value) -> String {
    if let Some(n) = id.as_i64() {
        n.to_string()
    } else if let Some(s) = id.as_str() {
        s.to_string()
    } else {
        id.to_string()
    }
}

/// team 稳定哈希 → generation(pending_server_requests 主键前半,隔离不同 team 的 request_id)。
pub fn team_generation(team_id: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    team_id.hash(&mut h);
    (h.finish() as i64).abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_stable_and_positive() {
        let a = team_generation("team-abc");
        let b = team_generation("team-abc");
        let c = team_generation("team-xyz");
        assert_eq!(a, b, "stable per team");
        assert!(a >= 0);
        assert_ne!(a, c, "different teams differ");
    }

    #[test]
    fn id_to_string_variants() {
        assert_eq!(id_to_string(&Value::from(42)), "42");
        assert_eq!(id_to_string(&Value::from("req-7")), "req-7");
    }
}

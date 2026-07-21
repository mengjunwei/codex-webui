//! team 事件持久化(M4/M3):订阅 codex:events,把 team 维度的事件落 PG。
//!
//! **双保险**(设计 §177):codex 的 server_request(审批)持久化到 pending_server_requests,
//! 前端重连可拉取未处理项,绝不丢;turn 错误落 turn_errors(team_id 隔离)。
//! team_id 从 thread_id 反查 threads 表(内存缓存降低 DB 压力)。

use crate::db::entities::thread::Entity as ThreadEntity;
use crate::db::entity::pending_server_request as psr;
use crate::db::entity::turn_diff;
use crate::db::entity::turn_error;
use crate::error::AppError;
use crate::services::multitenant::event_bus::EventBus;
use crate::services::multitenant::now_ms;
use sea_orm::ActiveModelTrait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 启动 team 事件持久化 task(订阅 codex:events)。
///
/// `node_id` 用于多副本 HA 的 primary 守门:Redis Pub/Sub fan-out 让所有节点收到同一事件,
/// 若每节点都落库会导致审批重复 N 行(只有本节点 generation 命中的 1 行能被 resolve,
/// 其余变幽灵)+ token 配额累加 N 次。只有 team 的主节点(primary_node==本节点)处理该
/// team 事件;无 session_replica 行(单节点/无 HA)时本节点即主,放行。
pub fn spawn_team_event_persistor(
    bus: Arc<dyn EventBus>,
    db: DatabaseConnection,
    node_id: String,
) {
    tokio::spawn(async move {
        // 启动时清 stale(>1h)pending 审批:多副本 HA(共享 PG)下不能全表 expire(会清其他
        // 节点活审批 → 审批双保险失效),只清 created_at 早于 1h 阈值的残留(上次运行/已失效 turn)。
        if let Err(e) = expire_stale_pending(&db).await {
            tracing::warn!(error = %e, "startup expire stale pending failed");
        }
        let mut rx = match bus.subscribe("codex:events").await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(error = %e, "persistor subscribe codex:events failed");
                return;
            }
        };
        let cache: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
        tracing::info!(node_id = %node_id, "team event persistor started");
        // 关键:Lagged 表示消费方落后、旧消息被丢弃但通道仍存活,必须 continue。
        // 原 `while let Ok` 把 Lagged 误当退出 → 一次积压后持久化 task 永久死亡,
        // 本节点所有审批/turn 错误/token 用量不再落 PG,quota 停止累加。
        loop {
            let payload = match rx.recv().await {
                Ok(p) => p,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(lagged = n, "event_persist lagged, skipping");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let msg: Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Err(e) = handle_event(&db, &msg, &cache, &node_id).await {
                tracing::warn!(error = %e, "team event persist failed (non-fatal)");
            }
        }
    });
}

async fn handle_event(
    db: &DatabaseConnection,
    msg: &Value,
    cache: &Mutex<HashMap<String, String>>,
    node_id: &str,
) -> Result<(), AppError> {
    let params = msg.get("params");
    let thread_id = params.and_then(|p| p.get("threadId")).and_then(Value::as_str);
    let Some(tid) = thread_id else { return Ok(()); };
    let team_id = match resolve_team(db, tid, cache).await? {
        Some(t) => t,
        None => return Ok(()),
    };
    // primary 守门(fan-out 去重):仅 team 主节点处理该 team 事件。
    // 多副本 HA 下 Redis Pub/Sub 把主节点 codex 的事件 fan-out 给所有节点;若都落库会导致
    // 审批重复 N 行(resolve 只清本节点 generation 那 1 行,其余幽灵)+ token 配额累加 N 次。
    // 无 session_replica 行(单节点/无 HA)时本节点即主,放行。
    if let Some(replica_row) =
        crate::services::multitenant::replication::get(db, &team_id).await?
    {
        if replica_row.primary_node != node_id {
            return Ok(()); // 非主节点,跳过(fan-out 去重)。
        }
    }
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    // server_request(带 id)→ 审批持久化(双保险)。
    if msg.get("id").is_some() && !method.is_empty() {
        // 双保险:bus 路径 persist 失败不阻断 event_persist 主循环(审批落库主路径是
        // realtime::record_server_request,它 propagate 错误使 realtime 跳过 emit 防幽灵)。
        let _ = persist_server_request(db, &team_id, msg).await;
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
    // turn diff → turn_diffs(team_id 隔离)。Bug5:此前 event_persist 漏处理 turn/diff/updated,
    // 多租户模式(per-team codex 事件经 Redis bus 来)下 turn_diffs 表永不写入,刷新/重连后历史 diff 丢失。
    // 直接 upsert(覆盖式):同 turn 多次 diff 更新取最新,等价 legacy 的缓冲+turn/completed 刷写。
    if method == "turn/diff/updated" {
        if let (Some(turn_id), Some(diff)) = (
            params.and_then(|p| p.get("turnId")).and_then(Value::as_str),
            params.and_then(|p| p.get("diff")).and_then(Value::as_str),
        ) {
            upsert_turn_diff(db, &team_id, tid, turn_id, diff).await;
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
        let mut c = cache.lock().await;
        // 防无界增长(thread_id 单调累积,数月可达数十 MB):超阈值清空(重新查 DB,性能可接受)。
        const CACHE_CAP: usize = 50_000;
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(thread_id.to_string(), t.clone());
    }
    Ok(team)
}

async fn persist_server_request(
    db: &DatabaseConnection,
    team_id: &str,
    msg: &Value,
) -> Result<(), AppError> {
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
        .map_err(|e| AppError::internal(format!("find pending_server_request: {e}")))?;
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
        am.update(db)
            .await
            .map_err(|e| AppError::internal(format!("update pending_server_request: {e}")))?;
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
        am.insert(db)
            .await
            .map_err(|e| AppError::internal(format!("insert pending_server_request: {e}")))?;
    }
    Ok(())
}

/// 记录审批请求(team 维度,team_generation 主键)。供 realtime 在 WS emit 之前调用,
/// 保证客户端立即 respond 时行已存在(TOCTOU:realtime 直订阅 codex 快于 bus 路径,先落库)。
///
/// 与 handle_event → persist_server_request(经 bus)用同一 team_generation(team_id) 主键,
/// 两路 upsert 合并为同一行 —— 消除原 event_subscribers::record_server_request 用 codex.generation()
/// + team_id=None 与 event_persist team_generation 双写产生的孤儿行(team_id=None 行 list/resolve
/// 命不中)+ TOCTOU 失效(realtime 落 codex.generation() 主键,handlers resolve 用 team_generation
/// 命不中)。
pub async fn record_server_request(db: &DatabaseConnection, msg: &Value) -> Result<(), AppError> {
    let params = msg.get("params");
    let thread_id = params.and_then(|p| p.get("threadId")).and_then(Value::as_str);
    let Some(tid) = thread_id else { return Ok(()); };
    // 反查 team_id(无缓存:审批请求低频,每次查 DB 可接受)。
    let row = ThreadEntity::find_by_id(tid.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("record_server_request query thread: {e}")))?;
    let Some(team_id) = row.map(|r| r.team_id) else { return Ok(()); };
    persist_server_request(db, &team_id, msg).await?;
    Ok(())
}

/// 启动时把 status='pending' 且较旧(stale > 1h)的审批批量过期。
/// 多副本 HA(共享 PG)下不能全表 expire(会清其他节点活审批 → mt_list_approvals 返回空 →
/// 审批双保险失效),只清 created_at 早于 1h 阈值的残留(上次运行/已失效 turn)。
async fn expire_stale_pending(db: &DatabaseConnection) -> Result<(), AppError> {
    use crate::db::entity::pending_server_request::{Column as PSRColumn, Entity as PSREntity};
    let now = now_ms();
    const STALE_MS: i64 = 3600 * 1000; // 1h:超过视为残留,活审批(近期)保留。
    let cutoff = now - STALE_MS;
    let rows = PSREntity::find()
        .filter(PSRColumn::Status.eq("pending"))
        .filter(PSRColumn::CreatedAt.lt(cutoff))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("find stale pending: {e}")))?;
    let expired_count = rows.len();
    for model in rows {
        let mut am: psr::ActiveModel = model.into();
        am.status = Set("expired".to_string());
        am.updated_at = Set(now);
        am.resolved_at = Set(Some(now));
        let _ = am.update(db).await;
    }
    tracing::debug!(expired_count, "expired stale pending requests: startup");
    Ok(())
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

/// upsert turn_diffs(team_id 隔离)。同 turn 多次更新取最新(覆盖式)。
async fn upsert_turn_diff(
    db: &DatabaseConnection,
    team_id: &str,
    thread_id: &str,
    turn_id: &str,
    diff: &str,
) {
    let now = now_ms();
    let existing = turn_diff::Entity::find_by_id((thread_id.to_string(), turn_id.to_string()))
        .one(db)
        .await
        .ok()
        .flatten();
    if let Some(model) = existing {
        let mut am: turn_diff::ActiveModel = model.into();
        am.team_id = Set(Some(team_id.to_string()));
        am.diff = Set(diff.to_string());
        am.updated_at = Set(now);
        let _ = am.update(db).await;
    } else {
        let am = turn_diff::ActiveModel {
            thread_id: Set(thread_id.to_string()),
            turn_id: Set(turn_id.to_string()),
            team_id: Set(Some(team_id.to_string())),
            diff: Set(diff.to_string()),
            updated_at: Set(now),
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

/// team 稳定哈希 + 进程启动 nonce → generation(pending_server_requests 主键前半,
/// 隔离不同 team 的 request_id)。
///
/// 关键:必须混入进程级 nonce。否则 codex 进程重启(idle_evict/key 轮换/崩溃)后 jsonrpc
/// next_id 重置为 1,新审批复用旧 request_id → upsert 命中旧行 → 覆盖历史审批记录。
/// 加 nonce 后,本进程内同 team 稳定(mark_approval_resolved 能查到),跨进程不同(防复用)。
/// failover 后新主节点 nonce 不同,查不到旧主的 pending(但旧主审批已随 codex 重启失效)。
static GEN_NONCE: once_cell::sync::Lazy<u64> = once_cell::sync::Lazy::new(|| {
    // 进程启动时间(ns)作为 nonce:每次重启不同,同进程稳定。
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
});

pub fn team_generation(team_id: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    team_id.hash(&mut h);
    let mixed = h.finish() ^ *GEN_NONCE;
    // 清最高位确保非负(主键列);主键值唯一性由 (generation, request_id) 复合保证。
    (mixed & 0x7FFF_FFFF_FFFF_FFFF) as i64
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

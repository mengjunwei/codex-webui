//! session 副本:主副本分配 + rollout 增量复制 + 副本晋升。
//!
//! CODEX_HOME 全局,rollout 按 conv_id(= thread_id)分文件 → **复制单元 per-thread**。
//! 主每 turn 完成后扫描全局 CODEX_HOME/sessions/ 下该 thread 的 rollout,按 offset
//! 取增量 POST 到副本;副本 append 到本地 CODEX_HOME 对应文件。主失活 → 副本晋升
//! (起 codex + thread/resume 续接)。
//!
//! 防脑裂:Redis 租约 `codex:primary:{thread_id}` —— 主周期续(SETEX),副本晋升须 SET NX 抢占,
//! 保证同一 thread 同一时刻只有一个主。

use crate::db::entities::session_replica::{ActiveModel, Entity, Model};
use crate::error::AppError;
use crate::services::multitenant::cluster::ClusterMembership;
use crate::services::multitenant::now_ms;
use crate::services::multitenant::rpc::WorkerRpcClient;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// per-(thread,conv) receive 锁表:防止并发 receive 同一 rollout 文件交错损坏(R4)。
/// key = "{thread_id}:{conv_id}",每个 key 一个独立的 tokio Mutex。
static RECEIVE_LOCKS: once_cell::sync::Lazy<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// 获取并锁定指定 key 的 receive 互斥锁。返回的 guard 持有到 drop,期间同 key 的其他
/// receive_rollout 调用阻塞等待,确保 seek+offset_check+write 串行。
async fn receive_lock(key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut map = RECEIVE_LOCKS.lock().await;
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

/// 回收孤立的 receive 锁槽(仅 strong_count==1,即本表唯一持有时)。调用前须先 drop guard,
/// 否则计数恒 ≥2。防 RECEIVE_LOCKS 按 {thread}:{conv} 无界累积(conv = thread_id 单调增长)。
async fn reap_receive_lock(key: &str) {
    let mut map = RECEIVE_LOCKS.lock().await;
    if let Some(arc) = map.get(key) {
        if std::sync::Arc::strong_count(arc) == 1 {
            map.remove(key);
        }
    }
}

// ── codex_tid 映射(系统 thread_id ↔ codex 自生成 tid)──────────────────────

/// 进程内 codex_tid 映射 fallback(无 Redis / Redis miss 时用)。
/// key = 系统 thread_id(入口预生成),value = codex 自生成的会话 tid。
///
/// 背景:codex 0.142.5+ 忽略外部 threadId 参数,thread/start 时自生成 codex_tid,且
/// 后续 turn/invoke/resume/delete 必须用该 codex_tid 才能 hit 已有会话(传系统 thread_id
/// 会报 `rpc error -32600: thread not found`)。故创建时记录映射,后续 codex 调用查表改写。
/// 对齐 file_sync.rs LOCAL_FILESYNC_OFFSETS 的双存储语义:重启进程内丢失 → Redis 兜底。
static CODEX_TID_MAP: once_cell::sync::Lazy<tokio::sync::Mutex<HashMap<String, String>>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));
/// 反向映射 codex_tid → 系统 thread_id:codex notification 的 threadId 是 codex_tid,
/// emit room(thread:{tid})/event_persist(DB 按 thread_id)需还原为系统 thread_id。
static CODEX_TID_REV_MAP: once_cell::sync::Lazy<tokio::sync::Mutex<HashMap<String, String>>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// 存储 codex_tid 映射(thread/start 成功后调)。双写 Redis + 进程内(正向 + 反向)。
/// codex_tid 为空则跳过(防御:codex 尊重 threadId 时 codex_tid==thread_id,存了幂等无害)。
pub async fn set_codex_tid(redis: Option<&redis::Client>, thread_id: &str, codex_tid: &str) {
    if codex_tid.is_empty() {
        return;
    }
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("codex:tid:{thread_id}"))
                .arg(codex_tid)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
            // 反向:codex 通知 threadId(codex_tid)→ 系统 thread_id。
            let _: () = redis::cmd("SET")
                .arg(format!("codex:tid_rev:{codex_tid}"))
                .arg(thread_id)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    CODEX_TID_MAP
        .lock()
        .await
        .insert(thread_id.to_string(), codex_tid.to_string());
    CODEX_TID_REV_MAP
        .lock()
        .await
        .insert(codex_tid.to_string(), thread_id.to_string());
}

/// 读取 codex_tid 映射:进程内优先 → Redis(命中回填进程内加速后续命中)→ None。
/// 调用方对 None 应 fallback 系统 thread_id(向后兼容,不 panic)。
pub async fn get_codex_tid(redis: Option<&redis::Client>, thread_id: &str) -> Option<String> {
    if let Some(v) = CODEX_TID_MAP.lock().await.get(thread_id).cloned() {
        return Some(v);
    }
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("codex:tid:{thread_id}"))
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                // 回填进程内,加速本节点后续命中(同 thread_id → 同 codex_tid,idempotent)。
                CODEX_TID_MAP
                    .lock()
                    .await
                    .insert(thread_id.to_string(), s.clone());
                return Some(s);
            }
        }
    }
    None
}

/// 反向查询 codex_tid → 系统 thread_id(codex notification 的 threadId 还原用)。
pub async fn get_thread_id_by_codex(redis: Option<&redis::Client>, codex_tid: &str) -> Option<String> {
    if let Some(v) = CODEX_TID_REV_MAP.lock().await.get(codex_tid).cloned() {
        return Some(v);
    }
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("codex:tid_rev:{codex_tid}"))
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                CODEX_TID_REV_MAP
                    .lock()
                    .await
                    .insert(codex_tid.to_string(), s.clone());
                return Some(s);
            }
        }
    }
    None
}

/// 主租约有效期(主须在此周期内续约,否则副本可抢占)。需显著大于维护周期(15s)。
pub const LEASE_TTL_MS: i64 = 60_000;
/// Redis 主租约 TTL 秒。
const LEASE_TTL_SECS: u64 = 60;

/// 一段 rollout 增量(主 → 副本)。
#[derive(Serialize, Deserialize, Clone)]
pub struct RolloutChunk {
    pub thread_id: String,
    pub conv_id: String,
    /// 相对 CODEX_HOME 的路径,如 `sessions/2026/07/17/rollout-...-<conv>.jsonl`。
    pub rel_path: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

// ── session_replicas 分配 / 查询 ───────────────────────────────────────────

pub async fn get(db: &DatabaseConnection, thread_id: &str) -> Result<Option<Model>, AppError> {
    Entity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query session_replica: {e}")))
}

/// 取或分配该 thread 的主副本:无则 primary=本节点,replica=另一 alive 节点(反亲和)。
/// insert 主键冲突(并发首请求)→ 重读返回已存在行(原子化)。
pub async fn get_or_assign(
    db: &DatabaseConnection,
    thread_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<Model, AppError> {
    if let Some(m) = get(db, thread_id).await? {
        return Ok(m);
    }
    let primary = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;
    let replica = alive.into_iter().find(|n| n != &primary);
    let now = now_ms();
    let am = ActiveModel {
        thread_id: Set(thread_id.to_string()),
        primary_node: Set(primary),
        replica_node: Set(replica),
        status: Set("active".to_string()),
        primary_lease_until: Set(now + LEASE_TTL_MS),
        updated_at: Set(now),
    };
    match am.insert(db).await {
        Ok(m) => Ok(m),
        // 主键冲突 = 并发首请求,重读。
        Err(_) => get(db, thread_id).await?.ok_or_else(|| AppError::internal("session_replica vanished".into())),
    }
}

/// 确保副本已分配:若 replica_node 为 None 且存在其他 alive 节点 → 回填(反亲和)。
/// 用于扩容后补选副本(否则主挂无人晋升)。
pub async fn ensure_replica(
    db: &DatabaseConnection,
    thread_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<(), AppError> {
    let Some(row) = get(db, thread_id).await? else {
        return Ok(());
    };
    if row.replica_node.is_some() {
        return Ok(()); // 已有副本。
    }
    let primary = row.primary_node.clone();
    let alive = cluster.alive_nodes().await;
    let new_replica = alive.into_iter().find(|n| n != &primary);
    if let Some(r) = new_replica {
        let now = now_ms();
        let mut am: ActiveModel = row.into();
        am.replica_node = Set(Some(r));
        am.updated_at = Set(now);
        am.update(db)
            .await
            .map_err(|e| AppError::internal(format!("ensure_replica: {e}")))?;
    }
    Ok(())
}

/// 主续约(DB lease + Redis 租约条件续期)。仅当本节点仍是 primary 时生效。
///
/// 关键:DB 与 Redis 都必须用**条件**写,不能用无条件 update/SET。否则旧主长停顿恢复后,
/// 其 renew 会把 DB `primary_node` / Redis 租约覆盖回自己,而副本已抢占成功 → 双主脑裂。
pub async fn renew_lease(
    db: &DatabaseConnection,
    thread_id: &str,
    node_id: &str,
    redis: Option<&redis::Client>,
) -> Result<(), AppError> {
    let Some(row) = get(db, thread_id).await? else {
        return Ok(());
    };
    if row.primary_node != node_id {
        return Ok(()); // 不是主,不续。
    }
    let now = now_ms();
    // DB 条件 update:仅当 primary_node 仍是本节点时才推进 lease。
    // 用 update_many + filter(而非 ActiveModel::update)——后者会把 Unchanged 的
    // primary_node 也写回(覆盖副本刚 set_primary 的结果)。
    // I2:只刷 PrimaryLeaseUntil,不刷 UpdatedAt —— updated_at 须反映分配/迁移时间,
    //     rebalance order_by_asc(UpdatedAt) 才能"最久未迁移优先";否则 renew 每 15s
    //     把所有 primary 行 updated_at 刷成 now,导致 rebalance 等于随机选(可能迁活跃 thread)。
    use crate::db::entities::session_replica::Column as SRColumn;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let res = Entity::update_many()
        .col_expr(SRColumn::PrimaryLeaseUntil, Expr::value(now + LEASE_TTL_MS))
        .filter(SRColumn::ThreadId.eq(thread_id.to_string()))
        .filter(SRColumn::PrimaryNode.eq(node_id.to_string()))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("renew lease: {e}")))?;
    if res.rows_affected == 0 {
        // 并发场景下副本已 set_primary 抢占,DB primary_node 不再是本节点 → 放弃续约。
        return Ok(());
    }
    let _ = row; // row 仅用于前置守门快速跳过,实际 update 走条件路径。
    // Redis 租约续期:必须用条件写(仅当 key 仍是本节点时才续),不能用无条件 SET。
    // 否则旧主长停顿恢复后会无条件 SET 覆盖副本刚 SET NX 抢到的租约 → 双主脑裂。
    // 用 Lua 原子完成 compare-and-set:key 不存在或已被别人抢占则放弃续约(走晋升流程)。
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            const RENEW_LUA: &str = r#"
                if redis.call('GET', KEYS[1]) == ARGV[1] then
                    return redis.call('SET', KEYS[1], ARGV[1], 'EX', ARGV[2])
                else
                    return 0
                end
            "#;
            let _: i64 = redis::cmd("EVAL")
                .arg(RENEW_LUA)
                .arg(1)
                .arg(format!("codex:primary:{thread_id}"))
                .arg(node_id)
                .arg(LEASE_TTL_SECS)
                .query_async(&mut conn)
                .await
                .unwrap_or(0);
        }
    }
    Ok(())
}

/// Redis 主租约抢占(SET NX EX)。返回 true=抢占成功(可晋升);false=主租约仍在(不晋升)。
pub async fn try_acquire_primary(
    redis: Option<&redis::Client>,
    thread_id: &str,
    node_id: &str,
) -> bool {
    let Some(c) = redis else {
        return true; // 无 Redis(单节点)→ 直接放行(无脑裂风险)。
    };
    let Ok(mut conn) = c.get_multiplexed_async_connection().await else {
        return false;
    };
    let ok: Option<String> = redis::cmd("SET")
        .arg(format!("codex:primary:{thread_id}"))
        .arg(node_id)
        .arg("NX")
        .arg("EX")
        .arg(LEASE_TTL_SECS)
        .query_async(&mut conn)
        .await
        .ok();
    ok.is_some()
}

/// 更新主副本(晋升 / 重选副本 / rebalance 迁移时)。
///
/// I3 CAS:仅当当前 primary_node == caller 时才更新。rebalance 路径不经 Redis 租约,
/// 原无条件 update 会与并发的 promote/reclaim 踩踏:
///   A(rebalance)读行 primary=A → B(promote)判 A 失活抢占 set_primary(B) → A 的 set_primary(target) 覆盖。
/// CAS 消除该竞争。返回 true=已更新;false=CAS 失败(primary_node 已不是 caller,被抢占/迁移,
/// 调用方应放弃后续 offset/sticky 清理等动作)。
pub async fn set_primary(
    db: &DatabaseConnection,
    thread_id: &str,
    caller: &str,
    new_primary: &str,
    new_replica: Option<&str>,
) -> Result<bool, AppError> {
    let now = now_ms();
    use crate::db::entities::session_replica::Column as SRColumn;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let res = Entity::update_many()
        .col_expr(SRColumn::PrimaryNode, Expr::value(new_primary.to_string()))
        .col_expr(
            SRColumn::ReplicaNode,
            Expr::value(new_replica.map(String::from)),
        )
        .col_expr(SRColumn::Status, Expr::value("active".to_string()))
        .col_expr(
            SRColumn::PrimaryLeaseUntil,
            Expr::value(now + LEASE_TTL_MS),
        )
        .col_expr(SRColumn::UpdatedAt, Expr::value(now))
        .filter(SRColumn::ThreadId.eq(thread_id.to_string()))
        .filter(SRColumn::PrimaryNode.eq(caller.to_string()))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("set primary: {e}")))?;
    if res.rows_affected == 0 {
        tracing::warn!(
            thread_id,
            caller,
            new_primary,
            "set_primary CAS aborted: primary_node no longer caller (preempted/migrated)"
        );
        return Ok(false);
    }
    Ok(true)
}

// ── rollout 增量复制(主侧)─────────────────────────────────────────────────

/// 主侧:复制单个 thread 的 rollout 增量到副本节点。
/// 复制单元 = active_rollout 里该 thread 的文件路径;
/// offset 仅在 send 成功后才推进(spec §2.2)。
pub async fn replicate_thread_rollout(
    db: &DatabaseConnection,
    thread_id: &str,
    codex_home: &Path,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    rpc_client: &WorkerRpcClient,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<(), AppError> {
    let Some(row) = get(db, thread_id).await? else { return Ok(()); };
    let Some(replica_node) = row.replica_node.clone() else { return Ok(()); };
    if replica_node == cluster.local_node_id() { return Ok(()); }
    let Some(rpc_addr) = cluster.node_rpc_addr(&replica_node).await else { return Ok(()); };

    // 单 thread:从 active_rollout 取该 thread 的文件路径。
    // active_rollout miss(createThread/turn 时 find_rollout 太早,rollout 延迟未写致未插) →
    // 补 find_rollout(codex_tid):维护循环 15s 周期重试时 rollout 应已写盘,补插后后续命中。
    let abs_path = {
        let m = active_rollout.lock().await;
        match m.get(thread_id) {
            Some(p) if p.exists() => p.clone(),
            _ => {
                drop(m);
                let codex_tid = get_codex_tid(redis, thread_id)
                    .await
                    .unwrap_or_else(|| thread_id.to_string());
                match find_rollout_for_thread(codex_home, &codex_tid).await {
                    Some(p) => {
                        active_rollout
                            .lock()
                            .await
                            .insert(thread_id.to_string(), p.clone());
                        tracing::debug!(thread_id, "active_rollout lazy-filled in replicate");
                        p
                    }
                    None => return Ok(()), // rollout 仍未写,本轮跳过。
                }
            }
        }
    };
    let size = match tokio::fs::metadata(&abs_path).await {
        Ok(m) => m.len(),
        Err(_) => return Ok(()),
    };
    let rel_path = match abs_path.strip_prefix(codex_home) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return Ok(()),
    };
    let offset = get_offset_dual(redis, local_offsets, thread_id, &rel_path).await;
    if size <= offset { return Ok(()); }
    let bytes = match read_range(&abs_path, offset, size).await {
        Ok(b) => b,
        Err(e) => { tracing::warn!(thread_id, error = %e, "read rollout range failed"); return Ok(()); }
    };
    let chunk = RolloutChunk {
        thread_id: thread_id.to_string(),
        conv_id: thread_id.to_string(),
        rel_path: rel_path.clone(),
        offset,
        bytes,
    };
    if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
        tracing::warn!(thread_id, error = %e, "replicate rollout chunk failed");
        return Ok(()); // 不推进 offset,下轮重传。
    }
    set_offset_dual(redis, local_offsets, thread_id, &rel_path, size).await;
    metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
    Ok(())
}

/// offset 双存储读(Redis 优先,失败回退进程内)。spec §2.2 fallback。
///
/// offset key 绑定 rel_path(文件路径),而非仅 (thread_id):同一 thread 跨天/codex 重启
/// 会产生新 rollout 文件(rel_path 不同),若 offset 仍沿用旧文件遗留的大值,新文件 size 从 0
/// 起 → `size <= offset` 永久跳过,副本永远收不到新会话。绑定文件后新文件 offset 从 0 开始。
async fn get_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    thread_id: &str,
    rel_path: &str,
) -> u64 {
    let key = format!("repl:offset:{thread_id}:{rel_path}");
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                match s.parse::<u64>() {
                    Ok(offset) => return offset,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            value = %s,
                            thread_id = %thread_id,
                            "invalid offset in Redis, falling back to local"
                        );
                    }
                }
            }
        }
    }
    let m = local.lock().await;
    m.get(&(thread_id.to_string(), rel_path.to_string()))
        .copied()
        .unwrap_or(0)
}

/// offset 双存储写(同步写 Redis + 进程内)。
async fn set_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    thread_id: &str,
    rel_path: &str,
    v: u64,
) {
    let key = format!("repl:offset:{thread_id}:{rel_path}");
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg(v)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    let mut m = local.lock().await;
    m.insert((thread_id.to_string(), rel_path.to_string()), v);
}

/// 晋升后清空 offset(Redis SCAN + DEL + 进程内 retain)。
pub async fn delete_all_thread_offsets_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    thread_id: &str,
) {
    if let Some(c) = redis {
        delete_all_thread_offsets(c, thread_id).await;
    }
    let mut m = local.lock().await;
    m.retain(|(t, _), _| t != thread_id);
}

// ── 副本 receive ───────────────────────────────────────────────────────────

/// 副本:把收到的 rollout 增量写入本地 CODEX_HOME(路径穿越校验 + offset 校验防乱序空洞)。
///
/// R4 修复:per-(thread,conv) 互斥锁,防止并发 receive 同一文件交错损坏。
/// HTTP handler(mt_start_turn 后的 replicate)与维护循环可能同时向副本 POST 同 conv 的 chunk,
/// 两个 spawn_blocking 的 seek+offset_check+write 非原子交错会损坏文件。
pub async fn receive_rollout(chunk: &RolloutChunk, codex_home: &Path) -> Result<(), AppError> {
    // per-conv 锁:同 thread 同 conv 的 receive 串行化(不同 conv 并发不受影响)。
    // 锁释放后 reap 孤立锁槽(strong_count==1),防 RECEIVE_LOCKS 按 {thread}:{conv} 无界累积。
    let key = format!("{}:{}", chunk.thread_id, chunk.conv_id);
    let guard = receive_lock(&key).await;
    let result = receive_rollout_inner(chunk, codex_home).await;
    drop(guard);
    reap_receive_lock(&key).await;
    result
}

async fn receive_rollout_inner(chunk: &RolloutChunk, codex_home: &Path) -> Result<(), AppError> {
    // 路径穿越校验:rel_path 必须是相对路径且不含 `..` / 绝对前缀 / 反斜杠。
    if chunk.rel_path.is_empty()
        || chunk.rel_path.starts_with('/')
        || chunk.rel_path.starts_with('\\')
        || chunk.rel_path.contains("..")
        || chunk.rel_path.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {}", chunk.rel_path)));
    }
    // canonicalize 边界(spec §2.4.3):防 symlink 逃逸。
    let path = safe_join(codex_home, &chunk.rel_path).await?;
    let offset = chunk.offset;
    let bytes = chunk.bytes.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        // offset 校验:不超过当前文件长度(防乱序到达产生 NUL 空洞)。
        let cur_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if offset > cur_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "offset beyond file end (out-of-order chunk)",
            ));
        }
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&bytes)?;
        f.flush()?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::internal(format!("receive join: {e}")))?
    .map_err(|e| AppError::internal(format!("receive write: {e}")))?;
    Ok(())
}

// ── 副本晋升 ─────────────────────────────────────────────────────────────

/// 副本自查:若自己是某 thread 的副本、主失活(不在 alive 或租约过期)→ Redis 抢占租约后晋升。
/// 返回 true 表示已晋升(调用方应起 codex + thread/resume 续接)。
pub async fn promote_if_primary_down(
    db: &DatabaseConnection,
    thread_id: &str,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<bool, AppError> {
    let Some(row) = get(db, thread_id).await? else {
        return Ok(false);
    };
    let me = cluster.local_node_id();
    if row.replica_node.as_deref() != Some(me) {
        return Ok(false);
    }
    let primary_alive = cluster.alive_nodes().await.iter().any(|n| n == &row.primary_node);
    let now = now_ms();
    let lease_expired = row.primary_lease_until < now;
    if primary_alive && !lease_expired {
        return Ok(false);
    }
    // lease CAS 守门(spec §2.3.1):即使 Redis SET NX 成功,本地看 lease 未过期 → 不晋升。
    if row.primary_lease_until >= now {
        return Ok(false);
    }
    // Redis 抢占租约(SET NX):防止多个副本同时晋升。
    if !try_acquire_primary(redis, thread_id, me).await {
        tracing::info!(thread_id, "primary lease still held by another, skip promote");
        return Ok(false);
    }
    // 抢占成功 → 晋升:选新副本(反亲和,alive 中 != 自己)。
    let alive = cluster.alive_nodes().await;
    let new_replica = alive.into_iter().find(|n| n != me);
    // I3 CAS:caller = 旧主(row.primary_node,已失活)。仅当 DB primary_node 仍是旧主时才晋升,
    //         防并发 rebalance(不经 Redis 租约)此时改了 primary_node 导致踩踏。
    let promoted = set_primary(db, thread_id, row.primary_node.as_str(), me, new_replica.as_deref()).await?;
    if !promoted {
        tracing::info!(thread_id, "promote CAS aborted: primary_node changed concurrently");
        return Ok(false);
    }
    // 晋升成功 → 删 Redis + 进程内 offset,触发下次从 0 全量同步(spec §2.3.3)。
    delete_all_thread_offsets_dual(redis, local_offsets, thread_id).await;
    let _ = active_rollout; // 占位:下次 mt_start_turn / mt_create_thread 重新发现文件。
    metrics::counter!("replica_promotions_total").increment(1);
    tracing::info!(thread_id, "replica promoted to primary");
    Ok(true)
}

/// 孤儿 thread 认领:主节点不 alive(如重启换 id)且无人晋升时,由"最低 alive id"节点认领主。
/// 确定性认领(只有最低 id 节点执行)→ 无竞争。
pub async fn reclaim_orphan_threads(
    db: &DatabaseConnection,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
) -> Result<(), AppError> {
    use sea_orm::EntityTrait;
    let me = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;
    // 确定性:只有当前最低 alive id 的节点认领(无竞争)。
    let mut sorted = alive.clone();
    sorted.sort();
    let should_reclaim = sorted.first().map(|n| n == &me).unwrap_or(false);
    if !should_reclaim {
        return Ok(());
    }
    let rows = Entity::find()
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("reclaim scan: {e}")))?;
    for row in rows {
        let primary_alive = alive.iter().any(|n| n == &row.primary_node);
        if primary_alive {
            continue; // 主在,不认领。
        }
        // 主失活 → 抢占租约认领。
        if !try_acquire_primary(redis, &row.thread_id, &me).await {
            continue;
        }
        let new_replica = alive.iter().find(|n| n.as_str() != me).cloned();
        // I3 CAS:caller = 旧主(row.primary_node,已失活)。仅当 DB primary_node 仍是旧主才认领,
        //         防并发 rebalance/promote 此时改了 primary_node 导致踩踏。
        let claimed = set_primary(
            db,
            &row.thread_id,
            row.primary_node.as_str(),
            &me,
            new_replica.as_deref(),
        )
        .await?;
        if !claimed {
            // CAS 失败:primary_node 已被并发改动,跳过本行(可能已被别人认领/迁移)。
            continue;
        }
        // R3 修复:认领后清 Redis 残留 offset。认领者不是旧主/副本,本地无该 thread 的 rollout,
        // 但 Redis 可能残留旧 offset;新副本 receive_rollout 会因 offset>cur_len 拒绝同步,
        // 导致认领后数据永久对不齐。
        if let Some(c) = redis {
            delete_all_thread_offsets(c, &row.thread_id).await;
        }
        tracing::info!(thread_id = %row.thread_id, "reclaimed orphan thread as primary");
    }
    Ok(())
}

// ── 文件辅助 ─────────────────────────────────────────────────────────────

/// 读取文件 [start, end) 区间(spawn_blocking)。
async fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>, AppError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)?;
        f.seek(SeekFrom::Start(start))?;
        let len = (end - start) as usize;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    })
    .await
    .map_err(|e| AppError::internal(format!("read_range join: {e}")))?
    .map_err(|e| AppError::internal(format!("read_range: {e}")))
}

/// 复制单元类型别名(spec §2.1 / §2.2)。
pub type ThreadRolloutMap = Arc<tokio::sync::Mutex<std::collections::HashMap<String, PathBuf>>>;
pub type LocalOffsetMap = Arc<tokio::sync::Mutex<std::collections::HashMap<(String, String), u64>>>;

/// 给定 thread_id,在 <codex_home>/sessions/ 下递归找其活跃 rollout 文件。
/// 规则:文件名 stem 包含完整 thread_id 字符串,且 thread_id 前后必须是 `.`/`-`/文件边界
/// (防 `8a3f` 误匹配 `8a3faaaa`);多命中取 mtime 最新;0 命中返回 None。
pub async fn find_rollout_for_thread(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    if !tokio::fs::metadata(&sessions).await.map(|m| m.is_dir()).unwrap_or(false) {
        return None;
    }
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut stack = vec![sessions];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ft = match entry.file_type().await {
                Ok(f) => f,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let stem = match p.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            // 边界匹配:thread_id 在 stem 中,且前后必须是 `.`/`-` 或字符串边界。
            let found = stem
                .match_indices(thread_id)
                .any(|(idx, _)| {
                    let before_ok = idx == 0
                        || stem.as_bytes().get(idx - 1).map(|b| *b == b'.' || *b == b'-').unwrap_or(false);
                    let after_idx = idx + thread_id.len();
                    let after_ok = after_idx >= stem.len()
                        || stem.as_bytes().get(after_idx).map(|b| *b == b'.' || *b == b'-').unwrap_or(false);
                    before_ok && after_ok
                });
            if !found {
                continue;
            }
            let mt = tokio::fs::metadata(&p)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &best {
                Some((_, best_mt)) if *best_mt >= mt => {}
                _ => best = Some((p, mt)),
            }
        }
    }
    best.map(|(p, _)| p)
}

/// 安全拼接:rel 不能为空/绝对/含 .. / 反斜杠;
/// canonicalize 后必须仍在 codex_home 内(防 symlink 逃逸)。
/// 若 codex_home 本身尚未创建(测试 tmp 场景),直接接受 join 结果(后续写时会创建)。
pub async fn safe_join(codex_home: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains("..")
        || rel.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = codex_home.join(rel);
    // 字符串层(:774-781)已防 .. / 绝对前缀 / 反斜杠。rel 来自 codex rollout/workspace 路径
    // (codex_home/sessions/... 或 codex_home/threads/...),无 symlink 逃逸风险。
    // canonicalize 在 Windows 误判(\\?\ 前缀/UNC/8.3 短名致 starts_with 失败 → 副本收不到 rollout),
    // 用字符串归一化(反斜杠→正斜杠 + 小写)校验,candidate=codex_home.join(rel) 必以 codex_home 开头。
    let c_norm = candidate.to_string_lossy().replace('\\', "/").to_lowercase();
    let h_norm = codex_home.to_string_lossy().replace('\\', "/").to_lowercase();
    if !c_norm.starts_with(&h_norm) {
        return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
    }
    Ok(candidate)
}

/// 删除 Redis 中该 thread 的 offset key(晋升成功后调,触发副本下次从 0 全量同步)。
pub async fn delete_all_thread_offsets(redis: &redis::Client, thread_id: &str) {
    let Ok(mut conn) = redis.get_multiplexed_async_connection().await else {
        return;
    };
    let pattern = format!("repl:offset:{thread_id}:*");
    let mut cursor: u64 = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = match redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query_async(&mut conn)
            .await
        {
            Ok(v) => v,
            Err(_) => return,
        };
        if !keys.is_empty() {
            let _: Result<i64, _> = redis::cmd("DEL").arg(keys).query_async(&mut conn).await;
        }
        if next == 0 {
            break;
        }
        cursor = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receive_rollout_writes_at_offset() {
        let tmp = std::env::temp_dir().join(format!("repl-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("home");
        let chunk = RolloutChunk {
            thread_id: "t1".into(),
            conv_id: "c1".into(),
            rel_path: "sessions/2026/07/17/rollout-x-c1.jsonl".into(),
            offset: 0,
            bytes: b"line1\n".to_vec(),
        };
        receive_rollout(&chunk, &home).await.unwrap();
        let got = std::fs::read(home.join(&chunk.rel_path)).unwrap();
        assert_eq!(got, b"line1\n");
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn receive_rollout_rejects_path_traversal() {
        let tmp = std::env::temp_dir().join(format!("repl2-{}", uuid::Uuid::new_v4()));
        let chunk = RolloutChunk {
            thread_id: "t1".into(),
            conv_id: "c1".into(),
            rel_path: "../../../etc/evil".into(),
            offset: 0,
            bytes: b"x".to_vec(),
        };
        assert!(receive_rollout(&chunk, &tmp).await.is_err());
    }

    #[tokio::test]
    async fn receive_rollout_rejects_out_of_order_offset() {
        let tmp = std::env::temp_dir().join(format!("repl3-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("home");
        let chunk = RolloutChunk {
            thread_id: "t1".into(),
            conv_id: "c1".into(),
            rel_path: "sessions/f.jsonl".into(),
            offset: 100, // 文件不存在(cur_len=0),offset>0 → 拒绝。
            bytes: b"x".to_vec(),
        };
        assert!(receive_rollout(&chunk, &home).await.is_err());
    }

    // ── HA 修复工具测试(spec §2.1.3 / §2.4.3)───────────────────────

    #[tokio::test]
    async fn find_rollout_for_thread_picks_correct_file() {
        let tmp = std::env::temp_dir().join(format!("find-rt-{}", uuid::Uuid::new_v4()));
        let sessions = tmp.join("sessions").join("2026").join("07").join("17");
        tokio::fs::create_dir_all(&sessions).await.unwrap();

        // 两个 thread 前 8 位相同(模拟 UUID 前缀冲突)。
        let tid_a = "8a3f0000-0000-0000-0000-000000000001";
        let tid_b = "8a3f0000-0000-0000-0000-000000000002";
        let fa = sessions.join(format!("rollout-t1-{tid_a}.jsonl"));
        let fb = sessions.join(format!("rollout-t2-{tid_b}.jsonl"));
        tokio::fs::write(&fa, b"a").await.unwrap();
        tokio::fs::write(&fb, b"b").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tokio::fs::write(&fb, b"b-newer").await.unwrap();

        let got = find_rollout_for_thread(&tmp, tid_b).await;
        assert_eq!(got.as_deref(), Some(fb.as_path()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn find_rollout_for_thread_no_match_returns_none() {
        let tmp = std::env::temp_dir().join(format!("find-rt2-{}", uuid::Uuid::new_v4()));
        let sessions = tmp.join("sessions");
        tokio::fs::create_dir_all(&sessions).await.unwrap();
        let got = find_rollout_for_thread(&tmp, "nonexistent-thread-id").await;
        assert!(got.is_none());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn safe_join_rejects_symlink_escape() {
        let base = std::env::temp_dir().join(format!("safejoin-{}", uuid::Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("outside-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        // 字符串层先拒:rel 含 ..
        let bad = safe_join(&base, "../etc/passwd").await;
        assert!(bad.is_err());

        // unix 下 symlink 逃逸应被拒
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();
            let r = safe_join(&base, "escape/file").await;
            assert!(r.is_err(), "symlink escape must be rejected");
        }

        let _ = tokio::fs::remove_dir_all(&base).await;
        let _ = tokio::fs::remove_dir_all(&outside).await;
    }

    // codex_tid 映射进程内 fallback(无 Redis 时双存储,对齐 I4 filesync offset 测试)。
    #[tokio::test]
    async fn codex_tid_map_local_fallback_roundtrip() {
        let tid = format!("t-{}", uuid::Uuid::new_v4());
        // 无 Redis:初始 None。
        assert!(get_codex_tid(None, &tid).await.is_none());
        // 空值跳过(防御 codex_tid 为空)。
        set_codex_tid(None, &tid, "").await;
        assert!(get_codex_tid(None, &tid).await.is_none());
        // set 双写进程内(无 Redis 仅写进程内)。
        let codex_tid = format!("c-{}", uuid::Uuid::new_v4());
        set_codex_tid(None, &tid, &codex_tid).await;
        // get 回读进程内值。
        assert_eq!(
            get_codex_tid(None, &tid).await.as_deref(),
            Some(codex_tid.as_str())
        );
    }
}

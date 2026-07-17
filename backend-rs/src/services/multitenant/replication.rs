//! session 副本:主副本分配 + rollout 增量复制 + 副本晋升。
//!
//! CODEX_HOME 全局,rollout 按 conv_id(= thread_id)分文件 → **复制单元 per-session**。
//! 主每 turn 完成后扫描全局 CODEX_HOME/sessions/ 下该 team 各 thread 的 rollout,按 offset
//! 取增量 POST 到副本;副本 append 到本地 CODEX_HOME 对应文件。主失活 → 副本晋升
//! (起 codex + thread/resume 续接)。
//!
//! 防脑裂:Redis 租约 `codex:primary:{team}` —— 主周期续(SETEX),副本晋升须 SET NX 抢占,
//! 保证同一 team 同一时刻只有一个主。

use crate::db::entities::session_replica::{ActiveModel, Entity, Model};
use crate::error::AppError;
use crate::services::multitenant::cluster::ClusterMembership;
use crate::services::multitenant::now_ms;
use crate::services::multitenant::rpc::WorkerRpcClient;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 主租约有效期(主须在此周期内续约,否则副本可抢占)。需显著大于维护周期(15s)。
pub const LEASE_TTL_MS: i64 = 120_000;
/// Redis 主租约 TTL 秒。
const LEASE_TTL_SECS: u64 = 120;

/// 一段 rollout 增量(主 → 副本)。
#[derive(Serialize, Deserialize, Clone)]
pub struct RolloutChunk {
    pub team_id: String,
    pub conv_id: String,
    /// 相对 CODEX_HOME 的路径,如 `sessions/2026/07/17/rollout-...-<conv>.jsonl`。
    pub rel_path: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

// ── session_replicas 分配 / 查询 ───────────────────────────────────────────

pub async fn get(db: &DatabaseConnection, team_id: &str) -> Result<Option<Model>, AppError> {
    Entity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query session_replica: {e}")))
}

/// 取或分配该 team 的主副本:无则 primary=本节点,replica=另一 alive 节点(反亲和)。
/// insert 主键冲突(并发首请求)→ 重读返回已存在行(原子化)。
pub async fn get_or_assign(
    db: &DatabaseConnection,
    team_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<Model, AppError> {
    if let Some(m) = get(db, team_id).await? {
        return Ok(m);
    }
    let primary = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;
    let replica = alive.into_iter().find(|n| n != &primary);
    let now = now_ms();
    let am = ActiveModel {
        team_id: Set(team_id.to_string()),
        primary_node: Set(primary),
        replica_node: Set(replica),
        status: Set("active".to_string()),
        primary_lease_until: Set(now + LEASE_TTL_MS),
        updated_at: Set(now),
    };
    match am.insert(db).await {
        Ok(m) => Ok(m),
        // 主键冲突 = 并发首请求,重读。
        Err(_) => get(db, team_id).await?.ok_or_else(|| AppError::internal("session_replica vanished".into())),
    }
}

/// 确保副本已分配:若 replica_node 为 None 且存在其他 alive 节点 → 回填(反亲和)。
/// 用于扩容后补选副本(否则主挂无人晋升)。
pub async fn ensure_replica(
    db: &DatabaseConnection,
    team_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<(), AppError> {
    let Some(row) = get(db, team_id).await? else {
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

/// 主续约(DB lease + Redis 租约覆盖续期)。仅当本节点仍是 primary 时生效。
pub async fn renew_lease(
    db: &DatabaseConnection,
    team_id: &str,
    node_id: &str,
    redis: Option<&redis::Client>,
) -> Result<(), AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(());
    };
    if row.primary_node != node_id {
        return Ok(()); // 不是主,不续。
    }
    let now = now_ms();
    let mut am: ActiveModel = row.into();
    am.primary_lease_until = Set(now + LEASE_TTL_MS);
    am.updated_at = Set(now);
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("renew lease: {e}")))?;
    // Redis 租约覆盖续期(主独占,EX 覆盖)。
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("codex:primary:{team_id}"))
                .arg(node_id)
                .arg("EX")
                .arg(LEASE_TTL_SECS)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    Ok(())
}

/// Redis 主租约抢占(SET NX EX)。返回 true=抢占成功(可晋升);false=主租约仍在(不晋升)。
pub async fn try_acquire_primary(
    redis: Option<&redis::Client>,
    team_id: &str,
    node_id: &str,
) -> bool {
    let Some(c) = redis else {
        return true; // 无 Redis(单节点)→ 直接放行(无脑裂风险)。
    };
    let Ok(mut conn) = c.get_multiplexed_async_connection().await else {
        return false;
    };
    let ok: Option<String> = redis::cmd("SET")
        .arg(format!("codex:primary:{team_id}"))
        .arg(node_id)
        .arg("NX")
        .arg("EX")
        .arg(LEASE_TTL_SECS)
        .query_async(&mut conn)
        .await
        .ok();
    ok.is_some()
}

/// 更新主副本(晋升 / 重选副本时)。
pub async fn set_primary(
    db: &DatabaseConnection,
    team_id: &str,
    new_primary: &str,
    new_replica: Option<&str>,
) -> Result<(), AppError> {
    let row = get(db, team_id)
        .await?
        .ok_or_else(|| AppError::internal("session_replica row missing".into()))?;
    let now = now_ms();
    let mut am: ActiveModel = row.into();
    am.primary_node = Set(new_primary.to_string());
    am.replica_node = Set(new_replica.map(String::from));
    am.status = Set("active".to_string());
    am.primary_lease_until = Set(now + LEASE_TTL_MS);
    am.updated_at = Set(now);
    am.update(db)
        .await
        .map_err(|e| AppError::internal(format!("set primary: {e}")))?;
    Ok(())
}

// ── rollout 增量复制(主侧)─────────────────────────────────────────────────

/// 主侧:复制该 team 所有 thread 的 rollout 增量到副本节点(spec §2.1.4)。
/// 复制单元 = active_rollout 里的 thread(thread_id → 路径);
/// offset 仅在 send 成功后才推进(spec §2.2)。
pub async fn replicate_team_rollouts(
    db: &DatabaseConnection,
    team_id: &str,
    codex_home: &Path,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    rpc_client: &WorkerRpcClient,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<(), AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(());
    };
    let Some(replica_node) = row.replica_node.clone() else {
        return Ok(());
    };
    if replica_node == cluster.local_node_id() {
        return Ok(());
    }
    let Some(rpc_addr) = cluster.node_rpc_addr(&replica_node).await else {
        return Ok(());
    };

    // 复制单元:遍历 active_rollout(thread_id → 文件路径),不再 walk sessions/。
    // 重启后 active_rollout 为空 → 本轮跳过(下次 mt_create_thread / mt_start_turn 会写入)。
    let entries: Vec<(String, PathBuf)> = {
        let m = active_rollout.lock().await;
        m.iter()
            .filter_map(|(tid, p)| p.exists().then(|| (tid.clone(), p.clone())))
            .collect()
    };

    for (conv, abs_path) in entries {
        let size = match tokio::fs::metadata(&abs_path).await {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let offset = get_offset_dual(redis, local_offsets, team_id, &conv).await;
        if size <= offset {
            continue;
        }
        let bytes = match read_range(&abs_path, offset, size).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(team_id, conv = %conv, error = %e, "read rollout range failed, skip this round");
                continue;
            }
        };
        let rel_path = match abs_path.strip_prefix(codex_home) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let chunk = RolloutChunk {
            team_id: team_id.to_string(),
            conv_id: conv.clone(),
            rel_path,
            offset,
            bytes,
        };
        if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
            tracing::warn!(team_id, conv = %conv, error = %e, "replicate rollout chunk failed (will retry next round)");
            // 不推进 offset → 下次重传同一段(spec §2.2)。
            continue;
        }
        set_offset_dual(redis, local_offsets, team_id, &conv, size).await;
        metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
    }
    Ok(())
}

/// offset 双存储读(Redis 优先,失败回退进程内)。spec §2.2 fallback。
async fn get_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
    conv: &str,
) -> u64 {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("repl:offset:{team_id}:{conv}"))
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                return s.parse().unwrap_or(0);
            }
        }
    }
    let m = local.lock().await;
    m.get(&(team_id.to_string(), conv.to_string())).copied().unwrap_or(0)
}

/// offset 双存储写(同步写 Redis + 进程内)。
async fn set_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
    conv: &str,
    v: u64,
) {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("repl:offset:{team_id}:{conv}"))
                .arg(v)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    let mut m = local.lock().await;
    m.insert((team_id.to_string(), conv.to_string()), v);
}

/// 晋升后清空 offset(Redis SCAN + DEL + 进程内 retain)。
pub async fn delete_all_team_offsets_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
) {
    if let Some(c) = redis {
        delete_all_team_offsets(c, team_id).await;
    }
    let mut m = local.lock().await;
    m.retain(|(t, _), _| t != team_id);
}

// ── 副本 receive ───────────────────────────────────────────────────────────

/// 副本:把收到的 rollout 增量写入本地 CODEX_HOME(路径穿越校验 + offset 校验防乱序空洞)。
pub async fn receive_rollout(chunk: &RolloutChunk, codex_home: &Path) -> Result<(), AppError> {
    // 路径穿越校验:rel_path 必须是相对路径且不含 `..` / 绝对前缀 / 反斜杠。
    if chunk.rel_path.is_empty()
        || chunk.rel_path.starts_with('/')
        || chunk.rel_path.starts_with('\\')
        || chunk.rel_path.contains("..")
        || chunk.rel_path.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {}", chunk.rel_path)));
    }
    let path = codex_home.join(&chunk.rel_path);
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

/// 副本自查:若自己是某 team 的副本、主失活(不在 alive 或租约过期)→ Redis 抢占租约后晋升。
/// 返回 true 表示已晋升(调用方应起 codex + thread/resume 续接)。
pub async fn promote_if_primary_down(
    db: &DatabaseConnection,
    team_id: &str,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<bool, AppError> {
    let Some(row) = get(db, team_id).await? else {
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
    if !try_acquire_primary(redis, team_id, me).await {
        tracing::info!(team_id, "primary lease still held by another, skip promote");
        return Ok(false);
    }
    // 抢占成功 → 晋升:选新副本(反亲和,alive 中 != 自己)。
    let alive = cluster.alive_nodes().await;
    let new_replica = alive.into_iter().find(|n| n != me);
    set_primary(db, team_id, me, new_replica.as_deref()).await?;
    // 晋升成功 → 删 Redis + 进程内 offset,触发下次从 0 全量同步(spec §2.3.3)。
    delete_all_team_offsets_dual(redis, local_offsets, team_id).await;
    let _ = active_rollout; // 占位:下次 mt_start_turn / mt_create_thread 重新发现文件。
    metrics::counter!("replica_promotions_total").increment(1);
    tracing::info!(team_id, "replica promoted to primary");
    Ok(true)
}

/// 孤儿 team 认领:主节点不 alive(如重启换 id)且无人晋升时,由"最低 alive id"节点认领主。
/// 确定性认领(只有最低 id 节点执行)→ 无竞争。
pub async fn reclaim_orphan_teams(
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
        if !try_acquire_primary(redis, &row.team_id, &me).await {
            continue;
        }
        let new_replica = alive.iter().find(|n| n.as_str() != me).cloned();
        set_primary(db, &row.team_id, &me, new_replica.as_deref()).await?;
        tracing::info!(team_id = %row.team_id, "reclaimed orphan team as primary");
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
    let canon_home = tokio::fs::canonicalize(codex_home)
        .await
        .map_err(|e| AppError::internal(format!("canonicalize codex_home: {e}")))?;
    let canon_path = match tokio::fs::canonicalize(&candidate).await {
        Ok(p) => p,
        Err(_) => {
            if let Some(parent) = candidate.parent() {
                let canon_parent = tokio::fs::canonicalize(parent)
                    .await
                    .map_err(|e| AppError::internal(format!("canonicalize parent: {e}")))?;
                if !canon_parent.starts_with(&canon_home) {
                    return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
                }
            }
            candidate
        }
    };
    if !canon_path.starts_with(&canon_home) {
        return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
    }
    Ok(canon_path)
}

/// 删除 Redis 中该 team 全部 thread 的 offset key(晋升成功后调,触发副本下次从 0 全量同步)。
pub async fn delete_all_team_offsets(redis: &redis::Client, team_id: &str) {
    let Ok(mut conn) = redis.get_multiplexed_async_connection().await else {
        return;
    };
    let pattern = format!("repl:offset:{team_id}:*");
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
            team_id: "t1".into(),
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
            team_id: "t1".into(),
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
            team_id: "t1".into(),
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
}

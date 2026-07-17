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
use crate::db::entities::thread::{Column as ThreadColumn, Entity as ThreadEntity};
use crate::error::AppError;
use crate::services::multitenant::cluster::ClusterMembership;
use crate::services::multitenant::now_ms;
use crate::services::multitenant::rpc::WorkerRpcClient;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// 主侧:复制该 team 所有 thread 的 rollout 增量到副本节点。
pub async fn replicate_team_rollouts(
    db: &DatabaseConnection,
    team_id: &str,
    codex_home: &Path,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    rpc_client: &WorkerRpcClient,
) -> Result<(), AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(());
    };
    let Some(replica_node) = row.replica_node.clone() else {
        return Ok(()); // 无副本,跳过。
    };
    if replica_node == cluster.local_node_id() {
        return Ok(()); // 副本是自己(单节点),跳过。
    }
    let Some(rpc_addr) = cluster.node_rpc_addr(&replica_node).await else {
        return Ok(()); // 副本 RPC 地址未知,跳过。
    };

    // 该 team 的所有 thread_id(= conv_id)。
    let thread_ids: std::collections::HashSet<String> = ThreadEntity::find()
        .filter(ThreadColumn::TeamId.eq(team_id.to_string()))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("query team threads for replication: {e}")))?
        .into_iter()
        .map(|t| t.id)
        .collect();
    if thread_ids.is_empty() {
        return Ok(());
    }

    // 扫描全局 CODEX_HOME/sessions/ 下所有 rollout 文件。
    let sessions_dir = codex_home.join("sessions");
    let files = list_rollout_files(&sessions_dir).await;
    for (abs_path, rel_path) in files {
        // 匹配:文件名包含该 team 的某个 thread_id(完整 UUID,避免文件名解析截断)。
        let fname = abs_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(conv) = thread_ids.iter().find(|tid| fname.contains(tid.as_str())).cloned() else {
            continue;
        };
        let size = match tokio::fs::metadata(&abs_path).await {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let offset = get_offset(redis, team_id, &conv).await;
        if size <= offset {
            continue; // 无增量(含 codex rollback 导致文件变短)。
        }
        // 单文件 IO 失败不中断整轮复制(continue + warn)。
        let bytes = match read_range(&abs_path, offset, size).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(team_id, conv = %conv, error = %e, "read rollout range failed, skip this round");
                continue;
            }
        };
        let chunk = RolloutChunk {
            team_id: team_id.to_string(),
            conv_id: conv.clone(),
            rel_path,
            offset,
            bytes,
        };
        if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
            tracing::warn!(team_id, conv = %conv, error = %e, "replicate rollout chunk failed (non-fatal)");
            continue;
        }
        set_offset(redis, team_id, &conv, size).await;
        metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
    }
    Ok(())
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
) -> Result<bool, AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(false);
    };
    let me = cluster.local_node_id();
    if row.replica_node.as_deref() != Some(me) {
        return Ok(false); // 不是该 team 副本。
    }
    let primary_alive = cluster.alive_nodes().await.iter().any(|n| n == &row.primary_node);
    let lease_valid = row.primary_lease_until > now_ms();
    if primary_alive && lease_valid {
        return Ok(false); // 主健康。
    }
    // Redis 抢占租约(SET NX):防止多个副本同时晋升(脑裂)。
    if !try_acquire_primary(redis, team_id, me).await {
        tracing::info!(team_id, "primary lease still held by another, skip promote");
        return Ok(false);
    }
    // 抢占成功 → 晋升:选新副本(反亲和,alive 中 != 自己)。
    let alive = cluster.alive_nodes().await;
    let new_replica = alive.into_iter().find(|n| n != me);
    set_primary(db, team_id, me, new_replica.as_deref()).await?;
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

// ── offset 跟踪(Redis)─────────────────────────────────────────────────────

async fn get_offset(redis: Option<&redis::Client>, team_id: &str, conv: &str) -> u64 {
    let Some(c) = redis else {
        return 0;
    };
    let mut conn = match c.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let v: Option<String> = redis::cmd("GET")
        .arg(format!("repl:offset:{team_id}:{conv}"))
        .query_async(&mut conn)
        .await
        .unwrap_or(None);
    v.and_then(|s| s.parse().ok()).unwrap_or(0)
}

async fn set_offset(redis: Option<&redis::Client>, team_id: &str, conv: &str, v: u64) {
    let Some(c) = redis else {
        return;
    };
    let mut conn = match c.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(_) => return,
    };
    let _: () = redis::cmd("SET")
        .arg(format!("repl:offset:{team_id}:{conv}"))
        .arg(v)
        .query_async(&mut conn)
        .await
        .unwrap_or(());
}

// ── 文件辅助 ─────────────────────────────────────────────────────────────

/// 递归列出 sessions 目录下所有 `.jsonl`(返回 (绝对路径, 相对 codex_home 的路径))。
async fn list_rollout_files(sessions_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let base = sessions_dir.parent().unwrap_or(sessions_dir);
    walk_jsonl(base, sessions_dir, &mut out).await;
    out
}

async fn walk_jsonl(base: &Path, cur: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(mut entries) = tokio::fs::read_dir(cur).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if ft.is_dir() {
            Box::pin(walk_jsonl(base, &path, out)).await;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push((path.clone(), rel.to_string_lossy().replace('\\', "/")));
            }
        }
    }
}

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
}

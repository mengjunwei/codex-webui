//! per-thread workspace 文件增量同步(主 → 副本)。
//!
//! 复制单元 = 单个 thread 的 workspace 目录(threads/{thread_id}/)。
//! 主侧维护循环扫描该目录下文件 mtime > last_sync 的,读全文经 RPC 推到副本;
//! 副本 safe_join 后覆盖写。offset = 已同步的最大 mtime(ms),存 Redis + 进程内。

use crate::error::AppError;
use crate::services::multitenant::replication::get as get_replica;
use crate::services::workspace::thread_workspace_path;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 进程内 filesync offset fallback(无 Redis / Redis miss 时用)。
/// key = thread_id,value = 已同步最大 mtime(ms)。对齐 rollout local_offsets 的双存储语义:
/// 重启归零(接受全量重扫);避免无 Redis 时 offset 恒 0 → 每 15s 全量重扫重传(I4)。
static LOCAL_FILESYNC_OFFSETS: once_cell::sync::Lazy<
    tokio::sync::Mutex<std::collections::HashMap<String, i64>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));

/// 文件变更类型。
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType {
    Create,
    Modify,
    Delete,
}

/// 一条文件变更(主 → 副本)。
#[derive(Serialize, Deserialize, Clone)]
pub struct FileChange {
    pub thread_id: String,
    pub relative_path: String, // 相对 threads/{thread_id}/,正斜杠分隔。
    pub change_type: ChangeType,
    pub content: Option<Vec<u8>>,
}

/// 扫描 dir 下所有文件 mtime(ms) > since_ms 的,返回 FileChange(Create/Modify)。
/// 不追踪 Delete(简化:v1 只同步新增/修改;删除靠 failover 后目录重建容忍)。
pub async fn scan_changes(dir: &Path, since_ms: i64) -> Result<Vec<FileChange>, AppError> {
    use std::time::UNIX_EPOCH;
    if !tokio::fs::metadata(dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&d).await {
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
            let mt = match tokio::fs::metadata(&p).await {
                Ok(m) => m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()),
                Err(_) => continue,
            };
            let Some(mt) = mt else { continue };
            let mt_ms = mt.as_millis() as i64;
            if mt_ms <= since_ms {
                continue;
            }
            let rel = match p.strip_prefix(dir) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let content = tokio::fs::read(&p).await.ok();
            let change_type = ChangeType::Modify; // 简化:统一 Modify(覆盖写)。
            out.push(FileChange {
                thread_id: String::new(), // 调用方(scan_and_replicate)回填。
                relative_path: rel,
                change_type,
                content,
            });
        }
    }
    Ok(out)
}

/// 读取该 thread 的文件同步 offset(已同步的最大 mtime,ms)。
/// I4:对齐 rollout local_offsets 双存储 —— Redis 优先,miss/无 Redis 回退进程内 map,
///     否则无 Redis 时 offset 恒 0 → 每 15s 全量重扫重传。
async fn get_filesync_offset(redis: Option<&redis::Client>, thread_id: &str) -> i64 {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("filesync:offset:{thread_id}"))
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                return s.parse().unwrap_or(0);
            }
        }
    }
    // Redis 未配置 / 连接失败 / miss → 进程内 fallback。
    LOCAL_FILESYNC_OFFSETS
        .lock()
        .await
        .get(thread_id)
        .copied()
        .unwrap_or(0)
}

/// 推进 offset(发送成功后才调用)。I4:双写 Redis + 进程内(对齐 rollout)。
/// Redis 失败静默忽略(进程内仍写入,下次重发幂等)。
async fn set_filesync_offset(redis: Option<&redis::Client>, thread_id: &str, v: i64) {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("filesync:offset:{thread_id}"))
                .arg(v)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    LOCAL_FILESYNC_OFFSETS
        .lock()
        .await
        .insert(thread_id.to_string(), v);
}

/// 主侧:扫描该 thread workspace 增量,经 RPC 推到副本。仅在 primary 节点调用。
///
/// 流程:读 offset → scan_changes → 查副本节点 → 解析 RPC 地址 →
/// 回填 thread_id → 算本轮最大 mtime → replicate_files 成功后推进 offset + metrics。
/// 任一中间步骤缺数据(无副本/副本即本节点/无 RPC 地址)静默返回,等下轮。
pub async fn scan_and_replicate(state: &AppState, thread_id: &str) -> Result<(), AppError> {
    let ws = thread_workspace_path(&state.workspace_root, thread_id);
    let last = get_filesync_offset(state.mt_redis.as_ref(), thread_id).await;
    let mut changes = scan_changes(&ws, last).await?;
    if changes.is_empty() {
        return Ok(());
    }

    let row = get_replica(&state.db, thread_id).await?;
    let replica_node = row.and_then(|r| r.replica_node);
    let Some(replica) = replica_node else {
        return Ok(());
    };
    if replica == state.node_id {
        return Ok(());
    }
    let Some(rpc_addr) = state.cluster.node_rpc_addr(&replica).await else {
        return Ok(());
    };

    for c in changes.iter_mut() {
        c.thread_id = thread_id.to_string();
    }

    // 计算本轮最大 mtime 作为新 offset(发送成功后才推进)。
    use std::time::UNIX_EPOCH;
    let mut max_mt = last;
    for c in &changes {
        let p = ws.join(&c.relative_path);
        if let Ok(m) = tokio::fs::metadata(&p).await {
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                    max_mt = max_mt.max(d.as_millis() as i64);
                }
            }
        }
    }

    if state
        .worker_rpc
        .replicate_files(&rpc_addr, &changes)
        .await
        .is_ok()
    {
        set_filesync_offset(state.mt_redis.as_ref(), thread_id, max_mt).await;
        metrics::counter!("filesync_bytes_total").increment(
            changes
                .iter()
                .map(|c| c.content.as_ref().map(|b| b.len()).unwrap_or(0) as u64)
                .sum(),
        );
    } else {
        tracing::warn!(thread_id, "replicate_files failed (will retry next round)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scan_changes_picks_files_newer_than_cutoff() {
        let tmp = std::env::temp_dir().join(format!("fs-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();
        // 旧文件(mtime < since)应被忽略。
        let old = tmp.join("old.txt");
        tokio::fs::write(&old, b"old").await.unwrap();
        // 截断 mtime 到 1 小时前(跨平台用 filetime 设置;测试简化:直接读 mtime 作 since)。
        let old_mt = old_mt_seconds(&old).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // 新文件。
        tokio::fs::write(tmp.join("new.txt"), b"new").await.unwrap();

        let changes = scan_changes(&tmp, old_mt + 1).await.unwrap();
        let names: Vec<_> = changes.iter().map(|c| c.relative_path.clone()).collect();
        assert!(names.contains(&"new.txt".to_string()));
        assert!(!names.contains(&"old.txt".to_string()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    async fn old_mt_seconds(p: &std::path::Path) -> i64 {
        use std::time::UNIX_EPOCH;
        let m = tokio::fs::metadata(p).await.unwrap();
        m.modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    // I4:filesync offset 进程内 fallback(无 Redis 时双存储)。
    #[tokio::test]
    async fn filesync_offset_local_fallback_roundtrip() {
        let tid = format!("t-{}", uuid::Uuid::new_v4());
        // 无 Redis:初始为 0。
        assert_eq!(get_filesync_offset(None, &tid).await, 0);
        // set 双写进程内(无 Redis 仅写进程内)。
        set_filesync_offset(None, &tid, 12345).await;
        // get 回读进程内值。
        assert_eq!(get_filesync_offset(None, &tid).await, 12345);
    }
}

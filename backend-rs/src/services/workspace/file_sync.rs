//! per-thread workspace 文件增量同步(主 → 副本)。
//!
//! 复制单元 = 单个 thread 的 workspace 目录(threads/{thread_id}/)。
//! 主侧维护循环扫描该目录下文件 mtime > last_sync 的,读全文经 RPC 推到副本;
//! 副本 safe_join 后覆盖写。offset = 已同步的最大 mtime(ms),存 Redis + 进程内。

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::path::Path;

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
}

//! 扩展指纹计算：扫描目录算每文件 SHA256 + 整体聚合 hash。
//!
//! 供后续上传登记指纹（Task 6）、同步时比对（Task 8）使用。

use crate::error::AppError;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

/// 单文件的指纹：相对路径、大小、SHA256、是否二进制。
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FileFingerprint {
    pub rel_path: String,
    pub size: i64,
    pub sha256: String,
    pub is_binary: bool,
}

/// 判定二进制：含 NUL 字节视为二进制。
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0u8)
}

/// 计算单个文件的指纹。rel_path 统一用正斜杠（跨平台稳定）。
async fn hash_one(root: &Path, entry: &walkdir::DirEntry) -> Result<FileFingerprint, AppError> {
    let rel = entry
        .path()
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = tokio::fs::read(entry.path())
        .await
        .map_err(|e| AppError::internal(format!("read {}: {e}", entry.path().display())))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(FileFingerprint {
        rel_path: rel,
        size: bytes.len() as i64,
        sha256: hex::encode(h.finalize()),
        is_binary: looks_binary(&bytes),
    })
}

/// 递归扫描 root 下所有文件（跳过目录、跳过 `.cluster-extensions.json` 本地状态文件），
/// 返回每文件的指纹。
pub async fn scan_dir(root: &Path) -> Result<Vec<FileFingerprint>, AppError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == ".cluster-extensions.json" {
            continue;
        }
        out.push(hash_one(root, &entry).await?);
    }
    Ok(out)
}

/// 按 rel_path 排序后对所有 (rel_path, sha256) 再做一次 SHA256，
/// 得到稳定的、与遍历顺序无关的整体 content_hash。
pub fn aggregate_hash(files: &[FileFingerprint]) -> String {
    let mut v = files.to_vec();
    v.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let mut h = Sha256::new();
    for f in &v {
        h.update(f.rel_path.as_bytes());
        h.update(f.sha256.as_bytes());
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn scan_dir_returns_fingerprint_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("SKILL.md"), "hello").unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/run.sh"), "#!/bin/sh").unwrap();
        let mut fps = scan_dir(root).await.unwrap();
        fps.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        assert_eq!(fps.len(), 2);
        assert_eq!(fps[0].rel_path, "SKILL.md");
        assert_eq!(fps[0].size, 5);
        assert_eq!(fps[1].rel_path, "scripts/run.sh");
    }

    #[test]
    fn aggregate_hash_is_deterministic_and_order_independent() {
        let fps = vec![
            FileFingerprint { rel_path: "b.md".into(), size: 1, sha256: "B".into(), is_binary: false },
            FileFingerprint { rel_path: "a.md".into(), size: 1, sha256: "A".into(), is_binary: false },
        ];
        let h1 = aggregate_hash(&fps);
        let mut rev = fps.clone(); rev.reverse();
        let h2 = aggregate_hash(&rev);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }
}

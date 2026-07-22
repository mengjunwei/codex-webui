use crate::error::AppError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 本地状态文件名：id → 本地扩展条目(name+hash) 映射，用于集群扩展同步对齐。
const STATE_FILE: &str = ".cluster-extensions.json";

/// 本地扩展条目：同时记录 name 与 content_hash。
///
/// 设计：name 用于删除分支定位 `skills/{name}/` 目录 —— **不依赖 PG 查询**，
/// 避免发起节点物理删 PG 行后、副本收到事件时 PG 已无该行 → `name_of(id)` 返回 None
/// → 目录成孤儿。hash 用于同步循环判断是否需要更新(与 PG content_hash 比对)。
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct LocalExtEntry {
    /// 扩展名(skill 目录名)，删除时定位目录用。
    pub name: String,
    /// 内容指纹，同步对齐用。
    pub hash: String,
}

/// skills 目录：`<codex_home>/skills`，存放每个扩展落盘后的文件树。
pub fn skills_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("skills")
}

/// 安全拼路径：拒绝空 / 绝对 / 含 `..` / 含反斜杠的相对路径；
/// 归一化（反斜杠→正斜杠、小写）后校验 candidate 必以 root 开头，防穿越。
async fn safe_join_local(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains("..")
        || rel.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = root.join(rel);
    let c = candidate
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let r = root.to_string_lossy().replace('\\', "/").to_lowercase();
    if !c.starts_with(&r) {
        return Err(AppError::internal(format!("path escapes root: {rel}")));
    }
    Ok(candidate)
}

/// 写文件（自动建父目录）。
pub async fn write_file_safe(root: &Path, rel: &str, content: &[u8]) -> Result<(), AppError> {
    let path = safe_join_local(root, rel).await?;
    if let Some(p) = path.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    }
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| AppError::internal(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// 删除 root/{name} 整个目录（skill 卸载）。目录不存在视为成功。
pub async fn remove_dir_safe(root: &Path, name: &str) -> Result<(), AppError> {
    let dir = safe_join_local(root, name).await?;
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| AppError::internal(format!("remove {}: {e}", dir.display())))?;
    }
    Ok(())
}

/// 读取本地状态文件；不存在或解析失败(含旧格式 id→hash)时返回空 map(容错)。
pub async fn load_local_state(codex_home: &Path) -> HashMap<String, LocalExtEntry> {
    let p = codex_home.join(STATE_FILE);
    match tokio::fs::read(&p).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// 写入本地状态文件(覆盖)。
pub async fn save_local_state(
    codex_home: &Path,
    map: &HashMap<String, LocalExtEntry>,
) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(map).map_err(|e| AppError::internal(format!("json: {e}")))?;
    tokio::fs::write(codex_home.join(STATE_FILE), &bytes)
        .await
        .map_err(|e| AppError::internal(format!("write state: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_file_safe_creates_nested_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file_safe(root, "my-skill/scripts/run.sh", b"hi").await.unwrap();
        let got = tokio::fs::read(root.join("my-skill/scripts/run.sh")).await.unwrap();
        assert_eq!(got, b"hi");
    }

    #[tokio::test]
    async fn write_file_safe_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let r = write_file_safe(tmp.path(), "../escape.sh", b"x").await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn local_state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = HashMap::new();
        m.insert(
            "ext_1".into(),
            LocalExtEntry {
                name: "skill-1".into(),
                hash: "deadbeef".into(),
            },
        );
        save_local_state(tmp.path(), &m).await.unwrap();
        let loaded = load_local_state(tmp.path()).await;
        let e = loaded.get("ext_1").expect("ext_1 应存在");
        assert_eq!(e.name, "skill-1");
        assert_eq!(e.hash, "deadbeef");
    }
}

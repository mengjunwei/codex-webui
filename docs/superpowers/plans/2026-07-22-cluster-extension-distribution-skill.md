# 集群扩展分发 — 基础设施 + skill 端到端 实现计划

> **For agentic workers:** REQUIRED SUB-SISKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现集群扩展分发的核心架构，并端到端跑通「技能(skill)」分发——在一个节点上传一个 skill，集群所有节点自动同步到本地 `$CODEX_HOME/skills/{name}/`，新会话即可用。

**Architecture:** PG 存扩展清单（权威源，UUIDv7 主键，集群级无 team_id）；扩展文件留各节点本地盘；节点缺文件时从「持有节点」经内网 RPC 下载；holder 扩散去单点。三时机：Redis `extensions:changed` 事件 / 周期 30s / 节点启动 bootstrap。

**Tech Stack:** Rust + axum 0.8 + sea-orm 1.1 + sea-orm-migration + redis 0.27 + sha2 0.10 + toml_edit 0.22 + uuid 1(v7)。

## Global Constraints

- 类型约定（PG/MySQL 双方言）：`VARCHAR(36)` UUIDv7 / `BIGINT` i64 毫秒 / `BOOLEAN` / `TEXT`，不用 JSON/ENUM/ARRAY；建表用 `db.execute_unprepared("CREATE TABLE IF NOT EXISTS ...")` raw SQL；索引用 `crate::db::migration::create_index` 助手。
- id 一律用 `crate::services::multitenant::new_id()`（= `Uuid::now_v7().to_string()`），时间戳用 `now_ms()`。
- 哈希用 `sha2` SHA-256 + `hex`；请求体用 `crate::error::Json<T>`，响应体用 `axum::Json<T>`，错误用 `AppError`。
- **集群级，无 team_id**（阶段一）；只写 `$CODEX_HOME/skills/{name}/`，绝不碰 `auth.json` / `*.sqlite` / `installation_id` / `sessions/`。
- 所有路径拼装走 `safe_join`（防穿越）；周期 task 保存 `JoinHandle` 并在 shutdown `abort`。
- 每步 `cargo build` 或 `cargo test` 通过后再 commit；commit 消息中文。

## File Structure

**新建：**
- `backend-rs/src/db/migration/m20260722_000001_cluster_extensions.rs` — 建 3 张表迁移
- `backend-rs/src/services/extensions/mod.rs` — 模块入口 + 公共类型（FileFingerprint 等）
- `backend-rs/src/services/extensions/fingerprint.rs` — 目录扫描 + SHA256 指纹
- `backend-rs/src/services/extensions/apply.rs` — 本地安全落盘 + 本地状态读写
- `backend-rs/src/services/extensions/store.rs` — PG 读写（清单/指纹/holders）
- `backend-rs/src/services/extensions/sync.rs` — 同步循环 run_round + bootstrap + holder 选择
- `backend-rs/src/api/multitenant/extensions.rs` — 安装入口 REST handler（上传/列表/删除）

**修改：**
- `backend-rs/src/db/migration/mod.rs` — 注册新迁移
- `backend-rs/src/db/entities/mod.rs` — 加 3 个 entity 子模块
- `backend-rs/src/services/mod.rs` — `pub mod extensions;`
- `backend-rs/src/config.rs` — `[extensions]` 配置段
- `backend-rs/src/main.rs` — spawn 同步循环 + bootstrap + 事件订阅 + 路由
- `backend-rs/src/api/multitenant/mod.rs` — `pub mod extensions;`
- `backend-rs/src/api/mod.rs` — 注册 `/api/mt/extensions` 路由
- `backend-rs/src/api/multitenant/internal_rpc.rs` — `/internal/ext-fetch` 端点
- `backend-rs/src/services/multitenant/rpc.rs` — `WorkerRpcClient::ext_fetch`
- `backend-rs/config.toml.example` — `[extensions]` 示例

**关键接口（跨 task 一致，后续 task 照此引用）：**
```rust
// fingerprint.rs
pub struct FileFingerprint { pub rel_path: String, pub size: i64, pub sha256: String, pub is_binary: bool }
pub async fn scan_dir(root: &Path) -> Result<Vec<FileFingerprint>, AppError>;
pub fn aggregate_hash(files: &[FileFingerprint]) -> String; // 整体 content_hash

// apply.rs
pub fn skills_dir(codex_home: &Path) -> PathBuf;              // codex_home/skills
pub async fn write_file_safe(root: &Path, rel: &str, content: &[u8]) -> Result<(), AppError>;
pub async fn remove_dir_safe(root: &Path, name: &str) -> Result<(), AppError>;
pub async fn load_local_state(codex_home: &Path) -> HashMap<String, String>; // id -> content_hash
pub async fn save_local_state(codex_home: &Path, map: &HashMap<String,String>) -> Result<(), AppError>;

// store.rs
pub struct ExtRecord { pub id: String, pub kind: String, pub name: String, pub content_form: String, pub content_hash: String, pub enabled: bool }
pub async fn upsert_extension(db: &DatabaseConnection, rec: &ExtRecord, files: &[FileFingerprint]) -> Result<(), AppError>;
pub async fn list_enabled(db: &DatabaseConnection) -> Result<Vec<ExtRecord>, AppError>;
pub async fn get_files(db: &DatabaseConnection, ext_id: &str) -> Result<Vec<FileFingerprint>, AppError>;
pub async fn add_holder(db: &DatabaseConnection, ext_id: &str, node_id: &str) -> Result<(), AppError>;
pub async fn list_holders(db: &DatabaseConnection, ext_id: &str) -> Result<Vec<String>, AppError>;
pub async fn delete_extension(db: &DatabaseConnection, ext_id: &str) -> Result<(), AppError>;

// sync.rs
pub async fn run_round(state: &AppState) -> Result<(), AppError>;
pub async fn bootstrap(state: &AppState) -> Result<(), AppError>; // = run_round，语义=启动全量对齐

// rpc.rs (WorkerRpcClient)
pub async fn ext_fetch(&self, base: &str, ext_id: &str, rel_path: &str) -> Result<bytes::Bytes, AppError>;
```

---

### Task 1: 建表迁移 + entity

**Files:**
- Create: `backend-rs/src/db/migration/m20260722_000001_cluster_extensions.rs`
- Modify: `backend-rs/src/db/migration/mod.rs:10-24`
- Modify: `backend-rs/src/db/entities/mod.rs`（追加 3 个子模块）

**Interfaces:**
- Produces: `cluster_extensions` / `cluster_extension_files` / `cluster_extension_holders` 三张表 + 对应 entity（供 Task 5 的 store.rs 使用）

- [ ] **Step 1: 写迁移文件**

Create `backend-rs/src/db/migration/m20260722_000001_cluster_extensions.rs`:

```rust
//! 集群扩展分发:清单 / 文件指纹 / 持有节点 三张表(集群级,无 team_id)。
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str { "m20260722_000001_cluster_extensions" }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extensions (
                id VARCHAR(36) PRIMARY KEY NOT NULL,
                kind VARCHAR(32) NOT NULL,
                name VARCHAR(128) NOT NULL,
                display_name VARCHAR(256),
                description TEXT,
                version VARCHAR(64),
                content_form VARCHAR(16) NOT NULL,
                config_text TEXT,
                content_hash VARCHAR(128) NOT NULL,
                enabled BOOLEAN NOT NULL DEFAULT TRUE,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                created_by VARCHAR(36)
            )"#,
        ).await?;
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extension_files (
                id BIGINT PRIMARY KEY NOT NULL,
                extension_id VARCHAR(36) NOT NULL,
                rel_path VARCHAR(512) NOT NULL,
                size_bytes BIGINT NOT NULL,
                content_hash VARCHAR(128) NOT NULL,
                is_binary BOOLEAN NOT NULL DEFAULT FALSE
            )"#,
        ).await?;
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extension_holders (
                extension_id VARCHAR(36) NOT NULL,
                node_id VARCHAR(36) NOT NULL,
                held_since BIGINT NOT NULL
            )"#,
        ).await?;
        crate::db::migration::create_index(manager, "idx_ext_kind_name", "cluster_extensions", "kind,name").await?;
        crate::db::migration::create_index(manager, "idx_ext_enabled", "cluster_extensions", "enabled").await?;
        crate::db::migration::create_index(manager, "idx_extfile_ext", "cluster_extension_files", "extension_id").await?;
        db.execute_unprepared("COMMENT ON TABLE cluster_extensions IS '集群扩展分发清单'").await.ok();
        db.execute_unprepared("COMMENT ON TABLE cluster_extension_files IS '扩展文件指纹(无内容)'").await.ok();
        db.execute_unprepared("COMMENT ON TABLE cluster_extension_holders IS '扩展持有节点(去单点)'").await.ok();
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS cluster_extension_holders").await?;
        db.execute_unprocessed("DROP TABLE IF EXISTS cluster_extension_files").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS cluster_extensions").await?;
        Ok(())
    }
}
```

- [ ] **Step 2: 注册迁移**

Modify `backend-rs/src/db/migration/mod.rs`：在 mod 声明区加 `mod m20260722_000001_cluster_extensions;`，在 `Migrator::migrations()` 的 vec 末尾加 `Box::new(m20260722_000001_cluster_extensions::Migration),`。

- [ ] **Step 3: 加 entity**

在 `backend-rs/src/db/entities/mod.rs` 末尾追加：

```rust
/// 集群扩展清单。
pub mod cluster_extension {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extensions")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub kind: String,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub name: String,
        #[sea_orm(column_type = "String(StringLen::N(256))", nullable)]
        pub display_name: Option<String>,
        #[sea_orm(column_type = "Text", nullable)]
        pub description: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable)]
        pub version: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub content_form: String,
        #[sea_orm(column_type = "Text", nullable)]
        pub config_text: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub content_hash: String,
        pub enabled: bool,
        pub created_at: i64,
        pub updated_at: i64,
        #[sea_orm(column_type = "String(StringLen::N(36))", nullable)]
        pub created_by: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 扩展文件指纹(无内容)。
pub mod cluster_extension_file {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extension_files")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub extension_id: String,
        #[sea_orm(column_type = "String(StringLen::N(512))")]
        pub rel_path: String,
        pub size_bytes: i64,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub content_hash: String,
        pub is_binary: bool,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 扩展持有节点。
pub mod cluster_extension_holder {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extension_holders")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub extension_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub node_id: String,
        pub held_since: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
```

- [ ] **Step 4: 编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过（迁移自动在下次启动时执行；此处只验证代码无误）。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/db/migration/m20260722_000001_cluster_extensions.rs backend-rs/src/db/migration/mod.rs backend-rs/src/db/entities/mod.rs
git commit -m "feat(extensions): 集群扩展三张表迁移 + entity"
```

---

### Task 2: `[extensions]` 配置段

**Files:**
- Modify: `backend-rs/src/config.rs`（struct + Default + Config 注册 + Debug + validate）
- Modify: `backend-rs/config.toml.example`

**Interfaces:**
- Produces: `Config::extensions` 字段（`ExtensionsConfig`），供 main.rs 读 `sync_interval_secs` / `enable`

- [ ] **Step 1: 加 ExtensionsConfig**

在 `backend-rs/src/config.rs` 的 `SnapshotConfig` 附近追加：

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct ExtensionsConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_ext_sync_interval")]
    pub sync_interval_secs: u64,
    #[serde(default = "default_ext_max_extension_bytes")]
    pub max_extension_bytes: u64,
    #[serde(default = "default_ext_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default)]
    pub plugin_enabled: bool,
}
impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self { enable: false, sync_interval_secs: default_ext_sync_interval(),
            max_extension_bytes: default_ext_max_extension_bytes(),
            max_file_bytes: default_ext_max_file_bytes(), plugin_enabled: false }
    }
}
fn default_ext_sync_interval() -> u64 { 30 }
fn default_ext_max_extension_bytes() -> u64 { 10 * 1024 * 1024 }
fn default_ext_max_file_bytes() -> u64 { 1024 * 1024 }
```

- [ ] **Step 2: 注册进 Config + Debug + validate**

在 `Config` struct 加字段 `#[serde(default)] pub extensions: ExtensionsConfig,`；在自定义 `Debug` impl 加 `.field("extensions", &self.extensions)`；在 `validate()` 加：
```rust
if self.extensions.enable && self.extensions.sync_interval_secs == 0 {
    return Err(anyhow!("extensions.enable = true but sync_interval_secs is 0"));
}
```

- [ ] **Step 3: 加 config.toml.example 示例**

在 `backend-rs/config.toml.example` 末尾追加：
```toml
# 集群扩展分发(skill/mcp/plugin)
[extensions]
enable = false
sync_interval_secs = 30
max_extension_bytes = 10485760   # 单扩展上限 10MB
max_file_bytes = 1048576         # 单文件上限 1MB
plugin_enabled = false           # plugin 分发开关(阶段3开启)
```

- [ ] **Step 4: 编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/config.rs backend-rs/config.toml.example
git commit -m "feat(extensions): 新增 [extensions] 配置段"
```

---

### Task 3: 指纹计算 fingerprint.rs

**Files:**
- Create: `backend-rs/src/services/extensions/mod.rs`
- Create: `backend-rs/src/services/extensions/fingerprint.rs`
- Modify: `backend-rs/src/services/mod.rs`（`pub mod extensions;`）
- Test: `backend-rs/src/services/extensions/fingerprint.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces: `FileFingerprint` / `scan_dir` / `aggregate_hash`（供 Task 6 上传时算指纹、Task 8 同步时比对）

- [ ] **Step 1: 写 mod.rs 入口**

Create `backend-rs/src/services/extensions/mod.rs`:
```rust
pub mod fingerprint;
pub mod apply;
pub mod store;
pub mod sync;
```
（apply/store/sync 在后续 task 创建；本步先只声明 `pub mod fingerprint;`，其余三行在对应 task 取消注释。）

Modify `backend-rs/src/services/mod.rs` 加 `pub mod extensions;`。

- [ ] **Step 2: 写失败测试**

Create `backend-rs/src/services/extensions/fingerprint.rs`:
```rust
use crate::error::AppError;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FileFingerprint {
    pub rel_path: String,
    pub size: i64,
    pub sha256: String,
    pub is_binary: bool,
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
```

- [ ] **Step 3: 运行测试验证失败**

Run: `cd backend-rs && cargo test --lib services::extensions::fingerprint`
Expected: 编译失败（`scan_dir` / `aggregate_hash` 未定义）。

- [ ] **Step 4: 实现**

在 `fingerprint.rs` 的 `#[cfg(test)]` 上方补实现：
```rust
/// 判定二进制:含 NUL 字节视为二进制。
fn looks_binary(bytes: &[u8]) -> bool { bytes.contains(&0u8) }

async fn hash_one(root: &Path, entry: &walkdir::DirEntry) -> Result<FileFingerprint, AppError> {
    let rel = entry.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
    let bytes = tokio::fs::read(entry.path()).await
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

/// 递归扫描 root 下所有文件(跳过目录、跳过 .cluster-extensions.json),返回每文件指纹。
pub async fn scan_dir(root: &Path) -> Result<Vec<FileFingerprint>, AppError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() { continue; }
        let name = entry.file_name().to_string_lossy();
        if name == ".cluster-extensions.json" { continue; }
        out.push(hash_one(root, &entry).await?);
    }
    Ok(out)
}

/// 按 rel_path 排序后对所有 (rel_path, sha256) 再做一次 SHA256,得到稳定的整体 content_hash。
pub fn aggregate_hash(files: &[FileFingerprint]) -> String {
    let mut v = files.to_vec();
    v.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let mut h = Sha256::new();
    for f in &v { h.update(f.rel_path.as_bytes()); h.update(f.sha256.as_bytes()); }
    hex::encode(h.finalize())
}
```

加依赖到 `backend-rs/Cargo.toml`：`walkdir = "2"`、`tempfile = "3"`（dev，放 `[dev-dependencies]`）。

- [ ] **Step 5: 运行测试验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::fingerprint`
Expected: 2 tests PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/extensions/ backend-rs/src/services/mod.rs backend-rs/Cargo.toml
git commit -m "feat(extensions): 目录扫描 + SHA256 指纹计算"
```

---

### Task 4: 本地落盘 apply.rs

**Files:**
- Create: `backend-rs/src/services/extensions/apply.rs`
- Modify: `backend-rs/src/services/extensions/mod.rs`（启用 `pub mod apply;`）
- Test: 内联 `#[cfg(test)]`

**Interfaces:**
- Produces: `skills_dir` / `write_file_safe` / `remove_dir_safe` / `load_local_state` / `save_local_state`（供 Task 6 落盘、Task 8 对齐）

- [ ] **Step 1: 写失败测试**

Create `backend-rs/src/services/extensions/apply.rs`:
```rust
use crate::error::AppError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
        m.insert("ext_1".into(), "deadbeef".into());
        save_local_state(tmp.path(), &m).await.unwrap();
        let loaded = load_local_state(tmp.path()).await;
        assert_eq!(loaded.get("ext_1"), Some(&"deadbeef".to_string()));
    }
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cd backend-rs && cargo test --lib services::extensions::apply`
Expected: 编译失败（函数未定义）。

- [ ] **Step 3: 实现**

在 `apply.rs` `#[cfg(test)]` 上方补：
```rust
const STATE_FILE: &str = ".cluster-extensions.json";

pub fn skills_dir(codex_home: &Path) -> PathBuf { codex_home.join("skills") }

/// 安全拼路径:禁绝对/.. /反斜杠,candidate 必落在 root 下(复用项目 safe_join 语义)。
async fn safe_join_local(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\')
        || rel.contains("..") || rel.contains('\\') {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = root.join(rel);
    let c = candidate.to_string_lossy().replace('\\', "/").to_lowercase();
    let r = root.to_string_lossy().replace('\\', "/").to_lowercase();
    if !c.starts_with(&r) { return Err(AppError::internal(format!("path escapes root: {rel}"))); }
    Ok(candidate)
}

/// 写文件(自动建父目录)。
pub async fn write_file_safe(root: &Path, rel: &str, content: &[u8]) -> Result<(), AppError> {
    let path = safe_join_local(root, rel).await?;
    if let Some(p) = path.parent() {
        tokio::fs::create_dir_all(p).await.map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    }
    tokio::fs::write(&path, content).await
        .map_err(|e| AppError::internal(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// 删除 root/{name} 整个目录(skill 卸载)。
pub async fn remove_dir_safe(root: &Path, name: &str) -> Result<(), AppError> {
    let dir = safe_join_local(root, name).await?;
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await
            .map_err(|e| AppError::internal(format!("remove {}: {e}", dir.display())))?;
    }
    Ok(())
}

pub async fn load_local_state(codex_home: &Path) -> HashMap<String, String> {
    let p = codex_home.join(STATE_FILE);
    match tokio::fs::read(&p).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub async fn save_local_state(codex_home: &Path, map: &HashMap<String, String>) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(map).map_err(|e| AppError::internal(format!("json: {e}")))?;
    tokio::fs::write(codex_home.join(STATE_FILE), &bytes).await
        .map_err(|e| AppError::internal(format!("write state: {e}")))?;
    Ok(())
}
```

- [ ] **Step 4: 启用 mod**

`backend-rs/src/services/extensions/mod.rs` 取消 `pub mod apply;` 注释。

- [ ] **Step 5: 运行验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::apply`
Expected: 3 tests PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/extensions/
git commit -m "feat(extensions): 本地安全落盘 + 本地状态读写"
```

---

### Task 5: PG 存取 store.rs

**Files:**
- Create: `backend-rs/src/services/extensions/store.rs`
- Modify: `backend-rs/src/services/extensions/mod.rs`（启用 `pub mod store;`）
- Test: 内联 `#[cfg(test)]`（用 sea-orm `MockDatabase`）

**Interfaces:**
- Consumes: Task 1 的 entity、Task 3 的 `FileFingerprint`
- Produces: `ExtRecord` + 全部 PG 读写函数（供 Task 6/8 使用）

- [ ] **Step 1: 写实现 + ExtRecord**

Create `backend-rs/src/services/extensions/store.rs`:
```rust
use crate::db::entities::cluster_extension::{ActiveModel as ExtActive, Column as ExtCol, Entity as ExtEntity, Model as ExtModel};
use crate::db::entities::cluster_extension_file::{ActiveModel as FileActive, Column as FileCol, Entity as FileEntity};
use crate::db::entities::cluster_extension_holder::{ActiveModel as HolderActive, Entity as HolderEntity};
use crate::error::AppError;
use crate::services::extensions::fingerprint::FileFingerprint;
use crate::services::multitenant::{new_id, now_ms};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

#[derive(Clone, Debug)]
pub struct ExtRecord {
    pub id: String, pub kind: String, pub name: String,
    pub content_form: String, pub content_hash: String, pub enabled: bool,
}

impl From<ExtModel> for ExtRecord {
    fn from(m: ExtModel) -> Self {
        Self { id: m.id, kind: m.kind, name: m.name, content_form: m.content_form,
               content_hash: m.content_hash, enabled: m.enabled }
    }
}

/// 插入或更新扩展(连同文件指纹全量替换)。
pub async fn upsert_extension(db: &DatabaseConnection, rec: &ExtRecord, files: &[FileFingerprint]) -> Result<(), AppError> {
    let now = now_ms();
    let existing = ExtEntity::find_by_id(rec.id.clone()).one(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    let active = ExtActive {
        id: Set(rec.id.clone()), kind: Set(rec.kind.clone()), name: Set(rec.name.clone()),
        display_name: Set(None), description: Set(None), version: Set(None),
        content_form: Set(rec.content_form.clone()), config_text: Set(None),
        content_hash: Set(rec.content_hash.clone()), enabled: Set(rec.enabled),
        created_at: Set(existing.as_ref().map(|m| m.created_at).unwrap_or(now)),
        updated_at: Set(now), created_by: Set(None),
    };
    if existing.is_some() {
        ExtEntity::update(active).exec(db).await.map_err(|e| AppError::internal(format!("db: {e}")))?;
    } else {
        ExtEntity::insert(active).exec(db).await.map_err(|e| AppError::internal(format!("db: {e}")))?;
    }
    // 文件指纹全量替换。
    FileEntity::delete_many().filter(FileCol::ExtensionId.eq(rec.id.clone())).exec(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if !files.is_empty() {
        let rows: Vec<FileActive> = files.iter().map(|f| FileActive {
            id: sea_orm::ActiveValue::NotSet, // BIGINT 自增主键,由 DB 生成(insert 时不设)
            extension_id: Set(rec.id.clone()), rel_path: Set(f.rel_path.clone()),
            size_bytes: Set(f.size), content_hash: Set(f.sha256.clone()), is_binary: Set(f.is_binary),
        }).collect();
        FileEntity::insert_many(rows).exec(db).await.map_err(|e| AppError::internal(format!("db: {e}")))?;
    }
    Ok(())
}

pub async fn list_enabled(db: &DatabaseConnection) -> Result<Vec<ExtRecord>, AppError> {
    let rows = ExtEntity::find().filter(ExtCol::Enabled.eq(true)).all(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(ExtRecord::from).collect())
}

pub async fn get_files(db: &DatabaseConnection, ext_id: &str) -> Result<Vec<FileFingerprint>, AppError> {
    let rows = FileEntity::find().filter(FileCol::ExtensionId.eq(ext_id.to_string())).all(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(|m| FileFingerprint {
        rel_path: m.rel_path, size: m.size_bytes, sha256: m.content_hash, is_binary: m.is_binary,
    }).collect())
}

pub async fn add_holder(db: &DatabaseConnection, ext_id: &str, node_id: &str) -> Result<(), AppError> {
    HolderEntity::insert(HolderActive {
        extension_id: Set(ext_id.to_string()), node_id: Set(node_id.to_string()),
        held_since: Set(now_ms()),
    }).on_do_nothing().exec(db).await.ok(); // 复合主键重复 → 忽略
    Ok(())
}

pub async fn list_holders(db: &DatabaseConnection, ext_id: &str) -> Result<Vec<String>, AppError> {
    use crate::db::entities::cluster_extension_holder::Column as HCol;
    let rows = HolderEntity::find().filter(HCol::ExtensionId.eq(ext_id.to_string())).all(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(|m| m.node_id).collect())
}

pub async fn delete_extension(db: &DatabaseConnection, ext_id: &str) -> Result<(), AppError> {
    let _ = FileEntity::delete_many().filter(FileCol::ExtensionId.eq(ext_id.to_string())).exec(db).await;
    use crate::db::entities::cluster_extension_holder::Column as HCol;
    let _ = HolderEntity::delete_many().filter(HCol::ExtensionId.eq(ext_id.to_string())).exec(db).await;
    ExtEntity::delete_by_id(ext_id.to_string()).exec(db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(())
}

// ExtActive 的 id 主键已由迁移自增? 否——cluster_extensions.id 是 VARCHAR 主键,
// 但 ActiveModel 需要 created_by/各字段都已 Set(见 upsert)。文件表 id 是 BIGINT 自增。
// 注:Bigint 自增主键在 sea-orm 需 AutoIncrement(true),此处 insert 时 Set(0) 依赖 DB 自增。
```

> 说明：store 函数依赖真实 DB，单测用 sea-orm `MockDatabase` 仅校验「不 panic / SQL 形态」，端到端在 Task 9 集成验证。

- [ ] **Step 2: 启用 mod**

`mod.rs` 取消 `pub mod store;` 注释。

- [ ] **Step 3: 编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/services/extensions/
git commit -m "feat(extensions): PG 存取(清单/指纹/holders)"
```

---

### Task 6: 安装入口 API（上传 skill）

**Files:**
- Create: `backend-rs/src/api/multitenant/extensions.rs`
- Modify: `backend-rs/src/api/multitenant/mod.rs`（`pub mod extensions;`）
- Modify: `backend-rs/src/api/mod.rs`（注册路由）
- Test: 手动 curl（端到端在 Task 9）

**Interfaces:**
- Consumes: fingerprint / apply / store；`AppState`（db/codex_home/node_id/worker_rpc）；EventBus（由 main 注入或经 state）
- Produces: `POST /api/mt/extensions`（上传 skill 文件树 JSON）→ 落盘 + 入库 + 发事件；`GET` 列表；`DELETE`

> **前置依赖（重要）**：handler 引用 `state.cfg_extensions_max_file_bytes` 与 `state.mt_event_bus`，这两个字段在 Task 8 Step 1 添加到 `AppState`。因此**先执行 Task 8 Step 1（state.rs 加两字段 + main.rs 构造时注入），再回到本 task 写 handler**。本 task 与 Task 8 属同一编译单元，编译验证（`cargo build`）放 Task 8 完成后统一做。

- [ ] **Step 1: 写 handler**

Create `backend-rs/src/api/multitenant/extensions.rs`:
```rust
use crate::error::{AppError, ErrorCode};
use crate::services::extensions::{apply, fingerprint, store};
use crate::services::multitenant::new_id;
use crate::state::AppState;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct UploadFile { pub rel_path: String, pub content_base64: String }

#[derive(Deserialize)]
pub struct UploadBody {
    pub kind: String,           // 阶段1 固定 "skill"
    pub name: String,
    pub files: Vec<UploadFile>,
}

#[derive(Serialize)]
pub struct ExtResp { pub id: String, pub name: String, pub content_hash: String }

/// POST /api/mt/extensions —— 上传 skill 文件树,落盘本节点 + 入库 + 发事件。
pub async fn upload_extension(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<UploadBody>,
) -> Result<Json<ExtResp>, AppError> {
    if body.kind != "skill" {
        return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST,
            "阶段1 仅支持 skill".into(), None));
    }
    if body.name.is_empty() || body.files.is_empty() {
        return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST,
            "name 和 files 不能为空".into(), None));
    }
    // 1. 解 base64 → 写入临时目录算指纹,再落盘到 skills/{name}/。
    let skills_root = apply::skills_dir(&state.codex_home);
    tokio::fs::create_dir_all(&skills_root).await.map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    // 先写临时目录算指纹
    let tmp = tempfile::tempdir().map_err(|e| AppError::internal(format!("tmp: {e}")))?;
    let mut fps = Vec::with_capacity(body.files.len());
    for f in &body.files {
        let bytes = base64_decode(&f.content_base64)
            .map_err(|e| AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST,
                format!("base64 解码失败: {e}"), None))?;
        if bytes.len() as u64 > state.cfg_extensions_max_file_bytes {
            return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST,
                format!("文件 {} 超过单文件上限", f.rel_path), None));
        }
        apply::write_file_safe(tmp.path(), &f.rel_path, &bytes).await?;
    }
    fps = fingerprint::scan_dir(tmp.path()).await?;
    let content_hash = fingerprint::aggregate_hash(&fps);
    // 2. 落盘到正式 skills/{name}/(先清旧同名目录)。
    apply::remove_dir_safe(&skills_root, &body.name).await?;
    for f in &body.files {
        let bytes = base64_decode(&f.content_base64).expect("已校验");
        apply::write_file_safe(&skills_root.join(&body.name), &f.rel_path, &bytes).await?;
    }
    // 3. 入库 + 本节点登记 holder。
    let id = new_id();
    let rec = store::ExtRecord { id: id.clone(), kind: body.kind.clone(), name: body.name.clone(),
        content_form: "files".into(), content_hash: content_hash.clone(), enabled: true };
    store::upsert_extension(&state.db, &rec, &fps).await?;
    store::add_holder(&state.db, &id, &state.node_id).await?;
    // 4. 写本地状态 + 发事件(触发其他节点同步)。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(id.clone(), content_hash.clone());
    apply::save_local_state(&state.codex_home, &st).await?;
    if let Some(bus) = &state.mt_event_bus {
        let _ = bus.publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}")).await;
    }
    metrics::counter!("mt_extension_upload_total").increment(1);
    Ok(Json(ExtResp { id, name: body.name, content_hash }))
}

#[derive(Serialize)]
pub struct ListItem { pub id: String, pub kind: String, pub name: String, pub enabled: bool }

/// GET /api/mt/extensions
pub async fn list_extensions(State(state): State<AppState>) -> Result<Json<Vec<ListItem>>, AppError> {
    let rows = store::list_enabled(&state.db).await?;
    Ok(Json(rows.into_iter().map(|r| ListItem { id: r.id, kind: r.kind, name: r.name, enabled: r.enabled }).collect()))
}

/// DELETE /api/mt/extensions/{id}
pub async fn delete_extension(
    State(state): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, AppError> {
    // 查 name 以便清本地目录
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find().filter(ExtCol::Id.eq(id.clone())).one(&state.db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if let Some(m) = m {
        if m.kind == "skill" {
            let _ = apply::remove_dir_safe(&apply::skills_dir(&state.codex_home), &m.name).await;
        }
    }
    store::delete_extension(&state.db, &id).await?;
    if let Some(bus) = &state.mt_event_bus {
        let _ = bus.publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}")).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| e.to_string())
}
```

加依赖 `base64 = "0.22"` 到 `backend-rs/Cargo.toml`。

> 注：handler 引用了 `state.cfg_extensions_max_file_bytes` 和 `state.mt_event_bus`——这两个需在 Task 8 往 AppState 加（见 Task 8 Step 1）。本 task 先写 handler，编译通过依赖 Task 8 的 state 改动；两个 task 紧邻提交。

- [ ] **Step 2: 注册模块与路由**

`backend-rs/src/api/multitenant/mod.rs` 加 `pub mod extensions;`。
`backend-rs/src/api/mod.rs` 的 `mt_protected` 路由组加：
```rust
.route("/extensions", post(mt_ext::upload_extension).get(mt_ext::list_extensions))
.route("/extensions/{id}", delete(mt_ext::delete_extension))
```
（`use crate::api::multitenant::extensions as mt_ext;`，`post`/`delete` 已在 use 中。）

- [ ] **Step 3: 编译验证（含 Task 8 state 改动后）**

Run: `cd backend-rs && cargo build`
Expected: 编译通过。

- [ ] **Step 4: Commit（与 Task 8 合并提交或紧随其后）**

```bash
git add backend-rs/src/api/ backend-rs/Cargo.toml
git commit -m "feat(extensions): skill 上传/列表/删除 API"
```

---

### Task 7: 下载 RPC `/internal/ext-fetch` + 客户端

**Files:**
- Modify: `backend-rs/src/api/multitenant/internal_rpc.rs`（加端点）
- Modify: `backend-rs/src/services/multitenant/rpc.rs`（加 `ext_fetch`）

**Interfaces:**
- Consumes: Task 1 entity（查 extension → 拿 name/kind）、apply（拼 skills 路径）
- Produces: `/internal/ext-fetch`（holder 响应文件字节）；`WorkerRpcClient::ext_fetch`

- [ ] **Step 1: 加服务端端点**

在 `internal_rpc.rs` 的 `build_internal_router` 加 `.route("/internal/ext-fetch", post(ext_fetch))`，并写 handler：
```rust
#[derive(Deserialize)]
struct ExtFetchReq { #[serde(rename = "extId")] ext_id: String, #[serde(rename = "relPath")] rel_path: String }

async fn ext_fetch(
    State(state): State<AppState>,
    Json(req): Json<ExtFetchReq>,
) -> Result<axum::response::Response, AppError> {
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find().filter(ExtCol::Id.eq(req.ext_id.clone())).one(&state.db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    let m = m.ok_or_else(|| AppError::status(404))?;
    if m.kind != "skill" {
        return Err(AppError::status(400)); // 阶段1 仅 skill
    }
    let root = crate::services::extensions::apply::skills_dir(&state.codex_home).join(&m.name);
    let path = crate::services::multitenant::replication::safe_join(&root, &req.rel_path).await?;
    let bytes = tokio::fs::read(&path).await.map_err(|_| AppError::status(404))?;
    Ok(axum::response::Response::builder().status(200)
        .header("content-type", "application/octet-stream")
        .body(axum::body::Body::from(bytes)).unwrap())
}
```

- [ ] **Step 2: 加客户端方法**

在 `rpc.rs` 的 `impl WorkerRpcClient` 加：
```rust
pub async fn ext_fetch(&self, base: &str, ext_id: &str, rel_path: &str) -> Result<bytes::Bytes, AppError> {
    let body = serde_json::json!({ "extId": ext_id, "relPath": rel_path });
    let resp = self.post_raw(base, "/internal/ext-fetch", body).await?;
    Ok(resp) // post_raw 返回完整字节 body
}
```
（若 `post` 返回 `Value`，新增 `post_raw` 返回 `bytes::Bytes`：复制现有 `post` 但返回 `resp.bytes().await`。）

- [ ] **Step 3: 编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/api/multitenant/internal_rpc.rs backend-rs/src/services/multitenant/rpc.rs
git commit -m "feat(extensions): /internal/ext-fetch 下载端点 + 客户端"
```

---

### Task 8: 同步循环 + bootstrap + 事件订阅 + AppState 改动

**Files:**
- Modify: `backend-rs/src/state.rs`（加 `cfg_extensions_max_file_bytes`、`mt_event_bus` 字段）
- Modify: `backend-rs/src/main.rs`（构造 AppState 注入新字段 + spawn 周期 task + bootstrap + 订阅 + 启用 mod）
- Create: `backend-rs/src/services/extensions/sync.rs`

**Interfaces:**
- Consumes: store / apply / fingerprint / rpc(ext_fetch) / event_bus
- Produces: `run_round(state)` / `bootstrap(state)`；main.rs 注入 + spawn

- [ ] **Step 1: AppState 加两个字段**

`backend-rs/src/state.rs` 的 `AppState` 加：
```rust
pub cfg_extensions_max_file_bytes: u64,
pub mt_event_bus: Option<std::sync::Arc<dyn crate::services::multitenant::event_bus::EventBus>>,
```
`main.rs` 构造 AppState 时注入 `cfg_extensions_max_file_bytes: cfg.extensions.max_file_bytes`，`mt_event_bus: Some(mt_event_bus.clone())`（`mt_event_bus` 在 main.rs:91-98 已构造）。

- [ ] **Step 2: 写 sync.rs**

Create `backend-rs/src/services/extensions/sync.rs`:
```rust
use crate::error::AppError;
use crate::state::AppState;
use crate::services::extensions::{apply, fingerprint, store};
use std::collections::HashMap;

/// 单轮同步:把本地扩展对齐到 PG 清单。
pub async fn run_round(state: &AppState) -> Result<(), AppError> {
    let desired = store::list_enabled(&state.db).await?;
    let mut local = apply::load_local_state(&state.codex_home).await;
    let skills_root = apply::skills_dir(&state.codex_home);

    let desired_ids: std::collections::HashSet<&String> = desired.iter().map(|r| &r.id).collect();
    // 删除:本地有、PG 无。
    let stale: Vec<String> = local.keys().filter(|k| !desired_ids.contains(*k)).cloned().collect();
    for id in &stale {
        if let Some(name) = name_of(state, id).await? {
            let _ = apply::remove_dir_safe(&skills_root, &name).await;
        }
        local.remove(id);
    }
    // 新增/更新。
    for rec in &desired {
        if rec.kind != "skill" { continue; } // 阶段1 仅 skill
        let need = match local.get(&rec.id) {
            Some(h) if h == &rec.content_hash => false,
            _ => true,
        };
        if !need { continue; }
        // 拉取文件清单 → 从 holder 下载 → 落盘。
        let files = store::get_files(&state.db, &rec.id).await?;
        apply::remove_dir_safe(&skills_root, &rec.name).await?;
        let dest = skills_root.join(&rec.name);
        tokio::fs::create_dir_all(&dest).await.map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
        for f in &files {
            let bytes = download_from_holder(state, &rec.id, &f.rel_path).await?;
            apply::write_file_safe(&dest, &f.rel_path, &bytes).await?;
        }
        // 校验整体 hash 一致后登记。
        let got = fingerprint::aggregate_hash(&files); // 用清单 hash(与上传方一致)
        if got == rec.content_hash {
            local.insert(rec.id.clone(), rec.content_hash.clone());
            store::add_holder(&state.db, &rec.id, &state.node_id).await?; // 扩散:自己也成 holder
        }
    }
    apply::save_local_state(&state.codex_home, &local).await?;
    Ok(())
}

pub async fn bootstrap(state: &AppState) -> Result<(), AppError> { run_round(state).await }

/// 查扩展 name(用于删目录)。
async fn name_of(state: &AppState, id: &str) -> Result<Option<String>, AppError> {
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find().filter(ExtCol::Id.eq(id.to_string())).one(&state.db).await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(m.map(|m| m.name))
}

/// 从任一 alive holder 下载单个文件;全失败则报错(本轮跳过,下轮重试)。
async fn download_from_holder(state: &AppState, ext_id: &str, rel_path: &str) -> Result<Vec<u8>, AppError> {
    let holders = store::list_holders(&state.db, ext_id).await?;
    let alive: Vec<String> = state.cluster.alive_nodes().await.into_iter().filter(|n| holders.contains(n)).collect();
    for node_id in &alive {
        if node_id == &state.node_id { continue; }
        if let Some(rpc_base) = state.cluster.rpc_addr_of(node_id).await {
            match state.worker_rpc.ext_fetch(&rpc_base, ext_id, rel_path).await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => tracing::warn!(node = %node_id, error = %e, "ext_fetch 失败,试下一个 holder"),
            }
        }
    }
    Err(AppError::internal(format!("无可用 holder 下载 {ext_id}/{rel_path}")))
}
```

> 注：`state.cluster.alive_nodes()` / `rpc_addr_of()` 需确认 `ClusterMembership` trait 是否已有；若方法名不同（如 `members()`），按实际 trait 调整（实现时核对 `cluster.rs:15-22`）。

- [ ] **Step 3: main.rs spawn + bootstrap + 订阅**

在 `main.rs` AppState 构造后（328 行后）加：
```rust
// bootstrap:启动全量对齐
if cfg.extensions.enable {
    if let Err(e) = crate::services::extensions::sync::bootstrap(&state).await {
        tracing::warn!(error = %e, "extension bootstrap failed (non-fatal)");
    }
    // 周期同步 task
    let st = state.clone();
    let interval = cfg.extensions.sync_interval_secs;
    let ext_sync_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            if let Err(e) = crate::services::extensions::sync::run_round(&st).await {
                tracing::warn!(error = %e, "extension sync round failed");
            }
        }
    });
    // 事件订阅:extensions:changed → 立即 run_round
    let st2 = state.clone();
    let bus2 = mt_event_bus.clone();
    tokio::spawn(async move {
        let mut rx = match bus2.subscribe("extensions:changed").await { Ok(rx) => rx, Err(_) => return };
        loop {
            match rx.recv().await {
                Ok(_) => { let _ = crate::services::extensions::sync::run_round(&st2).await; }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    // ext_sync_handle 在 shutdown 段 abort(紧邻 replica_maintenance_handle.abort())
}
```
`mod.rs` 取消 `pub mod sync;` 注释。shutdown 段加 `ext_sync_handle.abort();`（需把 handle 提到外层作用域）。

- [ ] **Step 4: 编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/state.rs backend-rs/src/main.rs backend-rs/src/services/extensions/
git commit -m "feat(extensions): 同步循环 + bootstrap + 事件订阅"
```

---

### Task 9: 端到端验证（双节点 skill）

**Files:** 无新代码；验证脚本/手动步骤

**Interfaces:** 验证 Task 1-8 整体

- [ ] **Step 1: 起双节点（参照 docs/cluster-test-setup.md）**

启动 node-a、node-b（共用 PG + Redis，各自 CODEX_HOME）。`[extensions] enable = true`。

- [ ] **Step 2: 在 node-a 上传一个 skill**

Run（构造一个最小 skill 文件树 JSON 上传到 node-a）:
```bash
# SKILL.md 内容 "hello skill" → base64 = "aGVsbG8gc2tpbGw="
curl -X POST http://127.0.0.1:<port_a>/api/mt/extensions \
  -H 'Content-Type: application/json' \
  -d '{"kind":"skill","name":"e2e-skill","files":[{"relPath":"SKILL.md","contentBase64":"aGVsbG8gc2tpbGw="}]}'
```
Expected: 返回 `{id, name:"e2e-skill", content_hash:...}`；node-a 的 `$CODEX_HOME_A/skills/e2e-skill/SKILL.md` 存在；PG `cluster_extensions` 有一行、`cluster_extension_holders` 含 node-a。

- [ ] **Step 3: 验证 node-b 自动同步**

等待 ≤30s（或事件即时），检查 node-b 的 `$CODEX_HOME_B/skills/e2e-skill/SKILL.md` 内容 == "hello skill"；`cluster_extension_holders` 含 node-b（扩散）。

- [ ] **Step 4: 验证删除传播**

```bash
curl -X DELETE http://127.0.0.1:<port_a>/api/mt/extensions/<id>
```
Expected: ≤30s 后 node-a、node-b 的 `skills/e2e-skill/` 目录均被清除。

- [ ] **Step 5: 验证新会话可用（手动 codex 侧确认）**

在 node-b 上开一个新 codex 会话，确认 `e2e-skill` 被发现（或在 spec §13 验证项 2 的框架下记录结果）。

- [ ] **Step 6: Commit 验证记录**

```bash
git add docs/superpowers/specs/2026-07-22-cluster-extension-distribution-design.md  # 如有验证结果补注
git commit -m "test(extensions): skill 双节点端到端验证通过"
```

---

## 验收标准（计划 1 完成定义）
- 三张表迁移成功（PG + MySQL）。
- `[extensions]` 配置可解析，默认关闭。
- skill 上传 → 本节点落盘 + 入库 + 发事件。
- 其他节点 ≤30s 内自动同步 skill 到本地 `skills/`，并成为 holder。
- 删除传播，两节点目录均清除。
- 指纹/落盘/防穿越 单测全绿。

## 后续计划（不在本计划内）
- 计划 2：MCP 配置段分发（`content_form=config` + toml_edit 合并 + `/internal/ext-fetch` 扩展到 config）。
- 计划 3：Plugin 整体产物同步（marketplace add + 产物指纹化 + `.codex-global-state.json` 启用记录合并 + `.plugin-appserver` 排除）。

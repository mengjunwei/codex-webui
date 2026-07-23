# 集群扩展分发 — plugin 端到端 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 skill 分发之上，端到端跑通「插件(plugin)」集群分发——管理员通过 webui 装 plugin（后端跑 `codex plugin add`），集群所有节点自动同步 plugin 的 cache 目录 + config 启用段，新会话即可用。

**Architecture:** 复用 skill plan 已建的 files 分发框架（PG 清单 + 节点间 RPC 下载 + holder 扩散 + 三时机）。plugin 增量：① DB 加 `marketplace` 列（plugin 的市场信息）② 新 `config_merge` 模块（`toml_edit` 合并/移除 `config.toml` 的 `[plugins."<name>@<market>"]` 段，未来 MCP 复用）③ apply/store/sync/upload/ext-fetch 增加 `kind=="plugin"` 分支。

**关键事实（来自 plugin 逆向，已实测）**：
- plugin 启用记录 = `config.toml` 的 `[plugins."<name>@<market>"]` 段（`enabled = true`），id **必须带 `@<market>` 后缀**。**不是** `.codex-global-state.json`。
- 产物 = `$CODEX_HOME/plugins/cache/<market>/<plugin>/<version>/`（`.codex-plugin/plugin.json` + `skills/` + `assets/`）。
- `.plugin-appserver/`（codex.exe 等共享运行时）、`.tmp/`、`openai-bundled/`、`openai-primary-runtime/` = **排除同步**（随 codex 自带 / 内置）。

**Tech Stack:** Rust + axum + sea-orm + toml_edit 0.22 + tokio（spawn codex 子进程）。

## Global Constraints

- 复用 skill plan 全局约束：类型 VARCHAR(36)/BIGINT/BOOLEAN/TEXT 不用 JSON/ENUM/ARRAY；raw SQL CREATE TABLE；create_index 助手；id 用 `new_id()`(UUIDv7)；时间戳 `now_ms()`；请求体 `crate::error::Json`、响应 `axum::Json`、错误 `AppError`；safe_join 防穿越；周期 task 保存 handle + shutdown abort；中文注释/commit。
- plugin 落盘只写 `$CODEX_HOME/plugins/cache/<market>/<name>/<version>/` + `config.toml` 的 `[plugins."<id>"]` 段；**绝不碰** `.plugin-appserver/`、`.tmp/`、`auth.json`、`*.sqlite`、`installation_id`。
- 集群所有节点 codex 版本必须一致（plugin 产物与 codex 版本绑定）——部署约束，写入运维文档。
- `kind=="plugin"` 守卫在 upload/ext_fetch/sync 三处一致收敛（阶段二只做 plugin，skill 已通，MCP 留后续）。

## File Structure

**新建：**
- `backend-rs/src/services/extensions/config_merge.rs` — config.toml 段合并/移除工具（`toml_edit`，通用：plugin 用 `[plugins."id"]`，未来 MCP 用 `[mcp_servers."id"]`）

**修改：**
- `backend-rs/src/db/migration/m20260722_000003_cluster_extensions_marketplace.rs` — 加 `marketplace` 列
- `backend-rs/src/db/entities/mod.rs` — `cluster_extension` entity 加 `marketplace` 字段
- `backend-rs/src/services/extensions/apply.rs` — 加 `plugins_cache_dir` + plugin 落盘/删除（调 config_merge）
- `backend-rs/src/services/extensions/store.rs` — `ExtRecord` 加 `marketplace`；upsert/list 适配
- `backend-rs/src/services/extensions/sync.rs` — `run_round`/`sync_one_extension` 加 `kind=="plugin"` 分支
- `backend-rs/src/api/multitenant/extensions.rs` — `upload_extension` 加 plugin 分支（spawn `codex plugin add`）
- `backend-rs/src/api/multitenant/internal_rpc.rs` — `ext_fetch` 加 `kind=="plugin"` 分支

**关键接口（增量，照此引用）：**
```rust
// config_merge.rs
pub async fn ensure_section_kv(cfg_path: &Path, section: &str, key: &str, value: &str) -> Result<(), AppError>;
//   确保 config.toml 有 [section] 段且 key=value（如 [plugins."foo@bar"] enabled="true"）
pub async fn remove_section(cfg_path: &Path, section: &str) -> Result<(), AppError>;
//   移除 [section] 段

// apply.rs（增量）
pub fn plugins_cache_dir(codex_home: &Path) -> PathBuf; // codex_home/plugins/cache
pub fn plugin_dest(codex_home: &Path, market: &str, name: &str, version: &str) -> PathBuf;
//   = plugins_cache_dir/market/name/version

// store.rs（增量）ExtRecord 加字段
pub struct ExtRecord { ... pub marketplace: Option<String> /* plugin 用 */ ... }

// extensions.rs upload 增量：plugin 分支调 codex plugin add（见 Task 5）
```

---

### Task 1: DB 加 `marketplace` 列 + entity

**Files:**
- Create: `backend-rs/src/db/migration/m20260722_000003_cluster_extensions_marketplace.rs`
- Modify: `backend-rs/src/db/migration/mod.rs`（注册）
- Modify: `backend-rs/src/db/entities/mod.rs`（`cluster_extension` 加 `marketplace` 字段）
- Modify: `backend-rs/src/services/extensions/store.rs`（`ExtRecord` 加 `marketplace: Option<String>` + upsert/list/from 适配）

**Interfaces:** Produces `cluster_extensions.marketplace` 列 + `ExtRecord.marketplace`

- [ ] **Step 1: 写迁移**

Create `backend-rs/src/db/migration/m20260722_000003_cluster_extensions_marketplace.rs`:
```rust
//! cluster_extensions 加 marketplace 列(plugin 的市场名,skill/mcp 为 NULL)。
use sea_orm_migration::prelude::*;
pub struct Migration;
impl MigrationName for Migration { fn name(&self) -> &str { "m20260722_000003_cluster_extensions_marketplace" } }
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // PG/MySQL 都支持 ADD COLUMN IF NOT EXISTS(PG 11+, MySQL 8.0+ 也支持 IF NOT EXISTS;MySQL 旧版无则去 IF,靠 .ok() 容错)
        db.execute_unprepared("ALTER TABLE cluster_extensions ADD COLUMN IF NOT EXISTS marketplace VARCHAR(128)").await
            .map_err(|e| DbErr::Migration(e.to_string()))?;
        // 给 plugin 行建索引(按 marketplace 查)
        crate::db::migration::create_index(manager, "idx_ext_marketplace", "cluster_extensions", "marketplace").await?;
        db.execute_unprepared("COMMENT ON COLUMN cluster_extensions.marketplace IS 'plugin 的市场名(skill/mcp 为空)'").await.ok();
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE cluster_extensions DROP COLUMN IF EXISTS marketplace").await
            .map_err(|e| DbErr::Migration(e.to_string()))?;
        Ok(())
    }
}
```
> 注：`ALTER TABLE ... ADD/DROP COLUMN IF NOT EXISTS` PG/MySQL 8.0+ 都支持；MySQL 5.7 无 IF NOT EXISTS 时会失败被 `?` 上抛——若需兼容 5.7，改用 try/catch `.ok()` 模式（参照项目其他迁移）。

- [ ] **Step 2: 注册迁移**

`mod.rs`：加 `mod m20260722_000003_cluster_extensions_marketplace;` + `Migrator::migrations()` vec 末尾加 `Box::new(m20260722_000003_cluster_extensions_marketplace::Migration),`。

- [ ] **Step 3: entity + ExtRecord 加字段**

`db/entities/mod.rs` 的 `cluster_extension` 模块 Model 加：
```rust
#[sea_orm(column_type = "String(StringLen::N(128))", nullable)]
pub marketplace: Option<String>,
```
`store.rs` 的 `ExtRecord` 加 `pub marketplace: Option<String>`，`From<ExtModel>` 映射 `marketplace: m.marketplace`，`upsert_extension` 里 `marketplace: Set(rec.marketplace.clone())`。

- [ ] **Step 4: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/db/migration/m20260722_000003_cluster_extensions_marketplace.rs backend-rs/src/db/migration/mod.rs backend-rs/src/db/entities/mod.rs backend-rs/src/services/extensions/store.rs
git commit -m "feat(extensions): cluster_extensions 加 marketplace 列(plugin)"
```

---

### Task 2: config_merge 模块（toml_edit 段合并/移除）

**Files:**
- Create: `backend-rs/src/services/extensions/config_merge.rs`
- Modify: `backend-rs/src/services/extensions/mod.rs`（`pub mod config_merge;`）
- Test: 内联 `#[cfg(test)]`

**Interfaces:** Produces `ensure_section_kv` / `remove_section`（plugin + 未来 MCP 复用）

- [ ] **Step 1: 写失败测试**

Create `backend-rs/src/services/extensions/config_merge.rs`:
```rust
use crate::error::AppError;
use std::path::Path;
use toml_edit::{DocumentMut, Item, value};

/// 确保 config.toml 有 [section] 段且 key=value;段不存在则建,已存在则更新 key(保留其他 key)。
/// section 形如 `plugins."foo@bar"` 或 `mcp_servers.xxx`(含引号/点按 TOML 规则)。
pub async fn ensure_section_kv(cfg_path: &Path, section: &str, key: &str, value: &str) -> Result<(), AppError> { todo!() }

/// 移除 config.toml 的 [section] 段;不存在视为成功。
pub async fn remove_section(cfg_path: &Path, section: &str) -> Result<(), AppError> { todo!() }

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ensure_creates_and_updates_quoted_section() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[model_providers.custom]\nname = \"x\"\n").await.unwrap();
        // 建 [plugins."foo@bar"] enabled="true"
        ensure_section_kv(&p, "plugins.\"foo@bar\"", "enabled", "true").await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("[plugins.\"foo@bar\"]"));
        assert!(s.contains("enabled = \"true\""));
        assert!(s.contains("[model_providers.custom]")); // 原段保留
    }
    #[tokio::test]
    async fn remove_drops_section_keeps_others() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[plugins.\"a@m\"]\nenabled = \"true\"\n[model_providers.x]\nname=\"y\"\n").await.unwrap();
        remove_section(&p, "plugins.\"a@m\"").await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(!s.contains("plugins.\"a@m\""));
        assert!(s.contains("[model_providers.x]"));
    }
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cd backend-rs && cargo test --lib services::extensions::config_merge`。Expected: 编译失败或 todo panic。

- [ ] **Step 3: 实现**

实现（`DocumentMut` + `entry(section)` 进入/建段，`as_table_mut` 设 key；段 key 是 quoted 形式如 `plugins."foo@bar"`，toml_edit 的 `doc.entry("plugins.\"foo@bar\"")` 不会被自动解析为嵌套——需用 `doc.entry("plugins")` 进 `plugins` 表再 `entry("foo@bar")`。所以 section 要拆成 `(parent, leaf)`）：
```rust
pub async fn ensure_section_kv(cfg_path: &Path, section: &str, key: &str, value: &str) -> Result<(), AppError> {
    let existing = tokio::fs::read_to_string(cfg_path).await.unwrap_or_default();
    let mut doc = existing.parse::<DocumentMut>().map_err(|e| AppError::internal(format!("parse config: {e}")))?;
    let (parent, leaf) = split_section(section); // "plugins.\"foo@bar\"" -> ("plugins", "foo@bar")
    let p = doc.entry(parent).or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let tbl = p.as_table_mut().ok_or_else(|| AppError::internal(format!("config [{parent}] 不是表")))?;
    let leaf_tbl = tbl.entry(leaf).or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let lt = leaf_tbl.as_table_mut().ok_or_else(|| AppError::internal(format!("config [{section}] 不是表")))?;
    set_kv(lt, key, value);
    let merged = doc.to_string();
    if merged == existing { return Ok(()); }
    tokio::fs::write(cfg_path, merged).await.map_err(|e| AppError::internal(format!("write config: {e}")))?;
    Ok(())
}

pub async fn remove_section(cfg_path: &Path, section: &str) -> Result<(), AppError> {
    let existing = match tokio::fs::read_to_string(cfg_path).await { Ok(s) => s, Err(_) => return Ok(()) };
    let mut doc = match existing.parse::<DocumentMut>() { Ok(d) => d, Err(_) => return Ok(()) };
    let (parent, leaf) = split_section(section);
    if let Some(Item::Table(tbl)) = doc.get_mut(parent) {
        tbl.remove(leaf);
    }
    let merged = doc.to_string();
    if merged != existing { let _ = tokio::fs::write(cfg_path, merged).await; }
    Ok(())
}

/// "plugins.\"foo@bar\"" -> ("plugins", "foo@bar");"mcp_servers.xxx" -> ("mcp_servers", "xxx")
fn split_section(section: &str) -> (String, String) {
    if let Some(dot) = section.find('.') {
        (section[..dot].to_string(), section[dot+1..].trim_matches('"').to_string())
    } else {
        (section.to_string(), String::new())
    }
}

fn set_kv(tbl: &mut toml_edit::Table, key: &str, val: &str) {
    if let Some(item) = tbl.get_mut(key) {
        if item.is_value() { *item = value(val); return; }
    }
    tbl.insert(key, value(val));
}
```

- [ ] **Step 4: mod.rs 加 `pub mod config_merge;`**

- [ ] **Step 5: 运行验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::config_merge`。Expected: 2 tests PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/extensions/config_merge.rs backend-rs/src/services/extensions/mod.rs
git commit -m "feat(extensions): config_merge 模块(toml_edit 段合并/移除)"
```

---

### Task 3: apply 加 plugin 落盘/删除

**Files:**
- Modify: `backend-rs/src/services/extensions/apply.rs`
- Test: 内联 `#[cfg(test)]`

**Interfaces:** Produces `plugins_cache_dir` / `plugin_dest`；复用 `write_file_safe`

- [ ] **Step 1: 写失败测试 + 实现**

在 `apply.rs` 加：
```rust
use crate::services::extensions::config_merge;

pub fn plugins_cache_dir(codex_home: &Path) -> PathBuf { codex_home.join("plugins").join("cache") }

pub fn plugin_dest(codex_home: &Path, market: &str, name: &str, version: &str) -> PathBuf {
    plugins_cache_dir(codex_home).join(market).join(name).join(version)
}

/// plugin 落盘后写启用段到 config.toml:[plugins."<name>@<market>"] enabled="true"
pub async fn enable_plugin_config(codex_home: &Path, name: &str, market: &str) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("plugins.\"{name}@{market}\"");
    config_merge::ensure_section_kv(&cfg, &section, "enabled", "true").await
}

/// 删除 plugin 时移除启用段
pub async fn disable_plugin_config(codex_home: &Path, name: &str, market: &str) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("plugins.\"{name}@{market}\"");
    config_merge::remove_section(&cfg, &section).await
}
```
单测（plugin_dest 路径拼接 + enable/disable config roundtrip，用 tempdir 写 config.toml）：
```rust
#[test]
fn plugin_dest_path() {
    let h = Path::new("/home/x");
    assert_eq!(plugin_dest(h, "mkt", "foo", "1.2.3"), Path::new("/home/x/plugins/cache/mkt/foo/1.2.3"));
}
#[tokio::test]
async fn plugin_config_enable_disable_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    enable_plugin_config(tmp.path(), "foo", "mkt").await.unwrap();
    let s = tokio::fs::read_to_string(tmp.path().join("config.toml")).await.unwrap();
    assert!(s.contains("[plugins.\"foo@mkt\"]"));
    disable_plugin_config(tmp.path(), "foo", "mkt").await.unwrap();
    let s2 = tokio::fs::read_to_string(tmp.path().join("config.toml")).await.unwrap();
    assert!(!s2.contains("foo@mkt"));
}
```

- [ ] **Step 2: 运行验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::apply`。Expected: 含新测试全 PASS。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/services/extensions/apply.rs
git commit -m "feat(extensions): apply 加 plugin 落盘路径 + 启用段合并"
```

---

### Task 4: ext-fetch 支持 plugin（下载端点）

**Files:**
- Modify: `backend-rs/src/api/multitenant/internal_rpc.rs`（`ext_fetch` handler 加 plugin 分支）
- Modify: `backend-rs/src/services/extensions/store.rs`（如需按 id 查 marketplace/version——实际 ext_fetch 需 market+name+version 拼 root）

**Interfaces:** Consumes Task 1 entity（marketplace 列）+ Task 3 `plugin_dest`

- [ ] **Step 1: ext_fetch 加 plugin 分支**

`ext_fetch` handler 当前只处理 `kind=="skill"`（`skills_dir`）。加 `kind=="plugin"` 分支：
```rust
// 查到 extension Model m 后:
let root = if m.kind == "skill" {
    crate::services::extensions::apply::skills_dir(&state.codex_home).join(&m.name)
} else if m.kind == "plugin" {
    let market = m.marketplace.as_deref().unwrap_or("");
    // version: 用 cluster_extension_files 之外的扩展元数据?——version 存在 cluster_extensions.version 列
    let version = m.version.as_deref().unwrap_or("");
    crate::services::extensions::apply::plugin_dest(&state.codex_home, market, &m.name, version)
} else {
    return Err(AppError::status(400));
};
// 后续 safe_join + read 不变
```

- [ ] **Step 2: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/api/multitenant/internal_rpc.rs
git commit -m "feat(extensions): ext-fetch 支持 plugin 文件下载"
```

---

### Task 5: upload plugin API（spawn codex plugin add）

**Files:**
- Modify: `backend-rs/src/api/multitenant/extensions.rs`（`upload_extension` 加 plugin 分支）

**Interfaces:** Consumes Task 1-4

- [ ] **Step 1: upload 加 plugin 分支**

`upload_extension` 当前只处理 skill。加 plugin 分支（请求体 `{kind:"plugin", name, marketplace}`，无 files——后端调 codex 装好后自己扫描）：
```rust
if body.kind == "plugin" {
    return upload_plugin(state, body).await;
}
// 原 skill 逻辑...

async fn upload_plugin(state: AppState, body: UploadBody) -> Result<Json<ExtResp>, AppError> {
    // body 需加 marketplace 字段(改 UploadBody: pub marketplace: Option<String>)
    let market = body.marketplace.clone().unwrap_or_else(|| "openai-api-curated".into());
    // 1. 后端 spawn: codex plugin add <name>@<market>  (CODEX_HOME = state.codex_home)
    let codex_bin = state.codex.bin_path(); // 核对 CodexProcessManager 的 bin 路径获取方式
    let mut cmd = tokio::process::Command::new(codex_bin);
    cmd.arg("plugin").arg("add").arg(format!("{}@{}", body.name, market))
       .env("CODEX_HOME", &state.codex_home)
       .current_dir(&state.codex_home);
    let out = cmd.output().await.map_err(|e| AppError::internal(format!("spawn codex plugin add: {e}")))?;
    if !out.status.success() {
        return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST,
            format!("codex plugin add 失败: {}", String::from_utf8_lossy(&out.stderr)), None));
    }
    // 2. 扫描 cache/<market>/<name>/ 下 version 子目录(取唯一/最新)
    let base = crate::services::extensions::apply::plugins_cache_dir(&state.codex_home).join(&market).join(&body.name);
    let version = pick_version(&base).await?; // 读唯一子目录名,或最大 mtime
    // 3. 指纹化 cache/<market>/<name>/<version>/
    let dest = crate::services::extensions::apply::plugin_dest(&state.codex_home, &market, &body.name, &version);
    let fps = crate::services::extensions::fingerprint::scan_dir(&dest).await?;
    let content_hash = crate::services::extensions::fingerprint::aggregate_hash(&fps);
    // 4. 写启用段 + 入库 + holder + 发事件
    crate::services::extensions::apply::enable_plugin_config(&state.codex_home, &body.name, &market).await?;
    let id = crate::services::extensions::store::find_id_by_kind_name(&state.db, "plugin", &body.name).await?
        .unwrap_or_else(crate::services::multitenant::new_id);
    let rec = crate::services::extensions::store::ExtRecord {
        id: id.clone(), kind: "plugin".into(), name: body.name.clone(),
        content_form: "files".into(), content_hash: content_hash.clone(), enabled: true,
        marketplace: Some(market.clone()),
    };
    // upsert 时 version 单独存(需 store::upsert_extension 支持 version 字段——见 Task 1 的 ExtRecord 是否含 version)
    crate::services::extensions::store::upsert_extension(&state.db, &rec, &fps).await?;
    // 注意:version 存 cluster_extensions.version 列——ExtRecord 加 version 字段或 upsert 单独传
    // 5. 本地 state + 发事件(同 skill)
    // ... (load_local_state insert {name, hash} + save + publish "extensions:changed")
    Ok(Json(ExtResp { id, name: body.name, content_hash }))
}

async fn pick_version(base: &Path) -> Result<String, AppError> {
    let mut rd = tokio::fs::read_dir(base).await.map_err(|e| AppError::internal(format!("read plugin dir: {e}")))?;
    let mut versions = Vec::new();
    while let Some(e) = rd.next_entry().await.map_err(|e| AppError::internal(format!("readdir: {e}")))? {
        if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            versions.push(e.file_name().to_string_lossy().to_string());
        }
    }
    versions.sort();
    versions.into_iter().next().ok_or_else(|| AppError::internal("plugin cache 无 version 子目录"))
}
```
> **实现时探查/确认**：
> - `state.codex.bin_path()` 的确切方法名（读 `CodexProcessManager` 看怎么拿 codex 二进制路径；可能 `codex.bin` 字段或 getter）。
> - `UploadBody` 加 `marketplace: Option<String>` 字段。
> - `ExtRecord` 是否已含 `version`——Task 1 加了 `marketplace`，version 用 cluster_extensions 现有 `version` 列。需让 `ExtRecord` 加 `version: Option<String>` 并在 upsert 写入（upsert_extension 当前没处理 version——补上 `version: Set(rec.version.clone())`）。
> - `pick_version` 取最新/唯一 version；若多 version 需选最大 mtime。

- [ ] **Step 2: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过（探查 bin_path/version 字段后）。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/api/multitenant/extensions.rs backend-rs/src/services/extensions/store.rs
git commit -m "feat(extensions): upload plugin(spawn codex plugin add + 指纹化 cache)"
```

---

### Task 6: sync 支持 plugin（run_round 分支）

**Files:**
- Modify: `backend-rs/src/services/extensions/sync.rs`（`sync_one_extension` 加 plugin 分支）
- Modify: `backend-rs/src/services/extensions/store.rs`（`ExtRecord` 加 `version`，list_enabled 返回）

**Interfaces:** Consumes Task 1-5

- [ ] **Step 1: sync_one_extension 加 plugin 分支**

当前 `sync_one_extension` 只处理 skill（`skills_dir`）。抽出"落盘根目录"按 kind 分发：
```rust
async fn sync_one_extension(state: &AppState, rec: &ExtRecord, local: &mut HashMap<String, LocalExtEntry>) -> Result<(), AppError> {
    let need = match local.get(&rec.id) { Some(e) if e.hash == rec.content_hash => false, _ => true };
    if !need { return Ok(()); }
    let files = store::get_files(&state.db, &rec.id).await?;
    // 落盘根目录按 kind:
    let (dest, name_for_state) = match rec.kind.as_str() {
        "skill" => (apply::skills_dir(&state.codex_home).join(&rec.name), rec.name.clone()),
        "plugin" => {
            let market = rec.marketplace.clone().unwrap_or_default();
            let version = rec.version.clone().unwrap_or_default();
            (apply::plugin_dest(&state.codex_home, &market, &rec.name, &version), rec.name.clone())
        }
        _ => return Ok(()), // 未知 kind 跳过(MCP 留后续)
    };
    // 清旧 + 建 + 逐文件下载(复用 holder_candidates + download_from_holder,逻辑同 skill)
    apply::remove_dir_safe(dest.parent().unwrap_or(dest.as_path()), &name_for_state).await.ok();
    tokio::fs::create_dir_all(&dest).await.map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    let holders = holder_candidates(state, &rec.id).await;
    for f in &files {
        let bytes = download_from_holder(state, &holders, &rec.id, &f.rel_path).await?;
        apply::write_file_safe(&dest, &f.rel_path, &bytes).await?;
    }
    // 落盘 hash 校验
    let landed = fingerprint::scan_dir(&dest).await?;
    let got = fingerprint::aggregate_hash(&landed);
    if got == rec.content_hash {
        if rec.kind == "plugin" {
            let market = rec.marketplace.clone().unwrap_or_default();
            apply::enable_plugin_config(&state.codex_home, &rec.name, &market).await?;
        }
        local.insert(rec.id.clone(), LocalExtEntry { name: name_for_state, hash: rec.content_hash.clone() });
        store::add_holder(&state.db, &rec.id, &state.node_id).await?;
    } else {
        let _ = apply::remove_dir_safe(dest.parent().unwrap_or(dest.as_path()), &name_for_state).await;
        tracing::warn!(ext=%rec.id, expected=%rec.content_hash, got=%got, "plugin 落盘 hash 不匹配,清理重试");
    }
    Ok(())
}
```
> **注意**：`remove_dir_safe` 的"删旧"对 plugin 要删 `cache/<market>/<name>/`（整个 name 目录，含所有 version），不是只删 version 子目录——调整 `remove_dir_safe` 调用的 root/name。上面用 `dest.parent()`（=cache/<market>/<name> 的父 = cache/<market>）+ name_for_state 不对——需删 `cache/<market>/<name>/`：root = `cache/<market>`, name = `<plugin_name>`。实现时核对路径。

- [ ] **Step 2: ExtRecord 加 version + store 适配**

`ExtRecord` 加 `version: Option<String>`，`From<ExtModel>` 映射，`upsert_extension` 写 `version: Set(rec.version.clone())`，`list_enabled` 返回。

- [ ] **Step 3: stale 删除分支适配 plugin**

`run_round` 的 stale 删除（本地有 PG 无）：skill 删 `skills/{name}/`，plugin 删 `cache/<market>/<name>/` + `disable_plugin_config`。用 `local[id].name` + kind 判断（需从 PG 查 kind/marketplace——但 PG 行已删；stale 删除的 kind 从 local_state 取？local_state 只存 name+hash，没 kind/marketplace）。
> **设计抉择**：stale 删除需要 kind/marketplace 来构造路径，但 PG 行已删、local_state 只存 name+hash。两个方案：(a) local_state 存 kind/marketplace（改 LocalExtEntry 加字段）；(b) stale 删除时按 name 在 skills/ 和 plugins/cache/ 下都尝试删。**推荐 (a)**：`LocalExtEntry` 加 `kind: String` + `market: Option<String>` + `version: Option<String>`，upload/sync 写入，stale 删除用本地存的构造路径。实现时改 LocalExtEntry + 所有写入点。

- [ ] **Step 4: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/extensions/sync.rs backend-rs/src/services/extensions/store.rs backend-rs/src/services/extensions/apply.rs
git commit -m "feat(extensions): sync 支持 plugin(kind 分支 + 本地状态存 kind/market/version)"
```

---

### Task 7: 端到端验证 plugin

**Files:** 无新代码；验证步骤

**Interfaces:** 验证 Task 1-6 整体

- [ ] **Step 1: 起环境**

参照 skill plan Task 9（停旧节点 → cargo build → 起 node-a/b/c，`[extensions] enable=true`）。确认迁移 000001/000002/000003 都 applied。

- [ ] **Step 2: 上传 plugin 到 node-a**

```bash
# admin token(同 skill 验证)
curl -X POST http://127.0.0.1:8182/api/mt/extensions \
  -H 'Content-Type: application/json' -H "Authorization: Bearer <admin_token>" \
  -d '{"kind":"plugin","name":"zotero","marketplace":"openai-api-curated"}'
```
Expected: 返回 id + content_hash；node-a 的 `plugins/cache/openai-api-curated/zotero/<version>/` 存在；config.toml 有 `[plugins."zotero@openai-api-curated"]` enabled="true"；PG 一行 + holders 含 node-a。

- [ ] **Step 3: 验证同步**

等 ~10s，检查 node-b/c 的 `plugins/cache/openai-api-curated/zotero/<version>/` 内容一致 + config.toml 有启用段 + holders 含 b/c（扩散）。

- [ ] **Step 4: 验证 codex 识别**

在 node-b 跑 `CODEX_HOME=<node-b_home> codex plugin list`，确认 `zotero@openai-api-curated` 显示 installed（非 not installed）。

- [ ] **Step 5: DELETE 验证传播**

curl DELETE，等同步，验证 node-a/b/c 的 `plugins/cache/.../zotero/` 清 + config 启用段移除 + PG 行删 + state 清。

- [ ] **Step 6: Commit 验证记录**

```bash
git commit --allow-empty -m "test(extensions): plugin 双节点端到端验证通过"
```

---

## 验收标准（plugin 计划完成定义）
- `marketplace`/`version` 列迁移成功（PG+MySQL）。
- `config_merge` 模块单测通过（段合并/移除，含 quoted key `[plugins."a@b"]`）。
- upload plugin（spawn codex plugin add）→ 本节点 cache + config 段 + 入库 + 发事件。
- 其他节点 ≤30s 同步 plugin cache 目录 + config 启用段，`codex plugin list` 显示 installed。
- DELETE 传播：cache 目录 + config 段全清。
- 全程不碰 `.plugin-appserver`/`.tmp`/`auth.json`/`*.sqlite`。

## 待实现时验证/探查的点（非空 TODO）
1. `CodexProcessManager` 拿 codex 二进制路径的确切方法（Task 5 upload spawn 用）。
2. plugin cache 多 version 时选哪个（`pick_version`：唯一/最新 mtime）。
3. `LocalExtEntry` 加 kind/marketplace/version 字段后，所有写入点（upload/sync）一致（Task 6）。
4. stale 删除 plugin 时路径构造（cache/<market>/<name>/，root=cache/<market> name=<plugin>）。
5. marketplace `openai-api-curated` 是内置保留名，per-node 自维护 `.tmp/`——若目标节点无该 marketplace 配置，`codex plugin list` 是否仍显示 installed（cache 文件在 + config 段在应够，实测确认）。

## 后续计划
- 计划 2（MCP）：复用 Task 2 的 `config_merge`（`[mcp_servers."id"]` 段）+ files 框架。

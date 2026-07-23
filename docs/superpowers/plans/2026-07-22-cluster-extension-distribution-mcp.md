# 集群扩展分发 — MCP 端到端 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 skill + plugin 之上，端到端跑通「MCP」集群分发——管理员上传 MCP 配置段，集群所有节点自动合并到本地 `config.toml` 的 `[mcp_servers."name"]` 段，codex 能连该 MCP。

**Architecture:** MCP 是**纯配置**（无文件）：`content_form="config"`，配置段文本存 PG `config_text`；同步 = 把段合并进各节点 `config.toml [mcp_servers."name"]`。复用 skill/plugin 已建的 store/sync/holder/三时机框架；新增 `config_merge::merge_full_section`（多字段段合并，区别于 plugin 的单 key）；**不走 ext-fetch/文件下载**。

**关键事实**：MCP = `config.toml` 的 `[mcp_servers."name"]` 段（含 `command`/`args`/`env` 等多字段），`codex mcp` 命令管理。无文件产物。

**Tech Stack:** Rust + toml_edit 0.22 + sea-orm + axum。

## Global Constraints

- 复用全局约束：类型约定、raw SQL、create_index、new_id/now_ms、crate::error::Json、AppError、safe_join、周期 task handle、中文注释/commit。
- MCP 只写 `config.toml` 的 `[mcp_servers."name"]` 段；**绝不碰** auth.json/*.sqlite/installation_id/sessions。
- `kind=="mcp"` 守卫在 upload/sync/delete 一致。
- config_text 存**段内容**（无 `[mcp_servers.name]` 头），如 `command = "node"\nargs = ["server.js"]`；merge 时由 `merge_full_section` 包头 parse。

## File Structure

**新建：** 无（全部改现有文件）

**修改：**
- `backend-rs/src/services/extensions/config_merge.rs` — 加 `merge_full_section`（多字段段合并）
- `backend-rs/src/services/extensions/apply.rs` — 加 `enable_mcp_config`/`disable_mcp_config`
- `backend-rs/src/services/extensions/store.rs` — `ExtRecord` 加 `config_text: Option<String>` + upsert/From
- `backend-rs/src/api/multitenant/extensions.rs` — `upload_extension` 加 mcp 分支 + `UploadBody.config_text`；`delete_extension` 加 mcp 分支
- `backend-rs/src/services/extensions/sync.rs` — `sync_one_extension` 加 mcp 分支；`cleanup_local_extension` 加 mcp 分支

**关键接口（增量）：**
```rust
// config_merge.rs
pub async fn merge_full_section(cfg_path: &Path, parent: &str, leaf: &str, content_toml: &str) -> Result<(), AppError>;
//   把 content_toml(段内容,无头) 合并进 config.toml 的 [parent.leaf] 段(逐 key clone,含嵌套)

// apply.rs
pub async fn enable_mcp_config(codex_home: &Path, name: &str, content_toml: &str) -> Result<(), AppError>;
//   = merge_full_section(config.toml, "mcp_servers", name, content_toml)
pub async fn disable_mcp_config(codex_home: &Path, name: &str) -> Result<(), AppError>;
//   = remove_section(config.toml, "mcp_servers.name")

// store.rs ExtRecord 加: pub config_text: Option<String>
```

---

### Task 1: config_merge 加 `merge_full_section`（多字段段合并）

**Files:**
- Modify: `backend-rs/src/services/extensions/config_merge.rs`
- Test: 内联 `#[cfg(test)]`

- [ ] **Step 1: 写失败测试**

在 `config_merge.rs` 加：
```rust
/// 把 content_toml(段内容,无 [parent.leaf] 头) 合并进 config.toml 的 [parent.leaf] 段。
/// 实现:包头 parse 成 doc,取其 [parent.leaf] table,逐 key clone 到目标 doc 的 [parent.leaf]。
/// 支持嵌套值(env = { ... } 等),因 toml_edit::Item clone 递归。
pub async fn merge_full_section(cfg_path: &Path, parent: &str, leaf: &str, content_toml: &str) -> Result<(), AppError> { todo!() }

#[cfg(test)]
mod merge_full_tests {
    use super::*;
    #[tokio::test]
    async fn merges_multifield_section_preserving_others() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[model_providers.custom]\nname = \"x\"\n").await.unwrap();
        let content = "command = \"node\"\nargs = [\"s.js\"]\n";
        merge_full_section(&p, "mcp_servers", "myserver", content).await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("[mcp_servers.myserver]") || s.contains("[mcp_servers.\"myserver\"]"));
        assert!(s.contains("command"));
        assert!(s.contains("s.js"));
        assert!(s.contains("[model_providers.custom]")); // 原段保留
    }
    #[tokio::test]
    async fn merge_is_idempotent_and_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        merge_full_section(&p, "mcp_servers", "s", "command = \"a\"\n").await.unwrap();
        // 再合并不同内容,应更新
        merge_full_section(&p, "mcp_servers", "s", "command = \"b\"\n").await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("b"));
    }
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cd backend-rs && cargo test --lib services::extensions::config_merge::merge_full`。Expected: todo panic。

- [ ] **Step 3: 实现**

```rust
pub async fn merge_full_section(cfg_path: &Path, parent: &str, leaf: &str, content_toml: &str) -> Result<(), AppError> {
    // 包头 parse: [parent.leaf] + content
    let wrapped = format!("[{parent}.{leaf}]\n{content_toml}\n");
    let src = wrapped.parse::<DocumentMut>().map_err(|e| AppError::internal(format!("parse mcp content: {e}")))?;
    let src_table = src.get(parent).and_then(|i| i.as_table())
        .and_then(|t| t.get(leaf)).and_then(|i| i.as_table())
        .ok_or_else(|| AppError::internal("merge_full_section: 解析后取不到段 table".into()))?;

    let existing = tokio::fs::read_to_string(cfg_path).await.unwrap_or_default();
    let mut doc = existing.parse::<DocumentMut>().map_err(|e| AppError::internal(format!("parse config: {e}")))?;
    let p = doc.entry(parent).or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let ptbl = p.as_table_mut().ok_or_else(|| AppError::internal(format!("config [{parent}] 不是表")))?;
    let leaf_item = ptbl.entry(leaf).or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let ltbl = leaf_item.as_table_mut().ok_or_else(|| AppError::internal(format!("config [{parent}.{leaf}] 不是表")))?;
    for (k, v) in src_table.iter() {
        ltbl.insert(k.clone(), v.clone());
    }
    let merged = doc.to_string();
    if merged == existing { return Ok(()); }
    tokio::fs::write(cfg_path, merged).await.map_err(|e| AppError::internal(format!("write config: {e}")))?;
    Ok(())
}
```

- [ ] **Step 4: 运行验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::config_merge`。Expected: 全 PASS（含新 2 + 原 ensure/remove 测试）。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/extensions/config_merge.rs
git commit -m "feat(extensions): config_merge 加 merge_full_section(多字段段合并)"
```

---

### Task 2: apply 加 `enable_mcp_config`/`disable_mcp_config`

**Files:**
- Modify: `backend-rs/src/services/extensions/apply.rs`
- Test: 内联 `#[cfg(test)]`

- [ ] **Step 1: 写实现 + 测试**

在 `apply.rs` 加：
```rust
/// MCP 启用:把 content_toml 段内容合并进 config.toml [mcp_servers.<name>]。
pub async fn enable_mcp_config(codex_home: &Path, name: &str, content_toml: &str) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    crate::services::extensions::config_merge::merge_full_section(&cfg, "mcp_servers", name, content_toml).await
}
/// MCP 卸载:移除 config.toml [mcp_servers.<name>] 段。
pub async fn disable_mcp_config(codex_home: &Path, name: &str) -> Result<(), AppError> {
    let cfg = codex_home.join("config.toml");
    let section = format!("mcp_servers.{name}");
    crate::services::extensions::config_merge::remove_section(&cfg, &section).await
}
```
单测：
```rust
#[tokio::test]
async fn mcp_config_enable_disable_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    enable_mcp_config(tmp.path(), "mysrv", "command = \"node\"\nargs = [\"s.js\"]\n").await.unwrap();
    let s = tokio::fs::read_to_string(tmp.path().join("config.toml")).await.unwrap();
    assert!(s.contains("command"));
    disable_mcp_config(tmp.path(), "mysrv").await.unwrap();
    let s2 = tokio::fs::read_to_string(tmp.path().join("config.toml")).await.unwrap();
    assert!(!s2.contains("mysrv"));
}
```

- [ ] **Step 2: 运行验证通过**

Run: `cd backend-rs && cargo test --lib services::extensions::apply`。Expected: 全 PASS。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/services/extensions/apply.rs
git commit -m "feat(extensions): apply 加 MCP 启用/卸载配置段"
```

---

### Task 3: store `ExtRecord` 加 `config_text`

**Files:**
- Modify: `backend-rs/src/services/extensions/store.rs`

- [ ] **Step 1: 加字段 + 全链路**

`ExtRecord` 加 `pub config_text: Option<String>`。`From<ExtModel>` 映射 `config_text: m.config_text`。`upsert_extension` 的 ActiveModel 加 `config_text: Set(rec.config_text.clone())`（当前若硬编码 `Set(None)`，改为 `Set(rec.config_text.clone())`）。`list_enabled`/get 返回含 config_text。所有构造 `ExtRecord` 处（skill upload `config_text:None`、plugin upload `config_text:None`）补 `None`。

> 核对：`cluster_extensions` 表已有 `config_text TEXT` 列（skill plan Task 1 建的）；entity `cluster_extension::Model` 已有 `config_text: Option<String>` 字段。本 task 只补 ExtRecord 链路。

- [ ] **Step 2: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/services/extensions/store.rs backend-rs/src/api/multitenant/extensions.rs
git commit -m "feat(extensions): ExtRecord 加 config_text 全链路(MCP 用)"
```

---

### Task 4: upload MCP API + delete MCP 分支

**Files:**
- Modify: `backend-rs/src/api/multitenant/extensions.rs`

- [ ] **Step 1: UploadBody 加 config_text + upload mcp 分支**

`UploadBody` 加 `#[serde(default)] pub config_text: Option<String>`。

`upload_extension` 开头加：`if body.kind == "mcp" { return upload_mcp(state, body).await; }`。

```rust
async fn upload_mcp(state: AppState, body: UploadBody) -> Result<Json<ExtResp>, AppError> {
    let content = body.config_text.clone().ok_or_else(|| AppError::business(
        ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST, "MCP 需要 config_text".into(), None))?;
    // 1. 校验 content 是合法 toml 段内容(包头 parse 验证)
    let wrapped = format!("[mcp_servers.{}]\n{}\n", body.name, content);
    wrapped.parse::<toml_edit::DocumentMut>().map_err(|e| AppError::business(
        ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST, format!("config_text 非法 toml: {e}"), None))?;
    // 2. content_hash = content 的 sha256
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new(); h.update(content.as_bytes());
    let content_hash = hex::encode(h.finalize());
    // 3. 合并到本节点 config
    crate::services::extensions::apply::enable_mcp_config(&state.codex_home, &body.name, &content).await?;
    // 4. 入库(kind=mcp, content_form=config, config_text)
    let id = crate::services::extensions::store::find_id_by_kind_name(&state.db, "mcp", &body.name).await?
        .unwrap_or_else(crate::services::multitenant::new_id);
    let rec = crate::services::extensions::store::ExtRecord {
        id: id.clone(), kind: "mcp".into(), name: body.name.clone(),
        content_form: "config".into(), content_hash: content_hash.clone(), enabled: true,
        marketplace: None, version: None, config_text: Some(content.clone()),
    };
    crate::services::extensions::store::upsert_extension(&state.db, &rec, &[]).await?; // files 空
    // 5. 本地 state + 发事件
    let mut st = crate::services::extensions::apply::load_local_state(&state.codex_home).await;
    st.insert(id.clone(), crate::services::extensions::apply::LocalExtEntry {
        name: body.name.clone(), hash: content_hash.clone(), kind: "mcp".into(), market: None, version: None,
    });
    let _ = crate::services::extensions::apply::save_local_state(&state.codex_home, &st).await;
    if let Some(bus) = &state.mt_event_bus { let _ = bus.publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}")).await; }
    metrics::counter!("mt_extension_upload_total").increment(1);
    Ok(Json(ExtResp { id, name: body.name, content_hash }))
}
```

- [ ] **Step 2: delete_extension 加 mcp 分支**

`delete_extension` handler 查到 `m` 后按 kind 分发（已有 skill/plugin），加 mcp：
```rust
"mcp" => {
    let _ = crate::services::extensions::apply::disable_mcp_config(&state.codex_home, &m.name).await;
}
```

- [ ] **Step 3: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/api/multitenant/extensions.rs
git commit -m "feat(extensions): upload/delete MCP(配置段)"
```

---

### Task 5: sync MCP 分支

**Files:**
- Modify: `backend-rs/src/services/extensions/sync.rs`

- [ ] **Step 1: sync_one_extension 加 mcp 分支**

当前 sync_one 处理 skill/plugin（文件落盘）。MCP 无文件，单独分支：
```rust
// 在 sync_one_extension 顶部,need 判断之后:
if rec.kind == "mcp" {
    let content = rec.config_text.clone().unwrap_or_default();
    crate::services::extensions::apply::enable_mcp_config(&state.codex_home, &rec.name, &content).await?;
    local.insert(rec.id.clone(), LocalExtEntry {
        name: rec.name.clone(), hash: rec.content_hash.clone(),
        kind: "mcp".into(), market: None, version: None,
    });
    crate::services::extensions::store::add_holder(&state.db, &rec.id, &state.node_id).await?;
    return Ok(());
}
// 原 skill/plugin 文件逻辑...
```
> 注：MCP 的"hash 校验"用 content_hash（PG 存的，upload 时算的 content sha256）；sync 侧直接信任 PG content_text + content_hash（不像 skill/plugin 下载后重算——MCP 无文件传输，config_text 直接从 PG 读，完整性由 PG 保证）。若要更严可对 content 重算 sha256 比对 rec.content_hash，但 PG 是权威源，非必须。

- [ ] **Step 2: cleanup_local_extension 加 mcp 分支**

stale 删除（本地有 PG 无）加 mcp：
```rust
"mcp" => {
    let _ = crate::services::extensions::apply::disable_mcp_config(codex_home, &e.name).await;
}
```

- [ ] **Step 3: 编译验证**

Run: `cd backend-rs && cargo check`。Expected: 通过。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/services/extensions/sync.rs
git commit -m "feat(extensions): sync/stale 支持 MCP(配置段)"
```

---

### Task 6: 端到端验证 MCP

**Files:** 无新代码；验证步骤

- [ ] **Step 1: 起环境**

参照 skill/plugin 验证（停旧节点 → cargo build → 起 node-a/b/c，enable=true）。确认迁移 applied。

- [ ] **Step 2: 上传 MCP 到 node-a**

```bash
curl -X POST http://127.0.0.1:8182/api/mt/extensions \
  -H 'Content-Type: application/json' -H "Authorization: Bearer <admin_token>" \
  -d '{"kind":"mcp","name":"e2e-mcp","config_text":"command = \"echo\"\nargs = [\"hi\"]\n"}'
```
Expected: 返回 id + hash；node-a `config.toml` 有 `[mcp_servers."e2e-mcp"]` 段（command/args）；PG 一行（kind=mcp）+ holders node-a。

- [ ] **Step 3: 验证同步**

等 ~10s，node-b/c 的 `config.toml` 也有 `[mcp_servers."e2e-mcp"]` 段（内容一致）+ holders 含 b/c。

- [ ] **Step 4: codex 识别**

`CODEX_HOME=<node-b> codex mcp list`，确认 `e2e-mcp` 出现。

- [ ] **Step 5: DELETE 验证传播**

curl DELETE，等同步，验证三节点 config.toml 的 `[mcp_servers."e2e-mcp"]` 段移除 + PG 行删 + state 清。

- [ ] **Step 6: Commit 验证记录**

```bash
git commit --allow-empty -m "test(extensions): MCP 三节点端到端验证通过"
```

---

## 验收标准（MCP 计划完成定义）
- `merge_full_section` 单测通过（多字段段合并 + 幂等更新 + 保留原段）。
- upload MCP → 本节点 config 段 + 入库（config_text）+ 发事件。
- 其他节点 ≤30s 同步 config 段，`codex mcp list` 识别。
- DELETE 传播：config 段全清。
- 全程不碰 auth.json/*.sqlite/sessions。

## 待实现时验证/探查的点
1. `config_text` 的 toml 合并对嵌套值（`env = { ... }`）的处理（toml_edit Item clone 递归，实测确认）。
2. `codex mcp list` 的实际输出格式（确认 e2e-mcp 出现）。
3. MCP 段名 `[mcp_servers.name]`：name 是裸 key 还是 quoted（toml_edit 自动，实测）。

## 完成后
skill + plugin + MCP 三件套齐全，集群扩展分发功能完整。

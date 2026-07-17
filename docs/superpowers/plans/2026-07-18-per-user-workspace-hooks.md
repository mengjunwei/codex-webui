# Per-User Workspace + Codex Hook Webhook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 注册时自动创建个人 + 所属 team 的 workspace;codex 调用 tool/skill/plugin/mcp 时通过 `POST /hooks/codex` 单端点 webhook 回调 backend 做权限校验与审计。

**Architecture:** 全局共享 `codex_home` 下挂 `users/{uid}/personal/` + `teams/{tid}/{shared,members/{uid}}/`;新增 axum 路由 `POST /hooks/codex`(独立 `INTERNAL_HOOK_TOKEN` 鉴权),按 `team_id+user_id` 查 `workspace_role` 决策,异步批量写 `workspace_audit`;`spawn_slot` 启动 codex 前向 `$CODEX_HOME/config.toml` 注入 hooks 配置并加 `--dangerously-bypass-hook-trust`。

**Tech Stack:** Rust + axum 0.7 + SeaORM + tokio;沿用现有 `mt_team_codex` per-team 进程池模型与 Redis event_bus;hook 走 HTTP(SSE 不在本期)。

## Global Constraints

- codex CLI 版本: `codex-cli 0.142.5`(用户机器实测,`/c/Users/Administrator/AppData/Local/Programs/OpenAI/Codex/bin/codex.exe`)
- 不破坏现有 per-team 进程池、全局 CODEX_HOME 模型、Redis event_bus、多副本 HA 复制链路
- 所有新增 env 启动必填,与 `INTERNAL_RPC_TOKEN` 同模式(≥32 字节,缺失/过短即启动失败)
- 失败语义:hook 内部异常 → fail-open(`continue: true`),**不阻断 codex**
- 审计批量入库:批大小 50 / 刷新间隔 1s,丢队列最多 1024 条
- 文件权限:POSIX `0o775`(Linux/macOS);Windows 不做 ACL,依赖 codex sandbox
- 全程中文注释 / 提交信息中文;代码风格沿用现有 `services/multitenant/*` 范式

## File Structure

新增:
- `backend-rs/src/services/workspace/mod.rs` — workspace 目录创建 + role 查询
- `backend-rs/src/services/workspace/audit_writer.rs` — 异步批量 audit 落库
- `backend-rs/src/services/workspace/decision.rs` — PreToolUse 决策表
- `backend-rs/src/api/hooks.rs` — `POST /hooks/codex` 路由
- `backend-rs/src/db/migration/m20260718_000001_workspace.rs` — `workspace_role` + `workspace_audit` 表
- `backend-rs/src/config.rs` — 新增 `INTERNAL_HOOK_TOKEN` 校验
- `backend-rs/src/state.rs` — 新增 `hook_token` + `audit_writer` 字段
- `backend-rs/src/services/multitenant/codex_pool.rs` — `spawn_slot` 注入 config.toml
- `backend-rs/src/api/multitenant/handlers.rs` — 注册/team/member 三处触发 `workspace::*`
- `backend-rs/src/main.rs` — 启动时构造 audit_writer、挂载 hooks 路由
- `backend-rs/src/api/mod.rs` — 导出 hooks 模块
- `backend-rs/tests/hooks_webhook.rs` — 集成测试
- `backend-rs/tests/workspace_test.rs` — workspace 单元测试

---

## Task 1: 数据库迁移 — workspace_role + workspace_audit

**Files:**
- Create: `backend-rs/src/db/migration/m20260718_000001_workspace.rs`
- Modify: `backend-rs/src/db/migration/mod.rs`(末尾加 `mod m20260718_000001_workspace;` + `vec![Box::new(m20260718_000001_workspace::Migration)]`)
- Test: 现有 `cargo test --features db` 跑迁移不报错

**Interfaces:**
- 产出: `workspace_role(team_id TEXT, user_id TEXT, role TEXT, created_at TIMESTAMPTZ)` 主键 `(team_id, user_id)`,role 限定 `owner|admin|member`
- 产出: `workspace_audit(id BIGSERIAL, team_id TEXT NULL, user_id TEXT NULL, thread_id TEXT NULL, event_type TEXT, tool_name TEXT NULL, payload_json JSONB, decision TEXT NULL, ts TIMESTAMPTZ)`,索引 `(team_id, user_id, ts DESC)`

- [ ] **Step 1: 创建迁移文件**

文件 `backend-rs/src/db/migration/m20260718_000001_workspace.rs`:

```rust
//! workspace 角色 + hook 审计表(per-user workspace 实施步骤 1)。

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.create_table(
            Table::create()
                .table(WorkspaceRole::Table)
                .if_not_exists()
                .col(string(WorkspaceRole::TeamId).not_null())
                .col(string(WorkspaceRole::UserId).not_null())
                .col(string(WorkspaceRole::Role).not_null())
                .col(timestamp_with_time_zone(WorkspaceRole::CreatedAt).not_null().default(Expr::current_timestamp()))
                .primary_key(Index::create().col(WorkspaceRole::TeamId).col(WorkspaceRole::UserId))
                .check_constraint(
                    Expr::cust("role IN ('owner','admin','member')")
                )
                .to_owned(),
        ).await?;

        manager.create_table(
            Table::create()
                .table(WorkspaceAudit::Table)
                .if_not_exists()
                .col(pk_auto(WorkspaceAudit::Id))
                .col(string_null(WorkspaceAudit::TeamId))
                .col(string_null(WorkspaceAudit::UserId))
                .col(string_null(WorkspaceAudit::ThreadId))
                .col(string(WorkspaceAudit::EventType).not_null())
                .col(string_null(WorkspaceAudit::ToolName))
                .col(json(WorkspaceAudit::PayloadJson).not_null())
                .col(string_null(WorkspaceAudit::Decision))
                .col(timestamp_with_time_zone(WorkspaceAudit::Ts).not_null().default(Expr::current_timestamp()))
                .to_owned(),
        ).await?;

        manager.create_index(
            Index::create()
                .name("workspace_audit_team_user_ts")
                .table(WorkspaceAudit::Table)
                .col(WorkspaceAudit::TeamId)
                .col(WorkspaceAudit::UserId)
                .col(WorkspaceAudit::Ts)
                .if_not_exists()
                .to_owned(),
        ).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.drop_index(Index::drop().name("workspace_audit_team_user_ts").table(WorkspaceAudit::Table).to_owned()).await?;
        manager.drop_table(Table::drop().table(WorkspaceAudit::Table).to_owned()).await?;
        manager.drop_table(Table::drop().table(WorkspaceRole::Table).to_owned()).await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
pub enum WorkspaceRole {
    Table,
    TeamId,
    UserId,
    Role,
    CreatedAt,
}

#[derive(DeriveIden)]
pub enum WorkspaceAudit {
    Table,
    Id,
    TeamId,
    UserId,
    ThreadId,
    EventType,
    ToolName,
    PayloadJson,
    Decision,
    Ts,
}
```

- [ ] **Step 2: 挂载到迁移列表**

在 `backend-rs/src/db/migration/mod.rs` 末尾追加:

```rust
mod m20260718_000001_workspace;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            // ... 现有 8 个迁移保持不变 ...
            Box::new(m20260718_000001_workspace::Migration),
        ]
    }
}
```

具体追加位置按文件实际结构,只追加一行 `mod` 声明 + 一行 `Box::new(...)`,不要重排现有项。

- [ ] **Step 3: 编译并跑迁移**

```bash
cd backend-rs && cargo build
DATABASE_URL=sqlite::memory: cargo test --lib migration_workspace_creates
```

期望:编译通过。如无 `migration_workspace_creates` 测试,先创建一个:

```rust
#[tokio::test]
async fn migration_workspace_creates() {
    use sea_orm::{Database, DbBackend};
    use sea_orm_migration::MigratorTrait;
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::migration::Migrator::up(&db, None).await.unwrap();
    // 两次 up 幂等
    crate::db::migration::Migrator::up(&db, None).await.unwrap();
}
```

放到 `backend-rs/src/db/migration/mod.rs` 同文件的 `#[cfg(test)] mod tests` 块里(若不存在则新建)。

- [ ] **Step 4: 提交**

```bash
git add backend-rs/src/db/migration/m20260718_000001_workspace.rs backend-rs/src/db/migration/mod.rs
git commit -m "feat(multitenant): workspace_role/workspace_audit 数据库迁移

- workspace_role(team_id,user_id,role) 主键 + role 限定
- workspace_audit id BIGSERIAL + JSONB payload + (team_id,user_id,ts) 索引
- 迁移幂等:create_table if_not_exists

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Workspace 模块 — 目录创建 + 角色查询

**Files:**
- Create: `backend-rs/src/services/workspace/mod.rs`
- Create: `backend-rs/src/services/workspace/audit_writer.rs`(占位,真实实现在 Task 5)
- Test: `backend-rs/tests/workspace_test.rs`

**Interfaces:**
- `pub async fn ensure_user_personal(state: &AppState, user_id: &str) -> Result<(), AppError>`
- `pub async fn ensure_team_shared(state: &AppState, team_id: &str) -> Result<(), AppError>`
- `pub async fn ensure_team_member_view(state: &AppState, team_id: &str, user_id: &str) -> Result<(), AppError>`
- `pub async fn upsert_role(db: &DatabaseConnection, team_id: &str, user_id: &str, role: &str) -> Result<(), AppError>`
- `pub async fn delete_role(db: &DatabaseConnection, team_id: &str, user_id: &str) -> Result<(), AppError>`
- `pub async fn get_role(db: &DatabaseConnection, team_id: &str, user_id: &str) -> Result<Option<String>, AppError>`

- [ ] **Step 1: 创建 mod.rs 框架**

文件 `backend-rs/src/services/workspace/mod.rs`:

```rust
//! workspace 物理目录创建 + role CRUD(per-user workspace 实施步骤 2)。
//!
//! 物理布局(均在 `state.codex_home` 下):
//! - `users/{user_id}/personal/`         个人 workspace(永久可写)
//! - `teams/{team_id}/shared/`            team 共享 workspace(owner/admin 可写)
//! - `teams/{team_id}/members/{user_id}/` 该成员视图目录

pub mod audit_writer;

use crate::error::AppError;
use crate::state::AppState;
use sea_orm::DatabaseConnection;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use std::path::PathBuf;

const PERSONAL_DIR: &str = "users";
const TEAMS_DIR: &str = "teams";
const SHARED_SUBDIR: &str = "shared";
const MEMBERS_SUBDIR: &str = "members";

/// 个人 workspace 绝对路径。
pub fn personal_path(codex_home: &std::path::Path, user_id: &str) -> PathBuf {
    codex_home.join(PERSONAL_DIR).join(user_id).join("personal")
}

/// team 共享 workspace 绝对路径。
pub fn team_shared_path(codex_home: &std::path::Path, team_id: &str) -> PathBuf {
    codex_home.join(TEAMS_DIR).join(team_id).join(SHARED_SUBDIR)
}

/// team 成员视图绝对路径。
pub fn team_member_path(codex_home: &std::path::Path, team_id: &str, user_id: &str) -> PathBuf {
    codex_home.join(TEAMS_DIR).join(team_id).join(MEMBERS_SUBDIR).join(user_id)
}

#[cfg(unix)]
fn shared_permissions() -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(0o775)
}

/// 确保个人 workspace 存在。
pub async fn ensure_user_personal(state: &AppState, user_id: &str) -> Result<(), AppError> {
    let path = personal_path(&state.codex_home, user_id);
    tokio::fs::create_dir_all(&path).await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(())
}

/// 确保 team 共享 workspace 存在。
pub async fn ensure_team_shared(state: &AppState, team_id: &str) -> Result<(), AppError> {
    let path = team_shared_path(&state.codex_home, team_id);
    tokio::fs::create_dir_all(&path).await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        let _ = tokio::fs::set_permissions(&path, shared_permissions()).await;
    }
    Ok(())
}

/// 确保 team 成员视图目录存在。
pub async fn ensure_team_member_view(state: &AppState, team_id: &str, user_id: &str) -> Result<(), AppError> {
    let path = team_member_path(&state.codex_home, team_id, user_id);
    tokio::fs::create_dir_all(&path).await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(())
}

/// SeaORM workspace_role 实体定义(同文件,便于单测引用)。
pub mod role_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "workspace_role")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub team_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        pub role: String,
        pub created_at: chrono::DateTime<chrono::Utc>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub async fn upsert_role(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
    role: &str,
) -> Result<(), AppError> {
    use role_entity::{ActiveModel, Entity};
    let now = chrono::Utc::now();
    let am = ActiveModel {
        team_id: Set(team_id.to_string()),
        user_id: Set(user_id.to_string()),
        role: Set(role.to_string()),
        created_at: Set(now),
    };
    // SeaORM upsert: insert ignore on PK conflict
    let stmt = sea_orm::sea_query::EntityStatement::insert(am)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([role_entity::Column::TeamId, role_entity::Column::UserId])
                .do_nothing()
                .to_owned(),
        )
        .to_owned();
    Entity::insert(am).on_conflict(
        sea_orm::sea_query::OnConflict::columns([role_entity::Column::TeamId, role_entity::Column::UserId])
            .do_nothing()
            .to_owned(),
    ).exec(db).await.map_err(|e| AppError::internal(format!("upsert role: {e}")))?;
    Ok(())
}

pub async fn delete_role(db: &DatabaseConnection, team_id: &str, user_id: &str) -> Result<(), AppError> {
    use role_entity::{Column, Entity};
    Entity::delete_many()
        .filter(Column::TeamId.eq(team_id))
        .filter(Column::UserId.eq(user_id))
        .exec(db).await.map_err(|e| AppError::internal(format!("delete role: {e}")))?;
    Ok(())
}

pub async fn get_role(db: &DatabaseConnection, team_id: &str, user_id: &str) -> Result<Option<String>, AppError> {
    use role_entity::{Column, Entity};
    let row = Entity::find()
        .filter(Column::TeamId.eq(team_id))
        .filter(Column::UserId.eq(user_id))
        .one(db).await.map_err(|e| AppError::internal(format!("get role: {e}")))?;
    Ok(row.map(|m| m.role))
}
```

(注:上面 `Entity::insert` 重复了 `sea_query::EntityStatement::insert`,实际写时删掉冗余,只保留 `Entity::insert(am).on_conflict(...).exec(db)` 那一行。)

- [ ] **Step 2: 创建 audit_writer 占位**

文件 `backend-rs/src/services/workspace/audit_writer.rs`:

```rust
//! hook 审计批量入库(per-user workspace 实施步骤 5 真实实现,本任务占位)。
use crate::error::AppError;
use sea_orm::DatabaseConnection;
use serde_json::Value;
use tokio::sync::mpsc;

/// audit 写入器:后台 task 批量刷库。
#[derive(Clone)]
pub struct AuditWriter {
    tx: mpsc::Sender<AuditEvent>,
}

pub struct AuditEvent {
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub thread_id: Option<String>,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub payload: Value,
    pub decision: Option<String>,
}

impl AuditWriter {
    /// 入队;队列满则丢弃(tracing::warn),不阻塞 caller。
    pub fn submit(&self, ev: AuditEvent) {
        if let Err(e) = self.tx.try_send(ev) {
            tracing::warn!(error = %e, "audit queue full; dropping event");
        }
    }
}

/// 启动后台 task:批大小 50,刷新间隔 1s。
pub fn spawn(db: DatabaseConnection) -> AuditWriter {
    let (tx, mut rx) = mpsc::channel::<AuditEvent>(1024);
    tokio::spawn(async move {
        let mut buf: Vec<AuditEvent> = Vec::with_capacity(64);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(ev) = rx.recv() => {
                    buf.push(ev);
                    if buf.len() >= 50 {
                        flush(&db, &mut buf).await;
                    }
                }
                _ = tick.tick() => {
                    if !buf.is_empty() {
                        flush(&db, &mut buf).await;
                    }
                }
                else => break,
            }
        }
    });
    AuditWriter { tx }
}

async fn flush(db: &DatabaseConnection, buf: &mut Vec<AuditEvent>) {
    if buf.is_empty() { return; }
    let drained: Vec<AuditEvent> = buf.drain(..).collect();
    // TODO(task5):用 INSERT ... VALUES (...),(...),... 批量写 workspace_audit
    // 当前 task 占位,直接丢弃;task5 实现 SeaORM batch insert。
    tracing::debug!(count = drained.len(), "audit flush (stub, task5 implements)");
}
```

- [ ] **Step 3: 注册模块**

在 `backend-rs/src/services/mod.rs`(若不存在则新增)中追加 `pub mod workspace;`;若该文件已存在则在合适位置插入,沿用现有 alphabetical/分组顺序。

- [ ] **Step 4: 写测试**

文件 `backend-rs/tests/workspace_test.rs`:

```rust
//! workspace 目录创建 + role CRUD 测试。

use codex_webui::services::workspace as ws;
use sea_orm::Database;
use std::path::PathBuf;

#[tokio::test]
async fn path_helpers_match_spec() {
    let home = PathBuf::from("/tmp/codex-home-test");
    assert_eq!(
        ws::personal_path(&home, "u1"),
        PathBuf::from("/tmp/codex-home-test/users/u1/personal")
    );
    assert_eq!(
        ws::team_shared_path(&home, "t1"),
        PathBuf::from("/tmp/codex-home-test/teams/t1/shared")
    );
    assert_eq!(
        ws::team_member_path(&home, "t1", "u1"),
        PathBuf::from("/tmp/codex-home-test/teams/t1/members/u1")
    );
}

#[tokio::test]
async fn ensure_dirs_is_idempotent() {
    let tmp = std::env::temp_dir().join(format!("codex-ws-test-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp).await.unwrap();
    // 直接调底层 tokio,绕开 AppState(测试不依赖 DB)
    tokio::fs::create_dir_all(ws::personal_path(&tmp, "u1")).await.unwrap();
    tokio::fs::create_dir_all(ws::personal_path(&tmp, "u1")).await.unwrap();
    assert!(tokio::fs::metadata(ws::personal_path(&tmp, "u1")).await.unwrap().is_dir());

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn role_upsert_and_get() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    codex_webui::db::migration::Migrator::up(&db, None).await.unwrap();

    ws::upsert_role(&db, "t1", "u1", "member").await.unwrap();
    assert_eq!(ws::get_role(&db, "t1", "u1").await.unwrap(), Some("member".into()));

    // 重复 upsert 幂等
    ws::upsert_role(&db, "t1", "u1", "admin").await.unwrap();
    // 二次写因 do_nothing,仍为 member
    assert_eq!(ws::get_role(&db, "t1", "u1").await.unwrap(), Some("member".into()));

    ws::delete_role(&db, "t1", "u1").await.unwrap();
    assert_eq!(ws::get_role(&db, "t1", "u1").await.unwrap(), None);
}
```

- [ ] **Step 5: 跑测试**

```bash
cd backend-rs && cargo test --test workspace_test -- --nocapture
```

期望:3 个测试全过。如失败先修代码,不要改测试期望。

- [ ] **Step 6: 提交**

```bash
git add backend-rs/src/services/workspace/mod.rs backend-rs/src/services/workspace/audit_writer.rs backend-rs/tests/workspace_test.rs
git commit -m "feat(multitenant): workspace 目录创建 + role CRUD

- users/{uid}/personal + teams/{tid}/{shared,members/{uid}} 路径助手
- ensure_* 三个 mkdir 幂等入口
- workspace_role upsert/get/delete(SeaORM,SQLite + PG 兼容)
- audit_writer 占位(task5 实现批量入库)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: 注册触发个人 workspace

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs` — `register` handler 末尾追加 `workspace::ensure_user_personal(&state, &user_id)`

**Interfaces:**
- Consumes: `state: AppState`, `user_id: String`
- Produces: 创建 `users/{user_id}/personal/`

- [ ] **Step 1: 定位 register handler**

```bash
grep -n "async fn register" backend-rs/src/api/multitenant/handlers.rs
```

确认返回 `Result<Json<...>, AppError>` 的 async fn。

- [ ] **Step 2: 在末尾追加调用**

在 `register` 函数返回 `Ok(...)` 之前(构造完 `user_id` 之后),插入:

```rust
// 注册即创建个人 workspace。
if let Err(e) = codex_webui::services::workspace::ensure_user_personal(&state, &user_id).await {
    tracing::warn!(error = %e, user_id = %user_id, "ensure_user_personal failed (non-fatal)");
}
```

**注意**:注册失败必须返回 200(workspace 创建失败不应阻断用户),仅记 warn。

- [ ] **Step 3: 编译**

```bash
cd backend-rs && cargo build
```

期望:0 warning,0 error。

- [ ] **Step 4: 提交**

```bash
git add backend-rs/src/api/multitenant/handlers.rs
git commit -m "feat(multitenant): 注册触发个人 workspace 创建

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Team 创建 / 加入触发 team workspace

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs`
  - `create_team` 末尾追加 `workspace::ensure_team_shared(&state, &team_id)`
  - `join_team` / `add_member` 末尾追加 `workspace::ensure_team_member_view + workspace::upsert_role('member')`
  - `remove_member` 末尾追加 `workspace::delete_role`

**Interfaces:**
- Consumes: `state: AppState`, `team_id: String`, `user_id: String`
- Produces: `teams/{team_id}/shared/`、`teams/{team_id}/members/{user_id}/`、`workspace_role` 行

- [ ] **Step 1: 定位三个 handler**

```bash
grep -n "async fn \(create_team\|join_team\|remove_member\|add_member\)" backend-rs/src/api/multitenant/handlers.rs
```

- [ ] **Step 2: create_team 追加**

```rust
if let Err(e) = codex_webui::services::workspace::ensure_team_shared(&state, &team_id).await {
    tracing::warn!(error = %e, team_id = %team_id, "ensure_team_shared failed (non-fatal)");
}
```

- [ ] **Step 3: join_team / add_member 追加**

```rust
if let Err(e) = codex_webui::services::workspace::ensure_team_member_view(&state, &team_id, &user_id).await {
    tracing::warn!(error = %e, "ensure_team_member_view failed (non-fatal)");
}
if let Err(e) = codex_webui::services::workspace::upsert_role(&state.db, &team_id, &user_id, "member").await {
    tracing::warn!(error = %e, "upsert_role failed (non-fatal)");
}
```

- [ ] **Step 4: remove_member 追加**

```rust
let _ = codex_webui::services::workspace::delete_role(&state.db, &team_id, &user_id).await;
```

(成员视图目录保留,便于审计查询。)

- [ ] **Step 5: 编译**

```bash
cd backend-rs && cargo build
```

- [ ] **Step 6: 提交**

```bash
git add backend-rs/src/api/multitenant/handlers.rs
git commit -m "feat(multitenant): team/join/remove 触发 workspace 创建与 role CRUD

- create_team -> ensure_team_shared
- join_team -> ensure_team_member_view + upsert_role('member')
- remove_member -> delete_role(目录保留)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: AuditWriter 批量入库真实实现

**Files:**
- Modify: `backend-rs/src/services/workspace/audit_writer.rs`(替换 `flush` 函数)

**Interfaces:**
- Consumes: `Vec<AuditEvent>`, `DatabaseConnection`
- Produces: 批量 INSERT 到 `workspace_audit`;失败 → tracing::error,不抛

- [ ] **Step 1: 引入 SeaORM 依赖**

确认 `backend-rs/Cargo.toml` 已有 `sea_orm = { version = "...", features = ["sqlx-sqlite", "sqlx-postgres"] }`(沿用现有)。如无 `json` feature,加 `"with-json"`。

- [ ] **Step 2: 在 audit_writer.rs 顶部新增实体**

```rust
mod audit_entity {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "workspace_audit")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub team_id: Option<String>,
        pub user_id: Option<String>,
        pub thread_id: Option<String>,
        pub event_type: String,
        pub tool_name: Option<String>,
        pub payload_json: serde_json::Value,
        pub decision: Option<String>,
        pub ts: chrono::DateTime<chrono::Utc>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
```

- [ ] **Step 3: 替换 flush 函数**

```rust
async fn flush(db: &DatabaseConnection, buf: &mut Vec<AuditEvent>) {
    use audit_entity::{ActiveModel, Entity};
    use sea_orm::{ActiveModelTrait, Set};
    if buf.is_empty() { return; }
    let drained: Vec<AuditEvent> = buf.drain(..).collect();
    for ev in drained {
        let am = ActiveModel {
            id: Default::default(),
            team_id: Set(ev.team_id),
            user_id: Set(ev.user_id),
            thread_id: Set(ev.thread_id),
            event_type: Set(ev.event_type),
            tool_name: Set(ev.tool_name),
            payload_json: Set(ev.payload),
            decision: Set(ev.decision),
            ts: Set(chrono::Utc::now()),
        };
        if let Err(e) = am.insert(db).await {
            tracing::error!(error = %e, "audit insert failed (dropped)");
        }
    }
}
```

(本期用逐行 insert;后续 task 可优化为真批量 INSERT ... VALUES (...),(...)。)

- [ ] **Step 4: 编译**

```bash
cd backend-rs && cargo build
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/services/workspace/audit_writer.rs
git commit -m "feat(multitenant): AuditWriter 真实入库 workspace_audit

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Config 新增 INTERNAL_HOOK_TOKEN

**Files:**
- Modify: `backend-rs/src/config.rs`
  - 字段 `pub internal_hook_token: String`
  - `from_env` 校验
  - `env_list` 追加 `"INTERNAL_HOOK_TOKEN"`
  - 测试 `set_required_env` 加一行

**Interfaces:**
- Consumes: env `INTERNAL_HOOK_TOKEN`
- Produces: `Config.internal_hook_token: String`(缺失/过短 → anyhow Err)

- [ ] **Step 1: 在 Config 结构体加字段**

紧随 `internal_token: String` 后追加:

```rust
/// hook webhook 鉴权 token(≥32 字节,启动必填)。
pub internal_hook_token: String,
```

- [ ] **Step 2: 在 from_env 加校验**

紧随 `internal_token` 校验后追加:

```rust
let internal_hook_token = env::var("INTERNAL_HOOK_TOKEN")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| anyhow!("INTERNAL_HOOK_TOKEN is required (≥32 bytes)"))?;
if internal_hook_token.len() < 32 {
    return Err(anyhow!(
        "INTERNAL_HOOK_TOKEN must be ≥32 bytes (current: {}); generate with `openssl rand -hex 32`",
        internal_hook_token.len()
    ));
}
```

- [ ] **Step 3: 构造处追加**

在 `Config { ... }` 字面量里加 `internal_hook_token,`。

- [ ] **Step 4: env_list 追加**

```rust
"INTERNAL_HOOK_TOKEN",
```

- [ ] **Step 5: 测试 set_required_env 加一行**

```rust
unsafe { env::set_var("INTERNAL_HOOK_TOKEN", "0123456789abcdef0123456789abcdef"); }
```

- [ ] **Step 6: 跑 config 测试**

```bash
cd backend-rs && cargo test --lib config
```

期望:全过;新加一个 `internal_hook_token_too_short_is_error` 测试(沿用 `internal_token_too_short_is_error` 模板):

```rust
#[test]
fn internal_hook_token_too_short_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::set_var("INTERNAL_HOOK_TOKEN", "short"); }
    assert!(Config::from_env().is_err());
}
```

- [ ] **Step 7: 提交**

```bash
git add backend-rs/src/config.rs
git commit -m "feat(multitenant): INTERNAL_HOOK_TOKEN 启动校验(≥32 字节)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: AppState 新增 hook_token + audit_writer

**Files:**
- Modify: `backend-rs/src/state.rs`
- Modify: `backend-rs/src/main.rs`(构造 AuditWriter,塞进 AppState)

**Interfaces:**
- `AppState.hook_token: String`
- `AppState.audit_writer: AuditWriter`

- [ ] **Step 1: state.rs 引入并新增字段**

```rust
use crate::services::workspace::audit_writer::AuditWriter;

// 在 struct 内 public 字段末尾追加:
pub hook_token: String,
pub audit_writer: AuditWriter,
```

- [ ] **Step 2: main.rs 在构造 AppState 前**

紧随 `cfg.internal_token.clone()` 后追加:

```rust
let audit_writer = codex_webui::services::workspace::audit_writer::spawn(db.clone());
```

- [ ] **Step 3: 构造 AppState 字面量追加**

```rust
hook_token: cfg.internal_hook_token.clone(),
audit_writer: audit_writer.clone(),
```

- [ ] **Step 4: 编译**

```bash
cd backend-rs && cargo build
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/state.rs backend-rs/src/main.rs
git commit -m "feat(multitenant): AppState 注入 hook_token + audit_writer

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Decision 模块 — PreToolUse 权限决策

**Files:**
- Create: `backend-rs/src/services/workspace/decision.rs`
- Modify: `backend-rs/src/services/workspace/mod.rs`(添加 `pub mod decision;`)

**Interfaces:**
- `pub fn decide_pre_tool_use(role: &str, tool_name: &str, path: &Path, codex_home: &Path) -> Decision`
- `pub enum Decision { Allow, Deny, Ask, AskWithUpdatedInput(serde_json::Value) }`
- `pub fn target_path(tool_input: &Value) -> Option<PathBuf>`(从 tool_input 提取目标路径)

- [ ] **Step 1: 创建 decision.rs**

```rust
//! PreToolUse 决策表(per-user workspace 实施步骤 8)。
//!
//! 决策矩阵:
//! - shell/exec_command 越界(写出 CODEX_HOME 外) → Deny
//! - 写 teams/{tid}/shared 且 role==member        → Deny
//! - 写已知 workspace 外                          → Ask
//! - 其他                                          → Allow

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
    AskWithUpdatedInput(Value),
}

/// 从 tool_input 提取目标绝对路径(简化版:取 `file_path` / `cwd` / `command` 中第一个存在的)。
pub fn target_path(tool_input: &Value) -> Option<PathBuf> {
    for key in ["file_path", "path", "cwd"] {
        if let Some(s) = tool_input.get(key).and_then(Value::as_str) {
            return Some(PathBuf::from(s));
        }
    }
    None
}

/// 决策入口。
pub fn decide_pre_tool_use(
    role: &str,
    tool_name: &str,
    target: &Path,
    codex_home: &Path,
) -> Decision {
    // 1) 越界:写出 CODEX_HOME 外 → Deny
    if let Ok(canon_home) = codex_home.canonicalize() {
        let probe = if target.exists() {
            target.canonicalize().unwrap_or_else(|_| target.to_path_buf())
        } else {
            target.to_path_buf()
        };
        if !probe.starts_with(&canon_home) {
            return Decision::Deny;
        }
    }

    // 2) 写 team 共享盘,member → Deny
    let is_team_shared = target.starts_with(codex_home.join("teams"));
        && target
            .ancestors()
            .any(|a| a == codex_home.join("teams").join("SHARED_MARKER").as_path());
    // 注:实际判定用更严格的形式,见下面的 helpers。

    if role == "member" && is_writing_tool(tool_name) && target.starts_with(codex_home.join("teams")) {
        // 命中 teams/* 但不属于 members 子目录 → shared
        let members_dir = codex_home.join("teams").join("__any_team__").join("members");
        let target_str = target.to_string_lossy();
        if !target_str.contains("/members/") {
            return Decision::Deny;
        }
    }

    Decision::Allow
}

fn is_writing_tool(name: &str) -> bool {
    matches!(name, "write_file" | "apply_patch" | "edit_file" | "shell" | "exec_command")
}
```

(以上 helpers 中的"判断是否 shared"逻辑实施时按实际工具输入更严谨;若实施中发现字段不统一,改用 `tool_input.file_path` + `tool_input.command` 双通道解析。)

- [ ] **Step 2: mod.rs 暴露**

```rust
pub mod decision;
```

- [ ] **Step 3: 单元测试**

在 `decision.rs` 末尾 `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_writing_team_shared_denied() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("teams/t1/shared/foo.txt");
        let d = decide_pre_tool_use("member", "write_file", &target, &home);
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn owner_writing_team_shared_allowed() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("teams/t1/shared/foo.txt");
        let d = decide_pre_tool_use("owner", "write_file", &target, &home);
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn escape_outside_home_denied() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = PathBuf::from("/etc/passwd");
        let d = decide_pre_tool_use("owner", "write_file", &target, &home);
        assert_eq!(d, Decision::Deny);
    }
}
```

- [ ] **Step 4: 跑测试**

```bash
cd backend-rs && cargo test --lib services::workspace::decision
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/services/workspace/decision.rs backend-rs/src/services/workspace/mod.rs
git commit -m "feat(multitenant): PreToolUse 决策表(member 写 shared 拒绝/越界拒绝)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Hook Webhook 路由

**Files:**
- Create: `backend-rs/src/api/hooks.rs`
- Modify: `backend-rs/src/api/mod.rs` 加 `pub mod hooks;`
- Modify: `backend-rs/src/main.rs` 在 `build_router` 前/后挂载

**Interfaces:**
- `POST /hooks/codex`(独立路由,不挂 `/api`,不走 JWT 中间件)
- 请求体 `HookPayload { hook_event, session_id, cwd, tool_name, tool_input, tool_output, team_id, user_id, raw }`
- 响应 `{ continue: bool, hookSpecificOutput: { permissionDecision, updatedInput? } }`

- [ ] **Step 1: 创建 hooks.rs**

```rust
//! codex hook webhook(per-user workspace 实施步骤 9)。
//!
//! 路由:`POST /hooks/codex`,独立鉴权(X-Hook-Token == INTERNAL_HOOK_TOKEN)。
//! 失败语义:任何内部异常 → 200 + continue=true(fail-open)。

use crate::error::AppError;
use crate::services::workspace as ws;
use crate::services::workspace::decision::{decide_pre_tool_use, Decision};
use crate::state::AppState;
use axum::{
    extract::{Json, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct HookPayload {
    #[serde(rename = "hook_event")]
    pub hook_event: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<Value>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Serialize)]
pub struct HookResponse {
    #[serde(rename = "continue")]
    pub continue_: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Serialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: &'static str, // allow|deny|ask
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedInput")]
    pub updated_input: Option<Value>,
}

/// 路由入口。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> impl IntoResponse {
    // 1) 验签
    let token_ok = headers
        .get("x-hook-token")
        .and_then(|v| v.to_str().ok())
        .map(|t| constant_time_eq(t.as_bytes(), state.hook_token.as_bytes()))
        .unwrap_or(false);
    if !token_ok {
        return (StatusCode::UNAUTHORIZED, "invalid hook token").into_response();
    }

    // 2) fail-open:包一层,任何异常返回 continue=true
    let resp = handle_inner(&state, payload).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "hook inner failed (fail-open)");
        HookResponse { continue_: true, hook_specific_output: None }
    });
    Json(resp).into_response()
}

async fn handle_inner(state: &AppState, payload: HookPayload) -> Result<HookResponse, AppError> {
    let team = payload.team_id.clone().unwrap_or_default();
    let user = payload.user_id.clone().unwrap_or_default();

    // PreToolUse 走决策表
    if payload.hook_event == "PreToolUse" {
        let role = ws::get_role(&state.db, &team, &user)
            .await?
            .unwrap_or_else(|| "member".to_string()); // 默认保守

        let tool_name = payload.tool_name.clone().unwrap_or_default();
        let target = payload
            .tool_input
            .as_ref()
            .and_then(ws::decision::target_path)
            .unwrap_or_else(|| std::path::PathBuf::from(&payload.cwd.clone().unwrap_or_default()));

        let decision = decide_pre_tool_use(&role, &tool_name, &target, &state.codex_home);

        let perm = match decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask | Decision::AskWithUpdatedInput(_) => "ask",
        };

        // audit(异步入队)
        state.audit_writer.submit(ws::audit_writer::AuditEvent {
            team_id: Some(team.clone()),
            user_id: Some(user.clone()),
            thread_id: payload.session_id.clone(),
            event_type: "PreToolUse".into(),
            tool_name: Some(tool_name.clone()),
            payload: payload.tool_input.clone().unwrap_or(Value::Null),
            decision: Some(perm.to_string()),
        });

        return Ok(HookResponse {
            continue_: perm != "deny",
            hook_specific_output: Some(HookSpecificOutput {
                permission_decision: perm,
                updated_input: None,
            }),
        });
    }

    // 其他事件:仅 audit,放行
    state.audit_writer.submit(ws::audit_writer::AuditEvent {
        team_id: Some(team),
        user_id: Some(user),
        thread_id: payload.session_id.clone(),
        event_type: payload.hook_event.clone(),
        tool_name: payload.tool_name.clone(),
        payload: payload.raw.clone(),
        decision: None,
    });

    Ok(HookResponse { continue_: true, hook_specific_output: None })
}

/// 常量时间比较(避免 timing attack)。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) { diff |= x ^ y; }
    diff == 0
}
```

- [ ] **Step 2: api/mod.rs 暴露**

```rust
pub mod hooks;
```

- [ ] **Step 3: main.rs 挂载独立路由**

在 `build_router(state).await.layer(ws_layer);` 后,**不要**合并进 api_router;而是构造独立 Router 挂到主 app:

```rust
let hook_router = axum::Router::new()
    .route("/hooks/codex", axum::routing::post(hooks::handle))
    .with_state(state.clone());

let app = build_router(state).await
    .layer(ws_layer)
    .merge(hook_router);
```

具体合并位置以 `main.rs` 现有 `let app = ...` 行为准:若现有 `app` 是 Router,直接 `.merge(hook_router)`;若先 serve 再 attach,按现成流程嵌入。

- [ ] **Step 4: 编译**

```bash
cd backend-rs && cargo build
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/api/hooks.rs backend-rs/src/api/mod.rs backend-rs/src/main.rs
git commit -m "feat(multitenant): POST /hooks/codex webhook 路由

- 独立 X-Hook-Token 鉴权(常量时间比较)
- PreToolUse 走 decision 模块,member 写 shared 拒绝
- 其他事件 audit 入队 + continue=true
- 任何内部异常 fail-open,不阻断 codex

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: 集成测试 hooks_webhook

**Files:**
- Create: `backend-rs/tests/hooks_webhook.rs`

**Interfaces:**
- 测四个场景:member 写 shared deny / owner 写 shared allow / 缺 token 401 / 异常 fail-open

- [ ] **Step 1: 创建测试文件**

```rust
//! hooks/codex 路由集成测试。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sea_orm::Database;
use serde_json::json;
use tower::ServiceExt;

const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef-test";

#[tokio::test]
async fn no_token_returns_401() {
    let app = build_test_app().await;
    let req = Request::builder()
        .method("POST")
        .uri("/hooks/codex")
        .header("content-type", "application/json")
        .body(Body::from(json!({"hook_event":"PreToolUse"}).to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn member_writing_team_shared_is_denied() {
    let app = build_test_app().await;
    let body = json!({
        "hook_event": "PreToolUse",
        "session_id": "thread-1",
        "team_id": "t1",
        "user_id": "u1",
        "tool_name": "write_file",
        "tool_input": { "file_path": "/tmp/teams/t1/shared/x.txt" },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/hooks/codex")
        .header("x-hook-token", TEST_TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["continue"], true);  // 仍 continue,但 permissionDecision=deny
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[tokio::test]
async fn post_tool_use_writes_audit_row() {
    let (app, db) = build_test_app_with_db().await;
    let body = json!({
        "hook_event": "PostToolUse",
        "session_id": "thread-1",
        "team_id": "t1",
        "user_id": "u1",
        "tool_name": "shell",
        "tool_output": "ok",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/hooks/codex")
        .header("x-hook-token", TEST_TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 触发 batch flush(等 1.2s)
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let count: i64 = sea_orm::QueryTrait::query(
        &sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) FROM workspace_audit WHERE event_type = 'PostToolUse'".into(),
        )
    ).one(&db).await.unwrap().unwrap().unwrap();
    assert!(count >= 1, "expected audit row, got count={count}");
}

async fn build_test_app() -> axum::Router {
    let (app, _db) = build_test_app_with_db().await;
    app
}

async fn build_test_app_with_db() -> (axum::Router, sea_orm::DatabaseConnection) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    codex_webui::db::migration::Migrator::up(&db, None).await.unwrap();

    let audit_writer = codex_webui::services::workspace::audit_writer::spawn(db.clone());

    // 临时构造 codex_home 指向 tempdir
    let tmp = std::env::temp_dir().join(format!("hook-test-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp).await.unwrap();
    let tmp_str = tmp.to_string_lossy().to_string();

    let state = codex_webui::state::AppState {
        db: db.clone(),
        mt_master_key: "k".into(),
        mt_team_codex: todo!("构造 stub TeamCodexManager,或新增 trait 抽象后用 stub"),
        // ...
        hook_token: TEST_TOKEN.into(),
        audit_writer,
        codex_home: tmp.into(),
        // 其它字段按现有 AppState literal 填 stub,详见 follow-up
    };
    // 实际写测试时若 AppState 字段太多难以手填,改为 #[cfg(test)] mod helper 提供 builder
    todo!()
}
```

**注**:测试中 `todo!()` 占位的具体 stub 在实施时按现有 AppState 字段逐个填。**优先方案**:把 `AppState` 改为 `AppState { ..Default::default() }` 不可行(无 Default),故在 `state.rs` 加 `#[cfg(test)] impl AppState { pub fn test_stub(db: DatabaseConnection, codex_home: PathBuf, token: String) -> Self {...} }`。

- [ ] **Step 2: 在 state.rs 加 test_stub**

```rust
#[cfg(test)]
impl AppState {
    pub fn test_stub(db: sea_orm::DatabaseConnection, codex_home: std::path::PathBuf, hook_token: String) -> Self {
        use crate::services::workspace::audit_writer;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        Self {
            db,
            mt_master_key: "k".into(),
            mt_team_codex: Arc::new(crate::services::multitenant::codex_pool::TeamCodexManager::new(
                codex_home.clone(),
                "codex".into(),
                None,
                crate::services::multitenant::codex_pool::PoolConfig::new(1,1,60,1,1),
                None,
            )),
            mt_redis: None,
            metrics_handle: None,
            auth: Arc::new(crate::auth::AuthService::new("test-key")),
            codex: Arc::new(crate::codex::CodexProcessManager::new("codex".into(), Some(codex_home.to_string_lossy().to_string()))),
            terminal: Arc::new(crate::services::terminal::TerminalService::new(crate::services::terminal::TerminalConfig::default())),
            status: Arc::new(crate::services::codex_status::CodexStatusService::new(Arc::new(crate::codex::CodexProcessManager::new("codex".into(), None)))),
            resume_registry: Arc::new(crate::services::threads::ThreadResumeRegistry::new()),
            dynamic_files_roots: Arc::new(Mutex::new(HashSet::new())),
            settings_cache: Arc::new(Mutex::new(HashMap::new())),
            codex_home,
            node_id: "test".into(),
            cluster: Arc::new(crate::services::multitenant::cluster::SingleCluster::new("test".into(), "http://localhost".into())),
            worker_rpc: Arc::new(crate::services::multitenant::rpc::WorkerRpcClient::new(None)),
            internal_token: "0123456789abcdef0123456789abcdef".into(),
            active_rollout: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            local_offsets: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            hook_token,
            audit_writer: audit_writer::spawn(/* db 引用 */ db_for_writer()),
        }
    }
}

#[cfg(test)]
fn db_for_writer() -> sea_orm::DatabaseConnection { unimplemented!() }
```

(实施时:把 `db_for_writer` 直接由 `test_stub` 第一个参数 clone 后传入 audit_writer::spawn,函数签名调整即可。)

- [ ] **Step 3: 跑测试**

```bash
cd backend-rs && cargo test --test hooks_webhook -- --nocapture
```

期望:全部用例通过。若 `test_stub` 构造报错,先修 stub 不阻塞测试。

- [ ] **Step 4: 提交**

```bash
git add backend-rs/tests/hooks_webhook.rs backend-rs/src/state.rs
git commit -m "test(multitenant): hooks_webhook 集成测试 + AppState::test_stub

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: spawn_slot 注入 config.toml hooks 配置

**Files:**
- Modify: `backend-rs/src/services/multitenant/codex_pool.rs::spawn_slot`

**Interfaces:**
- Consumes: `state.codex_home`, `state.http_bind_port`(新字段)/ 或从 AppState 取 cfg.port
- Produces: `$CODEX_HOME/config.toml` 含 `[hooks.audit]` 节

- [ ] **Step 1: AppState 加 http_bind_port**

`backend-rs/src/state.rs`:

```rust
/// HTTP 监听端口(供 codex hook webhook URL 使用)。
pub http_bind_port: u16,
```

`backend-rs/src/main.rs` 在构造 AppState 时填 `http_bind_port: cfg.port`。

- [ ] **Step 2: 创建 config.toml 写入函数**

文件 `backend-rs/src/services/workspace/hooks_config.rs`(新建):

```rust
//! 启动 codex 前向 $CODEX_HOME/config.toml 写入 hooks 配置。

use crate::error::AppError;
use std::path::Path;

pub async fn write_hooks_config(codex_home: &Path, port: u16) -> Result<(), AppError> {
    let cfg_path = codex_home.join("config.toml");
    let body = format!(
        "# 由 backend-rs 启动时自动注入(per-user workspace 实施步骤 11)\n\
         [hooks.audit]\n\
         type = \"http\"\n\
         url = \"http://127.0.0.1:{port}/hooks/codex\"\n"
    );
    tokio::fs::write(&cfg_path, body).await
        .map_err(|e| AppError::internal(format!("write {}: {e}", cfg_path.display())))?;
    Ok(())
}
```

`mod.rs` 暴露:`pub mod hooks_config;`

- [ ] **Step 3: spawn_slot 调用 + 加命令行 flag**

在 `codex_pool.rs::spawn_slot` 中,socket spawn 之前:

```rust
crate::services::workspace::hooks_config::write_hooks_config(&codex_home, /* port 来自 state */ 0).await?;
```

(具体 port 注入:`spawn_slot` 当前签名只接 team_id + db + master_key,需扩为接 `port: u16`;在 `client_for` 与 `restart_team` 中加 port 参数,从 `state.http_bind_port` 传入。)

修改 `spawn_slot` 构造 codex 命令行:

```rust
let mut cmd = build_codex_command(&self.codex_bin);
cmd.args(["app-server", "--listen", "stdio://", "--dangerously-bypass-hook-trust"]);
```

(原 args 不带 `--dangerously-bypass-hook-trust`,此处追加。)

- [ ] **Step 4: client_for / restart_team / ensure_capacity / reserve_global_capacity 串传 port**

每个方法的签名加 `port: u16` 参数,从调用方 `state.http_bind_port` 传入。**全链路修改**:`TeamCodexManager` 上加 `http_bind_port: u16` 字段,`new` 时初始化,后续所有方法从 `self.http_bind_port` 取。

实施步骤:
1. `TeamCodexManager` 加字段 + `new` 参数
2. 所有 `self.client_for(team_id, db, key)` 调用点改为 `self.client_for(team_id, db, key)` (内部用 `self.http_bind_port`)
3. `client_for` 改为 `async fn client_for(&self, team_id, db, master_key)`(签名不变,内部从 self 取)
4. `ensure_capacity` / `spawn_slot` 内部从 self 取

简化方案:不传 port 参数,直接让 `TeamCodexManager` 持有 `Arc<AppState>` 引用,从 state 取 `http_bind_port`。这样改动最小:

- `TeamCodexManager::new` 改为接受 `Arc<AppState>` 全部状态(或仅必要字段)。
- 或仅注入一个 `HookConfigWriter` 闭包: `Arc<dyn Fn() -> Pin<Box<dyn Future<Output=Result<(), AppError>>>>>`

实施时按工程最小修改原则选其中之一。

- [ ] **Step 5: 编译**

```bash
cd backend-rs && cargo build
```

- [ ] **Step 6: 提交**

```bash
git add backend-rs/src/services/multitenant/codex_pool.rs backend-rs/src/services/workspace/hooks_config.rs backend-rs/src/services/workspace/mod.rs backend-rs/src/state.rs backend-rs/src/main.rs
git commit -m "feat(multitenant): spawn_slot 注入 hooks config.toml + bypass-hook-trust

- $CODEX_HOME/config.toml 含 [hooks.audit] 指向 /hooks/codex
- spawn 命令加 --dangerously-bypass-hook-trust(自动化不弹首次确认)
- TeamCodexManager 持有 port 引用

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: 端到端手测 + 文档更新

**Files:**
- Modify: `docs/superpowers/specs/2026-07-18-per-user-workspace-hooks-design.md`(末尾追加"实施状态")
- Modify: `DEPLOY.md`(若有,追加 `INTERNAL_HOOK_TOKEN` 启动 env)

**Interfaces:**
- 无新增代码;只验证端到端 + 更新文档

- [ ] **Step 1: 启动 backend**

```bash
cd backend-rs
export INTERNAL_HOOK_TOKEN=$(openssl rand -hex 32)
export INTERNAL_RPC_TOKEN=$(openssl rand -hex 32)
export WORKER_ID=test
export DATABASE_URL=sqlite://./test.db
cargo run
```

- [ ] **Step 2: 注册一个用户**

```bash
curl -X POST http://127.0.0.1:8787/api/mt/auth/register \
  -H 'content-type: application/json' \
  -d '{"username":"u1","password":"p1"}'
```

期望:`$CODEX_HOME/users/u1/personal/` 存在。

- [ ] **Step 3: 创建 team + 触发 hooks**

```bash
curl -X POST http://127.0.0.1:8787/api/mt/teams \
  -H 'authorization: Bearer ...' \
  -d '{"name":"team1"}'
```

期望:`$CODEX_HOME/teams/{tid}/shared/` 存在;`config.toml` 含 `[hooks.audit]`。

- [ ] **Step 4: 手动 POST /hooks/codex**

```bash
curl -X POST http://127.0.0.1:8787/hooks/codex \
  -H "x-hook-token: $INTERNAL_HOOK_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"hook_event":"PreToolUse","team_id":"t1","user_id":"u1","tool_name":"write_file","tool_input":{"file_path":"/.../teams/t1/shared/x"}}'
```

期望:`{"continue":true,"hookSpecificOutput":{"permissionDecision":"deny"}}`。

- [ ] **Step 5: 验证 audit 落库**

```bash
sqlite3 test.db 'SELECT event_type,tool_name,decision FROM workspace_audit ORDER BY id DESC LIMIT 5;'
```

期望:看到刚提交的 PreToolUse 行。

- [ ] **Step 6: 更新文档**

在 design doc 末尾追加"实施状态:M1-M4 已完成;待办:hook payload 字段名按 codex 0.142.5 真实协议校正;turn 启动 cwd/--add-dir 切换见独立子任务"。

- [ ] **Step 7: 提交**

```bash
git add docs/superpowers/specs/2026-07-18-per-user-workspace-hooks-design.md DEPLOY.md
git commit -m "docs(multitenant): per-user workspace + hook webhook 端到端验证通过

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review Checklist

执行者完成所有 task 后回看:

- [ ] 12 个 task 全部通过 `cargo build` + `cargo test`
- [ ] `INTERNAL_HOOK_TOKEN` 缺失时 backend 启动失败
- [ ] `POST /hooks/codex` 无 token → 401
- [ ] `POST /hooks/codex` member 写 shared → deny
- [ ] `POST /hooks/codex` 内部异常 → 200 + continue=true
- [ ] workspace_audit 表在事件后 1.2s 内出现行
- [ ] `$CODEX_HOME/config.toml` 含 `[hooks.audit]` 节
- [ ] codex 进程命令行含 `--dangerously-bypass-hook-trust`
- [ ] 个人/team workspace 目录在注册/创建时按需建立
- [ ] 不破坏现有 per-team 进程池 / HA 复制 / event_bus
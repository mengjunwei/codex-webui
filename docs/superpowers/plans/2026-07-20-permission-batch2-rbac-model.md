# 权限加固 批次2：RBAC 权限数据模型 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 建立 RBAC 权限点系统数据模型与校验函数——migration（平台管理员字段 + role CHECK + role_permissions 表 + seed 矩阵）、TeamPermission enum、require_permission/require_platform_admin 校验函数、平台管理员 bootstrap。

**Architecture:** 三层叠加——(1) 新 migration `m20260720_000001_rbac_permissions` 扩 schema；(2) `permissions.rs` 定义 TeamPermission enum + 团队级/平台级校验函数（查 role_permissions / users.is_platform_admin）；(3) config `admin_emails` + 启动 bootstrap 初始化首个平台管理员。本批次**只建模型与函数，不迁移 handler 调用**（那是批次3），现有 `require_member/require_owner` 保留不动。

**Tech Stack:** Rust 2024 / SeaORM 1.1 + sea-orm-migration / axum / Postgres（MySQL 兼容）。

## Global Constraints

- 中文注释（项目惯例）。
- `cargo build -p codex-webui`（在 `backend-rs/` 下）必须零错误通过；`cargo test -p codex-webui` 全绿。
- 角色权限矩阵以 spec §4.1 为准（owner 全权限；admin 减 `team:owner:transfer`/`team:dissolve`/`team:member:role:write`；member 仅 `member:list`/`thread:create`/`thread:read`/`turn:write`）。
- permission code 格式 `team:{resource}:{action}`。
- 多方言兼容：raw SQL 用 `execute_unprepared`；PG 专属语法（`COMMENT ON COLUMN`）用 `.await.ok()` 容错 MySQL。
- **不改动** `require_member`/`require_owner`/`require_thread_team` 的现有调用点（批次3 才迁移）；不改动 handler。
- 测试约束（同批次1）：纯逻辑（`TeamPermission::code()`）严格 TDD；DB 依赖函数（require_permission/require_platform_admin）用编译 + 手动验证，自动化回归留批次4。

---

### Task 1: RBAC migration + 注册 Migrator

**Files:**
- Create: `backend-rs/src/db/migration/m20260720_000001_rbac_permissions.rs`
- Modify: `backend-rs/src/db/migration/mod.rs`（注册新 migration + `mod` 声明）

**Interfaces:**
- Produces: schema 变更——`users.is_platform_admin BOOLEAN`、`team_members.role` CHECK 约束、`role_permissions(role, permission)` 表 + seed。供 Task 2 entity 与 Task 3 校验函数使用。

- [ ] **Step 1: 创建 migration 文件**

新建 `backend-rs/src/db/migration/m20260720_000001_rbac_permissions.rs`：

```rust
//! RBAC 权限点系统 + 平台管理员字段。
//!
//! - users.is_platform_admin:平台超级管理员(可改全局配置/读全局日志)。
//! - team_members.role CHECK:只允许 owner/admin/member(消除幽灵角色)。
//! - role_permissions:角色→权限点映射,seed 三角色矩阵(spec §4.1)。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260720_000001_rbac_permissions"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 1. users.is_platform_admin(全新列;默认 false)。
        db.execute_unprepared(
            r#"ALTER TABLE users ADD COLUMN is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE"#,
        ).await?;
        // COMMENT 仅 PG;MySQL 无 COMMENT ON COLUMN,.ok() 容错。
        let _ = db.execute_unprepared(
            "COMMENT ON COLUMN users.is_platform_admin IS '平台超级管理员标记（可改全局配置/读全局日志）';"
        ).await;

        // 2. team_members.role CHECK 约束(PG/MySQL 8.0+ 强制;5.7 忽略,应用层亦有校验)。
        db.execute_unprepared(
            r#"ALTER TABLE team_members ADD CONSTRAINT team_members_role_chk
               CHECK (role IN ('owner','admin','member'))"#,
        ).await?;

        // 3. role_permissions 表(全局,无 team_id)。
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS role_permissions (
                role VARCHAR(16) NOT NULL,
                permission VARCHAR(48) NOT NULL,
                PRIMARY KEY (role, permission)
            )"#,
        ).await?;

        // 4. seed 角色权限矩阵(spec §4.1)。
        //    owner=全权限; admin=owner 减 transfer/dissolve/role:write; member=4 个基础。
        db.execute_unprepared(
            r#"INSERT INTO role_permissions (role, permission) VALUES
               ('owner','team:member:list'),
               ('owner','team:thread:create'),
               ('owner','team:thread:read'),
               ('owner','team:turn:write'),
               ('owner','team:member:invite'),
               ('owner','team:member:remove'),
               ('owner','team:member:role:write'),
               ('owner','team:api_key:read'),
               ('owner','team:api_key:write'),
               ('owner','team:audit:read'),
               ('owner','team:owner:transfer'),
               ('owner','team:dissolve'),
               ('admin','team:member:list'),
               ('admin','team:thread:create'),
               ('admin','team:thread:read'),
               ('admin','team:turn:write'),
               ('admin','team:member:invite'),
               ('admin','team:member:remove'),
               ('admin','team:api_key:read'),
               ('admin','team:api_key:write'),
               ('admin','team:audit:read'),
               ('member','team:member:list'),
               ('member','team:thread:create'),
               ('member','team:thread:read'),
               ('member','team:turn:write')"#,
        ).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(r#"DROP TABLE IF EXISTS role_permissions"#).await?;
        db.execute_unprepared(
            r#"ALTER TABLE team_members DROP CONSTRAINT IF EXISTS team_members_role_chk"#,
        ).await?;
        db.execute_unprepared(r#"ALTER TABLE users DROP COLUMN is_platform_admin"#).await?;
        Ok(())
    }
}
```

- [ ] **Step 2: 注册到 Migrator**

在 `backend-rs/src/db/migration/mod.rs`：

在 `mod m20260719_000001_combined_schema;`（行 10）下方加：

```rust
mod m20260720_000001_rbac_permissions;
```

把 `migrations()` 的 vec（行 16-18）改为：

```rust
        vec![
            Box::new(m20260719_000001_combined_schema::Migration),
            Box::new(m20260720_000001_rbac_permissions::Migration),
        ]
```

- [ ] **Step 3: 编译验证**

Run（从 `backend-rs/`）: `cargo build`
Expected: 零错误（migration 注册成功）。

- [ ] **Step 4: 手动验证 migration 跑通（需 PG；若无环境则标注待验）**

若有本地 PG：跑一次服务（`Migrator::up` 会执行新 migration），用 psql 确认：
- `\d users` 含 `is_platform_admin`
- `SELECT count(*) FROM role_permissions` = 25（owner 12 + admin 8 + member 4 + ... 实际 owner 12 + admin 8 + member 4 = 24；核对后修正期望值）
- `SELECT count(*) FROM role_permissions WHERE role='owner'` = 12

若无法跑 PG：在 report 标注"migration 跑通待 PG 环境验证"，不阻塞（编译通过即 gate）。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/db/migration/m20260720_000001_rbac_permissions.rs backend-rs/src/db/migration/mod.rs
git commit -m "feat(rbac): RBAC migration——平台管理员字段 + role CHECK + role_permissions 矩阵 seed"
```

---

### Task 2: 权限模型代码（entities + permissions.rs + 校验函数）

**Files:**
- Modify: `backend-rs/src/db/entities/mod.rs`（加 `role_permission` 模块；`user` 模块加 `is_platform_admin` 字段）
- Create: `backend-rs/src/services/multitenant/permissions.rs`（TeamPermission enum + 校验函数 + 单测）
- Modify: `backend-rs/src/services/multitenant/mod.rs`（`pub mod permissions;`）
- Modify: 任何构造 `user::ActiveModel` 的点（注册流程，`grep -rn "user::ActiveModel" backend-rs/src` 定位）——加 `is_platform_admin: Set(false)`

**Interfaces:**
- Consumes: Task 1 的 schema（role_permissions 表、users.is_platform_admin 列）；现有 `teams::member_role(db, team_id, user_id) -> Result<Option<String>, AppError>`（`teams.rs:48`）
- Produces:
  - `pub enum TeamPermission` + `fn code(&self) -> &'static str`
  - `pub const ROLE_ADMIN: &str = "admin"`（ROLE_OWNER/ROLE_MEMBER 已在 teams.rs）
  - `pub async fn require_permission(db, team_id, user_id, perm: TeamPermission) -> Result<String, AppError>`（返回 role）
  - `pub async fn require_platform_admin(db, user_id) -> Result<(), AppError>`
  - `pub async fn bootstrap_platform_admins(db, admin_emails: &[String]) -> Result<(), AppError>`（Task 3 用）

- [ ] **Step 1: 写失败测试 — TeamPermission::code() 映射**

新建 `backend-rs/src/services/multitenant/permissions.rs`，先只放 enum + 测试骨架：

```rust
//! 团队级权限点 + 角色校验函数。
//!
//! 权限点用 enum(编译期类型安全),角色→权限映射存 role_permissions 表(seed)。
//! require_permission 查 team_members.role → role_permissions 判定。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::role_permission::{
    Column as RolePermissionColumn, Entity as RolePermissionEntity,
};
use crate::db::entities::user::{Column as UserColumn, Entity as UserEntity};
use axum::http::StatusCode;
use sea_orm::entity::prelude::*;
use sea_orm::DatabaseConnection;

pub const ROLE_ADMIN: &str = "admin";

/// 团队级权限点。`code()` 对应 role_permissions.permission 列的 `team:{resource}:{action}`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TeamPermission {
    MemberList,
    MemberInvite,
    MemberRemove,
    MemberRoleWrite,
    ApiKeyRead,
    ApiKeyWrite,
    AuditRead,
    ThreadCreate,
    ThreadRead,
    TurnWrite,
    OwnerTransfer,
    TeamDissolve,
}

impl TeamPermission {
    pub fn code(&self) -> &'static str {
        match self {
            TeamPermission::MemberList => "team:member:list",
            TeamPermission::MemberInvite => "team:member:invite",
            TeamPermission::MemberRemove => "team:member:remove",
            TeamPermission::MemberRoleWrite => "team:member:role:write",
            TeamPermission::ApiKeyRead => "team:api_key:read",
            TeamPermission::ApiKeyWrite => "team:api_key:write",
            TeamPermission::AuditRead => "team:audit:read",
            TeamPermission::ThreadCreate => "team:thread:create",
            TeamPermission::ThreadRead => "team:thread:read",
            TeamPermission::TurnWrite => "team:turn:write",
            TeamPermission::OwnerTransfer => "team:owner:transfer",
            TeamPermission::TeamDissolve => "team:dissolve",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_code_format_is_team_namespace() {
        for perm in [
            TeamPermission::MemberList, TeamPermission::MemberInvite,
            TeamPermission::MemberRemove, TeamPermission::MemberRoleWrite,
            TeamPermission::ApiKeyRead, TeamPermission::ApiKeyWrite,
            TeamPermission::AuditRead, TeamPermission::ThreadCreate,
            TeamPermission::ThreadRead, TeamPermission::TurnWrite,
            TeamPermission::OwnerTransfer, TeamPermission::TeamDissolve,
        ] {
            assert!(perm.code().starts_with("team:"), "{} missing team: prefix", perm.code());
            // 至少两层冒号:team:{resource}:{action}
            assert_eq!(perm.code().matches(':').count(), 2, "{} must be team:r:a", perm.code());
        }
    }

    #[test]
    fn specific_codes_match_spec() {
        assert_eq!(TeamPermission::MemberRemove.code(), "team:member:remove");
        assert_eq!(TeamPermission::ApiKeyWrite.code(), "team:api_key:write");
        assert_eq!(TeamPermission::OwnerTransfer.code(), "team:owner:transfer");
        assert_eq!(TeamPermission::TeamDissolve.code(), "team:dissolve");
    }
}
```

- [ ] **Step 2: 运行测试确认通过（enum 已实现，建立基线）**

Run（从 `backend-rs/`）: `cargo test permissions::tests`
Expected: 2 个测试 PASS。

- [ ] **Step 3: 加 role_permission entity + user.is_platform_admin 字段**

在 `backend-rs/src/db/entities/mod.rs`：

**3a.** 在文件末尾（最后一个 `pub mod` 之后）加 `role_permission` 模块：

```rust
/// 角色权限映射(全局,无 team_id)。seed 由 migration 写入(spec §4.1 矩阵)。
pub mod role_permission {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "role_permissions")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(16))")]
        pub role: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(48))")]
        pub permission: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
```

**3b.** 在 `pub mod user`（约行 5-24）的 `Model` 结构体里，`updated_at` 字段后加：

```rust
        /// 平台超级管理员标记(可改全局配置/读全局日志)。默认 false。
        pub is_platform_admin: bool,
```

- [ ] **Step 4: 修复所有 user::ActiveModel 构造点**

Run: `grep -rn "user::ActiveModel" backend-rs/src`
对每个构造点（注册流程 `auth.rs` 等），加 `is_platform_admin: Set(false),`。示例（若 `auth.rs` 的 register 构造 `user::ActiveModel { id: Set(...), email: Set(...), ..., updated_at: Set(now) }`），在最后加：

```rust
            is_platform_admin: Set(false),
```

- [ ] **Step 5: 实现 require_permission / require_platform_admin / bootstrap**

在 `permissions.rs` 的 `impl TeamPermission` 块之后、`#[cfg(test)]` 之前加：

```rust
// ── 团队级权限校验 ─────────────────────────────────────────────────────────

/// 要求 user 在该 team 持有 perm;否则 403。返回其 role。
pub async fn require_permission(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
    perm: TeamPermission,
) -> Result<String, AppError> {
    let role = crate::services::multitenant::teams::member_role(db, team_id, user_id)
        .await?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpForbidden,
                StatusCode::FORBIDDEN,
                "not a team member".into(),
                None,
            )
        })?;
    let granted = RolePermissionEntity::find()
        .filter(RolePermissionColumn::Role.eq(role.clone()))
        .filter(RolePermissionColumn::Permission.eq(perm.code().to_string()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query role permission: {e}")))?
        .is_some();
    if !granted {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "permission denied".into(),
            None,
        ));
    }
    Ok(role)
}

// ── 平台级 ─────────────────────────────────────────────────────────────────

/// user 是否为平台超级管理员。
pub async fn is_platform_admin(db: &DatabaseConnection, user_id: &str) -> Result<bool, AppError> {
    let u = UserEntity::find_by_id(user_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query user: {e}")))?;
    Ok(u.map(|m| m.is_platform_admin).unwrap_or(false))
}

/// 要求 user 是平台超级管理员;否则 403。
pub async fn require_platform_admin(
    db: &DatabaseConnection,
    user_id: &str,
) -> Result<(), AppError> {
    if !is_platform_admin(db, user_id).await? {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "platform admin only".into(),
            None,
        ));
    }
    Ok(())
}

/// 启动期 bootstrap:把 admin_emails 里已存在的用户置 is_platform_admin=true。
/// 不撤销不在列表里的(避免误降权)。供 main.rs 在 migration 后调用。
pub async fn bootstrap_platform_admins(
    db: &DatabaseConnection,
    admin_emails: &[String],
) -> Result<u64, AppError> {
    if admin_emails.is_empty() {
        return Ok(0);
    }
    let emails: Vec<String> = admin_emails
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if emails.is_empty() {
        return Ok(0);
    }
    let res = UserEntity::update_many()
        .col_expr(UserColumn::IsPlatformAdmin, Expr::value(true))
        .filter(UserColumn::Email.is_in(emails))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("bootstrap platform admins: {e}")))?;
    Ok(res.rows_affected)
}
```

- [ ] **Step 6: 导出 permissions 模块**

在 `backend-rs/src/services/multitenant/mod.rs` 加（若已有 `pub mod` 列表则追加）：

```rust
pub mod permissions;
```

- [ ] **Step 7: 全量编译 + 测试**

Run（从 `backend-rs/`）: `cargo build && cargo test`
Expected: 编译零错误；permissions::tests 2 个 PASS；全部测试绿。

- [ ] **Step 8: Commit**

```bash
git add backend-rs/src/db/entities/mod.rs backend-rs/src/services/multitenant/permissions.rs backend-rs/src/services/multitenant/mod.rs
# 以及 grep 定位的 user::ActiveModel 构造点文件
git commit -m "feat(rbac): TeamPermission enum + require_permission/require_platform_admin + role_permission entity"
```

---

### Task 3: 平台管理员 bootstrap（config admin_emails + main.rs）

**Files:**
- Modify: `backend-rs/src/config.rs`（`SecurityConfig` 加 `admin_emails`）
- Modify: `backend-rs/src/main.rs`（migration 后调 `bootstrap_platform_admins`）

**Interfaces:**
- Consumes: Task 2 的 `bootstrap_platform_admins(db, &[String]) -> Result<u64, AppError>`；`main.rs:58` 的 `Migrator::up(&db, None)`
- Produces: 启动时自动把 `config.toml [security] admin_emails` 列表中的已存在用户置为平台管理员。

- [ ] **Step 1: config.rs 加 admin_emails**

在 `backend-rs/src/config.rs` 的 `SecurityConfig`（约行 243-247）加字段：

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct SecurityConfig {
    pub internal_rpc_token: String,
    pub internal_hook_token: String,
    /// 平台超级管理员邮箱列表(启动期 bootstrap:把这些已存在用户置 is_platform_admin=true)。
    /// 仅用于初始化首个管理员;之后以 DB 为准(不在列表里的不会被撤销)。
    #[serde(default)]
    pub admin_emails: Vec<String>,
}
```

- [ ] **Step 2: main.rs 在 migration 后调 bootstrap**

在 `backend-rs/src/main.rs`，定位 `Migrator::up(&db, None)`（约行 58-60）。在其 `.await.map_err(...)?;` 之后、AppState 构造之前，插入：

```rust
    // 平台管理员 bootstrap:把 config [security] admin_emails 里的用户置 admin。
    let bootstrapped = codex_webui::services::multitenant::permissions::bootstrap_platform_admins(
        &db,
        &cfg.security.admin_emails,
    )
    .await
    .map_err(|e| anyhow::anyhow!("bootstrap platform admins: {e}"))?;
    if bootstrapped > 0 {
        tracing::info!(count = bootstrapped, "bootstrapped platform admin(s)");
    }
```

> 若 main.rs 顶部已有 `use codex_webui::...`，保持引用风格一致；`cfg` 变量名以实际为准（main.rs 已加载的 Config 变量）。

- [ ] **Step 3: 编译验证**

Run（从 `backend-rs/`）: `cargo build`
Expected: 零错误。

- [ ] **Step 4: 验证 config 解析（单测补一行）**

在 `config.rs` 的 `#[cfg(test)] mod tests` 里，参考现有测试（如 `auth_master_key_enable_explicit`），新增一个最小测试验证 `admin_emails` 可解析（默认空、显式非空两种）。示例：

```rust
    #[test]
    fn security_admin_emails_default_empty_and_explicit() {
        let base = r#"
[server.api]
webui_api_key = "0123456789abcdef"
[cluster]
worker_id = "node-a-staaaaaaaaable"
[database]
host = "h"
user = "u"
name = "n"
[codex]
[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(base);
        let c = Config::load_from(&path).unwrap();
        assert!(c.security.admin_emails.is_empty(), "default should be empty");
        std::fs::remove_file(&path).ok();

        let with_admins = format!("{base}\nadmin_emails = [\"a@example.com\", \"b@example.com\"]\n");
        // 注:[security] 段已有,需把 admin_emails 放进 [security] 内。重写:
        let with_admins = r#"
[server.api]
webui_api_key = "0123456789abcdef"
[cluster]
worker_id = "node-a-staaaaaaaaable"
[database]
host = "h"
user = "u"
name = "n"
[codex]
[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
admin_emails = ["a@example.com", "b@example.com"]
"#;
        let path = write_cfg(with_admins);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.security.admin_emails, vec!["a@example.com", "b@example.com"]);
        std::fs::remove_file(&path).ok();
    }
```

- [ ] **Step 5: 运行测试**

Run（从 `backend-rs/`）: `cargo test config::tests::security_admin_emails`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/config.rs backend-rs/src/main.rs
git commit -m "feat(rbac): config admin_emails + 启动期 bootstrap_platform_admins"
```

---

## Self-Review 结果

**1. Spec 覆盖**：批次2 覆盖 spec §4.1（权限数据模型：enum + role_permissions + 矩阵）、§4.2（平台管理员 is_platform_admin + bootstrap）。✅ handler 迁移（§4.5）、全局面收紧（§4.6）在批次3。
**2. 占位符扫描**：所有代码步骤含完整代码；user::ActiveModel 构造点用 grep 精确定位指令。✅
**3. 类型一致**：`TeamPermission` 12 变体与 seed 的 24 条记录（owner 12 + admin 8 + member 4）的 permission code 一一对应；`require_permission(db, &str, &str, TeamPermission) -> Result<String, AppError>`、`require_platform_admin(db, &str) -> Result<(), AppError>`、`bootstrap_platform_admins(db, &[String]) -> Result<u64, AppError>` 跨任务一致。✅
**4. seed 行数核对**：owner=12（list/create/read/turn/invite/remove/role:write/api_key:read/api_key:write/audit/transfer/dissolve）、admin=8（list/create/read/turn/invite/remove/api_key:read/api_key:write/audit=实际 8：list,thread:create,thread:read,turn:write,member:invite,member:remove,api_key:read,api_key:write,audit:read = 9）。Task 1 Step 4 的"25"期望值需 implementer 按实际 INSERT 行数核对修正。
**5. 测试缺口（已记录）**：require_permission/require_platform_admin/bootstrap 的 DB 行为无自动化测试（无 DB 测试设施），用编译 + 手动验证；自动化回归留批次4。

## 批次2 完成后

- RBAC 权限模型就绪：schema + enum + 校验函数 + bootstrap
- 进入批次3（handler 迁移到 require_permission + /api/* 全局面收紧 + owner 转让/解散）的 writing-plans

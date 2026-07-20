# 权限加固 批次3a：后端应用层迁移 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** 让权限模型"用起来"——新建 me 端点、把所有 team/thread handler 从 `require_owner/require_member` 迁到 `require_permission`、收紧 `/api/*` 全局面为平台管理员专属、新增 owner 转让/team 解散/成员角色变更 API。

**Architecture:** 四块叠加——(1) `GET /api/mt/me` 返回当前用户 + is_platform_admin + 各 team 角色/权限；(2) handler 鉴权点逐个替换为 `require_permission(team_id, uid, TeamPermission::X)`（映射见 Task 2 表）；(3) `require_platform_admin_layer` middleware 挂到 `/api/settings` 写、`/api/logs`、`/api/files` 写操作；(4) 三个生命周期端点（转让/解散/角色变更）。批次2 的 `require_permission`/`require_platform_admin` 已就绪。

**Tech Stack:** Rust 2024 / axum 0.8 / SeaORM 1.1。

## Global Constraints

- 中文注释。
- `cargo build` / `cargo test`（`backend-rs/` 下）零错误全绿。
- 迁移后保留 `require_thread_team`（内部改用 `require_permission(ThreadRead)`），thread 维度 handler 继续调它。
- 不改前端（批次3b）；不改批次1/2 已完成文件的行为（permissions.rs/teams.rs 的现有函数签名不动，只新增 + 改 handlers.rs 调用点）。
- DB 依赖行为无自动化测试（项目无 DB 测试设施），用编译 + 手动验证；回归留批次4。
- 提交 220e4a9（用户有意的 config.rs 改动）不得 revert。

---

### Task 1: me 端点（GET /api/mt/me）

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs`（新增 `mt_me` handler）
- Modify: `backend-rs/src/api/mod.rs`（mt_protected 加路由 `/me`，注意放在 `/teams` 之前避免路径冲突）

**Interfaces:**
- Consumes: `permissions::is_platform_admin(db, user_id)`；`teams::member_role`；`UserId` extension
- Produces: `GET /api/mt/me` → `{ user: {id, email, display_name}, is_platform_admin: bool, teams: [{ team_id, role, permissions: ["team:..."] }] }`

- [ ] **Step 1: 实现 mt_me handler**

在 `handlers.rs` 新增（参考现有 `mt_list_my_threads` 的 `Extension(uid): Extension<UserId>` 取法）：

```rust
/// GET /api/mt/me:当前用户身份 + 平台管理员标记 + 各 team 角色/权限点。
/// 供前端权限驱动 UI 显隐。
pub async fn mt_me(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = &state.db;
    let user = user::Entity::find_by_id(uid.0.clone())
        .one(db).await
        .map_err(|e| AppError::internal(format!("query user: {e}")))?
        .ok_or_else(|| AppError::business(ErrorCode::HttpNotFound, StatusCode::NOT_FOUND, "user not found".into(), None))?;
    let is_admin = permissions::is_platform_admin(db, &uid.0).await?;
    // 用户的所有 team 成员关系
    let memberships = team_member::Entity::find()
        .filter(team_member::Column::UserId.eq(uid.0.clone()))
        .all(db).await
        .map_err(|e| AppError::internal(format!("query memberships: {e}")))?;
    let mut teams = Vec::with_capacity(memberships.len());
    for m in memberships {
        let perms = role_permission::Entity::find()
            .filter(role_permission::Column::Role.eq(m.role.clone()))
            .all(db).await
            .map_err(|e| AppError::internal(format!("query role perms: {e}")))?
            .into_iter().map(|r| r.permission).collect::<Vec<_>>();
        teams.push(serde_json::json!({
            "team_id": m.team_id,
            "role": m.role,
            "permissions": perms,
        }));
    }
    Ok(Json(serde_json::json!({
        "user": { "id": user.id, "email": user.email, "display_name": user.display_name },
        "is_platform_admin": is_admin,
        "teams": teams,
    })))
}
```

> import：确保 handlers.rs 顶部已 import `user`、`team_member`、`role_permission` entity（参考 teams.rs 的 import 风格 `use crate::db::entities::{user, team_member, role_permission};`，按需加 Column alias）。

- [ ] **Step 2: 注册路由**

在 `api/mod.rs` 的 `mt_protected`（约 :210），在 `.route("/teams", ...)` **之前**加：

```rust
        .route("/me", get(mt::mt_me))
```

- [ ] **Step 3: 编译验证**

Run（`backend-rs/`）: `cargo build`
Expected: 零错误。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs backend-rs/src/api/mod.rs
git commit -m "feat(mt): GET /api/mt/me 端点(用户身份+平台管理员+各 team 角色/权限)"
```

---

### Task 2: handler 迁移到 require_permission

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs`（替换鉴权调用点 + `require_thread_team` 内部）

**迁移映射表**（逐行替换 `teams::require_owner/require_member(...)` 为 `permissions::require_permission(team_id, &uid.0, TeamPermission::X)`，注意 `require_permission` 返回 role 而 `require_owner` 返回 `()`——丢弃返回值即可）：

| 行号 | handler | 现 | 迁移后 |
|---|---|---|---|
| 224 | list_members | require_member | require_permission(MemberList) |
| 235 | create_invitation | require_owner | require_permission(MemberInvite) |
| 264 | remove_member | require_owner | require_permission(MemberRemove) |
| 308 | set_team_api_key | require_owner | require_permission(ApiKeyWrite) |
| 333 | list_team_api_keys | require_owner | require_permission(ApiKeyRead) |
| 461 | mt_create_thread(team 分支) | require_member | require_permission(ThreadCreate) |
| 580 | mt_list_threads | require_member | require_permission(ThreadRead) |
| 758 | list_audit | require_owner | require_permission(AuditRead) |
| 434 | require_thread_team 内部(team 分支) | require_member | require_permission(ThreadRead) |

`require_thread_team`（:403）的 team 分支 `teams::require_member(db, &thread.team_id, user_id)` 改为 `permissions::require_permission(db, &thread.team_id, user_id, TeamPermission::ThreadRead).await?;`（丢弃返回的 role，或保留 `_role`）。personal 分支（所有权校验）不变。

thread 维度 handler（:693/:853/:881/:974/:991/:1008/:1025/:1050/:1080 调 `require_thread_team`）**不改**——它们经 `require_thread_team` 间接受益。

- [ ] **Step 1: 替换 9 个鉴权点**

按上表逐个替换。import：handlers.rs 顶部加 `use crate::services::multitenant::permissions::{self, TeamPermission};`。

示例（list_members :224）：
```rust
// 前: teams::require_member(db, &team_id, &uid.0).await?;
permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberList).await?;
```

- [ ] **Step 2: 编译 + 全量测试**

Run（`backend-rs/`）: `cargo build && cargo test`
Expected: 零错误；现有测试全绿（迁移不改变对外行为，仅鉴权实现切换）。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs
git commit -m "refactor(mt): team/thread handler 鉴权迁到 require_permission(权限点系统)"
```

---

### Task 3: 全局面收紧（require_platform_admin_layer）

**Files:**
- Modify: `backend-rs/src/multitenant/middleware.rs`（新增 `require_platform_admin_layer`）
- Modify: `backend-rs/src/api/mod.rs`（敏感路由挂 layer）

**Interfaces:**
- Consumes: `UserId` extension（require_user_auth 注入）；`permissions::require_platform_admin(db, user_id)`
- Produces: `require_platform_admin_layer` middleware，挂到 `/api/settings`（PATCH/DELETE）、`/api/logs`、`/api/logs/export`、`/api/files` 写操作（POST/DELETE/PATCH 方法路由）

- [ ] **Step 1: 实现 require_platform_admin_layer**

在 `multitenant/middleware.rs` 新增（参考现有 `require_user_auth` 模式）：

```rust
/// 平台管理员 gate:要求 require_user_auth 已注入的 UserId 是 is_platform_admin,否则 403。
/// 挂在 require_user_auth 之后。
pub async fn require_platform_admin_layer(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    let uid = req
        .extensions()
        .get::<UserId>()
        .cloned()
        .ok_or_else(|| AppError::unauthorized(crate::error::ErrorCode::AuthMissingHeader, "missing auth"))?;
    crate::services::multitenant::permissions::require_platform_admin(&state.db, &uid.0).await?;
    Ok(next.run(req).await)
}
```

> `UserId` 需 `Clone`（现 `pub struct UserId(pub String)` 已派生 Clone？若无需加 `#[derive(Clone)]`）。`AppState` import 同 require_user_auth。

- [ ] **Step 2: 挂到敏感路由**

在 `api/mod.rs`，定位 `/api/*` 受保护子 router（挂 require_user_auth 的那个，约 :150-206）。对其中的敏感路由**额外**挂 `require_platform_admin_layer`（在 require_user_auth layer 之内/之后）。

最高价值先收紧（若 files 写路由识别成本高，本 task 至少收紧 settings + logs，files 在 report 标注）：

```rust
// 对 settings 写、logs 整体挂平台管理员层
.route("/settings", get(settings::get_settings))  // GET 保持登录可读
.route("/settings/{key}", patch(settings::update_setting).delete(settings::delete_setting)
    .layer(axum::middleware::from_fn_with_state(state.clone(), crate::multitenant::middleware::require_platform_admin_layer)))
.route("/logs", get(logs::list_logs)
    .layer(axum::middleware::from_fn_with_state(state.clone(), crate::multitenant::middleware::require_platform_admin_layer)))
.route("/logs/export", get(logs::export_logs)
    .layer(axum::middleware::from_fn_with_state(state.clone(), crate::multitenant::middleware::require_platform_admin_layer)))
```

> 实际路由名/handler 名以 `api/mod.rs` 现状为准（grep `/settings`、`/logs`、`/files`）。files 写路由（POST/DELETE/PATCH on `/files/*`）同样挂该层；GET 读路由保持。

- [ ] **Step 3: 编译验证**

Run（`backend-rs/`）: `cargo build`
Expected: 零错误。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/multitenant/middleware.rs backend-rs/src/api/mod.rs
git commit -m "feat(authz): /api/settings,/api/logs,/api/files 写操作收紧为平台管理员专属"
```

---

### Task 4: 生命周期 API（owner 转让 / team 解散 / 成员角色变更）

**Files:**
- Modify: `backend-rs/src/services/multitenant/teams.rs`（新增 transfer/dissolve/set_member_role 业务函数）
- Modify: `backend-rs/src/api/multitenant/handlers.rs`（新增 3 个 handler）
- Modify: `backend-rs/src/api/mod.rs`（注册 3 个路由）

**Interfaces:**
- Consumes: `require_permission(OwnerTransfer/TeamDissolve/MemberRoleWrite)`；team_member/team/audit
- Produces: 3 个端点 + teams.rs 业务函数

- [ ] **Step 1: teams.rs 业务函数**

```rust
/// 转让队长:当前 owner 降为 admin,new_owner 升 owner,更新 teams.owner_id。事务。
/// new_owner_user_id 必须是当前成员。写 audit。
pub async fn transfer_owner(
    db: &DatabaseConnection, team_id: &str, current_owner: &str, new_owner_user_id: &str,
) -> Result<(), AppError> {
    if current_owner == new_owner_user_id {
        return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST, "cannot transfer to self".into(), None));
    }
    let txn = db.begin().await.map_err(|e| AppError::internal(format!("begin txn: {e}")))?;
    // new_owner 必须是成员
    let new_member = TeamMemberEntity::find()
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(new_owner_user_id.to_string()))
        .one(&txn).await
        .map_err(|e| AppError::internal(format!("query new owner: {e}")))?
        .ok_or_else(|| AppError::business(ErrorCode::HttpNotFound, StatusCode::NOT_FOUND, "new owner not a member".into(), None))?;
    // 当前 owner → admin
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(ROLE_ADMIN.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(current_owner.to_string()))
        .exec(&txn).await.map_err(|e| AppError::internal(format!("demote owner: {e}")))?;
    // new owner → owner
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(ROLE_OWNER.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(new_owner_user_id.to_string()))
        .exec(&txn).await.map_err(|e| AppError::internal(format!("promote owner: {e}")))?;
    // teams.owner_id
    TeamEntity::update_many()
        .col_expr(TeamColumn::OwnerId, Expr::value(new_owner_user_id.to_string()))
        .filter(TeamColumn::Id.eq(team_id.to_string()))
        .exec(&txn).await.map_err(|e| AppError::internal(format!("update team owner_id: {e}")))?;
    txn.commit().await.map_err(|e| AppError::internal(format!("commit txn: {e}")))?;
    Ok(())
}

/// 解散 team:CASCADE 删 members/threads/keys/audit。
pub async fn dissolve_team(db: &DatabaseConnection, team_id: &str) -> Result<(), AppError> {
    TeamEntity::delete_by_id(team_id.to_string()).exec(db).await
        .map_err(|e| AppError::internal(format!("dissolve team: {e}")))?;
    Ok(())
}

/// 改成员角色(member↔admin)。禁止改成 owner(用转让)。
pub async fn set_member_role(
    db: &DatabaseConnection, team_id: &str, user_id: &str, new_role: &str,
) -> Result<(), AppError> {
    if new_role != ROLE_ADMIN && new_role != ROLE_MEMBER {
        return Err(AppError::business(ErrorCode::HttpBadRequest, StatusCode::BAD_REQUEST, "role must be admin or member".into(), None));
    }
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(new_role.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(user_id.to_string()))
        .exec(db).await
        .map_err(|e| AppError::internal(format!("set member role: {e}")))?;
    Ok(())
}
```

> import：teams.rs 已有 TeamMemberEntity/Column、TeamEntity/Column、ROLE_OWNER/ROLE_MEMBER；ROLE_ADMIN 在 permissions.rs（`use crate::services::multitenant::permissions::ROLE_ADMIN;` 或加常量）。Expr 来自 sea_orm prelude。ErrorCode::HttpBadRequest 若不存在用实际存在的 bad-request 变体（grep ErrorCode）。

- [ ] **Step 2: handlers.rs 3 个 handler**

```rust
pub async fn transfer_team_owner(
    State(state): State<AppState>, Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>, Json(body): Json<TransferOwnerBody>,
) -> Result<StatusCode, AppError> {
    let db = &state.db;
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::OwnerTransfer).await?;
    teams::transfer_owner(db, &team_id, &uid.0, &body.new_owner_user_id).await?;
    audit::record(db, &team_id, &uid.0, "owner_transferred", Some(&body.new_owner_user_id)).await.ok();
    Ok(StatusCode::NO_CONTENT)
}

pub async fn dissolve_team_handler(
    State(state): State<AppState>, Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<StatusCode, AppError> {
    let db = &state.db;
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::TeamDissolve).await?;
    audit::record(db, &team_id, &uid.0, "team_dissolved", None).await.ok();
    teams::dissolve_team(db, &team_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_member_role_handler(
    State(state): State<AppState>, Extension(uid): Extension<UserId>,
    Path((team_id, user_id)): Path<(String, String)>, Json(body): Json<SetRoleBody>,
) -> Result<StatusCode, AppError> {
    let db = &state.db;
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberRoleWrite).await?;
    teams::set_member_role(db, &team_id, &user_id, &body.role).await?;
    audit::record(db, &team_id, &uid.0, "member_role_changed", Some(&format!("{}:{}", user_id, body.role))).await.ok();
    Ok(StatusCode::NO_CONTENT)
}
```

> Body struct：`struct TransferOwnerBody { #[serde(rename="newOwnerUserId")] new_owner_user_id: String }` 和 `struct SetRoleBody { role: String }`（加 `#[derive(Deserialize)]`）。`audit::record` 签名以现有 audit.rs 为准（grep `pub async fn record`）；若签名不同，适配。Path/Json/StatusCode import 同现有 handler。

- [ ] **Step 3: 注册路由（api/mod.rs mt_protected）**

```rust
        .route("/teams/{teamId}/transfer", post(mt::transfer_team_owner))
        .route("/teams/{teamId}", axum::routing::delete(mt::dissolve_team_handler))
        .route("/teams/{teamId}/members/{userId}/role", axum::routing::patch(mt::set_member_role_handler))
```

- [ ] **Step 4: 编译 + 测试**

Run（`backend-rs/`）: `cargo build && cargo test`
Expected: 零错误全绿。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/multitenant/teams.rs backend-rs/src/api/multitenant/handlers.rs backend-rs/src/api/mod.rs
git commit -m "feat(mt): owner 转让 + team 解散 + 成员角色变更 API"
```

---

## Self-Review 结果

**1. Spec 覆盖**：批次3a 覆盖 spec §4.5（handler 迁移）、§4.6（全局面收紧）、§4.7（生命周期 API）、me 端点（§4.9 前端依赖）。前端（§4.9 其余）在批次3b。
**2. 占位符**：迁移映射表精确到行号 + 权限点；me/生命周期 API 给完整代码；`audit::record`/`ErrorCode::HttpBadRequest`/files 路由名标注"以实际为准，grep 确认"（这些 implementer 需现场核对，已在步骤注明）。
**3. 类型一致**：`require_permission(db, &str, &str, TeamPermission) -> Result<String, AppError>`、`transfer_owner(db, &str, &str, &str) -> Result<(), AppError>` 等跨任务一致。
**4. 测试缺口**：所有新端点 DB 行为无自动化测试，编译 + 手动验证；回归留批次4。

## 批次3a 完成后

- 后端权限系统完整可用：me 端点 + 权限点鉴权 + 全局面收紧 + 生命周期 API
- 进入批次3b（前端：me 对接 + usePermission + 成员角色 UI + 转让/解散 UI + 平台管理员管理 UI + 全局面 UI 适配）

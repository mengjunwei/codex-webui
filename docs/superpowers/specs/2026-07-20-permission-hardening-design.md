# 多租户权限体系加固设计（对标最佳实践）

- 日期：2026-07-20
- 分支：`feat/multitenant-platform`
- 状态：设计已获批准，待拆实施计划

## 1. 背景与问题清单

基于对 `backend-rs` 权限体系的全面评估（4 路并行 agent 交叉验证），识别出按严重度排序的问题：

| 级别 | 问题 | 位置 |
|---|---|---|
| 🔴 P0 | WebSocket `thread.subscribe` 跨租户越权（IDOR），可实时窃听他人代码 diff 与审批参数 | `backend-rs/src/api/realtime.rs:204-218` |
| 🟠 P1 | `/api/settings`、`/api/files/*`、`/api/logs` 全局面无平台管理员，任何登录用户等同 admin | `backend-rs/src/api/mod.rs:203-206` |
| 🟠 P1 | RBAC 仅 owner/member 二元，无权限点；`admin` 是文档里的幽灵角色（schema 注释有，代码无） | `teams.rs:32-33`、`combined_schema.rs:82,85` |
| 🟡 P2 | owner 锁死：无转让/降级/解散，错误信息指向未实现的能力 | `teams.rs:226` |
| 🟡 P2 | 内网 RPC 鉴权靠每个 handler 手动调，非 layer 强制，新增 handler 漏调即裸奔 | `internal_rpc.rs:79-88` |
| 🟢 P3 | `require_auth` 死代码、`ARCHITECTURE.md` 描述过时 | `auth/middleware.rs:21` |
| 🟢 P3 | 限流依赖 `X-Forwarded-For`（可伪造）+ Redis fail-open | `handlers.rs:27-35,128-168` |

## 2. 目标与非目标

**目标**：把多租户权限体系对标业界最佳实践——认证已达标（保留），授权从粗粒度角色升级为完整权限点系统，堵住 P0 跨租户泄露，引入平台管理员与角色生命周期，前后端同步交付且前端好用。

**非目标**：本轮不做自定义角色（内置 owner/admin/member 三角色，schema 预留扩展位）；不做 OAuth/SSO/MFA（独立课题）；不动认证算法（argon2 + refresh 轮转已达标）。

## 3. 设计总览

```
AuthN 层
 ├─ JWT access(15min) + refresh(7天轮转)                    [现状,保留]
 ├─ WebSocket: on_connect 提取 user_id 注入 socket state;
 │             on_thread_subscribe 调 require_thread_team   [批次1]
 ├─ 内网 RPC: require_internal_token 提升为 layer            [批次1]
 └─ 废弃全局 API key 订阅 thread(API key 无 user_id)         [批次1]

AuthZ 层
 ├─ 平台级: users.is_platform_admin + require_platform_admin [批次2]
 ├─ 团队级: 完整权限点系统
 │    ├─ TeamPermission enum + role_permissions 表           [批次2]
 │    ├─ require_permission(team_id, perm)                   [批次2]
 │    └─ 所有 handler 迁移                                   [批次2-3]
 └─ 全局面收紧: /api/settings,/api/files,/api/logs           [批次3]

生命周期
 └─ owner 转让 + team 解散 + 成员角色变更                     [批次3]

加固
 ├─ 限流 trusted_proxies + 正确提取客户端 IP                  [批次4]
 ├─ 死代码清理 + 文档同步                                     [批次4]
 └─ 权限矩阵单测 + IDOR 回归测试                              [批次4]
```

## 4. 详细设计

### 4.1 权限数据模型

**TeamPermission enum**（`backend-rs/src/services/multitenant/permissions.rs` 新文件）：

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TeamPermission {
    MemberList, MemberInvite, MemberRemove,
    MemberRoleWrite,        // 改成员角色(member↔admin)
    ApiKeyRead, ApiKeyWrite,
    AuditRead,
    ThreadCreate, ThreadRead, TurnWrite,
    OwnerTransfer, TeamDissolve,
}
impl TeamPermission {
    pub fn code(&self) -> &'static str { /* "team:member:list" 等 */ }
}
```

**角色权限矩阵**（seed 进 `role_permissions` 表）：

| 权限点 \ 角色 | owner | admin | member |
|---|:---:|:---:|:---:|
| team:member:list | ✅ | ✅ | ✅ |
| team:thread:create | ✅ | ✅ | ✅ |
| team:thread:read | ✅ | ✅ | ✅ |
| team:turn:write | ✅ | ✅ | ✅ |
| team:member:invite | ✅ | ✅ | ❌ |
| team:member:remove | ✅ | ✅ | ❌ |
| team:member:role:write | ✅ | ❌ | ❌ |
| team:api_key:read | ✅ | ✅ | ❌ |
| team:api_key:write | ✅ | ✅ | ❌ |
| team:audit:read | ✅ | ✅ | ❌ |
| team:owner:transfer | ✅ | ❌ | ❌ |
| team:dissolve | ✅ | ❌ | ❌ |

**admin 定位**：可管成员（邀/踢）+ 密钥 + 审计的"副队长"，但不能改成员角色、不能转让 owner、不能解散 team。

**schema 变更**（新 migration `m20260720_000001_rbac_permissions.rs`）：

```sql
ALTER TABLE users ADD COLUMN is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE team_members ADD CONSTRAINT team_members_role_chk
  CHECK (role IN ('owner','admin','member'));

CREATE TABLE role_permissions (
    role        VARCHAR(16) NOT NULL,
    permission  VARCHAR(48) NOT NULL,
    PRIMARY KEY (role, permission)
);
-- seed owner/admin/member 三角色的映射(按矩阵)
```

**校验函数**（`teams.rs` / 新 `permissions.rs`）：

```rust
pub async fn has_permission(db, team_id, user_id, perm: TeamPermission) -> Result<bool, AppError>
pub async fn require_permission(db, team_id, user_id, perm: TeamPermission) -> Result<String, AppError>  // 返回 role
pub async fn require_platform_admin(db, user_id) -> Result<(), AppError>
```

迁移期保留便捷封装：`require_team_owner`（持 `MemberRoleWrite` 或 `OwnerTransfer`，等价 owner/admin）避免调用点重复。

### 4.2 平台管理员

- 字段：`users.is_platform_admin BOOLEAN DEFAULT FALSE`
- bootstrap：`config.toml` `[security] admin_emails = [...]`，启动时把匹配的已存在用户置 true（不撤销不在列表里的），解决"字段无法初始化"
- 中间件：`require_platform_admin` → 查 `users.is_platform_admin`
- 访问器：`Config` 加 `security.admin_emails: Vec<String>`
- 配套 API（平台管理员管理 UI 用，见 4.9）

### 4.3 WebSocket IDOR 修复（P0）

`backend-rs/src/api/realtime.rs`：

1. `on_connect`：把 `authenticate_token`（旧，不提取 user_id）改为优先 `verify_access`（多租户 access JWT）提取 user_id，存入 socket state。废弃 API key 订阅路径——API key 连接允许建立（兼容其他事件如 terminal），但**不允许订阅 thread 房间**。
2. `on_thread_subscribe`：取出 socket 的 user_id，调 `require_thread_team(db, thread_id, user_id)`，通过后才 `s.join(room)`。失败静默不 join（不泄露 thread 存在性）。
3. `on_thread_unsubscribe`：保持现状（离开房间无需校验）。
4. `RealtimeState` 已持有 `db`，无需新增依赖。

前端 `web/src/socket.ts` 已用 `getApiToken()`（access JWT）经 `auth: { token }` 传递，**无需改动**。

### 4.4 内网 RPC layer 化 + 死代码清理

- `internal_rpc.rs`：新增 `require_internal_token_layer`（axum middleware），挂到 `build_internal_router` 整层，替代每个 handler 第一行手动调用。手动调用删除。
- 删除 `backend-rs/src/auth/middleware.rs` 的 `require_auth`（全仓库无引用）。
- 同步 `docs/` 下 `ARCHITECTURE.md` 关于旧双轨认证、幽灵 admin 的描述。

### 4.5 handler 迁移映射（require_* → require_permission）

`backend-rs/src/api/multitenant/handlers.rs`：

| 现状 handler | 现校验 | 迁移后 |
|---|---|---|
| list_members | require_member | require_permission(MemberList) |
| create_invitation | require_owner | require_permission(MemberInvite) |
| remove_member | require_owner | require_permission(MemberRemove) |
| set_team_api_key | require_owner | require_permission(ApiKeyWrite) |
| list_team_api_keys | require_owner | require_permission(ApiKeyRead) |
| list_audit | require_owner | require_permission(AuditRead) |
| mt_create_thread(team) | require_member | require_permission(ThreadCreate) |
| mt_list_threads | require_member | require_permission(ThreadRead) |
| mt_start_turn / invoke | require_thread_team | require_thread_team(内部 ThreadRead/TurnWrite) |
| token-usage/turn-diffs/turn-errors/archive/rename/approvals | require_thread_team | require_thread_team(ThreadRead) |
| mt_delete_thread | created_by==uid | 保留(所有权,非角色权限) |

`require_thread_team` 内部：personal thread 保持所有权校验；team thread 用 `require_permission(team, ThreadRead)`。

### 4.6 全局面收紧（P1）

`/api/*` 下当前仅 `require_user_auth` 的敏感端点：

| 端点 | 收紧方式 |
|---|---|
| `PATCH /api/settings`（改全局配置） | 改为 `require_platform_admin` |
| `GET /api/settings` | 保持登录可读（只读安全） |
| `/api/files/*` 写操作（roots/write/delete/move/upload 等） | `require_platform_admin`（roots 是全局配置） |
| `/api/files/*` 读操作 | 保持登录可读 |
| `/api/logs`, `/api/logs/export` | `require_platform_admin`（全局日志） |
| `/api/codex/status`, `/api/codex/config` | 保持登录可读（已脱敏） |

实现：在主 router 把这些路由单独分组，挂 `require_platform_admin` layer（或 handler 内调用）。前端设置页相关操作改为仅平台管理员可见。

### 4.7 owner 转让 / team 解散 / 成员角色变更

新增 API（`handlers.rs` + `teams.rs`）：

| 方法 | 路径 | 校验 | 行为 |
|---|---|---|---|
| PATCH | `/api/mt/teams/{id}/members/{userId}/role` | require_permission(MemberRoleWrite) | 改 team_members.role（member↔admin）；禁止把人改成 owner（用转让） |
| POST | `/api/mt/teams/{id}/transfer` | require_permission(OwnerTransfer) | body: {newOwnerId}；newOwner 须为当前成员；当前 owner 降为 admin，新 owner 升 owner，更新 teams.owner_id |
| DELETE | `/api/mt/teams/{id}` | require_permission(TeamDissolve) | CASCADE 删 team_members/threads/team_api_keys/audit_log 等；前端二次确认 |

均写 audit_log。`remove_member` 保持"不能踢 owner"的限制——owner 想退出须先转让队长或解散团队（消除了原错误信息指向未实现能力的问题）。

### 4.8 限流加固

- `config.toml` `[security]` 加 `trusted_proxies: Vec<String>`（默认空=直连，取 peer socket addr）
- 新增 `client_ip(headers, connect_info, trusted_proxies) -> IpAddr`：仅当 peer 在 trusted_proxies 时才采信 `X-Forwarded-For` 最右可信 hop
- 登录/注册限流改用该函数取 IP；Redis 故障保留 fail-open（可用性优先），但加告警日志

### 4.9 前端同步（好用为目标）

**新增权限上下文**（`web/src/hooks/use-permissions.ts` + store）：

- `GET /api/mt/me` 扩展返回：`{ user, is_platform_admin, teams: [{ team_id, role, permissions: ["team:..."] }] }`
- 登录后一次性拉取，存全局 store
- `usePermission(teamId, perm)` / `useIsPlatformAdmin()` hook 驱动 UI 显隐

**成员管理升级**（`web/src/components/team/team-members.tsx`）：

- 角色 badge：owner / admin / member 三色区分
- 持 `team:member:role:write` 的用户（owner）：成员行显示角色下拉（member↔admin）
- 持 `team:owner:transfer`：成员行显示"设为队长"（二次确认弹窗）
- 持 `team:member:remove`：保留 Remove 按钮
- team 详情页：持 `team:dissolve` 显示"解散团队"（红色，二次输入团队名确认）

**设置页**（`web/src/components/settings/settings-page.tsx`）：

- 平台管理员可见"平台管理"tab：显示当前管理员列表（email），支持增（输入 email→若用户存在则置 admin）/删（撤销）。非平台管理员看不到此 tab 与全局 settings 写操作
- 团队 tab：增加危险操作区（转让队长 / 解散团队）

**移除不可用入口**：无对应权限的按钮/菜单主动隐藏（而非点了报错）

## 5. 分批实施计划

| 批次 | 内容 | 验证 |
|---|---|---|
| **批次1 堵漏** | 4.3 WS IDOR + 4.4 RPC layer + 死代码清理 | cargo test + 手测 WS 越权被拒 |
| **批次2 权限模型** | 4.1 migration+enum+矩阵 + 4.2 平台管理员字段+bootstrap + require_permission 函数 | migration 跑通 + 权限矩阵单测 |
| **批次3 迁移+收紧+生命周期** | 4.5 handler 迁移 + 4.6 全局面收紧 + 4.7 转让/解散/角色变更 + 前端同步 | 全量 handler 鉴权测试 + 前端 e2e |
| **批次4 加固** | 4.8 限流 + 文档 + IDOR 回归测试 | 限流测试 + 文档审阅 |

## 6. 测试策略

- **后端单测**：`permissions.rs` 矩阵（每个 role×permission 期望值）、`require_permission` 边界、平台管理员判定
- **后端集成测试**（`backend-rs/tests/`）：每个迁移后的 handler 用 owner/admin/member/非成员/平台管理员五种身份跑一遍期望 200/403；WS `thread.subscribe` 跨租户被拒（P0 回归）
- **前端**：权限 hook 的显隐逻辑测试；关键流程（转让、解散、改角色）手测

## 7. 风险与回滚

- **migration 不可逆字段**：`is_platform_admin`、`role_permissions` 为纯新增，`role` CHECK 约束在现有数据（仅 owner/member）上安全。回滚=还原代码+migration down
- **权限矩阵错误导致误拒绝/误放行**：批次2 先写矩阵单测再迁移 handler（TDD）
- **全局面收紧破坏现有部署**：批次3 收紧后，需平台管理员 bootstrap 配置 `admin_emails`，否则无人能改全局配置——在 DEPLOY.md 加显著提示
- **WS 改造兼容性**：前端已用 access JWT，无破坏；若有外部 API key 客户端订 thread，会失效（预期内，属修复）

## 8. 验收标准

1. 任意已认证用户无法订阅非自身/非所属 team 的 thread 房间（P0 闭合）
2. 非平台管理员无法 `PATCH /api/settings`、写 `/api/files` roots、读 `/api/logs`
3. owner 可转让队长、解散团队、改成员角色；admin 可邀/踢成员、管密钥、读审计；member 仅可读写 thread
4. `admin` 角色真实生效，文档无幽灵描述
5. 内网 RPC 新增 handler 默认受 layer 保护
6. 前端按权限显隐，无"点了才报错"的入口；设置页平台管理 tab 可用
7. 全部 cargo test + tsc + 前端 build 通过

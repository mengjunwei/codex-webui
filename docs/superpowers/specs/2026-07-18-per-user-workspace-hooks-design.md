# 2026-07-18 Per-User Workspace + Codex Hook Webhook — Design

## 1. 目标与范围

### 1.1 目标

1. 用户注册时自动创建个人 workspace，目录为 `CODEX_HOME/users/{user_id}/personal/`。
2. 用户加入 team 时自动创建 team 的共享 workspace，目录为 `CODEX_HOME/teams/{team_id}/shared/`；team 成员视图目录为 `CODEX_HOME/teams/{team_id}/members/{user_id}/`。
3. team workspace 的写入权限按 `workspace_role` 控制：owner/admin 可写、member 只读；个人 workspace 永远可写。
4. codex 在调用 tool / skill / plugin / mcp 时通过 HTTP webhook（`POST /hooks/codex`）回调 backend-rs，由 backend 统一做权限校验、参数改写、审计记录。
5. 不破坏现有 per-team 进程池、全局 `CODEX_HOME` 模型、Redis event_bus、多副本 HA 复制链路。

### 1.2 非目标（本期不做）

- 不做 turn 启动时 cwd / `--add-dir` 的自动切换（独立子任务，本 spec 仅在 §5 留接口）。
- 不做 Windows ACL（POSIX 权限 + codex 自身 sandbox 已能覆盖；Windows 部署暂不强制 ACL）。
- 不做 hook payload 的语义翻译层；事件原样落库 + 写日志，前端按需订阅。

## 2. 架构概览

```
┌────────────────┐  JSON-RPC over stdio   ┌────────────────────────────┐
│  backend-rs    │ <─────────────────────>│  codex app-server (per team)│
│  (axum)        │                        │  CODEX_HOME 布局:           │
│                │  POST /hooks/codex     │  ├── teams/{tid}/shared    │
│  HookRouter ───┼──<─────────────────────│  ├── teams/{tid}/members    │
│  Workspace     │                        │  └── users/{uid}/personal   │
│  Auth / Authz  │  Redis event_bus       └────────────────────────────┘
│  Audit         │ ───────────> Redis Stream "codex:events"
└────────────────┘
```

## 3. 数据模型

### 3.1 文件系统布局

所有路径以 `state.codex_home` 为根：

```
$CODEX_HOME/
├── teams/
│   └── {team_id}/
│       ├── shared/                          # team workspace
│       │                                   # owner/admin 可写，member 只读
│       └── members/
│           └── {user_id}/                   # 该成员在 team 内的视图（逻辑位）
├── users/
│   └── {user_id}/
│       └── personal/                        # 个人 workspace（永远可写）
├── config.toml                              # spawn_slot 注入 hooks 配置
└── ...（现有 sessions / auth.json 等不变）
```

### 3.2 数据库表

```sql
-- 用户在团队内的 workspace 角色
CREATE TABLE workspace_role (
    team_id   TEXT NOT NULL,
    user_id   TEXT NOT NULL,
    role      TEXT NOT NULL CHECK (role IN ('owner','admin','member')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (team_id, user_id)
);

-- hook 审计（按 batch flush）
CREATE TABLE workspace_audit (
    id           BIGSERIAL PRIMARY KEY,
    team_id      TEXT,
    user_id      TEXT,
    thread_id    TEXT,
    event_type   TEXT NOT NULL,           -- PreToolUse/PostToolUse/...
    tool_name    TEXT,
    payload_json JSONB NOT NULL,
    decision     TEXT,                    -- allow/deny/ask/none
    ts           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX workspace_audit_team_user_ts ON workspace_audit(team_id, user_id, ts DESC);
```

迁移由 SeaORM 新建 migration：`migration/src/m20260718_000001_workspace.rs`。

### 3.3 关键不变量

- team 创建时即建 `teams/{tid}/shared/`，即便无人也建空目录。
- user 注册时即建 `users/{uid}/personal/`。
- user 加入 team 时建 `teams/{tid}/members/{uid}/` + 写 `workspace_role` 默认 `member`。
- owner/admin 由业务现有 owner/管理员接口维护；本 spec 不重新定义 team 角色，仅新增 `workspace_role` 表。

## 4. Workspace 创建流程

### 4.1 触发点

| 业务事件 | 服务调用 |
|---|---|
| `POST /api/auth/register` | `workspace::ensure_user_personal(user_id)` |
| `POST /api/teams` | `workspace::ensure_team_shared(team_id)` |
| `POST /api/teams/{id}/members` | `workspace::ensure_team_member_view(team_id, user_id)` + `workspace_role.upsert('member')` |
| `DELETE /api/teams/{id}/members/{uid}` | `workspace_role.delete(team_id, user_id)`（目录保留，便于审计） |

### 4.2 实现

- 新模块 `backend-rs/src/services/workspace/mod.rs`
- 所有路径用 `PathBuf::join`，禁止字符串拼接
- 用 `tokio::fs::create_dir_all`（天然幂等）
- 权限设置：Linux/macOS 用 `std::fs::set_permissions` 把 `shared/` 设为 `0o775`；Windows 不做 ACL，依靠 codex sandbox 限制
- 失败语义：`mkdir` 失败必须 fail-fast；调用方返回 500，由调用方重试

### 4.3 幂等保证

- `create_dir_all` 本身幂等
- `workspace_role` 用 `ON CONFLICT (team_id, user_id) DO NOTHING`

## 5. Hook Webhook 路由

### 5.1 路由定义

```
POST /hooks/codex
Headers:
  X-Hook-Token: <INTERNAL_HOOK_TOKEN>   # 启动必填 ≥32 字节
  Content-Type: application/json
Body: HookPayload（见 §5.2）
```

挂在 `backend-rs/src/api/hooks.rs`，不挂 `/api` 前缀，不走 JWT 鉴权中间件，由独立 `INTERNAL_HOOK_TOKEN` 校验。

### 5.2 请求 / 响应契约

请求（codex → backend）：
```json
{
  "hook_event": "PreToolUse",
  "session_id": "thread-xxx",
  "cwd": "/abs/path",
  "tool_name": "shell",
  "tool_input": { "command": "ls" },
  "tool_output": "...",
  "team_id": "tid",
  "user_id": "uid",
  "raw": { /* codex 原始 payload 透传 */ }
}
```

事件类型覆盖（实现顺序与实施时校正以 codex 0.142.5 schema 为准）：
- `PreToolUse` / `PostToolUse`
- `SessionStart` / `SessionEnd`
- `Stop` / `SubagentStop`
- `UserPromptSubmit` / `Notification` / `PreCompact`

响应（backend → codex）：
```json
{
  "continue": true,
  "hookSpecificOutput": {
    "permissionDecision": "allow|deny|ask",
    "updatedInput": { /* 可选，PreToolUse 时改写工具输入 */ }
  }
}
```

### 5.3 分派逻辑

路由函数 `handle_hook(payload) -> HookResponse`：

1. 验签：`X-Hook-Token` header 与启动时加载的 `INTERNAL_HOOK_TOKEN` 常量时间比较；失败返回 401，不审计。
2. 按 `team_id + user_id` 查 `workspace_role`：
   - 查不到 → 当作 `member`（保守默认）
3. 按 `hook_event` 分派：

| 事件 | 行为 |
|---|---|
| `PreToolUse` | 见 §5.4 权限决策 |
| `PostToolUse` | audit 入队 |
| `SessionStart` | `active_rollout.insert(session_id, path)` + audit |
| `SessionEnd` | `active_rollout.remove(session_id)` |
| `Stop` / `SubagentStop` / `UserPromptSubmit` / `Notification` / `PreCompact` | audit |

4. 响应：所有 PreToolUse 返回 `permissionDecision`；其他返回 `continue: true`。
5. 失败语义：内部异常 → 返回 `continue: true`（fail-open）+ `tracing::warn!`，**不阻断 codex**。

### 5.4 PreToolUse 权限决策

```
fn decide(payload) -> Decision:
    let role = workspace_role(payload.team, payload.user);   // owner|admin|member|unknown
    let path = resolve_target_path(payload);                 // tool_input.cwd|file|command

    match payload.tool_name:
        "shell" | "exec_command":
            if path traverses out of CODEX_HOME:        -> deny  (workspace escape)
            if writes to teams/{tid}/shared AND role == member:
                                                      -> deny  (shared readonly for member)
            else                                       -> allow
        "write_file" | "apply_patch" | "edit_file":
            if writes to teams/{tid}/shared AND role == member:
                                                      -> deny
            if writes outside any known workspace:     -> ask
            else                                       -> allow
        "read_file" | "list_dir":
            always                                     -> allow
        _:
            allow
```

`path traverses out of CODEX_HOME` 用 `path.canonicalize().starts_with(codex_home.canonicalize())` 判断，处理 `..` 与符号链接。

## 6. Codex 进程启动配置

### 6.1 spawn_slot 改动

`backend-rs/src/services/multitenant/codex_pool.rs::spawn_slot`：

1. spawn 前确保 `CODEX_HOME/config.toml` 存在，内容：
   ```toml
   # 具体 schema 字段名实施时按 codex 0.142.5 校正
   [hooks.audit]
   type = "http"
   url = "http://127.0.0.1:${PORT}/hooks/codex"
   ```
2. spawn 命令加 `--dangerously-bypass-hook-trust`（自动化不弹首次信任确认）
3. 进程 CODEX_HOME 仍是全局 `state.codex_home`，不变

### 6.2 端口注入

`PORT` 取 `state.http_bind_port`（已在 `main.rs` 启动监听时记录）。本期不实现 turn 启动时 cwd 切换——但 `spawn_slot` 写 config.toml 时已知端口，写入即可。

## 7. 配置与启动

### 7.1 新增环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `INTERNAL_HOOK_TOKEN` | 是 | ≥32 字节，与现有 `INTERNAL_RPC_TOKEN` 同模式，启动校验 |

不设该 env → 启动失败（与 INTERNAL_RPC_TOKEN 一致：避免静默运行 hook）。

## 8. 测试

### 8.1 单元测试 `workspace::*`

- `ensure_user_personal` 重复调用幂等
- `ensure_team_shared` 创建后目录存在
- `ensure_team_member_view` + `workspace_role` 写入成功

### 8.2 集成测试 `tests/hooks_webhook.rs`

用 `tower::ServiceExt::oneshot` 模拟 codex 请求：

- member 写 `teams/{tid}/shared/...` → `deny`
- owner 写 `teams/{tid}/shared/...` → `allow`
- 不带 `X-Hook-Token` → 401
- `shell` 命令 `../escape` → `deny`
- 路由函数内部异常 → `continue: true`
- `PostToolUse` → `workspace_audit` 出现一行

### 8.3 端到端（手测）

启动 backend + codex 进程，触发 `shell` 工具调用；观察：
- webhook 收到 `PreToolUse` + `PostToolUse`
- `workspace_audit` 表有对应记录
- member 用户写 `shared/` 工具调用被 deny

## 9. 错误处理汇总

| 场景 | 行为 |
|---|---|
| workspace mkdir 失败 | 500，调用方重试 |
| hook 验签失败 | 401，不审计 |
| hook 内部异常 | 200 + `continue: true`，tracing::warn |
| codex 启动 hook 配置失败 | spawn 失败 → 进程不入 slot → `client_for` 重建 |
| audit 写 DB 失败 | 重试 3 次，丢队列 tracing::error，不影响 codex |

## 10. 迁移

- 数据库：新表 `workspace_role` / `workspace_audit` 由 SeaORM migration 新建，老库可平滑升级
- 现有部署：第一次跑会按需创建 user/team 目录，旧数据无感
- `INTERNAL_HOOK_TOKEN` 是新增 env；不设启动失败（强制要求）

## 11. 实施拆分（仅作指针，详细 plan 交给 writing-plans）

- M1: workspace 模块 + migration（§3、§4）
- M2: hooks 路由 + 鉴权 + 决策表（§5）
- M3: spawn_slot 写 config.toml + token env（§6、§7）
- M4: 集成测试 + audit batch flush（§8、§9）
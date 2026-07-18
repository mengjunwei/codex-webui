# Codex WebUI 后端架构文档

> 本文档基于 `backend-rs/src/` 全部源码逐模块验证，反映当前代码真实状态。

---

## 1. 项目定位

`backend-rs` 是 Codex WebUI 的 Rust 后端，替代原 NestJS TypeScript 实现。核心职责：

- **多租户 SaaS 平台**：用户注册/登录/团队管理，每人/每团队隔离 workspace
- **Codex app-server 进程池**：per-team codex 进程，共享全局 `CODEX_HOME`，JSON-RPC over stdio
- **多节点 HA 集群**：Redis/Memberlist 探活 + session 级 rollout 增量复制 + 副本晋升
- **Hook Webhook**：codex 调用工具/技能/插件/MCP 时通过 HTTP webhook 回调 backend 做权限校验与审计

---

## 2. 技术栈

| 层 | 技术 |
|---|---|
| HTTP 框架 | axum 0.8 + tower/tower-http |
| 数据库 | SeaORM 1.1（PG + MySQL 多方言）|
| 缓存/消息 | Redis（可选，无则单机模式）|
| 配置 | TOML（`toml` 1.1，无 env 兜底）|
| 认证 | JWT（HS256）+ API key（常量时间比较）|
| 多租户密码 | argon2 |
| 加密存储 | AES-256-GCM（team API key）|
| 进程间通信 | codex app-server JSON-RPC over stdio（`tokio::process`）|
| WebSocket | socketioxide（Socket.IO 协议）|
| 终端 | portable-pty + wezterm-term |
| 成员探活 | memberlist 0.8.5 gossip（feature gate）/ Redis 心跳 |
| 指标 | metrics + Prometheus exporter |
| 链路追踪 | tracing + OpenTelemetry（可选）|

---

## 3. 配置系统

**纯 TOML，无环境变量回退**。配置文件查找顺序：

1. `$CODEX_WEBUI_CONFIG` 精确路径
2. `$CODEX_HOME/config.toml`
3. `./config.toml`
4. `$HOME/.codex-webui/config.toml`

全部找不到 → 启动失败。

### 分块结构

```toml
[server]          # host / port / log_level
[server.api]      # webui_api_key（≥16 字节）
[cluster]         # internal_rpc_host / port / worker_id / worker_rpc_url（可选 enable）
[database]        # driver / host / port / user / password / name / ssl_mode
[redis]           # enable = true/false + host / port / password / db
[codex]           # bin / home（可选 enable）/ openai_api_key（可选 enable）
[auth]            # master_key（可选 enable）/ master_key_previous（可选 enable）
[security]        # internal_rpc_token / internal_hook_token（均 ≥32 字节）
[process_pool]    # max_processes_per_team / max_global / idle_evict_secs / ...
[memberlist]      # enable + seeds / bind
[snapshot]        # interval_secs / root（可选 enable）
[quota]           # default_turn_quota_hourly
[otel]            # enable + endpoint
```

所有可选段/字段都有 `enable = true/false` 开关，默认 `false`。`enable = false` 时字段忽略。

### database driver

```toml
[database]
driver = "postgres"  # 或 "mysql" / "pg" / "mariadb"
```

`url()` 方法根据 driver 拼接连接串。

---

## 4. 代码组织

```
backend-rs/src/
├── main.rs                    # 入口：Config::load → DB → Redis → cluster → codex_pool → HTTP + 内网 RPC
├── config.rs                  # TOML-only 配置，serde 反序列化 + validate()
├── state.rs                   # AppState（Arc 共享，全部 handler 注入）
├── error.rs                   # ErrorCode + AppError + IntoResponse
├── logging.rs                 # tracing 三层（stdout + 滚动文件 + OTLP）
├── lib.rs                     # crate 声明
│
├── api/                       # Handler 层
│   ├── mod.rs                 # build_router()：全部 HTTP 路由组装
│   ├── auth.rs                # POST /api/auth/login（单用户 JWT）
│   ├── health.rs              # GET /api/status / /api/_ping
│   ├── hooks.rs               # POST /hooks/codex（独立路由，X-Hook-Token 鉴权）
│   ├── threads.rs             # REST 代理 → codex JSON-RPC
│   ├── settings.rs            # 运行时设置 CRUD
│   ├── files.rs               # 文件操作（路径安全边界）
│   ├── sqlite.rs              # token-usage / turn-diff / turn-errors / pending-approvals
│   ├── logs.rs                # 日志读取 + 脱敏
│   ├── chat.rs                # 聊天附件上传
│   ├── realtime.rs            # Socket.IO WebSocket 网关
│   ├── proxies.rs             # REST → codex JSON-RPC 代理（account/apps/models/mcp/skills/plugins）
│   ├── onlyoffice.rs          # OnlyOffice Docs 集成
│   ├── event_subscribers.rs   # 事件驱动 DB 写入（token-usage / turn-diff / turn-error / pending）
│   └── multitenant/
│       ├── handlers.rs        # 多租户 HTTP（register/login/refresh + team/thread CRUD）
│       ├── routing.rs         # team → worker 路由（一致性哈希 + Redis/Local）
│       └── internal_rpc.rs    # 内网 RPC server（/internal/* 端点）
│
├── auth/                      # 认证
│   ├── mod.rs                 # AuthService（JWT 签名/校验 + API key 校验）
│   └── middleware.rs          # require_auth 中间件
│
├── codex/                     # Codex 子系统
│   ├── jsonrpc.rs             # JSON-RPC over stdio 客户端（有界写队列 + 请求超时）
│   ├── process.rs             # CodexProcessManager（生命周期管理 + 指数退避重启）
│   └── types.rs               # InitializeParams / InitializeResponse
│
├── db/                        # 数据层
│   ├── migration/             # SeaORM 多方言迁移（9 个，PG/MySQL）
│   ├── entities/mod.rs        # 多租户 entity（8 表：users/teams/team_members/invitations/refresh_tokens/threads/team_api_keys/audit_logs）
│   └── entity.rs              # 业务 entity（5 表：token_usage_snapshots/turn_diffs/settings/pending_server_requests/turn_errors）
│
├── multitenant/               # 中间件层
│   ├── middleware.rs           # require_user_auth（多租户 JWT → UserId 注入）
│   └── mod.rs                 # 工具函数 now_ms / new_id
│
└── services/                  # Service 层
    ├── codex_status.rs        # 聚合就绪状态探针
    ├── codex_status_config.rs # /codex/status + /codex/config REST
    ├── files.rs               # 文件安全路径解析
    ├── settings/              # 运行时设置（12 项，4 分类）
    ├── terminal.rs            # 共享 PTY 会话
    ├── threads.rs             # ThreadResumeRegistry（generation 去重）
    ├── workspace/             # Per-user workspace
    │   ├── mod.rs             # 目录创建 + role 查询（复用 team_members）
    │   ├── decision.rs        # PreToolUse 决策表（路径越界/共享盘权限）
    │   ├── audit_writer.rs    # 批量入库 workspace_audit（50 条/1s）
    │   └── hooks_config.rs    # 写 $CODEX_HOME/config.toml hooks 段
    └── multitenant/
        ├── auth.rs            # 多租户认证（argon2 + JWT mt_access）
        ├── teams.rs           # team CRUD + 成员管理
        ├── api_keys.rs        # BYOK（AES-256-GCM 加密存储）
        ├── audit.rs           # 审计日志（team 操作）
        ├── cluster.rs         # ClusterMembership trait + RedisCluster / MemberlistCluster / SingleCluster
        ├── codex_pool.rs      # TeamCodexManager（per-team 进程池 + LRU 扩缩）
        ├── event_bus.rs       # EventBus trait + InMemoryEventBus / RedisEventBus
        ├── event_persist.rs   # codex 事件落 PG（审批/错误/token 用量）
        ├── pool_policy.rs     # 进程池调度策略（纯逻辑）
        ├── quota.rs           # 计费/配额
        ├── rate_limit.rs      # Redis 固定窗口限流
        ├── replication.rs     # session 副本（主副本分配 + rollout 复制 + 晋升）
        ├── rpc.rs             # 节点间内网 RPC 客户端
        ├── snapshot.rs        # CODEX_HOME 快照（Local + S3）
        └── sticky.rs          # 会话粘性（thread → worker 绑定）
```

---

## 5. 启动流程（main.rs）

```
Config::load()          ← TOML 文件
logging::init()         ← stdout + 滚动文件 + OTLP
DB connect + migrate    ← SeaORM PG/MySQL
reconcile_settings()    ← 12 项运行时设置
Redis connect           ← 可选
EventBus 构造           ← RedisEventBus / 无
codex_home 创建         ← ~/.codex-webui/home 或配置值
cluster 初始化          ← MemberlistCluster / RedisCluster / SingleCluster
TeamCodexManager        ← 进程池 + 空闲回收
CodexProcessManager     ← 单进程（全局 codex app-server）
spawn emit tasks        ← codex 通知 → Socket.IO → 前端
spawn event_persist     ← codex 事件 → PG
AuditWriter             ← hook 审计批量入库
AppState 组装
Redis cluster heartbeat ← 10s 周期 + stale 清理
replica maintenance     ← 15s 周期（续约 + 复制 + 晋升）
内网 RPC server         ← /internal/*（独立端口）
build_router + hooks    ← HTTP + WebSocket + /hooks/codex
listen + graceful shutdown
```

---

## 6. AppState 字段一览

| 字段 | 类型 | 说明 |
|---|---|---|
| `db` | `DatabaseConnection` | SeaORM PG/MySQL |
| `mt_master_key` | `String` | 加密 team API key 的主密钥 |
| `mt_team_codex` | `Arc<TeamCodexManager>` | per-team 进程池 |
| `mt_redis` | `Option<redis::Client>` | Redis（可选）|
| `metrics_handle` | `Option<PrometheusHandle>` | Prometheus 指标 |
| `auth` | `Arc<AuthService>` | JWT + API key |
| `codex` | `Arc<CodexProcessManager>` | 全局 codex 进程 |
| `terminal` | `Arc<TerminalService>` | 共享 PTY |
| `status` | `Arc<CodexStatusService>` | 就绪状态探针 |
| `resume_registry` | `Arc<ThreadResumeRegistry>` | generation 去重 |
| `dynamic_files_roots` | `Arc<Mutex<HashSet<String>>>` | 动态文件根 |
| `settings_cache` | `SettingsCache` | 内存缓存 |
| `codex_home` | `PathBuf` | 全局 CODEX_HOME |
| `node_id` | `String` | 本节点 id |
| `cluster` | `Arc<dyn ClusterMembership>` | 集群探活 |
| `worker_rpc` | `Arc<WorkerRpcClient>` | 节点间 RPC |
| `internal_token` | `String` | 内网 RPC 鉴权 |
| `hook_token` | `String` | Hook webhook 鉴权 |
| `audit_writer` | `AuditWriter` | 审计批量写入 |
| `http_bind_port` | `u16` | 监听端口 |
| `active_rollout` | `Arc<Mutex<HashMap<String, PathBuf>>>` | thread → rollout 路径 |
| `local_offsets` | `Arc<Mutex<HashMap<(String,String), u64>>>` | 进程内 offset fallback |

---

## 7. 认证双轨制

### 单用户模式（/api/*）
- `POST /api/auth/login` → API key 换 JWT（HS256，24h，sub="webui"）
- `require_auth` 中间件：Bearer JWT 或 API key（常量时间比较）
- 查询参数 `?access_token=` 仅在 `/api/files/serve` 和 `/api/files/archive/entry` 有效

### 多租户模式（/api/mt/*）
- `POST /api/mt/auth/register` → 邮箱 + 密码（argon2）→ JWT（HS256，15min，typ="mt_access"）+ refresh token（7d）
- `require_user_auth` 中间件：Bearer mt_access JWT → 注入 `UserId`
- 两套 JWT 用同一 HMAC secret（从 `webui_api_key` 派生），靠 `typ` 字段区分

---

## 8. Per-User Workspace

### 目录布局（全局 CODEX_HOME 下）

```
$CODEX_HOME/
├── users/{user_id}/personal/          个人 workspace（永久可写）
├── teams/{team_id}/shared/            team 共享盘（owner/admin 可写，member 只读）
└── teams/{team_id}/members/{user_id}/ 成员视图目录
```

### 创建时机

| 事件 | 调用 |
|---|---|
| 注册 | `workspace::ensure_user_personal(user_id)` |
| 创建 team | `workspace::ensure_team_shared(team_id)` |
| 加入 team | `workspace::ensure_team_member_view(team_id, user_id)` |

### 角色

复用 `team_members.role`（owner/member），不新建表。`workspace::get_role()` 查 `team_members`，默认 `member`。

---

## 9. Hook Webhook

### 路由

`POST /hooks/codex`（独立路由，不走 `/api`，不走 JWT 中间件）

### 鉴权

`X-Hook-Token` header == `security.internal_hook_token`（常量时间比较，≥32 字节）

### 决策矩阵（PreToolUse）

| 条件 | 决策 |
|---|---|
| 路径含 `..` | Deny |
| 绝对路径不在 CODEX_HOME 内 | Deny |
| 写 `teams/{tid}/shared` + role=member | Deny |
| 其他 | Allow |

路径规范化：`C:\Users\...` 和 `/c/Users/...` 统一为 `/c/Users/...`。

### 其他事件

| 事件 | 行为 |
|---|---|
| PostToolUse | audit 入队 |
| SessionStart | 注册 active_rollout |
| SessionEnd | 清理 active_rollout |
| 其他（Stop/SubagentStop/UserPromptSubmit/Notification/PreCompact）| audit 入队 |

### 失败语义

**fail-open**：任何内部异常 → `{"continue": true}`，不阻断 codex。

### config.toml 注入（toml_edit 精确编辑）

启动 codex 前向 `$CODEX_HOME/config.toml` 注入 `[hooks.audit]` 段（指向本进程 `/hooks/codex`）。
`services/workspace/hooks_config.rs` 用 **`toml_edit`** 解析整个文件后只精确设置 4 个字段
（`type`/`url`/`auth_header`/`auth_env`），其余用户配置（`model`/`model_providers`/注释/空行/格式）
**原样保留**；内容无需变更时跳过写盘（不刷 mtime）。调用点：`codex_pool.rs`（多租户）、
`codex/process.rs`（单租户遗留）。详见 `docs/thread-resume-cache-troubleshooting.md` §3。

---

## 10. 多节点集群

### 架构

所有节点都是 **ingress + worker 一体**（无角色分流）。每个节点同时运行：
- HTTP API（对外）
- 内网 RPC server（`/internal/*`，对其他节点）
- TeamCodexManager（per-team codex 进程池）
- Replica maintenance（主副本续约 + 复制 + 晋升）

### ClusterMembership 三实现

| 实现 | 条件 | 探活机制 |
|---|---|---|
| `MemberlistCluster` | feature `memberlist-backend` + `memberlist_seeds` 非空 | memberlist gossip + Redis 心跳 |
| `RedisCluster` | 有 Redis | Redis SET EX TTL + SMEMBERS + EXISTS 过滤 |
| `SingleCluster` | 无 Redis | 只有自己 |

### Redis 心跳（RedisCluster / MemberlistCluster）

- 每 10s：`SADD cluster:nodes {id}` + `SET cluster:node:{id} rpc_url EX 30`
- 清理 stale：`SMEMBERS` → 逐个 `EXISTS` → 已死 `SREM`
- `alive_nodes()`：`SMEMBERS` + `EXISTS` 过滤

### Rollout 复制

- 主节点 15s 周期扫描 `active_rollout`（thread_id → 文件路径）
- 按 offset 取增量 → POST `/internal/replicate` → 副本 append
- offset 双存储（Redis + 进程内 fallback）
- 晋升后清空 offset → 下次全量同步

### 副本晋升

- 副本自查：主不在 alive 且 lease 过期 → Redis SET NX 抢占 → 晋升
- 孤儿认领：最低 alive id 节点认领无主 team

### thread/resume PG 缓存（thread_resume_cache 表）

`thread/start` 成功后 codex 异步落盘 rollout 文件；前端 create→resume 链路若立即调
`thread/resume` 会撞上落盘 race，codex 返回 `-32600 no rollout found`。解法是
**集群共享 PG 缓存**而非进程内 HashMap：

- `mt_create_thread` 成功后 `put_cached_resume(thread_id, response)` 写 PG
- `mt_invoke_thread` + 内部 RPC `thread_invoke` 调 `thread/resume` 前先 `get_cached_resume`，命中直接返回（不发 codex RPC）
- 真 race（cache miss）时 `-32600` 退避重试 3 次（200/400/600ms）兜底

跨进程共享 → 任意副本转发到 owner，owner 查 PG 命中；进程重启 / failover 后 PG 行仍在，自愈。
详见 `docs/thread-resume-cache-troubleshooting.md` §2。

---

## 11. 进程池调度（TeamCodexManager）

- per-team 多进程（`max_processes_per_team`）
- 全局上限（`max_global_processes`），满则跨 team LRU 回收
- 空闲回收（`idle_evict_secs`），后台 task 周期扫描
- 每进程并发 semaphore（`max_concurrent_per_process`）
- `client_for()` → 选最闲存活 slot → 获取 permit
- failover：spawn 前 CODEX_HOME 不存在则从快照 restore

---

## 12. 数据库 Schema

### 多租户表（8 表）

users / teams / team_members / invitations / refresh_tokens / threads / team_api_keys / audit_logs

### 业务表（5 表）

token_usage_snapshots / turn_diffs / settings / pending_server_requests / turn_errors

### HA 表（2 表）

team_routes / session_replicas

### Workspace 表（1 表）

workspace_audit（hook 审计落库）

### 缓存表（1 表）

thread_resume_cache（thread/resume 响应集群共享缓存，`response` 列为 JSON 类型 —— 全库唯一例外，存 codex 完整结构化响应）

### 类型约定

VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT（不用 JSON/ENUM/ARRAY，跨 PG/MySQL 一致）。
唯一例外：`thread_resume_cache.response` 用 JSON（PG/MySQL 均原生支持，存结构化 codex 响应）。

---

## 13. HTTP 路由表

### 公开路由

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/api/auth/login` | 单用户 API key → JWT |
| POST | `/api/mt/auth/register` | 多租户注册 |
| POST | `/api/mt/auth/login` | 多租户登录 |
| POST | `/api/mt/auth/refresh` | 刷新 token |
| POST | `/api/onlyoffice/callback` | OnlyOffice 保存回调 |
| GET | `/api/docs-json` | OpenAPI JSON |
| GET | `/metrics` | Prometheus 指标 |
| POST | `/hooks/codex` | Hook webhook（X-Hook-Token 鉴权）|

### 受保护路由（require_auth）

全部 `/api/*`（除公开路由外）

### 多租户受保护路由（require_user_auth）

`/api/mt/teams/*`、`/api/mt/threads/*`

### 内网 RPC（internal_token 鉴权）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/internal/thread/start` | 创建会话 |
| POST | `/internal/thread/invoke` | 通用会话方法 |
| POST | `/internal/turn/start` | 发起 turn |
| POST | `/internal/evict` | 踢除 team 进程 |
| POST | `/internal/approval/respond` | 审批响应 |
| POST | `/internal/replicate` | 接收 rollout 增量 |

---

## 14. 环境变量

启动时仅读取以下环境变量用于**配置文件查找**（不是配置值本身）：

| 变量 | 说明 |
|---|---|
| `CODEX_WEBUI_CONFIG` | 配置文件精确路径（最高优先）|
| `CODEX_HOME` | 配置文件查找候选 + 默认 codex_home |
| `HOME` / `USERPROFILE` | 配置文件查找候选 |

所有业务配置值**只从 TOML 文件读取**。

---

## 15. 测试

```bash
# lib 单测
cargo test --lib

# 集成测试
cargo test --tests

# 带 memberlist feature
cargo test --features memberlist-backend
```

当前：70 lib + 6 集成测试全过。

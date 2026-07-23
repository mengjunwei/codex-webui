# LLM 网关集成设计:协议适配 + failover + 多租户 provider 管理

- 日期:2026-07-20
- 分支:feat/multitenant-platform
- 状态:待 review
- 关联:[[permission-hardening]](供应商管理 API 复用其 RBAC 权限点)

## 1. 背景与目标

codex-webui 把 `codex app-server` 当子进程跑,真正调用 LLM 的是 codex CLI。codex 默认走 **OpenAI Responses 协议**(`POST /v1/responses`)。作者主力供应商是 **DeepSeek**,而 DeepSeek **只支持 Chat Completions 协议**(`/v1/chat/completions`),不支持 Responses。因此需要一个协议适配层把 Codex 的 Responses 请求/响应(含 SSE 流)双向转换成 Chat Completions。

外部工具 [cc-switch-cli](https://github.com/saladday/cc-switch-cli) 已经用 ~4600 行 Rust 纯函数实现了这套转换(`transform_codex_chat.rs` + `streaming_codex_chat.rs`),并附带完整单测。本设计把这套能力**移植进 backend-rs**,同时补齐多租户的供应商管理与 failover。

**目标**:
1. codex-webui 后端内置一个 LLM 网关,让 codex 能用 DeepSeek 这类只支持 Chat 协议的供应商(Responses ↔ Chat 自动转换)。
2. 多租户:每个 team / user 管理自己的多个供应商(base_url / model / 协议 / key),一个 active,可 failover。
3. 复用 cc-switch 成熟的协议适配与熔断/failover 实现,不重新发明。
4. 不引入外部进程依赖(取代当前 `start-wsl.sh` 里外挂 cc-switch 守护进程的方式)。

**非目标**(本期不做):
- Anthropic Messages / Gemini 协议适配(只做 Responses ↔ Chat)。
- 用量与成本统计(cc-switch 的 session_usage 体系不搬)。
- GitHub Copilot / Codex OAuth 托管账号(密钥旋转合规风险,后置)。
- cc-switch 的 daemon/supervisor、live 配置文件写入(`~/.claude` 等)、TUI、takeover 环境变量注入——这些是单用户本地场景特有,全部不搬。

## 2. 现状关键认知

### 2.1 codex-webui 后端
- **不打 LLM HTTP**。`codex_pool.rs::spawn_slot` spawn `codex app-server --listen stdio://`,通过 JSON-RPC over stdio 驱动;真正打 LLM 的是 codex 进程。
- **CODEX_HOME 全局共享**(`codex_pool.rs:83` 注释:"全局 CODEX_HOME(所有 team 共用;team 仅前端 UI 隔离,不隔离目录/进程)")。所有 team 的 codex 进程读写同一个 `config.toml`。**team 隔离只靠 env**:每个 team 的 codex 进程独立 spawn,各自 `cmd.env("OPENAI_API_KEY", &plain_key)`(`codex_pool.rs:350`)。
- **BYOK 现状**:`team_api_keys` / `user_api_keys` 两张表,只有 `provider: VARCHAR(32)` + AES-256-GCM 加密的 `encrypted_key` + `key_hint`,**没有 base_url / model / api_format 列**。`api_keys.rs::validate_openai_key(key, base_url)` 已接受 `base_url` 参数,但 `general.modelProviderBaseUrl` 这个 setting 在 `definitions.rs` 里**未定义**(占位 bug),导致永远返回空。
- **`/api/codex/status` 只读**地从 codex config 探出 provider/base_url;写操作已被作者下线(IDOR 风险),注释明确"per-team 配置管理待补 /api/mt/*"。
- **`hooks_config.rs::write_hooks_config`** 是用 `toml_edit` 精确注入 `config.toml` 段的现成模板(保留用户其余配置),网关的 `[model_providers.gateway]` 注入复用它。
- **路由**:`api/mod.rs::build_router` 顶层 Router,受保护路由挂 `require_user_auth`,mt 路由 `nest("/api/mt", ...)`。网关需要独立鉴权层。
- **RBAC**(刚落地):`services/multitenant/permissions.rs` 有 `TeamPermission::{ApiKeyRead, ApiKeyWrite, ...}` + `require_permission`,供应商管理 API 直接挂这两个权限点。

### 2.2 cc-switch-cli 可复用部分
- **`ProxyServer`(`proxy/server.rs`)**:axum HTTP 服务器,只依赖 `Arc<Database>`,已是"服务端友好"形态。
- **协议适配(纯逻辑,零本地依赖,可直接搬)**:
  - `providers/adapter.rs`(ProviderAdapter trait)、`providers/auth.rs`(AuthInfo/AuthStrategy)
  - `providers/codex.rs`(Codex adapter,783 行)
  - `providers/transform_codex_chat.rs`(Responses ↔ Chat 请求体转换,3090 行)← **本期核心**
  - `providers/streaming_codex_chat.rs`(Responses ↔ Chat SSE 流转换,1484 行)← **本期核心**
  - `proxy/sse.rs`(SSE 分块/多字节 UTF-8 工具,118 行)
- **熔断/failover**:`proxy/circuit_breaker.rs`(372 行,纯内存状态机)、`proxy/provider_router.rs` + `proxy/forwarder.rs`(转发重试循环)——需去 `crate::settings` 依赖、改多租户 key、重设计错误分类。
- **Provider 数据模型**:`provider.rs::Provider` 结构体 + SQLite `providers` 表(SSOT,已不写 live 文件)。
- **不搬**:daemon/、services/provider/live.rs、services/proxy.rs(单用户编排层 293KB)、copilot/codex_oauth 认证、Anthropic/Gemini adapter。

> cc-switch 的 `classify_upstream_response` 对几乎所有上游错误(含 401/403/429)都触发 failover。多租户下 401/403 是**该租户 key 失效**,切到队列里别人的 key 是错的——本期必须重设计(见 §4.5)。

## 3. 范围

| 类别 | 本期做 | 本期不做 |
|---|---|---|
| 协议适配 | Responses ↔ Chat Completions(请求 + SSE 响应,双向) | Anthropic Messages、Gemini、 Responses↔Responses 之外 |
| failover | 熔断器 + 多 provider 队列 + 多租户错误分类重设计 | 跨协议 failover(只在同协议 provider 间切) |
| provider 管理 | team / user 两级,多 provider + active + base_url/model/api_format/wire_api | OAuth 托管账号、远端模型拉取、speedtest |
| 网关 | 同进程 axum 路由 `/llm/v1/*`,代理 token 鉴权 | 独立端口、独立进程 |
| 用量统计 | 不做 | — |

## 4. 架构设计

### 4.1 网关位置:backend-rs 同进程路由
在 `build_router` 顶层 Router 新增 `.nest("/llm", gateway_router)`,复用现有 tokio runtime、reqwest、SeaORM 连接池。**不引入新进程、不依赖外部 cc-switch 二进制**。网关路由用独立的"代理 token"中间件鉴权,不走 `require_user_auth`(调用方是 codex 子进程,不是浏览器用户)。

### 4.2 调用链
```
codex-webui spawn codex app-server (per-team 进程, 全局 CODEX_HOME)
  ├─ spawn_slot 注入:
  │    cmd.env("CODEX_HOME", 全局目录)
  │    cmd.env("OPENAI_API_KEY", per-team 代理 token)   ← 改动点(原来是 BYOK 明文 key)
  ├─ write_gateway_config(全局 config.toml):注入 [model_providers.gateway]
  │    base_url = "http://127.0.0.1:{server.port}/llm/v1"
  │    wire_api = "responses"   env_key  = "OPENAI_API_KEY"
  │    (codex 照常发 Responses 协议)
  └─ codex 发 POST /llm/v1/responses  (Authorization: Bearer <代理 token>)
       ↓ gateway 中间件:HMAC 验证 token → 解析 (scope="team", team_id)
       ↓ ProviderRouter: 取该 team 的有序 provider 列表(active 优先 + failover 队列)
       ↓ 对每个 provider:
       │    ├─ 熔断器 allow?(key = "{team_id}:{provider_id}")
       │    ├─ 协议适配(若 provider.api_format != responses):
       │    │     Responses 请求 → Chat 请求(transform_codex_chat)
       │    ├─ reqwest 转发到 provider.base_url/v1/chat/completions
       │    │     Authorization 用 provider 解密后的真实 key(不透传 codex 的代理 token)
       │    ├─ 响应/SSE:Chat → Responses 转换(streaming_codex_chat)回 codex
       │    └─ 错误分类 → failover 或终止(见 §4.5)
       └─ codex 收到 Responses 格式响应,正常跑 agent loop
```

关键性质:**真实供应商 key 永远不进 codex 进程环境**,只在网关内解密后发给上游;codex 只持有一个无意义的代理 token。

### 4.3 多租户请求路由(核心难点)
全局 CODEX_HOME 决定了不能靠 `config.toml` 区分 team。路由靠 **per-team 代理 token**:

- **token 生成**:`tag = HMAC-SHA256(key=internal_rpc_token, msg="{scope}:{scope_id}")`,token = `"{scope}:{scope_id}:{hex(tag)}"`。`internal_rpc_token` 已是现有配置项(`[security]`,≥32 字节)。token **不落库**,任何时候可由 `(scope, scope_id)` 实时重建。
- **token 注入**:`codex_pool.rs::spawn_slot` 把 `OPENAI_API_KEY` 的值从 BYOK 明文 key 改为该 token(personal workspace 用 `scope=user`)。
- **网关验证**:中间件解析 token → 用本进程 `internal_rpc_token` 重算 HMAC 比对 → 得 `(scope, scope_id)` → 放入请求扩展供 handler 取用。
- **key 来源切换**:网关不再从 `Authorization` 拿真实 key 发上游,而是从 `team_providers`/`user_providers` 表解密 active provider 的 key。

> 安全边界:网关只监听本机(codex → 127.0.0.1),代理 token 伪造需持有 `internal_rpc_token`(已是平台根密钥级)。风险可接受。token 旋转随 `internal_rpc_token` 旋转(已有轮转机制)。

### 4.4 协议适配层移植
新增 crate 内模块 `backend-rs/src/llm/`(或 `services/llm_gateway/`),从 cc-switch 整块搬入并改 crate 路径:
- `llm/adapter.rs`(ProviderAdapter trait)
- `llm/auth.rs`(AuthInfo/AuthStrategy,裁掉 Copilot/CodexOAuth 分支)
- `llm/codex_adapter.rs`
- `llm/transform_codex_chat.rs`(请求体 Responses↔Chat)
- `llm/streaming_codex_chat.rs`(SSE Responses↔Chat)
- `llm/sse.rs`(SSE 工具)
- 对应 `tests.rs` **必须一起搬**(协议转换在 tool_use / reasoning / 跨 chunk UTF-8 等边界极易回归)。

这些是纯函数:`fn transform(body: serde_json::Value) -> Result<Value>`、`fn create_chat_sse_stream(resp) -> impl Stream`,输入输出与 cc-switch 基础设施无关,移植即编译。唯一改造:错误类型从 cc-switch 的 `ProxyError` 换成 backend-rs 的 `AppError`。

### 4.5 failover + 熔断(多租户重设计)
搬入 `circuit_breaker.rs`(纯内存状态机,几乎不改),重写 `ProviderRouter` + `forwarder` 核心循环:

- **熔断器 key**:`{scope_id}:{provider_id}`(带租户前缀,各租户熔断互不影响)。状态 Closed→Open(连续失败达阈值)→HalfOpen(超时后放一个探测)。
- **错误分类重设计**(关键修正):
  | 上游情况 | 分类 | 行为 |
  |---|---|---|
  | 401 / 403 | `FatalStop` | 该租户 key 失效,**直接 4xx 回 codex**,不 failover、不耗熔断额度 |
  | 429 / 5xx | `ProviderFailure` | 记熔断失败,**failover 到下一个 provider** |
  | 连接错误 / 超时 | `ProviderFailure` | 同 provider 内按 `max_retries` 重试,耗尽后 failover |
  | 400 / 422 | `NeutralRelease` | 请求格式问题(可能是协议转换 bug),不 failover、回原错误 + 告警 |
  | 网关内部错误(DB 等) | `FatalStop` | 500 回 codex |
- **provider 列表顺序**:active provider 在前,其后是 `failover_order` 排序的队列;熔断 Open 的跳过。

### 4.6 codex 集成(config.toml + env + evict)
- **`write_gateway_config(codex_home, port)`**(新函数,仿 `write_hooks_config`):用 `toml_edit` 在全局 `config.toml` 注入 `[model_providers.gateway]` + 设 `model_provider = "gateway"`。保留用户其余配置。内容未变则跳过写盘。因 CODEX_HOME 全局共享,所有 team 共用这一份网关配置(正合需要)。
- **spawn_slot 改 env**:`OPENAI_API_KEY` 注入代理 token(§4.3)。
- **切换 active provider 后**:`restart_team(team_id, ...)`(`codex_pool.rs:159` 已有)让 codex 进程重启读新状态——但实际上 provider 选择在网关侧(不在 codex config),所以**切换 provider 不必重启 codex**,只需让网关下次请求读新 active(网关每次请求实时查 DB)。这是网关模型相对"写 config.toml"模型的一个优势:切换生效零延迟、无进程重启。`restart_team` 仅在 token 旋转等场景用。

## 5. 数据模型

新增两张表(走 SeaORM migration,参考 `m20260720_000001_rbac_permissions` 风格):

### team_providers
| 列 | 类型 | 说明 |
|---|---|---|
| id | VARCHAR PK | uuid |
| team_id | VARCHAR → teams.id | 所属 team |
| name | VARCHAR | 展示名(如 "DeepSeek 主号") |
| base_url | VARCHAR | 真实上游(如 `https://api.deepseek.com/v1`) |
| model | VARCHAR | 默认模型(如 `deepseek-chat`);网关转发时用它覆盖 codex 请求里的 model,支持 per-team 不同模型 |
| api_format | VARCHAR CHECK IN ('responses','chat') | 上游协议;决定是否转换。DeepSeek='chat' |
| encrypted_key | TEXT | AES-256-GCM(复用 `api_keys.rs::encrypt_key`) |
| key_hint | VARCHAR | 尾 4 位 |
| is_active | BOOL | 该 team 当前是否选中(每 team 至多一个 true) |
| failover_order | INT NULL | failover 队列序号(NULL=不参与 failover) |
| sort_index | INT | UI 排序 |
| created_at / updated_at | BIGINT | now_ms |

唯一约束:`(team_id, is_active)` 至多一行(用事务保证,跨方言一致,仿 `set_team_api_key`)。

### user_providers
同结构,`team_id` 换成 `user_id → users.id`,服务 personal workspace。

> 旧 `team_api_keys` / `user_api_keys`:网关模式下不再被 codex 直接消费(改由 `team_providers` 喂网关)。保留表用于兼容/迁移;提供一次性迁移脚本(把旧 active key 迁成一条 `api_format` 由 provider 字段推断的 provider 行)。

## 6. API 设计

### 6.1 管理面(受 `require_user_auth` + RBAC)
挂在 `mt_protected` 路由:
```
GET    /api/mt/teams/{teamId}/providers          列出 team 全部 provider   [ApiKeyRead]
POST   /api/mt/teams/{teamId}/providers          新增 provider             [ApiKeyWrite]
PATCH  /api/mt/teams/{teamId}/providers/{id}     改名/base_url/model/key 等 [ApiKeyWrite]
DELETE /api/mt/teams/{teamId}/providers/{id}     删除                      [ApiKeyWrite]
POST   /api/mt/teams/{teamId}/providers/{id}/activate  设为 active          [ApiKeyWrite]
GET    /api/mt/user/providers                    / POST / PATCH / DELETE / activate  (personal,同上)
```
- key 字段写入前用 `validate_openai_key(key, Some(base_url))` 校验(best-effort,DeepSeek 等自定义端点失败允许写,记 warn——沿用现有策略)。
- OpenAPI schema 注册到 `ApiDoc`,前端走 `pnpm generate:api` 自动生成 client。
- 权限点复用 [[permission-hardening]] 的 `ApiKeyRead/Write`(owner/admin 有,member 看 plan)。

### 6.2 网关面(独立代理 token 中间件)
```
POST /llm/v1/responses        codex 的 Responses 入口 → 转 Chat 发上游(若 api_format=chat)
POST /llm/v1/chat/completions 透传(若上游本身就是 chat 协议,可不走转换)
GET  /llm/health              网关健康/熔断状态(运维用)
```
中间件 `require_gateway_token`:解析 `Authorization: Bearer <token>` → HMAC 验证 → 注入 `(scope, scope_id)`。验证失败 401。

## 7. 前端
- **`team-settings.tsx`**(当前 provider 写死 `'openai'`):改成 provider 列表 UI——展示 name/base_url/model/api_format/key_hint,新增/编辑/删除/设 active 表单。key 输入框写入后只回显 hint。
- **`account/account-settings.tsx`**(当前 3 选项无 base_url/model):同样扩成 user provider 列表。
- **`codex-settings.tsx`**:展示网关状态(active provider、协议、熔断),只读即可。
- API client 由 OpenAPI 自动生成,无需手写。

## 8. 安全考量
- 代理 token = HMAC(internal_rpc_token, scope+id),无状态、可旋转、不落库。
- 真实供应商 key 仅以 AES-256-GCM 密文存表,网关内解密后发给上游,**不进 codex 进程环境、不进日志**。
- failover 不跨租户:401/403 直接报错(§4.5),杜绝"用 A 的 key 失败切到 B 的 key"。
- 网关日志脱敏:`sanitize_url` 复用现有逻辑,Authorization header 不记录。
- 网关只监听 127.0.0.1(codex 本机回环),不对外暴露。

## 9. 测试策略
1. **协议适配单测**:搬 cc-switch `transform_codex_chat` / `streaming_codex_chat` 的全部单测,移植后原样跑通(tool_use 嵌套、reasoning content、多字节 UTF-8 跨 chunk 等)。
2. **熔断器单测**:Closed/Open/HalfOpen 状态迁移。
3. **多租户路由单测**:token 生成/验证、错误 token 拒绝、不同 scope 隔离。
4. **failover 分类单测**:401→FatalStop、500→ProviderFailure、400→NeutralRelease,各覆盖。
5. **端到端**:真实 DeepSeek key 跑一个 codex turn,验证 Responses→Chat→DeepSeek→Chat→Responses 全链路通。

## 10. 移植清单(从 cc-switch)
| cc-switch 文件 | 去向 | 改造 |
|---|---|---|
| `providers/adapter.rs` | `llm/adapter.rs` | 改 crate 路径 |
| `providers/auth.rs` | `llm/auth.rs` | 裁掉 Copilot/CodexOAuth 分支 |
| `providers/codex.rs` | `llm/codex_adapter.rs` | 改 Provider 结构体引用 |
| `providers/transform_codex_chat.rs` | `llm/transform_codex_chat.rs` | ProxyError→AppError |
| `providers/streaming_codex_chat.rs` | `llm/streaming_codex_chat.rs` | 同上 |
| `proxy/sse.rs` | `llm/sse.rs` | 原样 |
| `proxy/circuit_breaker.rs` | `llm/circuit_breaker.rs` | 原样(纯内存) |
| `proxy/forwarder.rs` 核心循环 | `llm/forwarder.rs` | 重写:去 settings 依赖、多租户 key、错误分类 |
| `proxy/provider_router.rs` | `llm/provider_router.rs` | 重写:从 SeaORM 查 provider、去 settings |
| `provider.rs::Provider` 结构 | `llm/types.rs` | 裁 ProviderMeta 到本期所需字段 |

## 11. 工作量与分期建议
建议分三个 phase(每个 phase 可独立合并、可验证):
- **Phase 1 — provider 管理(数据 + API + UI)**:新建 `team_providers`/`user_providers` + migration + entity + CRUD API(挂 RBAC)+ 前端 provider 列表 UI + 修 `general.modelProviderBaseUrl` 占位 bug。~1 周。此阶段网关尚未接管,codex 仍走旧 BYOK(可先让 UI 能管 provider,网关下一步接管)。
- **Phase 2 — 网关 + 协议适配 + 多租户路由**:移植协议适配层(带测试)+ 网关 axum 路由 + 代理 token 中间件 + `write_gateway_config` + spawn_slot 改 env 注入 token。端到端跑通 DeepSeek。~1.5–2 周。
- **Phase 3 — failover + 熔断**:搬熔断器 + 重写 router/forwarder + 错误分类重设计 + 多 provider 队列 UI。~1 周。

## 12. 风险与开放问题
1. **codex 原生 `wire_api = "chat"` 替代方案**:codex config.toml 的 `model_providers` 支持 `wire_api = "chat"`,理论上能让 codex 直接发 Chat 协议、绕过 DeepSeek 协议不匹配,不必移植 4600 行转换。cc-switch 之所以写了这么多,是因 codex 原生 chat 模式在 reasoning/tool_calls/SSE 边界 case 兼容不足。**开放问题**:Phase 2 动手前,先用 30 分钟验证"codex 原生 wire_api=chat + DeepSeek"能否跑通你的典型 turn;若能,Phase 2 可大幅缩减(只做 provider 管理 + 透传网关,不搬协议层)。已选协议适配路线,此处仅作风险记录。
2. **全局 CODEX_HOME 并发写**:`write_gateway_config` 可能被多个 team 的 spawn 并发触发。需保证幂等(内容未变跳过)+ 原子写(临时文件 + rename,仿 cc-switch)。多 team 写同一份网关配置是幂等的(内容相同),无冲突。
3. **旧 BYOK 兼容**:迁移期间 `team_api_keys`(旧)与 `team_providers`(新)并存。codex env 注入切换到代理 token 后,旧表不再消费;需明确迁移窗口与回滚路径。
4. **协议转换回归**:4600 行转换 + SSE 状态机,边界 case 多。必须带单测移植,且 Phase 2 端到端用真实 DeepSeek 覆盖一次完整 turn(含 tool_use)。
5. **token 与 `internal_rpc_token` 绑定**:该密钥旋转时,所有在跑的 codex 进程持有的旧 token 失效,需 `restart_team` 全量刷新。文档注明。

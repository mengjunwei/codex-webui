# codex 单进程多 thread 重构设计

- 日期：2026-07-21
- 分支：`feat/multitenant-platform`
- 状态：设计阶段，待 review

## 1. 背景与问题

当前 codex 进程模型是 **per-team 进程池**（`TeamCodexManager` / `codex_pool.rs`）：每个活跃 team 起一个独立 codex app-server 进程，`max_global_processes=25` 硬上限，`idle_evict_secs=900` 回收。

问题：
- **不扩展**：1000 team 场景，活跃 team 受 25 进程上限限制，超出的排队；每个进程是 node.js（几十~上百 MB），25 个 = GB 级内存。
- **过度设计**：codex app-server **原生支持单进程多 thread**（所有会话方法带 `threadId`，单进程按 thread_id 持有多个会话）。per-team 进程是把"按 team 隔离会话"错误理解成"按 team 起进程"——会话隔离本就靠 thread_id。
- **复杂度高**：进程池 + LRU + evict + max_global + 跨进程复制，~520 行调度逻辑。

## 2. 目标

重构为 **单进程多 thread**：全局一个 codex app-server 进程，所有 team/thread 通过 `thread_id` 复用。删掉 per-team 进程池。

BYOK key 走**统一本地代理**（已决定方案 A：所有 team 共享代理，代理管 key，webui 不 per-team 注入 key）。

## 3. 设计总览

```
重构前:                              重构后:
handler → mt_team_codex              handler → state.codex
         .client_for(team)                    .request(method, params)  ← 单进程
         .client().request(M,P)                                          ↓
                    ↓                                          全局 1 个 codex app-server
              per-team codex 进程(≤25)         (所有 thread 复用,thread_id 路由)
```

**关键事实**（Explore 确认）：
- webui `threads.id`(PG) == codex thread id（同一个 UUID，codex `thread/start` 生成）。turn/delete/resume 都带它。
- `state.codex`（`CodexProcessManager` 单进程）**已存在但闲置**（只供事件）。所有 thread/turn 走 `state.mt_team_codex`。
- `CODEX_HOME` 本就全局唯一（从未 per-team），rollout 按 thread_id 隔离（文件名）。`replication.rs` 不用动。

**重构核心** = handler 改调 `state.codex`，删 `state.mt_team_codex`。

## 4. 详细设计

### 4.1 删除（per-team 进程池整套）

- `services/multitenant/codex_pool.rs` 整文件（`TeamCodexManager`/`PoolConfig`/`ClientLease`/`ProcessSlot`/LRU/evict/idle_reaper，~520 行）
- `services/multitenant/mod.rs` 的 `pub mod codex_pool;`
- `config.rs` 的 `ProcessPoolConfig` 结构 + Default + 5 个默认函数 + Config 字段 + Debug 字段 + 测试
- `config.toml.example` 的 `[process_pool]` 段
- `state.rs` 的 `mt_team_codex` 字段 + import
- `main.rs` 的池构造（PoolConfig::new / TeamCodexManager::new / start_idle_reaper）
- `internal_rpc.rs` 的 `/internal/evict` 路由 + `rpc.rs` 的 `WorkerRpcClient::evict`（单进程无需驱逐）

### 4.2 改造（handler 改调单进程）

所有 `state.mt_team_codex.client_for(team, db, &master_key, is_personal).await?.client().request(M, P)` → `state.codex.request(M, Some(P)).await`（`CodexProcessManager::request` 已代理当前 client）。

| handler | 位置 | 改造 |
|---|---|---|
| mt_create_thread | handlers.rs:561-573 | `state.codex.request("thread/start", Some(rest))`；删 pool_team_id/is_personal/master_key 参数；保留 cwd 注入 |
| mt_start_turn | handlers.rs:824-833 | `state.codex.request("turn/start", Some(params))` |
| mt_invoke_thread | handlers.rs:1222-1229 | `state.codex.request(&body.method, Some(params))`；保留 -32600 重试 + cache_fallback |
| mt_delete_thread | handlers.rs:759-769 | `state.codex.request("thread/delete", Some({threadId}))` |
| mt_resolve_approval | handlers.rs:1006-1024 | `state.codex.client().await?.respond_to_server_request(...)` |
| set_team_api_key | handlers.rs:395 | 删 `.evict()`（统一代理管 key，DB 更新即可） |
| internal_rpc × 5 | internal_rpc.rs:103-283 | 同上 5 个转发 handler |
| promote_resume_team | main.rs:434-461 | `client_for` → `state.codex.client()`；预热 spawn 变空操作 |

### 4.3 新增：全局并发信号量

`CodexProcessManager` 加一个全局 `Semaphore`（`max_concurrent`，建议 32），`request()` 前 acquire。防单 stdin/stdout 管道过载（替代原 max_concurrent_per_process，但现在是系统级）。

config 加 `[codex] max_concurrent = 32`（或复用某字段）。

### 4.4 事件路径（保留 HA）

当前两路：
- (i) `state.codex` → realtime/event_subscribers 直接广播
- (ii) `state.mt_team_codex` → EventBus → event_persist + realtime 跨节点 fan-out

重构后：
- (i) 成为唯一执行路径（state.codex 现在实际跑 thread/turn，不再闲置）
- (ii) EventBus/event_persist **保留**（多节点 HA 跨节点 fan-out 仍需要），事件源从 state.codex 的 notification/server_request/lifecycle 通道接

### 4.5 不变

- `codex/jsonrpc.rs`（通用 request/notify/respond）
- `codex/process.rs` 的 `CodexProcessManager`（直接复用，仅加信号量）
- `services/multitenant/replication.rs`（单 CODEX_HOME + thread_id rollout）
- PG `threads` 表（id = codex thread id）
- `resume_cache` / `sticky` / `quota` / `audit` / `permissions`
- `api_keys.rs`（BYOK 加密存储保留，set_team_api_key/set_user_api_key 仍写 DB；只是 codex 不再读 per-team key）

## 5. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 单点故障（codex 崩溃影响所有 team） | CodexProcessManager 内置自动重启（3→60s 指数退避）；in-flight turn 丢失（30s 超时） |
| 并发瓶颈（单管道） | 全局信号量 max_concurrent=32 + WRITE_QUEUE_CAP=1024 兜底 |
| 线程内存积累（无 idle evict） | 后续加 thread 级驱逐（本重构先不做，监控 codex 内存） |
| BYOK per-team key 丢失 | 统一代理管 key（方案 A，已确认） |
| set_team_api_key 不即时生效 | 统一代理场景 team_api_keys 基本不用；需即时则重启 |

## 6. 验收

1. 1000 team 只有 1 个 codex 进程（`ps` 确认）
2. 发 turn 正常（codex 用 .codex 代理调 LLM，响应回前端）
3. 多 team 并发 turn 复用单进程（thread_id 隔离）
4. codex 崩溃后自动重启，新 turn 恢复
5. cargo build/test 全绿；删 ~520 行 codex_pool + 配置

## 7. 非目标

- thread 级内存驱逐（后续）
- 多节点 HA 深度改造（EventBus 保留，不在本次）
- per-team BYOK key 恢复（等 codex 支持 per-thread key 或保持统一代理）

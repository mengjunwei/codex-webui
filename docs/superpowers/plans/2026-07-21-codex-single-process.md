# codex 单进程多 thread 重构 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** 把 codex 从 per-team 进程池重构为 **per-node 单进程多 thread**——每节点一个 codex 进程（多节点 HA 下 N 节点 = N 进程 + sticky 分散 + failover），删 `codex_pool.rs`(~520 行)，所有 handler 改调 `state.codex`，加 per-node 并发信号量。

**Architecture:** `state.codex`(CodexProcessManager 单进程) 闲置→启用为唯一执行路径；删 `state.mt_team_codex`(TeamCodexManager per-team 池)。webui thread.id == codex thread id，CODEX_HOME 本就全局，rollout/jsonrpc/replication/权限不动。

**Tech Stack:** Rust 2024 / axum / tokio Semaphore。

## Global Constraints

- 中文注释。
- `cargo build` / `cargo test`（`backend-rs/`）零错误全绿。
- 改调用点必须编译通过（Task 2 改完 mt_team_codex 无引用，Task 3 才安全删）。
- 不改 jsonrpc.rs / replication.rs / PG threads / 权限 / codex_home。
- BYOK 走统一代理（不 per-team 注入 key）。
- spec：`docs/superpowers/specs/2026-07-21-codex-single-process-design.md`；Explore 详细清单见 task brief。

---

### Task 1: CodexProcessManager 加全局并发信号量

**Files:** `backend-rs/src/codex/process.rs`、`backend-rs/src/config.rs`

- [ ] **Step 1: config 加 max_concurrent**

`config.rs` 的 `CodexConfig` 加字段（替代删掉的 ProcessPoolConfig 的并发控制）：
```rust
pub struct CodexConfig {
    #[serde(default = "default_codex_bin")]
    pub bin: String,
    #[serde(default)]
    pub home: CodexHomeConfig,
    #[serde(default)]
    pub openai_api_key: CodexOpenaiConfig,
    /// 单进程全局并发上限(单 stdin/stdout 管道防过载;默认 32)。
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}
fn default_max_concurrent() -> usize { 32 }
```
加访问器 `cfg.codex_max_concurrent() -> usize`。

- [ ] **Step 2: CodexProcessManager 加信号量**

`process.rs` 的 `CodexProcessManager` 加 `concurrency: Arc<Semaphore>` 字段，`new()` 接收 max_concurrent 初始化。`request()` 前 `acquire()`：
```rust
use tokio::sync::Semaphore;
pub struct CodexProcessManager {
    codex_bin: String,
    codex_home: Option<String>,
    current: Mutex<Option<(u64, Arc<CodexJsonRpcClient>)>>,
    // ... 既有字段
    concurrency: Arc<Semaphore>,  // 新增:全局并发限制
}
// new(bin, home, max_concurrent) → concurrency = Arc::new(Semaphore::new(max_concurrent))
pub async fn request(&self, method, params) -> Result<Value, RpcError> {
    let _permit = self.concurrency.acquire().await.map_err(...)?;
    match self.client().await { Some(c) => c.request(method, params).await, None => Err(...) }
}
```
main.rs 构造 CodexProcessManager 时传 `cfg.codex_max_concurrent()`。

- [ ] **Step 3: 编译**

Run（`backend-rs/`）: `cargo build`，零错误。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/codex/process.rs backend-rs/src/config.rs backend-rs/src/main.rs
git commit -m "feat(codex): CodexProcessManager 加全局并发信号量(max_concurrent)"
```

---

### Task 2: 改所有 codex 调用点 → state.codex

**Files:** `handlers.rs`、`internal_rpc.rs`、`main.rs`(promote_resume_team)

把 `state.mt_team_codex.client_for(team, db, &master_key, is_personal).await?.client().request(M, P)` 全部改成 `state.codex.request(M, Some(P)).await`（approval 用 `state.codex.client().await?.respond_to_server_request(...)`）。**本 task 只改引用，不删 codex_pool**（Task 3 删），保证编译通过。

- [ ] **Step 1: handlers.rs 6 处**

| handler | 行 | 改造 |
|---|---|---|
| mt_create_thread | 561-573 | `let resp = state.codex.request("thread/start", Some(Value::Object(rest))).await.map_err(...)?;`（删 lease/client_for/pool_team_id/is_personal/master_key；保留 cwd 注入 :550-555） |
| mt_start_turn | 824-833 | `state.codex.request("turn/start", Some(params)).await`（删 lease/client_for；保留 threadId 注入 :821-823） |
| mt_invoke_thread | 1222-1229 | `state.codex.request(&body.method, Some(params.clone())).await`（删 lease/client_for；保留 -32600 重试 + cache_fallback） |
| mt_delete_thread | 759-769 | `if let Err(e) = state.codex.request("thread/delete", Some(json!({"threadId": thread_id}))).await { warn!(...) }` |
| mt_resolve_approval | 1006-1024 | `if let Some(client) = state.codex.client().await { client.respond_to_server_request(...) }` |
| set_team_api_key | 395 | **删** `state.mt_team_codex.evict(&team_id).await;`（统一代理管 key，删 evict） |

- [ ] **Step 2: internal_rpc.rs 5 处转发**

| handler | 行 | 改造 |
|---|---|---|
| thread_start | 109-110 | `state.codex.request("thread/start", ...)` |
| turn_start | 145-146 | `state.codex.request("turn/start", ...)` |
| approval_respond | 195-196 | `state.codex.client().await?.respond_to_server_request(...)` |
| thread_invoke | 252-253 | `state.codex.request(...)` |
| evict | 164 | **删** `.evict()` 调用（保留空 handler 或删路由，Task 3 清理） |

- [ ] **Step 3: main.rs promote_resume_team (:434-461)**

两处 `state.mt_team_codex.client_for(team_id,...)` → `state.codex.client().await`（预热 spawn :439 变空操作/删；per-thread resume 循环 :449 用 `state.codex`）。

- [ ] **Step 4: 编译**

Run: `cargo build`。**预期 codex_pool/mt_team_codex 出现 unused warning**（无引用了，Task 3 删）。零错误。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs backend-rs/src/api/multitenant/internal_rpc.rs backend-rs/src/main.rs
git commit -m "refactor(codex): 所有 handler 改调 state.codex 单进程(删 client_for/evict 调用)"
```

---

### Task 3: 删 codex_pool + per-team 进程池配置

**Files:** 删 `codex_pool.rs`、改 `mod.rs`/`config.rs`/`state.rs`/`main.rs`/`config.toml.example`/`internal_rpc.rs`/`rpc.rs`

- [ ] **Step 1: 删 codex_pool.rs + mod 引用**

`git rm backend-rs/src/services/multitenant/codex_pool.rs`；`services/multitenant/mod.rs` 删 `pub mod codex_pool;`。

- [ ] **Step 2: state.rs 删 mt_team_codex**

删 `pub mt_team_codex: Arc<TeamCodexManager>` 字段 + import。

- [ ] **Step 3: config.rs 删 ProcessPoolConfig**

删 `ProcessPoolConfig` struct + Default + 5 个 default 函数 + Config.process_pool 字段 + Debug 的 .field("process_pool") + 相关测试。

- [ ] **Step 4: main.rs 删池构造**

删 PoolConfig import、PoolConfig::new、TeamCodexManager::new、start_idle_reaper、AppState 的 mt_team_codex 初始化。

- [ ] **Step 5: 删 /internal/evict 路由 + WorkerRpcClient::evict**

`internal_rpc.rs` 删 evict handler + 路由注册；`rpc.rs` 删 WorkerRpcClient::evict 方法（单进程无需驱逐）。

- [ ] **Step 6: config.toml.example 删 [process_pool]**

删 `[process_pool]` 段（:98-108）。

- [ ] **Step 7: 编译 + 全量测试**

Run: `cargo build && cargo test`。零错误全绿。

- [ ] **Step 8: Commit**

```bash
git add -A backend-rs/
git commit -m "refactor(codex): 删除 per-team 进程池(codex_pool/ProcessPoolConfig/mt_team_codex/idle_reaper)"
```

---

### Task 4: 编译验证 + 重启后端 + 发 turn 端到端验证

- [ ] **Step 1: 全量编译 + 测试**

Run: `cargo build && cargo test`，零错误全绿。

- [ ] **Step 2: 重启后端（用新代码 + .codex 代理 config）**

停旧后端 → `CODEX_WEBUI_CONFIG="D:/code/rust/codex-webui/config.toml" cargo run --manifest-path backend-rs/Cargo.toml`（后台）。

- [ ] **Step 3: 发 turn 端到端验证**

login admin → 创建 team + thread → 发 turn（POST /turns）→ 等 codex 响应 → 确认前端收到（或 API 返回 turn 结果，不再超时）。
确认 **per-node 1 个 codex 进程**（`tasklist | grep codex` 或 ps，每节点一个；无 per-team 的多个进程）。

- [ ] **Step 4: Commit（如有验证修复）**

验证通过则重构完成。

---

## Self-Review 结果

**1. Spec 覆盖**：Task 1(信号量) + Task 2(改调用点) + Task 3(删池) + Task 4(验证) 覆盖 spec §4.1-4.4。✅
**2. 顺序安全**：Task 2 改引用（mt_team_codex 变 unused 但不删）→ Task 3 删（无引用安全）。每步编译通过。✅
**3. 不变确认**：jsonrpc/replication/PG threads/权限/codex_home 不动。✅
**4. 风险**：单点故障(自动重启) + 并发(信号量) + BYOK(统一代理)，spec §5 已记。

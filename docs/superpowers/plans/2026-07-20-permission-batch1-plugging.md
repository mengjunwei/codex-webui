# 权限加固 批次1：P0 安全堵漏 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 堵住 P0 WebSocket 跨租户越权（IDOR），把内网 RPC 鉴权提升为 layer 强制，清理死代码。

**Architecture:** 三处独立改动——(1) WebSocket `on_connect` 用多租户 access JWT 提取 user_id 并存入 socket 映射，`on_thread_subscribe` 调 `require_thread_team` 校验后才加入房间；(2) 内网 RPC 把 `require_internal_token` 从每 handler 手调改为整层 layer；(3) 删除无引用的 `auth::middleware::require_auth`。

**Tech Stack:** Rust 2024 / axum 0.8 / socketioxide 0.16 / SeaORM 1.1 / jsonwebtoken 9。

## Global Constraints

- 中文注释（项目惯例）。
- 测试约束：项目**无 DB 集成测试设施**（现有测试均为纯单测，dev-deps 仅 `tower` + `http-body-util`）。本批对**纯逻辑**（token 校验、JWT 验签）做严格 TDD；依赖 DB/WS 的 handler 改动用**编译 + 手动验证清单**保证，DB 回归测试在批次4统一搭建。
- 频繁提交：每个任务结束 commit。
- 编译必须通过：`cargo build`（每个任务验证步骤）。

---

### Task 1: 内网 RPC 鉴权提升为 layer

**Files:**
- Modify: `backend-rs/src/api/multitenant/internal_rpc.rs`（全文相关部分）

**Interfaces:**
- Consumes: `AppState.internal_token: String`（已存在，`internal_rpc.rs:52`）
- Produces: `pub async fn require_internal_token_layer`（axum middleware）；`fn check_internal_token(expected, headers) -> Result<(), AppError>`（纯函数，可单测）；`build_internal_router` 改为挂 layer。后续 6 个 `/internal/*` handler 删除手动 `require_internal_token` 调用与 `headers` 参数。

- [ ] **Step 1: 写失败测试 — check_internal_token 纯函数**

在 `backend-rs/src/api/multitenant/internal_rpc.rs` 文件末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn mk_headers(token: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert("x-internal-token", t.parse().unwrap());
        }
        h
    }

    const EXPECTED: &[u8] = b"0123456789abcdef0123456789abcdef"; // 32 字节

    #[test]
    fn check_internal_token_correct_passes() {
        let h = mk_headers(Some(std::str::from_utf8(EXPECTED).unwrap()));
        assert!(check_internal_token(EXPECTED, &h).is_ok());
    }

    #[test]
    fn check_internal_token_wrong_rejected() {
        // 等长但内容不同,验证不是仅比长度
        let h = mk_headers(Some("9999456789abcdef0123456789abcdef"));
        assert!(check_internal_token(EXPECTED, &h).is_err());
    }

    #[test]
    fn check_internal_token_missing_header_rejected() {
        let h = mk_headers(None);
        assert!(check_internal_token(EXPECTED, &h).is_err());
    }

    #[test]
    fn check_internal_token_empty_expected_rejected() {
        let h = mk_headers(Some("anything"));
        assert!(check_internal_token(b"", &h).is_err());
    }
}
```

- [ ] **Step 2: 运行测试确认失败（函数未定义）**

Run: `cargo test -p codex-webui --lib api::multitenant::internal_rpc::tests`
Expected: 编译失败，`check_internal_token` 未定义。

- [ ] **Step 3: 实现 check_internal_token + layer，重构 router/handlers**

在 `internal_rpc.rs` 中，把现有 `require_internal_token` 函数体替换为纯函数 `check_internal_token`，并新增 layer；改造 router 与各 handler。具体改动：

**3a.** 把 `require_internal_token`（`internal_rpc.rs:48-76`）替换为：

```rust
/// 纯函数:校验 x-internal-token(恒定时间比较)。供 layer 与单测复用。
fn check_internal_token(expected: &[u8], headers: &axum::http::HeaderMap) -> Result<(), AppError> {
    if expected.is_empty() {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "internal token not configured".into(),
            None,
        ));
    }
    let got = headers
        .get("x-internal-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .as_bytes();
    // 恒定时间比较:防时序攻击。
    if got.len() != expected.len() || !bool::from(got.ct_eq(expected)) {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "invalid internal token".into(),
            None,
        ));
    }
    Ok(())
}

/// axum middleware:整层强制 x-internal-token 校验。
pub async fn require_internal_token_layer(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    check_internal_token(state.internal_token.as_bytes(), &headers)?;
    Ok(next.run(req).await)
}
```

**3b.** 改造 `build_internal_router`（`internal_rpc.rs:79-88`）挂 layer：

```rust
/// 构建 worker 内网 RPC router(独立监听端口,与前端 axum 分离)。
/// 整层挂 require_internal_token_layer,所有 /internal/* 路由强制 token 校验。
pub fn build_internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/thread/start", post(thread_start))
        .route("/internal/thread/invoke", post(thread_invoke))
        .route("/internal/turn/start", post(turn_start))
        .route("/internal/evict", post(evict))
        .route("/internal/approval/respond", post(approval_respond))
        .route("/internal/replicate", post(replicate_receive))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_internal_token_layer,
        ))
        .with_state(state)
}
```

**3c.** 删除 6 个 handler 内第一行 `require_internal_token(&state, &headers).await?;`，并从其签名移除不再使用的 `headers: axum::http::HeaderMap,` 参数。涉及函数（按 `internal_rpc.rs` 出现顺序）：
- `thread_start`（`:90` 起）
- `thread_invoke`
- `turn_start`（`:128` 起）
- `evict`（`:151` 起）
- `approval_respond`（`:184` 起）
- `replicate_receive`（`:162` 起）

示例（`thread_start` 改造前后）：

改造前：
```rust
async fn thread_start(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ThreadStartReq>,
) -> Result<Json<Value>, AppError> {
    require_internal_token(&state, &headers).await?;
    metrics::counter!("internal_thread_start_total").increment(1);
    // ... 其余不变
```

改造后：
```rust
async fn thread_start(
    State(state): State<AppState>,
    Json(req): Json<ThreadStartReq>,
) -> Result<Json<Value>, AppError> {
    metrics::counter!("internal_thread_start_total").increment(1);
    // ... 其余不变
```

对其余 5 个 handler 做同样两处删除（删调用行 + 删 headers 参数）。若某 handler 除 token 外另有 `headers` 用途，则仅删调用行、保留参数（实际查证：6 个 handler 均只在首行用 headers 做 token 校验，参数可一并删除）。

- [ ] **Step 4: 运行测试确认通过 + 全量编译**

Run: `cargo test -p codex-webui --lib api::multitenant::internal_rpc::tests && cargo build -p codex-webui`
Expected: 4 个测试 PASS；`cargo build` 无错误（确认无遗留 `headers` 未使用警告升级为错误、无 `require_internal_token` 未使用错误——已删除该函数）。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/api/multitenant/internal_rpc.rs
git commit -m "refactor(internal-rpc): 鉴权提升为整层 layer,抽 check_internal_token 纯函数带单测"
```

---

### Task 2: WebSocket IDOR 修复（P0）

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs`（`require_thread_team` 改 `pub`，`:403`）
- Modify: `backend-rs/src/api/multitenant/mod.rs`（re-export `require_thread_team`）
- Modify: `backend-rs/src/api/realtime.rs`（`RealtimeState` 加字段；`on_connect` 提取 user_id；`on_thread_subscribe` 改 async 加校验；`on_disconnect` 清理）
- Modify: 构造 `RealtimeState` 的调用点（用 `grep -rn "RealtimeState {" backend-rs/src` 定位，加 `socket_users` 字段初始化）
- Test: `backend-rs/tests/multitenant_auth_test.rs`（新建，verify_access 基线测试）

**Interfaces:**
- Consumes: `crate::services::multitenant::auth::verify_access(token, secret) -> Result<String, AppError>`（`auth.rs:86`，已 pub）；`AuthService::jwt_secret()`（`auth/mod.rs:74`）
- Produces: `pub async fn require_thread_team(db, thread_id, user_id) -> Result<(String, String), AppError>`（从 handlers.rs 导出）；`RealtimeState.socket_users: Arc<Mutex<HashMap<String, String>>>`（socket_id → user_id）

**安全语义：** access JWT 客户端（前端）→ 提取 user_id → 可订阅自身/所属 team 的 thread；API key / 旧 webui JWT 客户端 → 无 user_id → 无法订阅任何 thread 房间（静默拒绝，不泄露 thread 存在性）。

- [ ] **Step 1: 写失败测试 — verify_access 基线（确保 on_connect 依赖的验签可靠）**

新建 `backend-rs/tests/multitenant_auth_test.rs`：

```rust
//! verify_access 多租户 access JWT 验签基线测试(WebSocket on_connect 依赖)。

use chrono::Utc;
use codex_webui::services::multitenant::auth::verify_access;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
    iat: usize,
    typ: String,
}

fn sign(secret: &str, sub: &str, typ: &str) -> String {
    let now = Utc::now().timestamp() as usize;
    let claims = Claims { sub: sub.into(), iat: now, exp: now + 900, typ: typ.into() };
    encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(secret.as_bytes())).unwrap()
}

#[test]
fn verify_access_returns_user_id_for_valid_mt_access_token() {
    let secret = "ws-test-secret";
    let token = sign(secret, "user-abc", "mt_access");
    let uid = verify_access(&token, secret).unwrap();
    assert_eq!(uid, "user-abc");
}

#[test]
fn verify_access_rejects_wrong_typ() {
    // typ != "mt_access"(如旧 sub="webui")→ 拒绝,WS 无法据此建立 user 身份
    let secret = "ws-test-secret";
    let token = sign(secret, "user-abc", "webui");
    assert!(verify_access(&token, secret).is_err());
}

#[test]
fn verify_access_rejects_bad_signature() {
    let token = sign("secret-a", "user-abc", "mt_access");
    assert!(verify_access(&token, "secret-b").is_err());
}
```

> 若 `cargo test` 报 `services::multitenant::auth` 不可访问，在 `backend-rs/src/lib.rs` 确认 `pub mod services;`（及链路 `pub mod multitenant;` `pub mod auth;`）已导出；缺失则补 `pub`。

- [ ] **Step 2: 运行测试确认通过（验证 verify_access 现有行为）**

Run: `cargo test -p codex-webui --test multitenant_auth_test`
Expected: 3 个测试 PASS（verify_access 已实现，测试建立回归基线）。

- [ ] **Step 3: 导出 require_thread_team**

在 `backend-rs/src/api/multitenant/handlers.rs:403`，把 `async fn require_thread_team` 改为 `pub async fn require_thread_team`。

在 `backend-rs/src/api/multitenant/mod.rs` 加 re-export（若该文件已有 `pub use handlers::...;` 则追加，否则新增一行）：

```rust
pub use handlers::require_thread_team;
```

- [ ] **Step 4: RealtimeState 加 socket_users 字段**

在 `backend-rs/src/api/realtime.rs` 的 `RealtimeState` 结构体（`:106-117`）加字段：

```rust
pub struct RealtimeState {
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    pub terminal: Arc<TerminalService>,
    pub db: DatabaseConnection,
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
    pub codex_home: std::path::PathBuf,
    pub active_threads: Arc<ActiveThreadRegistry>,
    /// socket_id → user_id(多租户 access JWT 提取)。API key 客户端无 user_id,无法订阅 thread。
    pub socket_users: Arc<Mutex<HashMap<String, String>>>,
}
```

定位 `RealtimeState` 构造点（`grep -rn "RealtimeState {" backend-rs/src`），在构造处加 `socket_users: Arc::new(Mutex::new(HashMap::new())),`。

- [ ] **Step 5: on_connect 提取 user_id 并存入映射**

在 `backend-rs/src/api/realtime.rs` 改造 `on_connect`（`:145-202`）。在 `authenticate_token` 调用**之前**，尝试用多租户 `verify_access` 提取 user_id；连接鉴权仍用 `authenticate_token`（兼容 API key）；通过后把 user_id 存入 `socket_users`；`on_disconnect` 清理。

改造后的 `on_connect` 关键段（替换 `:165-202` 中自 `let result = ...` 到 `on_disconnect` 闭包部分）：

```rust
    // 优先用多租户 access JWT 提取 user_id(WS 订阅 thread 的身份依据)。
    // 失败则置 None(API key / 旧 webui JWT 客户端:可连接,但不能订阅 thread)。
    let user_id = token.as_deref().and_then(|t| {
        crate::services::multitenant::auth::verify_access(t, state.auth.jwt_secret()).ok()
    });

    // 连接鉴权仍用 authenticate_token(兼容 API key 与多租户 JWT)。
    let result = state.auth.authenticate_token(token.as_deref(), Some(s.id.as_str()));
    if !result.ok {
        tracing::warn!(socket = %s.id, "rejected unauthenticated socket");
        let _ = s.disconnect();
        return;
    }
    tracing::debug!(socket = %s.id, has_user_id = user_id.is_some(), "client connected");

    if let Some(uid) = &user_id {
        state.socket_users.lock().unwrap().insert(s.id.clone(), uid.clone());
    }

    let _ = s.join(s.id.to_string());

    s.on("thread.subscribe", on_thread_subscribe);
    s.on("thread.unsubscribe", on_thread_unsubscribe);
    s.on("fs.subscribe", on_ack);
    s.on("fs.unsubscribe", on_ack);
    s.on("codex.serverResponse", on_server_response);
    // ── terminal 事件 ──
    s.on("terminal.config", on_term_config);
    s.on("terminal.list", on_term_list);
    s.on("terminal.open", on_term_open);
    s.on("terminal.reconnect", on_term_reconnect);
    s.on("terminal.input", on_term_input);
    s.on("terminal.resize", on_term_resize);
    s.on("terminal.rename", on_term_rename);
    s.on("terminal.detach", on_term_detach);
    s.on("terminal.download", on_term_download);
    s.on("terminal.close", on_term_close);
    // 断开连接时从所有终端分离 + 清理线程订阅 + 清理 user_id。
    let term = state.terminal.clone();
    let active = state.active_threads.clone();
    let users = state.socket_users.clone();
    let sid = s.id.clone();
    s.on_disconnect(move || {
        active.remove_socket(sid.as_str());
        users.lock().unwrap().remove(sid.as_str());
        term.detach(sid.as_str(), None);
    });
```

> `on_connect` 顶部提取 `token` 的逻辑（`:150-164`）保持不变。

- [ ] **Step 6: on_thread_subscribe 改 async + 归属校验**

把 `on_thread_subscribe`（`:204-218`）整体替换为：

```rust
async fn on_thread_subscribe(
    s: SocketRef,
    State(state): State<RealtimeState>,
    SocketData(data): SocketData<Value>,
) {
    let thread_id = data
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if thread_id.is_empty() {
        return;
    }
    // 必须有 user_id(多租户 access JWT);API key 客户端无身份 → 静默拒绝。
    let user_id = match state.socket_users.lock().unwrap().get(s.id.as_str()).cloned() {
        Some(u) => u,
        None => {
            tracing::warn!(socket = %s.id, "thread.subscribe denied: no user identity");
            return;
        }
    };
    // 校验 thread 归属(personal=创建者本人;team=成员)。失败静默不 join,不泄露存在性。
    if let Err(e) =
        crate::api::multitenant::handlers::require_thread_team(&state.db, &thread_id, &user_id).await
    {
        tracing::warn!(socket = %s.id, thread = %thread_id, "thread.subscribe denied: {e}");
        return;
    }
    let room = format!("thread:{thread_id}");
    let _ = s.join(room.clone());
    state.active_threads.subscribe(s.id.as_str(), &thread_id);
    tracing::debug!(socket = %s.id, room = %room, "subscribed");
}
```

- [ ] **Step 7: 全量编译 + 测试**

Run: `cargo build -p codex-webui && cargo test -p codex-webui --test multitenant_auth_test`
Expected: 编译通过（注意 `on_thread_subscribe` 改 async 后 socketioxide 接受 async handler）；3 个 verify_access 测试 PASS。

- [ ] **Step 8: 手动验证清单（DB/WS 依赖，无自动化测试）**

启动服务（需 PG + 配置），用两个账号验证：

- [ ] 用户 A（team T1）登录前端，打开自己的 thread，能正常收到实时事件（回归：自身订阅不受影响）
- [ ] 用 A 的 access JWT，在浏览器 console 手动构造 socket 订阅 B（team T2）的 threadId：`getSocket().emit('thread.subscribe', {threadId: '<B的thread>'})`，确认**收不到** B 的 `codex.notification` / `codex.serverRequest` 事件，且后端日志出现 `thread.subscribe denied`
- [ ] 仅持全局 API key（无 access JWT）的客户端连接 WS，订阅任意 thread，确认被拒（日志 `no user identity`）

- [ ] **Step 9: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs backend-rs/src/api/multitenant/mod.rs backend-rs/src/api/realtime.rs backend-rs/tests/multitenant_auth_test.rs
# 以及 grep 定位的 RealtimeState 构造点文件
git commit -m "fix(ws): 修复 thread.subscribe 跨租户越权(IDOR),on_connect 提取 user_id 并校验归属"
```

---

### Task 3: 清理 require_auth 死代码

**Files:**
- Delete: `backend-rs/src/auth/middleware.rs`（整文件，仅含无引用的 `require_auth` + 2 个私有辅助函数）
- Modify: `backend-rs/src/auth/mod.rs:12`（移除 `pub mod middleware;`）

**Interfaces:** 无（`require_auth` 全仓库无引用，删除不破坏任何调用方）。

- [ ] **Step 1: 确认无引用**

Run: `grep -rn "require_auth" backend-rs/src`
Expected: 仅命中 `auth/middleware.rs` 自身定义与注释，**无任何调用点**。若有调用点，停止本任务并先处理调用方。

- [ ] **Step 2: 删除文件 + 移除模块声明**

删除 `backend-rs/src/auth/middleware.rs`。

在 `backend-rs/src/auth/mod.rs` 删除第 12 行 `pub mod middleware;`。

- [ ] **Step 3: 全量编译 + 测试**

Run: `cargo build -p codex-webui && cargo test -p codex-webui`
Expected: 编译通过（无 `cannot find module middleware` 错误）；全部测试 PASS。

- [ ] **Step 4: Commit**

```bash
git add -A backend-rs/src/auth/
git commit -m "chore(auth): 删除无引用的 require_auth 死代码中间件"
```

---

## Self-Review 结果

**1. Spec 覆盖**：批次1覆盖 spec §4.3（WS IDOR）、§4.4（RPC layer + 死代码清理）。✅
**2. 占位符扫描**：所有代码步骤含完整代码；`RealtimeState` 构造点用 `grep` 精确定位指令（非占位符）。✅
**3. 类型一致**：`check_internal_token(&[u8], &HeaderMap)`、`require_thread_team(&DatabaseConnection, &str, &str) -> Result<(String,String), AppError>`、`socket_users: Arc<Mutex<HashMap<String,String>>>` 跨步骤一致。✅
**4. 测试缺口（已记录）**：WS IDOR 的 DB/WS 集成回归测试因项目无 DB 测试设施，本批用手动验证清单；自动化回归留批次4（与 spec §6 测试策略一致，非静默跳过）。

## 批次1完成后

- P0 跨租户越权闭合
- 内网 RPC 新增 handler 默认受 layer 保护
- 死代码清理
- 进入批次2（权限数据模型：migration + TeamPermission enum + 角色矩阵 + require_permission）的 writing-plans

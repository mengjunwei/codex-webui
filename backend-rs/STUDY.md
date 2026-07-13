# Codex WebUI 后端（backend-rs）学习文档

> 本文档面向第一次接触本项目的工程师，目的是让大家能够在较短时间内理解整套 Rust 后端的设计思路、模块协作、数据流向与关键算法。
> 阅读建议：先通读第 1–4 章建立全局认知，再按需深入第 5–10 章的细节。

---

## 目录

1. [项目定位与设计目标](#1-项目定位与设计目标)
2. [技术栈与依赖矩阵](#2-技术栈与依赖矩阵)
3. [代码组织与模块依赖图](#3-代码组织与模块依赖图)
4. [启动流程：main.rs 一次完整的"生命旅程"](#4-启动流程mainrs-一次完整的生命旅程)
5. [应用状态：AppState 与 Arc 共享机制](#5-应用状态appstate-与-arc-共享机制)
6. [Codex 子系统：JSON-RPC 客户端与进程管理器](#6-codex-子系统json-rpc-客户端与进程管理器)
7. [认证与授权：JWT + API key 双轨制](#7-认证与授权jwt--api-key-双轨制)
8. [路由层（axum）：路由表、中间件、错误模型](#8-路由层axum路由表中间件错误模型)
9. [文件子系统：路径安全边界与多根合并](#9-文件子系统路径安全边界与多根合并)
10. [设置、终端、聊天、Realtime、OnlyOffice 等其他模块](#10-设置终端聊天realtimeonlyoffice-等其他模块)
11. [关键算法与设计模式深度解析](#11-关键算法与设计模式深度解析)
12. [数据库 Schema 与迁移](#12-数据库-schema-与迁移)
13. [开发与调试指南](#13-开发与调试指南)
14. [常见陷阱与 FAQ](#14-常见陷阱与-faq)

---

## 1. 项目定位与设计目标

`backend-rs` 是 **Codex WebUI** 的 Rust 重写版后端。它的前任是一个基于 NestJS 的 TypeScript 实现，本次重写聚焦于：

- **更小的二进制体积与更快的启动速度**（约 9 MB 的单一可执行文件，几百毫秒启动）。
- **统一的错误模型**：所有错误都能被前端用作 i18n key。
- **更严格的资源安全**：Rust 借用检查 + 类型系统防止一类典型并发错误。
- **保持 1:1 API 与协议兼容**：所有 REST 路径、字段命名、错误码字符串都与原 TS 实现一致，前端可以无感切换。

整体可归纳为：**「一个 axum HTTP 服务 + 一组后台 tokio 任务 + 一个 SQLite 数据库 + 一个外部子进程（codex app-server）」**。

---

## 2. 技术栈与依赖矩阵

| 类别 | 选型 | 用途 |
|---|---|---|
| Web 框架 | `axum 0.8` | HTTP 路由、提取器、中间件 |
| 异步运行时 | `tokio 1` (full) | 异步运行时与同步原语 |
| 进程 / IO | `portable-pty 0.8` + `wezterm-term` | 跨平台 PTY 与 VT 终端模拟 |
| WebSocket | `socketioxide 0.16` | Socket.IO 服务端（实时通信） |
| HTTP 客户端 | `reqwest 0.12` (rustls) | OnlyOffice 下载校验等 |
| 数据库 | `rusqlite 0.36` (bundled) | 内嵌 SQLite，单连接同步模型 |
| 序列化 | `serde 1` / `serde_json 1` | DTO、JSON 解析 |
| 鉴权 | `jsonwebtoken 9` / `hmac 0.12` / `subtle 2` | JWT 签发与校验、恒定时间比较 |
| 加密 | `sha2 0.10` / `hex 0.4` | HMAC-SHA256 派生 JWT 密钥 |
| 日志 / Trace | `tracing 0.1` + `tracing-appender 0.2` | 日志 + 按大小滚动文件 |
| 可观测性 | `opentelemetry 0.27` + `opentelemetry-otlp 0.27` | OTLP/gRPC trace 导出（可选） |
| 文档 | `utoipa 5` | OpenAPI 文档生成（基础） |
| 压缩 | `flate2 1` / `tar 0.4` / `zip 2` / `sevenz-rust2 0.21` / `bzip2-rs 0.1` / `lzma-rust2 0.16` | 归档预览（zip/tar/7z 等） |

关键点：

- **rusqlite 的 `bundled` feature**：SQLite 源码静态链接，部署时不依赖系统库。
- **`wezterm-term`**：使用 git rev 锁定（`fff02ca5...`），保证 VT 解析器行为稳定。
- **`once_cell`**：单线程无需 `lazy_static!`，替代品。
- **`tower-http`**：只启用 `fs` 和 `trace` 两个 feature，最小化编译产物。

---

## 3. 代码组织与模块依赖图

```
src/
├── main.rs                  程序入口，启动 + 关闭
├── lib.rs                   模块汇总，供测试引用
├── config.rs                环境变量 → Config
├── state.rs                 AppState（Arc 共享）
├── logging.rs               tracing 初始化（stdout + rolling file + OTLP）
├── error.rs                 AppError / ErrorCode / IntoResponse
├── routes/                  路由层
│   ├── mod.rs               build_router + ApiDoc + request_logger 中间件
│   ├── auth.rs              /api/auth/login、/api/auth/logout
│   └── health.rs            /api/_ping、/api/status
│
├── auth/                    鉴权子系统
│   ├── mod.rs               AuthService（JWT + API key）
│   └── middleware.rs        require_auth Axum 中间件
│
├── codex/                   Codex app-server 集成（核心）
│   ├── mod.rs               模块入口
│   ├── jsonrpc.rs           JSON-RPC over stdio 客户端
│   ├── process.rs           CodexProcessManager（生命周期 + 重启）
│   └── types.rs             initialize 握手 DTO
│
├── db/                      数据库
│   ├── mod.rs               Db 封装（WAL/foreign_keys/busy_timeout）
│   └── migrations.rs        drizzle 迁移执行
│
├── settings/                设置子系统
│   ├── mod.rs               SettingsReader/Writer + 回退链
│   ├── definitions.rs       SETTINGS_DEFINITIONS（12 项）
│   ├── reconcile.rs         启动期 reconcile
│   └── handlers.rs          REST handler
│
├── terminal.rs              终端子系统（PTY + VT + 重连）
├── files.rs                 文件子系统（工作区根 + 路径安全）
├── chat.rs                  聊天附件上传
├── onlyoffice.rs            OnlyOffice 集成（编辑配置 + 保存回调）
├── realtime.rs              Socket.IO 网关（/ws 命名空间）
├── event_subscribers.rs     事件驱动的 DB 写入路径
├── sqlite_handlers.rs       Phase 2 SQLite 只读端点
├── codex_status.rs          /codex/status 聚合服务（含 TTL 缓存）
├── codex_status_config.rs   /codex/config 等读写
├── proxies.rs               轻量 REST → codex RPC 代理（account/apps/...）
├── threads.rs               threads + turns REST + ThreadResumeRegistry
└── logs.rs                  日志读取 + 诊断导出
```

### 依赖层级

```
level 0:   lib.rs（纯声明）
level 1:   config / state / error / logging / db
level 2:   auth / codex / settings
level 3:   terminal / files / chat / onlyoffice / threads / realtime
level 4:   proxies / sqlite_handlers / codex_status / logs / event_subscribers
level 5:   routes（聚合）
level 6:   main.rs
```

整个依赖图呈"漏斗"状，没有任何循环依赖；`event_subscribers` 是少数"自顶向下"引用的模块（直接依赖 codex + db + state，但不被其他业务模块依赖）。

---

## 4. 启动流程：main.rs 一次完整的"生命旅程"

打开 `src/main.rs` 看一眼主函数即可在脑中构建全貌：

```text
1. 加载 .env（dotenvy；不存在不报错）
2. 记录进程启动时间（logs::mark_process_start）
3. 解析 Config：从 env 读取 PORT/HOST/WEBUI_API_KEY/CODEX_HOME/...，
   校验 API key ≥ 16 字符
4. init tracing：stdout (human) + rolling file (JSON, 10MB × 5) + 可选 OTLP
5. 打开 SQLite（WAL + FK + busy_timeout=5000）
6. 执行 drizzle 迁移（含检测 TS 托管库并跳过）
7. reconcile settings（建行 + 刷新元数据，不覆盖用户值）
8. 创建 AuthService（派生 JWT 密钥 = HMAC-SHA256(key, "codex-webui-jwt")）
9. 后台 spawn CodexProcessManager.start()
10. spawn 事件订阅者（token-usage/turn-diff/turn-errors/pending-resolved/pending-expire）
11. 读 settings 创建 TerminalService
12. 创建 Socket.IO（/ws 命名空间）
13. spawn emit 任务（codex 通知、server-request 记录+emit、lifecycle、terminal）
14. 创建 CodexStatusService
15. 组装 AppState
16. build_router(state).layer(ws_layer)
17. axum::serve + with_graceful_shutdown（Ctrl-C / SIGTERM）
18. 退出前：codex_for_shutdown.destroy()（杀子进程 + 阻止重启循环）
```

### 优雅关闭（§6.7）

`axum::serve` 的 `with_graceful_shutdown` 接受一个 future；我们的实现是：

- Unix：监听 SIGTERM + Ctrl-C
- Windows：仅监听 Ctrl-C

收到信号后 axum 会**停止接受新连接**，等待在途请求完成；`server.await?` 返回后调用 `codex_for_shutdown.destroy()`，做最后清理。

---

## 5. 应用状态：AppState 与 Arc 共享机制

`state.rs` 是整个服务的"全局对象"，每个 axum handler 通过 `State<AppState>` 提取。

```rust
pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    pub terminal: Arc<TerminalService>,
    pub status: Arc<CodexStatusService>,
    pub resume_registry: Arc<ThreadResumeRegistry>,
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
    pub settings_cache: SettingsCache,  // Arc<Mutex<HashMap<...>>>
}
```

### 设计要点

- **Clone 廉价**：`#[derive(Clone)]` + 内部全部 `Arc`，clone 一次只增加引用计数。
- **没有 RwLock**：故意用 `Mutex` 而非 `RwLock`，因为读临界区都很短；写多读少的情况下 `Mutex` 反而吞吐更高。
- **`settings_cache`**：内存缓存解析过的设置，避免每次 resolve 都查 DB。

### `home_dir()` 与 settings_reader()

两个便利方法：

- `home_dir()`：Windows 优先 `USERPROFILE`，否则 `HOME`，最后空串。
- `settings_reader()`：构造 `SettingsReader<'_>`（生命周期与 `&self` 绑定，不暴露给其他线程）。

### 缓存失效（invalidate_settings_cache）

写路径（PATCH /api/settings 等）完成后调用此方法清空缓存，避免 stale 数据。

---

## 6. Codex 子系统：JSON-RPC 客户端与进程管理器

这是整个系统最复杂的部分。Codex CLI 通过 stdio 与我们通信（启动 `codex app-server --listen stdio://`），协议是"省去 jsonrpc 字段的 JSON-RPC"。

### 6.1 CodexJsonRpcClient（jsonrpc.rs）

#### 角色

- 维护 `(request_id, oneshot::Sender)` 映射表。
- 启动 3 个后台任务：读 stdout / 写 stdin / 写 JSONL 日志。
- 处理 4 类消息：响应（id+result/error）、服务端请求（id+method）、通知（method only）、解析失败。

#### 三类出站消息

| 方法 | 入参 | 行为 |
|---|---|---|
| `request(method, params)` | 必有 id | 分配 id、写入 stdin、`oneshot.await` 关联响应；30 s 超时 |
| `notify(method, params)` | 无 id | 写后即忘；closed 时静默丢弃 |
| `respond_to_server_request(id, result)` | 保留 id 类型 | 把响应写回 stdin |

#### JSONL 日志（`logs/codex-jsonrpc.jsonl`）

每条入站 / 出站消息都追加一行 `{ts, dir, msg}`：
- 有界通道（4096）+ `try_send`，慢盘时丢弃而非 OOM。
- `spawn_blocking` 线程同步写文件，单一持久文件句柄，避免每行都 open/close。
- `dir` 字段标记 `in` / `out`，便于排查。

#### 关键修复（H1 / T5 / T8）

- **H1**：notify forwarder 的 `Err(_) => break` 会把 `Lagged` 当 `Closed`，突发通知下转发永久静默。改为区分 `Lagged`（continue + warn）与 `Closed`（break）。
- **T5**：解析失败时按字符（不是字节）截断 200 字符预览，避免多字节 UTF-8 落点 panic。
- **T8**：JSONL 通道用有界 + `try_send`，背压保护。

### 6.2 CodexProcessManager（process.rs）

#### 职责

- 持有"当前活跃客户端" `(generation, Arc<CodexJsonRpcClient>)`。
- 自动重启子进程（指数退避：3s → 6s → 12s → ... → 60s 上限）。
- 通过 `broadcast` 通道将通知 / 服务端请求 / 生命周期事件转发给全局订阅者。
- 持久化"最近一次 initialize 握手结果"（供 `/codex/status` 暴露 `initialize.data`）。

#### 三阶段启动

```text
spawn_child → attach_forwarders + spawn_close_watcher → initialize_client
```

为什么三阶段：**如果子进程在 initialize 期间退出，关闭监视器必须先挂上才能捕获该事件**。否则会泄漏这个退出事件。

#### Generation 与 ThreadResumeRegistry

每次成功初始化 generation +1。`ThreadResumeRegistry::advance_generation` 在收到 `Ready` 事件时调用，保证旧 generation 的陈旧缓存被清空。**该推进动作在 lifecycle emit 任务内完成**，避免跨任务调度的竞态（H2 修复）。

#### Windows 上的 npm 垫片处理

`build_codex_command` 在 Windows 上识别 `.cmd` / `.bat`：
1. **首选**：解析 `<cmd_dir>/node_modules/@openai/codex/bin/codex.js`，直接 `node codex.js` —— 避免 `cmd.exe` 中间层切断 stdin/stdout 继承。
2. **兜底**：`cmd.exe /c <bin>`，但用 `COMSPEC` 环境变量拿绝对路径（Git Bash 进程的 PATH 可能不含 `C:\Windows\system32`）。
3. **真正的 .exe**：直接 spawn。

`locate_node_binary` 的多重探测：`where node.exe` → 常见安装目录 → `which node`（Git Bash）→ NVM 目录（`%APPDATA%\nvm\<version>\node.exe`）。

### 6.3 进程退出处理（handle_close）

`stdout EOF` 或 `destroy()` 都会触发 `CloseReason` 广播。`handle_close` 在监视器中调用：

1. 锁内 `current.take()` —— 只清除仍是该 generation 的活跃客户端。
2. 清空 `init_result`，广播 `Unavailable`。
3. 若非 `destroyed`，调度 `restart()`。

**关键**：`destroy()` 用 `take()` 而非 `lock().deref()`，避免持锁跨 await。

---

## 7. 认证与授权：JWT + API key 双轨制

### 7.1 AuthService（auth/mod.rs）

JWT 密钥派生：`HMAC-SHA256(key=WEBUI_API_KEY, msg="codex-webui-jwt").hexdigest`，与 TS 完全一致。

`authenticate_token(token)` 流程：

```text
1. 空 → invalid
2. verify_jwt(token) 成功 → ok (auth_type="jwt")
3. token 形似 JWT（3 个 .）但 verify 失败 → 记录 warn，继续
4. validate_api_key(token)（恒定时间比较 ct_eq） → ok (auth_type="apiKey")
5. 都不匹配 → invalid
```

`validate_api_key` 用 `subtle::ConstantTimeEq` 做字节级比较，防止时序攻击。

### 7.2 require_auth 中间件（auth/middleware.rs）

```text
1. Authorization: Bearer <token>      ← 主路径
   scheme 大小写不敏感（"bearer "/"BEARER " 均接受）
   偏移 7 切片提取 token
2. ?access_token=<jwt>（仅 GET /api/files/serve 和 /api/files/archive/entry）
   查询参数 token 必须是合法 JWT（3 段）
3. authenticate_token 或 verify_jwt（取决于来源）
   失败 → 401 AuthMissingHeader / AuthInvalidToken
```

### 7.3 API key 最小长度

`config.rs` 强制 `WEBUI_API_KEY >= 16` 字符，启动失败立即报错。

---

## 8. 路由层（axum）：路由表、中间件、错误模型

### 8.1 build_router 总览（routes/mod.rs）

```text
公开（无认证）：
  POST /api/auth/login
  POST /api/onlyoffice/callback
  GET  /api/docs-json

受保护（require_auth + DefaultBodyLimit）：
  GET  /api/_ping
  GET  /api/status
  POST /api/chat/upload  (multipart, body limit disabled)
  /api/settings、/api/settings/{key}
  /api/auth/logout
  /api/threads/...
  /api/pending-approvals/...
  /api/logs、/api/logs/export
  /api/account/...
  /api/apps / /api/models
  /api/mcp-servers/...
  /api/skills/...
  /api/plugins/...
  /api/codex/...
  /api/files/...
  /api/onlyoffice/config
  /api/threads/...
  (等等)

静态：ServeDir "public" → fallback ServeFile "public/index.html"
```

### 8.2 错误模型（error.rs）

```rust
enum AppError {
    Business { code, status, message, params },  // 业务错误，结构化
    Status { status },                            // 仅状态码
    Internal(String),                            // 500 + http.internal_error
}
```

`IntoResponse` 实现统一序列化为：

```json
{ "statusCode": 401, "errorCode": "auth.invalid_token", "message": "...", "params": {...} }
```

`ErrorCode::as_str()` 返回的字符串 **必须与 `src/common/error-codes.ts` 逐字一致**，因为前端将其用作 i18n key。

### 8.3 request_logger 中间件

记录 method / 脱敏 path / status / 耗时。path 走 `sanitize_url` 替换 `access_token=...` 为 `access_token=[Redacted]`。

### 8.4 上传大小限制

`build_router` 中显式设置 `DefaultBodyLimit::max(upload_limit)` 与 `DefaultBodyLimit::disable()`（用于 multipart 大文件上传）。`upload_limit` 从 settings `files.uploadMaxBytes` 读取，默认 100 MB。

---

## 9. 文件子系统：路径安全边界与多根合并

`files.rs` 是本项目最长的单文件，也是安全敏感度最高的部分。

### 9.1 工作区根目录的三个来源

```text
配置根（settings.security.workspaceRoots，逗号分隔）
  + 家目录（USERPROFILE/HOME 规范化）
  + 动态根（POST /api/files/roots 注册）
```

合并逻辑见 `compute_workspace_roots`。家目录用 `OnceCell` 缓存规范化结果（Windows 上 canonicalize 会带上 `\\?\` 前缀）。

### 9.2 resolve(state, input) —— 唯一的路径校验入口

```text
1. trim → 空 → files.path_required (400)
2. 含 NUL → 403
3. canonicalize → 不存在 → 404
4. 不在任一根目录之内 → 403 files.path_outside_workspace
5. metadata → 归类 File / Directory / Other
6. 返回 ResolvedTarget { original, resolved, kind, size, mtime_ms }
```

### 9.3 删除路径（delete_path）的安全关卡

```text
1. symlink_metadata 判断是否为符号链接
   - 是：校验父目录在工作区内 → remove_file（删链接本身）
2. resolve 后判断 is_workspace_root
   - 是 → 403（禁止删除工作区根）
3. 普通目录：recursive=true → remove_dir_all；否则非空 → 400 dirNotEmpty
```

### 9.4 写入路径（write_file）的符号链接逃逸防护

仅校验"父目录在工作区内"还不够 —— 工作区内的符号链接可能指向工作区外。因此：

```text
1. 父目录 → canonicalize → within_workspace?
2. 目标 → canonicalize（若存在）→ within_workspace?
```

两步都通过才能写入。

### 9.5 终端 cwd 沙箱（resolve_terminal_cwd）

```text
优先级：
  1. terminal.defaultCwd settings（空字符串视为未设置）
  2. context_key == "thread:..." → 必须显式提供 cwd
  3. 其他 → 回落到家目录
然后 canonicalize → must in workspace roots → 必须为已存在目录
```

### 9.6 内联预览（serve_file）支持 Range

`<img>/<video>/<pdf>` 用 `Range: bytes=start-end` 请求分片。后端读取 `Range` 头并切片响应，状态码 206。

---

## 10. 设置、终端、聊天、Realtime、OnlyOffice 等其他模块

### 10.1 settings —— 三层回退链

```text
DB (JSON 解码) → env (按类型解析) → default (按定义)
```

`SettingsReader::resolve(key)` 先查内存缓存，再按上面顺序解析，并把结果写入缓存。

类型校验在 `validate_value` 中完成：

- `Number`：`fract == 0` 检查 integer；min/max 强制；整数用 i64。
- `String`：必须是字符串；null 在 PATCH handler 中转为"重置"。
- `Boolean`：标准 bool。
- `Json`：拒绝 null；对象键名禁止 `__proto__/constructor/prototype`（防原型污染）。

`reconcile_settings` 在启动时为每个定义建行（INSERT OR IGNORE，保留用户值），并刷新元数据列。

### 10.2 terminal —— PTY + VT + 重连

- `portable-pty` 创建 PTY；`wezterm-term::Terminal` 作为 VT 解析器。
- reader_task 从 PTY 读字节 → 喂给 VT → 差异 emit 给 socket。
- reconnect 时用 VT 序列化屏幕状态（不再回放字节）。
- 优雅期 `graceMs`：detached 后的会话保留时长。

### 10.3 chat —— 附件上传 + 路径校验

- `POST /api/chat/upload`：multipart 流式写入 `{CODEX_HOME}/webui-uploads/{uuid}.{ext}`。
- `resolve_stored_upload_path`：仅允许引用上传根内的图片（防止 LFI）。
- 周期清理：每小时一次，删除超过 24h 的旧文件。

### 10.4 realtime —— Socket.IO 网关

- 命名空间 `/ws`，所有事件以 `codex.*` / `terminal.*` / `thread.subscribe` 等命名。
- 7 个 emit 转发任务：通知 / server-request (含 DB 持久化) / lifecycle (含 auto-resume) / terminal output/exit/closed/metadata。
- `ActiveThreadRegistry` 双向索引（socket ↔ thread），auto-resume 用。

### 10.5 onlyoffice —— JWT 签名 + 回调

- `get_config`：构造 OnlyOffice Document Server 的 `config` 对象（含 JWT 签名）。
- 编辑模式需要 `general.onlyofficeJwtSecret`。
- 回调 `POST /api/onlyoffice/callback` 公开（不走 auth），用 JWT 校验合法性。
- 下载 URL 校验：必须 HTTPS + origin 匹配。

### 10.6 codex_status —— 聚合状态服务

并行探针 `account/read` + `config/read` + `model/list`，结合进程管理器的 initialize 结果，聚合出 `/codex/status` 响应。

- 缓存：ready 30s，unavailable 5s。
- Single-flight：并发未命中共享一次刷新（`tokio::sync::Mutex<()>` 串行化）。

### 10.7 event_subscribers —— 事件驱动的 DB 写入

5 个独立的 tokio 任务：

1. **token-usage**：订阅 `thread/tokenUsage/updated` → upsert `token_usage_snapshots`。
2. **turn-diff**：订阅 `turn/diff/updated`（内存缓冲）+ `turn/completed`（刷写）。
3. **turn-errors**：订阅 `error`（willRetry=false）+ `turn/completed`（status=failed）。
4. **pending-resolved**：订阅 `serverRequest/resolved` → 标记为 resolved。
5. **pending-expire**：订阅 lifecycle Restarting/Unavailable → 按 generation 过期。

启动时还调用 `expire_all_pending`（与 TS `onModuleInit` 对齐）。

---

## 11. 关键算法与设计模式深度解析

### 11.1 broadcast 通道的 Lagged 处理

`tokio::sync::broadcast` 的 `RecvError::Lagged(n)` 表示订阅者太慢，旧消息被丢弃。错误地将其等同于 `Closed` 会导致转发任务静默死亡。

正确写法：

```rust
match rx.recv().await {
    Ok(msg) => forward(msg),
    Err(RecvError::Lagged(n)) => {
        tracing::warn!(lagged = n, "subscriber lagged, skipping");
        continue;  // 通道仍然存活
    }
    Err(RecvError::Closed) => break,
}
```

### 11.2 并发 resume 的 per-key 锁

`ThreadResumeRegistry::ensure_resumed` 用 std Mutex 短持有获取 tokio Mutex：

```rust
let lock = {  // std Mutex 短持有
    let mut guards = self.inflight.lock().unwrap();
    guards.entry(key).or_insert_with(...).clone()
};
let _guard = lock.lock().await;  // tokio Mutex 跨 await
// ... 执行 RPC 或读取缓存 ...
drop(_guard);
drop(lock);
self.reap_inflight_slot(&key);  // 仅 strong_count == 1 时移除
```

避免 start ↔ restart 的 Box::pin 递归：start 是 async fn，直接 await 会无穷递归；用 `Box::pin(self.start()).await` 强制堆分配。

### 11.3 写锁与 await 的非 Send 陷阱

`MutexGuard` 不是 `Send`，跨 `.await` 持有它会导致 future 变为 `!Send`。`sqlite_handlers::respond_to_request` 中：

```rust
// 1. 短持有 DB 锁查询
let existing_status = {
    let conn = db.conn.lock().await?;
    conn.query_row(...).optional()?
};  // 锁在这里释放

// 2. 跨 await 获取 client
let client = state.codex.client().await.ok_or(...)?;

// 3. 再取锁做事务
{
    let conn = db.conn.lock().await?;
    let tx = conn.unchecked_transaction()?;
    // ... CAS + 转发 ...
    tx.commit()?;
}
```

### 11.4 Windows 长路径前缀

Windows 上 `canonicalize` 返回 `\\?\C:\Users\...` 形式。前端回传 `C:\Users\...\file.txt` 时不能直接字符串比较。

```rust
let path_str = path.to_string_lossy();
let display_path = path_str.strip_prefix("\\\\?\\").unwrap_or(&path_str);
```

文件树响应里也要做同样剥离，否则前端展示不出来。

### 11.5 错误码与 i18n 集成

`ErrorCode::as_str()` 返回的点分字符串 `files.path_outside_workspace` 直接对应前端的翻译文件：

```json
{
  "files.path_outside_workspace": "路径不在工作区内",
  "auth.invalid_token": "认证失败",
  ...
}
```

增加错误码时必须**同步前端翻译文件**。

### 11.6 路径包含性比较

```rust
fn is_within(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}
```

`Path::starts_with` 是组件级比较（处理 `/foo/bar` 是 `/foo` 的子目录但不是 `/fo` 的子目录）。但是它**不做大小写归一化**，Windows 上需要先 `set_extension` 或 canonicalize。

### 11.7 数据库事务中的"事务 + 跨 await 转发"

`respond_to_request` 中：

```rust
let tx = conn.unchecked_transaction()?;
// CAS 更新
let changes = tx.execute(...)?;
if changes != 1 { return Err(...); }
// 同步转发给 codex（极快，不跨 await）
client.respond_to_server_request(id_value, result)?;
tx.commit()?;
```

如果 `respond_to_server_request` 失败，事务 Drop 时自动回滚，状态保持 pending —— 保证 DB 与 codex 状态一致。

---

## 12. 数据库 Schema 与迁移

迁移文件位于 `drizzle/0000_init.sql` ~ `0005_melted_mister_sinister.sql`，编译期通过 `include_str!` 内嵌。

### 主要表

```text
settings                     设置项定义 + 用户值
schema_migrations            自管迁移追踪
token_usage_snapshots        thread/turn × 累计 token 用量
turn_diffs                   thread/turn × 差异字符串
turn_errors                  thread/turn × 错误信息
pending_server_requests      codex 服务端请求（generation + request_id 主键）
```

### 启动期兼容性

`run_migrations` 检测 `__drizzle_migrations` 表（TS drizzle-kit 创建）。若存在则跳过执行，仅把 6 个文件登记为已执行，便于后续增量迁移。

---

## 13. 开发与调试指南

### 13.1 本地构建与运行

```bash
cd backend-rs
cargo build --release
WEBUI_API_KEY=your-32-char-secret ./target/release/codex-webui
```

### 13.2 关键环境变量

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `WEBUI_API_KEY` | ✅ | - | 至少 16 字符 |
| `PORT` | - | 8172 | HTTP 端口 |
| `HOST` | - | 0.0.0.0 | 监听地址 |
| `CODEX_HOME` | - | ~/.codex | codex 配置目录 |
| `CODEX_BIN` | - | codex | codex 可执行文件 |
| `WEBUI_DB_PATH` | - | 见 config.rs | SQLite 路径 |
| `OPENAI_API_KEY` | - | - | OpenAI 凭据 |
| `LOG_LEVEL` | - | info / debug | tracing EnvFilter |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | - | - | OTLP collector URL |

### 13.3 调试技巧

- 日志：`logs/app`（JSON）、`logs/codex-jsonrpc.jsonl`（JSONL）。
- 前端用 `/api/logs?level=info&source=http` 读日志。
- OTLP：设置 `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317` 后启动 Jaeger / Tempo。
- 单元测试：`cargo test`（在 config、logging、jsonrpc、settings 都有覆盖）。

### 13.4 常用 cargo 命令

```bash
cargo check                  # 快速类型检查
cargo clippy --all-targets   # lint
cargo test                   # 单测
cargo build --release        # 生产构建
RUST_LOG=debug cargo run     # 覆盖 LOG_LEVEL
```

---

## 14. 常见陷阱与 FAQ

### Q1：启动报 "WEBUI_API_KEY must be at least 16 characters"

答：检查 `.env` 或部署环境变量。API key 同时用作 bearer 回退凭据与 JWT 派生种子，过短易被爆破。

### Q2：Windows 上 codex 子进程无法通信

答：`codex.cmd` 是 npm 垫片，`cmd.exe /c` 不会继承管道。本项目已自动解析 `<bin_dir>/node_modules/@openai/codex/bin/codex.js`，若仍失败检查 `resolve_node_script` 探测日志。

### Q3：怎么新增一个 setting？

答：在 `settings/definitions.rs` 的 `SETTINGS_DEFINITIONS` 数组追加 `SettingDef`；重启服务，`reconcile_settings` 自动建行。

### Q4：怎么新增一个错误码？

答：在 `error.rs` 的 `ErrorCode` 枚举中追加；实现 `as_str()` 映射；前端翻译文件添加 key。

### Q5：JSONL 日志满了会怎样？

答：有界通道 + `try_send`，慢盘下丢弃。日志保留在 `RollingWriter`（10 MB × 5 个文件）。如需更长保留，修改 `logging.rs` 的 `max_size` / `max_files`。

### Q6：怎么让某个 endpoint 公开（不走 auth）？

答：在 `routes/mod.rs` 中，将该路由放在 `.nest("/api", api)` 之外 —— 它会绕开 `.layer(require_auth)`。`/api/auth/login` 与 `/api/onlyoffice/callback` 就是这样处理的。

### Q7：Mutex 锁中毒怎么办？

答：`db.conn.lock()` 的返回结果用 `?`/`map_err` 处理。锁中毒意味着某处 panic 后没释放，本项目不会主动 panic 在锁内，但仍保留 `.map_err(|e| anyhow!("db lock poisoned: {e}"))` 兜底。

### Q8：tokio::sync::broadcast 的 Lagged 与 Closed 区别？

答：Lagged 表示订阅者落后但通道还活着，应 continue；Closed 表示所有发送端都被 drop，应 break。错误判断会导致后台转发任务静默死亡。

### Q9：怎么验证 socketioxide 0.15 的 join 行为？

答：JS socket.io 自动把 socket 加入以自身 SID 命名的房间，socketioxide 不会。所以本项目 `on_connect` 中显式 `s.join(s.id)` —— 否则单 socket 的 terminal emit 会指向空房间。

### Q10：什么时候应该 invalidate settings cache？

答：所有 settings 写路径（PATCH /api/settings、PATCH /api/settings/{key}、DELETE /api/settings/{key}）之后必须调用 `state.invalidate_settings_cache()`，否则下次读取命中 stale。

---

## 附录 A：进程间通信图

```text
Browser
  ├── HTTPS REST ──→ axum (handlers)
  │                     ↓
  │                  state.codex.request(method, params)
  │                     ↓
  └── WSS /ws  ─────→ socketioxide
                       ↑
                       │ codex.notification / codex.serverRequest / codex.lifecycle
                       │
                CodexProcessManager (broadcast channels)
                       ↑
                       │
                  CodexJsonRpcClient
                  ├── reader_task  (stdout → dispatch_line)
                  ├── writer_task  (stdin)
                  └── jsonl_loop   (logs/codex-jsonrpc.jsonl)
                       ↕ stdin/stdout pipes
                  codex app-server child process
```

## 附录 B：DB 写路径事件流

```text
codex app-server
  │ emit thread/tokenUsage/updated
  ↓
CodexProcessManager.notify_tx (broadcast)
  ├──→ RealtimeEmitter   ──→ WS /ws (codex.notification)
  └──→ EventSubscribers.spawn_token_usage → DB upsert
```

```text
codex app-server
  │ server-request (approval/request)
  ↓
CodexProcessManager.server_request_tx
  ├──→ RealtimeEmitter (record + emit)：
  │     1. event_subscribers::record_server_request → DB INSERT
  │     2. socketioxide emit codex.serverRequest
  │     (顺序保证：DB 先，WS 后)
  └──→ （DB 失败则跳过 WS 发射，防幽灵请求）
```

## 附录 C：源码注释约定

本项目源码中：

- 模块顶部 `//!` 文档说明：模块职责、对齐 TS 的对应文件。
- 函数级 `///` 文档：参数、返回值、错误码。
- 行内 `//` 注释：解释"为什么"或"易错点"。
- 中文为主，技术专有名词（axum、JWT、SQLite、broadcast）保留英文。

---

> 最后更新：2026-07-13
> 维护建议：当模块结构发生大改时同步更新本文件第 3 章；新增错误码时同步更新第 8.2 与 FAQ；新增算法时同步更新第 11 章。
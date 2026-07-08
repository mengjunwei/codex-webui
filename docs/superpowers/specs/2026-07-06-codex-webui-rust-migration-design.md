# Codex WebUI 后端迁移至 Rust — 设计文档

- **日期**: 2026-07-06
- **状态**: Draft（待 review）
- **作者**: 迁移方案设计
- **关联项目**: `D:\code\rust\codex-webui`（现有 NestJS 后端）

---

## 1. 背景与现状

Codex WebUI 是给 [OpenAI Codex CLI](https://github.com/openai/codex) 做的 Web 前端。后端（本设计的目标）用 **NestJS 11** 实现，核心职责：

1. 拉起 `codex app-server` 子进程，通过 **stdio JSON-RPC** 与之通信；
2. 把 codex 的通知 / 服务端请求（审批等）经 **Socket.IO** 实时推给 React 前端；
3. 提供文件管理、终端、压缩包、Office 文档等周边能力的 REST API。

### 1.1 现有技术栈

```
NestJS 11 · Fastify 5 · Socket.IO · node-pty
SQLite (better-sqlite3 + Drizzle ORM) · Pino (pino-roll 滚动日志)
```

### 1.2 规模

- 约 **15,666 行**业务代码（不含 `.spec.ts`），144 个 TS 文件，**20 个功能模块**。
- 最大文件：`files.service.ts` (1072)、`terminal.service.ts` (634)、`codex-status.service.ts` (631)、`onlyoffice.controller.ts` (630)、`threads.controller.ts` (607)。

### 1.3 功能模块清单

| 层 | 模块 |
|---|---|
| 简单 CRUD | account · apps · models · mcp-servers · skills · plugins · token-usage · turn-diff · turn-errors · logs · pending-approvals · settings |
| 中等 | auth（JWT + API Key 全局 Guard）· chat（multipart 上传）· archive（zip/tar(.gz/.bz2/.xz)/7z）· onlyoffice |
| 核心/硬骨头 | **codex**（JSON-RPC + 进程管理，系统中枢）· **threads / files**（Socket.IO 实时）· **terminal**（node-pty + xterm headless VT） |

### 1.4 关键架构事实（影响迁移）

- **前后端契约 = OpenAPI**：前端 `web/` 的 API SDK 由 Swagger 自动生成（`pnpm generate:api`）。只要 Rust 后端暴露**相同的路由 + 相同的 operationId + 相同的 Socket.IO 事件名/namespace**，前端零改动。
- **错误码是前端 i18n key**：`ErrorCode`（`src/common/error-codes.ts`）的字符串码（如 `auth.invalid_api_key`）被前端当作翻译键使用，**必须逐字保留**。
- **数据库迁移是纯 SQL**：`drizzle/0000..0005.sql` 用 `--> statement-breakpoint` 分隔语句，Rust 侧可直接执行，无需重新设计 schema。
- **运行时配置在 SQLite**：`security.workspaceRoots`、`files.uploadMaxBytes`、终端参数等存于 `settings` 表，同名历史环境变量作 fallback。
- **codex 协议类型**：`src/codex/codex-schema/` 由 `codex app-server generate-ts` 生成（构建时）；`src/codex/dto/v2/` 是手写的协议镜像 DTO。

---

## 2. 目标与非目标

### 2.1 目标

- **A. 性能 / 资源占用**：降低内存、加快启动、提高并发吞吐。
- **B. 单一自包含二进制**：去掉 Node 运行时，一个文件部署 / 更小的 Docker 镜像。
- **C. 类型安全 + 长期可维护性**：借 Rust 强类型与所有权减少运行时错误。
- **验收线：生产环境可用**——行为与现有 TS 后端一致，前端无感切换。

### 2.2 非目标

- 不重写前端（`web/`）。
- 不改变数据库 schema（复用现有迁移）。
- 不改变对外 API 契约（路由 / operationId / WS 事件名）。
- 终端重连采用 `wezterm-term` VT 状态序列化（见 §6.5）：终端功能保留，重连回放 VT 屏幕纯文本快照。

---

## 3. 迁移路线决策

考虑过三条路线：

| 路线 | 描述 | 结论 |
|---|---|---|
| 1. 大爆炸重写 | 全部重写后一次性切换 | ❌ 15.7k 行 + 原生依赖 + 实时，风险集中、周期长、中间不可用 |
| 2. 经典绞杀者 | Rust 反代前置，逐步替换 Node 路由 | 风险最低可分批上线，但过渡期双跑 Node+Rust，与"单二进制"目标有张力，代理层是额外复杂度 |
| 3. **独立 Rust 重写 + 增量校验** | Rust 作为独立完整服务重写，按依赖顺序逐模块移植，每块用 TS 后端做行为对照 | ✅ **采用** |

**选 3 的理由**：终态单二进制（契合 B）；每步可对照验证（契合"生产可用"）；无需代理层（比 2 简单）；比 1 风险低得多。TS 后端在切换前保留为参考基准（reference oracle）。

---

## 4. 目标架构与技术栈

### 4.1 栈选型

| 关注点 | Node 现状 | Rust 选型 | 说明 |
|---|---|---|---|
| HTTP 框架 | NestJS + Fastify | **axum 0.7+** | extractor/middleware 对应 Guard/Filter/拦截器 |
| 多部分上传 | @fastify/multipart | **tower / axum::extract::Multipart** | `preservePath` 语义需对齐（webkitRelativePath） |
| 静态服务 | @nestjs/serve-static | **tower-http::ServeDir** | serve `public/`，`/api` 不走静态 |
| WebSocket | @nestjs/websockets + socket.io | **socketioxide** | Socket.IO 协议兼容，namespace `/ws`、rooms 语义可对齐 |
| 数据库 | better-sqlite3 + Drizzle ORM | **rusqlite**（bundled） | 直接跑现有 drizzle SQL 迁移 |
| 日志 | nestjs-pino + pino-roll | **tracing + tracing-appender** | 滚动：10m / 5 文件 / `logs/app`，对齐 pino-roll |
| OpenAPI | @nestjs/swagger | **utoipa** | operationId 工厂需对齐，保证前端 SDK 生成不坏 |
| 认证 | @nestjs/jwt + jsonwebtoken | **jsonwebtoken** crate | JWT 签名密钥派生自 WEBUI_API_KEY |
| 终端 PTY | node-pty | **portable-pty** | 跨平台 PTY |
| 压缩包 | 7zip-min / unrar-async / yauzl / tar-stream | **sevenz-rust2 / zip / tar / bzip2-rs / lzma-rust2** | rar 不支持；其余 5 种格式纯 Rust 实现 |
| 配置 | @nestjs/config (env) | **figment / config** + env | env 变量 + SQLite runtime settings |
| 序列化 | reflect-metadata + class-validator | **serde + serde_json + validator** | DTO 校验 |
| DI | NestJS IoC | **显式 `Arc<AppState>`** | 单例服务组装进共享 state，经 axum State 注入 |

### 4.2 依赖注入策略

NestJS 的 DI 在本项目里几乎都是**单例 service 互相注入**，没有复杂的作用域/生命周期差异。因此不引入 DI 框架，改为：

- 启动时按依赖顺序构造各 service，装入 `AppState`（`Arc` 包裹）。
- 通过 axum 的 `State<AppState>` 与自定义 extractor 注入到 handler。
- `OnModuleInit` / `OnModuleDestroy` → 显式 `init()` / graceful shutdown（axum `with_graceful_shutdown`）。
- 全局 `ApiKeyGuard` → axum middleware（在路由前校验 JWT/API Key，`@Public` 装饰器 → 公开路由组跳过）。

---

## 5. 模块映射与移植顺序

移植按**依赖深度 × 风险**从低到高分 6 个阶段。每个阶段产出可独立对照验证的成果。

### Phase 0 — 地基与脚手架
- Cargo workspace（单 crate 起步，必要时拆分）。
- 配置：env 变量全集（`WEBUI_API_KEY` 必填，`PORT=8172`、`OPENAI_API_KEY`、`LOG_LEVEL`、`CODEX_BIN`、`CODEX_HOME`、`WEBUI_DB_PATH`）+ SQLite runtime settings 读取 + 历史 env fallback。
- 日志：`tracing` + `tracing-appender` 滚动（10m / 5 文件 / `logs/app`）。**脱敏以 `app.module.ts` 的 `PINO_REDACT` 为权威对照源**——涵盖 `req.headers.authorization/cookie`、`req.query.access_token`、`res.headers.set-cookie`、`token/accessToken/apiKey/password` 及通配 `*.token` 等；URL 序列化仅剥离 `access_token` query 参数。
- 错误处理：`BusinessException` + `ErrorCode`（**逐字保留错误码字符串**）+ `AllExceptionsFilter` → axum 统一错误响应。
- DB 层：`rusqlite` 连接池 + 执行 `drizzle/0000..0005.sql`（按 `--> statement-breakpoint` 拆分）。
- 认证（以 `auth.service.ts` 为权威）：JWT 密钥派生 = `HMAC-SHA256(key=WEBUI_API_KEY, msg='codex-webui-jwt').hexdigest`，算法 HS256，TTL 24h，`sub='webui'`；bearer 校验**先验 JWT，失败再走 API Key fallback**（`timingSafeEqual` 常量时间比较；`looksLikeJwt` = 三段点分）。全局 axum middleware，`@Public` → 公开路由组。
- 优雅关闭（**增量增强，非平移**）：TS 版未 `enableShutdownHooks`，当前 codex 子进程靠进程退出被杀、未 drain。Rust 版用 axum `with_graceful_shutdown`（SIGTERM）：先停接新连接 → drain 进行中请求 → `CodexProcessManager` 销毁 codex 子进程 → 清理所有 PTY 会话 → 关闭 DB。

### Phase 1 — codex 核心（系统中枢，最早跑通）
- JSON-RPC 客户端（对齐 `codex-jsonrpc-client.ts`）：请求/响应关联、服务端发起请求、通知、每请求超时（默认 30s）、`logs/codex-jsonrpc.jsonl` 双向日志、BigInt→Number 序列化。**线格式关键点**：wire 报文**省略 `jsonrpc` 字段**（`{method,id,params}` / `{method,params}` / `{id,result|error}`），请求 id 从 1 递增——勿用会注入 `jsonrpc:"2.0"` 的通用 JSON-RPC crate。
- 进程管理（对齐 `codex-process-manager.service.ts`）：spawn `codex app-server --listen stdio://`、initialize 握手、generation 计数、退出自动重启（3000ms 固定退避）、生命周期事件、跨重启的事件转发器注册。**initialize 参数固定**：`clientInfo={name:'codex_webui', title:'Codex WebUI', version:'0.1.0'}`，`capabilities={experimentalApi:true}`。
- codex 类型：手翻 `src/codex/dto/v2/` DTO 为 serde struct（`#[serde(rename_all = "camelCase")]` 等，未识别字段用 `#[serde(flatten)]` 兜底 `serde_json::Value`）；`codex-schema` 若 codex 提供 JSON schema 再考虑自动生成。
- codex-status / codex-config service + controller。

### Phase 2 — 简单 CRUD 批量移植
account · apps · models · mcp-servers · skills · plugins · token-usage · turn-diff · turn-errors · logs · pending-approvals · settings。这些模块模式高度一致（controller + service + dto + drizzle 查询），可模板化批量推进。

### Phase 3 — 实时 gateway
- **threads gateway**：Socket.IO namespace `/ws`，room `thread:<id>`，`thread.subscribe` / `thread.unsubscribe` / `codex.serverResponse` 事件，`codex.notification` / `codex.serverRequest` / `codex.lifecycle` 路由逻辑；连接时校验 token。
- **threads service**：CRUD + `active-thread-registry` + `auto-resume` + `thread-resume-registry`。
- **files gateway**：当前是**空操作 stub**（chokidar watcher 已移除），仅需复刻 `fs.subscribe` / `fs.unsubscribe` 两个 ack（`{ok:true}`）以兼容存量前端 emit。
- **files.service**（1072 行，最大）+ **preview 工具**（`src/preview/file-response.ts`）：文件操作、路径安全校验、树形浏览、上传/下载/重命名/移动；以及 MIME 嗅探表、RFC 6905/9110 `Range` 解析、`Content-Disposition`（含 UTF-8 `filename*`）、`sendRangedStream`（HTTP **206 部分内容 + 416 不可满足**）。后者是视频/PDF 拖动等可观测行为，须作为共享 util 移植。

### Phase 4 — chat / archive / onlyoffice
- chat：multipart 上传 + `chat-upload.service`。
- archive：zip / tar(.gz/.bz2/.xz) / 7z 适配器（免解压预览）。**rar 不支持**（决策：不引入 unrar 的 C 绑定，纯 Rust RAR 解码器成熟度不足；前端对 .rar 不提供归档预览）。.tar.xz 已通过 lzma-rust2 支持（纯 Rust，移植自 tukaani 官方参考实现）。
- onlyoffice：630 行 controller，回调保存、publicBaseUrl 检测。

### Phase 5 — 终端
- `portable-pty` 会话、多 tab、按 context 分组（`global` / `thread:<id>`）、多 socket 附着。
- **重连策略**：使用 `wezterm-term` VT 模型——PTY 输出实时 `advance_bytes` 喂入 VT；重连 / 下载时序列化 VT 屏幕（回滚 + 可见行），遍历 cell `attrs` 输出 SGR 转义序列（颜色 / bold / italic / underline / reverse），前端 `term.write` 可恢复彩色 + 样式画面。scrollback 容量取自 settings。
- terminal gateway：复刻 WS 事件集（`terminal.reconnect` 返回 `{terminal, state}`、attach / input / resize / 输出流等）。`terminal.reconnect.state` 为带 SGR 的 VT 序列化字符串（类型仍为 string，前端 `term.write` 回放恢复彩色；不还原光标位置 / alternate-screen）。

### Phase 6 — 静态服务 / OpenAPI / 校验 / 切换
- 静态前端：用 **`rust-embed` / `include_dir`** 把构建产物（`web/` → `public/`）嵌入二进制（目标 B 单二进制），tower-http ServeDir 在嵌入资源上提供，**排除 `/api`**，`fallthrough` 对齐。
- utoipa OpenAPI：挂 `/api/docs`，**仅 dev 构建（`NODE_ENV !== production` 对应 cfg flag）暴露**，对齐 TS 行为；operationId 工厂对齐（`${controller}_${method}`，去 `Controller` 后缀 + 首字母小写）。
- multipart `preservePath` 对齐。
- 全量 parity 校验（见 §7）、性能基线对比、灰度切换、下线 TS 后端。

---

## 6. 关键设计决策

### 6.1 API 契约原样保留（生产可用的命门）
- REST 路径、方法、请求/响应结构、状态码、**错误码字符串**逐字保留。
- operationId 保持稳定（前端 SDK 依赖）。
- Socket.IO namespace `/ws`、事件名（`thread.subscribe`、`codex.notification`、`codex.serverRequest`、`codex.lifecycle` 等）、room 命名 `thread:<id>` 全部不变。
- → 前端与自动生成 SDK **零改动**。

### 6.2 SQLite schema 复用
- 不重新设计表，直接跑 `drizzle/*.sql`。
- rusqlice 执行时按 `--> statement-breakpoint` 拆分单语句依次执行。
- 迁移记录机制：可沿用文件名约定 + 一张 `_migrations` 追踪表（或引入 `refinery`）。

### 6.3 codex 类型来源
- 优先手翻 `src/codex/dto/v2/`（本就是手写镜像）。
- `codex-schema`（generate-ts 产物）：调研 codex 是否提供 JSON schema，若有则用 `quicktype` / `schemafy` 生成 Rust，否则继续手翻并随协议演进手动同步。

### 6.4 实时架构
- socketioxide 提供 Socket.IO server 协议实现，支持 namespace、rooms、ack。
- codex 通知路由：从 notification.params 取 `threadId`，有则 `to(room)`，无则广播——与 TS `handleCodexNotification` 完全一致。
- 审批（服务端请求）：首个响应的客户端胜出，结果回传 app-server；REST 响应走持久化 CAS 语义（pending-approvals）。

### 6.5 终端：wezterm-term VT 重连（带 SGR 颜色序列化）
- **决策**：终端功能完整保留（多 tab、PTY 流、共享会话）。重连采用 `wezterm-term` VT 模型：PTY 输出实时驱动 VT，重连 / 下载时序列化 VT 屏幕（回滚 + 可见行）。
- **保真度**：序列化遍历每个 cell 的 `attrs`（foreground / background / intensity / underline / italic / reverse），在样式变化处输出 SGR 转义序列（truecolor `\x1b[38;2;r;g;b m` / 256 色 `38;5;n` / bold `1` / italic `3` / underline `4` / reverse `7` 等）。重连时 xterm.js `term.write(state)` 可恢复**彩色 + 样式**的画面（对齐 TS xterm `SerializeAddon` 的可观测行为）。
- **不还原**：光标精确位置、alternate-screen buffer 切换、blink 等次要状态——重连后光标位于末尾；全屏 TUI（vim / htop）的 alternate-screen 仍需用户触发重绘（非目标，用户已知悉）。
- **依赖**：`wezterm-term`（VT 模型）+ `portable-pty`（PTY / 进程管理，不同职责，不可由 wezterm-term 替代）。

### 6.6 进程生命周期与 generation
- 完整复刻 `generation` 机制：app-server 每次重启 generation+1，generation-scoped 缓存（如 pending-approvals 的 `(generation, requestId)` 主键）必须正确。
- 重启退避（现 3000ms 固定）、destroy 语义、事件转发器跨重启保持注册。

### 6.7 日志与脱敏
- `tracing` 结构化日志，级别由 `LOG_LEVEL` 控制。
- **脱敏以 `app.module.ts` 的 `PINO_REDACT` 为权威对照源**：`req.headers.authorization/cookie`、`req.query.access_token`、`res.headers.set-cookie`、`token/accessToken/apiKey/password` 及通配 `*.token/*.accessToken/*.apiKey/*.password`；URL 序列化仅剥离 `access_token` query 参数。
- codex JSON-RPC 双向流量继续写 `logs/codex-jsonrpc.jsonl`（`{ts, dir, msg}` 每行一 JSON）。

---

## 7. 校验与验收策略

"生产可用"靠**行为对照**保证，TS 后端为参考基准：

1. **OpenAPI diff**：utoipa 产出的 spec 与现有 NestJS swagger 做 diff，路由 / schema / operationId 必须等价。
2. **Jest spec → parity 清单**：现有 `.spec.ts` 的断言转成 Rust 集成测试 + 跨后端对照测试（同输入比对响应）。
3. **并排运行**：两后端连同一份 codex / 同一 DB 副本，对同一前端流量录制回放，diff 响应与 WS 事件序列。
4. **codex 端到端**：Phase 1 完成后立即用真实 `codex app-server` 验 JSON-RPC 握手 / 通知 / 审批往返。
5. **性能基线**：内存（RSS）、冷启动、并发吞吐与 TS 版对比，量化 A 目标收益。
6. **错误码对照表**：逐条核对 `ErrorCode` 在 Rust 侧的映射，前端 i18n 不破。

---

## 8. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| codex 协议演进，手翻类型滞后 | 运行时反序列化失败 | 调研 JSON schema 自动生成；未识别字段用 `#[serde(flatten)]` 兜底 `Value` |
| Socket.IO 协议细节（ack / 二进制 / 版本协商）socketioxide 不完全一致 | 前端实时断流 | Phase 3 起用真实前端联调；保留 TS 版可回退 |
| 错误码 / 响应结构细微偏差 | 前端处理错乱 | OpenAPI diff + 错误码对照表（§7） |
| rusqlite 与 better-sqlite3 行为差异（如并发、WAL、Blob） | 数据一致性 | 同 schema 同 SQL，开启 WAL；关键路径集成测试 |
| 终端重连体验降级 | 用户感知 | 已声明为非目标；后续可用 wezterm-term 增强 |
| 迁移周期长、中间双维护 | 进度风险 | 模块化、Phase 2 批量模板化；每阶段独立可验、可暂停 |

---

## 9. 开放问题（实现期再定）

1. codex 是否提供机器可读 schema？决定类型是手翻还是自动生成（§6.3）。
2. 单 crate 还是多 crate workspace？建议起步单 crate，files / codex 过大时再拆。
3. 迁移记录用 `refinery` 还是自建 `_migrations` 表？（§6.2）
4. Docker 镜像：musl 静态 vs glibc 动态？（影响 B 目标镜像大小，二期优化）

---

## 10. 终态与切换

- 终态：单一 Rust 二进制，**前端静态产物经 `rust-embed`/`include_dir` 内嵌**，监听 `0.0.0.0:8172`，对外 API/WS 契约不变。
- 切换：灰度（反代按比例分流）→ 全量 → 下线 TS 后端。
- Docker：多阶段构建，最终镜像仅含 Rust 二进制 + 前端静态产物 + codex 运行时（镜像显著缩小，契合 B）。

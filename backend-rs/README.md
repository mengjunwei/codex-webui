# backend-rs

Codex WebUI 的 Rust 后端（替代 `../src` 中的 NestJS 后端）。

## 功能

- **多租户 SaaS**：用户注册/登录/团队管理，per-user workspace 隔离
- **Codex 进程池**：per-team codex app-server 进程，共享全局 CODEX_HOME，JSON-RPC over stdio
- **多节点 HA**：Redis/Memberlist 探活 + session 级 rollout 增量复制 + 副本晋升
- **Hook Webhook**：codex 工具调用前后回调，权限校验 + 审计落库
- **实时通信**：Socket.IO WebSocket 网关，codex 通知实时推送前端
- **文件系统**：多根工作区 + 路径安全边界（防 symlink 逃逸）
- **终端**：共享 PTY 会话（wezterm VT 模拟），支持重连

## 快速开始

```bash
# 1. 复制配置模板
cp config.toml.example config.toml
# 编辑 config.toml，填入必要字段

# 2. 启动（需要 PG + 可选 Redis）
cargo run --release

# 3. 带 memberlist 多节点支持
cargo run --release --features memberlist-backend
```

## 配置

纯 TOML，无环境变量回退。详见 `config.toml.example`。

配置文件查找顺序：
1. `$CODEX_WEBUI_CONFIG`
2. `$CODEX_HOME/config.toml`
3. `./config.toml`
4. `$HOME/.codex-webui/config.toml`

## 文档

- [ARCHITECTURE.md](./ARCHITECTURE.md) — 完整架构文档（基于源码逐模块验证）

## 测试

```bash
cargo test --lib           # 70 lib 单测
cargo test --tests         # 6 集成测试
cargo test --features memberlist-backend  # 带 memberlist
```

## 技术栈

Rust 2024 · axum 0.8 · SeaORM 1.1（PG/MySQL）· Redis · tokio · memberlist 0.8.5

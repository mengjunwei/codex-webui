//! Codex WebUI 后端 —— NestJS 后端的 Rust 重写版。
//!
//! 本 crate 汇总了所有模块，供二进制程序（`main.rs`）和
//! 集成测试（`tests/*.rs`）共同使用。每个模块在此声明为 `pub`，
//! 测试即可通过 `use codex_webui::<module>::*` 导入。

// ── 基础设施 ──────────────────────────────────────────────────────────
pub mod config;
pub mod error;
pub mod logging;
pub mod state;

// ── 三层架构 ──────────────────────────────────────────────────────────
pub mod api;       // Handler 层：HTTP 路由、请求解析、响应构建
pub mod services;  // Service 层：业务逻辑编排
pub mod db;        // DB 层：SeaORM Entity + Migration

// ── 领域模块 ──────────────────────────────────────────────────────────
pub mod auth;        // 认证中间件（JWT/API key 校验）
pub mod codex;       // Codex RPC 客户端（进程管理 + JSON-RPC）
pub mod multitenant; // 多租户中间件 + 工具函数（now_ms / new_id）

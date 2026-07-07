//! Codex WebUI 后端 —— NestJS 后端的 Rust 重写版。
//!
//! 本 crate 汇总了所有模块，供二进制程序（`main.rs`）和
//! 集成测试（`tests/*.rs`）共同使用。每个模块在此声明为 `pub`，
//! 测试即可通过 `use codex_webui::<module>::*` 导入。

pub mod auth;
pub mod chat;
pub mod codex;
pub mod codex_status;
pub mod codex_status_config;
pub mod config;
pub mod db;
pub mod error;
pub mod event_subscribers;
pub mod files;
pub mod logs;
pub mod logging;
pub mod onlyoffice;
pub mod proxies;
pub mod realtime;
pub mod routes;
pub mod settings;
pub mod sqlite_handlers;
pub mod state;
pub mod terminal;
pub mod threads;

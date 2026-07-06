//! Codex WebUI backend — Rust rewrite of the NestJS backend.
//!
//! This crate aggregates all modules for both the binary (`main.rs`) and
//! integration tests (`tests/*.rs`). Each module is `pub` here so tests can
//! import via `use codex_webui::<module>::*`.

pub mod auth;
pub mod codex;
pub mod config;
pub mod db;
pub mod error;
pub mod event_subscribers;
pub mod logs;
pub mod logging;
pub mod proxies;
pub mod routes;
pub mod settings;
pub mod sqlite_handlers;
pub mod state;

//! Proxy stubs for Phase 2 modules that depend on the codex JSON-RPC client
//! (Phase 1, not yet built). Returns 501 Not Implemented until Phase 1 lands.
//!
//! These routes exist so the frontend gets a clear "not yet available" signal
//! instead of a 404 for unknown paths.

use crate::error::{AppError, ErrorCode};
use axum::Json;
use serde_json::Value;

fn not_implemented(method: &str) -> AppError {
    AppError::business(
        ErrorCode::HttpInternalError,
        axum::http::StatusCode::NOT_IMPLEMENTED,
        format!("{method} requires the codex JSON-RPC client (Phase 1)"),
        None,
    )
}

async fn not_implemented_handler() -> Result<Json<Value>, AppError> {
    Err(not_implemented("this endpoint"))
}

// ── account ──────────────────────────────────────────────────────────────────

pub async fn account_read() -> Result<Json<Value>, AppError> {
    Err(not_implemented("GET /account"))
}
pub async fn account_login() -> Result<Json<Value>, AppError> {
    Err(not_implemented("POST /account/login"))
}
pub async fn account_login_cancel() -> Result<Json<Value>, AppError> {
    Err(not_implemented("POST /account/login/cancel"))
}
pub async fn account_logout() -> Result<Json<Value>, AppError> {
    Err(not_implemented("POST /account/logout"))
}
pub async fn account_rate_limits() -> Result<Json<Value>, AppError> {
    Err(not_implemented("GET /account/rate-limits"))
}

// ── apps ─────────────────────────────────────────────────────────────────────

pub async fn apps_list() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}

// ── models ───────────────────────────────────────────────────────────────────

pub async fn models_list() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}

// ── mcp-servers ──────────────────────────────────────────────────────────────

pub async fn mcp_servers_list() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn mcp_servers_reload() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn mcp_servers_oauth_login() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}

// ── skills ───────────────────────────────────────────────────────────────────

pub async fn skills_list() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn skills_config_write() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}

// ── pending-approvals (respond — needs Phase 1 client to forward to app-server) ──

pub async fn pending_approvals_respond() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}

// ── plugins ──────────────────────────────────────────────────────────────────

pub async fn plugins_list() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn plugins_detail() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn plugins_install() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
pub async fn plugins_uninstall() -> Result<Json<Value>, AppError> {
    not_implemented_handler().await
}
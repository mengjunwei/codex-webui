//! `POST /api/auth/login` — 用部署 API key 换取短期 JWT。
//!
//! 与 `src/auth/auth.controller.ts:login` 对齐。公开(无需鉴权)。

use crate::auth::{LoginRequest, LoginResponse};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{extract::State, http::StatusCode};
use crate::error::Json;

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    if !state.auth.validate_api_key(&req.api_key) {
        tracing::warn!(auth_type = "apiKeyLogin", reason = "invalidApiKey", "auth");
        return Err(AppError::business(
            ErrorCode::AuthInvalidApiKey,
            StatusCode::UNAUTHORIZED,
            "Invalid API key".into(),
            None,
        ));
    }

    tracing::info!(auth_type = "apiKeyLogin", reason = "loginSuccess", "auth");
    Ok(Json(state.auth.sign_jwt()?))
}

/// 无状态登出 — 浏览器清除已存储的 JWT。返回 204 No Content。
pub async fn logout() -> StatusCode {
    StatusCode::NO_CONTENT
}

//! `POST /api/auth/login` — 用部署 API key 换取短期 JWT。
//!
//! 与 `src/auth/auth.controller.ts:login` 对齐。公开(无需鉴权)。

use crate::auth::{LoginRequest, LoginResponse};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{extract::State, http::StatusCode};
use crate::error::Json;

#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "登录成功，返回短期 JWT", body = LoginResponse),
        (status = 401, description = "API key 无效", body = crate::error::ErrorResponse),
    )
)]
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
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "登出成功（无状态，浏览器清除本地 JWT）"),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn logout() -> StatusCode {
    StatusCode::NO_CONTENT
}

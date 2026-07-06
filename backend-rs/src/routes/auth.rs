//! `POST /api/auth/login` — exchange the deployment API key for a short-lived JWT.
//!
//! Parity with `src/auth/auth.controller.ts:login`. Public (no auth required).

use crate::auth::{LoginRequest, LoginResponse};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{extract::State, http::StatusCode, Json};

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

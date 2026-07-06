//! Axum auth middleware — parity with `src/auth/api-key.guard.ts`.
//!
//! Control flow:
//! 1. Extract bearer token from `Authorization: Bearer <token>` header.
//!    If absent, check for `access_token` query param (JWT only) on inline
//!    file-preview GET paths (`/api/files/serve`, `/api/files/archive/entry`).
//! 2. `authenticate_token(token)`: try JWT first, then API key (constant-time).
//! 3. On failure: return 401 `{ statusCode, errorCode, message }`.

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware::Next,
    response::Response,
};

/// Axum middleware: reject unauthenticated requests with 401.
pub async fn require_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let (token, is_query_source) = extract_token(&req);

    let token = match token {
        Some(t) => t,
        None => {
            return Err(AppError::unauthorized(
                ErrorCode::AuthMissingHeader,
                "Missing or invalid Authorization header",
            ))
        }
    };

    // Query-sourced tokens must be valid JWTs (no raw API key in URLs).
    let ok = if is_query_source {
        state.auth.verify_jwt(&token).unwrap_or(false)
    } else {
        state.auth.authenticate_token(Some(&token), None).ok
    };

    if !ok {
        return Err(AppError::unauthorized(
            ErrorCode::AuthInvalidToken,
            "Invalid authentication token",
        ));
    }

    Ok(next.run(req).await)
}

/// Extract token and its source from the request.
/// Returns `(Some(token_string), is_query_source)` or `(None, false)`.
fn extract_token(req: &Request<Body>) -> (Option<String>, bool) {
    // 1. Authorization: Bearer <token>
    if let Some(header) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = header.strip_prefix("Bearer ") {
            let t = rest.trim();
            if !t.is_empty() {
                return (Some(t.to_string()), false);
            }
        }
    }

    // 2. Query fallback: `?access_token=<jwt>` on inline preview endpoints only.
    if req.method() == axum::http::Method::GET && allows_query_token(req.uri().path()) {
        if let Some(query) = req.uri().query() {
            for pair in query.split('&') {
                if let Some(val) = pair.strip_prefix("access_token=") {
                    let val = val.trim();
                    // Query tokens must look like JWTs (3 dot-separated parts).
                    if !val.is_empty() && val.split('.').count() == 3 {
                        return (Some(val.to_string()), true);
                    }
                }
            }
        }
    }

    (None, false)
}

/// Only allow `?access_token=` on GET inline file preview paths.
/// Parity with `api-key.guard.ts:allowsQueryAccessToken`.
fn allows_query_token(path: &str) -> bool {
    matches!(
        path,
        "/api/files/serve" | "/api/files/archive/entry"
    ) || path.starts_with("/api/files/serve?")
        || path.starts_with("/api/files/archive/entry?")
}

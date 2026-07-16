//! 多租户认证中间件:校验 access JWT 并把 user_id 注入请求扩展。

use crate::error::{AppError, ErrorCode};
use crate::services::multitenant::auth::verify_access;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request};
use axum::middleware::Next;
use axum::response::Response;

/// 已认证的 user_id,通过请求扩展在 handler 间传递(handler 用 `Extension<UserId>` 取)。
#[derive(Debug, Clone)]
pub struct UserId(pub String);

/// 多租户受保护路由的鉴权中间件:提取 bearer access token -> 校验 -> 注入 UserId。
///
/// 缺/坏 token -> 401。多租户持久化层(DatabaseConnection)在 AppState 必选,不再有 503 分支。
pub async fn require_user_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let token = extract_bearer(&req).ok_or_else(|| {
        AppError::unauthorized(ErrorCode::AuthMissingHeader, "missing bearer token")
    })?;
    let user_id = verify_access(&token, state.auth.jwt_secret())?;
    let mut req = req;
    req.extensions_mut().insert(UserId(user_id));
    Ok(next.run(req).await)
}

/// 从 `Authorization: Bearer <token>` 提取 token(scheme 大小写不敏感)。
fn extract_bearer(req: &Request<Body>) -> Option<String> {
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let lower = h.to_ascii_lowercase();
    lower.strip_prefix("bearer ")?;
    let t = h[7..].trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
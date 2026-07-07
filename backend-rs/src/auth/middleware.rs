//! Axum 认证中间件 —— 与 `src/auth/api-key.guard.ts` 对齐。
//!
//! 控制流：
//! 1. 从 `Authorization: Bearer <token>` 头中提取 bearer token。
//!    若不存在，则在内联文件预览的 GET 路径（`/api/files/serve`、
//!    `/api/files/archive/entry`）上检查 `access_token` 查询参数（仅限 JWT）。
//! 2. `authenticate_token(token)`：先尝试 JWT，再尝试 API key（恒定时间比较）。
//! 3. 失败时：返回 401 `{ statusCode, errorCode, message }`。

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware::Next,
    response::Response,
};

/// Axum 中间件：以 401 拒绝未认证的请求。
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

    // 来源于查询参数的 token 必须是合法 JWT（URL 中不接受原始 API key）。
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

/// 从请求中提取 token 及其来源。
/// 返回 `(Some(token_string), is_query_source)` 或 `(None, false)`。
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

    // 2. 查询参数回退：仅在内联预览端点上支持 `?access_token=<jwt>`。
    if req.method() == axum::http::Method::GET && allows_query_token(req.uri().path()) {
        if let Some(query) = req.uri().query() {
            for pair in query.split('&') {
                if let Some(val) = pair.strip_prefix("access_token=") {
                    let val = val.trim();
                    // 查询参数 token 必须形似 JWT（3 个以点分隔的部分）。
                    if !val.is_empty() && val.split('.').count() == 3 {
                        return (Some(val.to_string()), true);
                    }
                }
            }
        }
    }

    (None, false)
}

/// 仅允许在 GET 内联文件预览路径上使用 `?access_token=`。
/// 与 `api-key.guard.ts:allowsQueryAccessToken` 对齐。
/// 注意：`req.uri().path()` 不会包含 `?`，因此此处只做精确路径匹配；
/// 查询参数的处理在调用方获取 `req.uri().query()` 之后进行。
fn allows_query_token(path: &str) -> bool {
    matches!(path, "/api/files/serve" | "/api/files/archive/entry")
}

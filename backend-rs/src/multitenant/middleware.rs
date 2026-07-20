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

/// 平台管理员 gate:要求 `require_user_auth` 已注入的 `UserId` 是 `is_platform_admin`,否则 403。
///
/// 必须挂在 `require_user_auth` 之后(由后者负责注入 `UserId` 扩展)。
/// 用于收紧全局敏感写操作(全局 settings、全局 logs、公共工作区 files 写)为平台管理员专属。
pub async fn require_platform_admin_layer(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let uid = req
        .extensions()
        .get::<UserId>()
        .cloned()
        .ok_or_else(|| AppError::unauthorized(ErrorCode::AuthMissingHeader, "missing auth"))?;
    crate::services::multitenant::permissions::require_platform_admin(&state.db, &uid.0).await?;
    Ok(next.run(req).await)
}

/// 文件内联预览/OnlyOffice 下载专用鉴权(给 /api/files/serve 与 /api/files/archive/entry)。
///
/// 这两个端点必须支持 `?access_token=` 查询参数:<img>/<video>/<pdf> 标签与 OnlyOffice
/// Document Server 都无法带 `Authorization` 头。下载 token 是 onlyoffice 签发的
/// sub="webui" 短期(5min) JWT;同时仍接受多租户 bearer(sub=user_id)。
///
/// 回归背景:此前 /api/* 走旧 require_auth(支持 query token),统一多租户认证切到
/// require_user_auth(只读 Authorization 头)后,query token 401,OnlyOffice/预览整体不可用。
pub async fn require_file_access(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    use axum::http::Method;
    // 1. Authorization: Bearer <多租户 access JWT>。
    if let Some(token) = extract_bearer(&req) {
        if let Ok(user_id) = verify_access(&token, state.auth.jwt_secret()) {
            let mut req = req;
            req.extensions_mut().insert(UserId(user_id));
            return Ok(next.run(req).await);
        }
    }
    // 2. ?access_token=<token>(仅 GET)。接受两种:
    //    (a) 前端内联预览(<img>/<video>/<pdf>)传入的多租户 access JWT(sub=user_id);
    //    (b) OnlyOffice Document Server 下载用的 download token(sub="webui",onlyoffice 签发)。
    if req.method() == Method::GET {
        if let Some(token) = extract_query_access_token(&req) {
            if verify_access(&token, state.auth.jwt_secret()).is_ok()
                || state.auth.verify_jwt(&token).unwrap_or(false)
            {
                return Ok(next.run(req).await);
            }
        }
    }
    Err(AppError::unauthorized(
        ErrorCode::AuthInvalidToken,
        "invalid authentication token",
    ))
}

/// 从查询参数提取 access_token(仅形似 JWT:3 段点分隔)。
fn extract_query_access_token(req: &Request<Body>) -> Option<String> {
    let query = req.uri().query()?;
    for pair in query.split('&') {
        if let Some(val) = pair.strip_prefix("access_token=") {
            let val = val.trim();
            if !val.is_empty() && val.split('.').count() == 3 {
                return Some(val.to_string());
            }
        }
    }
    None
}
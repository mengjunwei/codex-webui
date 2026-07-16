//! 多租户 HTTP handler:认证(register / login / refresh)+ team 管理。
//!
//! 公开路由:register / login / refresh。
//! 受 `require_user_auth` 保护(已注入 UserId):create_team / list_teams / list_members /
//! create_invitation / join_team / remove_member。

use crate::error::{AppError, ErrorCode};
use crate::multitenant::middleware::UserId;
use crate::multitenant::models::User;
use crate::multitenant::{auth, teams};
use crate::state::AppState;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// 取多租户连接池;未配置 → 503。
fn require_pool(state: &AppState) -> Result<&PgPool, AppError> {
    state.mt_pg.as_ref().ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpRequestFailed,
            StatusCode::SERVICE_UNAVAILABLE,
            "multitenant not configured".into(),
            None,
        )
    })
}

// ── 请求体 ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterBody {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct RefreshBody {
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
}

#[derive(Deserialize)]
pub struct CreateTeamBody {
    pub name: String,
}

#[derive(Deserialize)]
pub struct JoinBody {
    pub code: String,
}

#[derive(Deserialize)]
pub struct CreateInvitationBody {
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<i64>,
    #[serde(rename = "maxUses")]
    pub max_uses: Option<i32>,
}

// ── 响应体 ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct UserResp {
    pub id: String,
    pub email: String,
    pub display_name: Option<String>,
}

impl From<User> for UserResp {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            email: u.email,
            display_name: u.display_name,
        }
    }
}

#[derive(Serialize)]
pub struct AuthResp {
    pub user: UserResp,
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresIn")]
    pub expires_in: i64,
}

#[derive(Serialize)]
pub struct RefreshResp {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresIn")]
    pub expires_in: i64,
}

// ── 认证 handler(公开)───────────────────────────────────────────────────

pub async fn register(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<RegisterBody>,
) -> Result<Json<AuthResp>, AppError> {
    let pool = require_pool(&state)?;
    let secret = state.auth.jwt_secret();
    let user = auth::register_user(pool, &body.email, &body.password).await?;
    let tokens = auth::issue_tokens(&user.id, pool, secret).await?;
    Ok(Json(AuthResp {
        user: user.into(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

pub async fn login(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<LoginBody>,
) -> Result<Json<AuthResp>, AppError> {
    let pool = require_pool(&state)?;
    let (user, tokens) =
        auth::login(pool, state.auth.jwt_secret(), &body.email, &body.password).await?;
    Ok(Json(AuthResp {
        user: user.into(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

pub async fn refresh(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<RefreshBody>,
) -> Result<Json<RefreshResp>, AppError> {
    let pool = require_pool(&state)?;
    let tokens = auth::refresh_tokens(pool, state.auth.jwt_secret(), &body.refresh_token).await?;
    Ok(Json(RefreshResp {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

// ── team handler(受 require_user_auth 保护,UserId 已注入)────────────────

pub async fn create_team(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<CreateTeamBody>,
) -> Result<Json<teams::Team>, AppError> {
    let pool = require_pool(&state)?;
    Ok(Json(teams::create_team(pool, &uid.0, &body.name).await?))
}

pub async fn list_teams(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<Vec<teams::Team>>, AppError> {
    let pool = require_pool(&state)?;
    Ok(Json(teams::list_my_teams(pool, &uid.0).await?))
}

pub async fn list_members(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<teams::MemberView>>, AppError> {
    let pool = require_pool(&state)?;
    teams::require_member(pool, &team_id, &uid.0).await?;
    Ok(Json(teams::list_members(pool, &team_id).await?))
}

pub async fn create_invitation(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<CreateInvitationBody>,
) -> Result<Json<teams::Invitation>, AppError> {
    let pool = require_pool(&state)?;
    teams::require_owner(pool, &team_id, &uid.0).await?;
    Ok(Json(teams::create_invitation(
        pool,
        &team_id,
        &uid.0,
        body.expires_at,
        body.max_uses,
    )
    .await?))
}

pub async fn join_team(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<JoinBody>,
) -> Result<Json<teams::Team>, AppError> {
    let pool = require_pool(&state)?;
    Ok(Json(teams::join_team(pool, &uid.0, &body.code).await?))
}

pub async fn remove_member(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let pool = require_pool(&state)?;
    teams::require_owner(pool, &team_id, &uid.0).await?;
    teams::remove_member(pool, &team_id, &user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

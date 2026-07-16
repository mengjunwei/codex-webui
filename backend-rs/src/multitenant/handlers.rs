//! 多租户 HTTP handler:认证(register / login / refresh)+ team 管理。
//!
//! 公开路由:register / login / refresh。
//! 受 `require_user_auth` 保护(已注入 UserId):create_team / list_teams / list_members /
//! create_invitation / join_team / remove_member。
//!
//! 数据访问统一通过 SeaORM(`&DatabaseConnection`)操作 multitenant schema 下的 8 张表;
//! 业务 entity 直接读 `entity::thread::*` 等子模块,不再依赖旧 `models::FromRow`。

use crate::error::{AppError, ErrorCode};
use crate::multitenant::entity::thread::{ActiveModel as ThreadActiveModel, Column as ThreadColumn, Entity as ThreadEntity};
use crate::multitenant::entity::team_api_key::Model as TeamApiKey;
use crate::multitenant::entity::user::Model as User;
use crate::multitenant::middleware::UserId;
use crate::multitenant::{api_keys, audit, auth, teams};
use crate::state::AppState;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 从 X-Forwarded-For 取客户端 IP(取第一个;无则 "unknown")。
fn client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// 取多租户共用 DB 连接。pg 已为必选字段,直接借用 &state.db。
fn require_db(state: &AppState) -> &DatabaseConnection {
    &state.db
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
    headers: axum::http::HeaderMap,
    crate::error::Json(body): crate::error::Json<RegisterBody>,
) -> Result<Json<AuthResp>, AppError> {
    let db = require_db(&state);
    // M6-A 注册限流(防滥用):按 IP 每分钟 10 次;Redis 未配置则跳过。
    if let Some(client) = &state.mt_redis {
        let ip = client_ip(&headers);
        let limiter = crate::multitenant::rate_limit::RedisRateLimiter::new(client.clone());
        if !limiter.allow(&format!("rl:register:{ip}"), 10, 60).await? {
            return Err(AppError::status(429));
        }
    }
    metrics::counter!("mt_registrations_total").increment(1);
    let secret = state.auth.jwt_secret();
    let user = auth::register_user(db, &body.email, &body.password).await?;
    let tokens = auth::issue_tokens(&user.id, db, secret).await?;
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
    let db = require_db(&state);
    let (user, tokens) =
        auth::login(db, state.auth.jwt_secret(), &body.email, &body.password).await?;
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
    let db = require_db(&state);
    let tokens = auth::refresh_tokens(db, state.auth.jwt_secret(), &body.refresh_token).await?;
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
    let db = require_db(&state);
    Ok(Json(teams::create_team(db, &uid.0, &body.name).await?))
}

pub async fn list_teams(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<Vec<teams::Team>>, AppError> {
    let db = require_db(&state);
    Ok(Json(teams::list_my_teams(db, &uid.0).await?))
}

pub async fn list_members(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<teams::MemberView>>, AppError> {
    let db = require_db(&state);
    teams::require_member(db, &team_id, &uid.0).await?;
    Ok(Json(teams::list_members(db, &team_id).await?))
}

pub async fn create_invitation(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<CreateInvitationBody>,
) -> Result<Json<teams::Invitation>, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    let inv = teams::create_invitation(db, &team_id, &uid.0, body.expires_at, body.max_uses)
        .await?;
    audit::record(db, &team_id, &uid.0, "invitation_created", None).await;
    Ok(Json(inv))
}

pub async fn join_team(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<JoinBody>,
) -> Result<Json<teams::Team>, AppError> {
    let db = require_db(&state);
    Ok(Json(teams::join_team(db, &uid.0, &body.code).await?))
}

pub async fn remove_member(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    teams::remove_member(db, &team_id, &user_id).await?;
    audit::record(db, &team_id, &uid.0, "member_removed", Some(&user_id)).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── team API key(BYOK,owner only)─────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetKeyBody {
    pub key: String,
    pub provider: Option<String>,
}

/// key 响应(不含密文,只暴露 hint)。
#[derive(Serialize)]
pub struct ApiKeyResp {
    pub id: String,
    pub provider: String,
    pub key_hint: String,
    pub is_active: bool,
    pub created_at: i64,
}

impl From<TeamApiKey> for ApiKeyResp {
    fn from(k: TeamApiKey) -> Self {
        Self {
            id: k.id,
            provider: k.provider,
            key_hint: k.key_hint,
            is_active: k.is_active,
            created_at: k.created_at,
        }
    }
}

/// 设置/轮换 team 的 OpenAI key(owner):先调 OpenAI 验证 → AES-GCM 加密落库 → 旧 key 失活。
pub async fn set_team_api_key(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<SetKeyBody>,
) -> Result<Json<ApiKeyResp>, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    let provider = body.provider.unwrap_or_else(|| "openai".into());
    let k = api_keys::set_team_api_key(
        db,
        &team_id,
        &uid.0,
        &body.key,
        &provider,
        &state.mt_master_key,
    )
    .await?;
    audit::record(db, &team_id, &uid.0, "api_key_set", Some(&k.key_hint)).await;
    Ok(Json(k.into()))
}

/// 列出 team 的全部 key(owner,只返回 hint,不含密文)。
pub async fn list_team_api_keys(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<ApiKeyResp>>, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    let keys = api_keys::list_team_api_keys(db, &team_id).await?;
    Ok(Json(keys.into_iter().map(Into::into).collect()))
}

// ── 多租户 threads / turns(M3,经 TeamCodexManager)────────────────────────

#[derive(Deserialize)]
pub struct TeamIdQuery {
    #[serde(rename = "teamId")]
    pub team_id: String,
}

#[derive(Deserialize)]
pub struct MtCreateThreadBody {
    #[serde(rename = "teamId")]
    pub team_id: String,
    /// 透传给 codex thread/start 的其余字段(model/cwd/...)。
    #[serde(flatten)]
    pub rest: serde_json::Map<String, Value>,
}

/// 校验 thread 属于某 team 且 user 是该 team 成员,返回 team_id。
async fn require_thread_team(
    db: &DatabaseConnection,
    thread_id: &str,
    user_id: &str,
) -> Result<String, AppError> {
    let row = ThreadEntity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query thread team: {e}")))?;
    let team_id = match row {
        Some(t) => t.team_id,
        None => {
            return Err(AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "thread not found".into(),
                None,
            ))
        }
    };
    teams::require_member(db, &team_id, user_id).await?;
    Ok(team_id)
}

/// 创建会话:成员校验 → 按 team 启动 codex → thread/start → PG 元数据双写。
pub async fn mt_create_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<MtCreateThreadBody>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    teams::require_member(db, &body.team_id, &uid.0).await?;
    metrics::counter!("mt_threads_created_total").increment(1);
    let client = state
        .mt_team_codex
        .client_for(&body.team_id, db, &state.mt_master_key)
        .await?;
    let resp = client
        .request("thread/start", Some(Value::Object(body.rest)))
        .await
        .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?;

    // PG threads 元数据双写(尽力提取 thread id;失败不阻塞)。
    let thread_id = resp
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| resp.get("threadId").and_then(Value::as_str));
    if let Some(tid) = thread_id {
        let now = crate::multitenant::now_ms();
        // 多方言一致:先 find_by_id,不存在则 insert(thread id 主键冲突直接跳过)。
        match ThreadEntity::find_by_id(tid.to_string()).one(db).await {
            Ok(Some(_)) => { /* 已存在,跳过双写 */ }
            Ok(None) => {
                let am = ThreadActiveModel {
                    id: Set(tid.to_string()),
                    team_id: Set(body.team_id.clone()),
                    created_by_user_id: Set(uid.0.clone()),
                    title: Set(None),
                    status: Set("active".to_string()),
                    created_at: Set(now),
                    updated_at: Set(now),
                    last_activity_at: Set(now),
                };
                if let Err(e) = am.insert(db).await {
                    tracing::warn!(error = %e, "insert thread meta failed (non-fatal)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "query thread meta failed (non-fatal)");
            }
        }
    }
    Ok(Json(resp))
}

/// 列出 team 会话元数据(从 PG,team 内共享,按活跃时间倒序)。
pub async fn mt_list_threads(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Query(q): Query<TeamIdQuery>,
) -> Result<Json<Vec<crate::multitenant::entity::thread::Model>>, AppError> {
    let db = require_db(&state);
    teams::require_member(db, &q.team_id, &uid.0).await?;
    let list = ThreadEntity::find()
        .filter(ThreadColumn::TeamId.eq(q.team_id.clone()))
        .order_by_desc(ThreadColumn::LastActivityAt)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list threads: {e}")))?;
    Ok(Json(list))
}

/// 对会话发起 turn:校验 thread 所属 team + 成员 → codex turn/start → 更新活跃时间。
pub async fn mt_start_turn(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
    body: axum::Json<Value>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    let team_id = require_thread_team(db, &thread_id, &uid.0).await?;
    metrics::counter!("mt_turns_total").increment(1);
    let client = state
        .mt_team_codex
        .client_for(&team_id, db, &state.mt_master_key)
        .await?;
    let mut params = body.0;
    if let Value::Object(ref mut map) = params {
        map.entry("threadId").or_insert(Value::String(thread_id.clone()));
    }
    let resp = client
        .request("turn/start", Some(params))
        .await
        .map_err(|e| AppError::internal(format!("codex turn/start: {e}")))?;
    let now = crate::multitenant::now_ms();
    // 仅更新 last_activity_at / updated_at;非阻塞(同旧实现 `_ =` 静默吞错)。
    if let Ok(Some(model)) = ThreadEntity::find_by_id(thread_id.clone()).one(db).await {
        let mut am: ThreadActiveModel = model.into();
        am.last_activity_at = Set(now);
        am.updated_at = Set(now);
        let _ = am.update(db).await;
    }
    Ok(Json(resp))
}

// ── 审计日志(M6,owner 查询)────────────────────────────────────────────────
pub async fn list_audit(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::multitenant::entity::audit_log::Model>>, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    Ok(Json(audit::list(db, &team_id, 200).await?))
}
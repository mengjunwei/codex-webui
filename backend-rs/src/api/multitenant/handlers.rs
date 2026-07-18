//! 多租户 HTTP handler:认证(register / login / refresh)+ team 管理。
//!
//! 公开路由:register / login / refresh。
//! 受 `require_user_auth` 保护(已注入 UserId):create_team / list_teams / list_members /
//! create_invitation / join_team / remove_member。
//!
//! 数据访问统一通过 SeaORM(`&DatabaseConnection`)操作 multitenant schema 下的 8 张表;
//! 业务 entity 直接读 `entity::thread::*` 等子模块,不再依赖旧 `models::FromRow`。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::thread::{ActiveModel as ThreadActiveModel, Column as ThreadColumn, Entity as ThreadEntity};
use crate::db::entities::team_api_key::Model as TeamApiKey;
use crate::db::entities::user::Model as User;
use crate::multitenant::middleware::UserId;
use crate::services::multitenant::{api_keys, audit, auth, teams};
use crate::state::AppState;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 从 X-Forwarded-For 取客户端 IP(取第一段 = 原始客户端)。
/// 安全注意:仅在可信反向代理覆写 XFF 时可信;裸暴露时该字段可被客户端伪造,需配 trusted proxies。
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
    // M6-A 注册限流(防滥用):按 IP 每分钟 10 次;Redis 未配置跳过;Redis 故障 fail-open(不阻塞注册)。
    if let Some(client) = &state.mt_redis {
        let ip = client_ip(&headers);
        let limiter = crate::services::multitenant::rate_limit::RedisRateLimiter::new(client.clone());
        match limiter.allow(&format!("rl:register:{ip}"), 10, 60).await {
            Ok(false) => return Err(AppError::status(429)),
            Ok(true) => {}
            Err(e) => tracing::warn!(error = %e, "register rate-limit check failed, fail-open"),
        }
    }
    metrics::counter!("mt_registrations_total").increment(1);
    let secret = state.auth.jwt_secret();
    let user = auth::register_user(db, &body.email, &body.password).await?;
    let tokens = auth::issue_tokens(&user.id, db, secret).await?;
    // 注册即创建个人 workspace(per-user workspace 实施步骤 3)。
    if let Err(e) = crate::services::workspace::ensure_user_personal(&state, &user.id).await {
        tracing::warn!(error = %e, user_id = %user.id, "ensure_user_personal failed (non-fatal)");
    }
    Ok(Json(AuthResp {
        user: user.into(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    }))
}

pub async fn login(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    crate::error::Json(body): crate::error::Json<LoginBody>,
) -> Result<Json<AuthResp>, AppError> {
    let db = require_db(&state);
    // 登录限流(M6 防爆破):按 IP 每分钟 10 次;Redis 未配置跳过;Redis 故障 fail-open(不阻塞登录)。
    if let Some(client) = &state.mt_redis {
        let ip = client_ip(&headers);
        let limiter = crate::services::multitenant::rate_limit::RedisRateLimiter::new(client.clone());
        match limiter.allow(&format!("rl:login:{ip}"), 10, 60).await {
            Ok(false) => return Err(AppError::status(429)),
            Ok(true) => {}
            Err(e) => tracing::warn!(error = %e, "login rate-limit check failed, fail-open"),
        }
    }
    metrics::counter!("mt_logins_total").increment(1);
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
    let team = teams::create_team(db, &uid.0, &body.name).await?;
    // 创建 team 即建共享 workspace(per-user workspace 实施步骤 4)。
    if let Err(e) = crate::services::workspace::ensure_team_shared(&state, &team.id).await {
        tracing::warn!(error = %e, team_id = %team.id, "ensure_team_shared failed (non-fatal)");
    }
    Ok(Json(team))
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
    let team = teams::join_team(db, &uid.0, &body.code).await?;
    // 加入 team 即建成员视图目录(role 由 teams 模块写 team_members)。
    if let Err(e) =
        crate::services::workspace::ensure_team_member_view(&state, &team.id, &uid.0).await
    {
        tracing::warn!(error = %e, "ensure_team_member_view failed (non-fatal)");
    }
    Ok(Json(team))
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
    // 轮换串联(M2):踢除该 team 持有旧 key 的 codex 进程;下次请求用新 key 重启
    // (spawn 时重新解密 active key + 重写 auth.json)。
    state.mt_team_codex.evict(&team_id).await;
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

// ── 用户个人 API key(BYOK) ───────────────────────────────────────────────

/// 设置/轮换用户个人 OpenAI key。
pub async fn set_user_api_key(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<SetKeyBody>,
) -> Result<Json<ApiKeyResp>, AppError> {
    let db = require_db(&state);
    let provider = body.provider.unwrap_or_else(|| "openai".into());
    let k = api_keys::set_user_api_key(
        db,
        &uid.0,
        &body.key,
        &provider,
        &state.mt_master_key,
    )
    .await?;
    Ok(Json(ApiKeyResp {
        id: k.id,
        provider: k.provider,
        key_hint: k.key_hint,
        is_active: k.is_active,
        created_at: k.created_at,
    }))
}

/// 列出用户的全部个人 key(只返回 hint,不含密文)。
pub async fn list_user_api_keys(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<Vec<ApiKeyResp>>, AppError> {
    let db = require_db(&state);
    let keys = api_keys::list_user_api_keys(db, &uid.0).await?;
    Ok(Json(keys.into_iter().map(|k| ApiKeyResp {
        id: k.id,
        provider: k.provider,
        key_hint: k.key_hint,
        is_active: k.is_active,
        created_at: k.created_at,
    }).collect()))
}

// ── 多租户 threads / turns(M3,经 TeamCodexManager)────────────────────────

#[derive(Deserialize)]
pub struct TeamIdQuery {
    #[serde(rename = "teamId")]
    pub team_id: String,
}

/// 创建会话请求体。
/// 由于 #[serde(flatten)] 和 Option 组合可能有问题，
/// 改用 Value 接收整个 body，然后手动提取 teamId。
#[derive(Deserialize)]
pub struct MtCreateThreadBody {
    #[serde(rename = "teamId")]
    pub team_id: Option<String>,
    /// 透传给 codex thread/start 的其余字段。
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// 校验 thread 归属 + user 访问权限,返回 (team_id, workspace_type)。
/// 团队 thread:user 必须是 team 成员。个人 thread:user 必须是 created_by。
async fn require_thread_team(
    db: &DatabaseConnection,
    thread_id: &str,
    user_id: &str,
) -> Result<(String, String), AppError> {
    let row = ThreadEntity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query thread team: {e}")))?;
    let thread = match row {
        Some(t) => t,
        None => {
            return Err(AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "thread not found".into(),
                None,
            ))
        }
    };
    if thread.workspace_type == "personal" {
        if thread.created_by_user_id != user_id {
            return Err(AppError::business(
                ErrorCode::HttpForbidden,
                StatusCode::FORBIDDEN,
                "not your personal thread".into(),
                None,
            ));
        }
        return Ok((thread.team_id, thread.workspace_type));
    }
    teams::require_member(db, &thread.team_id, user_id).await?;
    Ok((thread.team_id, thread.workspace_type))
}

/// 创建会话:成员校验 → 主副本分配 → (本地直跑 / 转发主) → thread/start → PG 双写 + 主侧复制 rollout。
///
/// 支持两种模式:
/// - team workspace:传 teamId,使用团队共享 workspace + 团队 API key
/// - 个人 workspace:不传 teamId,使用用户个人 workspace + 个人 API key
pub async fn mt_create_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);

    // 手动提取 teamId，其余字段透传给 codex
    let team_id_raw = body.get("teamId").and_then(Value::as_str).map(String::from);
    let mut rest = match body {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    rest.remove("teamId"); // 不透传 teamId 给 codex

    // 确定 team_id:个人 workspace 用 "user:{user_id}" 格式标识
    let (team_id, is_personal) = match team_id_raw {
        Some(tid) => {
            teams::require_member(db, &tid, &uid.0).await?;
            (tid, false)
        }
        None => {
            // 个人 workspace:确保目录存在,用 "user:{user_id}" 格式标识
            let _ = crate::services::workspace::ensure_user_personal(&state, &uid.0).await;
            (format!("user:{}", uid.0), true)
        }
    };

    metrics::counter!("mt_threads_created_total").increment(1);

    // 对于个人 workspace,设置 cwd 到个人目录
    if is_personal {
        let personal_cwd = crate::services::workspace::personal_path(&state.codex_home, &uid.0);
        rest.insert("cwd".to_string(), Value::String(personal_cwd.to_string_lossy().to_string()));
    }

    let target = resolve_worker(&state, &team_id, None, is_personal).await?;
    let resp = if target == state.node_id {
        let lease = state
            .mt_team_codex
            .client_for(&team_id, db, &state.mt_master_key)
            .await?;
        // 对齐 main 分支旧实现:这两个参数确保 codex 持久化 rollout + 不开启 raw events。
        rest.entry("experimentalRawEvents".to_string()).or_insert(Value::Bool(false));
        rest.entry("persistExtendedHistory".to_string()).or_insert(Value::Bool(true));
        lease
            .client()
            .request("thread/start", Some(Value::Object(rest)))
            .await
            .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?
    } else {
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state
            .worker_rpc
            .thread_start(&rpc_url, &team_id, &uid.0, Value::Object(rest))
            .await?
    };

    // PG threads 元数据双写(共享库)。
    // codex thread/start 响应 result 结构:{thread:{id,cwd,...}, cwd, model, ...},
    // thread.id 即本次会话 UUID,fallback 取 threadId / 顶层 id(老版本/包装层兼容)。
    let thread_id = resp
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .or_else(|| resp.get("threadId").and_then(Value::as_str))
        .or_else(|| resp.get("id").and_then(Value::as_str));
    if let Some(tid) = thread_id {
        // PG threads.team_id 对个人 workspace 用纯 user_id(符合 VARCHAR(36));
        // workspace_type 显式区分 personal/team,路由仍按 "user:{uid}" 走。
        // 团队 workspace 用 teamId(uuid)。
        let pg_team_id: String = if is_personal { uid.0.clone() } else { team_id.clone() };
        let workspace_type = if is_personal { "personal" } else { "team" };
        double_write_thread_meta(db, tid, &pg_team_id, &uid.0, workspace_type).await;
        // 绑定粘性:确保新创建的 thread 后续 turn 路由到同一 worker。
        let _ = state.sticky.bind(tid, &target, 3600).await;
        // PG 缓存 thread/start 响应供后续 thread/resume 复用 —— 跨进程 + 重启都生效,
// 避免前端 create→resume 链路上 race codex 落盘前调 thread/resume 返回 -32600。
        let _ = crate::services::multitenant::resume_cache::put_cached_resume(
            db, tid, &resp,
        ).await;
    }
    // 包装 codex 响应为一致格式:前端期望 {thread, id, cwd} 而非扁平 codex 响应。
    let thread_id_str = thread_id.unwrap_or("");
    let cwd = resp.get("cwd").and_then(Value::as_str)
        .or_else(|| resp.get("thread").and_then(|t| t.get("cwd")).and_then(Value::as_str))
        .unwrap_or("");
    let wrapped = serde_json::json!({
        "thread": resp,
        "id": thread_id_str,
        "cwd": cwd,
    });
    // 主侧:把 thread 关联到其 rollout 文件,供 replicate_team_rollouts 精确读取。
    if target == state.node_id {
        if !thread_id_str.is_empty() {
            if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
                &state.codex_home, thread_id_str,
            )
            .await
            {
                state.active_rollout.lock().await.insert(thread_id_str.to_string(), p);
            }
        }
    }
    // 主侧:复制该 team 的 rollout 增量到副本(准实时 session 同步)。
    // 个人 workspace 跳过(个人 workspace 不参与 team 复制)。
    if target == state.node_id && !is_personal {
        let _ = crate::services::multitenant::replication::replicate_team_rollouts(
            db,
            &team_id,
            &state.codex_home,
            state.cluster.as_ref(),
            state.mt_redis.as_ref(),
            &state.worker_rpc,
            &state.active_rollout,
            &state.local_offsets,
        )
        .await;
    }
    Ok(Json(wrapped))
}

/// 列出 team 会话元数据(从 PG,team 内共享,按活跃时间倒序)。
pub async fn mt_list_threads(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Query(q): Query<TeamIdQuery>,
) -> Result<Json<Vec<crate::db::entities::thread::Model>>, AppError> {
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

/// 列出当前用户能看到的全部会话:所有团队 workspace + 个人 workspace。
/// 前端侧边栏聚合视图用(按 workspace_type 分个人/团队,再按 team_id 分组)。
/// 返回 thread model(含 workspace_type / team_id / status,前端据此分组渲染)。
pub async fn mt_list_my_threads(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<Vec<crate::db::entities::thread::Model>>, AppError> {
    let db = require_db(&state);
    // 用户加入的所有 team_id + 个人 workspace(直接用 user_id 作 team_id 命中)。
    let my_teams = teams::list_my_teams(db, &uid.0).await?;
    let mut scope_ids: Vec<String> = my_teams.iter().map(|t| t.id.clone()).collect();
    scope_ids.push(uid.0.clone());
    let list = ThreadEntity::find()
        .filter(ThreadColumn::TeamId.is_in(scope_ids))
        .order_by_desc(ThreadColumn::LastActivityAt)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list my threads: {e}")))?;
    Ok(Json(list))
}

/// 删除会话(含归档):权限校验 → codex thread/delete → 清 PG(threads/resume_cache/业务表)
/// + 删 rollout 文件 + 清 sticky。权限:个人 workspace 仅本人可删;团队 workspace 仅创建者可删。
pub async fn mt_delete_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    // 1. 查 thread + 权限校验。
    let thread = ThreadEntity::find_by_id(thread_id.clone())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query thread: {e}")))?
        .ok_or_else(|| AppError::business(
            ErrorCode::HttpNotFound, StatusCode::NOT_FOUND,
            "thread not found".into(), None))?;
    let allowed = thread.created_by_user_id == uid.0;
    if !allowed {
        return Err(AppError::business(
            ErrorCode::HttpForbidden, StatusCode::FORBIDDEN,
            "no permission to delete this thread".into(), None));
    }
    let team_id = thread.team_id.clone();
    let is_personal = thread.workspace_type == "personal";

    // 2. 调 codex thread/delete(若 codex 支持);失败不阻塞,继续清 PG/文件。
    if let Ok(target) = resolve_worker(&state, &team_id, Some(&thread_id), is_personal).await {
        if target == state.node_id {
            if let Ok(lease) = state.mt_team_codex.client_for(&team_id, db, &state.mt_master_key).await {
                if let Err(e) = lease
                    .client()
                    .request("thread/delete", Some(serde_json::json!({ "threadId": thread_id })))
                    .await
                {
                    tracing::warn!(error = %e, thread_id = %thread_id, "codex thread/delete failed (non-fatal, cleanup PG+file anyway)");
                }
            }
        } else if let Ok(rpc_url) = worker_rpc_url(&state, &target).await {
            let _ = state
                .worker_rpc
                .thread_invoke(&rpc_url, &team_id, &thread_id, "thread/delete", serde_json::json!({}))
                .await;
        }
    }

    // 3. 删 rollout 文件(codex home sessions/.../{thread_id}*.jsonl)。
    if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(&state.codex_home, &thread_id).await {
        if let Err(e) = tokio::fs::remove_file(&p).await {
            tracing::warn!(error = %e, path = %p.display(), "remove rollout file failed (non-fatal)");
        }
    }

    // 4. 清 PG:threads + thread_resume_cache + 业务表(by thread_id)。非阻塞,失败仅 warn。
    use crate::db::entity::{pending_server_request, token_usage_snapshot, turn_diff, turn_error};
    use crate::db::entities::thread_resume_cache;
    let _ = ThreadEntity::delete_by_id(thread_id.clone()).exec(db).await;
    let _ = thread_resume_cache::Entity::delete_by_id(thread_id.clone()).exec(db).await;
    let _ = token_usage_snapshot::Entity::delete_many()
        .filter(token_usage_snapshot::Column::ThreadId.eq(thread_id.clone())).exec(db).await;
    let _ = turn_diff::Entity::delete_many()
        .filter(turn_diff::Column::ThreadId.eq(thread_id.clone())).exec(db).await;
    let _ = turn_error::Entity::delete_many()
        .filter(turn_error::Column::ThreadId.eq(thread_id.clone())).exec(db).await;
    let _ = pending_server_request::Entity::delete_many()
        .filter(pending_server_request::Column::ThreadId.eq(thread_id.clone())).exec(db).await;

    // 5. 清 sticky 绑定 + 本地 active_rollout 记录。
    let _ = state.sticky.clear(&thread_id).await;
    state.active_rollout.lock().await.remove(&thread_id);

    tracing::info!(thread_id = %thread_id, team_id = %team_id, "thread deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// 对会话发起 turn:校验 thread 所属 team + 成员 → 配额 → 主副本选主节点 → (本地/转发) → 复制 rollout。
pub async fn mt_start_turn(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
    body: axum::Json<Value>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    let (team_id, workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    // 配额校验(M6):超额返回 429。
    crate::services::multitenant::quota::check_turn_quota(db, &team_id).await?;
    metrics::counter!("mt_turns_total").increment(1);
    let target = resolve_worker(&state, &team_id, Some(&thread_id), workspace_type == "personal").await?;
    let mut params = body.0;
    if let Value::Object(ref mut map) = params {
        map.entry("threadId").or_insert(Value::String(thread_id.clone()));
    }
    let resp = if target == state.node_id {
        let lease = state
            .mt_team_codex
            .client_for(&team_id, db, &state.mt_master_key)
            .await?;
        lease
            .client()
            .request("turn/start", Some(params))
            .await
            .map_err(|e| AppError::internal(format!("codex turn/start: {e}")))?
    } else {
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state
            .worker_rpc
            .turn_start(&rpc_url, &thread_id, &team_id, params)
            .await?
    };
    update_thread_activity(db, &thread_id).await;
    if let Err(e) = crate::services::multitenant::quota::incr_turn_usage(db, &team_id, None).await {
        tracing::warn!(error = %e, team_id = %team_id, "incr_turn_usage failed (non-fatal)");
    }
    // 主侧:把 thread 关联到其 rollout 文件,供 replicate_team_rollouts 精确读取。
    if target == state.node_id {
        if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
            &state.codex_home, &thread_id,
        )
        .await
        {
            state.active_rollout.lock().await.insert(thread_id.clone(), p);
        }
    }
    // 主侧:turn 完成后复制 rollout 增量到副本。
    if target == state.node_id {
        let _ = crate::services::multitenant::replication::replicate_team_rollouts(
            db,
            &team_id,
            &state.codex_home,
            state.cluster.as_ref(),
            state.mt_redis.as_ref(),
            &state.worker_rpc,
            &state.active_rollout,
            &state.local_offsets,
        )
        .await;
    }
    Ok(Json(resp))
}

/// 列表已被 Task 4 修复后保留在文件顶部(line 419);此处不留副本。
// ── 审计日志(M6,owner 查询)────────────────────────────────────────────────
pub async fn list_audit(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::db::entities::audit_log::Model>>, AppError> {
    let db = require_db(&state);
    teams::require_owner(db, &team_id, &uid.0).await?;
    Ok(Json(audit::list(db, &team_id, 200).await?))
}

// ── 多副本路由辅助 ──────────────────────────────────────────────────────

/// 选目标节点:先查粘性绑定(保证会话上下文本地性),未命中再查/分配 session_replicas。
async fn resolve_worker(state: &AppState, team_id: &str, thread_id: Option<&str>, is_personal: bool) -> Result<String, AppError> {
    // 个人 workspace:不走 session_replica HA 分配(per-team 概念不适用 personal),
    // 直接路由到本节点;sticky 也不绑(per-user 进程固定本节点)。
    if is_personal {
        return Ok(state.cluster.local_node_id().to_string());
    }
    // 1. 粘性优先:如果 thread 已绑定到某 worker 且该 worker 仍 alive,直接返回。
    if let Some(tid) = thread_id {
        if let Ok(Some(stuck_worker)) = state.sticky.lookup(tid).await {
            // 验证该 worker 仍 alive(避免路由到已死节点)。
            if crate::services::multitenant::cluster::is_alive(state.cluster.as_ref(), &stuck_worker).await {
                return Ok(stuck_worker);
            }
            // worker 已死,清除失效绑定。
            let _ = state.sticky.clear(tid).await;
        }
    }
    // 2. 回退到主副本分配。
    let row = crate::services::multitenant::replication::get_or_assign(
        &state.db,
        team_id,
        state.cluster.as_ref(),
    )
    .await?;
    // 3. 绑定粘性(如果有 thread_id)。
    if let Some(tid) = thread_id {
        // 绑定 TTL = 1 小时(活跃时在 turn 完成后续期)。
        let _ = state.sticky.bind(tid, &row.primary_node, 3600).await;
    }
    Ok(row.primary_node)
}

/// 解析节点内网 RPC 地址(转发到主节点时用)。
async fn worker_rpc_url(state: &AppState, node_id: &str) -> Result<String, AppError> {
    state
        .cluster
        .node_rpc_addr(node_id)
        .await
        .ok_or_else(|| AppError::internal(format!("no rpc addr for node {node_id}")))
}

/// threads 元数据双写:不存在则 insert(主键冲突等价跳过)。非阻塞。
async fn double_write_thread_meta(db: &DatabaseConnection, tid: &str, team_id: &str, created_by: &str, workspace_type: &str) {
    match ThreadEntity::find_by_id(tid.to_string()).one(db).await {
        Ok(Some(_)) => { /* 已存在,跳过 */ }
        Ok(None) => {
            let now = crate::services::multitenant::now_ms();
            let am = ThreadActiveModel {
                id: Set(tid.to_string()),
                team_id: Set(team_id.to_string()),
                created_by_user_id: Set(created_by.to_string()),
                title: Set(None),
                status: Set("active".to_string()),
                workspace_type: Set(workspace_type.to_string()),
                created_at: Set(now),
                updated_at: Set(now),
                last_activity_at: Set(now),
            };
            if let Err(e) = am.insert(db).await {
                tracing::warn!(error = %e, "insert thread meta failed (non-fatal)");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query thread meta failed (non-fatal)"),
    }
}

/// 更新会话活跃时间(last_activity_at / updated_at)。非阻塞。
async fn update_thread_activity(db: &DatabaseConnection, thread_id: &str) {
    let now = crate::services::multitenant::now_ms();
    if let Ok(Some(model)) = ThreadEntity::find_by_id(thread_id.to_string()).one(db).await {
        let mut am: ThreadActiveModel = model.into();
        am.last_activity_at = Set(now);
        am.updated_at = Set(now);
        if let Err(e) = am.update(db).await {
            tracing::warn!(error = %e, thread_id = %thread_id, "update thread activity failed (non-fatal)");
        }
    }
}

// ── 审批(M4 双保险):列出未处理 + resolve 回传 codex ──────────────────────

/// 列出会话的未处理审批(team 隔离;前端重连拉取,双保险)。
pub async fn mt_list_approvals(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::db::entity::pending_server_request::Model>>, AppError> {
    let db = require_db(&state);
    let (team_id, _workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    use crate::db::entity::pending_server_request::{Column as PSRColumn, Entity as PSREntity};
    let list = PSREntity::find()
        .filter(PSRColumn::TeamId.eq(team_id))
        .filter(PSRColumn::ThreadId.eq(thread_id))
        .filter(PSRColumn::Status.eq("pending"))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list approvals: {e}")))?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub struct ResolveApprovalBody {
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub approved: bool,
    pub result: Option<Value>,
}

/// 解析审批:经路由回传到持有会话的 worker 的 codex 进程,并更新 pending 状态。
pub async fn mt_resolve_approval(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<ResolveApprovalBody>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    let (team_id, workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    let target = resolve_worker(&state, &team_id, Some(&thread_id), workspace_type == "personal").await?;
    let id_val = parse_request_id(&body.request_id);
    let ok = if target == state.node_id {
        let lease = state
            .mt_team_codex
            .client_for(&team_id, db, &state.mt_master_key)
            .await?;
        if body.approved {
            lease
                .client()
                .respond_to_server_request(
                    id_val,
                    body.result.unwrap_or(Value::Object(Default::default())),
                )
                .is_ok()
        } else {
            lease
                .client()
                .respond_to_server_request_with_error(id_val, -32000, "denied by user")
                .is_ok()
        }
    } else {
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state
            .worker_rpc
            .approval_respond(
                &rpc_url,
                &team_id,
                &body.request_id,
                body.approved,
                body.result.clone(),
            )
            .await
            .is_ok()
    };
    if ok {
        // 仅在成功回传 codex 后标记已处理;失败则保留 pending 供前端重试(避免审批死锁)。
        if let Err(e) = mark_approval_resolved(db, &team_id, &body.request_id, &uid.0, body.approved).await {
            tracing::warn!(error = %e, request_id = %body.request_id, "mark_approval_resolved failed (non-fatal, pending retained for retry)");
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::internal("failed to respond to codex".into()))
    }
}

/// 字符串 request_id → codex id Value(数字优先,否则原样字符串)。
fn parse_request_id(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        Value::Number(serde_json::Number::from(n))
    } else {
        Value::String(s.to_string())
    }
}

/// 标记审批已处理(尽力,非阻塞)。
async fn mark_approval_resolved(
    db: &DatabaseConnection,
    team_id: &str,
    request_id: &str,
    user_id: &str,
    approved: bool,
) -> Result<(), AppError> {
    use crate::db::entity::pending_server_request::{ActiveModel as PSRActive, Entity as PSREntity};
    let gen_ = crate::services::multitenant::event_persist::team_generation(team_id);
    let row = PSREntity::find_by_id((gen_, request_id.to_string()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query approval: {e}")))?;
    if let Some(model) = row {
        let mut am: PSRActive = model.into();
        let now = crate::services::multitenant::now_ms();
        am.status = Set(if approved { "approved" } else { "rejected" }.to_string());
        am.resolved_by = Set(Some(user_id.to_string()));
        am.resolved_at = Set(Some(now));
        am.updated_at = Set(now);
        am.update(db)
            .await
            .map_err(|e| AppError::internal(format!("update approval status: {e}")))?;
    }
    Ok(())
}

// ── mt 会话操作补全(M4)──────────────────────────────────────────────────

/// 读取会话 token 用量(thread 维度;team 经 require_thread_team 校验)。
pub async fn mt_token_usage(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::db::entity::token_usage_snapshot::Model>>, AppError> {
    let db = require_db(&state);
    require_thread_team(db, &thread_id, &uid.0).await?;
    use crate::db::entity::token_usage_snapshot::{Column as TUCol, Entity as TUEntity};
    let list = TUEntity::find()
        .filter(TUCol::ThreadId.eq(thread_id))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list token usage: {e}")))?;
    Ok(Json(list))
}

/// 读取会话 turn diff(thread 维度)。
pub async fn mt_turn_diffs(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::db::entity::turn_diff::Model>>, AppError> {
    let db = require_db(&state);
    require_thread_team(db, &thread_id, &uid.0).await?;
    use crate::db::entity::turn_diff::{Column as TDCol, Entity as TDEntity};
    let list = TDEntity::find()
        .filter(TDCol::ThreadId.eq(thread_id))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list turn diffs: {e}")))?;
    Ok(Json(list))
}

/// 读取会话 turn 错误(thread 维度)。
pub async fn mt_turn_errors(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<Json<Vec<crate::db::entity::turn_error::Model>>, AppError> {
    let db = require_db(&state);
    require_thread_team(db, &thread_id, &uid.0).await?;
    use crate::db::entity::turn_error::{Column as TECol, Entity as TEEntity};
    let list = TEEntity::find()
        .filter(TECol::ThreadId.eq(thread_id))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list turn errors: {e}")))?;
    Ok(Json(list))
}

/// 归档会话(更新 threads.status)。
pub async fn mt_archive_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    require_thread_team(db, &thread_id, &uid.0).await?;
    if let Ok(Some(model)) = ThreadEntity::find_by_id(thread_id).one(db).await {
        let mut am: ThreadActiveModel = model.into();
        am.status = Set("archived".to_string());
        am.updated_at = Set(crate::services::multitenant::now_ms());
        am.update(db)
            .await
            .map_err(|e| AppError::internal(format!("archive thread: {e}")))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RenameThreadBody {
    pub name: String,
}

/// 重命名会话(更新 threads.title)。
pub async fn mt_rename_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<RenameThreadBody>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    require_thread_team(db, &thread_id, &uid.0).await?;
    if let Ok(Some(model)) = ThreadEntity::find_by_id(thread_id).one(db).await {
        let mut am: ThreadActiveModel = model.into();
        am.title = Set(Some(body.name));
        am.updated_at = Set(crate::services::multitenant::now_ms());
        am.update(db)
            .await
            .map_err(|e| AppError::internal(format!("rename thread: {e}")))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct InvokeThreadBody {
    pub method: String,
    pub params: Option<Value>,
}

/// 通用 codex 会话方法转发(fork / rollback / resume 等经路由到目标 worker)。
///
/// 对 `thread/resume` 与 `thread/read` 在收到 codex `-32600 no rollout found`
/// 时自动退避重试 3 次 —— codex 异步落盘,刚 `thread/start` 完立即 resume/read
/// 会撞上此 race。两方法均幂等,重试安全。
pub async fn mt_invoke_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((thread_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<InvokeThreadBody>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    let (team_id, workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    let target = resolve_worker(&state, &team_id, Some(&thread_id), workspace_type == "personal").await?;
    // thread/resume 走 PG 缓存:mt_create_thread 已写入,任意 owner 节点命中可复用,
    // 避免集群下 invoke 落到不同副本时各自走 codex RPC 触发 -32600 race。
    if body.method == "thread/resume" {
        if let Some(cached) = crate::services::multitenant::resume_cache::get_cached_resume(
            db, &thread_id,
        )
        .await
        {
            tracing::debug!(thread_id = %thread_id, "thread/resume pg cache hit");
            return Ok(Json(cached));
        }
    }
    let mut params = body.params.unwrap_or(Value::Object(Default::default()));
    if let Value::Object(ref mut m) = params {
        m.entry("threadId").or_insert(Value::String(thread_id.clone()));
        // 对齐 main 旧实现:resume 也持久化 rollout,前端后续 thread/read 才能命中。
        if body.method == "thread/resume" {
            m.entry("persistExtendedHistory".to_string()).or_insert(Value::Bool(true));
        }
    }
    const RETRY_METHODS: &[&str] = &["thread/resume", "thread/read"];
    let needs_retry = RETRY_METHODS.contains(&body.method.as_str());
    let resp = if target == state.node_id {
        let lease = state
            .mt_team_codex
            .client_for(&team_id, db, &state.mt_master_key)
            .await?;
        let mut last_err: Option<crate::codex::jsonrpc::RpcError> = None;
        let mut attempt = 0u32;
        let value = loop {
            let r = lease.client().request(&body.method, Some(params.clone())).await;
            match r {
                Ok(v) => break v,
                Err(crate::codex::jsonrpc::RpcError::ServerError { code: -32600, .. })
                    if needs_retry && attempt < 3 =>
                {
                    attempt += 1;
                    tracing::debug!(method = %body.method, thread_id = %thread_id, attempt, "codex -32600, retrying after backoff");
                    tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
                    last_err = Some(crate::codex::jsonrpc::RpcError::ServerError { code: -32600, message: String::new() });
                    continue;
                }
                Err(e) => {
                    return Err(AppError::internal(format!("codex {}: {e}", body.method)));
                }
            }
        };
        if attempt > 0 {
            tracing::info!(method = %body.method, thread_id = %thread_id, attempt, "codex recovered after retry");
        }
        let _ = last_err;
        // resume 成功也写 PG 缓存(跨节点 + 重启都可复用)
        if body.method == "thread/resume" {
            let _ = crate::services::multitenant::resume_cache::put_cached_resume(
                db, &thread_id, &value,
            ).await;
        }
        value
    } else {
        // 副本路径:转发 RPC,不在 worker 端做重试(主侧负责 retry)。
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state
            .worker_rpc
            .thread_invoke(&rpc_url, &team_id, &thread_id, &body.method, params)
            .await?
    };
    Ok(Json(resp))
}
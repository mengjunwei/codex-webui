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
use crate::db::entities::role_permission::{Column as RolePermissionColumn, Entity as RolePermissionEntity};
use crate::db::entities::team_api_key::Model as TeamApiKey;
use crate::db::entities::team_member::{Column as TeamMemberColumn, Entity as TeamMemberEntity};
use crate::db::entities::user::{Entity as UserEntity, Model as User};
use crate::multitenant::middleware::UserId;
use crate::services::multitenant::{api_keys, audit, auth, permissions, teams};
use crate::services::multitenant::permissions::TeamPermission;
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
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberList).await?;
    Ok(Json(teams::list_members(db, &team_id).await?))
}

pub async fn create_invitation(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<CreateInvitationBody>,
) -> Result<Json<teams::Invitation>, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberInvite).await?;
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
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberRemove).await?;
    teams::remove_member(db, &team_id, &user_id).await?;
    audit::record(db, &team_id, &uid.0, "member_removed", Some(&user_id)).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── 生命周期 API(owner 转让 / team 解散 / 成员角色变更)──────────────────────

#[derive(Deserialize)]
pub struct TransferOwnerBody {
    #[serde(rename = "newOwnerUserId")]
    pub new_owner_user_id: String,
}

#[derive(Deserialize)]
pub struct SetRoleBody {
    pub role: String,
}

/// 转让 team owner 给已有成员(owner→admin, member→owner)。
pub async fn transfer_team_owner(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
    crate::error::Json(body): crate::error::Json<TransferOwnerBody>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::OwnerTransfer).await?;
    teams::transfer_owner(db, &team_id, &uid.0, &body.new_owner_user_id).await?;
    audit::record(
        db,
        &team_id,
        &uid.0,
        "owner_transferred",
        Some(&body.new_owner_user_id),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// 解散 team(CASCADE 删 members / threads / keys / audit)。
/// audit 先于解散写入(team 仍存在,满足 FK),随后被 CASCADE 清理 —— 记录主要用于
/// 触发审计 side-effect(如外部监控),解散本身不可追溯属预期行为。
pub async fn dissolve_team_handler(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::TeamDissolve).await?;
    audit::record(db, &team_id, &uid.0, "team_dissolved", None).await;
    teams::dissolve_team(db, &team_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 修改成员角色(仅 member↔admin;owner 变更走 transfer_team_owner)。
pub async fn set_member_role_handler(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id, user_id)): Path<(String, String)>,
    crate::error::Json(body): crate::error::Json<SetRoleBody>,
) -> Result<StatusCode, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::MemberRoleWrite).await?;
    teams::set_member_role(db, &team_id, &user_id, &body.role).await?;
    audit::record(
        db,
        &team_id,
        &uid.0,
        "member_role_changed",
        Some(&format!("{}:{}", user_id, body.role)),
    )
    .await;
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
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::ApiKeyWrite).await?;
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
    // 单进程统一代理模式下 codex key 由全局 auth.json 管理,无需 per-team evict。
    Ok(Json(k.into()))
}

/// 列出 team 的全部 key(owner,只返回 hint,不含密文)。
pub async fn list_team_api_keys(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Path((team_id,)): Path<(String,)>,
) -> Result<Json<Vec<ApiKeyResp>>, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::ApiKeyRead).await?;
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

// ── 多租户 threads / turns(M3,经 state.codex 单进程)────────────────────────

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
pub async fn require_thread_team(
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
    permissions::require_permission(db, &thread.team_id, user_id, TeamPermission::ThreadRead).await?;
    Ok((thread.team_id, thread.workspace_type))
}

/// 创建会话:本地预生成 thread_id → per-thread workspace → 选节点(最少负载)
/// → thread/start(传 threadId + cwd)→ 登记 session_replicas + sticky → PG 双写 + 主侧复制 rollout。
///
/// 支持两种模式:
/// - team workspace:传 teamId,team_id 用于权限校验 + threads.team_id 列
/// - 个人 workspace:不传 teamId,team_id 落用户 id(personal)
///
/// 关键:thread_id 在本地预生成(UUIDv7),全系统(DB/sticky/session_replicas/cwd)统一用该 id,
/// 并通过 rest.threadId 传给 codex(参见 Step 4 codex threadId 行为验证,部署后任务)。
pub async fn mt_create_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    let team_id_raw = body.get("teamId").and_then(Value::as_str).map(String::from);
    let mut rest = match body {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    rest.remove("teamId"); // 不透传 teamId 给 codex

    // 权限校验 + team_id/workspace_type 判定(保留:用于权限 + threads.team_id 列)。
    // pg_team_id:个人 workspace 用纯 user_id(符合 VARCHAR(36));团队用 teamId(uuid)。
    let (pg_team_id, workspace_type) = match &team_id_raw {
        Some(tid) => {
            permissions::require_permission(db, tid, &uid.0, TeamPermission::ThreadCreate).await?;
            (tid.clone(), "team")
        }
        None => (uid.0.clone(), "personal"),
    };

    metrics::counter!("mt_threads_created_total").increment(1);

    // 本地预生成 thread_id(UUIDv7):用于 cwd/session_replicas/sticky/threads 表,
    // 并通过 rest.threadId 传给 codex thread/start 作为会话 id。
    let thread_id = uuid::Uuid::now_v7().to_string();

    // 统一 cwd = threads/{thread_id}/(个人/团队一致)。
    let _ = crate::services::workspace::ensure_thread_workspace(&state, &thread_id).await;
    let ws_cwd = crate::services::workspace::thread_workspace_path(&state.workspace_root, &thread_id);
    rest.insert("cwd".to_string(), Value::String(ws_cwd.to_string_lossy().to_string()));
    rest.insert("threadId".to_string(), Value::String(thread_id.clone()));

    // 两阶段:先选节点(最少负载),再 thread/start。
    let target = resolve_worker(&state, None).await?;
    let resp = if target == state.node_id {
        // 对齐 main 分支旧实现:这两个参数确保 codex 持久化 rollout + 不开启 raw events。
        rest.entry("experimentalRawEvents").or_insert(Value::Bool(false));
        rest.entry("persistExtendedHistory").or_insert(Value::Bool(true));
        state
            .codex
            .request("thread/start", Some(Value::Object(rest)))
            .await
            .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?
    } else {
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state
            .worker_rpc
            .thread_start(&rpc_url, &pg_team_id, &uid.0, Value::Object(rest))
            .await?
    };

    // PG 共享操作(入口节点写即可,双写幂等):threads 元数据 + resume cache。
    // 跨进程 + 重启都生效,避免前端 create→resume 链路上 race codex 落盘前调 thread/resume 返回 -32600。
    // I7:codex thread 已成功启动,这些 PG 操作均 best-effort 非阻塞(double_write_thread_meta /
    //     put_cached_resume 内部失败仅 warn)—— 不 `?` 中断,避免客户端重试生成新 thread_id 残留孤儿。
    double_write_thread_meta(db, &thread_id, &pg_team_id, &uid.0, workspace_type).await;
    crate::services::multitenant::resume_cache::put_cached_resume(db, &thread_id, &resp).await;

    // C1:仅本地分支登记 session_replicas/sticky/active_rollout/复制。
    //     转发场景(target != self)由 target 侧 internal_rpc thread_start 自登记:
    //     入口节点若在此调 get_or_assign 会用 local_node_id()=入口 把 primary_node 登记成入口,
    //     但 codex 进程实际跑在 target → primary_node 错乱,破坏 failover + rollout 复制。
    //     sticky.bind 也委托 target 侧(共享 Redis)。
    if target == state.node_id {
        // get_or_assign 在本地分支调 → local_node_id()=本节点=target,primary 登记正确。
        // I7:best-effort(codex thread 已起,`?` 中断会留孤儿)。
        if let Err(e) = crate::services::multitenant::replication::get_or_assign(
            &state.db,
            &thread_id,
            state.cluster.as_ref(),
        )
        .await
        {
            tracing::error!(error = %e, thread_id = %thread_id, "get_or_assign failed (best-effort, codex thread already started)");
        }
        if let Err(e) = state.sticky.bind(&thread_id, &target, 3600).await {
            tracing::error!(error = %e, thread_id = %thread_id, "sticky.bind failed (best-effort)");
        }

        // 主侧:关联 rollout 文件 + 复制增量到副本。
        // C2:用 codex 响应 tid 查找 rollout 文件名(兼容 codex 忽略外部 threadId 自生成 tid 的场景);
        //     active_rollout key 仍用系统 thread_id(replicate 按 thread_id 取路径)。
        let codex_tid = extract_codex_tid(&resp);
        if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
            &state.codex_home,
            codex_tid.as_deref().unwrap_or(&thread_id),
        )
        .await
        {
            state.active_rollout.lock().await.insert(thread_id.clone(), p);
        }
        let _ = crate::services::multitenant::replication::replicate_thread_rollout(
            db,
            &thread_id,
            &state.codex_home,
            state.cluster.as_ref(),
            state.mt_redis.as_ref(),
            &state.worker_rpc,
            &state.active_rollout,
            &state.local_offsets,
        )
        .await;
    }

    // 包装 codex 响应为一致格式:前端期望 {thread, id, cwd} 而非扁平 codex 响应。
    let cwd = resp
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| resp.get("thread").and_then(|t| t.get("cwd")).and_then(Value::as_str))
        .unwrap_or("");
    let wrapped = serde_json::json!({
        "thread": resp,
        "id": thread_id,
        "cwd": cwd,
    });
    Ok(Json(wrapped))
}

/// 列出 team 会话元数据(从 PG,team 内共享,按活跃时间倒序)。
pub async fn mt_list_threads(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    Query(q): Query<TeamIdQuery>,
) -> Result<Json<Vec<crate::db::entities::thread::Model>>, AppError> {
    let db = require_db(&state);
    permissions::require_permission(db, &q.team_id, &uid.0, TeamPermission::ThreadRead).await?;
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

/// GET /api/mt/me:当前用户身份 + 平台管理员标记 + 各 team 角色/权限点。
/// 供前端权限驱动 UI 显隐(导航/按钮/菜单)。
pub async fn mt_me(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = require_db(&state);
    let user = UserEntity::find_by_id(uid.0.clone())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query user: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "user not found".into(),
                None,
            )
        })?;
    let is_admin = permissions::is_platform_admin(db, &uid.0).await?;
    // 用户的所有 team 成员关系
    let memberships = TeamMemberEntity::find()
        .filter(TeamMemberColumn::UserId.eq(uid.0.clone()))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("query memberships: {e}")))?;
    let mut teams = Vec::with_capacity(memberships.len());
    for m in memberships {
        let perms = RolePermissionEntity::find()
            .filter(RolePermissionColumn::Role.eq(m.role.clone()))
            .all(db)
            .await
            .map_err(|e| AppError::internal(format!("query role perms: {e}")))?
            .into_iter()
            .map(|r| r.permission)
            .collect::<Vec<_>>();
        teams.push(serde_json::json!({
            "team_id": m.team_id,
            "role": m.role,
            "permissions": perms,
        }));
    }
    Ok(Json(serde_json::json!({
        "user": { "id": user.id, "email": user.email, "display_name": user.display_name },
        "is_platform_admin": is_admin,
        "teams": teams,
    })))
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
    // per-thread 路由后,is_personal 不再参与选节点(resolve_worker 仅看 thread_id);
    // 保留字段读取仅为后续可能的权限分支扩展。
    let _is_personal = thread.workspace_type == "personal";

    // 2. 调 codex thread/delete(若 codex 支持);失败不阻塞,继续清 PG/文件。
    if let Ok(target) = resolve_worker(&state, Some(&thread_id)).await {
        if target == state.node_id {
            if let Err(e) = state
                .codex
                .request("thread/delete", Some(serde_json::json!({ "threadId": thread_id })))
                .await
            {
                tracing::warn!(error = %e, thread_id = %thread_id, "codex thread/delete failed (non-fatal, cleanup PG+file anyway)");
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
    let (team_id, _workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    // 配额校验(M6):超额返回 429。
    crate::services::multitenant::quota::check_turn_quota(db, &team_id).await?;
    metrics::counter!("mt_turns_total").increment(1);
    let target = resolve_worker(&state, Some(&thread_id)).await?;
    let mut params = body.0;
    if let Value::Object(ref mut map) = params {
        map.entry("threadId").or_insert(Value::String(thread_id.clone()));
    }
    let resp = if target == state.node_id {
        state
            .codex
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
    // 主侧:把 thread 关联到其 rollout 文件,供 replicate_thread_rollout 精确读取。
    if target == state.node_id {
        // C2:用 codex 响应 tid 查找 rollout(codex 尊重 threadId 时 == 系统 thread_id);
        //     active_rollout key 用系统 thread_id。
        let codex_tid = extract_codex_tid(&resp);
        if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
            &state.codex_home, codex_tid.as_deref().unwrap_or(&thread_id),
        )
        .await
        {
            state.active_rollout.lock().await.insert(thread_id.clone(), p);
        }
    }
    // 主侧:turn 完成后复制该 thread 的 rollout 增量到副本(per-thread 复制)。
    if target == state.node_id {
        let _ = crate::services::multitenant::replication::replicate_thread_rollout(
            db,
            &thread_id,
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
    permissions::require_permission(db, &team_id, &uid.0, TeamPermission::AuditRead).await?;
    Ok(Json(audit::list(db, &team_id, 200).await?))
}

// ── 多副本路由辅助 ──────────────────────────────────────────────────────

/// 选目标节点(per-thread 调度):
/// 1. sticky 命中且 alive → 直接返回;
/// 2. I1:sticky 未命中但 thread 已登记 → 回落 session_replicas.primary_node(alive 则返回并重绑 sticky);
/// 3. 否则按最少负载选 alive 节点(本节点并列最少时优先,避免无谓 RPC 转发)。
async fn resolve_worker(state: &AppState, thread_id: Option<&str>) -> Result<String, AppError> {
    // 1. sticky 命中且 alive → 直接返回。
    if let Some(tid) = thread_id {
        if let Ok(Some(stuck)) = state.sticky.lookup(tid).await {
            if crate::services::multitenant::cluster::is_alive(state.cluster.as_ref(), &stuck).await {
                return Ok(stuck);
            }
            let _ = state.sticky.clear(tid).await;
        }
    }

    // 2. I1:sticky 未命中 → 回落 session_replicas.primary_node(会话本地性)。
    //    前提:C1 已修(primary_node 登记正确);primary 仍 alive 则回落并重绑 sticky。
    if let Some(tid) = thread_id {
        if let Ok(Some(row)) = crate::services::multitenant::replication::get(&state.db, tid).await {
            if crate::services::multitenant::cluster::is_alive(state.cluster.as_ref(), &row.primary_node).await {
                let _ = state.sticky.bind(tid, &row.primary_node, 3600).await;
                return Ok(row.primary_node);
            }
        }
    }

    // 3. 统计各 alive 节点的 primary thread 数(负载指标)。
    //    I6:改 SeaORM 聚合查询(group_by primary_node + count),避免每请求全表扫
    //    session_replicas(设计预期百万级 thread)。仅统计 alive 节点行。
    use crate::db::entities::session_replica::{Column as SRColumn, Entity as SREntity};
    use sea_orm::sea_query::Expr;
    use sea_orm::{EntityTrait, QuerySelect};
    let alive = state.cluster.alive_nodes().await;
    if alive.is_empty() {
        return Ok(state.cluster.local_node_id().to_string());
    }
    let mut load: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for n in &alive {
        load.insert(n.clone(), 0);
    }
    let rows: Vec<(String, i64)> = SREntity::find()
        .select_only()
        .column(SRColumn::PrimaryNode)
        .column_as(Expr::col(SRColumn::PrimaryNode).count(), "cnt")
        .group_by(SRColumn::PrimaryNode)
        .into_tuple::<(String, i64)>()
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("load scan: {e}")))?;
    for (node, cnt) in &rows {
        if let Some(c) = load.get_mut(node) {
            *c = *cnt;
        }
    }

    // 4. 选最少负载;并列时优先本节点(避免无谓 RPC 转发)。
    let me = state.cluster.local_node_id();
    let mut iter = alive.iter();
    let first = iter.next().expect("alive 非空(上面已 early-return)");
    let mut best_node = first.clone();
    let mut best_load = load[first];
    for n in iter {
        let l = load[n];
        if l < best_load || (l == best_load && n.as_str() == me) {
            best_node = n.clone();
            best_load = l;
        }
    }
    Ok(best_node)
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
    let (team_id, _workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    let target = resolve_worker(&state, Some(&thread_id)).await?;
    let id_val = parse_request_id(&body.request_id);
    let ok = if target == state.node_id {
        if let Some(client) = state.codex.client().await {
            if body.approved {
                client
                    .respond_to_server_request(
                        id_val,
                        body.result.unwrap_or(Value::Object(Default::default())),
                    )
                    .is_ok()
            } else {
                client
                    .respond_to_server_request_with_error(id_val, -32000, "denied by user")
                    .is_ok()
            }
        } else {
            false
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

/// 从 codex 响应提取其内部 tid(codex 实际用于 rollout 文件命名的会话 id)。
/// 取值顺序对齐 internal_rpc 既有取法:resp.thread.id → resp.threadId → resp.id。
/// 用途(C2):find_rollout_for_thread 按 codex_tid 匹配 rollout 文件名边界。
/// - codex 尊重外部 threadId → codex_tid == 系统 thread_id(行为不变);
/// - codex 忽略 threadId 自生成 → 返回 codex_tid,据此找到正确 rollout 文件。
fn extract_codex_tid(resp: &Value) -> Option<String> {
    resp.get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| resp.get("threadId").and_then(Value::as_str).map(String::from))
        .or_else(|| resp.get("id").and_then(Value::as_str).map(String::from))
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
    let (team_id, _workspace_type) = require_thread_team(db, &thread_id, &uid.0).await?;
    let target = resolve_worker(&state, Some(&thread_id)).await?;
    // thread/resume 读 PG cache 作兜底(codex -32600 race 时返回),不短路 ——
    // 仍调 codex 取最新 turns + 确保 codex 内存持有 thread(进程重启/evict 后需重新加载)。
    // 若短路返回旧 cache,进入会话后发消息再刷新会看不到新对话(cache 是旧快照)。
    let cache_fallback: Option<Value> = if body.method == "thread/resume" {
        crate::services::multitenant::resume_cache::get_cached_resume(db, &thread_id).await
    } else {
        None
    };
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
        let mut attempt = 0u32;
        let value = loop {
            let r = state.codex.request(&body.method, Some(params.clone())).await;
            match r {
                Ok(v) => break v,
                Err(crate::codex::jsonrpc::RpcError::ServerError { code: -32600, .. })
                    if needs_retry && attempt < 3 =>
                {
                    attempt += 1;
                    tracing::debug!(method = %body.method, thread_id = %thread_id, attempt, "codex -32600, retrying after backoff");
                    tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
                    continue;
                }
                // retry 耗尽(-32600):用 cache 兜底(若有),否则报错。
                Err(crate::codex::jsonrpc::RpcError::ServerError { code: -32600, .. })
                    if needs_retry =>
                {
                    if let Some(fb) = cache_fallback.clone() {
                        tracing::warn!(thread_id = %thread_id, method = %body.method, "codex -32600 after retries, serving cached fallback");
                        break fb;
                    }
                    return Err(AppError::internal(format!("codex {} -32600 after retries (no cache fallback)", body.method)));
                }
                Err(e) => {
                    return Err(AppError::internal(format!("codex {}: {e}", body.method)));
                }
            }
        };
        if attempt > 0 {
            tracing::info!(method = %body.method, thread_id = %thread_id, attempt, "codex recovered after retry");
        }
        // resume 成功写 PG 缓存(下次 codex -32600 race 时作兜底)
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

//! 策略管理 REST API：全局策略（平台管理员）+ 团队策略（team owner/admin）。

use crate::db::entities::tool_policy::{
    ActiveModel as ToolPolicyActiveModel, Column as ToolPolicyColumn,
    Entity as ToolPolicyEntity, Model as ToolPolicyModel,
};
use crate::error::{AppError, ErrorCode};
use crate::multitenant::middleware::UserId;
use crate::services::multitenant::{new_id, now_ms, permissions};
use crate::services::policy_engine::dto::{
    CreatePolicyRuleDto, PolicyRuleDto, PolicyRuleListResponse, PolicyRuleResponse,
    UpdatePolicyRuleDto,
};
use crate::state::AppState;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set};

const ROLE_OWNER: &str = "owner";
const ROLE_ADMIN: &str = "admin";

const ALLOWED_SCOPES: &[&str] = &["global", "team"];
const ALLOWED_RULE_TYPES: &[&str] = &["command", "tool", "skill", "plugin", "mcp"];
const ALLOWED_MATCH_MODES: &[&str] = &["blacklist", "whitelist", "regex", "exact"];
const ALLOWED_ACTIONS: &[&str] = &["allow", "deny"];
const ALLOWED_ROLES: &[&str] = &["owner", "admin", "member"];

fn require_db(state: &AppState) -> &DatabaseConnection {
    &state.db
}

fn validate_enum(field: &str, value: &str, allowed: &[&str]) -> Result<(), AppError> {
    if allowed.iter().any(|v| *v == value) {
        Ok(())
    } else {
        Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            format!("{field} 必须是 {:?} 之一", allowed),
            None,
        ))
    }
}

fn validate_pattern(pattern: &str) -> Result<(), AppError> {
    if pattern.is_empty() || pattern.len() > 4096 {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "pattern 长度必须在 1..=4096 之间".into(),
            None,
        ));
    }
    Ok(())
}

fn validate_role(role: Option<&String>) -> Result<(), AppError> {
    if let Some(r) = role {
        if !ALLOWED_ROLES.iter().any(|v| *v == r.as_str()) {
            return Err(AppError::business(
                ErrorCode::ValidationFieldInvalid,
                StatusCode::BAD_REQUEST,
                format!("role 必须是 {:?} 之一", ALLOWED_ROLES),
                None,
            ));
        }
    }
    Ok(())
}

fn model_to_dto(m: ToolPolicyModel) -> PolicyRuleDto {
    PolicyRuleDto {
        id: m.id,
        scope: m.scope,
        team_id: m.team_id,
        role: m.role,
        rule_type: m.rule_type,
        match_mode: m.match_mode,
        pattern: m.pattern,
        action: m.action,
        priority: m.priority,
        enabled: m.enabled,
        description: m.description,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

/// 写入后使缓存失效，并通过 EventBus 广播 `policies:changed` 让其他节点同步刷新。
async fn invalidate_and_broadcast(state: &AppState) {
    state.policy_store.invalidate().await;
    if let Some(bus) = state.mt_event_bus.as_ref() {
        if let Err(e) = bus
            .publish("policies:changed", &format!("{}", now_ms()))
            .await
        {
            tracing::warn!(error = %e, "publish policies:changed failed");
        }
    }
}

// ── 全局策略（平台管理员）──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/mt/policies/global",
    tag = "policies",
    responses(
        (status = 200, description = "全局策略列表", body = PolicyRuleListResponse),
    )
)]
pub async fn list_global_policies(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> Result<Json<PolicyRuleListResponse>, AppError> {
    permissions::require_platform_admin(&state.db, &uid).await?;
    let rows = ToolPolicyEntity::find()
        .filter(ToolPolicyColumn::Scope.eq("global".to_string()))
        .all(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(Json(PolicyRuleListResponse {
        items: rows.into_iter().map(model_to_dto).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/mt/policies/global",
    tag = "policies",
    responses(
        (status = 200, description = "新建全局策略", body = PolicyRuleResponse),
    )
)]
pub async fn create_global_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    crate::error::Json(body): crate::error::Json<CreatePolicyRuleDto>,
) -> Result<Json<PolicyRuleResponse>, AppError> {
    permissions::require_platform_admin(&state.db, &uid).await?;
    validate_enum("scope", &body.scope, ALLOWED_SCOPES)?;
    if body.scope != "global" {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "全局策略接口仅接受 scope=global".into(),
            None,
        ));
    }
    validate_enum("rule_type", &body.rule_type, ALLOWED_RULE_TYPES)?;
    validate_enum("match_mode", &body.match_mode, ALLOWED_MATCH_MODES)?;
    validate_enum("action", &body.action, ALLOWED_ACTIONS)?;
    validate_role(body.role.as_ref())?;
    validate_pattern(&body.pattern)?;

    let id = new_id();
    let now = now_ms();
    let am = ToolPolicyActiveModel {
        id: Set(id.clone()),
        scope: Set("global".to_string()),
        team_id: Set(None),
        role: Set(body.role.clone()),
        rule_type: Set(body.rule_type.clone()),
        match_mode: Set(body.match_mode.clone()),
        pattern: Set(body.pattern.clone()),
        action: Set(body.action.clone()),
        priority: Set(body.priority.unwrap_or(0)),
        enabled: Set(body.enabled.unwrap_or(true)),
        description: Set(body.description.clone()),
        created_at: Set(now),
        updated_at: Set(now),
    };
    let model = am
        .insert(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    invalidate_and_broadcast(&state).await;
    Ok(Json(PolicyRuleResponse {
        rule: model_to_dto(model),
    }))
}

#[utoipa::path(
    patch,
    path = "/api/mt/policies/global/{id}",
    tag = "policies",
    params(("id" = String, Path,)),
    responses(
        (status = 200, description = "更新全局策略", body = PolicyRuleResponse),
    )
)]
pub async fn update_global_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(id): Path<String>,
    crate::error::Json(body): crate::error::Json<UpdatePolicyRuleDto>,
) -> Result<Json<PolicyRuleResponse>, AppError> {
    permissions::require_platform_admin(&state.db, &uid).await?;
    if let Some(rt) = &body.rule_type {
        validate_enum("rule_type", rt, ALLOWED_RULE_TYPES)?;
    }
    if let Some(mm) = &body.match_mode {
        validate_enum("match_mode", mm, ALLOWED_MATCH_MODES)?;
    }
    if let Some(ac) = &body.action {
        validate_enum("action", ac, ALLOWED_ACTIONS)?;
    }
    validate_role(body.role.as_ref())?;
    if let Some(p) = &body.pattern {
        validate_pattern(p)?;
    }

    let existing = ToolPolicyEntity::find_by_id(id.clone())
        .one(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "策略不存在".into(),
                None,
            )
        })?;
    if existing.scope != "global" {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "全局策略接口仅能修改 scope=global 的规则".into(),
            None,
        ));
    }

    let mut am: ToolPolicyActiveModel = existing.into();
    if let Some(v) = body.role {
        am.role = Set(Some(v));
    }
    if let Some(v) = body.rule_type {
        am.rule_type = Set(v);
    }
    if let Some(v) = body.match_mode {
        am.match_mode = Set(v);
    }
    if let Some(v) = body.pattern {
        am.pattern = Set(v);
    }
    if let Some(v) = body.action {
        am.action = Set(v);
    }
    if let Some(v) = body.priority {
        am.priority = Set(v);
    }
    if let Some(v) = body.enabled {
        am.enabled = Set(v);
    }
    if let Some(v) = body.description {
        am.description = Set(Some(v));
    }
    am.updated_at = Set(now_ms());
    let model = am
        .update(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    invalidate_and_broadcast(&state).await;
    Ok(Json(PolicyRuleResponse {
        rule: model_to_dto(model),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/mt/policies/global/{id}",
    tag = "policies",
    params(("id" = String, Path,)),
    responses(
        (status = 204, description = "删除全局策略"),
    )
)]
pub async fn delete_global_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    permissions::require_platform_admin(&state.db, &uid).await?;
    let existing = ToolPolicyEntity::find_by_id(id.clone())
        .one(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "策略不存在".into(),
                None,
            )
        })?;
    if existing.scope != "global" {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "全局策略接口仅能删除 scope=global 的规则".into(),
            None,
        ));
    }
    let res = ToolPolicyEntity::delete_by_id(id)
        .exec(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if res.rows_affected == 0 {
        return Err(AppError::internal("删除失败".into()));
    }
    invalidate_and_broadcast(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── 团队策略（team owner/admin）────────────────────────────────────────

async fn require_team_admin(state: &AppState, team_id: &str, user_id: &str) -> Result<String, AppError> {
    let role = crate::services::multitenant::teams::require_member(&state.db, team_id, user_id).await?;
    if role != ROLE_OWNER && role != ROLE_ADMIN {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "team owner 或 admin 才能管理策略".into(),
            None,
        ));
    }
    Ok(role)
}

#[utoipa::path(
    get,
    path = "/api/mt/teams/{teamId}/policies",
    tag = "policies",
    params(("teamId" = String, Path,)),
    responses(
        (status = 200, description = "团队策略列表", body = PolicyRuleListResponse),
    )
)]
pub async fn list_team_policies(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(team_id): Path<String>,
) -> Result<Json<PolicyRuleListResponse>, AppError> {
    require_team_admin(&state, &team_id, &uid).await?;
    let rows = ToolPolicyEntity::find()
        .filter(ToolPolicyColumn::Scope.eq("team".to_string()))
        .filter(ToolPolicyColumn::TeamId.eq(team_id.clone()))
        .all(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(Json(PolicyRuleListResponse {
        items: rows.into_iter().map(model_to_dto).collect(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/mt/teams/{teamId}/policies",
    tag = "policies",
    params(("teamId" = String, Path,)),
    responses(
        (status = 200, description = "新建团队策略", body = PolicyRuleResponse),
    )
)]
pub async fn create_team_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(team_id): Path<String>,
    crate::error::Json(body): crate::error::Json<CreatePolicyRuleDto>,
) -> Result<Json<PolicyRuleResponse>, AppError> {
    require_team_admin(&state, &team_id, &uid).await?;
    if body.scope != "team" {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "团队策略接口仅接受 scope=team".into(),
            None,
        ));
    }
    if body.team_id.as_deref() != Some(team_id.as_str()) {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "team_id 必须与路径一致".into(),
            None,
        ));
    }
    validate_enum("rule_type", &body.rule_type, ALLOWED_RULE_TYPES)?;
    validate_enum("match_mode", &body.match_mode, ALLOWED_MATCH_MODES)?;
    validate_enum("action", &body.action, ALLOWED_ACTIONS)?;
    validate_role(body.role.as_ref())?;
    validate_pattern(&body.pattern)?;

    let id = new_id();
    let now = now_ms();
    let am = ToolPolicyActiveModel {
        id: Set(id.clone()),
        scope: Set("team".to_string()),
        team_id: Set(Some(team_id.clone())),
        role: Set(body.role.clone()),
        rule_type: Set(body.rule_type.clone()),
        match_mode: Set(body.match_mode.clone()),
        pattern: Set(body.pattern.clone()),
        action: Set(body.action.clone()),
        priority: Set(body.priority.unwrap_or(0)),
        enabled: Set(body.enabled.unwrap_or(true)),
        description: Set(body.description.clone()),
        created_at: Set(now),
        updated_at: Set(now),
    };
    let model = am
        .insert(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    invalidate_and_broadcast(&state).await;
    Ok(Json(PolicyRuleResponse {
        rule: model_to_dto(model),
    }))
}

#[utoipa::path(
    patch,
    path = "/api/mt/teams/{teamId}/policies/{id}",
    tag = "policies",
    params(("teamId" = String, Path,), ("id" = String, Path,)),
    responses(
        (status = 200, description = "更新团队策略", body = PolicyRuleResponse),
    )
)]
pub async fn update_team_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path((team_id, id)): Path<(String, String)>,
    crate::error::Json(body): crate::error::Json<UpdatePolicyRuleDto>,
) -> Result<Json<PolicyRuleResponse>, AppError> {
    require_team_admin(&state, &team_id, &uid).await?;
    if let Some(rt) = &body.rule_type {
        validate_enum("rule_type", rt, ALLOWED_RULE_TYPES)?;
    }
    if let Some(mm) = &body.match_mode {
        validate_enum("match_mode", mm, ALLOWED_MATCH_MODES)?;
    }
    if let Some(ac) = &body.action {
        validate_enum("action", ac, ALLOWED_ACTIONS)?;
    }
    validate_role(body.role.as_ref())?;
    if let Some(p) = &body.pattern {
        validate_pattern(p)?;
    }

    let existing = ToolPolicyEntity::find_by_id(id.clone())
        .one(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "策略不存在".into(),
                None,
            )
        })?;
    if existing.scope != "team" || existing.team_id.as_deref() != Some(team_id.as_str()) {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "团队策略接口仅能修改本团队策略".into(),
            None,
        ));
    }

    let mut am: ToolPolicyActiveModel = existing.into();
    if let Some(v) = body.role {
        am.role = Set(Some(v));
    }
    if let Some(v) = body.rule_type {
        am.rule_type = Set(v);
    }
    if let Some(v) = body.match_mode {
        am.match_mode = Set(v);
    }
    if let Some(v) = body.pattern {
        am.pattern = Set(v);
    }
    if let Some(v) = body.action {
        am.action = Set(v);
    }
    if let Some(v) = body.priority {
        am.priority = Set(v);
    }
    if let Some(v) = body.enabled {
        am.enabled = Set(v);
    }
    if let Some(v) = body.description {
        am.description = Set(Some(v));
    }
    am.updated_at = Set(now_ms());
    let model = am
        .update(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    invalidate_and_broadcast(&state).await;
    Ok(Json(PolicyRuleResponse {
        rule: model_to_dto(model),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/mt/teams/{teamId}/policies/{id}",
    tag = "policies",
    params(("teamId" = String, Path,), ("id" = String, Path,)),
    responses(
        (status = 204, description = "删除团队策略"),
    )
)]
pub async fn delete_team_policy(
    State(state): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path((team_id, id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    require_team_admin(&state, &team_id, &uid).await?;
    let existing = ToolPolicyEntity::find_by_id(id.clone())
        .one(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "策略不存在".into(),
                None,
            )
        })?;
    if existing.scope != "team" || existing.team_id.as_deref() != Some(team_id.as_str()) {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "团队策略接口仅能删除本团队策略".into(),
            None,
        ));
    }
    let res = ToolPolicyEntity::delete_by_id(id)
        .exec(require_db(&state))
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if res.rows_affected == 0 {
        return Err(AppError::internal("删除失败".into()));
    }
    invalidate_and_broadcast(&state).await;
    Ok(StatusCode::NO_CONTENT)
}
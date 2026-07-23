//! 团队级权限点 + 角色校验函数。
//!
//! 权限点用 enum(编译期类型安全),角色→权限映射存 role_permissions 表(seed)。
//! require_permission 查 team_members.role → role_permissions 判定。

use crate::error::{AppError, ErrorCode};
use crate::db::entities::role_permission::{
    Column as RolePermissionColumn, Entity as RolePermissionEntity,
};
use crate::db::entities::user::{Column as UserColumn, Entity as UserEntity};
use axum::http::StatusCode;
use sea_orm::entity::prelude::*;
use sea_orm::DatabaseConnection;

pub const ROLE_ADMIN: &str = "admin";

/// 团队级权限点。`code()` 对应 role_permissions.permission 列的 `team:{resource}:{action}`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TeamPermission {
    MemberList,
    MemberInvite,
    MemberRemove,
    MemberRoleWrite,
    ApiKeyRead,
    ApiKeyWrite,
    AuditRead,
    ThreadCreate,
    ThreadRead,
    TurnWrite,
    OwnerTransfer,
    TeamDissolve,
}

impl TeamPermission {
    pub fn code(&self) -> &'static str {
        match self {
            TeamPermission::MemberList => "team:member:list",
            TeamPermission::MemberInvite => "team:member:invite",
            TeamPermission::MemberRemove => "team:member:remove",
            TeamPermission::MemberRoleWrite => "team:member:role:write",
            TeamPermission::ApiKeyRead => "team:api_key:read",
            TeamPermission::ApiKeyWrite => "team:api_key:write",
            TeamPermission::AuditRead => "team:audit:read",
            TeamPermission::ThreadCreate => "team:thread:create",
            TeamPermission::ThreadRead => "team:thread:read",
            TeamPermission::TurnWrite => "team:turn:write",
            TeamPermission::OwnerTransfer => "team:owner:transfer",
            TeamPermission::TeamDissolve => "team:dissolve",
        }
    }
}

// ── 团队级权限校验 ─────────────────────────────────────────────────────────

/// 要求 user 在该 team 持有 perm;否则 403。返回其 role。
pub async fn require_permission(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
    perm: TeamPermission,
) -> Result<String, AppError> {
    let role = crate::services::multitenant::teams::member_role(db, team_id, user_id)
        .await?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpForbidden,
                StatusCode::FORBIDDEN,
                "not a team member".into(),
                None,
            )
        })?;
    let granted = RolePermissionEntity::find()
        .filter(RolePermissionColumn::Role.eq(role.clone()))
        .filter(RolePermissionColumn::Permission.eq(perm.code().to_string()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query role permission: {e}")))?
        .is_some();
    if !granted {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "permission denied".into(),
            None,
        ));
    }
    Ok(role)
}

// ── 平台级 ─────────────────────────────────────────────────────────────────

/// user 是否为平台超级管理员。
pub async fn is_platform_admin(db: &DatabaseConnection, user_id: &str) -> Result<bool, AppError> {
    let u = UserEntity::find_by_id(user_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query user: {e}")))?;
    Ok(u.map(|m| m.is_platform_admin).unwrap_or(false))
}

/// 要求 user 是平台超级管理员;否则 403。
pub async fn require_platform_admin(
    db: &DatabaseConnection,
    user_id: &str,
) -> Result<(), AppError> {
    if !is_platform_admin(db, user_id).await? {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "platform admin only".into(),
            None,
        ));
    }
    Ok(())
}

/// 启动期 bootstrap:把 admin_emails 里已存在的用户置 is_platform_admin=true。
/// 不撤销不在列表里的(避免误降权)。供 main.rs 在 migration 后调用。
pub async fn bootstrap_platform_admins(
    db: &DatabaseConnection,
    admin_emails: &[String],
) -> Result<u64, AppError> {
    if admin_emails.is_empty() {
        return Ok(0);
    }
    let emails: Vec<String> = admin_emails
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if emails.is_empty() {
        return Ok(0);
    }
    let res = UserEntity::update_many()
        .col_expr(UserColumn::IsPlatformAdmin, Expr::value(true))
        .filter(UserColumn::Email.is_in(emails))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("bootstrap platform admins: {e}")))?;
    Ok(res.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_code_format_is_team_namespace() {
        for perm in [
            TeamPermission::MemberList, TeamPermission::MemberInvite,
            TeamPermission::MemberRemove, TeamPermission::MemberRoleWrite,
            TeamPermission::ApiKeyRead, TeamPermission::ApiKeyWrite,
            TeamPermission::AuditRead, TeamPermission::ThreadCreate,
            TeamPermission::ThreadRead, TeamPermission::TurnWrite,
            TeamPermission::OwnerTransfer, TeamPermission::TeamDissolve,
        ] {
            assert!(perm.code().starts_with("team:"), "{} missing team: prefix", perm.code());
            // 至少 1 个冒号(team:{something});dissolve 为 1 层,member:role:write 为 3 层。
            // 真正的强约束是 starts_with("team:") + 非空尾段;count 仅用于表达意图。
            assert!(
                perm.code().matches(':').count() >= 1 && perm.code().len() > 5,
                "{} must be team:<something>",
                perm.code()
            );
        }
    }

    #[test]
    fn specific_codes_match_spec() {
        assert_eq!(TeamPermission::MemberRemove.code(), "team:member:remove");
        assert_eq!(TeamPermission::ApiKeyWrite.code(), "team:api_key:write");
        assert_eq!(TeamPermission::OwnerTransfer.code(), "team:owner:transfer");
        assert_eq!(TeamPermission::TeamDissolve.code(), "team:dissolve");
    }
}

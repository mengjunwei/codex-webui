//! team 管理:创建 / 列表 / 成员 / 邀请码 / 加入 / 踢除,以及成员校验 helper。
//!
//! 所有函数接 `&DatabaseConnection`(handler 在 M1.5 组装)。权限铁律:`require_member` /
//! `require_owner` 用于在 handler 里校验"当前 user 是否属于该 team / 是否 owner"。
//!
//! 注:本模块 entity 暂未声明 Relation 元数据,涉及多表 JOIN 走自定义 SELECT + 手动
//! 投影,确保 PG/MySQL 行为一致(避免 SeaORM builder 隐式补 ON 子句)。

pub use crate::db::entities::invitation::Model as Invitation;
pub use crate::db::entities::team::Model as Team;

use crate::error::{AppError, ErrorCode};
use crate::db::entities::invitation::{
    ActiveModel as InvitationActiveModel, Column as InvitationColumn, Entity as InvitationEntity,
};
use crate::db::entities::team::{
    ActiveModel as TeamActiveModel, Column as TeamColumn, Entity as TeamEntity,
};
use crate::db::entities::team_member::{
    ActiveModel as TeamMemberActiveModel, Column as TeamMemberColumn, Entity as TeamMemberEntity,
};
use crate::db::entities::user::{
    Column as UserColumn, Entity as UserEntity, Model as UserModel,
};
use crate::services::multitenant::permissions::ROLE_ADMIN;
use crate::services::multitenant::{new_id, now_ms};
use axum::http::StatusCode;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveModelTrait, DatabaseConnection, QueryOrder, QuerySelect, Set, TransactionTrait};
use rand::Rng;
use serde::Serialize;

pub const ROLE_OWNER: &str = "owner";
pub const ROLE_MEMBER: &str = "member";

/// 成员视图(联表 user + team_member),用于 list_members 返回。
#[derive(Debug, Serialize)]
pub struct MemberView {
    pub user_id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub role: String,
    pub joined_at: i64,
}

// ── 成员校验(handler 鉴权用)──────────────────────────────────────────────

/// 返回 user 在 team 中的 role;非成员返回 None。
pub async fn member_role(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
) -> Result<Option<String>, AppError> {
    let model = TeamMemberEntity::find()
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(user_id.to_string()))
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query member role: {e}")))?;
    Ok(model.map(|m| m.role))
}

/// 要求 user 是 team 成员,返回其 role;否则 403。
pub async fn require_member(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
) -> Result<String, AppError> {
    match member_role(db, team_id, user_id).await? {
        Some(r) => Ok(r),
        None => Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "not a team member".into(),
            None,
        )),
    }
}

/// 要求 user 是 team 的 owner;否则 403。
pub async fn require_owner(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
) -> Result<(), AppError> {
    let role = require_member(db, team_id, user_id).await?;
    if role != ROLE_OWNER {
        return Err(AppError::business(
            ErrorCode::HttpForbidden,
            StatusCode::FORBIDDEN,
            "owner only".into(),
            None,
        ));
    }
    Ok(())
}

// ── team CRUD ────────────────────────────────────────────────────────────

/// 创建 team:owner 同时作为首个成员(role=owner)。事务保证 team 与成员记录同生。
pub async fn create_team(
    db: &DatabaseConnection,
    owner_id: &str,
    name: &str,
) -> Result<Team, AppError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::business(
            ErrorCode::ValidationFieldInvalid,
            StatusCode::BAD_REQUEST,
            "invalid team name".into(),
            None,
        ));
    }
    let now = now_ms();
    let team_id = new_id();

    // 事务:team 插入 + owner 成员记录同生。
    db.transaction(|txn| {
        let team_id = team_id.clone();
        let name = name.to_string();
        let owner_id = owner_id.to_string();
        Box::pin(async move {
            let team_am = TeamActiveModel {
                id: Set(team_id.clone()),
                name: Set(name),
                owner_id: Set(owner_id.clone()),
                created_at: Set(now),
                updated_at: Set(now),
            };
            team_am.insert(txn).await
                .map_err(|e| AppError::internal(format!("insert team: {e}")))?;
            let member_am = TeamMemberActiveModel {
                team_id: Set(team_id),
                user_id: Set(owner_id),
                role: Set(ROLE_OWNER.to_string()),
                joined_at: Set(now),
            };
            member_am.insert(txn).await
                .map_err(|e| AppError::internal(format!("insert owner membership: {e}")))?;
            Ok(())
        })
    })
    .await
    .map_err(|e: sea_orm::TransactionError<AppError>| AppError::internal(format!("tx: {e}")))?;

    // 等价原 RETURNING 字段:再用主键回查一遍。
    load_team(db, &team_id).await
}

// ── 列表 ────────────────────────────────────────────────────────────────

/// 列出 user 所属的全部 team(一人多 team),按创建时间倒序。
pub async fn list_my_teams(
    db: &DatabaseConnection,
    user_id: &str,
) -> Result<Vec<Team>, AppError> {
    let member_rows = TeamMemberEntity::find()
        .filter(TeamMemberColumn::UserId.eq(user_id.to_string()))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list team members: {e}")))?;
    let team_ids: Vec<String> = member_rows.into_iter().map(|m| m.team_id).collect();
    if team_ids.is_empty() {
        return Ok(vec![]);
    }
    let teams = TeamEntity::find()
        .filter(TeamColumn::Id.is_in(team_ids))
        .order_by_desc(TeamColumn::CreatedAt)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list teams: {e}")))?;
    Ok(teams)
}

/// 列出 team 全部成员(两次查询 + 内存合并,避免跨方言 JOIN)。
pub async fn list_members(
    db: &DatabaseConnection,
    team_id: &str,
) -> Result<Vec<MemberView>, AppError> {
    let member_rows = TeamMemberEntity::find()
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list members: {e}")))?;
    let user_ids: Vec<String> = member_rows.iter().map(|m| m.user_id.clone()).collect();
    let users: std::collections::HashMap<String, UserModel> = if user_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        UserEntity::find()
            .filter(UserColumn::Id.is_in(user_ids))
            .all(db)
            .await
            .map_err(|e| AppError::internal(format!("list users: {e}")))?
            .into_iter()
            .map(|u| (u.id.clone(), u))
            .collect()
    };
    let result: Vec<MemberView> = member_rows
        .into_iter()
        .filter_map(|m| {
            let u = users.get(&m.user_id)?;
            Some(MemberView {
                user_id: u.id.clone(),
                email: u.email.clone(),
                display_name: u.display_name.clone(),
                role: m.role.clone(),
                joined_at: m.joined_at,
            })
        })
        .collect();
    Ok(result)
}

/// 踢除成员。owner 不可被踢(需先转让或解散),否则 400。
pub async fn remove_member(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
) -> Result<(), AppError> {
    // 先查 owner 用于防护。
    let team = TeamEntity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query team owner: {e}")))?;
    if let Some(t) = team.as_ref() {
        if t.owner_id == user_id {
            return Err(AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                "cannot remove owner; transfer or dissolve team instead".into(),
                None,
            ));
        }
    }
    let res = TeamMemberEntity::delete_many()
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(user_id.to_string()))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("delete member: {e}")))?;
    if res.rows_affected == 0 {
        return Err(AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "membership not found".into(),
            None,
        ));
    }
    Ok(())
}

// ── 生命周期 API(owner 转让 / team 解散 / 成员角色变更)──────────────────────

/// 转让队长:当前 owner 降为 admin,new_owner 升 owner,同步更新 teams.owner_id。事务。
/// `new_owner_user_id` 必须是当前成员(否则 404);禁止转让给自己(否则 400)。
pub async fn transfer_owner(
    db: &DatabaseConnection,
    team_id: &str,
    current_owner: &str,
    new_owner_user_id: &str,
) -> Result<(), AppError> {
    if current_owner == new_owner_user_id {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "cannot transfer to self".into(),
            None,
        ));
    }
    let txn = db
        .begin()
        .await
        .map_err(|e| AppError::internal(format!("begin txn: {e}")))?;
    // new_owner 必须是当前成员(同时取行,后续 update_many 会命中)。
    let _new_member = TeamMemberEntity::find()
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(new_owner_user_id.to_string()))
        .one(&txn)
        .await
        .map_err(|e| AppError::internal(format!("query new owner: {e}")))?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "new owner not a member".into(),
                None,
            )
        })?;
    // 当前 owner → admin
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(ROLE_ADMIN.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(current_owner.to_string()))
        .exec(&txn)
        .await
        .map_err(|e| AppError::internal(format!("demote owner: {e}")))?;
    // new owner → owner
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(ROLE_OWNER.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(new_owner_user_id.to_string()))
        .exec(&txn)
        .await
        .map_err(|e| AppError::internal(format!("promote owner: {e}")))?;
    // teams.owner_id 同步更新,保持与 team_members 一致。
    TeamEntity::update_many()
        .col_expr(TeamColumn::OwnerId, Expr::value(new_owner_user_id.to_string()))
        .filter(TeamColumn::Id.eq(team_id.to_string()))
        .exec(&txn)
        .await
        .map_err(|e| AppError::internal(format!("update team owner_id: {e}")))?;
    txn.commit()
        .await
        .map_err(|e| AppError::internal(format!("commit txn: {e}")))?;
    Ok(())
}

/// 解散 team:依赖 DB 外键 `ON DELETE CASCADE` 级联清理 members / threads / keys / audit。
pub async fn dissolve_team(db: &DatabaseConnection, team_id: &str) -> Result<(), AppError> {
    TeamEntity::delete_by_id(team_id.to_string())
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("dissolve team: {e}")))?;
    Ok(())
}

/// 改成员角色(仅 member↔admin)。禁止改成 owner(owner 变更走 transfer_owner)。
pub async fn set_member_role(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
    new_role: &str,
) -> Result<(), AppError> {
    if new_role != ROLE_ADMIN && new_role != ROLE_MEMBER {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "role must be admin or member".into(),
            None,
        ));
    }
    TeamMemberEntity::update_many()
        .col_expr(TeamMemberColumn::Role, Expr::value(new_role.to_string()))
        .filter(TeamMemberColumn::TeamId.eq(team_id.to_string()))
        .filter(TeamMemberColumn::UserId.eq(user_id.to_string()))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("set member role: {e}")))?;
    Ok(())
}

// ── 邀请码 ───────────────────────────────────────────────────────────────

/// owner 生成邀请码。expires_at/max_uses 为 None 表示不限。
pub async fn create_invitation(
    db: &DatabaseConnection,
    team_id: &str,
    created_by: &str,
    expires_at: Option<i64>,
    max_uses: Option<i32>,
) -> Result<Invitation, AppError> {
    let code = gen_invite_code();
    let id = new_id();
    let now = now_ms();
    let inv_am = InvitationActiveModel {
        id: Set(id.clone()),
        team_id: Set(team_id.to_string()),
        code: Set(code),
        created_by: Set(created_by.to_string()),
        expires_at: Set(expires_at),
        max_uses: Set(max_uses),
        used_count: Set(0),
        created_at: Set(now),
    };
    inv_am.insert(db)
        .await
        .map_err(|e| AppError::internal(format!("insert invitation: {e}")))?;
    let inv = InvitationEntity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("reload invitation: {e}")))?
        .ok_or_else(|| AppError::internal("inserted invitation not found".into()))?;
    Ok(inv)
}

/// 凭邀请码加入 team(成为 member)。幂等:已是成员直接返回 team。事务 + FOR UPDATE
/// 防并发超额使用。码不存在/过期/用尽 → 4xx。
pub async fn join_team(
    db: &DatabaseConnection,
    user_id: &str,
    code: &str,
) -> Result<Team, AppError> {
    let code_str = code.trim().to_string();
    let now = now_ms();
    let user_id = user_id.to_string();

    let team_id: String = db
        .transaction(|txn| {
            let code_str = code_str.clone();
            let user_id = user_id.clone();
            Box::pin(async move {
                // 行锁 invitation:SELECT ... FOR UPDATE(按 code 唯一索引锁定行)。
                let inv = InvitationEntity::find()
                    .filter(InvitationColumn::Code.eq(code_str))
                    .lock(sea_orm::sea_query::LockType::Update)
                    .one(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("query invitation: {e}")))?
                    .ok_or_else(|| {
                        AppError::business(
                            ErrorCode::HttpNotFound,
                            StatusCode::NOT_FOUND,
                            "invitation not found".into(),
                            None,
                        )
                    })?;

                if let Some(exp) = inv.expires_at {
                    if exp < now {
                        return Err(AppError::business(
                            ErrorCode::HttpBadRequest,
                            StatusCode::BAD_REQUEST,
                            "invitation expired".into(),
                            None,
                        ));
                    }
                }
                if let Some(max) = inv.max_uses {
                    if inv.used_count >= max {
                        return Err(AppError::business(
                            ErrorCode::HttpBadRequest,
                            StatusCode::BAD_REQUEST,
                            "invitation usage limit reached".into(),
                            None,
                        ));
                    }
                }

                // 幂等:已是成员则跳过插入与计数。
                let existing = TeamMemberEntity::find()
                    .filter(TeamMemberColumn::TeamId.eq(inv.team_id.clone()))
                    .filter(TeamMemberColumn::UserId.eq(user_id.clone()))
                    .one(txn)
                    .await
                    .map_err(|e| {
                        AppError::internal(format!("query existing membership: {e}"))
                    })?;

                if existing.is_none() {
                    let member_am = TeamMemberActiveModel {
                        team_id: Set(inv.team_id.clone()),
                        user_id: Set(user_id),
                        role: Set(ROLE_MEMBER.to_string()),
                        joined_at: Set(now),
                    };
                    member_am.insert(txn)
                        .await
                        .map_err(|e| AppError::internal(format!("insert membership: {e}")))?;
                    // used_count + 1:采用 ActiveModel.update()(get model → set → update)。
                    let mut inv_am: InvitationActiveModel = inv.clone().into();
                    inv_am.used_count = Set(inv.used_count + 1);
                    inv_am
                        .update(txn)
                        .await
                        .map_err(|e| {
                            AppError::internal(format!("update invitation used_count: {e}"))
                        })?;
                }

                Ok::<_, AppError>(inv.team_id)
            })
        })
        .await
        .map_err(|e| AppError::internal(format!("tx: {e}")))?;

    // 取出最终 team(与原 SELECT 行为一致)。
    load_team(db, &team_id).await
}

/// 取 team 主键对应记录;找不到则内部 500。
async fn load_team(db: &DatabaseConnection, team_id: &str) -> Result<Team, AppError> {
    TeamEntity::find_by_id(team_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query team: {e}")))?
        .ok_or_else(|| AppError::internal("team vanished after join".into()))
}

// ── 辅助 ─────────────────────────────────────────────────────────────────

/// 生成 12 位邀请码(去除易混淆字符 0/O/1/I)。
fn gen_invite_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_code_format() {
        for _ in 0..50 {
            let c = gen_invite_code();
            assert_eq!(c.len(), 12, "code must be 12 chars: {c}");
            assert!(c.chars().all(|ch| ch.is_ascii_alphanumeric()), "alphanumeric: {c}");
            // 不含易混淆字符。
            assert!(!c.contains('0') && !c.contains('O') && !c.contains('1') && !c.contains('I'));
        }
    }
}

//! team 管理:创建 / 列表 / 成员 / 邀请码 / 加入 / 踢除,以及成员校验 helper。
//!
//! 所有函数接 `&PgPool`(handler 在 M1.5 组装)。权限铁律:`require_member` /
//! `require_owner` 用于在 handler 里校验"当前 user 是否属于该 team / 是否 owner"。

use crate::error::{AppError, ErrorCode};
pub use crate::multitenant::models::{Invitation, Team};
use crate::multitenant::{new_id, now_ms};
use axum::http::StatusCode;
use rand::Rng;
use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::FromRow;

pub const ROLE_OWNER: &str = "owner";
pub const ROLE_MEMBER: &str = "member";

const TEAM_COLUMNS: &str = "id, name, owner_id, created_at, updated_at";
const INVITATION_COLUMNS: &str =
    "id, team_id, code, created_by, expires_at, max_uses, used_count, created_at";

/// 成员视图(联表 user + team_members),用于 list_members 返回。
#[derive(Debug, FromRow, Serialize)]
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
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<Option<String>, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT role FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| AppError::internal(format!("query member role: {e}")))?;
    Ok(row.map(|(r,)| r))
}

/// 要求 user 是 team 成员,返回其 role;否则 403。
pub async fn require_member(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<String, AppError> {
    match member_role(pool, team_id, user_id).await? {
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
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<(), AppError> {
    let role = require_member(pool, team_id, user_id).await?;
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
pub async fn create_team(pool: &PgPool, owner_id: &str, name: &str) -> Result<Team, AppError> {
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
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::internal(format!("begin tx: {e}")))?;
    let team: Team = sqlx::query_as(&format!(
        "INSERT INTO teams (id, name, owner_id, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $4) RETURNING {TEAM_COLUMNS}"
    ))
    .bind(&team_id)
    .bind(name)
    .bind(owner_id)
    .bind(now)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("insert team: {e}")))?;
    sqlx::query(
        "INSERT INTO team_members (team_id, user_id, role, joined_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(&team_id)
    .bind(owner_id)
    .bind(ROLE_OWNER)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("insert owner membership: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| AppError::internal(format!("commit tx: {e}")))?;
    Ok(team)
}

/// 列出 user 所属的全部 team(一人多 team),按创建时间倒序。
pub async fn list_my_teams(pool: &PgPool, user_id: &str) -> Result<Vec<Team>, AppError> {
    let sql = format!(
        "SELECT t.id, t.name, t.owner_id, t.created_at, t.updated_at \
         FROM teams t JOIN team_members m ON m.team_id = t.id \
         WHERE m.user_id = $1 ORDER BY t.created_at DESC"
    );
    sqlx::query_as::<_, Team>(&sql)
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::internal(format!("list teams: {e}")))
}

/// 列出 team 全部成员(联表 users)。
pub async fn list_members(pool: &PgPool, team_id: &str) -> Result<Vec<MemberView>, AppError> {
    sqlx::query_as::<_, MemberView>(
        "SELECT u.id AS user_id, u.email, u.display_name, m.role, m.joined_at \
         FROM team_members m JOIN users u ON u.id = m.user_id \
         WHERE m.team_id = $1 ORDER BY m.joined_at ASC",
    )
    .bind(team_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::internal(format!("list members: {e}")))
}

/// 踢除成员。owner 不可被踢(需先转让或解散),否则 400。
pub async fn remove_member(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<(), AppError> {
    let team: Option<(String,)> = sqlx::query_as("SELECT owner_id FROM teams WHERE id = $1")
        .bind(team_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| AppError::internal(format!("query team owner: {e}")))?;
    if let Some((owner,)) = team.as_ref() {
        if owner == user_id {
            return Err(AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                "cannot remove owner; transfer or dissolve team instead".into(),
                None,
            ));
        }
    }
    let res = sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
        .bind(team_id)
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::internal(format!("delete member: {e}")))?;
    if res.rows_affected() == 0 {
        return Err(AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "membership not found".into(),
            None,
        ));
    }
    Ok(())
}

// ── 邀请码 ───────────────────────────────────────────────────────────────

/// owner 生成邀请码。expires_at/max_uses 为 None 表示不限。
pub async fn create_invitation(
    pool: &PgPool,
    team_id: &str,
    created_by: &str,
    expires_at: Option<i64>,
    max_uses: Option<i32>,
) -> Result<Invitation, AppError> {
    let code = gen_invite_code();
    let id = new_id();
    let now = now_ms();
    let inv: Invitation = sqlx::query_as(&format!(
        "INSERT INTO invitations (id, team_id, code, created_by, expires_at, max_uses, used_count, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 0, $7) RETURNING {INVITATION_COLUMNS}"
    ))
    .bind(&id)
    .bind(team_id)
    .bind(&code)
    .bind(created_by)
    .bind(expires_at)
    .bind(max_uses)
    .bind(now)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::internal(format!("insert invitation: {e}")))?;
    Ok(inv)
}

/// 凭邀请码加入 team(成为 member)。幂等:已是成员直接返回 team。事务 + FOR UPDATE
/// 防并发超额使用。码不存在/过期/用尽 → 4xx。
pub async fn join_team(pool: &PgPool, user_id: &str, code: &str) -> Result<Team, AppError> {
    let code = code.trim();
    let now = now_ms();
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::internal(format!("begin tx: {e}")))?;

    let inv: Option<Invitation> = sqlx::query_as(&format!(
        "SELECT {INVITATION_COLUMNS} FROM invitations WHERE code = $1 FOR UPDATE"
    ))
    .bind(code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("query invitation: {e}")))?;
    let inv = match inv {
        Some(i) => i,
        None => {
            return Err(AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "invitation not found".into(),
                None,
            ))
        }
    };
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
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT user_id FROM team_members WHERE team_id = $1 AND user_id = $2",
    )
    .bind(&inv.team_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| AppError::internal(format!("query existing membership: {e}")))?;

    if existing.is_none() {
        sqlx::query(
            "INSERT INTO team_members (team_id, user_id, role, joined_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(&inv.team_id)
        .bind(user_id)
        .bind(ROLE_MEMBER)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::internal(format!("insert membership: {e}")))?;
        sqlx::query("UPDATE invitations SET used_count = used_count + 1 WHERE id = $1")
            .bind(&inv.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::internal(format!("update invitation used_count: {e}")))?;
    }

    let team: Team = sqlx::query_as(&format!("SELECT {TEAM_COLUMNS} FROM teams WHERE id = $1"))
        .bind(&inv.team_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::internal(format!("query team: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| AppError::internal(format!("commit tx: {e}")))?;
    Ok(team)
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

//! 审计日志(M6):记录 team owner 的关键操作(设 key / 邀请 / 踢除 / 解散等),
//! 供安全合规追溯。owner 可查本 team 的审计记录。

use crate::error::AppError;
use crate::multitenant::models::AuditLog;
use crate::multitenant::{new_id, now_ms};
use sqlx::PgPool;

/// 记录一条审计日志。best-effort:失败仅 warn,不阻断主操作(审计不应让业务失败)。
pub async fn record(
    pool: &PgPool,
    team_id: &str,
    actor_user_id: &str,
    action: &str,
    detail: Option<&str>,
) {
    let id = new_id();
    let now = now_ms();
    if let Err(e) = sqlx::query(
        "INSERT INTO audit_log (id, team_id, actor_user_id, action, detail, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(team_id)
    .bind(actor_user_id)
    .bind(action)
    .bind(detail)
    .bind(now)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, "insert audit log failed (non-fatal)");
    }
}

/// 列出 team 审计日志(按时间倒序,默认上限 200)。
pub async fn list(pool: &PgPool, team_id: &str, limit: i64) -> Result<Vec<AuditLog>, AppError> {
    let limit = limit.clamp(1, 500);
    sqlx::query_as::<_, AuditLog>(
        "SELECT id, team_id, actor_user_id, action, detail, created_at FROM audit_log \
         WHERE team_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(team_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::internal(format!("list audit: {e}")))
}

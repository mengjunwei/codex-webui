//! 审计日志(M6):记录 team owner 的关键操作(设 key / 邀请 / 踢除 / 解散等),
//! 供安全合规追溯。owner 可查本 team 的审计记录。

use crate::error::AppError;
use crate::db::entities::audit_log::{
    ActiveModel as AuditLogActiveModel, Column as AuditLogColumn, Entity as AuditLogEntity,
    Model as AuditLogModel,
};
use crate::services::multitenant::{new_id, now_ms};
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, QueryFilter, QueryOrder, QuerySelect, Set};

/// 记录一条审计日志。best-effort:失败仅 warn,不阻断主操作(审计不应让业务失败)。
pub async fn record(
    db: &DatabaseConnection,
    team_id: &str,
    actor_user_id: &str,
    action: &str,
    detail: Option<&str>,
) {
    let id = new_id();
    let now = now_ms();
    let am = AuditLogActiveModel {
        id: Set(id),
        team_id: Set(team_id.to_string()),
        actor_user_id: Set(actor_user_id.to_string()),
        action: Set(action.to_string()),
        // detail 列允许 NULL:None 显式写入 NULL(等价于原 sqlx bind(None))
        detail: Set(detail.map(str::to_string)),
        created_at: Set(now),
    };
    if let Err(e) = am.insert(db).await {
        tracing::warn!(error = %e, "insert audit log failed (non-fatal)");
    }
}

/// 列出 team 审计日志(按时间倒序,默认上限 200)。
pub async fn list(
    db: &DatabaseConnection,
    team_id: &str,
    limit: i64,
) -> Result<Vec<AuditLogModel>, AppError> {
    let limit = limit.clamp(1, 500);
    AuditLogEntity::find()
        .filter(AuditLogColumn::TeamId.eq(team_id.to_string()))
        .order_by_desc(AuditLogColumn::CreatedAt)
        .limit(limit as u64)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("list audit: {e}")))
}

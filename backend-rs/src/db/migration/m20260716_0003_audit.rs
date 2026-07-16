//! M6:审计日志(team owner 关键操作留痕:设 key / 邀请 / 踢除等)。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0003_audit"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS audit_log (
                    id VARCHAR(36) PRIMARY KEY,
                    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
                    actor_user_id VARCHAR(36) NOT NULL REFERENCES users(id),
                    action VARCHAR(64) NOT NULL,
                    detail TEXT,
                    created_at BIGINT NOT NULL
                )"#,
            )
            .await?;

        create_index(manager, "idx_audit_team", "audit_log", "team_id, created_at DESC").await?;
        Ok(())
    }
}

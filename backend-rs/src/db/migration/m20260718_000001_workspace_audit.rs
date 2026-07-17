//! M(per-user workspace 实施步骤 1):hook 审计落库。
//!
//! workspace_role 复用 `team_members.role`(owner/admin/member),不再单建表;
//! workspace_audit 单独建表,接 hook webhook 落库。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260718_000001_workspace_audit"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // workspace_audit:hook webhook 推送的所有事件原样入库(JSON payload 用 TEXT 存,与项目约定一致)。
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS workspace_audit (
                    id BIGSERIAL PRIMARY KEY,
                    team_id VARCHAR(36),
                    user_id VARCHAR(36),
                    thread_id VARCHAR(36),
                    event_type VARCHAR(64) NOT NULL,
                    tool_name VARCHAR(64),
                    payload_json TEXT NOT NULL,
                    decision VARCHAR(16),
                    created_at BIGINT NOT NULL
                )"#,
            )
            .await?;

        create_index(
            manager,
            "idx_workspace_audit_team_user_ts",
            "workspace_audit",
            "team_id, user_id, created_at DESC",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS workspace_audit")
            .await?;
        Ok(())
    }
}
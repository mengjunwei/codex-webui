//! M2:team_api_keys(BYOK 加密存储)。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0002_api_keys"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS team_api_keys (
                    id VARCHAR(36) PRIMARY KEY,
                    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
                    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
                    encrypted_key TEXT NOT NULL,
                    key_hint VARCHAR(16) NOT NULL,
                    set_by VARCHAR(36) NOT NULL REFERENCES users(id),
                    is_active BOOLEAN NOT NULL DEFAULT FALSE,
                    created_at BIGINT NOT NULL,
                    updated_at BIGINT NOT NULL
                )"#,
            )
            .await?;

        create_index(manager, "idx_team_api_keys_team", "team_api_keys", "team_id, is_active").await?;
        Ok(())
    }
}

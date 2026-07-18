//! M9: user_api_keys(用户个人 BYOK 加密存储)。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260718_000002_user_api_keys"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS user_api_keys (
                    id VARCHAR(36) PRIMARY KEY,
                    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
                    encrypted_key TEXT NOT NULL,
                    key_hint VARCHAR(16) NOT NULL,
                    is_active BOOLEAN NOT NULL DEFAULT FALSE,
                    created_at BIGINT NOT NULL,
                    updated_at BIGINT NOT NULL
                )"#,
            )
            .await?;

        create_index(manager, "idx_user_api_keys_user", "user_api_keys", "user_id, is_active").await?;
        Ok(())
    }
}

//! 业务表迁移:现有 rusqlite 5 张业务表(token_usage_snapshots/turn_diffs/settings/
//! pending_server_requests/turn_errors)建 PG/MySQL 版。对应 drizzle 0000~0004。
//! 类型:SQLite text->VARCHAR/TEXT,integer->BIGINT。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0004_business"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS token_usage_snapshots (
                thread_id VARCHAR(36) NOT NULL,
                turn_id VARCHAR(64) NOT NULL,
                total_tokens BIGINT NOT NULL,
                input_tokens BIGINT NOT NULL,
                cached_input_tokens BIGINT NOT NULL,
                output_tokens BIGINT NOT NULL,
                reasoning_output_tokens BIGINT NOT NULL,
                last_total_tokens BIGINT NOT NULL,
                last_input_tokens BIGINT NOT NULL,
                last_cached_input_tokens BIGINT NOT NULL,
                last_output_tokens BIGINT NOT NULL,
                last_reasoning_output_tokens BIGINT NOT NULL,
                model_context_window BIGINT,
                raw_payload TEXT NOT NULL,
                updated_at BIGINT NOT NULL,
                PRIMARY KEY (thread_id, turn_id)
            )"#,
        )
        .await?;
        create_index(manager, "idx_token_usage_thread_updated", "token_usage_snapshots", "thread_id, updated_at").await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS turn_diffs (
                thread_id VARCHAR(36) NOT NULL,
                turn_id VARCHAR(64) NOT NULL,
                diff TEXT NOT NULL,
                updated_at BIGINT NOT NULL,
                PRIMARY KEY (thread_id, turn_id)
            )"#,
        )
        .await?;
        create_index(manager, "idx_turn_diffs_thread", "turn_diffs", "thread_id").await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS settings (
                setting_key VARCHAR(128) PRIMARY KEY NOT NULL,
                value TEXT,
                type VARCHAR(32) NOT NULL,
                category VARCHAR(64) NOT NULL,
                description TEXT NOT NULL,
                default_value TEXT NOT NULL,
                constraints TEXT NOT NULL,
                updated_at BIGINT NOT NULL
            )"#,
        )
        .await?;
        create_index(manager, "idx_settings_category", "settings", "category").await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS pending_server_requests (
                generation BIGINT NOT NULL,
                request_id VARCHAR(64) NOT NULL,
                thread_id VARCHAR(36) NOT NULL,
                turn_id VARCHAR(64),
                item_id VARCHAR(128),
                method VARCHAR(64) NOT NULL,
                params_json TEXT NOT NULL,
                status VARCHAR(32) NOT NULL,
                resolved_by VARCHAR(128),
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                resolved_at BIGINT,
                PRIMARY KEY (generation, request_id)
            )"#,
        )
        .await?;
        create_index(manager, "idx_pending_requests_thread_status", "pending_server_requests", "thread_id, status").await?;
        create_index(manager, "idx_pending_requests_status_updated", "pending_server_requests", "status, updated_at").await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS turn_errors (
                thread_id VARCHAR(36) NOT NULL,
                turn_id VARCHAR(64) NOT NULL,
                message TEXT NOT NULL,
                created_at BIGINT NOT NULL,
                PRIMARY KEY (thread_id, turn_id)
            )"#,
        )
        .await?;
        create_index(manager, "idx_turn_errors_thread", "turn_errors", "thread_id").await?;
        Ok(())
    }
}

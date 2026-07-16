//! M1 初始迁移:users / teams / team_members / invitations / refresh_tokens / threads 元数据。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0001_initial"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS users (
                id VARCHAR(36) PRIMARY KEY,
                email VARCHAR(255) NOT NULL UNIQUE,
                password_hash VARCHAR(255) NOT NULL,
                email_verified_at BIGINT,
                display_name VARCHAR(255),
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS teams (
                id VARCHAR(36) PRIMARY KEY,
                name VARCHAR(255) NOT NULL,
                owner_id VARCHAR(36) NOT NULL REFERENCES users(id),
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS team_members (
                team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
                user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                role VARCHAR(16) NOT NULL,
                joined_at BIGINT NOT NULL,
                PRIMARY KEY (team_id, user_id)
            )"#,
        )
        .await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS invitations (
                id VARCHAR(36) PRIMARY KEY,
                team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
                code VARCHAR(64) NOT NULL UNIQUE,
                created_by VARCHAR(36) NOT NULL REFERENCES users(id),
                expires_at BIGINT,
                max_uses INT,
                used_count INT NOT NULL DEFAULT 0,
                created_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS refresh_tokens (
                id VARCHAR(36) PRIMARY KEY,
                user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                token_hash VARCHAR(255) NOT NULL UNIQUE,
                expires_at BIGINT NOT NULL,
                revoked BOOLEAN NOT NULL DEFAULT FALSE,
                created_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS threads (
                id VARCHAR(36) PRIMARY KEY,
                team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
                created_by_user_id VARCHAR(36) NOT NULL REFERENCES users(id),
                title VARCHAR(255),
                status VARCHAR(16) NOT NULL DEFAULT 'active',
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                last_activity_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        create_index(manager, "idx_team_members_user", "team_members", "user_id").await?;
        create_index(manager, "idx_threads_team", "threads", "team_id").await?;
        create_index(manager, "idx_threads_status", "threads", "team_id, status").await?;
        Ok(())
    }
}

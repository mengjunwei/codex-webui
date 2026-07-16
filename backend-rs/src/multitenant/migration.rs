//! 手动迁移执行器。
//!
//! 为何不用 sqlx::migrate!:sqlx 的 `migrate` feature 会经 `sqlx-sqlite?/...` 牵出
//! sqlx-sqlite,与现有 rusqlite 的 libsqlite3-sys links 冲突(SeaORM issue #2725 同源)。
//! 故 sqlx 仅开 `runtime-tokio-rustls + postgres`,迁移用本模块手动管理:
//! `schema_migrations` 表记录已应用版本,启动时顺序执行未应用的。

use sqlx::postgres::PgPool;

use crate::multitenant::now_ms;

/// M1 初始迁移:users / teams / team_members / invitations / refresh_tokens / threads 元数据。
///
/// 类型约定(兼容 PG/MySQL):主键外键 VARCHAR(36) UUIDv7;时间 BIGINT(i64 UTC 毫秒);
/// 枚举 VARCHAR + 应用层校验;布尔 BOOLEAN;不用 JSON/ENUM 特殊类型。
const MIGRATION_2026071601_INITIAL: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id VARCHAR(36) PRIMARY KEY,
    email VARCHAR(255) NOT NULL UNIQUE,
    password_hash VARCHAR(255) NOT NULL,
    email_verified_at BIGINT,
    display_name VARCHAR(255),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS teams (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    owner_id VARCHAR(36) NOT NULL REFERENCES users(id),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS team_members (
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(16) NOT NULL,
    joined_at BIGINT NOT NULL,
    PRIMARY KEY (team_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_team_members_user ON team_members(user_id);

CREATE TABLE IF NOT EXISTS invitations (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    code VARCHAR(64) NOT NULL UNIQUE,
    created_by VARCHAR(36) NOT NULL REFERENCES users(id),
    expires_at BIGINT,
    max_uses INT,
    used_count INT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS refresh_tokens (
    id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash VARCHAR(255) NOT NULL UNIQUE,
    expires_at BIGINT NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS threads (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    created_by_user_id VARCHAR(36) NOT NULL REFERENCES users(id),
    title VARCHAR(255),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    last_activity_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_threads_team ON threads(team_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(team_id, status);
"#;

/// M2:team_api_keys(BYOK 加密存储)。
const MIGRATION_2026071602_API_KEYS: &str = r#"
CREATE TABLE IF NOT EXISTS team_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
    encrypted_key TEXT NOT NULL,
    key_hint VARCHAR(16) NOT NULL,
    set_by VARCHAR(36) NOT NULL REFERENCES users(id),
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_team_api_keys_team ON team_api_keys(team_id, is_active);
"#;

/// 所有迁移(版本号 → SQL),按顺序应用。新增迁移在此追加。
fn migrations() -> Vec<(&'static str, &'static str)> {
    vec![
        ("2026071601_initial", MIGRATION_2026071601_INITIAL),
        ("2026071602_api_keys", MIGRATION_2026071602_API_KEYS),
    ]
}

/// 执行所有未应用的多租户迁移。幂等:已应用的跳过。
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 版本记录表。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version TEXT PRIMARY KEY, applied_at BIGINT NOT NULL)",
    )
    .execute(pool)
    .await?;

    for (version, sql) in migrations() {
        let applied: Option<(String,)> =
            sqlx::query_as("SELECT version FROM schema_migrations WHERE version = $1")
                .bind(version)
                .fetch_optional(pool)
                .await?;
        if applied.is_some() {
            continue;
        }
        // raw_sql 支持多语句(PG simple query 协议),用于一次执行多条 CREATE。
        sqlx::raw_sql(sql).execute(pool).await?;
        sqlx::query("INSERT INTO schema_migrations (version, applied_at) VALUES ($1, $2)")
            .bind(version)
            .bind(now_ms())
            .execute(pool)
            .await?;
        tracing::info!(version, "applied multitenant migration");
    }
    Ok(())
}

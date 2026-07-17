//! 计费/配额预留(M6):per-team 配额与用量计数表。
//!
//! - `turn_quota_hourly` / `token_quota_monthly`:配额上限(0 = 不限)。
//! - `used_turns_hour` + `hour_bucket`:滑动小时用量,`hour_bucket` 变化时重置。
//! - `used_tokens_month` + `month_bucket`:月度 token 用量。
//!
//! 应用层在每次 turn 前校验配额、turn 完成后累加用量(原子 UPDATE)。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0006_quota"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS team_quotas (
                team_id VARCHAR(36) PRIMARY KEY NOT NULL,
                plan VARCHAR(32) NOT NULL DEFAULT 'free',
                turn_quota_hourly BIGINT NOT NULL DEFAULT 0,
                token_quota_monthly BIGINT NOT NULL DEFAULT 0,
                used_turns_hour BIGINT NOT NULL DEFAULT 0,
                hour_bucket BIGINT NOT NULL DEFAULT 0,
                used_tokens_month BIGINT NOT NULL DEFAULT 0,
                month_bucket VARCHAR(7) NOT NULL DEFAULT '',
                updated_at BIGINT NOT NULL
            )"#,
        )
        .await?;
        Ok(())
    }
}

//! team_routes 覆盖表(M4 failover 决策记录):team_id → worker_id + 映射原因。
//! 防节点抖动回切:查询路由先 team_routes 后哈希;failover 时落新 worker 并记 reason。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0007_team_routes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS team_routes (
                    team_id VARCHAR(36) PRIMARY KEY NOT NULL,
                    worker_id VARCHAR(64) NOT NULL,
                    mapped_at BIGINT NOT NULL,
                    mapped_reason VARCHAR(16) NOT NULL DEFAULT 'initial'
                )"#,
            )
            .await?;
        Ok(())
    }
}

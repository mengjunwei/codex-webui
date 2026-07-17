//! session_replicas:per-team 主副本映射(active-passive HA)。
//! team_id PK → primary_node(跑 codex 的主节点)+ replica_node(存 rollout 副本的副本节点)。
//! 替代 team_routes(多 worker 分片模型)。status:active|promoting|degraded。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0008_session_replicas"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS session_replicas (
                    team_id VARCHAR(36) PRIMARY KEY NOT NULL,
                    primary_node VARCHAR(64) NOT NULL,
                    replica_node VARCHAR(64),
                    status VARCHAR(16) NOT NULL DEFAULT 'active',
                    primary_lease_until BIGINT NOT NULL DEFAULT 0,
                    updated_at BIGINT NOT NULL
                )"#,
            )
            .await?;
        Ok(())
    }
}

//! session_replicas 主键迁移:per-team(team_id) → per-thread(thread_id)。
//!
//! 阶段 B 基础设施第一步。旧表改名暂存,新建 per-thread 表后,
//! 按 threads.team_id join 展开每个 team 的当前活跃 thread 为 per-thread 行;
//! 无活跃 thread 的旧 team 行丢弃(per-team 记录在 per-thread 模型下无意义,
//! 后续运行时 get_or_assign 会按需补建)。
//!
//! 多方言(PG/MySQL):
//! - ALTER ... RENAME TO / CREATE TABLE IF NOT EXISTS / DROP TABLE IF EXISTS:PG/MySQL 均支持。
//! - COMMENT ON ...:仅 PG;MySQL 下 .ok() 吞错。
//! - I5:数据迁移按方言分支 —— PG 用 INSERT...ON CONFLICT DO NOTHING;
//!   MySQL 不支持 ON CONFLICT(整条失败被 .ok() 吞 → 零行迁移 → 随后 DROP 旧表丢数据),
//!   改用 INSERT IGNORE。失败改 tracing::warn!(不再静默 .ok())。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260721_000001_session_replicas_per_thread"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 1. 旧表 session_replicas(team_id PK) 改名为 session_replicas_old。
        //    IF EXISTS 兼容全新库(旧表不存在);.ok() 容错。
        db.execute_unprepared(
            "ALTER TABLE IF EXISTS session_replicas RENAME TO session_replicas_old",
        )
        .await
        .ok();

        // 2. 建新表(thread_id PK)。
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS session_replicas (
                thread_id VARCHAR(36) PRIMARY KEY NOT NULL,
                primary_node VARCHAR(64) NOT NULL,
                replica_node VARCHAR(64),
                status VARCHAR(16) NOT NULL DEFAULT 'active',
                primary_lease_until BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL
            )"#,
        )
        .await?;

        // 3. I5:数据迁移按方言分支。为每个旧 team 行,按该 team 当前活跃 thread 展开成 per-thread 行。
        //    无活跃 thread 的旧 team 行丢弃。PG 用 ON CONFLICT;MySQL 用 INSERT IGNORE
        //    (MySQL 不支持 ON CONFLICT,原整条失败被 .ok() 吞 → 零行迁移 → DROP 旧表丢既有映射)。
        //    必须在步骤 4 DROP 旧表前完成迁移尝试;失败 warn(不静默),便于运维介入。
        use sea_orm::DbBackend;
        let insert_sql = match manager.get_database_backend() {
            DbBackend::MySql => r#"INSERT IGNORE INTO session_replicas (thread_id, primary_node, replica_node, status, primary_lease_until, updated_at)
                   SELECT t.id, o.primary_node, o.replica_node, o.status, o.primary_lease_until, o.updated_at
                   FROM threads t
                   JOIN session_replicas_old o ON t.team_id = o.team_id"#,
            // Postgres(及其它):ON CONFLICT DO NOTHING 处理主键冲突。
            _ => r#"INSERT INTO session_replicas (thread_id, primary_node, replica_node, status, primary_lease_until, updated_at)
                   SELECT t.id, o.primary_node, o.replica_node, o.status, o.primary_lease_until, o.updated_at
                   FROM threads t
                   JOIN session_replicas_old o ON t.team_id = o.team_id
                   ON CONFLICT (thread_id) DO NOTHING"#,
        };
        if let Err(e) = db.execute_unprepared(insert_sql).await {
            tracing::warn!(
                error = %e,
                "session_replicas per-thread data migration failed \
                 (per-thread rows will be built at runtime by get_or_assign)"
            );
        }

        // 4. 删旧表(迁移已在步骤 3 尝试过)。
        db.execute_unprepared("DROP TABLE IF EXISTS session_replicas_old")
            .await
            .ok();

        // 5. 注释(仅 PG;MySQL .ok() 吞错)。
        //    每条 COMMENT 独立 execute_unprepared:PG 下 execute_unprepared 走单 prepared
        //    statement,多语句拼接时 libpq 默认只执行首条,后续 COMMENT 会静默丢失 → 表/列
        //    元数据建不完整。拆分后保证每条都执行。
        db.execute_unprepared(
            "COMMENT ON TABLE session_replicas IS 'per-thread 主副本映射(active-passive HA):thread_id → primary + replica'",
        )
        .await
        .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.thread_id IS '会话 ID(主键)'")
            .await
            .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.primary_node IS '跑 codex 的主节点 ID'")
            .await
            .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.replica_node IS '存 rollout/workspace 副本的节点 ID(可空)'")
            .await
            .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.status IS '状态:active / promoting / degraded'")
            .await
            .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.primary_lease_until IS '主节点租约到期时间戳(毫秒)'")
            .await
            .ok();
        db.execute_unprepared("COMMENT ON COLUMN session_replicas.updated_at IS '更新时间戳(毫秒)'")
            .await
            .ok();

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // 不可逆(per-team → per-thread 展开是多对一,无法还原)。
        Ok(())
    }
}

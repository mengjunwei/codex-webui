//! cluster_extensions 加 marketplace 列(plugin 的市场名,skill/mcp 为 NULL)。
//!
//! 多方言(PG/MySQL):
//! - ADD/DROP COLUMN:PG 支持 `IF NOT EXISTS`/`IF EXISTS` 幂等修饰符;
//!   MySQL(含 8.0)不支持 `ADD COLUMN IF NOT EXISTS`/`DROP COLUMN IF EXISTS` 语法 →
//!   去 IF + `.ok()` 容错(列已存在/不存在时静默,参照项目 000002 迁移的 MySQL 容错模式)。
//! - 索引:复用 `create_index` 助手(PG `IF NOT EXISTS`,MySQL 普通 `CREATE INDEX`,
//!   迁移顺序保证首次执行索引不存在)。

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260722_000003_cluster_extensions_marketplace"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();

        // 加列:PG 用 IF NOT EXISTS 幂等;MySQL 不支持该语法,去 IF + .ok() 容错(列已存在静默)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extensions ADD COLUMN marketplace VARCHAR(128)",
                )
                .await
                .ok();
            }
            _ => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extensions ADD COLUMN IF NOT EXISTS marketplace VARCHAR(128)",
                )
                .await?;
            }
        }

        // 给 plugin 行建索引(按 marketplace 查);助手内部按方言分支,幂等。
        crate::db::migration::create_index(
            manager,
            "idx_ext_marketplace",
            "cluster_extensions",
            "marketplace",
        )
        .await?;

        // PG 列注释(MySQL 无 COMMENT ON COLUMN,.ok() 静默)。
        db.execute_unprepared(
            "COMMENT ON COLUMN cluster_extensions.marketplace IS 'plugin 的市场名(skill/mcp 为空)'",
        )
        .await
        .ok();

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();

        // 删列:PG 用 IF EXISTS 幂等;MySQL 不支持该语法,去 IF + .ok() 容错(列不存在静默)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared("ALTER TABLE cluster_extensions DROP COLUMN marketplace")
                    .await
                    .ok();
            }
            _ => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extensions DROP COLUMN IF EXISTS marketplace",
                )
                .await?;
            }
        }

        Ok(())
    }
}

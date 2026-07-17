//! 业务表多租户隔离:为 4 张业务表(token_usage_snapshots / turn_diffs /
//! pending_server_requests / turn_errors)补 `team_id` 列 + 索引,堵死跨 team 越权。
//!
//! `settings` 为全局运行时配置,不归属 team,不加 team_id。
//!
//! team_id 来源:应用层写入业务表时从 threads.team_id 推导并填入;
//! 历史数据(全新库为空)按列可空过渡,查询永远带 team_id 过滤。
//!
//! 跨方言幂等:用 information_schema 检查列是否存在,避免重复 ADD COLUMN 报错
//! (PG 支持 IF NOT EXISTS,MySQL 不支持;统一走检查)。

use super::create_index;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260716_0005_business_team_id"
    }
}

/// 判断列是否存在(information_schema 跨 PG/MySQL 一致)。
async fn has_column(db: &impl ConnectionTrait, table: &str, col: &str) -> bool {
    let backend = db.get_database_backend();
    let sql = format!(
        "SELECT COUNT(*) AS c FROM information_schema.columns \
         WHERE table_name = '{table}' AND column_name = '{col}'"
    );
    db.query_one(Statement::from_sql_and_values(backend, sql, []))
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get_by_index::<i64>(0).ok())
        .map(|c| c > 0)
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();

        // 4 张业务表补 team_id 列(可空过渡)+ 索引。
        let tables: &[&str] = &[
            "token_usage_snapshots",
            "turn_diffs",
            "pending_server_requests",
            "turn_errors",
        ];
        for t in tables {
            if !has_column(db, t, "team_id").await {
                // ALTER TABLE … ADD COLUMN team_id VARCHAR(36)(PG/MySQL 均支持的通用语法)。
                db.execute_unprepared(&format!(
                    "ALTER TABLE {t} ADD COLUMN team_id VARCHAR(36)"
                ))
                .await?;
            }
            create_index(manager, &format!("idx_{t}_team"), t, "team_id").await?;
        }

        // 回填历史数据的 team_id(跨方言分支;全新库为空,等价 no-op)。
        match backend {
            DatabaseBackend::Postgres => {
                for t in tables {
                    db.execute_unprepared(&format!(
                        "UPDATE {t} SET team_id = th.team_id \
                         FROM threads th \
                         WHERE th.id = {t}.thread_id AND {t}.team_id IS NULL"
                    ))
                    .await?;
                }
            }
            DatabaseBackend::MySql => {
                for t in tables {
                    db.execute_unprepared(&format!(
                        "UPDATE {t} JOIN threads th ON th.id = {t}.thread_id \
                         SET {t}.team_id = th.team_id \
                         WHERE {t}.team_id IS NULL"
                    ))
                    .await?;
                }
            }
            DatabaseBackend::Sqlite => {}
        }

        Ok(())
    }
}

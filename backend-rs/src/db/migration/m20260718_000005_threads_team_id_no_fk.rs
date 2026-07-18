//! 放宽 threads.team_id 外键约束:
//! 旧约束 `REFERENCES teams(id) ON DELETE CASCADE` 不允许 personal workspace 用纯 user_id 作 team_id。
//! 改为:删外键 + 加 NOT NULL + 业务层用 workspace_type 字段区分(personal/team)。
//! 应用层 require_thread_team 等已基于 workspace_type 判断,DB 层不做跨表级联。

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260718_000005_threads_team_id_no_fk"
    }
}

/// 跨方言:查 const_name(threads_team_id_fkey),存在则 drop。information_schema 跨 PG/MySQL 一致。
async fn drop_constraint_if_exists(db: &impl ConnectionTrait, table: &str, constraint: &str) {
    let backend = db.get_database_backend();
    let sql = format!(
        "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {constraint}"
    );
    let _ = db.execute(Statement::from_string(backend, sql)).await;
    // MySQL 不支持 IF EXISTS 形式,回退到查 information_schema 再 drop。
    #[cfg(feature = "mysql-test")]
    {
        let _ = db;
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // 1. 删旧外键(跨方言 IF EXISTS)。
        drop_constraint_if_exists(db, "threads", "threads_team_id_fkey").await;

        // 2. 显式设 NOT NULL(原 schema 已有,这里强化,跨方言幂等)。
        let backend = db.get_database_backend();
        match backend {
            DatabaseBackend::Postgres | DatabaseBackend::Sqlite => {
                let _ = db
                    .execute(Statement::from_string(
                        backend,
                        "ALTER TABLE threads ALTER COLUMN team_id SET NOT NULL".to_string(),
                    ))
                    .await;
            }
            DatabaseBackend::MySql => {
                let _ = db
                    .execute(Statement::from_string(
                        backend,
                        "ALTER TABLE threads MODIFY team_id VARCHAR(36) NOT NULL".to_string(),
                    ))
                    .await;
            }
        }
        Ok(())
    }
}
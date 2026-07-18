//! threads 表加 workspace_type 列:显式标识会话属于「个人 workspace」还是「团队 workspace」。
//!
//! 背景:此前仅靠 team_id 格式(team="团队uuid",个人="user:{userId}")隐式推导归属类型,
//! 不健壮。加显式列后,聚合列表/分组渲染/权限判断都直接读 workspace_type。
//!
//! 历史数据:DEFAULT 'team' 自动覆盖现有行(均为团队会话);team_id LIKE 'user:%' 的
//! 回填为 personal。跨方言用 information_schema 检查列是否存在(对齐 m0005)。

use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260718_000004_thread_workspace_type"
    }
}

/// 判断列是否存在(information_schema 跨 PG/MySQL 一致)。
async fn has_column(db: &impl ConnectionTrait, table: &str, col: &str) -> bool {
    let backend = db.get_database_backend();
    let sql = format!(
        "SELECT COUNT(*) AS c FROM information_schema.columns \
         WHERE table_name = '{table}' AND column_name = '{col}'"
    );
    db.query_one(Statement::from_string(backend, sql))
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
        if !has_column(db, "threads", "workspace_type").await {
            // NOT NULL DEFAULT 'team':现有行(均为团队会话)自动填 team。
            db.execute_unprepared(
                "ALTER TABLE threads ADD COLUMN workspace_type VARCHAR(8) NOT NULL DEFAULT 'team'",
            )
            .await?;
        }
        // 回填:team_id 以 'user:' 开头的标记为 personal(个人 workspace 历史数据)。
        db.execute_unprepared(
            "UPDATE threads SET workspace_type = 'personal' WHERE team_id LIKE 'user:%'",
        )
        .await?;
        Ok(())
    }
}

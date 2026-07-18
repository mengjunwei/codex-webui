//! SeaORM 多方言迁移(PG/MySQL)。替代旧的手动 sqlx 迁移(`multitenant/migration.rs`)+ drizzle。
//!
//! 放弃 `mt` schema(MySQL 无 schema 概念),所有表建在默认 schema(PG public / MySQL 默认库)。
//! 类型约定:VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT,不用 JSON/ENUM/ARRAY。
//! 建表用通用 raw SQL(PG/MySQL 均支持);索引按方言分支(PG `IF NOT EXISTS`,MySQL 普通 `CREATE INDEX`)。

pub use sea_orm_migration::prelude::*;
use sea_orm::DatabaseBackend;

mod m20260716_0001_initial;
mod m20260716_0002_api_keys;
mod m20260716_0003_audit;
mod m20260716_0004_business;
mod m20260716_0005_business_team_id;
mod m20260716_0006_quota;
mod m20260716_0007_team_routes;
mod m20260716_0008_session_replicas;
mod m20260718_000001_workspace_audit;
mod m20260718_000002_user_api_keys;
mod m20260718_000003_thread_resume_cache;
mod m20260718_000004_thread_workspace_type;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260716_0001_initial::Migration),
            Box::new(m20260716_0002_api_keys::Migration),
            Box::new(m20260716_0003_audit::Migration),
            Box::new(m20260716_0004_business::Migration),
            Box::new(m20260716_0005_business_team_id::Migration),
            Box::new(m20260716_0006_quota::Migration),
            Box::new(m20260716_0007_team_routes::Migration),
            Box::new(m20260716_0008_session_replicas::Migration),
            Box::new(m20260718_000001_workspace_audit::Migration),
            Box::new(m20260718_000002_user_api_keys::Migration),
            Box::new(m20260718_000003_thread_resume_cache::Migration),
            Box::new(m20260718_000004_thread_workspace_type::Migration),
        ]
    }
}

/// 多方言索引创建:PG/SQLite 用 `IF NOT EXISTS`;MySQL 不支持该语法,用普通 `CREATE INDEX`
/// (sea-orm-migration 保证每个迁移只执行一次,首次执行索引不存在,幂等)。
pub(crate) async fn create_index(
    manager: &SchemaManager<'_>,
    name: &str,
    table: &str,
    cols: &str,
) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DatabaseBackend::MySql => format!("CREATE INDEX {name} ON {table} ({cols})"),
        _ => format!("CREATE INDEX IF NOT EXISTS {name} ON {table} ({cols})"),
    };
    manager.get_connection().execute_unprepared(&sql).await?;
    Ok(())
}

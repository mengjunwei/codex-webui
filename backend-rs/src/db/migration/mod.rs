//! SeaORM 多方言迁移(PG/MySQL)。替代旧的手动 sqlx 迁移(`multitenant/migration.rs`)+ drizzle。
//!
//! 放弃 `mt` schema(MySQL 无 schema 概念),所有表建在默认 schema(PG public / MySQL 默认库)。
//! 类型约定:VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT,不用 JSON/ENUM/ARRAY。
//! 建表用通用 raw SQL(PG/MySQL 均支持);索引按方言分支(PG `IF NOT EXISTS`,MySQL 普通 `CREATE INDEX`)。

pub use sea_orm_migration::prelude::*;
use sea_orm::DatabaseBackend;

mod m20260719_000001_combined_schema;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260719_000001_combined_schema::Migration),
        ]
    }
}

/// 多方言索引创建:PostgreSQL 用 `IF NOT EXISTS`;MySQL 不支持该语法,用普通 `CREATE INDEX`
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

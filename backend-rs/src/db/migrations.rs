//! Drizzle SQL 迁移执行器。
//!
//! 在编译期通过 `include_str!` 内嵌 `drizzle/0000..0005.sql`。
//! 语句按 `--> statement-breakpoint` 标记进行拆分。
//! 已执行的文件记录在 `schema_migrations` 表中。
//!
//! **TS 管理的数据库兼容性**：如果存在 `__drizzle_migrations` 表
//! （由原始 NestJS drizzle-kit 迁移器创建），则假定 schema 已是最新并跳过执行 ——
//! 仅将全部 6 个文件记录为已执行，以便将来（若有）的增量迁移能正常工作。

use crate::db::Db;
use anyhow::Result;

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0000_init.sql",
        include_str!("../../drizzle/0000_init.sql"),
    ),
    (
        "0001_fresh_daredevil.sql",
        include_str!("../../drizzle/0001_fresh_daredevil.sql"),
    ),
    (
        "0002_certain_starbolt.sql",
        include_str!("../../drizzle/0002_certain_starbolt.sql"),
    ),
    (
        "0003_mature_chameleon.sql",
        include_str!("../../drizzle/0003_mature_chameleon.sql"),
    ),
    (
        "0004_lethal_rhodey.sql",
        include_str!("../../drizzle/0004_lethal_rhodey.sql"),
    ),
    (
        "0005_melted_mister_sinister.sql",
        include_str!("../../drizzle/0005_melted_mister_sinister.sql"),
    ),
];

const BREAKPOINT: &str = "--> statement-breakpoint";

/// 执行所有待处理的 drizzle 迁移。幂等操作；可在每次启动时安全调用。
pub fn run_migrations(db: &Db) -> Result<()> {
    let mut conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;

    // 确保我们的追踪表存在。
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            filename  TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    // 检测 TS 管理的数据库（drizzle-kit 自有的迁移表）。
    let drizzle_managed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='__drizzle_migrations'",
        [],
        |r| r.get(0),
    )?;

    if drizzle_managed > 0 {
        tracing::info!(
            "__drizzle_migrations present; assuming TS-managed DB, \
             skipping Rust migrations"
        );
        for (name, _) in MIGRATIONS {
            conn.execute(
                "INSERT OR IGNORE INTO schema_migrations(filename, applied_at) \
                 VALUES (?1, strftime('%s','now'))",
                [*name],
            )?;
        }
        return Ok(());
    }

    // 常规路径：按顺序执行每个文件，跳过已执行过的。
    for (name, sql) in MIGRATIONS {
        let applied: i64 = conn.query_row(
            "SELECT count(*) FROM schema_migrations WHERE filename = ?1",
            [*name],
            |r| r.get(0),
        )?;
        if applied > 0 {
            continue;
        }

        // 每个迁移文件用事务包裹以保证原子性：中途失败则整体回滚，
        // 避免"半应用 schema"在下次启动时触发 table already exists 死循环。
        let tx = conn.transaction()?;
        for stmt in sql.split(BREAKPOINT) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            tx.execute_batch(stmt)
                .map_err(|e| anyhow::anyhow!("migration {} failed: {}", name, e))?;
        }

        tx.execute(
            "INSERT INTO schema_migrations(filename, applied_at) \
             VALUES (?1, strftime('%s','now'))",
            [*name],
        )?;
        tx.commit()?;
        tracing::info!("applied migration {}", name);
    }

    Ok(())
}

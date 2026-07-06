//! Drizzle SQL migration runner.
//!
//! Embeds `drizzle/0000..0005.sql` at compile time (`include_str!`).
//! Statements are split on the `--> statement-breakpoint` marker.
//! Applied files are tracked in a `schema_migrations` table.
//!
//! **TS-managed DB compatibility**: If a `__drizzle_migrations` table exists
//! (created by the original NestJS drizzle-kit migrator), we assume the schema
//! is already up to date and skip execution — we only record all 6 files as
//! applied so that future incremental migrations (if any) work correctly.

use crate::db::Db;
use anyhow::Result;

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0000_init.sql",
        include_str!("../../../drizzle/0000_init.sql"),
    ),
    (
        "0001_fresh_daredevil.sql",
        include_str!("../../../drizzle/0001_fresh_daredevil.sql"),
    ),
    (
        "0002_certain_starbolt.sql",
        include_str!("../../../drizzle/0002_certain_starbolt.sql"),
    ),
    (
        "0003_mature_chameleon.sql",
        include_str!("../../../drizzle/0003_mature_chameleon.sql"),
    ),
    (
        "0004_lethal_rhodey.sql",
        include_str!("../../../drizzle/0004_lethal_rhodey.sql"),
    ),
    (
        "0005_melted_mister_sinister.sql",
        include_str!("../../../drizzle/0005_melted_mister_sinister.sql"),
    ),
];

const BREAKPOINT: &str = "--> statement-breakpoint";

/// Run all pending drizzle migrations. Idempotent; safe to call on every startup.
pub fn run_migrations(db: &Db) -> Result<()> {
    let conn = db.conn.lock().map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;

    // Ensure our tracking table exists.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            filename  TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    // Detect a TS-managed database (drizzle-kit's own migration table).
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

    // Normal path: apply each file in order, skipping already-applied ones.
    for (name, sql) in MIGRATIONS {
        let applied: i64 = conn.query_row(
            "SELECT count(*) FROM schema_migrations WHERE filename = ?1",
            [*name],
            |r| r.get(0),
        )?;
        if applied > 0 {
            continue;
        }

        for stmt in sql.split(BREAKPOINT) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            conn.execute_batch(stmt)
                .map_err(|e| anyhow::anyhow!("migration {} failed: {}", name, e))?;
        }

        conn.execute(
            "INSERT INTO schema_migrations(filename, applied_at) \
             VALUES (?1, strftime('%s','now'))",
            [*name],
        )?;
        tracing::info!("applied migration {}", name);
    }

    Ok(())
}

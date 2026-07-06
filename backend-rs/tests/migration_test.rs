//! Integration tests for the Drizzle SQL migration runner.
//!
//! Tests use `rusqlite::Connection::open_in_memory` so no filesystem is touched.
//! `Db` fields are `pub` so tests can construct it directly.

use codex_webui::db::{run_migrations, Db};
use rusqlite::Connection;

fn fresh() -> Db {
    let c = Connection::open_in_memory().unwrap();
    Db {
        conn: std::sync::Mutex::new(c),
    }
}

#[test]
fn creates_all_tables() {
    let db = fresh();
    run_migrations(&db).unwrap();
    let conn = db.conn.lock().unwrap();
    for table in [
        "token_usage_snapshots",
        "turn_diffs",
        "settings",
        "pending_server_requests",
        "turn_errors",
    ] {
        let n: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{}'",
                    table
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "table {} should exist", table);
    }
}

#[test]
fn idempotent_rerun() {
    let db = fresh();
    run_migrations(&db).unwrap();
    // Second call must not fail (CREATE TABLE IF NOT EXISTS is idempotent).
    run_migrations(&db).unwrap();
}

#[test]
fn skips_when_drizzle_managed() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE __drizzle_migrations(id integer);\
         CREATE TABLE settings(x);",
    )
    .unwrap();
    let db = Db {
        conn: std::sync::Mutex::new(c),
    };
    // Should detect the drizzle table, skip execution, and succeed.
    run_migrations(&db).unwrap();

    // All 6 filenames should be recorded in schema_migrations.
    let conn = db.conn.lock().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_migrations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 6, "expected 6 migration records, got {}", count);
}

//! SQLite connection wrapper.
//!
//! Parity with `src/database/database.service.ts`: WAL journal mode,
//! foreign_keys=ON, busy_timeout=5000. Uses a single connection (NestJS
//! better-sqlite3 is single-connection synchronous). `Mutex<Connection>`
//! mirrors that behavior; no connection pool needed for this application.
//!
//! Re-exports `run_migrations` from the migrations submodule.

pub mod migrations;
pub use migrations::run_migrations;

use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

pub struct Db {
    pub conn: Mutex<Connection>,
}

impl Db {
    /// Open (or create) the SQLite database at `path`, set pragmas,
    /// and ensure the parent directory exists.
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        tracing::info!("SQLite database ready at {}", path);
        Ok(Self { conn: Mutex::new(conn) })
    }
}

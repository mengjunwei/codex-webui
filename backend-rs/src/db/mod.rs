//! SQLite 连接封装。
//!
//! 与 `src/database/database.service.ts` 对齐：WAL 日志模式、
//! foreign_keys=ON、busy_timeout=5000。使用单连接（NestJS
//! better-sqlite3 也是单连接同步模型）。`Mutex<Connection>`
//! 与其行为一致；本应用无需连接池。
//!
//! 重新导出 migrations 子模块中的 `run_migrations`。

pub mod migrations;
pub use migrations::run_migrations;

use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

pub struct Db {
    pub conn: Mutex<Connection>,
}

impl Db {
    /// 打开（或创建）位于 `path` 的 SQLite 数据库，设置 pragma，
    /// 并确保父目录存在。
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

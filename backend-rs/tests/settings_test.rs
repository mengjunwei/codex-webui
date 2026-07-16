//! Integration tests for settings reconcile + reader.

use codex_webui::db::{run_migrations, Db};
use codex_webui::services::settings::{reconcile_settings, SettingsReader};
use rusqlite::Connection;
use std::sync::Mutex;

fn db() -> Db {
    let c = Connection::open_in_memory().unwrap();
    Db {
        conn: Mutex::new(c),
    }
}

#[test]
fn reconcile_inserts_defaults() {
    let db = db();
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();

    let r = SettingsReader::new(&db, None);
    // files.uploadMaxBytes default 100 MB, no env override set
    assert_eq!(r.get_number("files.uploadMaxBytes"), Some(104_857_600.0));
}

#[test]
fn db_override_wins() {
    let db = db();
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();

    // Simulate user override via Settings page.
    {
        let c = db.conn.lock().unwrap();
        c.execute(
            "UPDATE settings SET value='209715200', updated_at=strftime('%s','now') \
             WHERE key='files.uploadMaxBytes'",
            [],
        )
        .unwrap();
    }

    let r = SettingsReader::new(&db, None);
    assert_eq!(r.get_number("files.uploadMaxBytes"), Some(209_715_200.0));
}

#[test]
fn env_fallback_when_db_null() {
    // security.workspaceRoots has envKey WORKSPACE_ROOTS; DB value is NULL (not
    // written by reconcile). If env var is set, it should be used.
    // SAFETY: test serializes env access via the shared ENV_LOCK in config tests.
    unsafe { std::env::set_var("WORKSPACE_ROOTS", "/ws1,/ws2") };

    let db = db();
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();

    let r = SettingsReader::new(&db, None);
    assert_eq!(
        r.get_string("security.workspaceRoots"),
        Some("/ws1,/ws2".to_string())
    );

    unsafe { std::env::remove_var("WORKSPACE_ROOTS") };
}

#[test]
fn default_value_fallback() {
    // terminal.scrollback has envKey WEBUI_TERMINAL_SCROLLBACK.
    // With no DB value and no env var set, the default "5000" wins.
    unsafe { std::env::remove_var("WEBUI_TERMINAL_SCROLLBACK") };

    let db = db();
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();

    let r = SettingsReader::new(&db, None);
    assert_eq!(r.get_number("terminal.scrollback"), Some(5000.0));
}

#[test]
fn unknown_key_returns_none() {
    let db = db();
    run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();

    let r = SettingsReader::new(&db, None);
    assert!(r.get_string("nonexistent.key").is_none());
    assert!(r.get_number("nonexistent.key").is_none());
}

//! Runtime settings subsystem.
//!
//! `SettingsReader` resolves values through a 3-tier fallback chain:
//! 1. **DB value** (non-NULL, non-empty) — set by the user via the Settings page
//! 2. **envKey fallback** — the legacy environment variable from `SettingDef`
//! 3. **defaultValue** — the built-in default from `definitions.rs`
//!
//! Phase 0: read-only; full CRUD controller lands in Phase 2.

pub mod definitions;
pub mod reconcile;
pub use reconcile::reconcile_settings;

use crate::db::Db;
use definitions::{SettingDef, SETTINGS_DEFINITIONS};

/// Find the `SettingDef` for a given key. Returns `None` for unknown keys.
fn find_def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS_DEFINITIONS.iter().find(|d| d.key == key)
}

pub struct SettingsReader<'a> {
    db: &'a Db,
}

impl<'a> SettingsReader<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Resolve the raw string value for `key` through the fallback chain.
    fn raw_value(&self, key: &str) -> Option<String> {
        let def = find_def(key)?;

        let conn = self.db.conn.lock().ok()?;
        let db_val: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [key],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();

        // DB non-empty → envKey fallback → built-in default.
        db_val
            .filter(|s| !s.is_empty())
            .or_else(|| {
                def.env_key
                    .and_then(|ek| std::env::var(ek).ok())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| Some(def.default_value.to_string()))
    }

    /// Get a setting value as a raw string (or `None` if the key is unknown).
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.raw_value(key)
    }

    /// Get a setting value parsed as `f64` (or `None` on unknown key / parse failure).
    pub fn get_number(&self, key: &str) -> Option<f64> {
        let _ = find_def(key)?; // return None for unknown keys
        self.raw_value(key)?.parse::<f64>().ok()
    }

    /// Get a setting value parsed as `bool` (or `None` on unknown key / parse failure).
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        let _ = find_def(key)?;
        match self.raw_value(key)?.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" | "" => Some(false),
            _ => None,
        }
    }

    /// Convenience: `files.uploadMaxBytes` as `u64` with fallback 100 MB.
    pub fn get_upload_max_bytes(&self) -> u64 {
        self.get_number("files.uploadMaxBytes")
            .map(|n| n as u64)
            .unwrap_or(104_857_600)
    }
}

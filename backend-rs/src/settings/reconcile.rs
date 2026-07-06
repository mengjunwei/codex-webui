//! Startup reconcile: ensure every `SETTINGS_DEFINITIONS` entry exists in the
//! `settings` table. Existing rows are NOT overwritten (user values preserved);
//! only the metadata columns (`type`, `category`, `description`, `default_value`,
//! `constraints`, `updated_at`) are updated to the current definition.
//!
//! Parity with `src/settings/settings.service.ts` reconcile logic.

use crate::db::Db;
use crate::settings::definitions::SETTINGS_DEFINITIONS;
use anyhow::Result;

pub fn reconcile_settings(db: &Db) -> Result<()> {
    let conn = db
        .conn
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;

    for def in SETTINGS_DEFINITIONS {
        // INSERT OR IGNORE: preserves any existing value set by the user.
        conn.execute(
            "INSERT OR IGNORE INTO settings \
             (key, value, type, category, description, default_value, constraints, updated_at) \
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, '{}', strftime('%s','now'))",
            rusqlite::params![
                def.key,
                def.ty.as_str(),
                def.category.as_str(),
                def.description,
                def.default_value,
            ],
        )?;

        // UPDATE metadata — never touches `value` (user overrides are sacred).
        conn.execute(
            "UPDATE settings \
             SET type = ?1, category = ?2, description = ?3, \
                 default_value = ?4, updated_at = strftime('%s','now') \
             WHERE key = ?5",
            rusqlite::params![
                def.ty.as_str(),
                def.category.as_str(),
                def.description,
                def.default_value,
                def.key,
            ],
        )?;
    }

    tracing::info!(
        "reconciled {} settings definitions",
        SETTINGS_DEFINITIONS.len()
    );
    Ok(())
}

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
        let constraints_json = def.constraints.to_json();
        let constraints_str = serde_json::to_string(&constraints_json)
            .unwrap_or_else(|_| "{}".to_string());
        // INSERT OR IGNORE: preserves any existing value set by the user.
        conn.execute(
            "INSERT OR IGNORE INTO settings \
             (key, value, type, category, description, default_value, constraints, updated_at) \
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, (strftime('%s','now')*1000))",
            rusqlite::params![
                def.key,
                def.ty.as_str(),
                def.category.as_str(),
                def.description,
                def.default_value,
                constraints_str,
            ],
        )?;

        // UPDATE metadata + constraints — never touches `value` (user overrides are sacred)。
        // 仅当元数据实际变化时才 UPDATE（避免每次启动都刷新 updated_at，对齐 TS hasMetadataChanged）。
        let changed: bool = conn
            .query_row(
                "SELECT type, category, description, default_value, constraints FROM settings WHERE key = ?1",
                rusqlite::params![def.key],
                |r| {
                    let cur: (String, String, String, String, String) =
                        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
                    Ok(cur != (
                        def.ty.as_str().to_string(),
                        def.category.as_str().to_string(),
                        def.description.to_string(),
                        def.default_value.to_string(),
                        constraints_str.clone(),
                    ))
                },
            )
            .unwrap_or(true); // 行缺失或查询错误 → 保守执行 UPDATE。
        if changed {
            conn.execute(
                "UPDATE settings \
                 SET type = ?1, category = ?2, description = ?3, \
                     default_value = ?4, constraints = ?5, updated_at = (strftime('%s','now')*1000) \
                 WHERE key = ?6",
                rusqlite::params![
                    def.ty.as_str(),
                    def.category.as_str(),
                    def.description,
                    def.default_value,
                    constraints_str,
                    def.key,
                ],
            )?;
        }
    }

    tracing::info!(
        "reconciled {} settings definitions",
        SETTINGS_DEFINITIONS.len()
    );
    Ok(())
}

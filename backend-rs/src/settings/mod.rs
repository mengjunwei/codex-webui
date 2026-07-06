//! Runtime settings subsystem.
//!
//! `SettingsReader` resolves values through a 3-tier fallback chain:
//! 1. **DB value** (non-NULL, non-empty) — set by the user via the Settings page
//! 2. **envKey fallback** — the legacy environment variable from `SettingDef`
//! 3. **defaultValue** — the built-in default from `definitions.rs`
//!
//! Phase 2: CRUD handlers + source tracking. Write operations go directly to DB.

pub mod definitions;
pub mod handlers;
pub mod reconcile;
pub use reconcile::reconcile_settings;

use crate::db::Db;
use anyhow::Result;
use definitions::{SettingDef, SettingType, SETTINGS_DEFINITIONS};

// ── Value source tracking ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    Db,
    Env,
    Default,
}

/// A setting value resolved through the fallback chain, with source annotation.
pub struct ResolvedSetting {
    pub key: &'static str,
    pub raw_value: String,
    pub source: ValueSource,
    pub def: &'static SettingDef,
    pub updated_at: Option<i64>,
}

/// Find the `SettingDef` for a given key. Returns `None` for unknown keys.
pub fn find_def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS_DEFINITIONS.iter().find(|d| d.key == key)
}

// ── Reader ───────────────────────────────────────────────────────────────────

pub struct SettingsReader<'a> {
    db: &'a Db,
}

impl<'a> SettingsReader<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Resolve one setting with source tracking.
    pub fn resolve(&self, key: &str) -> Option<ResolvedSetting> {
        let def = find_def(key)?;
        let conn = self.db.conn.lock().ok()?;

        let (db_val, updated_at): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT value, updated_at FROM settings WHERE key = ?1",
                [key],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .unwrap_or((None, None));

        let (raw, source) = if let Some(v) = db_val.filter(|s| !s.is_empty()) {
            (v, ValueSource::Db)
        } else if let Some(v) = def
            .env_key
            .and_then(|ek| std::env::var(ek).ok())
            .filter(|s| !s.is_empty())
        {
            (v, ValueSource::Env)
        } else {
            (def.default_value.to_string(), ValueSource::Default)
        };

        Some(ResolvedSetting {
            key: def.key,
            raw_value: raw,
            source,
            def,
            updated_at,
        })
    }

    /// Resolve all settings, optionally filtered by category.
    pub fn list_all(&self, category: Option<&str>) -> Vec<ResolvedSetting> {
        SETTINGS_DEFINITIONS
            .iter()
            .filter(|d| category.map_or(true, |c| d.category.as_str() == c))
            .filter_map(|d| self.resolve(d.key))
            .collect()
    }

    /// Get a setting value as a raw string (or `None` if the key is unknown).
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.resolve(key).map(|r| r.raw_value)
    }

    /// Get a setting value parsed as `f64` (or `None` on unknown key / parse failure).
    pub fn get_number(&self, key: &str) -> Option<f64> {
        self.resolve(key)?.raw_value.parse::<f64>().ok()
    }

    /// Get a setting value parsed as `bool` (or `None` on unknown key / parse failure).
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.resolve(key)?.raw_value.to_ascii_lowercase().as_str() {
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

// ── Writer ───────────────────────────────────────────────────────────────────

/// Write (upsert) a setting value. `value` is `None` to reset to env/default.
pub fn write_setting(db: &Db, key: &str, value: Option<&str>) -> Result<()> {
    find_def(key).ok_or_else(|| anyhow::anyhow!("unknown setting: {}", key))?;
    let conn = db
        .conn
        .lock()
        .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
    conn.execute(
        "UPDATE settings SET value = ?1, updated_at = strftime('%s','now') WHERE key = ?2",
        rusqlite::params![value, key],
    )?;
    Ok(())
}

/// Validate a JSON value against the setting's declared type.
/// Returns the string representation for DB storage.
pub fn validate_and_serialize(
    def: &SettingDef,
    value: &serde_json::Value,
) -> Result<String, String> {
    match def.ty {
        SettingType::Number => {
            if let Some(n) = value.as_f64() {
                // Store as integer string if it's a whole number (common case).
                if n.fract() == 0.0 && n.is_finite() {
                    Ok(format!("{}", n as i64))
                } else {
                    Ok(n.to_string())
                }
            } else {
                Err(format!("expected number for {}", def.key))
            }
        }
        SettingType::String => {
            if let Some(s) = value.as_str() {
                Ok(s.to_string())
            } else if value.is_null() {
                Ok(String::new())
            } else {
                Err(format!("expected string for {}", def.key))
            }
        }
        SettingType::Boolean => {
            if let Some(b) = value.as_bool() {
                Ok(if b { "true" } else { "false" }.to_string())
            } else {
                Err(format!("expected boolean for {}", def.key))
            }
        }
        SettingType::Json => {
            // Accept any JSON value; store as serialized string.
            serde_json::to_string(value).map_err(|e| format!("invalid json: {e}"))
        }
    }
}

/// Convert a raw string value + setting type to a type-aware JSON value.
pub fn to_json_value(raw: &str, ty: SettingType) -> serde_json::Value {
    match ty {
        SettingType::Number => match raw.parse::<f64>() {
            // Whole numbers within i64 range → i64-backed Number (matches frontend integer expectations).
            Ok(n)
                if n.is_finite()
                    && n.fract() == 0.0
                    && n >= i64::MIN as f64
                    && n <= i64::MAX as f64 =>
            {
                serde_json::Value::Number(serde_json::Number::from(n as i64))
            }
            Ok(n) => serde_json::Number::from_f64(n)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::String(raw.to_string())),
            Err(_) => serde_json::Value::String(raw.to_string()),
        },
        SettingType::String => serde_json::Value::String(raw.to_string()),
        SettingType::Boolean => serde_json::Value::Bool(matches!(
            raw.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        SettingType::Json => {
            serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.to_string()))
        }
    }
}

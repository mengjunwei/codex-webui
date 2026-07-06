//! Runtime settings subsystem.
//!
//! Resolution produces a typed `serde_json::Value` through a 3-tier chain:
//! 1. **DB value** — JSON-decoded (parity with TS `decodeStoredValue = JSON.parse`).
//!    TS stores ALL values JSON-encoded (`encodeJson = JSON.stringify`); e.g. a
//!    string `"abc"` is stored as `"abc"` *with embedded quotes*. Only SQL NULL
//!    counts as missing (an empty string is a valid stored value).
//! 2. **envKey fallback** — type-parsed raw env string (parity with TS `readEnvValue`).
//! 3. **defaultValue** — type-interpreted from the raw definition string.
//!
//! Writes JSON-encode the value for storage. Constraints (min/max/integer) are
//! modeled, persisted by reconcile, returned in the DTO, and enforced on write.

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
    pub value: serde_json::Value,
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

        let (db_raw, updated_at): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT value, updated_at FROM settings WHERE key = ?1",
                [key],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .unwrap_or((None, None));

        // DB tier: JSON-decode; TS treats only NULL as missing (not empty string).
        if let Some(raw) = db_raw {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                return Some(ResolvedSetting {
                    key: def.key,
                    value,
                    source: ValueSource::Db,
                    def,
                    updated_at,
                });
            }
            // Corrupt stored value: TS warns + falls through to env/default.
            tracing::warn!("ignoring invalid stored setting {}: {}", def.key, raw);
        }

        // env tier: type-parse the raw env string.
        if let Some(raw) = def
            .env_key
            .and_then(|ek| std::env::var(ek).ok())
            .filter(|s| !s.is_empty())
        {
            if let Some(value) = parse_env_value(&raw, def.ty) {
                return Some(ResolvedSetting {
                    key: def.key,
                    value,
                    source: ValueSource::Env,
                    def,
                    updated_at,
                });
            }
        }

        // default tier: type-interpret the definition's default string.
        Some(ResolvedSetting {
            key: def.key,
            value: default_as_value(def),
            source: ValueSource::Default,
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

    pub fn get_string(&self, key: &str) -> Option<String> {
        self.resolve(key)?.value.as_str().map(|s| s.to_string())
    }

    pub fn get_number(&self, key: &str) -> Option<f64> {
        self.resolve(key)?.value.as_f64()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.resolve(key)?.value.as_bool()
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
        "UPDATE settings SET value = ?1, updated_at = (strftime('%s','now')*1000) WHERE key = ?2",
        rusqlite::params![value, key],
    )?;
    Ok(())
}

/// Validate a JSON value against the setting's declared type AND constraints,
/// then JSON-encode for storage. Parity with TS `validateValue` + `encodeJson`.
pub fn validate_and_serialize(
    def: &SettingDef,
    value: &serde_json::Value,
) -> Result<String, String> {
    let validated = validate_value(def, value)?;
    serde_json::to_string(&validated).map_err(|e| format!("encode error: {e}"))
}

/// Type + constraint validation. Returns the (possibly normalized) Value.
pub fn validate_value(
    def: &SettingDef,
    value: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    match def.ty {
        SettingType::Number => {
            let n = value
                .as_f64()
                .ok_or_else(|| format!("expected number for {}", def.key))?;
            if def.constraints.integer && n.fract() != 0.0 {
                return Err(format!("{} must be an integer", def.key));
            }
            if let Some(min) = def.constraints.min {
                if n < min {
                    return Err(format!("{} must be >= {}", def.key, min));
                }
            }
            if let Some(max) = def.constraints.max {
                if n > max {
                    return Err(format!("{} must be <= {}", def.key, max));
                }
            }
            Ok(num_value(n))
        }
        SettingType::String => match value {
            serde_json::Value::String(_) => Ok(value.clone()),
            // null → treat as empty string (parity with TS which rejects non-strings;
            // but PATCH null means "reset" handled at the handler layer).
            _ => Err(format!("expected string for {}", def.key)),
        },
        SettingType::Boolean => value
            .as_bool()
            .map(serde_json::Value::Bool)
            .ok_or_else(|| format!("expected boolean for {}", def.key)),
        SettingType::Json => Ok(value.clone()),
    }
}

/// Build a `serde_json::Number` Value, preferring i64 for whole numbers.
fn num_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.is_finite() {
        serde_json::Value::Number(serde_json::Number::from(n as i64))
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

/// Type-interpret the definition's raw default string into a Value.
pub fn default_as_value(def: &SettingDef) -> serde_json::Value {
    match def.ty {
        SettingType::Number => def.default_value.parse::<f64>().ok().map(num_value).unwrap_or(serde_json::Value::Null),
        SettingType::String => serde_json::Value::String(def.default_value.to_string()),
        SettingType::Boolean => serde_json::Value::Bool(matches!(def.default_value, "true" | "1")),
        SettingType::Json => serde_json::from_str(def.default_value).unwrap_or(serde_json::Value::Null),
    }
}

/// Type-parse a raw env string into a Value (parity with TS `readEnvValue`).
fn parse_env_value(raw: &str, ty: SettingType) -> Option<serde_json::Value> {
    match ty {
        SettingType::Number => raw.parse::<f64>().ok().map(num_value),
        SettingType::String => Some(serde_json::Value::String(raw.to_string())),
        SettingType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(serde_json::Value::Bool(true)),
            "0" | "false" | "no" | "off" => Some(serde_json::Value::Bool(false)),
            _ => None,
        },
        SettingType::Json => serde_json::from_str(raw).ok(),
    }
}

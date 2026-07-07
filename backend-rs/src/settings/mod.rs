//! 运行时设置子系统。
//!
//! 解析过程通过三层回退链产出一个有类型的 `serde_json::Value`：
//! 1. **DB 值** —— JSON 解码（与 TS 的 `decodeStoredValue = JSON.parse` 对齐）。
//!    TS 将所有值以 JSON 编码存储（`encodeJson = JSON.stringify`）；例如字符串
//!    `"abc"` 会存储为 `"abc"`（*内嵌引号*）。只有 SQL NULL 才算缺失
//!    （空字符串是合法的存储值）。
//! 2. **envKey 回退** —— 按类型解析原始环境变量字符串（与 TS 的 `readEnvValue` 对齐）。
//! 3. **defaultValue** —— 由定义中的原始字符串按类型解释得到。
//!
//! 写入时会对值进行 JSON 编码后存储。约束（min/max/integer）会被建模、由
//! reconcile 持久化、在 DTO 中返回，并在写入时强制校验。

pub mod definitions;
pub mod handlers;
pub mod reconcile;
pub use reconcile::reconcile_settings;

use crate::db::Db;
use anyhow::Result;
use definitions::{SettingDef, SettingType, SETTINGS_DEFINITIONS};

// ── 值来源追踪 ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    Db,
    Env,
    Default,
}

/// 经回退链解析出的设置值，附带来源标注。
pub struct ResolvedSetting {
    pub key: &'static str,
    pub value: serde_json::Value,
    pub source: ValueSource,
    pub def: &'static SettingDef,
    pub updated_at: Option<i64>,
}

/// 查找给定 key 对应的 `SettingDef`。未知 key 返回 `None`。
pub fn find_def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS_DEFINITIONS.iter().find(|d| d.key == key)
}

// ── 读取器 ───────────────────────────────────────────────────────────────────

pub struct SettingsReader<'a> {
    db: &'a Db,
}

impl<'a> SettingsReader<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 解析单个设置，并追踪其来源。
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

        // DB 层：JSON 解码；TS 仅把 NULL 视为缺失（空字符串不算）。
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
            // 存储值已损坏：TS 会告警并回退到 env/default。
            tracing::warn!("ignoring invalid stored setting {}: {}", def.key, raw);
        }

        // env 层：按类型解析原始环境变量字符串。
        if let Some(raw) = def
            .env_key
            .and_then(|ek| std::env::var(ek).ok())
            .filter(|s| !s.is_empty())
        {
            if let Some(value) = parse_env_value(&raw, def) {
                return Some(ResolvedSetting {
                    key: def.key,
                    value,
                    source: ValueSource::Env,
                    def,
                    updated_at,
                });
            }
        }

        // default 层：按类型解释定义中的默认值字符串。
        Some(ResolvedSetting {
            key: def.key,
            value: default_as_value(def),
            source: ValueSource::Default,
            def,
            updated_at,
        })
    }

    /// 解析所有设置，可按 category 过滤。
    pub fn list_all(&self, category: Option<&str>) -> Vec<ResolvedSetting> {
        SETTINGS_DEFINITIONS
            .iter()
            .filter(|d| category.map_or(true, |c| d.category.as_str() == c))
            .filter_map(|d| self.resolve(d.key))
            .collect()
    }

    pub fn get_string(&self, key: &str) -> Option<String> {
        // 将空字符串归一化为 None（与 TS 的 getStringSetting：`s.value || null` 对齐）。
        self.resolve(key)?.value.as_str().filter(|s| !s.is_empty()).map(|s| s.to_string())
    }

    pub fn get_number(&self, key: &str) -> Option<f64> {
        self.resolve(key)?.value.as_f64()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.resolve(key)?.value.as_bool()
    }

    /// 便捷方法：将 `files.uploadMaxBytes` 转为 `u64`，缺失时回退到 100 MB。
    pub fn get_upload_max_bytes(&self) -> u64 {
        self.get_number("files.uploadMaxBytes")
            .map(|n| n as u64)
            .unwrap_or(104_857_600)
    }
}

// ── 写入器 ───────────────────────────────────────────────────────────────────

/// 写入（upsert）一个设置值。`value` 为 `None` 时重置回 env/default。
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

/// 依据设置项声明的类型与约束校验一个 JSON 值，
/// 随后进行 JSON 编码以供存储。与 TS 的 `validateValue` + `encodeJson` 对齐。
pub fn validate_and_serialize(
    def: &SettingDef,
    value: &serde_json::Value,
) -> Result<String, String> {
    let validated = validate_value(def, value)?;
    serde_json::to_string(&validated).map_err(|e| format!("encode error: {e}"))
}

/// 类型与约束校验。返回（可能经过归一化的）Value。
pub fn validate_value(
    def: &SettingDef,
    value: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // 先按类型归一化（number 范围/整数校验在此完成）。
    let normalized = match def.ty {
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
            num_value(n)
        }
        SettingType::String => match value {
            serde_json::Value::String(_) => value.clone(),
            // null → 视作空字符串（与 TS 对齐，TS 会拒绝非字符串；
            // 但 PATCH 的 null 表示“重置”，由 handler 层处理）。
            _ => return Err(format!("expected string for {}", def.key)),
        },
        SettingType::Boolean => value
            .as_bool()
            .map(serde_json::Value::Bool)
            .ok_or_else(|| format!("expected boolean for {}", def.key))?,
        // JSON 类型：递归校验合法性（对齐 TS isJsonValue + validateValue 的 json 分支）。
        // 拒绝 null；对象键名禁止 __proto__/constructor/prototype（防原型污染）。
        SettingType::Json => {
            if !is_valid_json_value(value) {
                return Err(format!("{} must be JSON", def.key));
            }
            value.clone()
        }
    };

    // enum 约束校验（对齐 TS constraints.enum）：归一化后的值必须命中其一。
    if let Some(enum_values) = &def.constraints.enum_values {
        if !enum_values.iter().any(|c| c == &normalized) {
            return Err(format!("{} is not an allowed value", def.key));
        }
    }

    Ok(normalized)
}

/// 递归校验 JSON 值的合法性（对齐 TS settings.service.ts:isJsonValue，
/// 并增加原型污染键名防护）。
/// - null 视为非法（validateValue 的 json 分支要求 value !== null）
/// - serde_json::Number 不含 NaN/Infinity，等价于 TS Number.isFinite 校验
/// - 对象键名禁止 __proto__/constructor/prototype
fn is_valid_json_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => true,
        serde_json::Value::Array(arr) => arr.iter().all(is_valid_json_value),
        serde_json::Value::Object(obj) => obj.iter().all(|(k, v)| {
            k != "__proto__" && k != "constructor" && k != "prototype" && is_valid_json_value(v)
        }),
    }
}

/// 构造一个 `serde_json::Number` Value，整数优先使用 i64。
fn num_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.is_finite() {
        serde_json::Value::Number(serde_json::Number::from(n as i64))
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

/// 将定义中的原始默认值字符串按类型解释为 Value。
pub fn default_as_value(def: &SettingDef) -> serde_json::Value {
    match def.ty {
        SettingType::Number => def.default_value.parse::<f64>().ok().map(num_value).unwrap_or(serde_json::Value::Null),
        SettingType::String => serde_json::Value::String(def.default_value.to_string()),
        SettingType::Boolean => serde_json::Value::Bool(matches!(def.default_value, "true" | "1")),
        SettingType::Json => serde_json::from_str(def.default_value).unwrap_or(serde_json::Value::Null),
    }
}

/// 将原始环境变量字符串按类型解析为 Value（与 TS 的 `readEnvValue` 对齐）。
/// 数值会按 constraints 截断（integer）并夹到 [min,max]（对齐 TS clampNumber）。
fn parse_env_value(raw: &str, def: &SettingDef) -> Option<serde_json::Value> {
    match def.ty {
        SettingType::Number => {
            let mut n = raw.parse::<f64>().ok()?;
            if def.constraints.integer {
                n = n.trunc();
            }
            if let Some(min) = def.constraints.min {
                n = n.max(min);
            }
            if let Some(max) = def.constraints.max {
                n = n.min(max);
            }
            Some(num_value(n))
        }
        SettingType::String => Some(serde_json::Value::String(raw.to_string())),
        SettingType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" => Some(serde_json::Value::Bool(true)),
            "0" | "false" => Some(serde_json::Value::Bool(false)),
            _ => None,
        },
        SettingType::Json => serde_json::from_str(raw).ok(),
    }
}

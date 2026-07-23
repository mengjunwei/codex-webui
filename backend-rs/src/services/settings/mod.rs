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
//!
//! 数据层:SeaORM(多方言 PG/MySQL),`settings` 表 entity `crate::db::entity::setting`
//! (DB 列 `setting_key`,避免 MySQL 保留字 `key`)。

pub mod definitions;
pub mod reconcile;
pub use reconcile::reconcile_settings;

use crate::db::entity::setting::{ActiveModel as SettingActiveModel, Column as SettingColumn, Entity as SettingEntity, Model as SettingModel};
use crate::services::multitenant::now_ms;
use crate::state::SettingsCache;
use anyhow::Result;
use definitions::{SettingDef, SettingType, SETTINGS_DEFINITIONS};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

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

impl ResolvedSetting {
    /// 透出声明的 description(SettingDef 没有 pub description)。
    pub fn description(&self) -> &'static str {
        self.def.description
    }
}

/// 查找给定 key 对应的 `SettingDef`。未知 key 返回 `None`。
pub fn find_def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS_DEFINITIONS.iter().find(|d| d.key == key)
}

// ── 读取器 ───────────────────────────────────────────────────────────────────

pub struct SettingsReader<'a> {
    db: &'a DatabaseConnection,
    cache: Option<&'a SettingsCache>,
}

impl<'a> SettingsReader<'a> {
    pub fn new(db: &'a DatabaseConnection, cache: Option<&'a SettingsCache>) -> Self {
        Self { db, cache }
    }

    /// 解析单个设置，并追踪其来源。优先从内存缓存读取（对齐 TS SettingsService.cache）。
    pub async fn resolve(&self, key: &str) -> Option<ResolvedSetting> {
        let def = find_def(key)?;

        // 缓存命中：直接返回（std::sync::Mutex 在 async 内仅做同步短操作，不跨 await）。
        if let Some(cache_ref) = self.cache {
            if let Ok(cache) = cache_ref.lock() {
                if let Some((value, source, updated_at)) = cache.get(key) {
                    return Some(ResolvedSetting {
                        key: def.key,
                        value: value.clone(),
                        source: *source,
                        def,
                        updated_at: *updated_at,
                    });
                }
            }
        }

        // DB 层：SeaORM 按主键 setting_key 查询。错误记 warn 后回退到 env/default。
        let row: Option<SettingModel> = SettingEntity::find_by_id(key)
            .one(self.db)
            .await
            .map_err(|e| {
                tracing::warn!("settings db read error for {}: {}", key, e);
                e
            })
            .ok()
            .flatten();
        let (db_raw, updated_at) = match row {
            Some(m) => (m.value, Some(m.updated_at)),
            None => (None, None),
        };

        // DB 层：JSON 解码；TS 仅把 NULL 视为缺失（空字符串不算）。
        if let Some(raw) = db_raw {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                let result = ResolvedSetting {
                    key: def.key,
                    value: value.clone(),
                    source: ValueSource::Db,
                    def,
                    updated_at,
                };
                if let Some(cache_ref) = self.cache {
                    if let Ok(mut cache) = cache_ref.lock() {
                        cache.insert(key.to_string(), (value, ValueSource::Db, updated_at));
                    }
                }
                return Some(result);
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
                if let Some(cache_ref) = self.cache {
                    if let Ok(mut cache) = cache_ref.lock() {
                        cache.insert(key.to_string(), (value.clone(), ValueSource::Env, updated_at));
                    }
                }
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
        let value = default_as_value(def);
        if let Some(cache_ref) = self.cache {
            if let Ok(mut cache) = cache_ref.lock() {
                cache.insert(key.to_string(), (value.clone(), ValueSource::Default, updated_at));
            }
        }
        Some(ResolvedSetting {
            key: def.key,
            value,
            source: ValueSource::Default,
            def,
            updated_at,
        })
    }

    /// 解析所有设置，可按 category 过滤。
    pub async fn list_all(&self, category: Option<&str>) -> Vec<ResolvedSetting> {
        let mut out = Vec::new();
        for d in SETTINGS_DEFINITIONS
            .iter()
            .filter(|d| category.map_or(true, |c| d.category.as_str() == c))
        {
            if let Some(r) = self.resolve(d.key).await {
                out.push(r);
            }
        }
        out
    }

    pub async fn get_string(&self, key: &str) -> Option<String> {
        self.resolve(key)
            .await?
            .value
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    pub async fn get_number(&self, key: &str) -> Option<f64> {
        self.resolve(key).await?.value.as_f64()
    }

    pub async fn get_bool(&self, key: &str) -> Option<bool> {
        self.resolve(key).await?.value.as_bool()
    }

    /// 便捷方法：将 `files.uploadMaxBytes` 转为 `u64`，缺失时回退到 100 MB。
    pub async fn get_upload_max_bytes(&self) -> u64 {
        self.get_number("files.uploadMaxBytes")
            .await
            .map(|n| n as u64)
            .unwrap_or(104_857_600)
    }
}

// ── 写入器 ───────────────────────────────────────────────────────────────────

/// 写入（upsert）一个设置值。`value` 为 `None` 时重置回 env/default。
///
/// 通过 `update_many` + 行数检查实现"行存在才更新",0 行则报错（行缺失 = reconcile 未跑），
/// 行存在则更新 value + updated_at。不在 DB 层做 upsert `ON CONFLICT`,避免跨方言差异；
/// `reconcile_settings` 启动时已为每个定义建行,这里只允许改 value。
pub async fn write_setting(db: &DatabaseConnection, key: &str, value: Option<&str>) -> Result<()> {
    find_def(key).ok_or_else(|| anyhow::anyhow!("unknown setting: {}", key))?;
    let res = SettingEntity::update_many()
        .col_expr(SettingColumn::Value, Expr::value(value))
        .col_expr(SettingColumn::UpdatedAt, Expr::value(now_ms()))
        .filter(SettingColumn::Key.eq(key.to_string()))
        .exec(db)
        .await
        .map_err(|e| anyhow::anyhow!("db: {e}"))?;
    if res.rows_affected == 0 {
        return Err(anyhow::anyhow!(
            "setting row not found for key '{key}' (was reconcile_settings run?)"
        ));
    }
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
            _ => return Err(format!("expected string for {}", def.key)),
        },
        SettingType::Boolean => value
            .as_bool()
            .map(serde_json::Value::Bool)
            .ok_or_else(|| format!("expected boolean for {}", def.key))?,
        SettingType::Json => {
            if !is_valid_json_value(value) {
                return Err(format!("{} must be JSON", def.key));
            }
            value.clone()
        }
    };

    if let Some(enum_values) = &def.constraints.enum_values {
        if !enum_values.iter().any(|c| c == &normalized) {
            return Err(format!("{} is not an allowed value", def.key));
        }
    }

    Ok(normalized)
}

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

fn num_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.is_finite() {
        serde_json::Value::Number(serde_json::Number::from(n as i64))
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

pub fn default_as_value(def: &SettingDef) -> serde_json::Value {
    match def.ty {
        SettingType::Number => def.default_value.parse::<f64>().ok().map(num_value).unwrap_or(serde_json::Value::Null),
        SettingType::String => serde_json::Value::String(def.default_value.to_string()),
        SettingType::Boolean => serde_json::Value::Bool(matches!(def.default_value, "true" | "1")),
        SettingType::Json => serde_json::from_str(def.default_value).unwrap_or(serde_json::Value::Null),
    }
}

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

// 抑制 unused 警告(ActiveModel trait 在 future 扩展用到)。
#[allow(unused_imports)]
use SettingActiveModel as _;

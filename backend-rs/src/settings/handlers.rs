//! 设置 CRUD 的 HTTP handler。
//!
//! 路由（挂载于 `/api/settings`）：
//! - GET    /                —— 列出所有设置（可选 `?category=`）
//! - GET    /:key            —— 获取单个设置
//! - PATCH  /                —— 批量更新 `{ updates: [{ key, value }] }`
//! - PATCH  /:key            —— 更新单个 `{ value }`（null 表示重置）
//! - DELETE /:key            —— 重置回 env/default

use crate::db::Db;
use crate::error::{AppError, ErrorCode};
use crate::settings::{
    default_as_value, find_def, validate_and_serialize, write_setting, ResolvedSetting,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

// ── DTO ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SettingDto {
    pub key: String,
    pub value: serde_json::Value,
    pub source: &'static str,
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    #[serde(rename = "defaultValue")]
    pub default_value: serde_json::Value,
    pub constraints: serde_json::Value,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<i64>,
}

impl SettingDto {
    fn from_resolved(r: &ResolvedSetting, constraints: serde_json::Value) -> Self {
        Self {
            key: r.key.to_string(),
            value: r.value.clone(),
            source: match r.source {
                crate::settings::ValueSource::Db => "db",
                crate::settings::ValueSource::Env => "env",
                crate::settings::ValueSource::Default => "default",
            },
            ty: r.def.ty.as_str(),
            category: r.def.category.as_str(),
            description: r.def.description,
            default_value: default_as_value(r.def),
            constraints,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Deserialize)]
pub struct UpdatePayload {
    pub value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct BatchUpdatePayload {
    pub updates: Vec<UpdateEntry>,
}

#[derive(Deserialize)]
pub struct UpdateEntry {
    pub key: String,
    pub value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub category: Option<String>,
}

// ── Handler ─────────────────────────────────────────────────────────────────

pub async fn list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    // M5 修复：若提供 category 则进行校验。
    if let Some(ref cat) = q.category {
        if !matches!(cat.as_str(), "terminal" | "files" | "security" | "general") {
            return Err(AppError::business(
                ErrorCode::SettingsInvalidCategory,
                StatusCode::BAD_REQUEST,
                format!("Invalid category: {cat}"),
                None,
            ));
        }
    }
    let reader = crate::settings::SettingsReader::new(&state.db);
    let resolved = reader.list_all(q.category.as_deref());
    let dtos: Vec<SettingDto> = resolved
        .iter()
        .map(|r| SettingDto::from_resolved(r, constraints_for_key(&state.db, r.key)))
        .collect();
    Ok(Json(serde_json::json!({ "settings": dtos })))
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<SettingDto>, AppError> {
    if find_def(&key).is_none() {
        return Err(AppError::business(
            ErrorCode::SettingsNotFound,
            StatusCode::NOT_FOUND,
            format!("Unknown setting key: {key}"),
            None,
        ));
    }
    let reader = crate::settings::SettingsReader::new(&state.db);
    let r = reader.resolve(&key).ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "Setting not found".into(),
            None,
        )
    })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key),
    )))
}

pub async fn update_batch(
    State(state): State<AppState>,
    Json(payload): Json<BatchUpdatePayload>,
) -> Result<Json<serde_json::Value>, AppError> {
    // M7 修复：检测重复 key（TS 会抛出 settings.duplicate_key）。
    let mut seen = std::collections::HashSet::new();
    for entry in &payload.updates {
        if !seen.insert(&entry.key) {
            return Err(AppError::business(
                ErrorCode::SettingsDuplicateKey,
                StatusCode::BAD_REQUEST,
                format!("Duplicate setting key: {}", entry.key),
                None,
            ));
        }
    }

    // 先校验所有条目。
    let mut prepared: Vec<(String, Option<String>)> = Vec::with_capacity(payload.updates.len());
    for entry in &payload.updates {
        let def = find_def(&entry.key).ok_or_else(|| {
            AppError::business(
                ErrorCode::SettingsNotFound,
                StatusCode::NOT_FOUND,
                format!("Unknown setting key: {}", entry.key),
                None,
            )
        })?;
        let serialized = match &entry.value {
            Some(v) => Some(validate_and_serialize(def, v).map_err(|msg| {
                AppError::business(
                    ErrorCode::SettingsInvalidValue,
                    StatusCode::BAD_REQUEST,
                    msg,
                    None,
                )
            })?),
            None => None, // 重置回 env/default
        };
        prepared.push((entry.key.clone(), serialized));
    }

    // 持久化（用作用域隔离，以便在下方回读之前释放连接锁，
    // 因为回读会再次调用 resolve() → conn.lock() —— 否则会死锁）。
    {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|e| AppError::internal(format!("db lock poisoned: {e}")))?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| AppError::internal(format!("tx begin: {e}")))?;
        for (key, value) in &prepared {
            tx.execute(
                "UPDATE settings SET value = ?1, updated_at = (strftime('%s','now')*1000) WHERE key = ?2",
                rusqlite::params![value.as_deref(), key],
            )
            .map_err(|e| AppError::internal(format!("update {key}: {e}")))?;
        }
        tx.commit()
            .map_err(|e| AppError::internal(format!("tx commit: {e}")))?;
    }

    // 返回更新后的设置（锁会在 resolve 内部安全地重新获取）。
    let dtos: Vec<SettingDto> = prepared
        .iter()
        .filter_map(|(k, _)| {
            let r = resolve(&state.db, k)?;
            Some(SettingDto::from_resolved(&r, constraints_for_key(&state.db, k)))
        })
        .collect();
    Ok(Json(serde_json::json!({ "settings": dtos })))
}

pub async fn update_one(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<SettingDto>, AppError> {
    let def = find_def(&key).ok_or_else(|| {
        AppError::business(
            ErrorCode::SettingsNotFound,
            StatusCode::NOT_FOUND,
            format!("Unknown setting key: {key}"),
            None,
        )
    })?;
    // H3 修复：区分"value 字段缺失"（→ 400）和"value 为 null"（→ 重置）。
    // TS settings.controller.ts:83 使用 hasOwnProperty('value') 进行区分。
    let has_value = body.get("value").is_some();
    if !has_value {
        let mut params = std::collections::BTreeMap::new();
        params.insert("field".to_string(), serde_json::Value::String("value".into()));
        return Err(AppError::business(
            ErrorCode::ValidationFieldRequired,
            StatusCode::BAD_REQUEST,
            "value is required".into(),
            Some(params),
        ));
    }
    let value = &body["value"];
    let serialized = if value.is_null() {
        None // 显式 null → 重置为 env/default
    } else {
        Some(validate_and_serialize(def, value).map_err(|msg| {
            AppError::business(
                ErrorCode::SettingsInvalidValue,
                StatusCode::BAD_REQUEST,
                msg,
                None,
            )
        })?)
    };
    write_setting(&state.db, &key, serialized.as_deref())
        .map_err(|e| AppError::internal(format!("write {key}: {e}")))?;

    let r = crate::settings::SettingsReader::new(&state.db)
        .resolve(&key)
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::HttpNotFound,
                StatusCode::NOT_FOUND,
                "Setting not found".into(),
                None,
            )
        })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key),
    )))
}

pub async fn delete_one(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<SettingDto>, AppError> {
    if find_def(&key).is_none() {
        return Err(AppError::business(
            ErrorCode::SettingsNotFound,
            StatusCode::NOT_FOUND,
            format!("Unknown setting key: {key}"),
            None,
        ));
    }
    write_setting(&state.db, &key, None)
        .map_err(|e| AppError::internal(format!("delete {key}: {e}")))?;

    let r = resolve(&state.db, &key).ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "Setting not found".into(),
            None,
        )
    })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key),
    )))
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 从 `settings` 行加载 constraints JSON，缺失时默认为 `{}`。
fn constraints_for_key(db: &Db, key: &str) -> serde_json::Value {
    let conn = match db.conn.lock() {
        Ok(c) => c,
        Err(_) => return serde_json::json!({}),
    };
    conn.query_row(
        "SELECT constraints FROM settings WHERE key = ?1",
        [key],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or(serde_json::json!({}))
}

/// 供 handler 作用域使用的独立函数 `resolve`。
fn resolve(db: &Db, key: &str) -> Option<ResolvedSetting> {
    crate::settings::SettingsReader::new(db).resolve(key)
}
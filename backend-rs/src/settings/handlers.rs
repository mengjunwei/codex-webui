//! Settings CRUD HTTP handlers.
//!
//! Routes (mounted at `/api/settings`):
//! - GET    /                — list all settings (optional `?category=`)
//! - GET    /:key            — get one setting
//! - PATCH  /                — batch update `{ updates: [{ key, value }] }`
//! - PATCH  /:key            — update one `{ value }` (null resets)
//! - DELETE /:key            — reset to env/default

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

// ── DTOs ─────────────────────────────────────────────────────────────────────

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

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    // M5 FIX: validate category if provided.
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
    // M7 FIX: detect duplicate keys (TS throws settings.duplicate_key).
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

    // Validate all entries first.
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
            None => None, // reset to env/default
        };
        prepared.push((entry.key.clone(), serialized));
    }

    // Persist (scoped so the connection lock is released before the read-back below,
    // which calls resolve() → conn.lock() again — otherwise deadlock).
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

    // Return updated settings (lock re-acquired safely inside resolve).
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
    Json(payload): Json<UpdatePayload>,
) -> Result<Json<SettingDto>, AppError> {
    let def = find_def(&key).ok_or_else(|| {
        AppError::business(
            ErrorCode::SettingsNotFound,
            StatusCode::NOT_FOUND,
            format!("Unknown setting key: {key}"),
            None,
        )
    })?;
    let serialized = match &payload.value {
        Some(v) => Some(validate_and_serialize(def, v).map_err(|msg| {
            AppError::business(
                ErrorCode::SettingsInvalidValue,
                StatusCode::BAD_REQUEST,
                msg,
                None,
            )
        })?),
        None => None,
    };
    write_setting(&state.db, &key, serialized.as_deref())
        .map_err(|e| AppError::internal(format!("write {key}: {e}")))?;

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

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Load constraints JSON from the `settings` row, defaulting to `{}`.
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

/// Free-function `resolve` for use in handler scope.
fn resolve(db: &Db, key: &str) -> Option<ResolvedSetting> {
    crate::settings::SettingsReader::new(db).resolve(key)
}
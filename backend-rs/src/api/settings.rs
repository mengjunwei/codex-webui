//! 设置 CRUD 的 HTTP handler(Settings 全部 async,SeaORM)。

use sea_orm::DatabaseConnection;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};

use crate::db::entity::setting::{Column as SettingColumn, Entity as SettingEntity};
use crate::error::{AppError, ErrorCode, Json};
use crate::services::settings::{
    default_as_value, find_def, validate_and_serialize, write_setting, SettingsReader,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

// ── DTO ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
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
    fn from_resolved(r: &crate::services::settings::ResolvedSetting, constraints: serde_json::Value) -> Self {
        Self {
            key: r.key.to_string(),
            value: r.value.clone(),
            source: match r.source {
                crate::services::settings::ValueSource::Db => "db",
                crate::services::settings::ValueSource::Env => "env",
                crate::services::settings::ValueSource::Default => "default",
            },
            ty: r.def.ty.as_str(),
            category: r.def.category.as_str(),
            description: r.description(),
            default_value: default_as_value(r.def),
            constraints,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdatePayload {
    pub value: Option<serde_json::Value>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub category: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SettingListResponse {
    pub settings: Vec<SettingDto>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SettingBatchEntry {
    pub key: String,
    pub value: Option<serde_json::Value>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SettingBatchUpdateBody {
    pub updates: Vec<SettingBatchEntry>,
}

// ── Handler ─────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/settings",
    tag = "settings",
    params(ListQuery),
    responses(
        (status = 200, description = "所有设置（可按 category 过滤）", body = SettingListResponse),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
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
    let reader = state.settings_reader();
    let resolved = reader.list_all(q.category.as_deref()).await;
    let mut dtos: Vec<SettingDto> = Vec::with_capacity(resolved.len());
    for r in &resolved {
        dtos.push(SettingDto::from_resolved(r, constraints_for_key(&state.db, r.key).await));
    }
    Ok(Json(serde_json::json!({ "settings": dtos })))
}

#[utoipa::path(
    get,
    path = "/api/settings/{key}",
    tag = "settings",
    params(("key" = String, Path, description = "设置项 key")),
    responses(
        (status = 200, description = "单个设置", body = SettingDto),
    )
)]
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
    let reader = state.settings_reader();
    let r = reader.resolve(&key).await.ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "Setting not found".into(),
            None,
        )
    })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key).await,
    )))
}

#[utoipa::path(
    patch,
    path = "/api/settings",
    tag = "settings",
    request_body = SettingBatchUpdateBody,
    responses((status = 200, description = "更新后的设置列表", body = SettingListResponse))
)]
pub async fn update_batch(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    let updates = body
        .get("updates")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::SettingsUpdatesRequired,
                StatusCode::BAD_REQUEST,
                "updates is required and must be an array".into(),
                None,
            )
        })?;
    let mut parsed: Vec<(String, Option<serde_json::Value>)> = Vec::with_capacity(updates.len());
    for (idx, entry) in updates.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            AppError::business(
                ErrorCode::SettingsKeyRequired,
                StatusCode::BAD_REQUEST,
                format!("updates[{idx}] must be an object"),
                None,
            )
        })?;
        let key = obj
            .get("key")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::business(
                    ErrorCode::SettingsKeyRequired,
                    StatusCode::BAD_REQUEST,
                    format!("updates[{idx}].key is required"),
                    None,
                )
            })?;
        parsed.push((key.to_string(), obj.get("value").cloned()));
    }

    let mut seen = std::collections::HashSet::new();
    for (key, _) in &parsed {
        if !seen.insert(key.clone()) {
            return Err(AppError::business(
                ErrorCode::SettingsDuplicateKey,
                StatusCode::BAD_REQUEST,
                format!("Duplicate setting key: {key}"),
                None,
            ));
        }
    }

    let mut prepared: Vec<(String, Option<String>)> = Vec::with_capacity(parsed.len());
    for (key, value) in &parsed {
        let def = find_def(key).ok_or_else(|| {
            AppError::business(
                ErrorCode::SettingsNotFound,
                StatusCode::NOT_FOUND,
                format!("Unknown setting key: {key}"),
                None,
            )
        })?;
        let serialized = match value {
            Some(v) if !v.is_null() => Some(validate_and_serialize(def, v).map_err(|msg| {
                AppError::business(
                    ErrorCode::SettingsInvalidValue,
                    StatusCode::BAD_REQUEST,
                    msg,
                    None,
                )
            })?),
            _ => None,
        };
        prepared.push((key.clone(), serialized));
    }

    // 事务:逐条 update_many .exec(txn)(rows_affected==0 判行缺失,报错对齐原行为)。
    state
        .db
        .transaction::<_, _, AppError>(|txn| {
            let prepared = prepared.clone();
            Box::pin(async move {
                for (key, value) in &prepared {
                    let res = SettingEntity::update_many()
                        .col_expr(SettingColumn::Value, Expr::value(value.clone()))
                        .col_expr(
                            SettingColumn::UpdatedAt,
                            Expr::value(crate::services::multitenant::now_ms()),
                        )
                        .filter(SettingColumn::Key.eq(key.to_string()))
                        .exec(txn)
                        .await
                        .map_err(|e| AppError::internal(format!("update {key}: {e}")))?;
                    if res.rows_affected == 0 {
                        return Err(AppError::business(
                            ErrorCode::SettingsNotFound,
                            StatusCode::NOT_FOUND,
                            format!("setting row not found for key '{key}' (was reconcile run?)"),
                            None,
                        ));
                    }
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| AppError::internal(format!("batch tx: {e}")))?;

    state.invalidate_settings_cache();

    // 闭包异步重构:先 prepare 列表,再 for await 逐条 resolve。
    let reader = state.settings_reader();
    let mut dtos = Vec::with_capacity(prepared.len());
    for (k, _) in &prepared {
        if let Some(r) = reader.resolve(k).await {
            dtos.push(SettingDto::from_resolved(
                &r,
                constraints_for_key(&state.db, k).await,
            ));
        }
    }
    Ok(Json(serde_json::json!({ "settings": dtos })))
}

#[utoipa::path(
    patch,
    path = "/api/settings/{key}",
    tag = "settings",
    params(("key" = String, Path, description = "设置项 key")),
    request_body = UpdatePayload,
    responses((status = 200, description = "更新后的设置", body = SettingDto))
)]
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
        None
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
        .await
        .map_err(|e| AppError::internal(format!("write {key}: {e}")))?;
    state.invalidate_settings_cache();

    let reader = state.settings_reader();
    let r = reader.resolve(&key).await.ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "Setting not found".into(),
            None,
        )
    })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key).await,
    )))
}

#[utoipa::path(
    delete,
    path = "/api/settings/{key}",
    tag = "settings",
    params(("key" = String, Path, description = "设置项 key")),
    responses((status = 200, description = "重置后的设置", body = SettingDto))
)]
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
        .await
        .map_err(|e| AppError::internal(format!("delete {key}: {e}")))?;
    state.invalidate_settings_cache();

    let reader = state.settings_reader();
    let r = reader.resolve(&key).await.ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpNotFound,
            StatusCode::NOT_FOUND,
            "Setting not found".into(),
            None,
        )
    })?;
    Ok(Json(SettingDto::from_resolved(
        &r,
        constraints_for_key(&state.db, &key).await,
    )))
}

/// 从 `settings` 行加载 constraints JSON,缺失时默认为 `{}`。
pub(crate) async fn constraints_for_key(db: &DatabaseConnection, key: &str) -> serde_json::Value {
    if let Ok(Some(model)) = SettingEntity::find_by_id(key.to_string()).one(db).await {
        if let Ok(c) = serde_json::from_str(&model.constraints) {
            return c;
        }
    }
    serde_json::json!({})
}

// 抑制 SettingsReader 未使用警告(命名空间导入备用)。
#[allow(unused_imports)]
use SettingsReader as _;

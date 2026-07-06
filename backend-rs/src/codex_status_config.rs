//! Codex status + config REST endpoints.
//!
//! Parity with `codex-status.controller.ts` + `codex-config.controller.ts`.
//!
//! - GET  /codex/status          — aggregated readiness (simplified: generation/
//!   connected/initialized; full account/config/provider/model probe aggregation
//!   deferred). Drives the UI's "codex ready" indicator.
//! - POST /codex/approval-policy — config/batchWrite approval_policy (validated)
//! - POST /codex/sandbox-mode    — config/batchWrite sandbox_mode (validated)
//! - GET  /codex/config          — config/read (includeLayers), secrets redacted
//! - PATCH /codex/config         — curated edits (allowlist), config/batchWrite
//! - GET  /codex/config/raw      — read user config.toml
//! - PUT  /codex/config/raw      — write user config.toml + hot-reload

use crate::codex::RpcError;
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

fn map_rpc(e: RpcError) -> AppError {
    AppError::internal(format!("codex: {e}"))
}
fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

const APPROVAL_POLICY_VALUES: &[&str] = &["untrusted", "on-failure", "on-request", "never"];
const SANDBOX_MODE_VALUES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];

const CODEX_CONFIG_EDITABLE_KEYS: &[&str] = &[
    "profile",
    "model",
    "review_model",
    "model_provider",
    "model_context_window",
    "model_auto_compact_token_limit",
    "instructions",
    "developer_instructions",
    "compact_prompt",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_verbosity",
    "web_search",
    "service_tier",
];

/// Allowlist patterns for curated app/tool config keys (parity with TS dto).
static APP_CONFIG_PATTERNS: Lazy<[Regex; 2]> = Lazy::new(|| {
    [
        Regex::new(r"^apps\.[A-Za-z0-9_-]+\.(enabled|destructive_enabled|open_world_enabled|default_tools_approval_mode|default_tools_enabled)$").unwrap(),
        Regex::new(r"^apps\.[A-Za-z0-9_-]+\.tools\.[A-Za-z0-9_-]+\.(enabled|approval_mode)$").unwrap(),
    ]
});

fn is_editable_key(key: &str) -> bool {
    if CODEX_CONFIG_EDITABLE_KEYS.contains(&key) {
        return true;
    }
    APP_CONFIG_PATTERNS.iter().any(|re| re.is_match(key))
}

/// Pattern matching sensitive key names redacted in config output.
static SENSITIVE_KEY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new("(?i)(token|password|api[_-]?key|secret|authorization)").unwrap());

// ── status ───────────────────────────────────────────────────────────────────

pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let generation = state.codex.generation();
    let connected = state.codex.client().await.is_some();
    // Simplified: full account/config/provider/model aggregation deferred.
    Ok(Json(json!({
        "ready": generation > 0,
        "generation": generation,
        "connected": connected,
        "initialized": generation > 0,
    })))
}

// ── approval-policy / sandbox-mode ───────────────────────────────────────────

#[derive(Deserialize)]
pub struct ApprovalPolicyBody {
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: Option<String>,
}

pub async fn update_approval_policy(
    State(state): State<AppState>,
    Json(body): Json<ApprovalPolicyBody>,
) -> Result<StatusCode, AppError> {
    let value = body.approval_policy.as_deref().map(|s| s.trim()).unwrap_or("");
    if value.is_empty() || !APPROVAL_POLICY_VALUES.contains(&value) {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidApprovalPolicy,
            "Invalid approval policy",
        ));
    }
    state
        .codex
        .request(
            "config/batchWrite",
            Some(json!({
                "edits": [{ "keyPath": "approval_policy", "value": value, "mergeStrategy": "replace" }],
                "reloadUserConfig": true,
            })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SandboxModeBody {
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: Option<String>,
}

pub async fn update_sandbox_mode(
    State(state): State<AppState>,
    Json(body): Json<SandboxModeBody>,
) -> Result<StatusCode, AppError> {
    let value = body.sandbox_mode.as_deref().map(|s| s.trim()).unwrap_or("");
    if value.is_empty() || !SANDBOX_MODE_VALUES.contains(&value) {
        return Err(bad_request(
            ErrorCode::ThreadsInvalidSandboxMode,
            "Invalid sandbox mode",
        ));
    }
    state
        .codex
        .request(
            "config/batchWrite",
            Some(json!({
                "edits": [{ "keyPath": "sandbox_mode", "value": value, "mergeStrategy": "replace" }],
                "reloadUserConfig": true,
            })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── config (structured) ──────────────────────────────────────────────────────

pub async fn read_config(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let response = state
        .codex
        .request("config/read", Some(json!({ "includeLayers": true })))
        .await
        .map_err(map_rpc)?;
    let config = response.get("config").cloned().unwrap_or(Value::Null);
    let origins = response.get("origins").cloned().unwrap_or(Value::Null);
    Ok(Json(json!({
        "config": redact_secrets(&config, ""),
        "origins": redact_secrets(&origins, ""),
    })))
}

#[derive(Deserialize)]
pub struct UpdateConfigBody {
    pub edits: Vec<ConfigEdit>,
}

#[derive(Deserialize)]
pub struct ConfigEdit {
    #[serde(rename = "keyPath")]
    pub key_path: String,
    pub value: Value,
}

pub async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<UpdateConfigBody>,
) -> Result<Json<Value>, AppError> {
    // Validate all edits against the allowlist.
    let mut validated: Vec<Value> = Vec::with_capacity(body.edits.len());
    for (i, edit) in body.edits.iter().enumerate() {
        let key = edit.key_path.trim();
        if key.is_empty() {
            return Err(bad_request(
                ErrorCode::CodexEditInvalid,
                format!("Invalid config edit at index {i}"),
            ));
        }
        if !is_editable_key(key) {
            return Err(bad_request(
                ErrorCode::CodexKeyUnsupported,
                format!("Unsupported config key: {key}"),
            ));
        }
        if edit.value.is_null() {
            return Err(bad_request(
                ErrorCode::CodexValueInvalid,
                "Clearing config values is not supported",
            ));
        }
        if !is_json_value(&edit.value) {
            return Err(bad_request(
                ErrorCode::CodexValueInvalidJson,
                format!("Invalid JSON value for {key}"),
            ));
        }
        validated.push(json!({ "keyPath": key, "value": edit.value, "mergeStrategy": "replace" }));
    }
    tracing::info!("updating {} codex config field(s)", validated.len());
    state
        .codex
        .request(
            "config/batchWrite",
            Some(json!({ "edits": validated, "reloadUserConfig": true })),
        )
        .await
        .map_err(map_rpc)?;
    // Return the updated config (re-read).
    read_config(State(state)).await
}

/// Validate a JSON value is safe (no prototype-pollution keys; finite numbers).
/// Parity with TS `isJsonValue`.
fn is_json_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(_) | Value::String(_) => true,
        Value::Number(n) => n.is_f64() && n.as_f64().map(|f| f.is_finite()).unwrap_or(false),
        Value::Array(a) => a.iter().all(is_json_value),
        Value::Object(m) => m
            .keys()
            .all(|k| k != "__proto__" && k != "constructor" && k != "prototype")
            && m.values().all(is_json_value),
    }
}

// ── config/raw (config.toml file) ────────────────────────────────────────────

pub async fn read_raw_config(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let path = user_config_path(&state).await?;
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(Json(json!({ "filePath": path, "content": content })))
}

#[derive(Deserialize)]
pub struct UpdateRawConfigBody {
    pub content: Value,
}

pub async fn update_raw_config(
    State(state): State<AppState>,
    Json(body): Json<UpdateRawConfigBody>,
) -> Result<Json<Value>, AppError> {
    let content = body
        .content
        .as_str()
        .ok_or_else(|| bad_request(ErrorCode::CodexRawContentInvalid, "Raw config content must be a string"))?;
    let path = user_config_path(&state).await?;
    tracing::info!("writing raw config.toml ({} bytes)", content.len());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    }
    std::fs::write(&path, content)
        .map_err(|e| AppError::internal(format!("write config: {e}")))?;
    // Hot-reload with empty edit batch.
    state
        .codex
        .request(
            "config/batchWrite",
            Some(json!({ "edits": [], "reloadUserConfig": true })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(Json(json!({ "filePath": path })))
}

/// Resolve the user config.toml path from config/read layers
/// (the user layer: `{ name: { type: 'user', file: <path> } }`).
async fn user_config_path(state: &AppState) -> Result<String, AppError> {
    let response = state
        .codex
        .request("config/read", Some(json!({ "includeLayers": true })))
        .await
        .map_err(map_rpc)?;
    if let Some(layers) = response.get("layers").and_then(Value::as_array) {
        for layer in layers {
            let name = layer.get("name");
            if name.and_then(|n| n.get("type")).and_then(Value::as_str) == Some("user") {
                if let Some(file) = name.and_then(|n| n.get("file")).and_then(Value::as_str) {
                    let f = file.trim();
                    if !f.is_empty() {
                        return Ok(f.to_string());
                    }
                }
            }
        }
    }
    Err(AppError::business(
        ErrorCode::CodexWriteFailed,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Codex user config.toml path was not reported by config/read".into(),
        None,
    ))
}

/// Recursively redact sensitive-keyed values (parity with TS `redactSecrets`).
fn redact_secrets(value: &Value, parent_key: &str) -> Value {
    if !parent_key.is_empty() && SENSITIVE_KEY_RE.is_match(parent_key) && !value.is_null() {
        return Value::String("[redacted]".into());
    }
    match value {
        Value::Array(a) => Value::Array(a.iter().map(|v| redact_secrets(v, parent_key)).collect()),
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                out.insert(k.clone(), redact_secrets(v, k));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

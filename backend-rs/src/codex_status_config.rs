//! Codex status + config REST 端点。
//!
//! 与 `codex-status.controller.ts` + `codex-config.controller.ts` 对齐。
//!
//! - GET  /codex/status          — 聚合就绪状态(appServer/initialize/account/
//!   config/provider/models/runtime,由 CodexStatusService 并行探针聚合 + TTL 缓存;
//!   详见 codex_status.rs)。驱动 UI 的 "codex ready" 指示器。
//! - POST /codex/approval-policy — config/batchWrite approval_policy(已校验)
//! - POST /codex/sandbox-mode    — config/batchWrite sandbox_mode(已校验)
//! - GET  /codex/config          — config/read(includeLayers),敏感信息已脱敏
//! - PATCH /codex/config         — 精选编辑(白名单),config/batchWrite
//! - GET  /codex/config/raw      — 读取用户 config.toml
//! - PUT  /codex/config/raw      — 写入用户 config.toml + 热重载

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

/// 精选 app/tool 配置键的白名单模式(与 TS dto 对齐)。
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

/// 匹配敏感键名的正则模式,用于在配置输出中脱敏。
static SENSITIVE_KEY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new("(?i)(token|password|api[_-]?key|secret|authorization)").unwrap());

// ── status ───────────────────────────────────────────────────────────────────

pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // 由 CodexStatusService 聚合（探针 + TTL 缓存），结构对齐 TS CodexStatusResponse。
    Ok(Json(state.status.get_status().await))
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
    state.status.invalidate();
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
    state.status.invalidate();
    Ok(StatusCode::NO_CONTENT)
}

// ── config(结构化)─────────────────────────────────────────────────────────

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
    // 依据白名单校验所有编辑。
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
    // 配置已变更，失效就绪状态缓存。
    state.status.invalidate();
    // 返回更新后的配置(重新读取)。
    read_config(State(state)).await
}

/// 校验 JSON 值是否安全(无原型污染键;数字有限)。
/// 与 TS `isJsonValue` 对齐。
fn is_json_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(_) | Value::String(_) => true,
        Value::Number(_) => true, // serde_json 永远不会存储非有限数值
        Value::Array(a) => a.iter().all(is_json_value),
        Value::Object(m) => m
            .keys()
            .all(|k| k != "__proto__" && k != "constructor" && k != "prototype")
            && m.values().all(is_json_value),
    }
}

// ── config/raw(config.toml 文件)───────────────────────────────────────────

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
    // 以空的编辑批次进行热重载。
    state
        .codex
        .request(
            "config/batchWrite",
            Some(json!({ "edits": [], "reloadUserConfig": true })),
        )
        .await
        .map_err(map_rpc)?;
    state.status.invalidate();
    Ok(Json(json!({ "filePath": path })))
}

/// 从 config/read 的 layers 中解析用户 config.toml 路径
/// (user 层:`{ name: { type: 'user', file: <path> } }`)。
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

/// 递归脱敏敏感键对应的值(与 TS `redactSecrets` 对齐)。
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

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
use crate::codex::jsonrpc::CodexJsonRpcClient;
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::State,
    http::StatusCode,
};
use crate::error::Json;
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

// ── status 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）────────

/// 单个 Codex 状态探针的错误元数据（对齐 TS CodexStatusErrorDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexStatusError {
    /// 错误信息
    pub message: String,
    /// 错误码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// app-server 进程与初始化状态（对齐 TS CodexAppServerStatusDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexAppServerProbe {
    /// 进程是否健康
    pub ok: bool,
    /// stdio 是否连通
    pub connected: bool,
    /// 是否完成 initialize 握手
    pub initialized: bool,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// initialize 探针结果（对齐 TS CodexInitializeStatusDto；data 为 app-server 返回的任意 JSON）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexInitializeProbe {
    /// initialize 握手是否成功
    pub ok: bool,
    /// app-server 返回的握手数据（任意 JSON）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// 通用状态探针结果（对齐 TS CodexAccountStatusDto；用于 account 段）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexStatusProbe {
    /// 账户探针是否通过
    pub ok: bool,
    /// account/read 返回的数据（任意 JSON）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// 脱敏后的 config/read 摘要（对齐 TS CodexConfigSummaryDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexConfigSummary {
    /// 沙箱模式：read-only/workspace-write/danger-full-access
    #[serde(rename = "sandboxMode", skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    /// 沙箱是否允许网络访问
    #[serde(rename = "sandboxNetworkAccess", skip_serializing_if = "Option::is_none")]
    pub sandbox_network_access: Option<bool>,
    /// 审批策略（untrusted/on-failure/on-request/never）
    #[serde(rename = "approvalPolicy", skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<serde_json::Value>,
    /// 当前模型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 模型提供方
    #[serde(rename = "modelProvider", skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
}

/// config 探针结果（对齐 TS CodexConfigStatusDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexConfigProbe {
    /// 配置探针是否通过
    pub ok: bool,
    /// 脱敏后的配置摘要
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<CodexConfigSummary>,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// provider 凭证可见性（对齐 TS CodexProviderStatusDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexProviderProbe {
    /// provider 探针是否通过
    pub ok: bool,
    /// provider 标识
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// provider 名称
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 脱敏后的 base URL
    #[serde(rename = "baseUrlMasked", skip_serializing_if = "Option::is_none")]
    pub base_url_masked: Option<String>,
    /// API key 对应的环境变量名
    #[serde(rename = "envKey", skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// 环境变量是否已设置
    #[serde(rename = "envPresent", skip_serializing_if = "Option::is_none")]
    pub env_present: Option<bool>,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// model/list 探针摘要（对齐 TS CodexModelsStatusDto；完整列表见 GET /api/models）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexModelsProbe {
    /// model/list 探针是否通过
    pub ok: bool,
    /// 模型列表是否可枚举
    pub listable: bool,
    /// 默认模型
    #[serde(rename = "defaultModel", skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 可用模型数量
    pub count: i64,
    /// 错误详情（失败时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodexStatusError>,
}

/// 运行时聚合状态（对齐 TS CodexRuntimeStatusDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexRuntimeProbe {
    /// 聚合状态：ready/degraded/unavailable
    pub status: String,
    /// 降级/不可用原因列表
    pub reasons: Vec<String>,
    /// 检查时间（RFC3339）
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
    /// 缓存有效期（毫秒）
    #[serde(rename = "cacheTtlMs")]
    pub cache_ttl_ms: i64,
}

/// 聚合就绪状态响应（对齐 TS CodexStatusResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexStatusResponse {
    /// app-server 子进程状态
    #[serde(rename = "appServer")]
    pub app_server: CodexAppServerProbe,
    /// initialize 握手探针
    pub initialize: CodexInitializeProbe,
    /// 账户探针
    pub account: CodexStatusProbe,
    /// 配置探针
    pub config: CodexConfigProbe,
    /// provider 凭证探针
    pub provider: CodexProviderProbe,
    /// 模型列表探针
    pub models: CodexModelsProbe,
    /// 运行时聚合状态
    pub runtime: CodexRuntimeProbe,
}

// ── status ───────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/codex/status",
    tag = "codex",
    responses(
        (status = 200, description = "聚合就绪状态（appServer/initialize/account/config/provider/models/runtime）", body = CodexStatusResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // 由 CodexStatusService 聚合（探针 + TTL 缓存），结构对齐 TS CodexStatusResponse。
    Ok(Json(state.status.get_status().await))
}

// ── approval-policy / sandbox-mode ───────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ApprovalPolicyBody {
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/codex/approval-policy",
    tag = "codex",
    request_body = ApprovalPolicyBody,
    responses(
        (status = 204, description = "approval_policy 已更新并热重载"),
        (status = 400, description = "非法 approval policy", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SandboxModeBody {
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/codex/sandbox-mode",
    tag = "codex",
    request_body = SandboxModeBody,
    responses(
        (status = 204, description = "sandbox_mode 已更新并热重载"),
        (status = 400, description = "非法 sandbox mode", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

// ── 启动默认配置 ─────────────────────────────────────────────────────────────

/// 启动时应用默认 codex 配置（仅当对应键缺失时写入）。
///
/// 读环境变量 `CODEX_DEFAULT_SANDBOX_MODE` / `CODEX_DEFAULT_APPROVAL_POLICY`，
/// 校验合法性后，对 codex config 中**缺失**的键发 `config/batchWrite` 设默认值。
/// 已存在的键不覆盖——尊重用户已有配置 / WebUI 修改（方案 B）。
///
/// 由 `codex::process::CodexProcessManager::start` 在 initialize 握手成功后调用，
/// 因此 codex 崩溃重启后会再次检查并补齐缺失项。best-effort：任何失败仅告警，不阻断启动。
pub async fn apply_defaults_if_absent(client: &CodexJsonRpcClient) {
    let sandbox = read_default_env("CODEX_DEFAULT_SANDBOX_MODE", SANDBOX_MODE_VALUES);
    let approval = read_default_env("CODEX_DEFAULT_APPROVAL_POLICY", APPROVAL_POLICY_VALUES);
    if sandbox.is_none() && approval.is_none() {
        return;
    }

    let response = match client
        .request("config/read", Some(json!({ "includeLayers": true })))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("apply_defaults: config/read 失败，跳过: {e}");
            return;
        }
    };
    let config = response.get("config").cloned().unwrap_or(Value::Null);

    let mut edits: Vec<Value> = Vec::new();
    if let Some(sm) = &sandbox {
        if config.get("sandbox_mode").is_none() {
            edits.push(json!({ "keyPath": "sandbox_mode", "value": sm, "mergeStrategy": "replace" }));
        }
    }
    if let Some(ap) = &approval {
        if config.get("approval_policy").is_none() {
            edits.push(json!({ "keyPath": "approval_policy", "value": ap, "mergeStrategy": "replace" }));
        }
    }

    if edits.is_empty() {
        return;
    }

    match client
        .request("config/batchWrite", Some(json!({ "edits": edits, "reloadUserConfig": true })))
        .await
    {
        Ok(_) => tracing::info!("apply_defaults: 已写入 {} 个缺失的默认配置项", edits.len()),
        Err(e) => tracing::warn!("apply_defaults: config/batchWrite 失败: {e}"),
    }
}

/// 读取默认值环境变量并校验是否在白名单内；非法或空值返回 None（非法值告警）。
fn read_default_env(var: &str, allowed: &[&str]) -> Option<String> {
    let v = std::env::var(var).ok()?.trim().to_string();
    if v.is_empty() {
        return None;
    }
    if !allowed.contains(&v.as_str()) {
        tracing::warn!("{var}={v:?} 不在合法值 {:?} 内，跳过", allowed);
        return None;
    }
    Some(v)
}

// ── config(结构化)─────────────────────────────────────────────────────────

/// 结构化配置响应（对齐 TS CodexConfigResponseDto；脱敏后的任意 JSON）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CodexConfigResponse {
    /// 脱敏后的配置
    pub config: serde_json::Value,
    /// 脱敏后的配置来源映射
    pub origins: serde_json::Value,
}

#[utoipa::path(
    get,
    path = "/api/codex/config",
    tag = "codex",
    responses(
        (status = 200, description = "结构化配置（含 layers；敏感字段已脱敏）", body = CodexConfigResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateConfigBody {
    pub edits: Vec<ConfigEdit>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ConfigEdit {
    #[serde(rename = "keyPath")]
    pub key_path: String,
    pub value: Value,
}

#[utoipa::path(
    patch,
    path = "/api/codex/config",
    tag = "codex",
    request_body = UpdateConfigBody,
    responses(
        (status = 200, description = "精选字段已更新，返回最新配置（白名单校验）", body = CodexConfigResponse),
        (status = 400, description = "edit/key/value 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

/// 用户 config.toml 原始内容（对齐 TS RawConfigResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RawConfigResponse {
    /// config.toml 绝对路径
    #[serde(rename = "filePath")]
    pub file_path: String,
    /// 文件原始内容
    pub content: String,
}

/// 写入 config.toml 后的响应（对齐 TS RawConfigWriteResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RawConfigWriteResponse {
    /// config.toml 绝对路径
    #[serde(rename = "filePath")]
    pub file_path: String,
}

#[utoipa::path(
    get,
    path = "/api/codex/config/raw",
    tag = "codex",
    responses(
        (status = 200, description = "用户 config.toml 原始内容（filePath + content）", body = RawConfigResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn read_raw_config(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let path = user_config_path(&state).await?;
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(Json(json!({ "filePath": path, "content": content })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateRawConfigBody {
    pub content: Value,
}

#[utoipa::path(
    put,
    path = "/api/codex/config/raw",
    tag = "codex",
    request_body = UpdateRawConfigBody,
    responses(
        (status = 200, description = "config.toml 已写入并热重载", body = RawConfigWriteResponse),
        (status = 400, description = "content 非字符串", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 413, description = "content 超过 1MB 上限", body = crate::error::ErrorResponse),
    )
)]
pub async fn update_raw_config(
    State(state): State<AppState>,
    Json(body): Json<UpdateRawConfigBody>,
) -> Result<Json<Value>, AppError> {
    let content = body
        .content
        .as_str()
        .ok_or_else(|| bad_request(ErrorCode::CodexRawContentInvalid, "Raw config content must be a string"))?;
    const MAX_RAW_CONFIG_BYTES: usize = 1024 * 1024;
    if content.len() > MAX_RAW_CONFIG_BYTES {
        return Err(AppError::business(
            ErrorCode::CodexRawContentInvalid,
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("raw config too large (max {MAX_RAW_CONFIG_BYTES} bytes)"),
            None,
        ));
    }
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
                        // H3 安全校验：限制路径位于 CODEX_HOME 下且为 .toml，防止任意文件读写。
                        let validated = validate_user_config_path(f)?;
                        return Ok(validated.to_string_lossy().into_owned());
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

/// H3 安全校验：限制用户 config 路径必须为 .toml 文件，且（若配置了 CODEX_HOME）
/// 位于 CODEX_HOME 目录下，防止 config/read 返回异常路径导致任意文件读写。
fn validate_user_config_path(raw: &str) -> Result<std::path::PathBuf, AppError> {
    use std::path::Path;
    let p = std::path::Path::new(raw);
    let is_toml = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("toml"))
        .unwrap_or(false);
    if !is_toml {
        return Err(AppError::business(
            ErrorCode::CodexWriteFailed,
            StatusCode::BAD_REQUEST,
            "user config path must point to a .toml file".into(),
            None,
        ));
    }
    let canonical = canonicalize_path_or_parent(p);
    if let Some(home) = std::env::var("CODEX_HOME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let home_c = canonicalize_path_or_parent(Path::new(&home));
        if !canonical.starts_with(&home_c) {
            return Err(AppError::business(
                ErrorCode::CodexWriteFailed,
                StatusCode::BAD_REQUEST,
                "user config path must reside under CODEX_HOME".into(),
                None,
            ));
        }
    }
    Ok(canonical)
}

/// 规范化路径：优先 canonicalize（文件存在时）；否则规范化父目录后拼接文件名。
fn canonicalize_path_or_parent(p: &std::path::Path) -> std::path::PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(c) = std::fs::canonicalize(parent) {
                if let Some(name) = p.file_name() {
                    return c.join(name);
                }
                return c;
            }
        }
    }
    p.to_path_buf()
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

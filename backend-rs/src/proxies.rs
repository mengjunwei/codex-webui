//! 轻量 REST 代理 → codex app-server JSON-RPC。
//!
//! 与 6 个 TS 代理模块对齐(account/apps/models/mcp-servers/skills/
//! plugins)。每个处理器校验输入、构建 JSON-RPC 参数、通过
//! `state.codex.request(method, params)` 转发,并将原始结果透传。
//! 返回 204 的端点(logout、login/cancel、mcp reload)返回 No Content。

use crate::codex::RpcError;
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// 将 codex RPC 错误映射为 500 AppError(TS 会透传 codex 错误 → 500)。
fn map_rpc(e: RpcError) -> AppError {
    AppError::internal(format!("codex: {e}"))
}

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

// ── 共享的 query/body 解析器 ────────────────────────────────────────────────

fn parse_limit(value: Option<&str>) -> Result<Option<i64>, AppError> {
    match value.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 1 && n <= 100 => Ok(Some(n)),
            _ => Err(bad_request(
                ErrorCode::ValidationFieldInvalid,
                "limit must be an integer between 1 and 100",
            )),
        },
    }
}

fn parse_optional_bool_query(value: Option<&str>, field: &str) -> Result<Option<bool>, AppError> {
    match value {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        _ => Err(bad_request(
            ErrorCode::ValidationTypeMismatch,
            format!("{field} must be a boolean"),
        )),
    }
}

fn parse_optional_bool_json(value: &Value, field: &str) -> Result<Option<bool>, AppError> {
    match value {
        Value::Null | Value::Bool(_) => Ok(value.as_bool()),
        _ => Err(bad_request(
            ErrorCode::ValidationTypeMismatch,
            format!("{field} must be a boolean"),
        )),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// account
// ════════════════════════════════════════════════════════════════════════════

pub async fn account_read(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let account = state
        .codex
        .request("account/read", Some(json!({ "refreshToken": false })))
        .await
        .map_err(map_rpc)?;
    // provider 元数据来自 CodexStatusService（对齐 TS getProviderStatus）。
    let provider = state.status.provider_status().await;
    let mut merged = account;
    if let Value::Object(ref mut m) = merged {
        m.insert("provider".into(), provider);
    }
    Ok(Json(merged))
}

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "accessToken")]
    pub access_token: Option<String>,
    #[serde(rename = "chatgptAccountId")]
    pub chatgpt_account_id: Option<String>,
    #[serde(rename = "chatgptPlanType")]
    pub chatgpt_plan_type: Option<String>,
}

pub async fn account_login(
    State(state): State<AppState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<Value>, AppError> {
    let params = match body.ty.as_str() {
        "apiKey" => {
            let api_key = body.api_key.as_deref().map(|s| s.trim()).unwrap_or("");
            if api_key.is_empty() {
                return Err(bad_request(
                    ErrorCode::AccountApiKeyRequired,
                    "apiKey is required",
                ));
            }
            json!({ "type": "apiKey", "apiKey": api_key })
        }
        "chatgpt" => json!({ "type": "chatgpt" }),
        "chatgptDeviceCode" => json!({ "type": "chatgptDeviceCode" }),
        "chatgptAuthTokens" => {
            let access_token = body.access_token.as_deref().map(|s| s.trim()).unwrap_or("");
            if access_token.is_empty() {
                return Err(bad_request(
                    ErrorCode::AccountAccessTokenRequired,
                    "accessToken is required",
                ));
            }
            let account_id = body.chatgpt_account_id.as_deref().map(|s| s.trim()).unwrap_or("");
            if account_id.is_empty() {
                return Err(bad_request(
                    ErrorCode::AccountChatgptAccountIdRequired,
                    "chatgptAccountId is required",
                ));
            }
            json!({
                "type": "chatgptAuthTokens",
                "accessToken": access_token,
                "chatgptAccountId": account_id,
                "chatgptPlanType": body.chatgpt_plan_type,
            })
        }
        _ => {
            return Err(bad_request(
                ErrorCode::AccountInvalidLoginType,
                "Invalid login type",
            ))
        }
    };
    let result = state
        .codex
        .request("account/login/start", Some(params))
        .await
        .map_err(map_rpc)?;
    state.status.invalidate();
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct LoginCancelBody {
    #[serde(rename = "loginId")]
    pub login_id: String,
}

pub async fn account_login_cancel(
    State(state): State<AppState>,
    Json(body): Json<LoginCancelBody>,
) -> Result<StatusCode, AppError> {
    let login_id = body.login_id.trim();
    if login_id.is_empty() {
        return Err(bad_request(
            ErrorCode::AccountLoginIdRequired,
            "loginId is required",
        ));
    }
    state
        .codex
        .request("account/login/cancel", Some(json!({ "loginId": login_id })))
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn account_logout(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("account/logout", None)
        .await
        .map_err(map_rpc)?;
    state.status.invalidate();
    Ok(StatusCode::NO_CONTENT)
}

pub async fn account_rate_limits(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let result = state
        .codex
        .request("account/rateLimits/read", None)
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════════════════
// apps
// ════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct AppsQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(rename = "forceRefetch")]
    pub force_refetch: Option<String>,
}

pub async fn apps_list(
    State(state): State<AppState>,
    Query(q): Query<AppsQuery>,
) -> Result<Json<Value>, AppError> {
    let mut params = serde_json::Map::new();
    if let Some(c) = q.cursor.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        params.insert("cursor".into(), Value::String(c.into()));
    }
    if let Some(limit) = parse_limit(q.limit.as_deref())? {
        params.insert("limit".into(), Value::Number(limit.into()));
    }
    if let Some(t) = q.thread_id.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        params.insert("threadId".into(), Value::String(t.into()));
    }
    if let Some(f) = parse_optional_bool_query(q.force_refetch.as_deref(), "forceRefetch")? {
        params.insert("forceRefetch".into(), Value::Bool(f));
    }
    let result = state
        .codex
        .request("app/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════════════════
// models
// ════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct ModelsQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
}

pub async fn models_list(
    State(state): State<AppState>,
    Query(q): Query<ModelsQuery>,
) -> Result<Json<Value>, AppError> {
    let mut params = serde_json::Map::new();
    if let Some(c) = q.cursor {
        params.insert("cursor".into(), Value::String(c));
    }
    if let Some(s) = q.limit.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match s.parse::<i64>() {
            Ok(n) => { params.insert("limit".into(), Value::Number(n.into())); }
            Err(_) => {} // TS:Number(bad) = NaN → undefined;省略
        }
    }
    let result = state
        .codex
        .request("model/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════════════════
// mcp-servers
// ════════════════════════════════════════════════════════════════════════════

const MCP_DETAIL_VALUES: &[&str] = &["full", "toolsAndAuthOnly"];

#[derive(Deserialize)]
pub struct McpListQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    pub detail: Option<String>,
}

pub async fn mcp_servers_list(
    State(state): State<AppState>,
    Query(q): Query<McpListQuery>,
) -> Result<Json<Value>, AppError> {
    let mut params = serde_json::Map::new();
    if let Some(c) = q.cursor.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        params.insert("cursor".into(), Value::String(c.into()));
    }
    if let Some(limit) = parse_limit(q.limit.as_deref())? {
        params.insert("limit".into(), Value::Number(limit.into()));
    }
    if let Some(d) = q.detail.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !MCP_DETAIL_VALUES.contains(&d) {
            return Err(bad_request(
                ErrorCode::McpInvalidServerDetail,
                "Invalid MCP server detail",
            ));
        }
        params.insert("detail".into(), Value::String(d.into()));
    }
    let result = state
        .codex
        .request("mcpServerStatus/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

pub async fn mcp_servers_reload(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("config/mcpServer/reload", None)
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct McpOauthBody {
    pub name: Option<String>,
    pub scopes: Option<Vec<String>>,
    #[serde(rename = "timeoutSecs")]
    pub timeout_secs: Option<i64>,
}

pub async fn mcp_servers_oauth_login(
    State(state): State<AppState>,
    Json(body): Json<McpOauthBody>,
) -> Result<Json<Value>, AppError> {
    let name = body.name.as_deref().map(|s| s.trim()).unwrap_or("");
    if name.is_empty() {
        return Err(bad_request(
            ErrorCode::ValidationFieldRequired,
            "name is required",
        ));
    }
    let mut params = serde_json::Map::new();
    params.insert("name".into(), Value::String(name.into()));
    if let Some(scopes) = body.scopes {
        let cleaned: Vec<String> = scopes
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !cleaned.is_empty() {
            params.insert(
                "scopes".into(),
                Value::Array(cleaned.into_iter().map(Value::String).collect()),
            );
        }
    }
    if let Some(t) = body.timeout_secs {
        if !(1..=600).contains(&t) {
            return Err(bad_request(
                ErrorCode::McpTimeoutTooLarge,
                "timeoutSecs must be an integer between 1 and 600",
            ));
        }
        params.insert("timeoutSecs".into(), Value::Number(t.into()));
    }
    let result = state
        .codex
        .request("mcpServer/oauth/login", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════════════════
// skills
// ════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct SkillsListQuery {
    pub cwd: Option<String>,
}

pub async fn skills_list(
    State(state): State<AppState>,
    Query(q): Query<SkillsListQuery>,
) -> Result<Json<Value>, AppError> {
    let cwd = q.cwd.as_deref().map(|s| s.trim()).unwrap_or("");
    if cwd.is_empty() {
        return Err(bad_request(
            ErrorCode::SkillsCwdRequired,
            "cwd is required",
        ));
    }
    let result = state
        .codex
        .request(
            "skills/list",
            Some(json!({ "cwds": [cwd] })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct SkillConfigBody {
    pub path: Option<String>,
    pub name: Option<String>,
    pub enabled: bool,
}

pub async fn skills_config_write(
    State(state): State<AppState>,
    Json(body): Json<SkillConfigBody>,
) -> Result<Json<Value>, AppError> {
    // `enabled` 为必填布尔值(serde 强制类型校验);path 优先于 name。
    let path = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    let (key, val) = if !path.is_empty() {
        ("path", path.to_string())
    } else {
        let name = body.name.as_deref().map(|s| s.trim()).unwrap_or("");
        if name.is_empty() {
            return Err(bad_request(
                ErrorCode::SkillsPathOrNameRequired,
                "path or name is required",
            ));
        }
        ("name", name.to_string())
    };
    let mut params = serde_json::Map::new();
    params.insert(key.into(), Value::String(val));
    params.insert("enabled".into(), Value::Bool(body.enabled));
    let result = state
        .codex
        .request("skills/config/write", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════════════════
// plugins
// ════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct PluginsListQuery {
    pub cwds: Option<String>,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<String>,
}

pub async fn plugins_list(
    State(state): State<AppState>,
    Query(q): Query<PluginsListQuery>,
) -> Result<Json<Value>, AppError> {
    let mut params = serde_json::Map::new();
    if let Some(cwds) = q.cwds.as_deref() {
        let list: Vec<Value> = cwds
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(Value::String)
            .collect();
        if !list.is_empty() {
            params.insert("cwds".into(), Value::Array(list));
        }
    }
    if let Some(f) = parse_optional_bool_query(q.force_remote_sync.as_deref(), "forceRemoteSync")? {
        params.insert("forceRemoteSync".into(), Value::Bool(f));
    }
    let result = state
        .codex
        .request("plugin/list", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct PluginDetailQuery {
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: Option<String>,
    #[serde(rename = "pluginName")]
    pub plugin_name: Option<String>,
}

pub async fn plugins_detail(
    State(state): State<AppState>,
    Query(q): Query<PluginDetailQuery>,
) -> Result<Json<Value>, AppError> {
    let mp = require_trimmed(&q.marketplace_path, "marketplacePath")?;
    let pn = require_trimmed(&q.plugin_name, "pluginName")?;
    let result = state
        .codex
        .request(
            "plugin/read",
            Some(json!({ "marketplacePath": mp, "pluginName": pn })),
        )
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct PluginInstallBody {
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: String,
    #[serde(rename = "pluginName")]
    pub plugin_name: String,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<Value>,
}

pub async fn plugins_install(
    State(state): State<AppState>,
    Json(body): Json<PluginInstallBody>,
) -> Result<Json<Value>, AppError> {
    let mp = require_trimmed(&Some(body.marketplace_path), "marketplacePath")?;
    let pn = require_trimmed(&Some(body.plugin_name), "pluginName")?;
    let mut params = serde_json::Map::new();
    params.insert("marketplacePath".into(), Value::String(mp));
    params.insert("pluginName".into(), Value::String(pn));
    if let Some(f) = body.force_remote_sync {
        if let Some(b) = parse_optional_bool_json(&f, "forceRemoteSync")? {
            params.insert("forceRemoteSync".into(), Value::Bool(b));
        }
    }
    let result = state
        .codex
        .request("plugin/install", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct PluginUninstallBody {
    #[serde(rename = "pluginId")]
    pub plugin_id: String,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<Value>,
}

pub async fn plugins_uninstall(
    State(state): State<AppState>,
    Json(body): Json<PluginUninstallBody>,
) -> Result<Json<Value>, AppError> {
    let pid = require_trimmed(&Some(body.plugin_id), "pluginId")?;
    let mut params = serde_json::Map::new();
    params.insert("pluginId".into(), Value::String(pid));
    if let Some(f) = body.force_remote_sync {
        if let Some(b) = parse_optional_bool_json(&f, "forceRemoteSync")? {
            params.insert("forceRemoteSync".into(), Value::Bool(b));
        }
    }
    let result = state
        .codex
        .request("plugin/uninstall", Some(Value::Object(params)))
        .await
        .map_err(map_rpc)?;
    Ok(Json(result))
}

fn require_trimmed(value: &Option<String>, field: &str) -> Result<String, AppError> {
    match value.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.into()),
        None => Err(bad_request(
            ErrorCode::PluginsFieldRequired,
            format!("{field} is required"),
        )),
    }
}

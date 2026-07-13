//! 轻量 REST 代理 → codex app-server JSON-RPC。
//!
//! 与 6 个 TS 代理模块对齐(account/apps/models/mcp-servers/skills/
//! plugins)。每个处理器校验输入、构建 JSON-RPC 参数、通过
//! `state.codex.request(method, params)` 转发,并将原始结果透传。
//! 返回 204 的端点(logout、login/cancel、mcp reload)返回 No Content。

use crate::codex::RpcError;
use crate::error::{AppError, ErrorCode, Params};
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
};
use crate::error::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// 将 codex RPC 错误映射为 500 AppError(TS 会透传 codex 错误 → 500)。
fn map_rpc(e: RpcError) -> AppError {
    AppError::internal(format!("codex: {e}"))
}

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

/// 构造带 i18n 插值参数的业务错误（对齐 TS 错误响应的 params 字段）。
fn bad_request_params(code: ErrorCode, msg: impl Into<String>, params: Params) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), Some(params))
}
fn one_param(k: &str, v: impl Into<Value>) -> Params {
    let mut p = Params::new();
    p.insert(k.into(), v.into());
    p
}
fn two_params(k1: &str, v1: impl Into<Value>, k2: &str, v2: impl Into<Value>) -> Params {
    let mut p = Params::new();
    p.insert(k1.into(), v1.into());
    p.insert(k2.into(), v2.into());
    p
}

// ── 共享的 query/body 解析器 ────────────────────────────────────────────────

fn parse_limit(value: Option<&str>) -> Result<Option<i64>, AppError> {
    match value.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 1 && n <= 100 => Ok(Some(n)),
            _ => Err(bad_request_params(
                ErrorCode::ValidationFieldInvalid,
                "limit must be an integer between 1 and 100",
                one_param("field", "limit"),
            )),
        },
    }
}

fn parse_optional_bool_query(value: Option<&str>, field: &str) -> Result<Option<bool>, AppError> {
    match value {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        _ => Err(bad_request_params(
            ErrorCode::ValidationTypeMismatch,
            format!("{field} must be a boolean"),
            two_params("field", field, "type", "boolean"),
        )),
    }
}

fn parse_optional_bool_json(value: &Value, field: &str) -> Result<Option<bool>, AppError> {
    match value {
        Value::Null | Value::Bool(_) => Ok(value.as_bool()),
        _ => Err(bad_request_params(
            ErrorCode::ValidationTypeMismatch,
            format!("{field} must be a boolean"),
            two_params("field", field, "type", "boolean"),
        )),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// account
// ════════════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/api/account",
    tag = "account",
    responses(
        (status = 200, description = "账户信息 + provider 元数据（codex account/read 透传）", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
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

#[utoipa::path(
    post,
    path = "/api/account/login",
    tag = "account",
    request_body = LoginBody,
    responses(
        (status = 200, description = "登录流程已启动（codex account/login/start 透传）", body = crate::error::GenericJson),
        (status = 400, description = "login type 非法/必填字段缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct LoginCancelBody {
    #[serde(rename = "loginId")]
    pub login_id: String,
}

#[utoipa::path(
    post,
    path = "/api/account/login/cancel",
    tag = "account",
    request_body = LoginCancelBody,
    responses(
        (status = 204, description = "登录流程已取消"),
        (status = 400, description = "loginId 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[utoipa::path(
    post,
    path = "/api/account/logout",
    tag = "account",
    responses(
        (status = 204, description = "已登出（codex account/logout 透传）"),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn account_logout(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("account/logout", None)
        .await
        .map_err(map_rpc)?;
    state.status.invalidate();
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/account/rate-limits",
    tag = "account",
    responses(
        (status = 200, description = "速率限制（codex account/rate-limits 透传）", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AppsQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(rename = "forceRefetch")]
    pub force_refetch: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/apps",
    tag = "apps",
    params(AppsQuery),
    responses(
        (status = 200, description = "应用列表（codex app/list 透传）", body = crate::error::GenericJson),
        (status = 400, description = "limit/forceRefetch 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ModelsQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/models",
    tag = "models",
    params(ModelsQuery),
    responses(
        (status = 200, description = "模型列表（codex model/list 透传）", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct McpListQuery {
    pub cursor: Option<String>,
    pub limit: Option<String>,
    pub detail: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/mcp-servers",
    tag = "mcp-servers",
    params(McpListQuery),
    responses(
        (status = 200, description = "MCP 服务端状态列表（codex mcpServerStatus/list 透传）", body = crate::error::GenericJson),
        (status = 400, description = "limit/detail 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[utoipa::path(
    post,
    path = "/api/mcp-servers/reload",
    tag = "mcp-servers",
    responses(
        (status = 204, description = "MCP 配置已重载（codex config/mcpServer/reload）"),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn mcp_servers_reload(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    state
        .codex
        .request("config/mcpServer/reload", None)
        .await
        .map_err(map_rpc)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct McpOauthBody {
    pub name: Option<String>,
    /// 保留原始 JSON 值，在 mcp_servers_oauth_login 内手动校验类型，
    /// 避免非数组直接触发 axum 422（无 errorCode），对齐 TS parseScopes。
    pub scopes: Option<Value>,
    /// 同上，避免非整数触发 axum 422，对齐 TS parseTimeoutSecs。
    #[serde(rename = "timeoutSecs")]
    pub timeout_secs: Option<Value>,
}

#[utoipa::path(
    post,
    path = "/api/mcp-servers/oauth/login",
    tag = "mcp-servers",
    request_body = McpOauthBody,
    responses(
        (status = 200, description = "OAuth 登录已启动（codex mcpServer/oauth/login 透传）", body = crate::error::GenericJson),
        (status = 400, description = "name/scopes/timeoutSecs 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn mcp_servers_oauth_login(
    State(state): State<AppState>,
    Json(body): Json<McpOauthBody>,
) -> Result<Json<Value>, AppError> {
    let name = body.name.as_deref().map(|s| s.trim()).unwrap_or("");
    if name.is_empty() {
        return Err(bad_request_params(
            ErrorCode::ValidationFieldRequired,
            "name is required",
            one_param("field", "name"),
        ));
    }
    let mut params = serde_json::Map::new();
    params.insert("name".into(), Value::String(name.into()));
    // scopes：手动校验原始 JSON，对齐 TS parseScopes。
    // - None/null → 省略 scopes；
    // - 非数组 → mcp.scopes_invalid；
    // - 任一元素非字符串或 trim 后为空 → mcp.scopes_empty；
    // - 全部非空才组装为 Vec<String>；空数组省略（与 TS 一致）。
    if let Some(scopes_val) = body.scopes {
        if !scopes_val.is_null() {
            let scopes_arr = scopes_val.as_array().ok_or_else(|| {
                bad_request(ErrorCode::McpScopesInvalid, "scopes must be an array")
            })?;
            let mut cleaned: Vec<String> = Vec::with_capacity(scopes_arr.len());
            for scope in scopes_arr {
                // 非字符串或 trim 后为空统一抛 mcp.scopes_empty（对齐 TS）。
                let trimmed = scope
                    .as_str()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        bad_request(
                            ErrorCode::McpScopesEmpty,
                            "scopes must be a non-empty array of strings",
                        )
                    })?;
                cleaned.push(trimmed.to_string());
            }
            if !cleaned.is_empty() {
                params.insert(
                    "scopes".into(),
                    Value::Array(cleaned.into_iter().map(Value::String).collect()),
                );
            }
        }
    }
    // timeoutSecs：手动校验原始 JSON，对齐 TS parseTimeoutSecs。
    // - None/null → 省略；
    // - 非整数 → mcp.timeout_invalid；
    // - 超出 1..=600 → mcp.timeout_too_large。
    if let Some(t_val) = body.timeout_secs {
        if !t_val.is_null() {
            let t = t_val
                .as_i64()
                .ok_or_else(|| {
                    bad_request(ErrorCode::McpTimeoutInvalid, "timeoutSecs must be an integer")
                })?;
            if !(1..=600).contains(&t) {
                return Err(bad_request_params(
                    ErrorCode::McpTimeoutTooLarge,
                    "timeoutSecs must be an integer between 1 and 600",
                    one_param("max", 600),
                ));
            }
            params.insert("timeoutSecs".into(), Value::Number(t.into()));
        }
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SkillsListQuery {
    pub cwd: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/skills",
    tag = "skills",
    params(SkillsListQuery),
    responses(
        (status = 200, description = "技能列表（codex skills/list 透传）", body = crate::error::GenericJson),
        (status = 400, description = "cwd 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SkillConfigBody {
    pub path: Option<String>,
    pub name: Option<String>,
    pub enabled: bool,
}

#[utoipa::path(
    post,
    path = "/api/skills/config",
    tag = "skills",
    request_body = SkillConfigBody,
    responses(
        (status = 200, description = "技能配置已写入（codex skills/config/write 透传）", body = crate::error::GenericJson),
        (status = 400, description = "path/name 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PluginsListQuery {
    pub cwds: Option<String>,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/plugins",
    tag = "plugins",
    params(PluginsListQuery),
    responses(
        (status = 200, description = "插件列表（codex plugin/list 透传）", body = crate::error::GenericJson),
        (status = 400, description = "forceRemoteSync 非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PluginDetailQuery {
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: Option<String>,
    #[serde(rename = "pluginName")]
    pub plugin_name: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/plugins/detail",
    tag = "plugins",
    params(PluginDetailQuery),
    responses(
        (status = 200, description = "插件详情（codex plugin/read 透传）", body = crate::error::GenericJson),
        (status = 400, description = "marketplacePath/pluginName 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PluginInstallBody {
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: String,
    #[serde(rename = "pluginName")]
    pub plugin_name: String,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<Value>,
}

#[utoipa::path(
    post,
    path = "/api/plugins/install",
    tag = "plugins",
    request_body = PluginInstallBody,
    responses(
        (status = 200, description = "插件已安装（codex plugin/install 透传）", body = crate::error::GenericJson),
        (status = 400, description = "marketplacePath/pluginName 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PluginUninstallBody {
    #[serde(rename = "pluginId")]
    pub plugin_id: String,
    #[serde(rename = "forceRemoteSync")]
    pub force_remote_sync: Option<Value>,
}

#[utoipa::path(
    post,
    path = "/api/plugins/uninstall",
    tag = "plugins",
    request_body = PluginUninstallBody,
    responses(
        (status = 200, description = "插件已卸载（codex plugin/uninstall 透传）", body = crate::error::GenericJson),
        (status = 400, description = "pluginId 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
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
        // 对齐 TS requireTrimmedString：携带 { field } 供前端 i18n 插值。
        None => Err(bad_request_params(
            ErrorCode::PluginsFieldRequired,
            format!("{field} is required"),
            one_param("field", field),
        )),
    }
}

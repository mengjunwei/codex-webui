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

// ── account 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）──────

/// 账户信息（对齐 TS AccountDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AccountDto {
    #[serde(rename = "type")]
    pub account_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(rename = "planType", skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
}

/// provider 错误元数据（对齐 TS AccountErrorDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AccountProviderError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// provider 凭证可见性（对齐 TS AccountProviderDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AccountProviderDto {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "baseUrlMasked", skip_serializing_if = "Option::is_none")]
    pub base_url_masked: Option<String>,
    #[serde(rename = "envKey", skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    #[serde(rename = "envPresent", skip_serializing_if = "Option::is_none")]
    pub env_present: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AccountProviderError>,
}

/// GET /api/account 响应（对齐 TS AccountReadResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AccountReadResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountDto>,
    #[serde(rename = "requiresOpenaiAuth")]
    pub requires_openai_auth: bool,
    pub provider: AccountProviderDto,
}

/// POST /api/account/login 响应（对齐 TS LoginAccountResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct LoginAccountResponse {
    #[serde(rename = "type")]
    pub account_type: String,
    #[serde(rename = "loginId", skip_serializing_if = "Option::is_none")]
    pub login_id: Option<String>,
    #[serde(rename = "authUrl", skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    #[serde(rename = "verificationUrl", skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    #[serde(rename = "userCode", skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}

/// rate-limit 窗口（对齐 TS RateLimitWindowDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RateLimitWindowDto {
    #[serde(rename = "usedPercent")]
    pub used_percent: f64,
    #[serde(rename = "windowDurationMins", skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<i64>,
    #[serde(rename = "resetsAt", skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// credits 快照（对齐 TS CreditsSnapshotDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct CreditsSnapshotDto {
    #[serde(rename = "hasCredits")]
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// rate-limit 快照（对齐 TS RateLimitSnapshotDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct RateLimitSnapshotDto {
    #[serde(rename = "limitId", skip_serializing_if = "Option::is_none")]
    pub limit_id: Option<String>,
    #[serde(rename = "limitName", skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateLimitWindowDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateLimitWindowDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditsSnapshotDto>,
    #[serde(rename = "planType", skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
}

/// GET /api/account/rate-limits 响应（对齐 TS AccountRateLimitsResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AccountRateLimitsResponse {
    #[serde(rename = "rateLimits")]
    pub rate_limits: RateLimitSnapshotDto,
    #[serde(rename = "rateLimitsByLimitId", skip_serializing_if = "Option::is_none")]
    pub rate_limits_by_limit_id: Option<serde_json::Value>,
}

#[utoipa::path(
    get,
    path = "/api/account",
    tag = "account",
    responses(
        (status = 200, description = "账户信息 + provider 元数据（codex account/read 透传）", body = AccountReadResponse),
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
        (status = 200, description = "登录流程已启动（codex account/login/start 透传）", body = LoginAccountResponse),
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
        (status = 200, description = "速率限制（codex account/rate-limits 透传）", body = AccountRateLimitsResponse),
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

// ── apps 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）────────

/// app 品牌信息（对齐 TS AppBrandingDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppBrandingDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(rename = "privacyPolicy", skip_serializing_if = "Option::is_none")]
    pub privacy_policy: Option<String>,
    #[serde(rename = "termsOfService", skip_serializing_if = "Option::is_none")]
    pub terms_of_service: Option<String>,
    #[serde(rename = "isDiscoverableApp")]
    pub is_discoverable_app: bool,
}

/// app 审核状态（对齐 TS AppReviewDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppReviewDto {
    pub status: String,
}

/// app 截图（对齐 TS AppScreenshotDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppScreenshotDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "fileId", skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(rename = "userPrompt")]
    pub user_prompt: String,
}

/// app 扩展元数据（对齐 TS AppMetadataDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppMetadataDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<AppReviewDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    #[serde(rename = "subCategories", skip_serializing_if = "Option::is_none")]
    pub sub_categories: Option<Vec<String>>,
    #[serde(rename = "seoDescription", skip_serializing_if = "Option::is_none")]
    pub seo_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshots: Option<Vec<AppScreenshotDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "versionId", skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(rename = "versionNotes", skip_serializing_if = "Option::is_none")]
    pub version_notes: Option<String>,
    #[serde(rename = "firstPartyType", skip_serializing_if = "Option::is_none")]
    pub first_party_type: Option<String>,
    #[serde(rename = "firstPartyRequiresInstall", skip_serializing_if = "Option::is_none")]
    pub first_party_requires_install: Option<bool>,
    #[serde(rename = "showInComposerWhenUnlinked", skip_serializing_if = "Option::is_none")]
    pub show_in_composer_when_unlinked: Option<bool>,
}

/// app 信息行（对齐 TS AppInfoDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppInfoDto {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "logoUrl", skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(rename = "logoUrlDark", skip_serializing_if = "Option::is_none")]
    pub logo_url_dark: Option<String>,
    #[serde(rename = "distributionChannel", skip_serializing_if = "Option::is_none")]
    pub distribution_channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branding: Option<AppBrandingDto>,
    #[serde(rename = "appMetadata", skip_serializing_if = "Option::is_none")]
    pub app_metadata: Option<AppMetadataDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<serde_json::Value>,
    #[serde(rename = "installUrl", skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
    #[serde(rename = "isAccessible")]
    pub is_accessible: bool,
    #[serde(rename = "isEnabled")]
    pub is_enabled: bool,
    #[serde(rename = "pluginDisplayNames")]
    pub plugin_display_names: Vec<String>,
}

/// GET /api/apps 响应（对齐 TS AppsListResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct AppsListResponse {
    pub data: Vec<AppInfoDto>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/apps",
    tag = "apps",
    params(AppsQuery),
    responses(
        (status = 200, description = "应用列表（codex app/list 透传）", body = AppsListResponse),
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

// ── models 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）───────

/// model 升级信息（对齐 TS ModelUpgradeInfoDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ModelUpgradeInfoDto {
    pub model: String,
    #[serde(rename = "upgradeCopy", skip_serializing_if = "Option::is_none")]
    pub upgrade_copy: Option<String>,
    #[serde(rename = "modelLink", skip_serializing_if = "Option::is_none")]
    pub model_link: Option<String>,
    #[serde(rename = "migrationMarkdown", skip_serializing_if = "Option::is_none")]
    pub migration_markdown: Option<String>,
}

/// model 可用性提示（对齐 TS ModelAvailabilityNuxDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ModelAvailabilityNuxDto {
    pub message: String,
}

/// reasoning effort 选项（对齐 TS ReasoningEffortOptionDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ReasoningEffortOptionDto {
    #[serde(rename = "reasoningEffort")]
    pub reasoning_effort: String,
    pub description: String,
}

/// model 信息（对齐 TS ModelDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ModelDto {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
    #[serde(rename = "upgradeInfo", skip_serializing_if = "Option::is_none")]
    pub upgrade_info: Option<ModelUpgradeInfoDto>,
    #[serde(rename = "availabilityNux", skip_serializing_if = "Option::is_none")]
    pub availability_nux: Option<ModelAvailabilityNuxDto>,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub description: String,
    pub hidden: bool,
    #[serde(rename = "supportedReasoningEfforts")]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOptionDto>,
    #[serde(rename = "defaultReasoningEffort")]
    pub default_reasoning_effort: String,
    #[serde(rename = "inputModalities")]
    pub input_modalities: Vec<String>,
    #[serde(rename = "supportsPersonality")]
    pub supports_personality: bool,
    #[serde(rename = "additionalSpeedTiers")]
    pub additional_speed_tiers: Vec<String>,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

/// GET /api/models 响应（对齐 TS ModelListResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ModelListResponse {
    pub data: Vec<ModelDto>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/models",
    tag = "models",
    params(ModelsQuery),
    responses(
        (status = 200, description = "模型列表（codex model/list 透传）", body = ModelListResponse),
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

// ── mcp-servers 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）──

/// GET /api/mcp-servers 响应（对齐 TS McpServersListResponseDto；元素透传 unknown）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct McpServersListResponse {
    pub data: Vec<serde_json::Value>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// POST /api/mcp-servers/oauth/login 响应（对齐 TS McpServerOauthLoginResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct McpServerOauthLoginResponse {
    #[serde(rename = "authorizationUrl")]
    pub authorization_url: String,
}

#[utoipa::path(
    get,
    path = "/api/mcp-servers",
    tag = "mcp-servers",
    params(McpListQuery),
    responses(
        (status = 200, description = "MCP 服务端状态列表（codex mcpServerStatus/list 透传）", body = McpServersListResponse),
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
        (status = 200, description = "OAuth 登录已启动（codex mcpServer/oauth/login 透传）", body = McpServerOauthLoginResponse),
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

// ── skills 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）──────

/// GET /api/skills 响应（对齐 TS SkillsListResponseDto；元素透传 unknown）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SkillsListResponse {
    pub data: Vec<serde_json::Value>,
}

/// POST /api/skills/config 响应（对齐 TS SkillsConfigWriteResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct SkillsConfigWriteResponse {
    #[serde(rename = "effectiveEnabled")]
    pub effective_enabled: bool,
}

#[utoipa::path(
    get,
    path = "/api/skills",
    tag = "skills",
    params(SkillsListQuery),
    responses(
        (status = 200, description = "技能列表（codex skills/list 透传）", body = SkillsListResponse),
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
        (status = 200, description = "技能配置已写入（codex skills/config/write 透传）", body = SkillsConfigWriteResponse),
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

// ── plugins 响应 DTO（仅用于 OpenAPI 文档；运行时仍返回 Json<Value>）────

/// marketplace 接口信息（对齐 TS MarketplaceInterfaceDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct MarketplaceInterfaceDto {
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// marketplace 加载错误（对齐 TS MarketplaceLoadErrorInfoDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct MarketplaceLoadErrorInfoDto {
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: String,
    pub message: String,
}

/// plugin 展示元数据（对齐 TS PluginInterfaceDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginInterfaceDto {
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "shortDescription", skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    #[serde(rename = "longDescription", skip_serializing_if = "Option::is_none")]
    pub long_description: Option<String>,
    #[serde(rename = "developerName", skip_serializing_if = "Option::is_none")]
    pub developer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub capabilities: Vec<String>,
    #[serde(rename = "websiteUrl", skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
    #[serde(rename = "privacyPolicyUrl", skip_serializing_if = "Option::is_none")]
    pub privacy_policy_url: Option<String>,
    #[serde(rename = "termsOfServiceUrl", skip_serializing_if = "Option::is_none")]
    pub terms_of_service_url: Option<String>,
    #[serde(rename = "defaultPrompt", skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<Vec<String>>,
    #[serde(rename = "brandColor", skip_serializing_if = "Option::is_none")]
    pub brand_color: Option<String>,
    #[serde(rename = "composerIcon", skip_serializing_if = "Option::is_none")]
    pub composer_icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    pub screenshots: Vec<String>,
}

/// plugin 摘要行（对齐 TS PluginSummaryDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginSummaryDto {
    pub id: String,
    pub name: String,
    pub source: serde_json::Value,
    pub installed: bool,
    pub enabled: bool,
    #[serde(rename = "installPolicy")]
    pub install_policy: String,
    #[serde(rename = "authPolicy")]
    pub auth_policy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<PluginInterfaceDto>,
}

/// marketplace 条目（对齐 TS PluginMarketplaceEntryDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginMarketplaceEntryDto {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<MarketplaceInterfaceDto>,
    pub plugins: Vec<PluginSummaryDto>,
}

/// GET /api/plugins 响应（对齐 TS PluginListResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginListResponse {
    pub marketplaces: Vec<PluginMarketplaceEntryDto>,
    #[serde(rename = "marketplaceLoadErrors")]
    pub marketplace_load_errors: Vec<MarketplaceLoadErrorInfoDto>,
    #[serde(rename = "remoteSyncError", skip_serializing_if = "Option::is_none")]
    pub remote_sync_error: Option<String>,
    #[serde(rename = "featuredPluginIds")]
    pub featured_plugin_ids: Vec<String>,
}

/// plugin skill 摘要（对齐 TS PluginSkillSummaryDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginSkillSummaryDto {
    pub name: String,
    pub description: String,
    #[serde(rename = "shortDescription", skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    pub path: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<serde_json::Value>,
}

/// plugin app 摘要（对齐 TS PluginAppSummaryDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginAppSummaryDto {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "installUrl", skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
    #[serde(rename = "needsAuth")]
    pub needs_auth: bool,
}

/// plugin 详情（对齐 TS PluginDetailDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginDetailDto {
    #[serde(rename = "marketplaceName")]
    pub marketplace_name: String,
    #[serde(rename = "marketplacePath")]
    pub marketplace_path: String,
    pub summary: PluginSummaryDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub skills: Vec<PluginSkillSummaryDto>,
    pub apps: Vec<PluginAppSummaryDto>,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<String>,
}

/// GET /api/plugins/detail 响应（对齐 TS PluginReadResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginReadResponse {
    pub plugin: PluginDetailDto,
}

/// POST /api/plugins/install 响应（对齐 TS PluginInstallResponseDto）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginInstallResponse {
    #[serde(rename = "authPolicy")]
    pub auth_policy: String,
    #[serde(rename = "appsNeedingAuth")]
    pub apps_needing_auth: Vec<PluginAppSummaryDto>,
}

/// POST /api/plugins/uninstall 响应（对齐 TS PluginUninstallResponseDto；空对象）。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct PluginUninstallResponse {}

#[utoipa::path(
    get,
    path = "/api/plugins",
    tag = "plugins",
    params(PluginsListQuery),
    responses(
        (status = 200, description = "插件列表（codex plugin/list 透传）", body = PluginListResponse),
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
        (status = 200, description = "插件详情（codex plugin/read 透传）", body = PluginReadResponse),
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
        (status = 200, description = "插件已安装（codex plugin/install 透传）", body = PluginInstallResponse),
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
        (status = 200, description = "插件已卸载（codex plugin/uninstall 透传）", body = PluginUninstallResponse),
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

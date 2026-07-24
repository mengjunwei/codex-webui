//! OnlyOffice Docs 集成 —— 编辑器配置构建器（对齐
//! `onlyoffice.controller.ts:getConfig`）。
//!
//! 编辑模式（默认）需要配置 `general.onlyofficeJwtSecret`，以便安全地校验
//! 保存回调。回调端点本身（`/api/onlyoffice/callback`）需要 HTTP 下载 +
//! 原子写入 —— 待文件服务支持 multipart/流式传输后再实现。

use crate::error::{AppError, ErrorCode};
use crate::services::files;
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use crate::error::Json;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use url::Url;
use once_cell::sync::Lazy;

/// 全局复用的 HTTP 客户端：禁用重定向跟随以防 SSRF（下载 URL 校验仅覆盖初始 origin），
/// 并复用连接池与 TLS 上下文。保存请求的超时在每次请求上单独指定。
static SAVE_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build onlyoffice save HTTP client")
});

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

/// 413 Payload Too Large —— 对齐 TS BusinessException.payloadTooLarge。
fn payload_too_large(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::PAYLOAD_TOO_LARGE, msg.into(), None)
}

// ── GET /onlyoffice/config?path=…&mode=… ────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConfigQuery {
    pub path: Option<String>,
    pub mode: Option<String>,
}

/// GET /api/onlyoffice/config 成功响应 —— 含 API 脚本地址与签名后的编辑器配置。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(non_snake_case)]
pub struct OnlyOfficeConfigResponse {
    /// OnlyOffice Docs API 脚本地址（`.../web-apps/apps/api/documents/api.js`）。
    pub scriptUrl: String,
    /// 编辑器配置对象（结构复杂，透传为原始 JSON；配置密钥时含 `token` 签名）。
    pub config: serde_json::Value,
}

#[utoipa::path(
    get,
    path = "/api/onlyoffice/config",
    tag = "onlyoffice",
    params(ConfigQuery),
    responses(
        (status = 200, description = "OnlyOffice 编辑器配置（含 JWT 签名）", body = OnlyOfficeConfigResponse),
        (status = 400, description = "未配置/URL 非法/格式不支持/edit 模式缺 secret", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "文件不存在", body = crate::error::ErrorResponse),
    )
)]
pub async fn get_config(
    State(state): State<AppState>,
    Query(q): Query<ConfigQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let reader = state.settings_reader();

    // 1. 解析 OnlyOffice URL（必填）。
    let onlyoffice_url = reader
        .get_string("general.onlyofficeUrl").await
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request(
            ErrorCode::OnlyOfficeNotConfigured,
            "OnlyOffice is not configured",
        ))?;
    let normalized_url = normalize_http_base_url(&onlyoffice_url, "general.onlyofficeUrl")
        .map_err(|_| bad_request(
            ErrorCode::OnlyOfficeInvalidUrl,
            "general.onlyofficeUrl must be a valid http(s) URL",
        ))?;

    // 2. 解析 JWT 密钥。
    let secret = reader.get_string("general.onlyofficeJwtSecret").await;

    // 3. 编辑模式必须有密钥。
    let editor_mode = if q.mode.as_deref() == Some("view") {
        "view"
    } else {
        "edit"
    };
    if editor_mode == "edit" && secret.is_none() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeJwtRequired,
            "OnlyOffice edit mode requires general.onlyofficeJwtSecret to be configured",
        ));
    }

    // 4. 校验文件路径（必须是位于工作区根目录之下的已存在文件）。
    let raw_path = q.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw_path.is_empty() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeFileRequired,
            "path is required",
        ));
    }
    let resolved = crate::services::files::resolve_safe_path(&state, raw_path).await?;
    let meta = tokio::fs::metadata(&resolved).await
        .map_err(|e| AppError::internal(format!("metadata: {e}")))?;
    if !meta.is_file() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeFileRequired,
            "OnlyOffice requires a file path",
        ));
    }
    let filename = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // 5. 校验是否为受支持的扩展名。
    let file_type = match filename.rsplit('.').next().map(|s| s.to_ascii_lowercase()) {
        Some(ext) if matches!(ext.as_str(), "docx" | "xlsx" | "pptx") => ext,
        _ => {
            return Err(bad_request(
                ErrorCode::OnlyOfficeUnsupportedFormat,
                "OnlyOffice supports DOCX, XLSX, and PPTX files",
            ));
        }
    };
    let document_type = match file_type.as_str() {
        "docx" => "word",
        "xlsx" => "cell",
        _ => "slide",
    };

    // 6. 构建稳定的文档 key（path+mtime+size 的哈希，≤128 字符）。
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len();
    // H2 修复：用解析后（规范化）的路径生成稳定的文档 key，而非原始输入
    //（避免相对/绝对/符号链接等不同表示导致 key 不一致 → OO 缓存陈旧）。
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", resolved.display(), mtime, size).as_bytes());
    let key = hex::encode(hasher.finalize());
    let key = key[..key.len().min(48)].to_string();

    // 7. 公共 base URL（对齐 TS 的 publicBaseUrl 设置或 host 请求头）。
    let base_url = reader
        .get_string("general.publicBaseUrl").await
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    // H3 修复：当 publicBaseUrl 为空时，从请求头自动探测
    // （对齐 TS onlyoffice.controller.ts:518-546）。
    let base_url = if !base_url.is_empty() {
        normalize_http_base_url(&base_url, "general.publicBaseUrl")
            .map_err(|_| bad_request(
                ErrorCode::OnlyOfficeInvalidUrl,
                "general.publicBaseUrl must be a valid http(s) URL",
            ))?
    } else {
        let proto = headers.get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "http".to_string());
        let host = headers.get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_string())
            .or_else(|| headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).map(|s| s.to_string()));
        match host {
            Some(h) if !h.is_empty() => {
                let inferred = format!("{proto}://{h}");
                normalize_http_base_url(&inferred, "request host headers")
                    .map_err(|_| bad_request(
                        ErrorCode::OnlyOfficePublicHostRequired,
                        "Cannot determine public host from request headers. Configure general.publicBaseUrl.",
                    ))?
            }
            _ => return Err(bad_request(
                ErrorCode::OnlyOfficePublicHostRequired,
                "Cannot determine public host. Configure general.publicBaseUrl in Settings.",
            )),
        }
    };

    // H4 修复：使用解析后（规范化）的路径替代 raw_path，避免相对路径/路径穿越
    // 导致 documentUrl、documentKey、callback state 三处 key 不一致
    // （对齐 TS 使用 metadata.path 而非用户原始输入）。
    let norm_path = resolved.to_string_lossy().to_string();

    // 提取调用方的 bearer JWT 用于文档 URL（OnlyOffice Document Server
    // 不带凭证获取 —— 需按 RFC 6750 §2.3 通过 access_token 查询参数传递）。
    // 中等优先级修复：大小写不敏感的 Bearer 前缀，正确的偏移量切片。
    let caller_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            let lower = h.to_ascii_lowercase();
            if let Some(_) = lower.strip_prefix("bearer ") {
                // 安全："bearer " 是 7 个 ASCII 字符，转小写不会改变字节长度。
                Some(h[7..].trim().to_string())
            } else if lower.strip_prefix("bearer\t").is_some() {
                Some(h[7..].trim().to_string())
            } else {
                None
            }
        })
        .filter(|t| !t.is_empty());

    // H2 安全修复：不把调用方的长期会话 JWT（24h TTL）嵌入 document_url，
    // 改为用同一 JWT 密钥签发短期（5 分钟）下载 token，将泄漏窗口从 24h 降至 5min。
    // 仅当调用方已通过 bearer 认证（caller_token 存在）时才签发。
    let document_url = if caller_token.is_some() {
        let now = chrono::Utc::now().timestamp() as usize;
        let dl_claims = json!({ "sub": "webui", "iat": now, "exp": now + 300 });
        let dl_token = encode(
            &Header::new(Algorithm::HS256),
            &dl_claims,
            &EncodingKey::from_secret(state.auth.jwt_secret().as_bytes()),
        )
        .map_err(|e| AppError::internal(format!("download token sign: {e}")))?;
        format!(
            "{}/api/files/serve?path={}&access_token={}",
            base_url.trim_end_matches('/'),
            url_encode(&norm_path),
            url_encode(&dl_token)
        )
    } else {
        format!(
            "{}/api/files/serve?path={}",
            base_url.trim_end_matches('/'),
            url_encode(&norm_path)
        )
    };
    let callback_state_token = if let Some(ref s) = secret {
        let now = chrono::Utc::now().timestamp() as usize;
        Some(encode(
            &Header::new(Algorithm::HS256),
            &json!({ "path": norm_path, "key": key, "iat": now, "exp": now + 86400 }),
            &EncodingKey::from_secret(s.as_bytes()),
        )
        .map_err(|e| AppError::internal(format!("jwt sign: {e}")))?)
    } else {
        None
    };
    let mut callback_params = format!("path={}", url_encode(&norm_path));
    if let Some(ref t) = callback_state_token {
        callback_params.push_str(&format!("&state={}", url_encode(t)));
    }
    let callback_url = format!(
        "{}/api/onlyoffice/callback?{}",
        base_url.trim_end_matches('/'),
        callback_params
    );

    // 8. 组装编辑器配置 payload。
    let mut config = json!({
        "type": "desktop",
        "width": "100%",
        "height": "100%",
        "documentType": document_type,
        "document": {
            "fileType": file_type,
            "key": key,
            "permissions": {
                "comment": editor_mode == "edit",
                "copy": true,
                "download": true,
                "edit": editor_mode == "edit",
                "print": true,
                "review": false,
            },
            "title": filename,
            "url": document_url,
        },
        "editorConfig": {
            "callbackUrl": callback_url,
            "mode": editor_mode,
            "customization": {
                "compactToolbar": true,
                "hideRightMenu": editor_mode == "view",
            },
        },
    });

    // 9. 若配置了密钥，则用 JWT 对配置签名。
    if let Some(ref s) = secret {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &config,
            &EncodingKey::from_secret(s.as_bytes()),
        )
        .map_err(|e| AppError::internal(format!("jwt sign: {e}")))?;
        config["token"] = Value::String(token);
    }

    Ok(Json(json!({
        "scriptUrl": format!("{}/web-apps/apps/api/documents/api.js", normalized_url.trim_end_matches('/')),
        "config": config,
    })))
}

// ── POST /onlyoffice/callback ────────────────────────────────────────────────
//
// OnlyOffice 保存回调 —— 对齐 TS 的 handleCallback。公共端点
// （前端不传 JWT；由 OnlyOffice Document Server 直接调用）。
//
// 安全模型：
// 1. 必须配置 onlyofficeJwtSecret（编辑模式会强制要求）。
// 2. 校验 state token（已签名的 path+key）。
// 3. 校验 OnlyOffice 回调 JWT（body.token 或 Authorization 请求头）。
// 4. 下载 URL 的源必须与配置的 onlyofficeUrl 匹配（防 SSRF）。
// 5. 文件路径需通过工作区根目录校验。
// 6. 原子写入（临时文件 + rename，含大小限制与超时）。

const SAVE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_SAVE_BYTES: u64 = 104_857_600; // 100 MB

#[derive(Deserialize, Default, utoipa::ToSchema)]
pub struct CallbackBody {
    pub status: Option<i64>,
    pub url: Option<String>,
    pub key: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallbackQuery {
    pub path: Option<String>,
    pub state: Option<String>, // 已签名的 JWT state token
}

#[derive(serde::Deserialize)]
struct CallbackStatePayload {
    path: Option<String>,
    key: Option<String>,
}

#[derive(serde::Deserialize)]
struct CallbackJwtPayload {
    #[serde(flatten)]
    _extra: std::collections::HashMap<String, Value>,
}

/// POST /api/onlyoffice/callback 成功响应 —— OnlyOffice 期望的 `{error: 0|1}`。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct OnlyOfficeCallbackResponse {
    /// 回调处理结果：0=成功，1=失败。
    pub error: i64,
}

#[utoipa::path(
    post,
    path = "/api/onlyoffice/callback",
    tag = "onlyoffice",
    params(CallbackQuery),
    request_body = CallbackBody,
    responses(
        (status = 200, description = "回调处理结果 {error: 0|1}（公开端点，用 JWT 校验）", body = OnlyOfficeCallbackResponse),
        (status = 400, description = "回调 JWT/state 非法/下载 URL 校验失败", body = crate::error::ErrorResponse),
        (status = 413, description = "保存内容超过上限", body = crate::error::ErrorResponse),
    )
)]
pub async fn handle_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
    Json(body): Json<CallbackBody>,
) -> Result<Json<Value>, AppError> {
    // 对非保存状态立即应答。
    let status = body.status.unwrap_or(0);
    if status != 2 && status != 6 {
        return Ok(Json(json!({ "error": 0 })));
    }

    let file_path = q.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if file_path.is_empty() {
        tracing::warn!("OnlyOffice callback missing file path");
        return Ok(Json(json!({ "error": 1 })));
    }
    let download_url = body.url.as_deref().unwrap_or("");
    if download_url.is_empty() {
        tracing::warn!(key = ?body.key, "OnlyOffice save callback missing download URL");
        return Ok(Json(json!({ "error": 1 })));
    }

    // 将真正的工作放在一个 async 块中；任何错误都返回 {error: 1}。
    let save_status = body.status.unwrap_or(0);
    match callback_inner(&state, &q, &body, &headers).await {
        Ok(()) => {
            tracing::info!(path = q.path.as_deref().unwrap_or(""), status = save_status, "OnlyOffice document saved");
            Ok(Json(json!({ "error": 0 })))
        }
        Err(e) => {
            tracing::error!(path = q.path.as_deref().unwrap_or(""), error = %e, "OnlyOffice callback failed");
            Ok(Json(json!({ "error": 1 })))
        }
    }
}

async fn callback_inner(
    state: &AppState,
    q: &CallbackQuery,
    body: &CallbackBody,
    headers: &HeaderMap,
) -> Result<(), AppError> {
    let reader = state.settings_reader();
    let file_path = q.path.as_deref().unwrap_or("").trim();

    // 1. 必须已配置 JWT 密钥。
    let secret = reader
        .get_string("general.onlyofficeJwtSecret").await
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request(ErrorCode::OnlyOfficeJwtRequired, "JWT secret not configured"))?;

    // 2. 校验回调 state token（查询参数 ?state= 中已签名的 path + key）。
    let _state_payload = verify_callback_state(q.state.as_deref(), &secret, file_path, body.key.as_deref())?;

    // 3. 校验 OnlyOffice 回调 JWT（body.token 或 Authorization 请求头）。
    let oo_token: Option<String> = body
        .token
        .clone()
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|h| {
                    let lower = h.to_ascii_lowercase();
                    if lower.strip_prefix("bearer ").is_some() {
                        Some(h[7..].trim().to_string())
                    } else if lower.strip_prefix("bearer\t").is_some() {
                        Some(h[7..].trim().to_string())
                    } else {
                        None
                    }
                })
        });
    verify_onlyoffice_token(oo_token.as_deref(), &secret)?;

    // 4. 校验下载 URL 的源是否与配置的 OnlyOffice 服务器一致。
    let onlyoffice_url = reader
        .get_string("general.onlyofficeUrl").await
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request(ErrorCode::OnlyOfficeNotConfigured, "OnlyOffice URL not configured"))?;
    let download_url = body.url.as_deref().unwrap_or("");
    validate_download_url(download_url, &onlyoffice_url)?;

    // 5. 校验文件路径是否位于工作区根目录之内。
    let resolved = files::resolve_safe_path(state, file_path).await?;

    // 6. 带超时与大小限制地抓取，并原子写入。
    let max_bytes = reader
        .get_number("general.onlyofficeSaveMaxBytes").await
        .map(|n| n as u64)
        .unwrap_or(DEFAULT_MAX_SAVE_BYTES);

    // H1 安全修复：复用全局 Client 并禁用重定向（Policy::none）。
    // validate_download_url 仅校验初始 origin，若跟随 3xx 可被导向内网（SSRF）；
    // 禁用后任何 3xx 都会因 !is_success() 被当作失败拒绝。
    let response = SAVE_CLIENT
        .get(download_url)
        .timeout(std::time::Duration::from_secs(SAVE_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("download failed: {e}")))?;

    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "OnlyOffice download HTTP {}",
            response.status()
        )));
    }

    // 校验 Content-Length 请求头是否超出限制。
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            return Err(payload_too_large(
                ErrorCode::OnlyOfficeSaveTooLarge,
                "OnlyOffice save payload exceeds size limit",
            ));
        }
    }

    // 以字节计数器将响应体流式写入临时文件。
    let parent = resolved.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".onlyoffice-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    match download_and_write(&tmp_path, response, max_bytes).await {
        Ok(()) => {
            tokio::fs::rename(&tmp_path, &resolved)
                .await
                .map_err(|e| AppError::internal(format!("rename: {e}")))?;
            Ok(())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(e)
        }
    }
}

async fn download_and_write(
    tmp_path: &Path,
    response: reqwest::Response,
    max_bytes: u64,
) -> Result<(), AppError> {
    let mut file = tokio::fs::File::create(tmp_path)
        .await
        .map_err(|e| AppError::internal(format!("create tmp: {e}")))?;

    // H1 修复：将响应体实时带字节计数地流式写入磁盘（而非用 bytes()
    // 将整个响应缓冲在内存中 —— 在 chunked-encoding 下存在 DoS 风险）。
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AppError::internal(format!("download stream: {e}")))?;
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(payload_too_large(
                ErrorCode::OnlyOfficeSaveTooLarge,
                "OnlyOffice save payload exceeds size limit",
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| AppError::internal(format!("write chunk: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| AppError::internal(format!("flush: {e}")))?;
    // 空响应体保护（对齐 TS onlyoffice.controller.ts 的 saveNoBody）。
    // TS 在 writeAtomically 开头检查 !response.body 即抛 saveNoBody；
    // Rust 这里在流式读取完成后通过 total==0 判定无内容下载，
    // 避免把空文件写回工作区（当前实现会先创建空临时文件，由调用方清理）。
    if total == 0 {
        return Err(bad_request(
            ErrorCode::OnlyOfficeSaveNoBody,
            "OnlyOffice save response has no body",
        ));
    }
    Ok(())
}

/// 校验已签名的回调 state token（含 {path, key} 的 JWT）。
fn verify_callback_state(
    state_token: Option<&str>,
    secret: &str,
    expected_path: &str,
    expected_key: Option<&str>,
) -> Result<CallbackStatePayload, AppError> {
    let token = state_token.ok_or_else(|| {
        bad_request(ErrorCode::OnlyOfficeMissingCallbackState, "Missing state token")
    })?;
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.leeway = 0;
    // OnlyOffice 回调 JWT 可能不携带 exp（TS 的 verify 并不要求该字段）。
    v.required_spec_claims.clear();
    // L3 修复：禁用 aud 校验（jsonwebtoken 默认会拒绝带 aud 字段的 token）。
    v.validate_aud = false;
    let data = decode::<CallbackStatePayload>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map_err(|_| {
        bad_request(
            ErrorCode::OnlyOfficeInvalidCallbackState,
            "Invalid callback state token",
        )
    })?;
    if data.claims.path.as_deref() != Some(expected_path) {
        return Err(bad_request(
            ErrorCode::OnlyOfficeInvalidCallbackStatePayload,
            "Callback state path mismatch",
        ));
    }
    if let (Some(ek), Some(pk)) = (expected_key, data.claims.key.as_deref()) {
        if ek != pk {
            return Err(bad_request(
                ErrorCode::OnlyOfficeInvalidCallbackStatePayload,
                "Callback state key mismatch",
            ));
        }
    }
    Ok(data.claims)
}

/// 校验 OnlyOffice 回调 JWT（body.token 或 Authorization 请求头）。
fn verify_onlyoffice_token(token: Option<&str>, secret: &str) -> Result<(), AppError> {
    let token = token.ok_or_else(|| {
        bad_request(
            ErrorCode::OnlyOfficeMissingCallbackJwt,
            "Missing OnlyOffice callback JWT",
        )
    })?;
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    v.leeway = 0;
    v.required_spec_claims.clear();
    v.validate_aud = false;
    decode::<CallbackJwtPayload>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map_err(|_| {
        bad_request(
            ErrorCode::OnlyOfficeInvalidCallbackJwt,
            "Invalid OnlyOffice callback JWT",
        )
    })?;
    Ok(())
}

/// 校验下载 URL 的源与配置的 OnlyOffice 服务器是否匹配（防 SSRF）。
fn validate_download_url(raw_url: &str, allowed_origin_url: &str) -> Result<(), AppError> {
    let download = Url::parse(raw_url).map_err(|_| {
        bad_request(
            ErrorCode::OnlyOfficeInvalidDownloadUrl,
            "Invalid download URL",
        )
    })?;
    if download.scheme() != "http" && download.scheme() != "https" {
        return Err(bad_request(
            ErrorCode::OnlyOfficeDownloadUrlNotHttps,
            "Download URL must use HTTP(S)",
        ));
    }
    let allowed = Url::parse(allowed_origin_url).map_err(|e| {
        AppError::internal(format!("allowed origin parse: {e}"))
    })?;
    // 比较源（scheme + host + port）。
    if download.origin() != allowed.origin() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeDownloadUrlOriginMismatch,
            "Download URL origin does not match configured OnlyOffice server",
        ));
    }
    Ok(())
}

// ── helpers（辅助函数）────────────────────────────────────────────────────

fn normalize_http_base_url(raw: &str, _label: &str) -> Result<String, String> {
    let mut url = url::Url::parse(raw).map_err(|_| "invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("unsupported protocol: {}", url.scheme()));
    }
    // L2 修复：剥离 query 和 fragment（对齐 TS onlyoffice.controller.ts:608-609）。
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn url_encode(s: &str) -> String {
    // 针对查询参数值的最小化百分号编码。
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

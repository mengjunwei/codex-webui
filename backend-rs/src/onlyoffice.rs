//! OnlyOffice Docs integration — editor config builder (parity with
//! `onlyoffice.controller.ts:getConfig`).
//!
//! Edit mode (default) requires `general.onlyofficeJwtSecret` so the save
//! callback can be securely verified. The callback endpoint itself
//! (`/api/onlyoffice/callback`) needs HTTP download + atomic write —
//! deferred until files service gains multipart/streaming support.

use crate::error::{AppError, ErrorCode};
use crate::files;
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use url::Url;

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

// ── GET /onlyoffice/config?path=…&mode=… ────────────────────────────────────

#[derive(Deserialize)]
pub struct ConfigQuery {
    pub path: Option<String>,
    pub mode: Option<String>,
}

pub async fn get_config(
    State(state): State<AppState>,
    Query(q): Query<ConfigQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let reader = state.settings_reader();

    // 1. Resolve OnlyOffice URL (required).
    let onlyoffice_url = reader
        .get_string("general.onlyofficeUrl")
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

    // 2. Resolve JWT secret.
    let secret = reader.get_string("general.onlyofficeJwtSecret");

    // 3. Edit mode requires secret.
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

    // 4. Validate file path (must be an existing file under workspace root).
    let raw_path = q.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw_path.is_empty() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeFileRequired,
            "path is required",
        ));
    }
    let resolved = crate::files::resolve_safe_path(&state, raw_path).await?;
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

    // 5. Validate supported extension.
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

    // 6. Build a stable document key (hash of path+mtime+size, ≤128 chars).
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len();
    // H2 FIX: use resolved (canonical) path for stable document key, not raw input.
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", resolved.to_string_lossy(), mtime, size).as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let key = key[..key.len().min(48)].to_string();

    // 7. Public base URL (parity with TS publicBaseUrl setting or host headers).
    let base_url = reader
        .get_string("general.publicBaseUrl")
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if base_url.is_empty() {
        return Err(bad_request(
            ErrorCode::OnlyOfficePublicHostRequired,
            "Cannot determine public host. Configure general.publicBaseUrl in Settings.",
        ));
    }
    let base_url = normalize_http_base_url(&base_url, "general.publicBaseUrl")
        .map_err(|_| bad_request(
            ErrorCode::OnlyOfficeInvalidUrl,
            "general.publicBaseUrl must be a valid http(s) URL",
        ))?;

    // Extract caller's bearer JWT for the document URL (OnlyOffice Document Server
    // fetches without credentials — needs access_token query per RFC 6750 §2.3).
    let caller_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ").map(|t| t.trim()))
        .filter(|t| !t.is_empty());

    let document_url = if let Some(ref t) = caller_token {
        format!(
            "{}/api/files/serve?path={}&access_token={}",
            base_url.trim_end_matches('/'),
            url_encode(raw_path),
            url_encode(t)
        )
    } else {
        format!(
            "{}/api/files/serve?path={}",
            base_url.trim_end_matches('/'),
            url_encode(raw_path)
        )
    };
    let callback_state_token = if let Some(ref s) = secret {
        let now = chrono::Utc::now().timestamp() as usize;
        Some(encode(
            &Header::new(Algorithm::HS256),
            &json!({ "path": resolved.to_string_lossy(), "key": key, "iat": now, "exp": now + 86400 }),
            &EncodingKey::from_secret(s.as_bytes()),
        )
        .map_err(|e| AppError::internal(format!("jwt sign: {e}")))?)
    } else {
        None
    };
    let mut callback_params = format!("path={}", url_encode(raw_path));
    if let Some(ref t) = callback_state_token {
        callback_params.push_str(&format!("&state={}", url_encode(t)));
    }
    let callback_url = format!(
        "{}/api/onlyoffice/callback?{}",
        base_url.trim_end_matches('/'),
        callback_params
    );

    // 8. Assemble the editor config payload.
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

    // 9. Sign the config with JWT if a secret is configured.
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
// OnlyOffice save callback — parity with TS handleCallback. Public endpoint
// (no JWT from frontend; OnlyOffice Document Server calls it directly).
//
// Security model:
// 1. onlyofficeJwtSecret must be configured (edit mode enforces this).
// 2. State token (signed path+key) verified.
// 3. OnlyOffice callback JWT (body.token or Authorization header) verified.
// 4. Download URL origin must match configured onlyofficeUrl (anti-SSRF).
// 5. File path validated against workspace roots.
// 6. Atomic write (temp file + rename, size limit, timeout).

const SAVE_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_SAVE_BYTES: u64 = 104_857_600; // 100 MB

#[derive(Deserialize, Default)]
pub struct CallbackBody {
    pub status: Option<i64>,
    pub url: Option<String>,
    pub key: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub path: Option<String>,
    pub state: Option<String>, // signed JWT state token
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

pub async fn handle_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
    Json(body): Json<CallbackBody>,
) -> Result<Json<Value>, AppError> {
    // Acknowledge non-save statuses immediately.
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

    // Wrap the real work in an async block; any error → {error: 1}.
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

    // 1. JWT secret must be configured.
    let secret = reader
        .get_string("general.onlyofficeJwtSecret")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request(ErrorCode::OnlyOfficeJwtRequired, "JWT secret not configured"))?;

    // 2. Verify callback state token (signed path + key in query ?state=).
    let _state_payload = verify_callback_state(q.state.as_deref(), &secret, file_path, body.key.as_deref())?;

    // 3. Verify OnlyOffice callback JWT (body.token or Authorization header).
    let oo_token = body
        .token
        .as_deref()
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer ").or(Some(h)))
        });
    verify_onlyoffice_token(oo_token, &secret)?;

    // 4. Validate download URL origin against configured OnlyOffice server.
    let onlyoffice_url = reader
        .get_string("general.onlyofficeUrl")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request(ErrorCode::OnlyOfficeNotConfigured, "OnlyOffice URL not configured"))?;
    let download_url = body.url.as_deref().unwrap_or("");
    validate_download_url(download_url, &onlyoffice_url)?;

    // 5. Validate file path against workspace roots.
    let resolved = files::resolve_safe_path(state, file_path).await?;

    // 6. Fetch with timeout + size limit, write atomically.
    let max_bytes = reader
        .get_number("general.onlyofficeSaveMaxBytes")
        .map(|n| n as u64)
        .unwrap_or(DEFAULT_MAX_SAVE_BYTES);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(SAVE_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::internal(format!("http client: {e}")))?;

    let response = client
        .get(download_url)
        .send()
        .await
        .map_err(|e| AppError::internal(format!("download failed: {e}")))?;

    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "OnlyOffice download HTTP {}",
            response.status()
        )));
    }

    // Check Content-Length header against limit.
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            return Err(bad_request(
                ErrorCode::OnlyOfficeSaveTooLarge,
                "OnlyOffice save payload exceeds size limit",
            ));
        }
    }

    // Stream body to temp file with byte counter.
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

    // Collect all bytes (up to max_bytes; typical Office docs are <100MB).
    // For very large files, a streaming approach with reqwest stream feature
    // would be more memory-efficient, but the size limit guards this path.
    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::internal(format!("download: {e}")))?;
    if bytes.len() as u64 > max_bytes {
        return Err(bad_request(
            ErrorCode::OnlyOfficeSaveTooLarge,
            "OnlyOffice save payload exceeds size limit",
        ));
    }
    file.write_all(&bytes)
        .await
        .map_err(|e| AppError::internal(format!("write: {e}")))?;
    file.flush()
        .await
        .map_err(|e| AppError::internal(format!("flush: {e}")))?;
    Ok(())
}

/// Verify the signed callback state token (JWT with {path, key}).
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
    // OnlyOffice callback JWT may not carry exp (TS verify doesn't require it).
    v.required_spec_claims.clear();
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

/// Verify the OnlyOffice callback JWT (body.token or Authorization header).
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

/// Validate download URL origin matches the configured OnlyOffice server (anti-SSRF).
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
    // Compare origin (scheme + host + port).
    if download.origin() != allowed.origin() {
        return Err(bad_request(
            ErrorCode::OnlyOfficeDownloadUrlOriginMismatch,
            "Download URL origin does not match configured OnlyOffice server",
        ));
    }
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────

fn normalize_http_base_url(raw: &str, _label: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|_| "invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("unsupported protocol: {}", url.scheme()));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn url_encode(s: &str) -> String {
    // Minimal percent-encoding for query values.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

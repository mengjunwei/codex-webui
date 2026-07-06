//! OnlyOffice Docs integration — editor config builder (parity with
//! `onlyoffice.controller.ts:getConfig`).
//!
//! Edit mode (default) requires `general.onlyofficeJwtSecret` so the save
//! callback can be securely verified. The callback endpoint itself
//! (`/api/onlyoffice/callback`) needs HTTP download + atomic write —
//! deferred until files service gains multipart/streaming support.

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", raw_path, mtime, size).as_bytes());
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

    let document_url = format!(
        "{}/api/files/serve?path={}",
        base_url.trim_end_matches('/'),
        url_encode(raw_path)
    );
    let callback_state_token = if let Some(ref s) = secret {
        Some(encode(
            &Header::new(Algorithm::HS256),
            &json!({ "path": raw_path, "key": key }),
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
// Stub for now: returns 500 "deferred". The full implementation needs
// fetch + atomic write + JWT state/callback verification + origin check
// (parity with TS onlyoffice.controller.ts:handleCallback). The signature
// here matches the route registration so the frontend gets a 500 (not 404).
pub async fn handle_callback(
    State(_state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    Err(AppError::internal(
        "OnlyOffice callback requires fetch + atomic write (deferred)".into(),
    ))
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

//! Chat attachment upload — saves browser-uploaded images to a Codex-readable
//! staging directory. Parity with `src/chat/chat-upload.service.ts`.
//!
//! Files are saved to `{CODEX_HOME}/webui-uploads/{uuid}.{ext}` with a
//! size limit from `files.uploadMaxBytes`. Returns `{ path, size, mimeType }`.

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use std::path::PathBuf;

const CHAT_UPLOAD_DIR_NAME: &str = "webui-uploads";
const MAX_EXTENSION_LENGTH: usize = 32;

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

/// POST /api/chat/upload — single-file multipart attachment.
pub async fn upload_attachment(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, AppError> {
    let max_bytes = state.settings_reader().get_upload_max_bytes();
    let upload_root = ensure_upload_root()?;

    // Read exactly one file part (field name "file").
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue; // skip non-file parts
        }
        let raw_filename = field.file_name().unwrap_or("upload").to_string();
        let mime_type = field.content_type().unwrap_or("application/octet-stream").to_string();

        // Validate filename.
        let filename = raw_filename.trim();
        if filename.is_empty() {
            return Err(bad_request(ErrorCode::ChatFilenameRequired, "filename is required"));
        }
        if filename.contains('/') || filename.contains('\\') || filename.contains('\0') {
            return Err(bad_request(ErrorCode::ChatFileInvalid, "filename must not contain path separators"));
        }

        // Sanitize: strip path components (defense in depth).
        let safe_name = std::path::Path::new(filename)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".to_string());

        let extension = get_safe_extension(&safe_name);
        let id = uuid::Uuid::new_v4();
        let target_path = upload_root.join(format!("{id}{extension}"));
        let tmp_path = upload_root.join(format!(".{id}.tmp"));

        // Read field data with size limit.
        let data = field.bytes().await
            .map_err(|e| AppError::internal(format!("read multipart: {e}")))?;
        if data.len() as u64 > max_bytes {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(AppError::business(
                ErrorCode::FilesFileTooLarge,
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Uploaded file exceeds maximum size ({max_bytes} bytes)"),
                None,
            ));
        }

        // Atomic write: temp → rename.
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| AppError::internal(format!("write tmp: {e}")))?;
        tokio::fs::rename(&tmp_path, &target_path)
            .await
            .map_err(|e| AppError::internal(format!("rename: {e}")))?;

        let size = data.len();
        tracing::info!(path = %target_path.display(), size, "chat upload saved");

        return Ok(Json(json!({
            "path": target_path.to_string_lossy(),
            "size": size,
            "mimeType": mime_type,
        })));
    }

    Err(bad_request(ErrorCode::ChatFileRequired, "Uploaded file is required"))
}

/// Resolve the upload root directory: `{CODEX_HOME}/webui-uploads/`.
fn ensure_upload_root() -> Result<PathBuf, AppError> {
    let codex_home = std::env::var("CODEX_HOME").ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let base = codex_home
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".codex")
        });
    let upload_root = base.join(CHAT_UPLOAD_DIR_NAME);
    std::fs::create_dir_all(&upload_root)
        .map_err(|e| AppError::internal(format!("create upload dir: {e}")))?;
    Ok(upload_root)
}

/// Extract a safe file extension (including the dot), capped at MAX_EXTENSION_LENGTH.
fn get_safe_extension(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("");
    if ext.is_empty() || ext.len() > MAX_EXTENSION_LENGTH {
        return String::new();
    }
    // Only allow alphanumeric extension.
    if !ext.chars().all(|c| c.is_alphanumeric()) {
        return String::new();
    }
    format!(".{}", ext.to_ascii_lowercase())
}

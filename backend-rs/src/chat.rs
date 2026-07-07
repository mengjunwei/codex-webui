//! 聊天附件上传 —— 将浏览器上传的图片保存到一个 Codex 可读取的
//! 暂存目录中。与 `src/chat/chat-upload.service.ts` 对齐。
//!
//! 文件保存到 `{CODEX_HOME}/webui-uploads/{uuid}.{ext}`，
//! 大小上限取自 `files.uploadMaxBytes`。返回 `{ path, size, mimeType }`。

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

/// POST /api/chat/upload —— 单文件 multipart 附件上传。
pub async fn upload_attachment(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, AppError> {
    let max_bytes = state.settings_reader().get_upload_max_bytes();
    let upload_root = ensure_upload_root()?;

    // 只读取一个文件 part（字段名为 "file"）。
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue; // 跳过非文件 part
        }
        let raw_filename = field.file_name().unwrap_or("upload").to_string();
        let mime_type = field.content_type().unwrap_or("application/octet-stream").to_string();

        // 校验文件名。
        let filename = raw_filename.trim();
        if filename.is_empty() {
            return Err(bad_request(ErrorCode::ChatFilenameRequired, "filename is required"));
        }
        if filename.contains('/') || filename.contains('\\') || filename.contains('\0') {
            return Err(bad_request(ErrorCode::ChatFileInvalid, "filename must not contain path separators"));
        }

        // 净化：剔除路径成分（纵深防御）。
        let safe_name = std::path::Path::new(filename)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload".to_string());

        let extension = get_safe_extension(&safe_name);
        let id = uuid::Uuid::new_v4();
        let target_path = upload_root.join(format!("{id}{extension}"));
        let tmp_path = upload_root.join(format!(".{id}.tmp"));

        // 读取字段数据并施加大小限制。
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

        // 原子写入：临时文件 → 重命名。
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

/// 解析上传根目录：`{CODEX_HOME}/webui-uploads/`。
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

/// 提取安全的文件扩展名（包含点号），长度上限为 MAX_EXTENSION_LENGTH。
fn get_safe_extension(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("");
    if ext.is_empty() || ext.len() > MAX_EXTENSION_LENGTH {
        return String::new();
    }
    // 仅允许字母数字组成的扩展名。
    if !ext.chars().all(|c| c.is_alphanumeric()) {
        return String::new();
    }
    format!(".{}", ext.to_ascii_lowercase())
}

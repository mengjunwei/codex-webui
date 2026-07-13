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
};
use crate::error::Json;
use serde_json::{json, Value};
use std::path::PathBuf;

const CHAT_UPLOAD_DIR_NAME: &str = "webui-uploads";
const MAX_EXTENSION_LENGTH: usize = 32;

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}

/// POST /api/chat/upload 成功响应 —— 已暂存的上传附件信息。
#[derive(serde::Serialize, utoipa::ToSchema)]
#[allow(non_snake_case)]
pub struct ChatUploadResponse {
    /// 已暂存文件的绝对路径（Codex app-server 可读取）。
    pub path: String,
    /// 文件大小（字节）。
    pub size: i64,
    /// multipart 请求报告的 MIME 类型。
    pub mimeType: String,
}

/// POST /api/chat/upload —— 单文件 multipart 附件上传。
#[utoipa::path(
    post,
    path = "/api/chat/upload",
    tag = "chat",
    responses(
        (status = 200, description = "上传成功 {path, size, mimeType}。请求体为 multipart/form-data，字段名 file", body = ChatUploadResponse),
        (status = 400, description = "文件缺失/文件名非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 413, description = "超出上传上限", body = crate::error::ErrorResponse),
    )
)]
pub async fn upload_attachment(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, AppError> {
    let max_bytes = state.settings_reader().get_upload_max_bytes();
    let upload_root = ensure_upload_root()?;
    // 周期性清理超过 TTL 的陈旧上传（对齐 TS，节流到每小时一次）。
    // 用 spawn_blocking 包裹同步目录遍历，避免阻塞 tokio worker。
    {
        let sweep_root = upload_root.clone();
        let _ = tokio::task::spawn_blocking(move || maybe_sweep_uploads(&sweep_root)).await;
    }

    // 只读取一个文件 part（字段名为 "file"）。
    while let Ok(Some(mut field)) = multipart.next_field().await {
        if field.name() != Some("file") {
            continue; // 跳过非文件 part
        }
        let raw_filename = field.file_name().unwrap_or("upload").to_string();
        let mime_type = field
            .content_type()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string();

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

        // 流式写入临时文件 + 累计字节（对齐 TS pipeline；避免大文件全量缓冲导致内存峰值）。
        // 一次性以 tokio::fs 打开（unix 设 0o600，对齐 TS mode:0o600），避免原来的
        // "std 创建 + tokio 重新打开"两次 open，并在 async 上下文中改用异步 IO。
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
        }
        let mut tmp = opts
            .open(&tmp_path)
            .await
            .map_err(|e| AppError::internal(format!("open tmp: {e}")))?;
        let mut total: u64 = 0;
        let mut oversize = false;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::internal(format!("read multipart: {e}")))?
        {
            total = total.saturating_add(chunk.len() as u64);
            if total > max_bytes {
                oversize = true;
                break;
            }
            use tokio::io::AsyncWriteExt;
            tmp.write_all(&chunk)
                .await
                .map_err(|e| AppError::internal(format!("write tmp: {e}")))?;
        }
        drop(tmp);
        if oversize {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(AppError::business(
                ErrorCode::FilesUploadTooLarge,
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Uploaded file exceeds maximum size ({max_bytes} bytes)"),
                None,
            ));
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, &target_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(AppError::internal(format!("rename: {e}")));
        }

        let size = total;
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
    // unix 下以 0o700 创建上传目录（对齐 TS mode:0o700）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&upload_root)
            .map_err(|e| AppError::internal(format!("create upload dir: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(&upload_root)
            .map_err(|e| AppError::internal(format!("create upload dir: {e}")))?;
    }
    Ok(upload_root)
}

/// 校验 localImage 路径必须位于 chat 上传根目录 `{CODEX_HOME}/webui-uploads/` 之内
/// （对齐 TS `ChatUploadService.resolveStoredUploadPath`）：解析符号链接逃逸，
/// 确保真实路径在上传根内、且为普通文件。返回规范化后的路径。
/// 失败映射到 chat.* 错误码（image_path_absolute / upload_not_found /
/// image_outside_root / image_not_file）。
pub(crate) async fn resolve_stored_upload_path(path: &str) -> Result<PathBuf, AppError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(bad_request(ErrorCode::ChatImagePathRequired, "image path is required"));
    }
    let parsed = std::path::Path::new(trimmed);
    if !parsed.is_absolute() {
        return Err(bad_request(ErrorCode::ChatImageAbsolutePath, "image path must be absolute"));
    }
    let canonical = match tokio::fs::canonicalize(parsed).await {
        Ok(c) => c,
        Err(_) => {
            return Err(AppError::business(
                ErrorCode::ChatUploadNotFound,
                StatusCode::NOT_FOUND,
                "uploaded file not found".into(),
                None,
            ));
        }
    };
    let upload_root = ensure_upload_root()?;
    if !canonical.starts_with(&upload_root) {
        return Err(bad_request(ErrorCode::ChatImageOutsideRoot, "image path is outside the upload root"));
    }
    let meta = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| AppError::internal(format!("stat upload: {e}")))?;
    if !meta.is_file() {
        return Err(bad_request(ErrorCode::ChatImageNotFile, "image path is not a file"));
    }
    Ok(canonical)
}

const CHAT_UPLOAD_TTL_SECS: u64 = 24 * 3600;
const CHAT_UPLOAD_SWEEP_INTERVAL_SECS: u64 = 3600;
static LAST_SWEEP_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 周期性清理超过 24h 的上传文件（对齐 TS 的 TTL 清理，节流到每小时一次）。
fn maybe_sweep_uploads(root: &std::path::Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_SWEEP_SECS.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < CHAT_UPLOAD_SWEEP_INTERVAL_SECS {
        return;
    }
    LAST_SWEEP_SECS.store(now, std::sync::atomic::Ordering::Relaxed);
    let cutoff = now.saturating_sub(CHAT_UPLOAD_TTL_SECS);
    if let Ok(mut entries) = std::fs::read_dir(root) {
        while let Some(Ok(entry)) = entries.next() {
            let stale = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// 提取安全的文件扩展名（包含点号），长度上限为 MAX_EXTENSION_LENGTH。
fn get_safe_extension(filename: &str) -> String {
    let trimmed = filename.trim();
    // M4：对齐 TS path.extname —— 取最后一个 '.'；无点或点在首位（dotfile）→ 无扩展名。
    // 原用 rsplit('.').next() 会把无点/点开头文件名整个当扩展名（"README"→"README"、
    // ".bashrc"→"bashrc"），与 TS 落盘行为不符。
    let dot_idx = match trimmed.rfind('.') {
        Some(0) | None => return String::new(),
        Some(i) => i,
    };
    let ext = &trimmed[dot_idx + 1..];
    // 用字符计数（与下方 chars 校验一致），避免多字节扩展名按字节长度误判。
    if ext.is_empty() || ext.chars().count() > MAX_EXTENSION_LENGTH {
        return String::new();
    }
    // 对齐 TS 正则 ^\.[A-Za-z0-9][A-Za-z0-9._-]*$：首字符须为字母数字，
    // 其余可含 . _ -（如 "tar.gz"、"HR_png"）。
    let mut chars = ext.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return String::new(),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return String::new();
    }
    format!(".{}", ext)
}

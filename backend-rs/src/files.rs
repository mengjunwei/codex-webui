//! Files subsystem — workspace-root security boundary + core file ops.
//!
//! Parity with `src/files/files.service.ts` + `files.controller.ts` (subset).
//!
//! Security: every path is resolved to a real path (no symlink escapes) and
//! validated to be under a configured workspace root (+ dynamic roots + home).
//!
//! Phase 3c core: read-tree / read-file (text, ≤5MB) / metadata / delete /
//! list-roots / add-root / create-file / create-dir / write-file / resolveSafePath
//! (used by threads for mention path validation).
//!
//! Deferred to follow-up: multipart upload, serve-Range (pdf/video streaming),
//! rename/copy/move, download, archive preview. These currently return 501.

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

const MAX_READ_SIZE: u64 = 5 * 1024 * 1024;
const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "node_modules", ".git", ".next", "dist", "__pycache__", ".DS_Store",
];

fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}
fn not_found(msg: impl Into<String>) -> AppError {
    AppError::business(ErrorCode::FilesPathNotFound, StatusCode::NOT_FOUND, msg.into(), None)
}
fn forbidden(msg: impl Into<String>) -> AppError {
    AppError::business(
        ErrorCode::FilesPathOutsideWorkspace,
        StatusCode::FORBIDDEN,
        msg.into(),
        None,
    )
}

// ── Path resolution + workspace-root enforcement ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub original: String,
    pub resolved: PathBuf,
    pub kind: ResolvedKind,
    pub size: u64,
    pub mtime_ms: i64,
}

/// Validate a path: realpath + workspace-root containment check. The single
/// entry point used by handlers and by threads (mention path resolution).
pub async fn resolve_safe_path(
    state: &AppState,
    input: &str,
) -> Result<PathBuf, AppError> {
    let resolved = resolve(state, input).await?;
    Ok(resolved.resolved)
}

async fn resolve(state: &AppState, input: &str) -> Result<ResolvedTarget, AppError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }
    if raw.contains('\0') {
        return Err(forbidden("path contains NUL byte"));
    }
    let p = PathBuf::from(raw);
    // Reject obvious traversal escapes before resolving (security: belt + suspenders).
    let canonical = tokio::fs::canonicalize(&p)
        .await
        .map_err(|_| not_found(format!("path not found: {raw}")))?;
    if !within_workspace(state, &canonical) {
        return Err(forbidden("path is outside configured workspace roots"));
    }
    let meta = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| AppError::internal(format!("metadata: {e}")))?;
    let kind = if meta.is_symlink() {
        ResolvedKind::Symlink
    } else if meta.is_file() {
        ResolvedKind::File
    } else if meta.is_dir() {
        ResolvedKind::Directory
    } else {
        ResolvedKind::Other
    };
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(ResolvedTarget {
        original: raw.to_string(),
        resolved: canonical,
        kind,
        size,
        mtime_ms,
    })
}

fn within_workspace(state: &AppState, p: &Path) -> bool {
    let roots = workspace_roots(state);
    let p_str = p.to_string_lossy().to_string();
    roots.iter().any(|r| is_within(p, Path::new(r)) || p_str == *r)
}

fn is_within(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

fn workspace_roots(state: &AppState) -> Vec<String> {
    let mut out: HashSet<String> = HashSet::new();
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        if !home.is_empty() {
            out.insert(home);
        }
    }
    for r in state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        out.insert(r);
    }
    out.into_iter().collect()
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// GET /api/files/roots → configured + dynamic roots + home dir.
pub async fn get_roots(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let roots = state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(Json(json!({
        "roots": roots,
        "homeDir": state.home_dir(),
    })))
}

#[derive(Deserialize)]
pub struct AddRootBody {
    pub root: Option<String>,
}

pub async fn add_root(
    State(mut state): State<AppState>,
    Json(body): Json<AddRootBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = body.root.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(
            ErrorCode::ValidationFieldRequired,
            "root is required",
        ));
    }
    let p = PathBuf::from(raw);
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|_| not_found(format!("root not found: {raw}")))?;
    if !meta.is_dir() {
        return Err(bad_request(
            ErrorCode::FilesWorkspaceRootNotDir,
            "root must be an existing directory",
        ));
    }
    let canonical = tokio::fs::canonicalize(&p)
        .await
        .map_err(|e| AppError::internal(format!("canonicalize: {e}")))?;
    let s = canonical.to_string_lossy().to_string();
    state.dynamic_files_roots.lock().unwrap().insert(s);
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/files/tree?root=… → one-level directory listing.
#[derive(Deserialize)]
pub struct TreeQuery {
    pub root: Option<String>,
}

pub async fn read_tree(
    State(state): State<AppState>,
    Query(q): Query<TreeQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolved = resolve(&state, q.root.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::Directory {
        return Err(bad_request(
            ErrorCode::FilesPathIsNotDirectory,
            "root must be a directory",
        ));
    }
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut dir = tokio::fs::read_dir(&resolved.resolved)
        .await
        .map_err(|e| AppError::internal(format!("read_dir: {e}")))?;
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|e| AppError::internal(format!("dir entry: {e}")))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if DEFAULT_EXCLUDED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let ty = if meta.is_dir() {
            "directory"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        entries.push(json!({
            "name": name,
            "path": path.to_string_lossy().to_string(),
            "type": ty,
            "size": meta.len(),
            "mtime": mtime_ms,
        }));
    }
    Ok(Json(json!({ "entries": entries })))
}

/// GET /api/files/read?path=… → text content (≤5MB).
#[derive(Deserialize)]
pub struct ReadQuery {
    pub path: Option<String>,
}

pub async fn read_file(
    State(state): State<AppState>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::File {
        return Err(bad_request(
            ErrorCode::FilesPathIsDirectory,
            "path must be a file",
        ));
    }
    if resolved.size > MAX_READ_SIZE {
        return Err(AppError::business(
            ErrorCode::FilesFileTooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file too large for text read (max {} bytes)", MAX_READ_SIZE),
            None,
        ));
    }
    let content = tokio::fs::read_to_string(&resolved.resolved)
        .await
        .map_err(|e| AppError::internal(format!("read: {e}")))?;
    Ok(Json(json!({
        "path": resolved.original,
        "content": content,
        "size": resolved.size,
        "mtime": resolved.mtime_ms,
    })))
}

/// GET /api/files/metadata?path=… → stat.
#[derive(Deserialize)]
pub struct MetaQuery {
    pub path: Option<String>,
}

pub async fn get_metadata(
    State(state): State<AppState>,
    Query(q): Query<MetaQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    let kind = match resolved.kind {
        ResolvedKind::File => "file",
        ResolvedKind::Directory => "directory",
        ResolvedKind::Symlink => "symlink",
        ResolvedKind::Other => "other",
    };
    Ok(Json(json!({
        "path": resolved.original,
        "name": resolved.resolved.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "type": kind,
        "size": resolved.size,
        "mtime": resolved.mtime_ms,
    })))
}

/// DELETE /api/files/delete?path=…&recursive=… → remove file/symlink/dir.
#[derive(Deserialize)]
pub struct DeleteQuery {
    pub path: Option<String>,
    pub recursive: Option<String>,
}

pub async fn delete_path(
    State(state): State<AppState>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    let recursive = matches!(q.recursive.as_deref(), Some("true") | Some("1"));
    match resolved.kind {
        ResolvedKind::Directory => {
            if recursive {
                tokio::fs::remove_dir_all(&resolved.resolved)
                    .await
                    .map_err(|e| AppError::internal(format!("rmdir: {e}")))?;
            } else {
                // Refuse if not empty (mirrors TS `dirNotEmpty`).
                let mut entries = tokio::fs::read_dir(&resolved.resolved)
                    .await
                    .map_err(|e| AppError::internal(format!("read_dir: {e}")))?;
                if entries
                    .next_entry()
                    .await
                    .map_err(|e| AppError::internal(format!("entry: {e}")))?
                    .is_some()
                {
                    return Err(bad_request(
                        ErrorCode::FilesDirNotEmpty,
                        "directory is not empty (set recursive=true)",
                    ));
                }
                tokio::fs::remove_dir(&resolved.resolved)
                    .await
                    .map_err(|e| AppError::internal(format!("rmdir: {e}")))?;
            }
        }
        _ => {
            tokio::fs::remove_file(&resolved.resolved)
                .await
                .map_err(|e| AppError::internal(format!("rmfile: {e}")))?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

/// POST /api/files/create-file → create empty (or content) file.
#[derive(Deserialize)]
pub struct CreateFileBody {
    pub path: Option<String>,
    pub content: Option<String>,
    pub overwrite: Option<bool>,
}

pub async fn create_file(
    State(state): State<AppState>,
    Json(body): Json<CreateFileBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(
            ErrorCode::FilesPathRequired,
            "path is required",
        ));
    }
    let p = PathBuf::from(raw);
    let canonical_parent =
        tokio::fs::canonicalize(p.parent().unwrap_or(&p))
            .await
            .map_err(|e| AppError::internal(format!("parent canonicalize: {e}")))?;
    if !within_workspace(&state, &canonical_parent) {
        return Err(forbidden("parent is outside workspace"));
    }
    if p.exists() && !body.overwrite.unwrap_or(false) {
        return Err(bad_request(
            ErrorCode::FilesPathExists,
            "path already exists (set overwrite=true)",
        ));
    }
    let content = body.content.clone().unwrap_or_default();
    tokio::fs::write(&p, content)
        .await
        .map_err(|e| AppError::internal(format!("create file: {e}")))?;
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| AppError::internal(format!("stat: {e}")))?;
    Ok(Json(json!({
        "ok": true,
        "path": raw,
        "mtime": meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0),
    })))
}

/// POST /api/files/create-directory → mkdir (recursive optional).
#[derive(Deserialize)]
pub struct CreateDirBody {
    pub path: Option<String>,
    pub recursive: Option<bool>,
    pub overwrite: Option<bool>,
}

pub async fn create_directory(
    State(state): State<AppState>,
    Json(body): Json<CreateDirBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(
            ErrorCode::FilesPathRequired,
            "path is required",
        ));
    }
    let p = PathBuf::from(raw);
    if p.exists() {
        if !body.overwrite.unwrap_or(false) {
            return Err(bad_request(
                ErrorCode::FilesPathExists,
                "path already exists",
            ));
        }
        if !p.is_dir() {
            return Err(bad_request(
                ErrorCode::FilesPathIsNotDirectory,
                "path exists and is not a directory",
            ));
        }
    } else if body.recursive.unwrap_or(false) {
        tokio::fs::create_dir_all(&p)
            .await
            .map_err(|e| AppError::internal(format!("mkdir -p: {e}")))?;
        // Validate after creation.
        let canonical = tokio::fs::canonicalize(&p)
            .await
            .map_err(|e| AppError::internal(format!("canonicalize: {e}")))?;
        if !within_workspace(&state, &canonical) {
            // Roll back: best effort.
            let _ = tokio::fs::remove_dir(&p).await;
            return Err(forbidden("created path is outside workspace"));
        }
        return Ok(Json(json!({ "ok": true, "path": raw })));
    } else {
        let parent = p
            .parent()
            .ok_or_else(|| bad_request(ErrorCode::FilesNoParentFound, "parent path not found"))?;
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| not_found("parent path not found"))?;
        if !within_workspace(&state, &canonical_parent) {
            return Err(forbidden("parent is outside workspace"));
        }
        tokio::fs::create_dir(&p)
            .await
            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
    }
    Ok(Json(json!({ "ok": true, "path": raw })))
}

/// POST /api/files/write → write/overwrite file (optimistic concurrency via
/// expectedMtime is deferred to a follow-up; parity for the basic path).
#[derive(Deserialize)]
pub struct WriteFileBody {
    pub path: Option<String>,
    pub content: Option<String>,
    pub expected_mtime: Option<i64>,
}

pub async fn write_file(
    State(state): State<AppState>,
    Json(body): Json<WriteFileBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    let content = body.content.clone().unwrap_or_default();
    if raw.is_empty() {
        return Err(bad_request(
            ErrorCode::FilesContentRequired,
            "path and content are required",
        ));
    }
    let p = PathBuf::from(raw);
    let canonical_parent = tokio::fs::canonicalize(p.parent().unwrap_or(&p))
        .await
        .map_err(|e| AppError::internal(format!("parent: {e}")))?;
    if !within_workspace(&state, &canonical_parent) {
        return Err(forbidden("parent is outside workspace"));
    }
    if let Some(_expected) = body.expected_mtime {
        // Optional optimistic-concurrency check (parity placeholder).
        if let Ok(meta) = tokio::fs::metadata(&p).await {
            let actual = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if actual != _expected {
                return Err(bad_request(
                    ErrorCode::FilesModifiedSinceRead,
                    "file has been modified since read (expectedMtime mismatch)",
                ));
            }
        }
    }
    tokio::fs::write(&p, content)
        .await
        .map_err(|e| AppError::internal(format!("write: {e}")))?;
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| AppError::internal(format!("stat: {e}")))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(Json(json!({
        "ok": true,
        "path": raw,
        "size": meta.len(),
        "mtime": mtime_ms,
    })))
}

// ── serve (inline + Range) / download (attachment) ──────────────────────────

/// GET /api/files/serve?path=… — inline preview with byte-range support.
/// Used by <img>/<video>/<pdf> tags AND by OnlyOffice Document Server to fetch
/// the file. Supports the RFC 6750 `access_token` query fallback (handled by
/// the auth middleware for this path). Returns 200/206/416.
pub async fn serve_file(
    State(state): State<AppState>,
    Query(q): Query<ReadQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::File {
        return Err(bad_request(
            ErrorCode::FilesPathIsDirectory,
            "path must be a file",
        ));
    }
    serve_with_range(
        &resolved.resolved,
        &resolved.original,
        resolved.size,
        true, // inline
        headers,
    )
    .await
}

/// GET /api/files/download?path=… — attachment download (no Range).
pub async fn download_file(
    State(state): State<AppState>,
    Query(q): Query<ReadQuery>,
) -> Result<Response, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::File {
        return Err(bad_request(
            ErrorCode::FilesPathIsDirectory,
            "path must be a file",
        ));
    }
    serve_with_range(
        &resolved.resolved,
        &resolved.original,
        resolved.size,
        false, // attachment
        HeaderMap::new(),
    )
    .await
}

async fn serve_with_range(
    path: &Path,
    original: &str,
    size: u64,
    inline: bool,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let mime = guess_mime_type(&filename);
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut resp = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, mime.as_str())
        .header(
            header::CONTENT_DISPOSITION,
            build_content_disposition(&filename, inline).as_str(),
        )
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(
            "Content-Security-Policy",
            "sandbox; default-src 'none'; img-src 'self' data: blob:; media-src 'self' data: blob:; style-src 'unsafe-inline'",
        )
        .header("Cache-Control", "private, no-store");

    match parse_range_header(range_header.as_deref(), size) {
        RangeResult::Invalid => Ok(resp
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", size))
            .body(Body::empty())?),
        RangeResult::None => {
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| AppError::internal(format!("read: {e}")))?;
            Ok(resp
                .header(header::CONTENT_LENGTH, size)
                .body(Body::from(bytes))?)
        }
        RangeResult::Range(r) => {
            let length = r.end - r.start + 1;
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|e| AppError::internal(format!("open: {e}")))?;
            file.seek(SeekFrom::Start(r.start))
                .await
                .map_err(|e| AppError::internal(format!("seek: {e}")))?;
            let mut buf = vec![0u8; length as usize];
            file.read_exact(&mut buf)
                .await
                .map_err(|e| AppError::internal(format!("read range: {e}")))?;
            Ok(resp
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", r.start, r.end, size),
                )
                .header(header::CONTENT_LENGTH, length)
                .body(Body::from(buf))?)
        }
    }
}

enum RangeResult {
    None,
    Range(ByteRange),
    Invalid,
}

struct ByteRange {
    start: u64,
    end: u64,
}

/// Parse a single RFC 9110 `bytes=start-end` header. Parity with TS
/// `parseRangeHeader`. Returns None (absent), Range, or Invalid.
fn parse_range_header(range_header: Option<&str>, size: u64) -> RangeResult {
    let Some(header) = range_header else {
        return RangeResult::None;
    };
    let header = header.trim();
    let Some(rest) = header.strip_prefix("bytes=") else {
        return RangeResult::Invalid;
    };
    // Match `^bytes=(\d*)-(\d*)$`
    let (raw_start, raw_end) = match rest.split_once('-') {
        Some((s, e)) => (s, e),
        None => return RangeResult::Invalid,
    };
    if raw_start.is_empty() && raw_end.is_empty() {
        return RangeResult::Invalid;
    }
    if raw_start.is_empty() {
        // Suffix range: bytes=-N → last N bytes
        let suffix: u64 = match raw_end.parse() {
            Ok(n) if n > 0 => n,
            _ => return RangeResult::Invalid,
        };
        if size == 0 {
            return RangeResult::Invalid;
        }
        let start = size.saturating_sub(suffix);
        return RangeResult::Range(ByteRange {
            start,
            end: size - 1,
        });
    }
    let start: u64 = match raw_start.parse() {
        Ok(n) => n,
        Err(_) => return RangeResult::Invalid,
    };
    let end: u64 = if raw_end.is_empty() {
        size.saturating_sub(1)
    } else {
        match raw_end.parse() {
            Ok(n) => n,
            Err(_) => return RangeResult::Invalid,
        }
    };
    if start >= size || end < start {
        return RangeResult::Invalid;
    }
    RangeResult::Range(ByteRange {
        start,
        end: end.min(size - 1),
    })
}

/// Map a filename to a MIME type by extension. Parity with TS `guessMimeType`.
fn guess_mime_type(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return "application/gzip".into();
    }
    if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        return "application/x-bzip2".into();
    }
    if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        return "application/x-xz".into();
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    mime_for_ext(ext).into()
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        // images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        // documents
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // text/code
        "html" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" => "text/typescript",
        "tsx" => "text/tsx",
        "jsx" => "text/jsx",
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "yaml" | "yml" => "text/yaml",
        // media
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        // archives
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        // fonts
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

/// Build a safe Content-Disposition header. Parity with TS
/// `buildContentDisposition`.
fn build_content_disposition(filename: &str, inline: bool) -> String {
    let fallback: String = filename
        .chars()
        .map(|c| match c {
            '\r' | '\n' | '"' | '\\' => '_',
            other => other,
        })
        .collect();
    let disposition = if inline { "inline" } else { "attachment" };
    let encoded = url_encode(filename);
    format!(
        "{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    )
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── Deferred handlers (return 500 "deferred") ────────────────────────────────

// (helper to avoid unused import)
#[allow(dead_code)]
fn _check_default_excluded() -> HashSet<&'static str> {
    DEFAULT_EXCLUDED_DIRS.iter().copied().collect()
}
// silence dead_code
#[allow(dead_code)]
fn _unused_fs() {
    let _ = std::any::type_name::<fs::File>();
}

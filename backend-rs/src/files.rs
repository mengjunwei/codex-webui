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
    http::{header, HeaderMap, StatusCode},
    response::Response,
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
    // Home dir — canonicalize to match the verbatim prefix (\\?\) of
    // canonicalized file paths on Windows (fixes C1 from review).
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        if !home.is_empty() {
            match std::fs::canonicalize(&home) {
                Ok(c) => { out.insert(c.to_string_lossy().to_string()); }
                Err(_) => { out.insert(home); }
            }
        }
    }
    // Configured WORKSPACE_ROOTS from settings (fixes H1 from review).
    let reader = crate::settings::SettingsReader::new(&state.db);
    if let Some(roots_str) = reader.get_string("security.workspaceRoots") {
        for root in roots_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Ok(c) = std::fs::canonicalize(root) {
                out.insert(c.to_string_lossy().to_string());
            }
        }
    }
    // Dynamic roots (already canonicalized at add_root time).
    for r in state.dynamic_files_roots.lock().map(|g| g.iter().cloned().collect::<Vec<_>>()).unwrap_or_default() {
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
    // H3 FIX: dynamic root must be within an already-configured root (TS isAllowedPath).
    if !within_workspace(&state, &canonical) {
        return Err(forbidden("root must be within an existing workspace root"));
    }
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
    let raw = q.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }

    // H4 FIX: check for symlinks BEFORE canonicalize (which follows links).
    // A symlink must be removed via remove_file (deletes the link, not target).
    let sym_meta = tokio::fs::symlink_metadata(raw).await
        .map_err(|_| not_found(&format!("path not found: {raw}")))?;
    if sym_meta.is_symlink() {
        // Validate the symlink's parent is within workspace.
        let parent = std::path::Path::new(raw).parent()
            .ok_or_else(|| bad_request(ErrorCode::FilesNoParentFound, "parent path not found"))?;
        let parent_canon = tokio::fs::canonicalize(parent).await
            .map_err(|_| not_found("parent path not found"))?;
        if !within_workspace(&state, &parent_canon) {
            return Err(forbidden("path is outside configured workspace roots"));
        }
        tokio::fs::remove_file(raw).await
            .map_err(|e| AppError::internal(format!("remove symlink: {e}")))?;
        return Ok(Json(json!({ "ok": true })));
    }

    // Normal path: resolve (canonicalize) then delete by type.
    let resolved = resolve(&state, raw).await?;
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
    // H1 FIX: if target exists, canonicalize it and verify within workspace
    // (prevents symlink escape: a symlink in-workspace → target outside).
    if let Ok(canonical_target) = tokio::fs::canonicalize(&p).await {
        if !within_workspace(&state, &canonical_target) {
            return Err(forbidden("target resolves outside workspace (symlink escape?)"));
        }
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
    _original: &str,
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

    let resp = Response::builder()
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

// ── rename / copy / move ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RenameBody {
    pub path: Option<String>,
    #[serde(rename = "newName")]
    pub new_name: Option<String>,
    pub overwrite: Option<bool>,
}

pub async fn rename_path(
    State(state): State<AppState>,
    Json(body): Json<RenameBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }
    let new_name = body.new_name.as_deref().map(|s| s.trim()).unwrap_or("");
    if new_name.is_empty() {
        return Err(bad_request(ErrorCode::FilesNameRequired, "newName is required"));
    }
    if new_name.contains('/') || new_name.contains('\\') {
        return Err(bad_request(ErrorCode::FilesNameInvalid, "newName must not contain path separators"));
    }
    let resolved = resolve(&state, raw).await?;
    let parent = resolved.resolved.parent()
        .ok_or_else(|| bad_request(ErrorCode::FilesNoParentFound, "parent path not found"))?;
    let dest = parent.join(new_name);
    if dest.exists() && !body.overwrite.unwrap_or(false) {
        return Err(conflict(ErrorCode::FilesPathExists, "destination already exists (set overwrite=true)"));
    }
    tokio::fs::rename(&resolved.resolved, &dest)
        .await
        .map_err(|e| AppError::internal(format!("rename: {e}")))?;
    let canonical = tokio::fs::canonicalize(&dest).await
        .map_err(|e| AppError::internal(format!("canonicalize: {e}")))?;
    Ok(Json(json!({
        "ok": true,
        "oldPath": resolved.resolved.to_string_lossy(),
        "newPath": canonical.to_string_lossy(),
    })))
}

#[derive(Deserialize)]
pub struct CopyMoveBody {
    #[serde(rename = "sourcePath")]
    pub source_path: Option<String>,
    #[serde(rename = "destinationPath")]
    pub destination_path: Option<String>,
    pub overwrite: Option<bool>,
}

pub async fn copy_path(
    State(state): State<AppState>,
    Json(body): Json<CopyMoveBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    do_relocate(&state, &body, false).await
}

pub async fn move_path(
    State(state): State<AppState>,
    Json(body): Json<CopyMoveBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    do_relocate(&state, &body, true).await
}

async fn do_relocate(
    state: &AppState,
    body: &CopyMoveBody,
    is_move: bool,
) -> Result<Json<serde_json::Value>, AppError> {
    let src_raw = body.source_path.as_deref().map(|s| s.trim()).unwrap_or("");
    let dst_raw = body.destination_path.as_deref().map(|s| s.trim()).unwrap_or("");
    if src_raw.is_empty() || dst_raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesSourceAndDestRequired,
            "sourcePath and destinationPath are required"));
    }
    let src = resolve(state, src_raw).await?;
    // Validate dest: canonicalize dest (or its parent if dest doesn't exist yet).
    let dst_canonical = match tokio::fs::canonicalize(dst_raw).await {
        Ok(c) => c,
        Err(_) => {
            let parent = std::path::Path::new(dst_raw)
                .parent()
                .unwrap_or(std::path::Path::new(dst_raw));
            tokio::fs::canonicalize(parent).await
                .map_err(|_| not_found("destination parent path not found"))?
        }
    };
    if !within_workspace(state, &dst_canonical) {
        return Err(forbidden("destination is outside workspace"));
    }
    let dest = std::path::PathBuf::from(dst_raw);
    if dest.exists() && !body.overwrite.unwrap_or(false) {
        return Err(conflict(ErrorCode::FilesPathExists,
            "destination already exists (set overwrite=true)"));
    }
    if is_move {
        tokio::fs::rename(&src.resolved, &dest)
            .await
            .map_err(|e| AppError::internal(format!("move: {e}")))?;
    } else if src.kind == ResolvedKind::Directory {
        copy_dir_recursive(&src.resolved, &dest).await
            .map_err(|e| AppError::internal(format!("copy dir: {e}")))?;
    } else {
        tokio::fs::copy(&src.resolved, &dest)
            .await
            .map_err(|e| AppError::internal(format!("copy file: {e}")))?;
    }
    Ok(Json(json!({
        "ok": true,
        "sourcePath": src.resolved.to_string_lossy(),
        "destinationPath": dest.to_string_lossy(),
    })))
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = entry.file_type().await?;
        if meta.is_dir() {
            Box::pin(copy_dir_recursive(&from, &to)).await?;
        } else {
            tokio::fs::copy(&from, &to).await?;
        }
    }
    Ok(())
}

// ── multipart upload ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UploadQuery {
    #[serde(rename = "destinationPath")]
    pub destination_path: Option<String>,
    pub overwrite: Option<String>,
}

pub async fn upload_files(
    State(state): State<AppState>,
    Query(q): Query<UploadQuery>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let dest_raw = q.destination_path.as_deref().map(|s| s.trim()).unwrap_or("");
    if dest_raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesDestRequired, "destinationPath is required"));
    }
    let overwrite = matches!(q.overwrite.as_deref(), Some("true") | Some("1"));
    // Validate destination dir is within workspace.
    let dest_canonical = tokio::fs::canonicalize(dest_raw)
        .await
        .map_err(|_| not_found("destination directory not found"))?;
    if !within_workspace(&state, &dest_canonical) {
        return Err(forbidden("destination is outside workspace"));
    }
    if !dest_canonical.is_dir() {
        return Err(bad_request(ErrorCode::FilesPathIsNotDirectory,
            "destinationPath must be a directory"));
    }

    let mut files = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let raw_filename = field.file_name().unwrap_or("upload").to_string();
        // CRITICAL FIX (C2): sanitize filename — strip path components to prevent
        // path traversal (e.g. "..\\evil.dll" → "evil.dll").
        let safe_name = std::path::Path::new(&raw_filename)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|s| !s.is_empty() && s != "." && s != "..")
            .unwrap_or_else(|| "upload".to_string());
        let data = field.bytes().await
            .map_err(|e| AppError::internal(format!("read multipart field: {e}")))?;
        let file_path = dest_canonical.join(&safe_name);
        if file_path.exists() && !overwrite {
            return Err(conflict(ErrorCode::FilesPathExists,
                format!("{safe_name} already exists (set overwrite=true)")));
        }
        tokio::fs::write(&file_path, &data)
            .await
            .map_err(|e| AppError::internal(format!("write {safe_name}: {e}")))?;
        let size = data.len();
        files.push(json!({ "path": file_path.to_string_lossy(), "size": size }));
    }
    Ok(Json(json!({ "ok": true, "files": files })))
}

// ── archive list / entry (zip + tar/gz/bz2/xz) ──────────────────────────────

pub async fn archive_list(
    State(state): State<AppState>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::File {
        return Err(bad_request(ErrorCode::FilesPathIsDirectory, "archive path must be a file"));
    }
    let path = resolved.resolved.clone();
    let entries = tokio::task::spawn_blocking(move || list_archive_entries(&path))
        .await
        .map_err(|e| AppError::internal(format!("archive task: {e}")))?
        .map_err(|e| AppError::internal(format!("archive list: {e}")))?;
    Ok(Json(json!({ "path": resolved.original, "entries": entries })))
}

pub async fn archive_entry(
    State(state): State<AppState>,
    Query(q): Query<ArchiveEntryQuery>,
) -> Result<Response, AppError> {
    let resolved = resolve(&state, q.path.as_deref().unwrap_or("")).await?;
    if resolved.kind != ResolvedKind::File {
        return Err(bad_request(ErrorCode::FilesPathIsDirectory, "archive path must be a file"));
    }
    let entry_path = q.entry.as_deref().map(|s| s.trim()).unwrap_or("");
    if entry_path.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "entry is required"));
    }
    let path = resolved.resolved.clone();
    let ep = entry_path.to_string();
    let data = tokio::task::spawn_blocking(move || read_archive_entry(&path, &ep))
        .await
        .map_err(|e| AppError::internal(format!("archive task: {e}")))?
        .map_err(|e| AppError::internal(format!("archive read: {e}")))?;

    let mime = guess_mime_type(entry_path);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, mime.as_str())
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            build_content_disposition(
                &std::path::Path::new(entry_path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                true,
            ).as_str(),
        )
        .body(Body::from(data))
        .map_err(|e| AppError::internal(format!("response: {e}")))?)
}

#[derive(Deserialize)]
pub struct ArchiveEntryQuery {
    pub path: Option<String>,
    pub entry: Option<String>,
}

enum ArchiveFormat { Zip, Tar, TarGz, TarBz2, TarXz }

fn detect_format(path: &Path) -> Option<ArchiveFormat> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    if name.ends_with(".zip") { Some(ArchiveFormat::Zip) }
    else if name.ends_with(".tar") { Some(ArchiveFormat::Tar) }
    else if name.ends_with(".tar.gz") || name.ends_with(".tgz") { Some(ArchiveFormat::TarGz) }
    else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") { Some(ArchiveFormat::TarBz2) }
    else if name.ends_with(".tar.xz") || name.ends_with(".txz") { Some(ArchiveFormat::TarXz) }
    else { None }
}

fn list_archive_entries(path: &Path) -> Result<Vec<serde_json::Value>, String> {
    let fmt = detect_format(path).ok_or("unsupported archive format")?;
    match fmt {
        ArchiveFormat::Zip => {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
            let mut entries = Vec::new();
            for i in 0..archive.len() {
                let entry = archive.by_index(i).map_err(|e| e.to_string())?;
                let name = entry.name().to_string();
                let is_dir = entry.is_dir();
                entries.push(json!({
                    "name": name.rsplit('/').next().unwrap_or(&name),
                    "path": name,
                    "type": if is_dir { "directory" } else { "file" },
                    "size": if is_dir { serde_json::Value::Null } else { serde_json::json!(entry.size()) },
                }));
            }
            Ok(entries)
        }
        ArchiveFormat::Tar => list_tar(std::fs::File::open(path).map_err(|e| e.to_string())?),
        ArchiveFormat::TarGz => list_tar(flate2::read::GzDecoder::new(std::fs::File::open(path).map_err(|e| e.to_string())?)),
        ArchiveFormat::TarBz2 | ArchiveFormat::TarXz => {
            // H5 FIX: bz2/xz need decompression crate; return explicit error
            // instead of feeding compressed bytes to tar (which yields garbage 500).
            Err("bz2/xz archive listing not yet supported".into())
        }
    }
}

fn list_tar<R: std::io::Read>(reader: R) -> Result<Vec<serde_json::Value>, String> {
    let mut archive = tar::Archive::new(reader);
    let mut entries = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().to_string();
        let is_dir = entry.header().entry_type().is_dir();
        entries.push(json!({
            "name": path.rsplit('/').next().unwrap_or(&path),
            "path": path,
            "type": if is_dir { "directory" } else { "file" },
            "size": if is_dir { serde_json::Value::Null } else { serde_json::json!(entry.size()) },
        }));
    }
    Ok(entries)
}

fn read_archive_entry(path: &Path, entry_name: &str) -> Result<Vec<u8>, String> {
    let fmt = detect_format(path).ok_or("unsupported archive format")?;
    match fmt {
        ArchiveFormat::Zip => {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
            let mut entry = archive.by_name(entry_name).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).map_err(|e| e.to_string())?;
            Ok(buf)
        }
        ArchiveFormat::Tar => read_tar_entry(std::fs::File::open(path).map_err(|e| e.to_string())?, entry_name),
        ArchiveFormat::TarGz => read_tar_entry(flate2::read::GzDecoder::new(std::fs::File::open(path).map_err(|e| e.to_string())?), entry_name),
        ArchiveFormat::TarBz2 | ArchiveFormat::TarXz => {
            // bz2/xz decoders require additional crates; deferred.
            Err("bz2/xz archive entry extraction not yet supported".into())
        }
    }
}

fn read_tar_entry<R: std::io::Read>(reader: R, entry_name: &str) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().to_string();
        if path == entry_name {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).map_err(|e| e.to_string())?;
            return Ok(buf);
        }
    }
    Err(format!("entry not found: {entry_name}"))
}

#[allow(dead_code)]
fn _unused_fs() {
    let _ = std::any::type_name::<fs::File>();
}

// ── Conflict helper (409 CONFLICT, parity with TS assertNoOverwrite) ─────────
fn conflict(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::CONFLICT, msg.into(), None)
}

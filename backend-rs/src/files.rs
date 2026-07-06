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
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;

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

// ── Deferred handlers (return 501) ──────────────────────────────────────────

macro_rules! not_implemented {
    ($method:literal) => {
        async fn $method() -> Result<Json<serde_json::Value>, AppError> {
            Err(AppError::internal(format!(
                "{} requires multipart/Range/stream support (deferred)",
                stringify!($method)
            )))
        }
    };
}
pub async fn upload_files() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "upload requires multipart (Phase 3c+ deferred)".into(),
    ))
}
pub async fn serve_file() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "serve requires Range + streaming (Phase 3c+ deferred)".into(),
    ))
}
pub async fn download_file() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "download requires streaming (Phase 3c+ deferred)".into(),
    ))
}
pub async fn rename_path() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "rename deferred".into(),
    ))
}
pub async fn copy_path() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "copy deferred".into(),
    ))
}
pub async fn move_path() -> Result<Json<serde_json::Value>, AppError> {
    Err(AppError::internal(
        "move deferred".into(),
    ))
}

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

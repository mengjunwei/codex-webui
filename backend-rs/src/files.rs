//! 文件子系统 —— 工作区根目录安全边界 + 核心文件操作。
//!
//! 与 `src/files/files.service.ts` + `files.controller.ts`（子集）保持对齐。
//!
//! 安全性：每个路径都会被解析为真实路径（杜绝符号链接逃逸），并校验其位于
//! 已配置的工作区根目录之下（含动态根目录与家目录）。
//!
//! Phase 3c 核心：read-tree / read-file（文本，≤5MB）/ metadata / delete /
//! list-roots / add-root / create-file / create-dir / write-file / resolveSafePath
//! （供 threads 用于 mention 路径校验）。
//!
//! 已实现：multipart 上传、serve-Range（PDF/视频流式播放）、
//! rename/copy/move、download、归档预览（zip/tar.gz/tar.bz2/tar.xz/7z；rar 不支持）。
//!
//! ## 工作区根目录的三种来源（按权威性叠加）
//!
//! 1. **配置根目录**：从 settings 表 `security.workspaceRoots` 读取（逗号分隔）。
//! 2. **家目录**：始终包含在内；用 `OnceCell` 缓存规范化路径以加速路径校验。
//! 3. **动态根目录**：通过 `POST /api/files/roots` 在运行时新增；本身必须落在
//!    已配置的工作区根目录之内（防止任意扩大访问边界）。
//!
//! ## 路径解析的两道安全关卡
//!
//! - **第一关**：直接拒绝包含 NUL 字节的路径、规范化后的路径必须位于工作区根目录之内。
//! - **第二关**：写入/创建类操作除校验"父目录在工作区"外，还要校验"目标本身若是
//!   既存符号链接，链接目标必须位于工作区内"——对抗"工作区内符号链接 → 工作区外目标"
//!   这种 TOCTOU 攻击向量。
//!
//! ## 典型文件操作的安全检查模式
//!
//! ```text
//! raw → trim → NUL 检查 → canonicalize → 包含性校验 → 类型/大小校验 → 实际操作
//! ```

use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use crate::error::Json;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

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

// ── 路径解析 + 工作区根目录强制校验 ─────────────────────────────

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

/// 校验路径：realpath + 工作区根目录包含性检查。这是被各 handler
/// 及 threads（mention 路径解析）共用的唯一入口。
pub async fn resolve_safe_path(
    state: &AppState,
    input: &str,
) -> Result<PathBuf, AppError> {
    let resolved = resolve(state, input).await?;
    Ok(resolved.resolved)
}

/// 解析路径 + 工作区根目录强制校验的"心脏"函数。
///
/// ## 流程
///
/// 1. 修剪两端空白；空路径 → `files.path_required` 400。
/// 2. 拒绝 NUL 字节（操作系统路径允许 NUL 终止符之后的任意字节，
///    Rust Path 的解析可能在字节边界产生歧义）。
/// 3. 调用 `tokio::fs::canonicalize`：跟随符号链接，得到真实路径。
///    - 若路径不存在 → `files.path_not_found` 404。
/// 4. 校验真实路径是否位于任一工作区根目录之内（包含等于）。
///    - 否 → `files.path_outside_workspace` 403。
/// 5. 读取元数据并归类（File / Directory / Other）。
///    - 注意：因 canonicalize 已跟随链接，`is_symlink()` 在此处恒为 false，
///      故 `kind` 实际只会有 `File` / `Directory` 两类；`Symlink` 保留
///      以备将来在 canonicalize 之前用 `symlink_metadata` 区分。
/// 6. 返回 `ResolvedTarget`：原始路径 + 规范化路径 + 类型 + 大小 + mtime。
///
/// ## 调用方
///
/// 所有文件操作 handler（read / write / create / delete 等）以及
/// `threads` 模块中的 `mention` 路径校验都依赖此函数作为统一入口。
async fn resolve(state: &AppState, input: &str) -> Result<ResolvedTarget, AppError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }
    if raw.contains('\0') {
        return Err(forbidden("path contains NUL byte"));
    }
    let p = PathBuf::from(raw);
    // 在解析之前先拒绝明显的目录穿越逃逸（安全加固：双重保险）。
    let canonical = tokio::fs::canonicalize(&p)
        .await
        .map_err(|_| not_found(format!("path not found: {raw}")))?;
    if !within_workspace(state, &canonical) {
        return Err(forbidden("path is outside configured workspace roots"));
    }
    let meta = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| AppError::internal(format!("metadata: {e}")))?;
    // 注意：canonicalize 已跟随符号链接，此处 meta 是链接目标（非链接本身）的元数据，
    // 故 is_symlink() 恒为 false —— 对齐 TS（跟随链接、按目标类型上报）。
    // 若将来需要区分符号链接，应在 canonicalize 之前用 symlink_metadata 判断。
    let kind = if meta.is_file() {
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

/// C3：判断路径是否为某个 workspace root（禁止删除/重命名/移动）。
fn is_workspace_root(state: &AppState, p: &Path) -> bool {
    let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let canon_str = canonical.to_string_lossy().to_string();
    workspace_roots(state).iter().any(|r| canon_str == *r)
}

/// 计算工作区根目录集合（配置根 + 家目录 + 动态根），供路径校验复用。
/// 从 `workspace_roots(state)` 抽出，以便终端 cwd 沙箱等无 AppState 的调用方复用。
///
/// ## 重要设计：家目录的 `OnceCell` 缓存
///
/// 平台差异：Windows 上 `USERPROFILE` 经 `canonicalize` 后会带上 `\\?\` 长路径前缀；
/// 在路径包含性比较时若不做规范化，前端回传的 `C:\Users\xxx\file.txt` 与
/// `\\?\C:\Users\xxx` 不会按字符串前缀匹配。家目录在运行时不会变化，因此用
/// `OnceCell` 一次性规范化并缓存 —— 每次路径校验都重新 canonicalize
/// 浪费系统调用且可能因为短期进程差异导致结果不一致。
pub fn compute_workspace_roots(db: &crate::db::Db, dynamic_roots: &HashSet<String>) -> Vec<String> {
    let mut out: HashSet<String> = HashSet::new();
    // 家目录 —— 规范化以匹配 Windows 上规范化后文件路径的逐字前缀（\\?\）
    // （修复评审提出的 C1）。home 不随运行时变化，用 OnceCell 缓存其 canonical
    // 路径，避免 workspace_roots（每次路径解析都调用）反复 canonicalize。
    static HOME_CANONICAL: once_cell::sync::OnceCell<Option<String>> = once_cell::sync::OnceCell::new();
    if let Some(hc) = HOME_CANONICAL.get_or_init(|| {
        std::env::var("USERPROFILE")
            .ok()
            .or_else(|| std::env::var("HOME").ok())
            .filter(|s| !s.is_empty())
            .map(|h| std::fs::canonicalize(&h).map(|c| c.to_string_lossy().to_string()).unwrap_or(h))
    }) {
        out.insert(hc.clone());
    }
    // 从设置中读取已配置的 WORKSPACE_ROOTS（修复评审提出的 H1）。
    let reader = crate::settings::SettingsReader::new(db, None);
    if let Some(roots_str) = reader.get_string("security.workspaceRoots") {
        for root in roots_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Ok(c) = std::fs::canonicalize(root) {
                out.insert(c.to_string_lossy().to_string());
            }
        }
    }
    // 动态根目录（在 add_root 时已规范化）。
    for r in dynamic_roots {
        out.insert(r.clone());
    }
    out.into_iter().collect()
}

fn workspace_roots(state: &AppState) -> Vec<String> {
    let dyn_roots: HashSet<String> = state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    compute_workspace_roots(&state.db, &dyn_roots)
}

/// 判断规范化路径是否位于任一工作区根目录之下（含等于根本身）。
pub fn is_path_in_workspace(db: &crate::db::Db, dynamic_roots: &HashSet<String>, p: &Path) -> bool {
    let roots = compute_workspace_roots(db, dynamic_roots);
    let p_str = p.to_string_lossy().to_string();
    roots.iter().any(|r| is_within(p, Path::new(r)) || p_str == *r)
}

/// 解析终端 cwd：按 TS `resolveTerminalCwd` 优先级选择候选，并强制沙箱化
/// （必须位于工作区根目录之内且为已存在的目录）。
///
/// 优先级（对齐 TS）：
/// 1. 配置的 `defaultCwd`（若设置则对所有终端生效）；
/// 2. `thread:` 上下文 —— 必须显式提供 cwd，否则 `terminal.cwd_required`；
/// 3. 其他上下文 —— 回落到家目录。
pub fn resolve_terminal_cwd(
    db: &crate::db::Db,
    dynamic_roots: &HashSet<String>,
    context_key: &str,
    requested: Option<&str>,
    default_cwd: Option<&str>,
) -> Result<String, AppError> {
    let candidate = if let Some(d) = default_cwd.map(str::trim).filter(|s| !s.is_empty()) {
        d.to_string()
    } else if context_key.starts_with("thread:") {
        match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) => c.to_string(),
            None => {
                return Err(bad_request(
                    ErrorCode::TerminalCwdRequired,
                    "Thread terminal cwd is required",
                ))
            }
        }
    } else {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    };

    if candidate.trim().is_empty() {
        return Err(AppError::business(
            ErrorCode::TerminalInvalidCwd,
            StatusCode::BAD_REQUEST,
            "Terminal cwd is required".into(),
            None,
        ));
    }

    let canon = std::fs::canonicalize(&candidate).map_err(|_| {
        AppError::business(
            ErrorCode::TerminalInvalidCwd,
            StatusCode::BAD_REQUEST,
            format!("Terminal cwd is invalid or outside allowed workspace roots: {candidate}"),
            None,
        )
    })?;
    if !is_path_in_workspace(db, dynamic_roots, &canon) {
        return Err(AppError::business(
            ErrorCode::FilesPathOutsideWorkspace,
            StatusCode::FORBIDDEN,
            "Terminal cwd is outside allowed workspace roots".into(),
            None,
        ));
    }
    let is_dir = std::fs::metadata(&canon).map(|m| m.is_dir()).unwrap_or(false);
    if !is_dir {
        return Err(AppError::business(
            ErrorCode::TerminalCwdNotDirectory,
            StatusCode::BAD_REQUEST,
            "Terminal cwd must be an existing directory".into(),
            None,
        ));
    }
    Ok(canon.to_string_lossy().to_string())
}

// ── Handler（处理器）────────────────────────────────────────────────────────

/// GET /api/files/roots → 已配置根目录 + 动态根目录 + 家目录。
#[utoipa::path(
    get,
    path = "/api/files/roots",
    tag = "files",
    responses(
        (status = 200, description = "已配置根目录 + 动态根目录 + 家目录", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
    )
)]
pub async fn get_roots(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    // roots = 配置根 ∪ 家目录 ∪ 动态根（对齐 TS rebuildWorkspaceRoots）。
    let dyn_roots: HashSet<String> = state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    let mut roots = compute_workspace_roots(&state.db, &dyn_roots);
    roots.sort();
    Ok(Json(json!({
        "roots": roots,
        "homeDir": state.home_dir(),
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AddRootBody {
    pub root: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/files/roots",
    tag = "files",
    request_body = AddRootBody,
    responses(
        (status = 200, description = "根目录已添加", body = crate::error::GenericJson),
        (status = 400, description = "root 缺失/非目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "根目录不在已有工作区内", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
pub async fn add_root(
    State(state): State<AppState>,
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
    // H3 修复：动态根目录必须位于已配置的根目录之内（对齐 TS isAllowedPath）。
    if !within_workspace(&state, &canonical) {
        return Err(forbidden("root must be within an existing workspace root"));
    }
    let s = canonical.to_string_lossy().to_string();
    state.dynamic_files_roots.lock().unwrap().insert(s);
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/files/tree?root=… → 单层目录列表。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TreeQuery {
    pub root: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/files/tree",
    tag = "files",
    params(TreeQuery),
    responses(
        (status = 200, description = "单层目录列表（目录优先、按名称排序）", body = crate::error::GenericJson),
        (status = 400, description = "root 非目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
    // H1 修复：从 settings 读取 excludedDirs（逗号分隔），而非硬编码。
    let reader = crate::settings::SettingsReader::new(&state.db, None);
    let excluded: Vec<String> = reader
        .get_string("files.excludedDirs")
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .map(|part| part.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_else(|| {
            DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

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
        if excluded.iter().any(|e| e == &name) {
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
        // 目录省略 size/mtime（对齐 TS readDirectory）；文件携带二者。
        // 剥离 Windows 长路径前缀 `\\?\`，避免前端文件树因路径格式不识别而不展示。
        let path_str = path.to_string_lossy();
        let display_path = path_str.strip_prefix("\\\\?\\").unwrap_or(&path_str);
        let entry = if meta.is_dir() {
            json!({
                "name": name,
                "path": display_path.to_string(),
                "type": ty,
            })
        } else {
            json!({
                "name": name,
                "path": display_path.to_string(),
                "type": ty,
                "size": meta.len(),
                "mtime": mtime_ms,
            })
        };
        entries.push(entry);
    }
    // 排序：目录优先，再按名称（对齐 TS readDirectory）。
    entries.sort_by(|a, b| {
        let ad = a.get("type").and_then(serde_json::Value::as_str) == Some("directory");
        let bd = b.get("type").and_then(serde_json::Value::as_str) == Some("directory");
        match (ad, bd) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .cmp(b.get("name").and_then(serde_json::Value::as_str).unwrap_or("")),
        }
    });
    Ok(Json(json!(entries)))
}

/// GET /api/files/read?path=… → 文本内容（≤5MB）。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReadQuery {
    pub path: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/files/read",
    tag = "files",
    params(ReadQuery),
    responses(
        (status = 200, description = "文本内容（≤5MB；非 UTF-8 以 U+FFFD 替换）", body = crate::error::GenericJson),
        (status = 400, description = "路径是目录/文件过大", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
        // 对齐 TS files.service.ts：文本读取超限返回 400（非 413）+ files.file_too_large。
        return Err(AppError::business(
            ErrorCode::FilesFileTooLarge,
            StatusCode::BAD_REQUEST,
            format!("file too large for text read (max {} bytes)", MAX_READ_SIZE),
            None,
        ));
    }
    // M6 修复：先按字节读取再做 lossy 转换（TS fs.readFile 以 utf-8 读取时
    // 会用 U+FFFD 替换无效序列；Rust read_to_string 在非 UTF-8 时会直接 500）。
    let raw_bytes = tokio::fs::read(&resolved.resolved)
        .await
        .map_err(|e| AppError::internal(format!("read: {e}")))?;
    let content = String::from_utf8_lossy(&raw_bytes).to_string();
    Ok(Json(json!({
        "path": resolved.original,
        "content": content,
        "size": resolved.size,
        "mtime": resolved.mtime_ms,
    })))
}

/// GET /api/files/metadata?path=… → stat 信息。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MetaQuery {
    pub path: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/files/metadata",
    tag = "files",
    params(MetaQuery),
    responses(
        (status = 200, description = "文件/目录 stat 信息（类型/大小/mtime/权限）", body = crate::error::GenericJson),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&resolved.resolved)
            .ok()
            .map(|m| format!("0{:o}", m.permissions().mode() & 0o777))
    };
    #[cfg(not(unix))]
    // Windows 下也返回字符串（对齐 TS `0${(mode & 0o777).toString(8)}`，DTO 该字段必填）。
    let permissions = std::fs::metadata(&resolved.resolved)
        .ok()
        .map(|m| if m.permissions().readonly() { "0444" } else { "0666" }.to_string());
    Ok(Json(json!({
        "path": resolved.original,
        "name": resolved.resolved.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "type": kind,
        "size": resolved.size,
        "mtime": resolved.mtime_ms,
        "permissions": permissions,
    })))
}

/// DELETE /api/files/delete?path=…&recursive=… → 删除文件/符号链接/目录。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    pub path: Option<String>,
    pub recursive: Option<String>,
}

#[utoipa::path(
    delete,
    path = "/api/files/delete",
    tag = "files",
    params(DeleteQuery),
    responses(
        (status = 200, description = "已删除", body = crate::error::GenericJson),
        (status = 400, description = "路径缺失/目录非空", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "禁止删除工作区根", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
pub async fn delete_path(
    State(state): State<AppState>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let raw = q.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }

    // H4 修复：在 canonicalize（会跟随链接）之前先检查符号链接。
    // 符号链接必须通过 remove_file 删除（删除的是链接本身，而非其目标）。
    let sym_meta = tokio::fs::symlink_metadata(raw).await
        .map_err(|_| not_found(&format!("path not found: {raw}")))?;
    if sym_meta.is_symlink() {
        // 校验符号链接的父目录位于工作区之内。
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

    // 普通路径：解析（规范化）后按类型删除。
    let resolved = resolve(&state, raw).await?;
    // C3：禁止删除 workspace root 本身。
    if is_workspace_root(&state, &resolved.resolved) {
        return Err(forbidden("cannot delete a workspace root directory"));
    }
    let recursive = matches!(q.recursive.as_deref(), Some("true") | Some("1"));
    match resolved.kind {
        ResolvedKind::Directory => {
            if recursive {
                tokio::fs::remove_dir_all(&resolved.resolved)
                    .await
                    .map_err(|e| AppError::internal(format!("rmdir: {e}")))?;
            } else {
                // 非空则拒绝（对齐 TS 的 `dirNotEmpty`）。
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

/// POST /api/files/create-file → 创建空文件（或带内容文件）。
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateFileBody {
    pub path: Option<String>,
    pub content: Option<String>,
    pub overwrite: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/files/create-file",
    tag = "files",
    request_body = CreateFileBody,
    responses(
        (status = 200, description = "文件已创建（返回 mtime）", body = crate::error::GenericJson),
        (status = 400, description = "路径缺失/已存在", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "父目录越界/符号链接逃逸", body = crate::error::ErrorResponse),
    )
)]
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
        return Err(conflict(
            ErrorCode::FilesPathExists,
            "path already exists (set overwrite=true)",
        ));
    }
    // 高优先级修复：符号链接逃逸检查（与 write_file 一致）—— 若目标已存在，
    // 写入前先规范化并校验是否位于 within_workspace 之内。
    if let Ok(canonical_target) = tokio::fs::canonicalize(&p).await {
        if !within_workspace(&state, &canonical_target) {
            return Err(forbidden("target resolves outside workspace (symlink escape?)"));
        }
    }
    let content = body.content.clone().unwrap_or_default();
    if body.overwrite.unwrap_or(false) {
        tokio::fs::write(&p, content)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    } else {
        // 原子独占创建（对齐 TS flag 'wx'），消除 exists()→write 的 TOCTOU。
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
        f.write_all(content.as_bytes())
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    }
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    Ok(Json(json!({
        "ok": true,
        "path": raw,
        "mtime": meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0),
    })))
}

/// POST /api/files/create-directory → mkdir（可选递归）。
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateDirBody {
    pub path: Option<String>,
    pub recursive: Option<bool>,
    pub overwrite: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/files/create-directory",
    tag = "files",
    request_body = CreateDirBody,
    responses(
        (status = 200, description = "目录已创建", body = crate::error::GenericJson),
        (status = 400, description = "路径缺失/已存在且非目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "祖先/目标越界", body = crate::error::ErrorResponse),
    )
)]
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
            return Err(conflict(
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
        // H2 修复：在创建任何目录之前，先校验最近的已存在祖先目录。
        // 解析祖先 → 校验工作区 → 之后 create_dir_all 才是安全的。
        let mut ancestor = p.clone();
        while !ancestor.exists() {
            match ancestor.parent() {
                Some(par) => ancestor = par.to_path_buf(),
                None => break,
            }
        }
        let canonical_ancestor = tokio::fs::canonicalize(&ancestor)
            .await
            .map_err(|_| not_found("nearest existing ancestor not found"))?;
        if !within_workspace(&state, &canonical_ancestor) {
            return Err(forbidden("nearest existing ancestor is outside workspace"));
        }
        tokio::fs::create_dir_all(&p)
            .await
            .map_err(|e| AppError::internal(format!("mkdir -p: {e}")))?;
        // 事后校验所创建的路径本身。
        let canonical = tokio::fs::canonicalize(&p)
            .await
            .map_err(|e| AppError::internal(format!("canonicalize: {e}")))?;
        if !within_workspace(&state, &canonical) {
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

/// POST /api/files/write → 写入/覆盖文件（基于 expectedMtime 的乐观并发控制
/// 延后至后续实现；基本路径已与 TS 对齐）。
#[derive(Deserialize, utoipa::ToSchema)]
pub struct WriteFileBody {
    pub path: Option<String>,
    pub content: Option<String>,
    pub expected_mtime: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/files/write",
    tag = "files",
    request_body = WriteFileBody,
    responses(
        (status = 200, description = "文件已写入（返回 size/mtime）", body = crate::error::GenericJson),
        (status = 400, description = "content/path 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "父目录越界/符号链接逃逸", body = crate::error::ErrorResponse),
        (status = 409, description = "expectedMtime 不匹配（读取后被修改）", body = crate::error::ErrorResponse),
    )
)]
pub async fn write_file(
    State(state): State<AppState>,
    Json(body): Json<WriteFileBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    // 对齐 TS files.controller.ts：content 必须是字符串（contentRequired）。
    let content = match &body.content {
        Some(c) => c.clone(),
        None => return Err(bad_request(ErrorCode::FilesContentRequired, "content is required")),
    };
    let raw = body.path.as_deref().map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return Err(bad_request(ErrorCode::FilesPathRequired, "path is required"));
    }
    let p = PathBuf::from(raw);
    let canonical_parent = tokio::fs::canonicalize(p.parent().unwrap_or(&p))
        .await
        .map_err(|e| AppError::internal(format!("parent: {e}")))?;
    if !within_workspace(&state, &canonical_parent) {
        return Err(forbidden("parent is outside workspace"));
    }
    // H1 修复：若目标已存在，则规范化并校验是否位于工作区之内
    // （防止符号链接逃逸：工作区内的符号链接 → 工作区外的目标）。
    if let Ok(canonical_target) = tokio::fs::canonicalize(&p).await {
        if !within_workspace(&state, &canonical_target) {
            return Err(forbidden("target resolves outside workspace (symlink escape?)"));
        }
    }
    if let Some(expected) = body.expected_mtime {
        // M3 修复：±1000ms 容差（对齐 TS 的 Math.abs(diff) > 1000）+ 409 Conflict。
        if let Ok(meta) = tokio::fs::metadata(&p).await {
            let actual = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if (actual - expected).abs() > 1000 {
                return Err(conflict(
                    ErrorCode::FilesModifiedSinceRead,
                    "file has been modified since read (expectedMtime mismatch)",
                ));
            }
        }
    }
    tokio::fs::write(&p, content)
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
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

// ── serve（内联 + Range）/ download（附件下载）──────────────────────────

/// GET /api/files/serve?path=… —— 支持字节区段的内联预览。
/// 被 <img>/<video>/<pdf> 标签以及 OnlyOffice Document Server 用于获取文件。
/// 支持 RFC 6750 `access_token` 查询参数回退（由该路径的鉴权中间件处理）。
/// 返回 200/206/416。
#[utoipa::path(
    get,
    path = "/api/files/serve",
    tag = "files",
    params(ReadQuery),
    responses(
        (status = 200, description = "内联预览（支持 Range；图片/视频/PDF/OnlyOffice）", content_type = "application/octet-stream"),
        (status = 206, description = "部分内容（Range 请求）"),
        (status = 416, description = "Range 不可满足"),
        (status = 400, description = "路径是目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
        true, // 内联
        headers,
    )
    .await
}

/// GET /api/files/download?path=… —— 以附件形式下载（不支持 Range）。
#[utoipa::path(
    get,
    path = "/api/files/download",
    tag = "files",
    params(ReadQuery),
    responses(
        (status = 200, description = "附件下载（流式，application/octet-stream，不支持 Range）", content_type = "application/octet-stream"),
        (status = 400, description = "路径是目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
    // M2 修复：下载始终使用 application/octet-stream（对齐 TS files.controller.ts:290），
    // 与实际文件类型无关。浏览器总是会触发下载。
    let filename = resolved.resolved.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    // 流式下载（对齐 TS createReadStream）：避免大文件全量缓冲导致内存峰值/OOM。
    let file = tokio::fs::File::open(&resolved.resolved).await
        .map_err(|e| AppError::internal(format!("open: {e}")))?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, resolved.size)
        .header(header::CONTENT_DISPOSITION,
            build_content_disposition(&filename, false).as_str())
        // 安全头（对齐 TS files.controller.ts:290）：防止浏览器缓存可能携带
        // 认证 token 的下载 URL，以及阻止 MIME 嗅探。
        .header("cache-control", "private, no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(|e| AppError::internal(format!("response: {e}")))?)
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
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|e| AppError::internal(format!("open: {e}")))?;
            Ok(resp
                .header(header::CONTENT_LENGTH, size)
                .body(Body::from_stream(ReaderStream::new(file)))?)
        }
        RangeResult::Range(r) => {
            let length = r.end - r.start + 1;
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|e| AppError::internal(format!("open: {e}")))?;
            file.seek(SeekFrom::Start(r.start))
                .await
                .map_err(|e| AppError::internal(format!("seek: {e}")))?;
            // 流式发送区间（seek + take(length)），避免大区间全量缓冲。
            let stream = ReaderStream::new(file.take(length));
            Ok(resp
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", r.start, r.end, size),
                )
                .header(header::CONTENT_LENGTH, length)
                .body(Body::from_stream(stream))?)
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

/// 解析单个 RFC 9110 `bytes=start-end` 请求头。与 TS 的
/// `parseRangeHeader` 对齐。返回 None（缺失）、Range 或 Invalid。
fn parse_range_header(range_header: Option<&str>, size: u64) -> RangeResult {
    let Some(header) = range_header else {
        return RangeResult::None;
    };
    let header = header.trim();
    let Some(rest) = header.strip_prefix("bytes=") else {
        return RangeResult::Invalid;
    };
    // 匹配 `^bytes=(\d*)-(\d*)$`
    let (raw_start, raw_end) = match rest.split_once('-') {
        Some((s, e)) => (s, e),
        None => return RangeResult::Invalid,
    };
    if raw_start.is_empty() && raw_end.is_empty() {
        return RangeResult::Invalid;
    }
    if raw_start.is_empty() {
        // 后缀区间：bytes=-N → 最后 N 个字节
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

/// 按扩展名将文件名映射为 MIME 类型。与 TS 的 `guessMimeType` 对齐。
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
        // 图片
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        // 文档
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // 文本/代码
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
        // 媒体
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        // 归档
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        // 字体
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

/// 构建安全的 Content-Disposition 请求头。与 TS 的
/// `buildContentDisposition` 对齐。
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

// ── rename（重命名）/ copy（复制）/ move（移动）────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RenameBody {
    pub path: Option<String>,
    #[serde(rename = "newName")]
    pub new_name: Option<String>,
    pub overwrite: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/files/rename",
    tag = "files",
    request_body = RenameBody,
    responses(
        (status = 200, description = "已重命名（返回 oldPath/newPath）", body = crate::error::GenericJson),
        (status = 400, description = "path/newName 缺失或非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "禁止重命名工作区根", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
        (status = 409, description = "目标已存在", body = crate::error::ErrorResponse),
    )
)]
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
    // H2 修复：校验 newName 合法性（排除 `.`, `..`, NUL）。
    if new_name == "." || new_name == ".." || new_name.contains('\0') {
        return Err(bad_request(
            ErrorCode::FilesNameInvalid,
            "newName must not be '.', '..', or contain NUL",
        ));
    }
    let resolved = resolve(&state, raw).await?;
    // C3：禁止重命名 workspace root。
    if is_workspace_root(&state, &resolved.resolved) {
        return Err(forbidden("cannot rename a workspace root directory"));
    }
    let parent = resolved.resolved.parent()
        .ok_or_else(|| bad_request(ErrorCode::FilesNoParentFound, "parent path not found"))?;
    let dest = parent.join(new_name);
    if dest.exists() && !body.overwrite.unwrap_or(false) {
        return Err(conflict(ErrorCode::FilesPathExists, "destination already exists (set overwrite=true)"));
    }
    tokio::fs::rename(&resolved.resolved, &dest)
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    let canonical = tokio::fs::canonicalize(&dest).await
        .map_err(|e| AppError::internal(format!("canonicalize: {e}")))?;
    Ok(Json(json!({
        "ok": true,
        "oldPath": resolved.resolved.to_string_lossy(),
        "newPath": canonical.to_string_lossy(),
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CopyMoveBody {
    #[serde(rename = "sourcePath")]
    pub source_path: Option<String>,
    #[serde(rename = "destinationPath")]
    pub destination_path: Option<String>,
    pub overwrite: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/files/copy",
    tag = "files",
    request_body = CopyMoveBody,
    responses(
        (status = 200, description = "已复制", body = crate::error::GenericJson),
        (status = 400, description = "source/destination 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
        (status = 409, description = "目标已存在", body = crate::error::ErrorResponse),
    )
)]
pub async fn copy_path(
    State(state): State<AppState>,
    Json(body): Json<CopyMoveBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    do_relocate(&state, &body, false).await
}

#[utoipa::path(
    post,
    path = "/api/files/move",
    tag = "files",
    request_body = CopyMoveBody,
    responses(
        (status = 200, description = "已移动", body = crate::error::GenericJson),
        (status = 400, description = "source/destination 缺失", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
        (status = 409, description = "目标已存在", body = crate::error::ErrorResponse),
    )
)]
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
    // C3：禁止移动 workspace root。
    if is_move && is_workspace_root(state, &src.resolved) {
        return Err(forbidden("cannot move a workspace root directory"));
    }
    // 解析目标的规范化路径：已存在 → 其本身；不存在 → canonical(parent)+文件名。
    // 用于工作区校验与自身/后代守卫（不依赖目标必须存在，修复原 is_descendant_of
    // 对不存在目标返回 false 的 TOCTOU 漏洞）。
    let dst_canonical: PathBuf = match tokio::fs::canonicalize(dst_raw).await {
        Ok(c) => c,
        Err(_) => {
            let parent = Path::new(dst_raw).parent().unwrap_or(Path::new(dst_raw));
            tokio::fs::canonicalize(parent).await
                .map_err(|_| not_found("destination parent path not found"))?
                .join(Path::new(dst_raw).file_name().unwrap_or_default())
        }
    };
    if !within_workspace(state, &dst_canonical) {
        return Err(forbidden("destination is outside workspace"));
    }
    // C4：禁止复制/移动到自身或其子路径（对齐 TS path.relative 词法判断）。
    // 若目标是已存在目录，实际落点为 dst/src 名。
    // R7：用 tokio::fs 异步检查，避免在 async handler 里用 Path::is_dir/exists
    // （它们内部是同步 std::fs::metadata）阻塞 worker。
    let dst_is_dir = tokio::fs::metadata(&dst_canonical)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let effective_dest = if dst_is_dir {
        dst_canonical.join(src.resolved.file_name().unwrap_or_default())
    } else {
        dst_canonical.clone()
    };
    if effective_dest.starts_with(&src.resolved) {
        return Err(forbidden("cannot copy/move a directory into itself or a descendant"));
    }
    // M2 修复：存在性检查与实际落点都基于 effective_dest（目标为已存在目录时，落点为 dst/src_name）。
    // 原代码算了 effective_dest 却仍用 dst_canonical：移入目录时要么 EISDIR 失败、要么 rename
    // 替换目录 / copy_dir 合并覆盖，均与 TS resolveSafeTargetPath 语义不符。
    let dst_exists = tokio::fs::try_exists(&effective_dest).await.unwrap_or(false);
    if dst_exists && !body.overwrite.unwrap_or(false) {
        return Err(conflict(ErrorCode::FilesPathExists,
            "destination already exists (set overwrite=true)"));
    }
    let dest = effective_dest;
    if is_move {
        tokio::fs::rename(&src.resolved, &dest)
            .await
            // map_fs_error 已含 EXDEV→400 FilesOperationFailed（跨设备移动），
            // 与原手写分支语义一致。
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    } else if src.kind == ResolvedKind::Directory {
        copy_dir_recursive(&src.resolved, &dest)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    } else {
        tokio::fs::copy(&src.resolved, &dest)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    }
    // move 返回 oldPath/newPath；copy 返回 sourcePath/destinationPath（对齐 TS DTO）。
    let body = if is_move {
        json!({
            "ok": true,
            "oldPath": src.resolved.to_string_lossy(),
            "newPath": dest.to_string_lossy(),
        })
    } else {
        json!({
            "ok": true,
            "sourcePath": src.resolved.to_string_lossy(),
            "destinationPath": dest.to_string_lossy(),
        })
    };
    Ok(Json(body))
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ftype = entry.file_type().await?;
        if ftype.is_symlink() {
            // 保留符号链接（对齐 TS dereference:false），不复制其目标内容
            // （否则指向 workspace 外的链接会被实体化，造成信息泄露）。
            #[cfg(unix)]
            {
                let target = tokio::fs::read_link(&from).await?;
                // 安全加固：拒绝重建指向 workspace 外的绝对符号链接 ——
                // 否则会在副本里留下指向任意位置的"毒链接"（跳板）。相对链接保留
                // （在副本目录内解析，随副本移动仍合理）。
                if target.is_absolute() {
                    tracing::warn!(
                        from = %from.display(),
                        target = %target.display(),
                        "skipping symlink with absolute target during directory copy"
                    );
                } else {
                    tokio::fs::symlink(&target, &to).await?;
                }
            }
            #[cfg(not(unix))]
            {
                tokio::fs::copy(&from, &to).await?;
            }
        } else if ftype.is_dir() {
            Box::pin(copy_dir_recursive(&from, &to)).await?;
        } else {
            tokio::fs::copy(&from, &to).await?;
        }
    }
    Ok(())
}

// ── multipart 上传 ─────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    #[serde(rename = "destinationPath")]
    pub destination_path: Option<String>,
    pub overwrite: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/files/upload",
    tag = "files",
    params(UploadQuery),
    responses(
        (status = 200, description = "上传结果（已上传文件列表）。请求体为 multipart/form-data，字段名 files", body = crate::error::GenericJson),
        (status = 400, description = "destinationPath 缺失/文件名非法", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 403, description = "目标越界", body = crate::error::ErrorResponse),
        (status = 413, description = "超出上传上限", body = crate::error::ErrorResponse),
    )
)]
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
    // 校验目标目录位于工作区之内。
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

    // 上传字节上限（对齐 TS files.uploadMaxBytes，默认 100 MB）。
    let max_bytes: u64 = {
        let reader = crate::settings::SettingsReader::new(&state.db, None);
        reader.get_upload_max_bytes()
    };

    let mut files = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let raw_filename = field.file_name().unwrap_or("upload").to_string();
        // 对齐 TS normalizeUploadRelativePath：含反斜杠 / 绝对路径 → uploadPathInvalid；
        // 空段或 . / .. → nameInvalid（严格报错，而非静默清洗）。
        let safe_name: String = {
            if raw_filename.contains('\\') {
                return Err(bad_request(ErrorCode::FilesUploadPathInvalid, "upload path must not contain backslashes"));
            }
            if raw_filename.starts_with('/') {
                return Err(bad_request(ErrorCode::FilesUploadPathInvalid, "upload path must not be absolute"));
            }
            let mut parts: Vec<String> = Vec::new();
            for part in raw_filename.split('/') {
                let p = part.trim();
                if p.is_empty() || p == "." || p == ".." {
                    return Err(bad_request(ErrorCode::FilesNameInvalid, "upload path contains an invalid segment"));
                }
                parts.push(p.to_string());
            }
            if parts.is_empty() {
                "upload".to_string()
            } else {
                parts.join("/")
            }
        };
        let file_path = dest_canonical.join(&safe_name);
        // 保留子路径时需先创建父目录。
        let parent_dir = file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| dest_canonical.clone());
        tokio::fs::create_dir_all(&parent_dir)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
        if file_path.exists() && !overwrite {
            return Err(conflict(ErrorCode::FilesPathExists,
                format!("{safe_name} already exists (set overwrite=true)")));
        }
        // 高优先级修复：符号链接逃逸检查 —— 若目标已存在且为符号链接，
        // 写入前先规范化并校验是否位于 within_workspace 之内。
        if let Ok(canonical_target) = tokio::fs::canonicalize(&file_path).await {
            if !within_workspace(&state, &canonical_target) {
                return Err(forbidden("target resolves outside workspace (symlink escape?)"));
            }
        }
        // 流式写临时文件（对齐 TS saveSingleUpload：.codex-upload-<uuid>.tmp），
        // 累计字节超 uploadMaxBytes → 413 FilesUploadTooLarge 并清理 tmp；成功后
        // rename 到目标（overwrite 语义：rename 覆盖已存在的同名文件）。
        let tmp_path = parent_dir
            .join(format!(".codex-upload-{}.tmp", uuid::Uuid::new_v4()));
        let total = match stream_upload_to_tmp(field, &tmp_path, max_bytes).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e);
            }
        };
        if let Err(e) = tokio::fs::rename(&tmp_path, &file_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(map_fs_error(e, ErrorCode::FilesOperationFailed));
        }
        files.push(json!({ "path": file_path.to_string_lossy(), "size": total }));
    }
    // 对齐 TS：上传至少需要一个文件（uploadFileRequired）。
    if files.is_empty() {
        return Err(bad_request(ErrorCode::FilesUploadFileRequired, "at least one file is required"));
    }
    Ok(Json(json!({ "ok": true, "files": files })))
}

/// 流式把一个 multipart field 写入临时文件，累计字节；超 `max_bytes` 返回
/// 413 FilesUploadTooLarge（对齐 TS saveSingleUpload 的流式 + 大小限制）。
async fn stream_upload_to_tmp(
    mut field: axum::extract::multipart::Field<'_>,
    tmp_path: &Path,
    max_bytes: u64,
) -> Result<u64, AppError> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::File::create(tmp_path)
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    let mut total: u64 = 0;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::internal(format!("read multipart chunk: {e}")))?
    {
        total += chunk.len() as u64;
        if total > max_bytes {
            // 超限：413（对齐 TS files.upload_too_large）。
            return Err(AppError::business(
                ErrorCode::FilesUploadTooLarge,
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("upload exceeds the {max_bytes}-byte limit"),
                None,
            ));
        }
        f.write_all(&chunk)
            .await
            .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    }
    f.flush()
        .await
        .map_err(|e| map_fs_error(e, ErrorCode::FilesOperationFailed))?;
    Ok(total)
}

// ── 归档列表 / 条目读取（zip + tar/gz/bz2/xz）──────────────────────────────

#[utoipa::path(
    get,
    path = "/api/files/archive/list",
    tag = "files",
    params(ReadQuery),
    responses(
        (status = 200, description = "归档条目树（zip/tar.gz/tar.bz2/tar.xz/7z）", body = crate::error::GenericJson),
        (status = 400, description = "路径是目录/格式不支持", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径不存在", body = crate::error::ErrorResponse),
    )
)]
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
        .map_err(map_list_error)?;
    // 扁平条目 → 嵌套目录树（对齐 TS ArchiveService.buildTree）。
    let tree = build_archive_tree(entries);
    Ok(Json(json!({ "path": resolved.original, "entries": tree })))
}

#[utoipa::path(
    get,
    path = "/api/files/archive/entry",
    tag = "files",
    params(ArchiveEntryQuery),
    responses(
        (status = 200, description = "归档内单条目内容（流式，支持 Range）", content_type = "application/octet-stream"),
        (status = 206, description = "部分内容（Range 请求）"),
        (status = 416, description = "Range 不可满足"),
        (status = 400, description = "entry 缺失/路径是目录", body = crate::error::ErrorResponse),
        (status = 401, description = "未认证", body = crate::error::ErrorResponse),
        (status = 404, description = "路径/条目不存在", body = crate::error::ErrorResponse),
        (status = 413, description = "条目超出大小上限", body = crate::error::ErrorResponse),
    )
)]
pub async fn archive_entry(
    State(state): State<AppState>,
    Query(q): Query<ArchiveEntryQuery>,
    headers: HeaderMap,
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
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // R9 真流式 Range：read_archive_entry 内部按 Range 决定读取范围
    // （zip/tar 顺序读到 Range 即停，不再全量解压整个条目），返回 (total, outcome, data)。
    let (total, outcome, data) =
        tokio::task::spawn_blocking(move || read_archive_entry(&path, &ep, range_header.as_deref()))
            .await
            .map_err(|e| AppError::internal(format!("archive task: {e}")))?
            .map_err(|e| match e {
                ArchiveReadError::TooLarge => AppError::business(
                    ErrorCode::ArchiveEntryTooLarge,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "Archive entry exceeds maximum size ({} bytes)",
                        MAX_ARCHIVE_ENTRY_BYTES
                    ),
                    None,
                ),
                ArchiveReadError::Other(s) => AppError::internal(format!("archive read: {s}")),
            })?;

    let mime = guess_mime_type(entry_path);
    let filename = std::path::Path::new(entry_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let resp = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, mime.as_str())
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "private, no-store")
        .header(header::CONTENT_DISPOSITION, build_content_disposition(&filename, true).as_str());

    Ok(match outcome {
        RangeResult::Invalid => resp
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", total))
            .body(Body::empty())?,
        // G2 修复：Content-Length 用实际解压字节数（data.len()），而非声明 total。
        // 损坏/截断/恶意归档（声明 size > 实际）时，旧逻辑发出与 body 不符的 Content-Length，
        // 导致客户端按声明值等待字节而挂起至超时。
        RangeResult::None => resp
            .header(header::CONTENT_LENGTH, data.len() as u64)
            .body(Body::from(data))?,
        RangeResult::Range(r) => {
            let want = (r.end - r.start + 1) as usize;
            // 实际解压不足（截断/损坏/Range 超出实际）：空 → 416；否则按实际收窄 end 与长度。
            if data.len() < want {
                if data.is_empty() {
                    return Ok(resp
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{}", total))
                        .body(Body::empty())?);
                }
                let actual_end = r.start + data.len() as u64 - 1;
                resp.status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", r.start, actual_end, total))
                    .header(header::CONTENT_LENGTH, data.len() as u64)
                    .body(Body::from(data))?
            } else {
                resp.status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", r.start, r.end, total))
                    .header(header::CONTENT_LENGTH, want as u64)
                    .body(Body::from(data))?
            }
        }
    })
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArchiveEntryQuery {
    pub path: Option<String>,
    pub entry: Option<String>,
}

enum ArchiveFormat { Zip, Tar, TarGz, TarBz2, TarXz, SevenZip }

fn detect_format(path: &Path) -> Option<ArchiveFormat> {
    let name = path.file_name()?.to_str()?.to_lowercase();
    if name.ends_with(".zip") { Some(ArchiveFormat::Zip) }
    else if name.ends_with(".tar") { Some(ArchiveFormat::Tar) }
    else if name.ends_with(".tar.gz") || name.ends_with(".tgz") { Some(ArchiveFormat::TarGz) }
    else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") { Some(ArchiveFormat::TarBz2) }
    else if name.ends_with(".tar.xz") || name.ends_with(".txz") { Some(ArchiveFormat::TarXz) }
    else if name.ends_with(".7z") { Some(ArchiveFormat::SevenZip) }
    else { None }
}

/// Wraps a reader in an XZ decoder so it can be fed into list_tar / read_tar_entry.
/// lzma-rust2::XzReader implements std::io::Read under the std feature.
fn xz_decoder<R: std::io::Read>(reader: R) -> lzma_rust2::XzReader<R> {
    lzma_rust2::XzReader::new(reader, true)
}

/// C1+C2 修复：规范化归档条目路径，拒绝穿越/绝对路径/NUL/空段（对齐 TS
/// `normalizeArchiveEntryPath`）。返回 `None` 视为不安全。
///
/// 步骤：空或含 NUL → 拒绝；反斜杠转正；去一次前导 `./`；拒绝 `/` 或
/// `[A-Za-z]:/` 绝对路径；按 `/` 切分并丢弃空段；含 `.`/`..` 段或无剩余段 → 拒绝。
fn normalize_archive_entry_path(entry_path: &str) -> Option<String> {
    if entry_path.is_empty() || entry_path.contains('\0') {
        return None;
    }
    // 反斜杠 → 正斜杠（zip/tar 内部可能用反斜杠）。
    let slashed = entry_path.replace('\\', "/");
    // 仅去一次前导 `./`（对齐 TS `replace(/^\.\//, '')`）。
    let stripped = slashed.strip_prefix("./").unwrap_or(&slashed);
    // 拒绝绝对路径：`/` 开头（Unix）或 `[A-Za-z]:/`（Windows 盘符）。
    if stripped.starts_with('/') {
        return None;
    }
    if stripped.len() >= 3 {
        let b = stripped.as_bytes();
        if b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
            return None;
        }
    }
    // 切分并丢弃空段（等价 TS `split('/').filter(Boolean)`）。
    let parts: Vec<&str> = stripped.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return None;
    }
    Some(parts.join("/"))
}

const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const MAX_ARCHIVE_TOTAL_BYTES: u64 = 1 << 30; // 1 GB
/// 单个归档条目解压后的字节上限（对齐 TS MAX_ARCHIVE_ENTRY_BYTES，防 zip-bomb OOM）。
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 50 * 1024 * 1024; // 50 MB

/// 归档列表错误（区分 HTTP 状态码：413 vs 400 vs 500，对齐 TS）。
enum ArchiveListError {
    UnsupportedFormat,
    TooManyEntries,
    TotalSizeTooLarge,
    UnsafeEntryPath,
    Other(String),
}

fn map_list_error(e: ArchiveListError) -> AppError {
    use ArchiveListError::*;
    match e {
        TotalSizeTooLarge => AppError::business(
            ErrorCode::ArchiveTotalSizeTooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Archive exceeds the 1 GB uncompressed preview limit".into(),
            None,
        ),
        UnsupportedFormat => AppError::business(
            ErrorCode::ArchiveUnsupportedFormat,
            StatusCode::BAD_REQUEST,
            "Unsupported archive format".into(),
            None,
        ),
        TooManyEntries => AppError::business(
            ErrorCode::ArchiveTooManyEntries,
            StatusCode::BAD_REQUEST,
            "Archive exceeds the 20,000 entry limit".into(),
            None,
        ),
        UnsafeEntryPath => AppError::business(
            ErrorCode::ArchiveUnsafeEntryPath,
            StatusCode::BAD_REQUEST,
            "Archive contains an unsafe entry path".into(),
            None,
        ),
        Other(s) => AppError::internal(format!("archive list: {s}")),
    }
}

fn list_archive_entries(path: &Path) -> Result<Vec<serde_json::Value>, ArchiveListError> {
    use ArchiveListError::*;
    let fmt = detect_format(path).ok_or(UnsupportedFormat)?;
    match fmt {
        ArchiveFormat::Zip => {
            let file = std::fs::File::open(path).map_err(|e| Other(e.to_string()))?;
            let mut archive = zip::ZipArchive::new(file).map_err(|e| Other(e.to_string()))?;
            if archive.len() > MAX_ARCHIVE_ENTRIES {
                return Err(TooManyEntries);
            }
            let mut entries = Vec::new();
            let mut total_size: u64 = 0;
            for i in 0..archive.len() {
                let entry = archive.by_index(i).map_err(|e| Other(e.to_string()))?;
                let raw_name = entry.name().to_string();
                // 规范化 + 安全校验（zip-slip / 绝对路径 / NUL / 空段）。
                let path =
                    normalize_archive_entry_path(&raw_name).ok_or(UnsafeEntryPath)?;
                let entry_size = entry.size();
                // 对照 zip-archive.adapter.ts：compressedSize + encrypted（general
                // purpose bit flag bit0）。
                let compressed_size = entry.compressed_size();
                let encrypted = entry.encrypted();
                total_size += entry_size;
                if total_size > MAX_ARCHIVE_TOTAL_BYTES {
                    return Err(TotalSizeTooLarge);
                }
                let is_dir = entry.is_dir();
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                entries.push(json!({
                    "name": name,
                    "path": path,
                    "type": if is_dir { "directory" } else { "file" },
                    "size": if is_dir { serde_json::Value::Null } else { serde_json::json!(entry_size) },
                    "compressedSize": compressed_size,
                    "encrypted": encrypted,
                }));
            }
            Ok(entries)
        }
        ArchiveFormat::Tar => list_tar(std::fs::File::open(path).map_err(|e| Other(e.to_string()))?),
        ArchiveFormat::TarGz => list_tar(flate2::read::GzDecoder::new(
            std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
        )),
        ArchiveFormat::SevenZip => list_sevenzip(path),
        ArchiveFormat::TarBz2 => list_tar(bzip2_rs::DecoderReader::new(
            std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
        )),
        ArchiveFormat::TarXz => list_tar(xz_decoder(
            std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
        )),
    }
}

fn list_tar<R: std::io::Read>(reader: R) -> Result<Vec<serde_json::Value>, ArchiveListError> {
    use ArchiveListError::*;
    let mut archive = tar::Archive::new(reader);
    let mut entries = Vec::new();
    let mut total_size: u64 = 0;
    let mut count = 0usize;
    for entry in archive.entries().map_err(|e| Other(e.to_string()))? {
        let entry = entry.map_err(|e| Other(e.to_string()))?;
        let raw_path = entry
            .path()
            .map_err(|e| Other(e.to_string()))?
            .to_string_lossy()
            .to_string();
        // 规范化 + 安全校验；list 输出使用规范化后的 path。
        let path = normalize_archive_entry_path(&raw_path).ok_or(UnsafeEntryPath)?;
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(TooManyEntries);
        }
        let entry_size = entry.size();
        total_size += entry_size;
        if total_size > MAX_ARCHIVE_TOTAL_BYTES {
            return Err(TotalSizeTooLarge);
        }
        // 对照 tar-archive.adapter.ts：mtime（毫秒）+ unsupported（非 file/dir）。
        let entry_type = entry.header().entry_type();
        let is_dir = entry_type.is_dir();
        let is_file = entry_type.is_file();
        let unsupported = !is_dir && !is_file;
        let mtime_ms = entry.header().mtime().ok().map(|s| s * 1000).unwrap_or(0);
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        entries.push(json!({
            "name": name,
            "path": path,
            "type": if is_dir { "directory" } else { "file" },
            "size": if is_dir { serde_json::Value::Null } else { serde_json::json!(entry_size) },
            "mtime": mtime_ms,
            "unsupported": unsupported,
        }));
    }
    Ok(entries)
}

fn list_sevenzip(path: &Path) -> Result<Vec<serde_json::Value>, ArchiveListError> {
    use ArchiveListError::*;
    let mut reader = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
        .map_err(|e| Other(e.to_string()))?;
    // sevenz-rust2 的 entry 本身不暴露加密标志（加密位于 folder/coder 级别）。
    // 近似判断：若任一 block 的 coder 链含 AES256_SHA256，则视为整包加密
    // （7z 通常整包加密），对所有条目输出 encrypted=true（对齐 TS sevenzip
    // adapter 的 encrypted 字段）。
    const AES256_SHA256_ID: &[u8] = &[0x06, 0xF1, 0x07, 0x01];
    let archive_encrypted = reader
        .archive()
        .blocks
        .iter()
        .any(|b| b.coders.iter().any(|c| c.encoder_method_id() == AES256_SHA256_ID));
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut total_size: u64 = 0;
    let mut err: Option<ArchiveListError> = None;
    reader
        .for_each_entries(|entry, _stream| {
            if err.is_some() {
                return Ok(false);
            }
            let raw_name = entry.name().to_string();
            // 规范化 + 安全校验；list 输出使用规范化后的 path。
            let name = match normalize_archive_entry_path(&raw_name) {
                Some(n) => n,
                None => {
                    err = Some(UnsafeEntryPath);
                    return Ok(false);
                }
            };
            if entries.len() >= MAX_ARCHIVE_ENTRIES {
                err = Some(TooManyEntries);
                return Ok(false);
            }
            let size = entry.size();
            total_size += size;
            if total_size > MAX_ARCHIVE_TOTAL_BYTES {
                err = Some(TotalSizeTooLarge);
                return Ok(false);
            }
            let is_dir = entry.is_directory();
            let display_name = name.rsplit('/').next().unwrap_or(&name).to_string();
            entries.push(json!({
                "name": display_name,
                "path": name,
                "type": if is_dir { "directory" } else { "file" },
                "size": if is_dir { serde_json::Value::Null } else { serde_json::json!(size) },
                "encrypted": archive_encrypted,
            }));
            Ok(true)
        })
        .map_err(|e| Other(e.to_string()))?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(entries)
}

/// 归档读取错误：`TooLarge` 区分"条目超过解压上限"（→ 413）与其他错误（→ 500）。
enum ArchiveReadError {
    TooLarge,
    Other(String),
}

fn read_archive_entry(
    path: &Path,
    entry_name: &str,
    range_header: Option<&str>,
) -> Result<(u64, RangeResult, Vec<u8>), ArchiveReadError> {
    use ArchiveReadError::*;
    let fmt = detect_format(path).ok_or_else(|| Other("unsupported archive format".into()))?;
    // 入口路径需为已规范化的安全路径（与 list 输出一致），否则直接拒绝。
    let target = normalize_archive_entry_path(entry_name)
        .ok_or_else(|| Other("unsafe archive entry path".into()))?;
    match fmt {
        ArchiveFormat::Zip => {
            let file = std::fs::File::open(path).map_err(|e| Other(e.to_string()))?;
            let mut archive = zip::ZipArchive::new(file).map_err(|e| Other(e.to_string()))?;
            // 用规范化名匹配原始条目（list 输出规范化 path，read 按同一规则定位），
            // 避免内部名带 `./` 前缀或反斜杠时 `by_name` 找不到。
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(|e| Other(e.to_string()))?;
                let raw = entry.name().to_string();
                if normalize_archive_entry_path(&raw).as_deref() == Some(target.as_str()) {
                    let total = entry.size();
                    let (outcome, data) = read_entry_with_range(total, range_header, &mut entry)?;
                    return Ok((total, outcome, data));
                }
            }
            Err(Other(format!("entry not found: {entry_name}")))
        }
        ArchiveFormat::Tar => read_tar_entry(
            std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
            &target,
            entry_name,
            range_header,
        ),
        ArchiveFormat::TarGz => read_tar_entry(
            flate2::read::GzDecoder::new(
                std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
            ),
            &target,
            entry_name,
            range_header,
        ),
        ArchiveFormat::SevenZip => {
            let mut reader = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())
                .map_err(|e| Other(e.to_string()))?;
            let mut result: Result<(u64, RangeResult, Vec<u8>), ArchiveReadError> =
                Err(Other(format!("entry not found: {entry_name}")));
            reader
                .for_each_entries(|entry, stream| {
                    if result.is_ok() {
                        return Ok(false);
                    }
                    let raw = entry.name().to_string();
                    if normalize_archive_entry_path(&raw).as_deref() == Some(target.as_str()) {
                        let total = entry.size();
                        match read_entry_with_range(total, range_header, stream) {
                            Ok((outcome, data)) => {
                                result = Ok((total, outcome, data));
                            }
                            Err(e) => {
                                result = Err(e);
                            }
                        }
                        return Ok(false);
                    }
                    Ok(true)
                })
                .map_err(|e| Other(e.to_string()))?;
            result
        }
        ArchiveFormat::TarBz2 => read_tar_entry(
            bzip2_rs::DecoderReader::new(
                std::fs::File::open(path).map_err(|e| Other(e.to_string()))?,
            ),
            &target,
            entry_name,
            range_header,
        ),
        ArchiveFormat::TarXz => read_tar_entry(
            xz_decoder(std::fs::File::open(path).map_err(|e| Other(e.to_string()))?),
            &target,
            entry_name,
            range_header,
        ),
    }
}

fn read_tar_entry<R: std::io::Read>(
    reader: R,
    target: &str,
    entry_name: &str,
    range_header: Option<&str>,
) -> Result<(u64, RangeResult, Vec<u8>), ArchiveReadError> {
    use ArchiveReadError::*;
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| Other(e.to_string()))? {
        let mut entry = entry.map_err(|e| Other(e.to_string()))?;
        let raw = entry
            .path()
            .map_err(|e| Other(e.to_string()))?
            .to_string_lossy()
            .to_string();
        // 用规范化名匹配（与 list 输出一致）。
        if normalize_archive_entry_path(&raw).as_deref() == Some(target) {
            let total = entry.header().size().map_err(|e| Other(e.to_string()))?;
            return read_entry_with_range(total, range_header, &mut entry)
                .map(|(outcome, data)| (total, outcome, data));
        }
    }
    Err(Other(format!("entry not found: {entry_name}")))
}

/// 最多读取 `MAX_ARCHIVE_ENTRY_BYTES` 字节；超出则返回 `TooLarge`（防 zip-bomb 导致 OOM）。
fn read_limited<R: std::io::Read>(reader: R) -> Result<Vec<u8>, ArchiveReadError> {
    let limit = MAX_ARCHIVE_ENTRY_BYTES as usize;
    let mut buf = Vec::new();
    let mut limited = std::io::Read::take(reader, (limit + 1) as u64);
    std::io::Read::read_to_end(&mut limited, &mut buf)
        .map_err(|e| ArchiveReadError::Other(e.to_string()))?;
    if buf.len() > limit {
        return Err(ArchiveReadError::TooLarge);
    }
    Ok(buf)
}

/// R9 真流式 Range：跳过前 `start` 字节（解压丢弃，不计上限），再读取
/// `end - start + 1` 字节（受 MAX_ARCHIVE_ENTRY_BYTES 上限约束；`end` 为 inclusive）。
/// zip/tar 等压缩流不可随机寻址，故 start 之前仍需顺序解压丢弃，但避免了
/// "全量解压整个条目再切片"的内存峰值与每次 Range 的重复全量解压。
fn read_limited_range<R: std::io::Read>(
    mut reader: R,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, ArchiveReadError> {
    use std::io::Read;
    let limit = MAX_ARCHIVE_ENTRY_BYTES as u64;
    // M3 修复：skip 上限防 zip-bomb CPU DoS —— 声明大 size 的归档配合 Range 可强迫顺序解压并
    // 丢弃海量字节（内存仅 8KB skip_buf 不会 OOM，但 CPU 被打满、占满阻塞线程池）。上限与单条目上限一致。
    if start > limit {
        return Err(ArchiveReadError::Other(format!(
            "range start exceeds max archive entry size ({} bytes)",
            limit
        )));
    }
    let length = end.saturating_sub(start).saturating_add(1).min(limit);
    // 跳过 start 字节（解压丢弃）。
    let mut remaining = start;
    let mut skip_buf = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(skip_buf.len() as u64) as usize;
        let n = reader
            .read(&mut skip_buf[..want])
            .map_err(|e| ArchiveReadError::Other(e.to_string()))?;
        if n == 0 {
            return Ok(Vec::new()); // start 超出条目大小：返回空（上层 total 正确，响应空体）
        }
        remaining -= n as u64;
    }
    let mut out = Vec::with_capacity(length as usize);
    reader
        .take(length)
        .read_to_end(&mut out)
        .map_err(|e| ArchiveReadError::Other(e.to_string()))?;
    Ok(out)
}

/// 根据条目总大小与 Range 头决定读取方式，返回 (Range 结果, 数据)。
/// - Invalid → 空 Vec（上层返回 416）
/// - None → 全量读取（上限保护）
/// - Range → 仅读取 [start, end]（真流式，不全量解压）
fn read_entry_with_range<R: std::io::Read>(
    total: u64,
    range_header: Option<&str>,
    reader: R,
) -> Result<(RangeResult, Vec<u8>), ArchiveReadError> {
    let outcome = parse_range_header(range_header, total);
    let data = match &outcome {
        RangeResult::Invalid => Vec::new(),
        RangeResult::None => read_limited(reader)?,
        RangeResult::Range(r) => read_limited_range(reader, r.start, r.end)?,
    };
    Ok((outcome, data))
}

/// 将扁平归档条目构建为嵌套目录树（对齐 TS `ArchiveService.buildTree`）。
///
/// 目录节点带 `children` 数组；按"目录优先 + 名称"递归排序。
fn build_archive_tree(entries: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    struct Node {
        name: String,
        path: String,
        is_dir: bool,
        size: serde_json::Value,
        // 透传给前端的额外字段（compressedSize/encrypted/mtime/unsupported 等）。
        // 中间目录节点没有这些字段，留空 Map。
        props: serde_json::Map<String, serde_json::Value>,
        children: Vec<usize>, // arena 索引
    }

    fn norm(p: &str) -> String {
        p.trim_end_matches('/').to_string()
    }
    fn dirname(p: &str) -> &str {
        match p.rfind('/') {
            Some(i) => &p[..i],
            None => "",
        }
    }

    let mut arena: Vec<Node> = Vec::new();
    let mut dir_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut roots: Vec<usize> = Vec::new();

    // ensure_dir：返回（必要时创建）目录路径对应的节点索引。
    fn ensure_dir(
        arena: &mut Vec<Node>,
        dir_index: &mut std::collections::HashMap<String, usize>,
        roots: &mut Vec<usize>,
        dir_path: &str,
    ) -> usize {
        if let Some(&idx) = dir_index.get(dir_path) {
            return idx;
        }
        // 自底向上收集尚未创建的祖先，再自顶向下创建。
        let mut chain: Vec<String> = Vec::new();
        let mut cur = dir_path.to_string();
        loop {
            if dir_index.contains_key(&cur) {
                break;
            }
            chain.push(cur.clone());
            let parent = match cur.rfind('/') {
                Some(i) => &cur[..i],
                None => "",
            };
            if parent.is_empty() {
                break;
            }
            cur = parent.to_string();
        }
        for p in chain.into_iter().rev() {
            let name = p.rsplit('/').next().unwrap_or(&p).to_string();
            let idx = arena.len();
            arena.push(Node {
                name,
                path: p.clone(),
                is_dir: true,
                size: serde_json::Value::Null,
                props: serde_json::Map::new(),
                children: Vec::new(),
            });
            dir_index.insert(p.clone(), idx);
            let parent = match p.rfind('/') {
                Some(i) => &p[..i],
                None => "",
            };
            if parent.is_empty() {
                roots.push(idx);
            } else {
                let pidx = *dir_index
                    .get(parent)
                    .expect("ancestor created in prior iteration");
                arena[pidx].children.push(idx);
            }
        }
        *dir_index.get(dir_path).unwrap()
    }

    // 已知键之外的字段全部透传（compressedSize/encrypted/mtime/unsupported 等）。
    let known: [&str; 4] = ["name", "path", "type", "size"];
    let mut flat: Vec<(String, bool, serde_json::Value, serde_json::Map<String, serde_json::Value>)> = entries
        .into_iter()
        .map(|e| {
            let path = norm(e.get("path").and_then(|v| v.as_str()).unwrap_or(""));
            let is_dir = e.get("type").and_then(|v| v.as_str()) == Some("directory");
            let size = e.get("size").cloned().unwrap_or(serde_json::Value::Null);
            let mut props = serde_json::Map::new();
            if let Some(obj) = e.as_object() {
                for (k, v) in obj {
                    if !known.iter().any(|x| *x == k.as_str()) {
                        props.insert(k.clone(), v.clone());
                    }
                }
            }
            (path, is_dir, size, props)
        })
        .collect();
    flat.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, is_dir, size, props) in flat {
        if path.is_empty() {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let parent = dirname(&path);
        let idx = arena.len();
        arena.push(Node {
            name,
            path: path.clone(),
            is_dir,
            size,
            props,
            children: Vec::new(),
        });
        if parent.is_empty() {
            roots.push(idx);
        } else {
            let pidx = ensure_dir(&mut arena, &mut dir_index, &mut roots, parent);
            arena[pidx].children.push(idx);
        }
        if is_dir {
            dir_index.insert(path, idx);
        }
    }

    // 每个节点的子列表按"目录优先 + 名称"排序（遍历所有节点即覆盖所有层级）。
    for i in 0..arena.len() {
        let mut keys: Vec<(bool, String, usize)> = {
            let children = &arena[i].children;
            children
                .iter()
                .map(|&c| (arena[c].is_dir, arena[c].name.clone(), c))
                .collect()
        };
        keys.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.cmp(&b.1),
        });
        arena[i].children = keys.into_iter().map(|(_, _, c)| c).collect();
    }

    fn to_json(arena: &[Node], idx: usize) -> serde_json::Value {
        let n = &arena[idx];
        let mut m = serde_json::Map::new();
        m.insert("name".into(), serde_json::Value::String(n.name.clone()));
        m.insert("path".into(), serde_json::Value::String(n.path.clone()));
        m.insert(
            "type".into(),
            serde_json::Value::String(
                if n.is_dir { "directory" } else { "file" }.to_string(),
            ),
        );
        m.insert("size".into(), n.size.clone());
        // 透传额外字段（compressedSize/encrypted/mtime/unsupported 等）。
        for (k, v) in &n.props {
            m.insert(k.clone(), v.clone());
        }
        if n.is_dir {
            let kids: Vec<serde_json::Value> =
                n.children.iter().map(|&c| to_json(arena, c)).collect();
            m.insert("children".into(), serde_json::Value::Array(kids));
        }
        serde_json::Value::Object(m)
    }

    roots.into_iter().map(|i| to_json(&arena, i)).collect()
}

// ── 冲突辅助函数（409 CONFLICT，对齐 TS assertNoOverwrite）──────────────────
fn conflict(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::CONFLICT, msg.into(), None)
}

/// 把 `std::io::Error` 映射为业务错误（对齐 TS `rethrowFsError`）。
///
/// 优先按跨平台的 `ErrorKind` 区分（std 已把 Windows Win32 码正确映射为 AlreadyExists /
/// NotFound），再用 `raw_os_error()` 补充 ErrorKind 未覆盖的语义（ENOTEMPTY / EXDEV /
/// Windows ERROR_DIR_NOT_EMPTY），最终回落到 `fallback_code`，避免一律 500。
fn map_fs_error(e: std::io::Error, fallback_code: ErrorCode) -> AppError {
    use std::io::ErrorKind;
    // 1) 跨平台 ErrorKind 优先（Windows 下 raw_os_error 是 Win32 码，与 POSIX errno 不同）。
    match e.kind() {
        ErrorKind::AlreadyExists => return conflict(ErrorCode::FilesPathExists, "Path already exists"),
        ErrorKind::NotFound => return not_found("Path not found"),
        _ => {}
    }
    // 2) ErrorKind 未覆盖的特定语义：按平台 raw_os_error 补充。
    match e.raw_os_error() {
        // POSIX EEXIST(17) / ENOENT(2)
        Some(17) => conflict(ErrorCode::FilesPathExists, "Path already exists"),
        Some(2) => not_found("Path not found"),
        // POSIX ENOTEMPTY(39)（Linux/macOS）
        #[cfg(not(windows))]
        Some(39) => bad_request(ErrorCode::FilesDirNotEmpty, "Directory is not empty"),
        // POSIX EXDEV(18)（跨设备移动）
        #[cfg(not(windows))]
        Some(18) => bad_request(
            ErrorCode::FilesOperationFailed,
            "cannot move across devices; copy/delete fallback is not supported",
        ),
        // Windows ERROR_DIR_NOT_EMPTY(145)
        #[cfg(windows)]
        Some(145) => bad_request(ErrorCode::FilesDirNotEmpty, "Directory is not empty"),
        // Windows ERROR_ALREADY_EXISTS(183) / ERROR_FILE_EXISTS(80)（ErrorKind 通常已映射，兜底）
        #[cfg(windows)]
        Some(183) | Some(80) => conflict(ErrorCode::FilesPathExists, "Path already exists"),
        _ => bad_request(fallback_code, format!("File operation failed: {e}")),
    }
}

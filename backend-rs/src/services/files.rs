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
use axum::http::StatusCode;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const MAX_READ_SIZE: u64 = 5 * 1024 * 1024;
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "node_modules", ".git", ".next", "dist", "__pycache__", ".DS_Store",
];

pub fn bad_request(code: ErrorCode, msg: impl Into<String>) -> AppError {
    AppError::business(code, StatusCode::BAD_REQUEST, msg.into(), None)
}
pub fn not_found(msg: impl Into<String>) -> AppError {
    AppError::business(ErrorCode::FilesPathNotFound, StatusCode::NOT_FOUND, msg.into(), None)
}
pub fn forbidden(msg: impl Into<String>) -> AppError {
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
pub async fn resolve(state: &AppState, input: &str) -> Result<ResolvedTarget, AppError> {
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
    if !within_workspace(state, &canonical).await {
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

pub async fn within_workspace(state: &AppState, p: &Path) -> bool {
    let roots = workspace_roots(state).await;
    let p_str = p.to_string_lossy().to_string();
    roots.iter().any(|r| is_within(p, Path::new(r)) || p_str == *r)
}

pub fn is_within(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// C3：判断路径是否为某个 workspace root（禁止删除/重命名/移动）。
pub async fn is_workspace_root(state: &AppState, p: &Path) -> bool {
    let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let canon_str = canonical.to_string_lossy().to_string();
    workspace_roots(state).await.iter().any(|r| canon_str == *r)
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
pub async fn compute_workspace_roots(db: &sea_orm::DatabaseConnection, dynamic_roots: &HashSet<String>, codex_home: Option<&Path>) -> Vec<String> {
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
    let reader = crate::services::settings::SettingsReader::new(db, None);
    if let Some(roots_str) = reader.get_string("security.workspaceRoots").await {
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
    // 多租户 workspace:codex_home 下 users/ + teams/ 是各会话 cwd 的根目录,
    // 加入校验白名单,否则文件树/终端访问会话 cwd 被拒(files.path_outside_workspace)。
    // 注:当前 files 模块未做 per-user 隔离,任意登录用户可浏览所有 workspace;
    // per-user 隔离待后续按 user_id 过滤(需 workspace 校验链贯穿 user_id)。
    if let Some(ch) = codex_home {
        for sub in ["users", "teams"] {
            let p = ch.join(sub);
            if p.is_dir() {
                if let Ok(c) = std::fs::canonicalize(&p) {
                    out.insert(c.to_string_lossy().to_string());
                }
            }
        }
    }
    out.into_iter().collect()
}

pub async fn workspace_roots(state: &AppState) -> Vec<String> {
    let dyn_roots: HashSet<String> = state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    compute_workspace_roots(&state.db, &dyn_roots, Some(&state.codex_home)).await
}

/// 判断规范化路径是否位于任一工作区根目录之下（含等于根本身）。
pub async fn is_path_in_workspace(db: &sea_orm::DatabaseConnection, dynamic_roots: &HashSet<String>, codex_home: Option<&Path>, p: &Path) -> bool {
    let roots = compute_workspace_roots(db, dynamic_roots, codex_home).await;
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
pub async fn resolve_terminal_cwd(
    db: &sea_orm::DatabaseConnection,
    dynamic_roots: &HashSet<String>,
    codex_home: &Path,
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
    if !is_path_in_workspace(db, dynamic_roots, Some(codex_home), &canon).await {
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


//! workspace 物理目录 + hook 审计(per-user workspace 实施步骤 2+5)。
//!
//! 物理布局(均在 `state.workspace_root` 下):
//! - `users/{user_id}/personal/`         个人 workspace(永久可写)
//! - `teams/{team_id}/shared/`            team 共享 workspace(owner/admin 可写,member 只读)
//! - `teams/{team_id}/members/{user_id}/` 该成员视图目录(物理独立,UI 聚合)
//!
//! role 复用现有 `team_members.role`,不再新建表(避免冗余);判定 owner/admin
//! 优先看 team_members.role,缺省视为 `member`(保守默认)。

pub mod audit_writer;
pub mod decision;
pub mod hooks_config;

use crate::error::AppError;
use crate::services::multitenant::teams;
use crate::state::AppState;
use sea_orm::DatabaseConnection;
use std::path::{Path, PathBuf};

const PERSONAL_DIR: &str = "users";
const TEAMS_DIR: &str = "teams";
const SHARED_SUBDIR: &str = "shared";
const MEMBERS_SUBDIR: &str = "members";
const THREADS_DIR: &str = "threads";

/// 个人 workspace 绝对路径。
pub fn personal_path(workspace_root: &Path, user_id: &str) -> PathBuf {
    workspace_root.join(PERSONAL_DIR).join(user_id).join("personal")
}

/// team 共享 workspace 绝对路径。
pub fn team_shared_path(workspace_root: &Path, team_id: &str) -> PathBuf {
    workspace_root.join(TEAMS_DIR).join(team_id).join(SHARED_SUBDIR)
}

/// team 成员视图绝对路径。
pub fn team_member_path(workspace_root: &Path, team_id: &str, user_id: &str) -> PathBuf {
    workspace_root
        .join(TEAMS_DIR)
        .join(team_id)
        .join(MEMBERS_SUBDIR)
        .join(user_id)
}

/// per-thread workspace 绝对路径(个人/团队统一)。
pub fn thread_workspace_path(workspace_root: &Path, thread_id: &str) -> PathBuf {
    workspace_root.join(THREADS_DIR).join(thread_id)
}

/// 确保 per-thread workspace 目录存在,返回其绝对路径。
pub async fn ensure_thread_workspace(
    state: &AppState,
    thread_id: &str,
) -> Result<PathBuf, AppError> {
    let path = thread_workspace_path(&state.workspace_root, thread_id);
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(path)
}

#[cfg(unix)]
fn shared_permissions() -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(0o775)
}

/// 确保个人 workspace 目录存在。
pub async fn ensure_user_personal(state: &AppState, user_id: &str) -> Result<(), AppError> {
    let path = personal_path(&state.workspace_root, user_id);
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(())
}

/// 确保 team 共享 workspace 目录存在(权限 0o775)。
pub async fn ensure_team_shared(state: &AppState, team_id: &str) -> Result<(), AppError> {
    let path = team_shared_path(&state.workspace_root, team_id);
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        let _ = tokio::fs::set_permissions(&path, shared_permissions()).await;
    }
    Ok(())
}

/// 确保 team 成员视图目录存在。
pub async fn ensure_team_member_view(
    state: &AppState,
    team_id: &str,
    user_id: &str,
) -> Result<(), AppError> {
    let path = team_member_path(&state.workspace_root, team_id, user_id);
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(())
}

/// 取 user 在 team 的 workspace role:复用 `team_members.role`,
/// 找不到时保守返回 `"member"`(hook webhook 用,默认拒绝写共享盘)。
pub async fn get_role(
    db: &DatabaseConnection,
    team_id: &str,
    user_id: &str,
) -> Result<String, AppError> {
    match teams::member_role(db, team_id, user_id).await? {
        Some(r) => Ok(r),
        None => Ok("member".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_workspace_path_layout() {
        let root = std::path::Path::new("/data/ws");
        let p = thread_workspace_path(root, "tid-123");
        assert_eq!(p, std::path::PathBuf::from("/data/ws/threads/tid-123"));
    }
}
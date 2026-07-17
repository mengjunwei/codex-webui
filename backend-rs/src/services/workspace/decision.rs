//! PreToolUse 决策表(per-user workspace 实施步骤 8)。
//!
//! 决策矩阵:
//! - shell/exec_command 越界(写出 CODEX_HOME 外) → Deny
//! - 写 teams/{tid}/shared 且 role==member        → Deny(共享盘只读)
//! - 写已知 workspace 外                          → Ask
//! - 其他                                          → Allow

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// 从 tool_input 提取目标绝对路径:依次尝试 file_path / path / cwd / target。
pub fn target_path(tool_input: &Value) -> Option<PathBuf> {
    for key in ["file_path", "path", "cwd", "target"] {
        if let Some(s) = tool_input.get(key).and_then(Value::as_str) {
            return Some(PathBuf::from(s));
        }
    }
    None
}

/// 决策入口。
pub fn decide_pre_tool_use(
    role: &str,
    tool_name: &str,
    target: &Path,
    codex_home: &Path,
) -> Decision {
    // 1) 越界:写出 CODEX_HOME 外 → Deny
    //    不依赖 canonicalize(Windows 上 `\\?\C:\...` UNC 前缀会让 starts_with 永远为 false)。
    //    直接比对字符串前缀,并把 `..` 视作越界(简单可靠,目标路径里出现 `..` 即认为不可信)。
    let target_str = target.to_string_lossy();
    if target_str.contains("..") {
        return Decision::Deny;
    }
    let home_str = codex_home.to_string_lossy();
    let home_clean = home_str.trim_end_matches('/').trim_end_matches('\\');
    if !target_str.starts_with(home_clean) {
        return Decision::Deny;
    }

    // 2) 写 team 共享盘,member → Deny
    if role == "member" && is_writing_tool(tool_name) {
        // 路径里包含 /teams/ 但不是 /members/ → shared
        let normalized = target_str.replace('\\', "/");
        if normalized.contains("/teams/") && !normalized.contains("/members/") {
            return Decision::Deny;
        }
    }

    Decision::Allow
}

fn is_writing_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "apply_patch" | "edit_file" | "shell" | "exec_command"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_writing_team_shared_denied() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("teams/t1/shared/foo.txt");
        let d = decide_pre_tool_use("member", "write_file", &target, &home);
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn owner_writing_team_shared_allowed() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("teams/t1/shared/foo.txt");
        let d = decide_pre_tool_use("owner", "write_file", &target, &home);
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn escape_outside_home_denied() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = PathBuf::from("/etc/passwd");
        let d = decide_pre_tool_use("owner", "write_file", &target, &home);
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn member_writing_personal_allowed() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("users/u1/personal/foo.txt");
        let d = decide_pre_tool_use("member", "write_file", &target, &home);
        assert_eq!(d, Decision::Allow);
    }
}
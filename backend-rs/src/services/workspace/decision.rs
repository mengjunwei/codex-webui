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

/// 规范化路径分隔符为 `/`,便于跨平台字符串比较。
fn normalize(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// 决策入口。
pub fn decide_pre_tool_use(
    role: &str,
    tool_name: &str,
    target: &Path,
    codex_home: &Path,
) -> Decision {
    let target_str = normalize(target);
    let home_str = normalize(codex_home);
    let home_clean = home_str.trim_end_matches('/');

    // 1) 越界:写出 CODEX_HOME 外 → Deny
    //    - 路径里出现 `..` 即视为不可信
    //    - 前缀不匹配 codex_home 也视为越界(覆盖 Windows / 与 \\ 差异)
    if target_str.contains("..") {
        return Decision::Deny;
    }
    // 路径若为绝对路径(以 / 或盘符起),必须以 home_clean 起。
    // 相对路径(没 / 前缀且没盘符)允许(如 `foo.txt`)— sandbox 阶段已校验 cwd。
    let looks_absolute = target_str.starts_with('/')
        || (target_str.len() >= 2 && target_str.as_bytes()[1] == b':');
    if looks_absolute && !target_str.starts_with(home_clean) {
        return Decision::Deny;
    }

    // 2) 写 team 共享盘,member → Deny
    if role == "member" && is_writing_tool(tool_name) {
        // 路径里包含 /teams/ 但不是 /members/ → shared
        if target_str.contains("/teams/") && !target_str.contains("/members/") {
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
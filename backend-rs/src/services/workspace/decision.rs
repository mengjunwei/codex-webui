//! PreToolUse 决策表(per-user workspace 实施步骤 8)。
//!
//! 决策矩阵:
//! - shell/exec_command 越界(写出 workspace_root 外) → Deny
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

/// 规范化路径:统一斜杠 + 规范化 Windows 盘符前缀。
///
/// - `\\` → `/`
/// - `C:/...` → `/c/...`(小写盘符,去冒号,加前缀斜杠)
/// - `/c/...` 保持不变(Git Bash 路径已经是此格式)
///
/// 这样 `C:\Users\...` 和 `/c/Users/...` 规范化后相同,`starts_with` 比较正确。
fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    normalize_str(&s)
}

fn normalize_str(s: &str) -> String {
    // 先统一斜杠
    let s = s.replace('\\', "/");
    // 处理 Windows 盘符前缀:C:/... → /c/...
    let mapped = if s.len() >= 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.as_bytes()[1] == b':' {
        let drive = (s.as_bytes()[0] as char).to_ascii_lowercase();
        format!("/{}{}", drive, &s[2..])
    } else {
        s
    };
    // Windows 文件系统不区分大小写:整体小写,让 C:/Users/Foo 与 /c/users/foo 归一,
    // 否则 is_within_home 会把同一路径的不同大小写写法误判为越界(Deny 误杀)。
    // Unix 区分大小写,保持原样。
    #[cfg(windows)]
    { mapped.to_ascii_lowercase() }
    #[cfg(not(windows))]
    { mapped }
}

/// 决策入口。
pub fn decide_pre_tool_use(
    role: &str,
    tool_name: &str,
    target: &Path,
    workspace_root: &Path,
) -> Decision {
    let target_str = normalize(target);
    let home_str = normalize(workspace_root);
    let home_clean = home_str.trim_end_matches('/');

    // 1) 越界:写出 workspace_root 外 → Deny
    //    - 路径里出现 `..` 即视为不可信
    //    - 前缀不匹配 workspace_root 也视为越界(覆盖 Windows / 与 \\ 差异)
    if target_str.contains("..") {
        return Decision::Deny;
    }
    // 路径若为绝对路径(以 / 或盘符起),必须落在 workspace_root 内。
    // 相对路径(没 / 前缀且没盘符)允许(如 `foo.txt`)— sandbox 阶段已校验 cwd。
    let looks_absolute = target_str.starts_with('/')
        || (target_str.len() >= 2 && target_str.as_bytes()[1] == b':');
    if looks_absolute && !is_within_home(&target_str, home_clean) {
        return Decision::Deny;
    }

    // 2) 写 team 共享盘,member → Deny
    if role == "member" && is_writing_tool(tool_name) {
        // 精确匹配 teams/{tid}/shared 结构,而非启发式 "含 /teams/ 且不含 /members/"。
        // 否则 member 写 teams/{tid}/shared/members/evil.txt 会因含 /members/ 被误判为
        // member view → 绕过共享盘只读限制。所有 codex 进程同用户跑,OS 层 shared 可写,
        // decision 是唯一防线,必须精确。
        if is_team_shared_path(&target_str) {
            return Decision::Deny;
        }
    }

    // 3) shell/exec_command:命令字符串动态,target 通常是 cwd(回落),无法静态判断是否
    // 写共享盘。member 经 `echo > ../../teams/t1/shared/x` 或 `cp … /shared/` 即可绕过
    // 上方路径校验(target=cwd=personal → Allow)。OS 层 shared 可写,故对 member 的 shell
    // 类工具保守 Ask(前端审批),不直接放行。
    if role == "member" && is_shell_tool(tool_name) {
        return Decision::Ask;
    }

    Decision::Allow
}

/// 判断是否 shell 类工具(命令字符串动态,无法静态分析写目标)。
fn is_shell_tool(name: &str) -> bool {
    matches!(name, "shell" | "exec_command")
}

/// 判断路径是否为 team 共享盘(`teams/{tid}/shared` 及其子路径)。
///
/// 精确匹配 `/teams/{tid}/shared(/|$)`,避免启发式 `contains("/members/")` 被
/// `teams/.../shared/members/...` 这类插入 /members/ 段的路径绕过。
fn is_team_shared_path(target: &str) -> bool {
    let Some(idx) = target.find("/teams/") else {
        return false;
    };
    // after_teams 形如 "{tid}/shared/..." 或 "{tid}/shared"
    let after_teams = &target[idx + "/teams/".len()..];
    let Some(slash) = after_teams.find('/') else {
        return false; // 仅有 "{tid}",无后续 shared 段
    };
    let rest = &after_teams[slash + 1..];
    rest == "shared" || rest.starts_with("shared/")
}

fn is_writing_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "apply_patch" | "edit_file" | "shell" | "exec_command"
    )
}

/// 判断 target 是否落在 codex_home 内(路径段边界匹配)。
///
/// 必须用边界分隔符比较,而非裸 `starts_with(home_clean)`:
/// 否则 `home_clean = "/c/codex/home"` 会把 `/c/codex/home-evil/...`
/// (home 的同级目录,已逃逸 CODEX_HOME)误判为"以 home 起"。
///
/// 合法情况:
/// - target == home_clean(恰好等于 home 本身)
/// - target 以 `home_clean + "/"` 起(home 的子路径)
fn is_within_home(target: &str, home_clean: &str) -> bool {
    if target == home_clean {
        return true;
    }
    let mut prefix = home_clean.to_string();
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    target.starts_with(&prefix)
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

    /// 回归:home 的同级目录(前缀字符串相同但非子路径)必须 Deny,
    /// 防裸 starts_with 前缀混淆穿越。
    #[test]
    fn sibling_directory_with_shared_prefix_denied() {
        // codex_home 规范化为 /.../codex/home;攻击路径 /.../codex/home-evil/x
        // 裸 starts_with("/.../codex/home") 会误判 true,这里必须 Deny。
        let home = PathBuf::from("/c/codex/home");
        let evil = PathBuf::from("/c/codex/home-evil/secret.txt");
        let d = decide_pre_tool_use("owner", "write_file", &evil, &home);
        assert_eq!(d, Decision::Deny, "sibling dir sharing home prefix must be denied");

        // 对照:真正的 home 子路径必须 Allow(owner)。
        let ok = PathBuf::from("/c/codex/home/users/u1/personal/foo.txt");
        let d = decide_pre_tool_use("owner", "write_file", &ok, &home);
        assert_eq!(d, Decision::Allow, "true child of home must be allowed");

        // 另一种混淆:home- 后接字母
        let evil2 = PathBuf::from("/c/codex/homexyz/foo");
        let d = decide_pre_tool_use("owner", "write_file", &evil2, &home);
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn member_writing_personal_allowed() {
        let home = std::env::temp_dir().join("ws-test-home");
        let target = home.join("users/u1/personal/foo.txt");
        let d = decide_pre_tool_use("member", "write_file", &target, &home);
        assert_eq!(d, Decision::Allow);
    }

    /// 回归:member 在 shared 路径中插入 /members/ 段企图绕过只读限制,必须 Deny。
    #[test]
    fn member_cannot_bypass_shared_via_members_segment() {
        let home = PathBuf::from("/c/codex/home");
        // shared 下伪造 members 子路径 —— 旧启发式会误判为 member view 放行。
        let evil = PathBuf::from("/c/codex/home/teams/t1/shared/members/evil.txt");
        let d = decide_pre_tool_use("member", "write_file", &evil, &home);
        assert_eq!(d, Decision::Deny, "shared/members/... must not bypass read-only");

        // 正常 shared 写 —— owner Allow。
        let d = decide_pre_tool_use("owner", "write_file", &evil, &home);
        assert_eq!(d, Decision::Allow);

        // 正常 member view 写 —— 不在 shared 下,member Allow。
        let mv = PathBuf::from("/c/codex/home/teams/t1/members/u1/foo.txt");
        let d = decide_pre_tool_use("member", "write_file", &mv, &home);
        assert_eq!(d, Decision::Allow, "member writing own view must be allowed");

        // member view 下恰好有 shared 命名的子目录 —— 非 team shared,member Allow。
        let mv_shared = PathBuf::from("/c/codex/home/teams/t1/members/u1/shared/note.txt");
        let d = decide_pre_tool_use("member", "write_file", &mv_shared, &home);
        assert_eq!(d, Decision::Allow, "shared-named subdir under member view is not team shared");
    }

    /// Windows 盘符路径 + Git Bash 路径混合比较:确保 normalize 正确。
    #[test]
    fn windows_drive_letter_paths_match() {
        // codex_home 是 Windows 原生路径(C:\Users\...)
        let home = PathBuf::from("C:\\Users\\admin\\.codex-webui\\home");
        // hook payload 里 file_path 是 Git Bash 路径格式(/c/Users/...)
        let target = PathBuf::from("/c/Users/admin/.codex-webui/home/teams/t1/shared/foo.txt");
        let d = decide_pre_tool_use("owner", "write_file", &target, &home);
        assert_eq!(d, Decision::Allow, "owner writing shared should allow even with mixed path formats");

        let d = decide_pre_tool_use("member", "write_file", &target, &home);
        assert_eq!(d, Decision::Deny, "member writing shared should deny");
    }

    #[test]
    fn normalize_str_handles_drive_letter() {
        // Windows 文件系统不区分大小写,normalize 整体小写归一;Unix 保留大小写。
        #[cfg(windows)]
        {
            assert_eq!(normalize_str("C:\\Users\\admin"), "/c/users/admin");
            assert_eq!(normalize_str("D:/code/rust"), "/d/code/rust");
            assert_eq!(normalize_str("/c/Users/admin"), "/c/users/admin");
            assert_eq!(normalize_str("/etc/passwd"), "/etc/passwd");
            assert_eq!(normalize_str("relative/path"), "relative/path");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(normalize_str("C:\\Users\\admin"), "/c/Users/admin");
            assert_eq!(normalize_str("D:/code/rust"), "/d/code/rust");
            assert_eq!(normalize_str("/c/Users/admin"), "/c/Users/admin");
            assert_eq!(normalize_str("/etc/passwd"), "/etc/passwd");
            assert_eq!(normalize_str("relative/path"), "relative/path");
        }
    }
}
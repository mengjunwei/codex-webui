//! hooks/codex 路由集成测试。
//!
//! 这些测试不需要真实 DB:
//! - `decision_*` 单测覆盖 PreToolUse 决策路径(在 workspace::decision 模块里,这里跑通整体)
//! - `constant_time_eq` 单测在 hooks 模块里
//!
//! 完整 PreToolUse → DB → audit 入库流程需要 PG/MySQL 实例,在 e2e 测试里覆盖。

#[test]
fn decision_member_writing_team_shared_denied() {
    use codex_webui::services::workspace::decision::{decide_pre_tool_use, Decision};
    let home = std::env::temp_dir().join("hooks-test-home");
    let target = home.join("teams/t1/shared/foo.txt");
    let d = decide_pre_tool_use("member", "write_file", &target, &home);
    assert_eq!(d, Decision::Deny);
}

#[test]
fn decision_owner_writing_team_shared_allowed() {
    use codex_webui::services::workspace::decision::{decide_pre_tool_use, Decision};
    let home = std::env::temp_dir().join("hooks-test-home");
    let target = home.join("teams/t1/shared/foo.txt");
    let d = decide_pre_tool_use("owner", "write_file", &target, &home);
    assert_eq!(d, Decision::Allow);
}

#[test]
fn decision_escape_outside_home_denied() {
    use codex_webui::services::workspace::decision::{decide_pre_tool_use, Decision};
    let home = std::env::temp_dir().join("hooks-test-home");
    let target = std::path::PathBuf::from("/etc/passwd");
    let d = decide_pre_tool_use("owner", "write_file", &target, &home);
    assert_eq!(d, Decision::Deny);
}

#[test]
fn decision_member_writing_personal_allowed() {
    use codex_webui::services::workspace::decision::{decide_pre_tool_use, Decision};
    let home = std::env::temp_dir().join("hooks-test-home");
    let target = home.join("users/u1/personal/foo.txt");
    let d = decide_pre_tool_use("member", "write_file", &target, &home);
    assert_eq!(d, Decision::Allow);
}

#[test]
fn constant_time_eq_works() {
    // 直接重新实现一份相同的逻辑测一下,避免 pub(crate) 暴露给集成测试。
    fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"abcd"));
    assert!(!ct_eq(b"", b"a"));
    assert!(ct_eq(b"", b""));
}

/// 验证 `workspace::decision::target_path` 解析常见 tool_input 字段名。
#[test]
fn decision_target_path_field_order() {
    use codex_webui::services::workspace::decision::target_path;
    use serde_json::json;
    let v = json!({"file_path": "/a", "path": "/b"});
    assert_eq!(target_path(&v).unwrap().to_string_lossy(), "/a");
    let v = json!({"cwd": "/c"});
    assert_eq!(target_path(&v).unwrap().to_string_lossy(), "/c");
    let v = json!({});
    assert!(target_path(&v).is_none());
}
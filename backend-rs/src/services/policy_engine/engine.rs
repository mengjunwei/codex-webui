//! 策略规则匹配引擎。

use super::{MatchMode, PolicyAction, PolicyDecision, PolicyInput, PolicyRule, RuleType};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// 匹配引擎。
///
/// 规则按 `scope(team > global)`、`priority 降序`、`id 升序` 排序，
/// 取第一个命中的规则决定 allow/deny；无命中则默认 Allow(fail-open)。
#[derive(Clone, Default)]
pub struct PolicyEngine {
    rules: Arc<Vec<PolicyRule>>,
    /// 缓存已编译的正则，避免重复编译。
    regex_cache: Arc<HashMap<String, Regex>>,
}

impl PolicyEngine {
    /// 用规则列表构造引擎。输入列表无需排序，构造时完成排序与正则缓存。
    pub fn new(rules: Vec<PolicyRule>) -> Self {
        let mut sorted = rules;
        sorted.sort_by(|a, b| {
            let scope_order = |s: &_| match s {
                super::PolicyScope::Team => 0,
                super::PolicyScope::Global => 1,
            };
            scope_order(&a.scope)
                .cmp(&scope_order(&b.scope))
                .then_with(|| b.priority.cmp(&a.priority))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut cache = HashMap::new();
        for r in &sorted {
            if r.match_mode == MatchMode::Regex && !cache.contains_key(&r.pattern) {
                if let Ok(re) = Regex::new(&r.pattern) {
                    cache.insert(r.pattern.clone(), re);
                }
            }
        }

        Self {
            rules: Arc::new(sorted),
            regex_cache: Arc::new(cache),
        }
    }

    /// 对一次工具调用进行策略判定。
    pub fn evaluate<F>(
        &self,
        input: PolicyInput,
        mut role_provider: F,
    ) -> PolicyDecision
    where
        F: FnMut(&str, &str, &str) -> bool,
    {
        for rule in self.rules.iter().filter(|r| r.enabled) {
            // role 过滤：global 规则可不指定 role；team 规则也可不指定 role（作用于全团队）。
            if let Some(ref required_role) = rule.role {
                if !role_provider(input.team_id, input.user_id, required_role) {
                    continue;
                }
            }

            let target = match rule.rule_type {
                RuleType::Command => match extract_command(input.tool_input) {
                    Some(v) => v,
                    None => continue,
                },
                RuleType::Tool => input.tool_name.to_string(),
                RuleType::Skill => match extract_skill(input.tool_input, input.tool_name) {
                    Some(v) => v,
                    None => continue,
                },
                RuleType::Plugin => match extract_plugin(input.tool_input, input.tool_name) {
                    Some(v) => v,
                    None => continue,
                },
                RuleType::Mcp => match extract_mcp(input.tool_input, input.tool_name) {
                    Some(v) => v,
                    None => continue,
                },
            };

            let matched = match rule.match_mode {
                MatchMode::Exact => target == rule.pattern,
                MatchMode::Blacklist | MatchMode::Whitelist => {
                    target.to_ascii_lowercase().contains(&rule.pattern.to_ascii_lowercase())
                }
                MatchMode::Regex => match self.regex_cache.get(&rule.pattern) {
                    Some(re) => re.is_match(&target),
                    None => continue,
                },
            };

            if matched {
                let reason = format!(
                    "命中{}策略 {} ({} / {} / priority={})",
                    rule.scope, rule.id, rule.rule_type, rule.match_mode, rule.priority
                );
                return match rule.action {
                    PolicyAction::Allow => PolicyDecision::Allow,
                    PolicyAction::Deny => PolicyDecision::Deny { rule_id: rule.id.clone(), reason },
                };
            }
        }

        PolicyDecision::Allow
    }
}

/// 从 tool_input 提取命令字符串。
/// 兼容 codex shell 工具常见字段：`command`、`args`、`cmd`。
fn extract_command(tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?;
    for key in ["command", "cmd", "args"] {
        if let Some(v) = input.get(key) {
            if let Some(s) = v.as_str() {
                return Some(s.to_string());
            }
            // args 为数组时拼接成字符串
            if let Some(arr) = v.as_array() {
                let joined: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
                if !joined.is_empty() {
                    return Some(joined.join(" "));
                }
            }
        }
    }
    None
}

fn extract_skill(tool_input: Option<&Value>, tool_name: &str) -> Option<String> {
    if let Some(input) = tool_input {
        if let Some(v) = input.get("skill").and_then(|x| x.as_str()) {
            return Some(v.to_string());
        }
    }
    // 某些 skill 调用 tool_name 即 skill 名
    Some(tool_name.to_string())
}

fn extract_plugin(tool_input: Option<&Value>, tool_name: &str) -> Option<String> {
    if let Some(input) = tool_input {
        if let Some(v) = input.get("plugin").and_then(|x| x.as_str()) {
            return Some(v.to_string());
        }
    }
    Some(tool_name.to_string())
}

fn extract_mcp(tool_input: Option<&Value>, tool_name: &str) -> Option<String> {
    if let Some(input) = tool_input {
        if let Some(v) = input.get("server").and_then(|x| x.as_str()) {
            return Some(v.to_string());
        }
        if let Some(v) = input.get("mcp_server").and_then(|x| x.as_str()) {
            return Some(v.to_string());
        }
    }
    Some(tool_name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::policy_engine::{PolicyScope, PolicyRule};

    fn rule(
        id: &str,
        scope: PolicyScope,
        rule_type: RuleType,
        mode: MatchMode,
        pattern: &str,
        action: PolicyAction,
        priority: i32,
    ) -> PolicyRule {
        PolicyRule {
            id: id.to_string(),
            scope,
            team_id: None,
            role: None,
            rule_type,
            match_mode: mode,
            pattern: pattern.to_string(),
            action,
            priority,
            enabled: true,
        }
    }

    fn input(tool_name: &str, tool_input: Option<&Value>) -> PolicyInput {
        PolicyInput {
            team_id: "t1",
            user_id: "u1",
            role: "member",
            tool_name,
            tool_input,
        }
    }

    #[test]
    fn exact_deny_command() {
        let engine = PolicyEngine::new(vec![rule(
            "r1",
            PolicyScope::Global,
            RuleType::Command,
            MatchMode::Exact,
            "rm -rf /",
            PolicyAction::Deny,
            100,
        )]);
        let tool_input = serde_json::json!({"command": "rm -rf /"});
        let decision = engine.evaluate(input("shell", Some(&tool_input)), |_, _, _| true);
        assert!(
            matches!(decision, PolicyDecision::Deny { rule_id, .. } if rule_id == "r1"),
            "expected deny, got {:?}",
            decision
        );
    }

    #[test]
    fn blacklist_deny_substring() {
        let engine = PolicyEngine::new(vec![rule(
            "r1",
            PolicyScope::Global,
            RuleType::Command,
            MatchMode::Blacklist,
            "DROP TABLE",
            PolicyAction::Deny,
            100,
        )]);
        let tool_input = serde_json::json!({"args": ["mysql", "-e", "DROP TABLE users"]});
        let decision = engine.evaluate(input("shell", Some(&tool_input)), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[test]
    fn whitelist_allow_skill() {
        let engine = PolicyEngine::new(vec![
            rule(
                "r1",
                PolicyScope::Global,
                RuleType::Skill,
                MatchMode::Whitelist,
                "safe-skill",
                PolicyAction::Allow,
                100,
            ),
            rule(
                "r2",
                PolicyScope::Global,
                RuleType::Skill,
                MatchMode::Blacklist,
                "dangerous",
                PolicyAction::Deny,
                90,
            ),
        ]);
        let tool_input = serde_json::json!({"skill": "safe-skill"});
        let decision = engine.evaluate(input("skill", Some(&tool_input)), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn regex_deny_tool() {
        let engine = PolicyEngine::new(vec![rule(
            "r1",
            PolicyScope::Global,
            RuleType::Tool,
            MatchMode::Regex,
            r"^shell\.(exec|write)\b",
            PolicyAction::Deny,
            100,
        )]);
        let decision = engine.evaluate(input("shell.exec", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
        let decision = engine.evaluate(input("shell.read", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn team_rule_overrides_global() {
        let engine = PolicyEngine::new(vec![
            rule(
                "global-deny",
                PolicyScope::Global,
                RuleType::Tool,
                MatchMode::Exact,
                "x",
                PolicyAction::Deny,
                100,
            ),
            rule(
                "team-allow",
                PolicyScope::Team,
                RuleType::Tool,
                MatchMode::Exact,
                "x",
                PolicyAction::Allow,
                100,
            ),
        ]);
        let decision = engine.evaluate(input("x", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn higher_priority_wins_same_scope() {
        let engine = PolicyEngine::new(vec![
            rule(
                "low",
                PolicyScope::Global,
                RuleType::Tool,
                MatchMode::Exact,
                "x",
                PolicyAction::Allow,
                10,
            ),
            rule(
                "high",
                PolicyScope::Global,
                RuleType::Tool,
                MatchMode::Exact,
                "x",
                PolicyAction::Deny,
                100,
            ),
        ]);
        let decision = engine.evaluate(input("x", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Deny { rule_id, .. } if rule_id == "high"));
    }

    #[test]
    fn disabled_rule_ignored() {
        let mut r = rule(
            "r1",
            PolicyScope::Global,
            RuleType::Tool,
            MatchMode::Exact,
            "x",
            PolicyAction::Deny,
            100,
        );
        r.enabled = false;
        let engine = PolicyEngine::new(vec![r]);
        let decision = engine.evaluate(input("x", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn role_filter_applied() {
        let mut r = rule(
            "r1",
            PolicyScope::Team,
            RuleType::Tool,
            MatchMode::Exact,
            "x",
            PolicyAction::Deny,
            100,
        );
        r.role = Some("admin".to_string());
        let engine = PolicyEngine::new(vec![r]);
        let decision = engine.evaluate(input("x", None), |_, _, role| role == "admin");
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
        let decision = engine.evaluate(input("x", None), |_, _, role| role == "member");
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn no_match_defaults_allow() {
        let engine = PolicyEngine::new(vec![]);
        let decision = engine.evaluate(input("anything", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }

    #[test]
    fn invalid_regex_ignored() {
        let engine = PolicyEngine::new(vec![rule(
            "r1",
            PolicyScope::Global,
            RuleType::Tool,
            MatchMode::Regex,
            "(",
            PolicyAction::Deny,
            100,
        )]);
        let decision = engine.evaluate(input("x", None), |_, _, _| true);
        assert!(matches!(decision, PolicyDecision::Allow));
    }
}

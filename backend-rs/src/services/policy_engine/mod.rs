//! 策略引擎：命令审查与 skill/plugin/mcp 使用限制。

pub mod dto;
pub mod engine;
pub mod store;

pub use engine::PolicyEngine;
pub use store::PolicyStore;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyScope {
    Global,
    Team,
}

impl std::fmt::Display for PolicyScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Global => "global",
                Self::Team => "team",
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    Command,
    Tool,
    Skill,
    Plugin,
    Mcp,
}

impl std::fmt::Display for RuleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Command => "command",
                Self::Tool => "tool",
                Self::Skill => "skill",
                Self::Plugin => "plugin",
                Self::Mcp => "mcp",
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    Blacklist,
    Whitelist,
    Regex,
    Exact,
}

impl std::fmt::Display for MatchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Blacklist => "blacklist",
                Self::Whitelist => "whitelist",
                Self::Regex => "regex",
                Self::Exact => "exact",
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    Allow,
    Deny,
}

impl std::fmt::Display for PolicyAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Allow => "allow",
                Self::Deny => "deny",
            }
        )
    }
}

#[derive(Debug, Clone)]
pub struct PolicyRule {
    pub id: String,
    pub scope: PolicyScope,
    pub team_id: Option<String>,
    pub role: Option<String>,
    pub rule_type: RuleType,
    pub match_mode: MatchMode,
    pub pattern: String,
    pub action: PolicyAction,
    pub priority: i32,
    pub enabled: bool,
}

pub struct PolicyInput<'a> {
    pub team_id: &'a str,
    pub user_id: &'a str,
    pub role: &'a str,
    pub tool_name: &'a str,
    pub tool_input: Option<&'a Value>,
}

#[derive(Debug, Clone)]
pub enum PolicyDecision {
    Allow,
    Deny { rule_id: String, reason: String },
}

//! 策略规则存储与缓存层。

use super::{PolicyEngine, PolicyRule, PolicyScope};
use crate::db::entities::tool_policy::{Column, Entity as ToolPolicyEntity, Model};
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter, ColumnTrait};
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_TTL_MS: i64 = 60_000;

/// 规则存储：进程内缓存 + DB 回源 + TTL 兜底。
#[derive(Clone)]
pub struct PolicyStore {
    db: DatabaseConnection,
    cached_engine: Arc<RwLock<(PolicyEngine, i64)>>,
    ttl_ms: i64,
}

impl PolicyStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            cached_engine: Arc::new(RwLock::new((PolicyEngine::default(), 0))),
            ttl_ms: DEFAULT_TTL_MS,
        }
    }

    /// 从 DB 重新加载全部启用的规则，刷新缓存。
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let models: Vec<Model> = ToolPolicyEntity::find()
            .filter(Column::Enabled.eq(true))
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("db: {e}"))?;

        let rules: Vec<PolicyRule> = models.into_iter().map(into_policy_rule).collect();
        let engine = PolicyEngine::new(rules);
        let now = crate::services::multitenant::now_ms();
        *self.cached_engine.write().await = (engine, now);
        Ok(())
    }

    /// 获取当前策略引擎（缓存过期时自动回源）。
    pub async fn engine(&self) -> PolicyEngine {
        {
            let lock = self.cached_engine.read().await;
            let now = crate::services::multitenant::now_ms();
            if now - lock.1 < self.ttl_ms {
                return lock.0.clone();
            }
        }
        if let Err(e) = self.refresh().await {
            tracing::warn!("policy store refresh failed: {e}");
        }
        self.cached_engine.read().await.0.clone()
    }

    /// 手动失效缓存（写入后调用，配合 Redis 广播实现集群一致）。
    pub async fn invalidate(&self) {
        *self.cached_engine.write().await = (PolicyEngine::default(), 0);
    }
}

fn into_policy_rule(m: Model) -> PolicyRule {
    PolicyRule {
        id: m.id,
        scope: parse_scope(&m.scope),
        team_id: m.team_id,
        role: m.role,
        rule_type: parse_rule_type(&m.rule_type),
        match_mode: parse_match_mode(&m.match_mode),
        pattern: m.pattern,
        action: parse_action(&m.action),
        priority: m.priority,
        enabled: m.enabled,
    }
}

fn parse_scope(s: &str) -> PolicyScope {
    match s {
        "team" => PolicyScope::Team,
        _ => PolicyScope::Global,
    }
}

fn parse_rule_type(s: &str) -> super::RuleType {
    match s {
        "command" => super::RuleType::Command,
        "tool" => super::RuleType::Tool,
        "skill" => super::RuleType::Skill,
        "plugin" => super::RuleType::Plugin,
        "mcp" => super::RuleType::Mcp,
        _ => super::RuleType::Tool,
    }
}

fn parse_match_mode(s: &str) -> super::MatchMode {
    match s {
        "blacklist" => super::MatchMode::Blacklist,
        "whitelist" => super::MatchMode::Whitelist,
        "regex" => super::MatchMode::Regex,
        "exact" => super::MatchMode::Exact,
        _ => super::MatchMode::Blacklist,
    }
}

fn parse_action(s: &str) -> super::PolicyAction {
    match s {
        "allow" => super::PolicyAction::Allow,
        _ => super::PolicyAction::Deny,
    }
}

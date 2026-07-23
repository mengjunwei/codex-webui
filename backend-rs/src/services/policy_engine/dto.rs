//! 策略管理 REST API 的 DTO。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PolicyRuleDto {
    pub id: String,
    pub scope: String,
    pub team_id: Option<String>,
    pub role: Option<String>,
    pub rule_type: String,
    pub match_mode: String,
    pub pattern: String,
    pub action: String,
    pub priority: i32,
    pub enabled: bool,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct CreatePolicyRuleDto {
    pub scope: String,
    pub team_id: Option<String>,
    pub role: Option<String>,
    pub rule_type: String,
    pub match_mode: String,
    pub pattern: String,
    pub action: String,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Clone, ToSchema)]
pub struct UpdatePolicyRuleDto {
    pub role: Option<String>,
    pub rule_type: Option<String>,
    pub match_mode: Option<String>,
    pub pattern: Option<String>,
    pub action: Option<String>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct PolicyRuleListResponse {
    pub items: Vec<PolicyRuleDto>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct PolicyRuleResponse {
    pub rule: PolicyRuleDto,
}

#[derive(Debug, Serialize, Clone)]
pub struct PolicyEvaluationResponse {
    pub decision: String,
    pub matched_rule_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PolicyEvaluationRequest {
    pub team_id: String,
    pub user_id: String,
    pub role: String,
    pub tool_name: String,
    pub tool_input: Option<serde_json::Value>,
}

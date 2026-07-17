//! codex hook webhook(per-user workspace 实施步骤 9)。
//!
//! 路由:`POST /hooks/codex`,独立鉴权(X-Hook-Token == INTERNAL_HOOK_TOKEN,常量时间比较)。
//! 失败语义:任何内部异常 → 200 + continue=true(fail-open,不阻断 codex)。

use crate::error::AppError;
use crate::services::workspace as ws;
use crate::services::workspace::decision::{decide_pre_tool_use, Decision};
use crate::state::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// codex 推过来的 hook 事件(字段命名以 codex 0.142.5 hooks 协议为准;实施时按真实 payload 校正)。
#[derive(Debug, Deserialize)]
pub struct HookPayload {
    /// 事件类型:PreToolUse / PostToolUse / SessionStart / SessionEnd / Stop /
    /// SubagentStop / UserPromptSubmit / Notification / PreCompact。
    #[serde(rename = "hook_event")]
    pub hook_event: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_output: Option<Value>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Serialize)]
pub struct HookResponse {
    #[serde(rename = "continue")]
    pub continue_: bool,
    #[serde(skip_serializing_if = "Option::is_none", rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Serialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: &'static str, // allow|deny|ask
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedInput")]
    pub updated_input: Option<Value>,
}

/// 路由入口。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::Json<HookPayload>,
) -> impl IntoResponse {
    // 1) 验签(X-Hook-Token == INTERNAL_HOOK_TOKEN,常量时间比较)
    let token_ok = headers
        .get("x-hook-token")
        .and_then(|v| v.to_str().ok())
        .map(|t| constant_time_eq(t.as_bytes(), state.hook_token.as_bytes()))
        .unwrap_or(false);
    if !token_ok {
        return (StatusCode::UNAUTHORIZED, "invalid hook token").into_response();
    }

    // 2) fail-open:包一层,任何内部异常返回 continue=true
    let resp = handle_inner(&state, body.0).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "hook inner failed (fail-open)");
        HookResponse {
            continue_: true,
            hook_specific_output: None,
        }
    });
    axum::Json(resp).into_response()
}

async fn handle_inner(state: &AppState, payload: HookPayload) -> Result<HookResponse, AppError> {
    let team = payload.team_id.clone().unwrap_or_default();
    let user = payload.user_id.clone().unwrap_or_default();
    let event_type = payload.hook_event.clone();

    // PreToolUse:走决策表
    if event_type == "PreToolUse" {
        let role = ws::get_role(&state.db, &team, &user).await?;
        let tool_name = payload.tool_name.clone().unwrap_or_default();
        let target = payload
            .tool_input
            .as_ref()
            .and_then(ws::decision::target_path)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(payload.cwd.clone().unwrap_or_default())
            });

        let decision = decide_pre_tool_use(&role, &tool_name, &target, &state.codex_home);
        let perm = match decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        };

        state.audit_writer.submit(ws::audit_writer::AuditEvent {
            team_id: Some(team.clone()),
            user_id: Some(user.clone()),
            thread_id: payload.session_id.clone(),
            event_type: event_type.clone(),
            tool_name: Some(tool_name),
            payload: payload.tool_input.clone().unwrap_or(Value::Null),
            decision: Some(perm.to_string()),
        });

        return Ok(HookResponse {
            continue_: perm != "deny",
            hook_specific_output: Some(HookSpecificOutput {
                permission_decision: perm,
                updated_input: None,
            }),
        });
    }

    // SessionStart:active_rollout 注册(沿用现有机制)
    if event_type == "SessionStart" {
        if let (Some(sid), Some(cwd)) = (payload.session_id.as_ref(), payload.cwd.as_ref()) {
            let mut map = state.active_rollout.lock().await;
            map.insert(sid.clone(), std::path::PathBuf::from(cwd));
        }
    }
    // SessionEnd:active_rollout 清理
    if event_type == "SessionEnd" {
        if let Some(sid) = payload.session_id.as_ref() {
            let mut map = state.active_rollout.lock().await;
            map.remove(sid);
        }
    }

    // 其他事件:仅 audit,放行
    state.audit_writer.submit(ws::audit_writer::AuditEvent {
        team_id: Some(team),
        user_id: Some(user),
        thread_id: payload.session_id.clone(),
        event_type: event_type.clone(),
        tool_name: payload.tool_name.clone(),
        payload: payload.raw.clone(),
        decision: None,
    });

    Ok(HookResponse {
        continue_: true,
        hook_specific_output: None,
    })
}

/// 常量时间比较(避免 timing attack;不依赖外部 crate)。
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
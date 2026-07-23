//! codex hook webhook(per-user workspace 实施步骤 9)。
//!
//! 路由:`POST /hooks/codex`,独立鉴权(X-Hook-Token == INTERNAL_HOOK_TOKEN,常量时间比较)。
//! 失败语义:任何内部异常 → 200 + continue=true(fail-open,不阻断 codex)。

use crate::error::AppError;
use crate::services::policy_engine::{PolicyDecision, PolicyInput};
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
///
/// body 用 `Bytes` 而非 `axum::Json<HookPayload>` 手动解析:axum 的 Json 提取器在
/// handler 调用前反序列化,失败会直接返回 4xx rejection,绕过本函数的 fail-open 包装。
/// codex payload schema 不稳态(版本升级/字段漂移)时,4xx 会让整个 webhook 失效;
/// 若 codex 对非 200 采取 fail-closed,会错误阻断所有工具调用。这里手动解析,
/// 解析失败也走 continue=true(token 已验过,是合法 codex 的格式问题,不应阻断)。
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // 1) 验签(X-Hook-Token == INTERNAL_HOOK_TOKEN,常量时间比较)—— 必须在任何处理前。
    let token_ok = headers
        .get("x-hook-token")
        .and_then(|v| v.to_str().ok())
        .map(|t| constant_time_eq(t.as_bytes(), state.hook_token.as_bytes()))
        .unwrap_or(false);
    if !token_ok {
        return (StatusCode::UNAUTHORIZED, "invalid hook token").into_response();
    }

    // 2) 手动解析 payload:失败 → fail-open(continue=true),不阻断 codex。
    let payload: HookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "hook payload parse failed (fail-open)");
            return axum::Json(HookResponse {
                continue_: true,
                hook_specific_output: None,
            })
            .into_response();
        }
    };

    // 3) fail-open:包一层,任何内部异常返回 continue=true
    let resp = handle_inner(&state, payload).await.unwrap_or_else(|e| {
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
        let raw_target = payload
            .tool_input
            .as_ref()
            .and_then(ws::decision::target_path)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(payload.cwd.clone().unwrap_or_default())
            });
        // 相对路径解析:codex 可能下发相对 file_path(如 "teams/t1/shared/x"),
        // 若不 join cwd 成绝对路径,decision 的 is_team_shared_path 依赖前导 / 会漏判,
        // 导致 member 用相对路径绕过共享盘只读限制。
        let target = if raw_target.is_absolute() {
            raw_target
        } else if let Some(cwd) = payload.cwd.as_deref().filter(|s| !s.is_empty()) {
            std::path::PathBuf::from(cwd).join(&raw_target)
        } else {
            raw_target
        };

        // 策略引擎(spec 2026-07-23):先于原决策表,命中 Deny 直接 deny,异常 fail-open。
        let mut policy_block: Option<(String, String)> = None;
        match state.policy_store.engine().await.evaluate(
            PolicyInput {
                team_id: &team,
                user_id: &user,
                role: &role,
                tool_name: &tool_name,
                tool_input: payload.tool_input.as_ref(),
            },
            |_tid, _uid, required_role| role == required_role,
        ) {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny { rule_id, reason } => {
                policy_block = Some((rule_id, reason));
            }
        }

        let decision = decide_pre_tool_use(&role, &tool_name, &target, &state.workspace_root);
        let perm = if policy_block.is_some() {
            "deny"
        } else {
            match decision {
                Decision::Allow => "allow",
                Decision::Deny => "deny",
                Decision::Ask => "ask",
            }
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

    // SessionStart:不再注册 active_rollout。cwd 是 codex 的**工作目录**(目录),
    // 不是 rollout 文件路径(sessions/.../rollout-<conv>.jsonl)。旧代码把 cwd insert 进
    // active_rollout 会让 replicate_team_rollouts 把目录当文件读(metadata 是目录 →
    // read_range 失败 → 复制静默跳过)。active_rollout 的正确填充由 mt_create_thread /
    // mt_start_turn 的 find_rollout_for_thread 完成(按 thread_id 匹配真实 rollout 文件)。
    // 此处仅记录审计,不碰 active_rollout。

    // SessionEnd:active_rollout 清理
    if event_type == "SessionEnd" {
        if let Some(sid) = payload.session_id.as_ref() {
            let mut map = state.active_rollout.lock().await;
            map.remove(sid);
        }
    }

    // 其他事件:仅 audit,放行
    // payload 优先用 raw(完整 hook payload),fallback 到 {tool_input|tool_output} 合成对象,
    // 避免 raw 缺失时落库为 "null" 字符串。
    let audit_payload = if !payload.raw.is_null() {
        payload.raw.clone()
    } else {
        let mut obj = serde_json::Map::new();
        if let Some(t) = &payload.tool_input {
            obj.insert("tool_input".into(), t.clone());
        }
        if let Some(o) = &payload.tool_output {
            obj.insert("tool_output".into(), o.clone());
        }
        Value::Object(obj)
    };

    state.audit_writer.submit(ws::audit_writer::AuditEvent {
        team_id: Some(team),
        user_id: Some(user),
        thread_id: payload.session_id.clone(),
        event_type: event_type.clone(),
        tool_name: payload.tool_name.clone(),
        payload: audit_payload,
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
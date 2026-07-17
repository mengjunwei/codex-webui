//! ingress → worker 内网 RPC 客户端(HTTP/JSON,带内部 token 校验)。
//!
//! ingress 路由决策后,若目标 worker 非本地,则用本客户端调用该 worker 的内网 RPC 端点
//! (`POST /internal/{thread/start, turn/start, evict}`)完成 codex 调用。
//! 本地 worker 不走本客户端,直接调用 TeamCodexManager 短路。

use crate::error::AppError;
use serde_json::{json, Value};
use std::time::Duration;

pub struct WorkerRpcClient {
    http: reqwest::Client,
    token: Option<String>,
}

impl WorkerRpcClient {
    pub fn new(token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, token }
    }

    fn url(base: &str, path: &str) -> String {
        format!("{}{}", base.trim_end_matches('/'), path)
    }

    async fn post(&self, base: &str, path: &str, body: Value) -> Result<Value, AppError> {
        let mut req = self.http.post(Self::url(base, path)).json(&body);
        if let Some(t) = &self.token {
            req = req.header("x-internal-token", t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::internal(format!("worker rpc {path} send: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            serde_json::from_str::<Value>(&text).map_err(|e| {
                AppError::internal(format!("worker rpc {path} decode: {e}; body={text}"))
            })
        } else {
            Err(AppError::internal(format!(
                "worker rpc {path} failed: HTTP {status}; body={text}"
            )))
        }
    }

    /// 创建会话(转发到远程 worker)。
    pub async fn thread_start(
        &self,
        base: &str,
        team_id: &str,
        created_by: &str,
        params: Value,
    ) -> Result<Value, AppError> {
        self.post(
            base,
            "/internal/thread/start",
            json!({ "teamId": team_id, "createdBy": created_by, "params": params }),
        )
        .await
    }

    /// 发起 turn(转发到远程 worker)。
    pub async fn turn_start(
        &self,
        base: &str,
        thread_id: &str,
        team_id: &str,
        params: Value,
    ) -> Result<Value, AppError> {
        self.post(
            base,
            "/internal/turn/start",
            json!({ "threadId": thread_id, "teamId": team_id, "params": params }),
        )
        .await
    }

    /// 踢除 team 进程(key 轮换/管理)。
    pub async fn evict(&self, base: &str, team_id: &str) -> Result<(), AppError> {
        self.post(base, "/internal/evict", json!({ "teamId": team_id }))
            .await?;
        Ok(())
    }

    /// 通用 codex 会话方法转发(fork/rollback/resume 等)。
    pub async fn thread_invoke(
        &self,
        base: &str,
        team_id: &str,
        thread_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, AppError> {
        self.post(
            base,
            "/internal/thread/invoke",
            json!({ "teamId": team_id, "threadId": thread_id, "method": method, "params": params }),
        )
        .await
    }

    /// 复制 rollout 增量到副本节点(主 → 副本)。
    pub async fn replicate_rollout(
        &self,
        base: &str,
        chunk: &crate::services::multitenant::replication::RolloutChunk,
    ) -> Result<(), AppError> {
        let body = serde_json::to_value(chunk)
            .map_err(|e| AppError::internal(format!("serialize rollout chunk: {e}")))?;
        self.post(base, "/internal/replicate", body).await?;
        Ok(())
    }

    /// 响应审批(转发到远程 worker 的 codex 进程)。
    pub async fn approval_respond(
        &self,
        base: &str,
        team_id: &str,
        request_id: &str,
        approved: bool,
        result: Option<Value>,
    ) -> Result<(), AppError> {
        self.post(
            base,
            "/internal/approval/respond",
            json!({ "teamId": team_id, "requestId": request_id, "approved": approved, "result": result }),
        )
        .await?;
        Ok(())
    }
}

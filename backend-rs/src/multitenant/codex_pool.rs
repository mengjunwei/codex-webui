//! TeamCodexManager:按 team 启动 codex app-server 进程(多租户核心)。
//!
//! 每个 team 一个独立 `CODEX_HOME`(本地目录),启动时注入该 team 的 OpenAI key
//! (M2 `get_active_plain_key` 解密)。进程按需启动并缓存在进程表,存活则复用。
//! 复用 `CodexJsonRpcClient` 做 JSON-RPC 通信,复用 `process::build_codex_command`
//! 处理 Windows npm shim。
//!
//! 注:这是"按需启动 + 缓存复用"的 M3 单机版。空闲回收、并发上限、一致性哈希、
//! 跨 worker 路由留待 M3 后期 / M4。

use crate::codex::jsonrpc::CodexJsonRpcClient;
use crate::codex::process::build_codex_command;
use crate::codex::types::default_initialize_params;
use crate::error::{AppError, ErrorCode};
use crate::multitenant::api_keys;
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 按 team 管理 codex app-server 进程。
pub struct TeamCodexManager {
    /// 存放各 team CODEX_HOME 的根目录(其下结构为 `{team_id}/.codex`)。
    teams_root: PathBuf,
    codex_bin: String,
    /// team_id → 活跃 client(按需启动,缓存复用)。
    clients: Mutex<HashMap<String, Arc<CodexJsonRpcClient>>>,
}

impl TeamCodexManager {
    pub fn new(teams_root: PathBuf, codex_bin: String) -> Self {
        Self {
            teams_root,
            codex_bin,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// 取 team 的 codex client:缓存命中且存活 → 复用;否则按需启动。
    pub async fn client_for(
        &self,
        team_id: &str,
        pool: &PgPool,
        master_key: &str,
    ) -> Result<Arc<CodexJsonRpcClient>, AppError> {
        {
            let map = self.clients.lock().await;
            if let Some(c) = map.get(team_id) {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
        }
        self.spawn_team(team_id, pool, master_key).await
    }

    /// 强制(重新)启动指定 team 的 codex 进程:先移除并销毁旧 client。
    pub async fn restart_team(
        &self,
        team_id: &str,
        pool: &PgPool,
        master_key: &str,
    ) -> Result<Arc<CodexJsonRpcClient>, AppError> {
        self.evict(team_id).await;
        self.spawn_team(team_id, pool, master_key).await
    }

    async fn spawn_team(
        &self,
        team_id: &str,
        pool: &PgPool,
        master_key: &str,
    ) -> Result<Arc<CodexJsonRpcClient>, AppError> {
        let plain_key = api_keys::get_active_plain_key(pool, team_id, master_key)
            .await?
            .ok_or_else(|| {
                AppError::business(
                    ErrorCode::AuthInvalidApiKey,
                    StatusCode::BAD_REQUEST,
                    "team has no active API key; owner must set one first".into(),
                    None,
                )
            })?;

        let codex_home = self.teams_root.join(team_id).join(".codex");
        tokio::fs::create_dir_all(&codex_home)
            .await
            .map_err(|e| AppError::internal(format!("create team codex_home: {e}")))?;

        let mut cmd = build_codex_command(&self.codex_bin);
        cmd.args(["app-server", "--listen", "stdio://"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.env("CODEX_HOME", &codex_home);
        // 注入 team 的 OpenAI key(BYOK)。codex 读 OPENAI_API_KEY 环境变量。
        cmd.env("OPENAI_API_KEY", &plain_key);

        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::internal(format!("spawn codex for team {team_id}: {e}")))?;

        // stderr 诊断输出转日志。
        if let Some(stderr) = child.stderr.take() {
            let team_id_owned = team_id.to_string();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "codex", team = %team_id_owned, "stderr: {}", line.trim());
                }
            });
        }

        let client = CodexJsonRpcClient::new(child, None)
            .map_err(|e| AppError::internal(format!("create jsonrpc client: {e}")))?;
        let client = Arc::new(client);

        // initialize 握手。
        let init_params = serde_json::to_value(default_initialize_params())
            .map_err(|e| AppError::internal(format!("serialize init params: {e}")))?;
        client
            .request("initialize", Some(init_params))
            .await
            .map_err(|e| AppError::internal(format!("codex initialize: {e}")))?;
        client
            .notify("initialized", Some(Value::Object(Default::default())))
            .map_err(|e| AppError::internal(format!("codex initialized notify: {e}")))?;

        tracing::info!(team_id, "codex app-server initialized for team");
        let mut map = self.clients.lock().await;
        map.insert(team_id.to_string(), client.clone());
        Ok(client)
    }

    /// 从缓存移除并销毁指定 team 的 client(用于 key 轮换 / 清理 / 故障)。
    pub async fn evict(&self, team_id: &str) {
        let removed = self.clients.lock().await.remove(team_id);
        if let Some(c) = removed {
            // destroy 含 kill 子进程,限时等待避免长时间阻塞。
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.destroy()).await;
        }
    }
}

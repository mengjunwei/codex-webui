//! TeamCodexManager:per-team codex app-server 进程池 + 调度策略(M3/M4)。
//!
//! 调度(对齐设计 §4.4 / §8):
//! - **per-team 多进程**(`max_processes_per_team`):每 team 维护一个 slot 列表,client_for
//!   选**最闲**存活 slot;当最闲 slot 的并发已达 `process_scale_threshold` 且未达上限时**扩进程**。
//! - **全局进程上限**(`max_global_processes`):达上限时跨 team **LRU 回收**最久未活跃的 slot 腾位;
//!   只剩当前 team 无法回收则背压(503)。
//! - **空闲 LRU 回收**(`idle_evict_secs`):后台 task 周期扫描,`last_active` 超时则回收该 slot。
//! - **每进程并发 semaphore**(`max_concurrent_per_process`):`client_for` 返回 `ClientLease`,
//!   lease 持有 permit 直到 drop,限制单进程并发,配合 write_tx 有界背压根治 OOM。
//! - **failover 恢复**:spawn 前 CODEX_HOME 不存在则从快照 restore(worker 故障迁移到新机后自动恢复)。
//!
//! 每个 team 一个独立 `CODEX_HOME`(本地目录),启动时注入 team 的 OpenAI key
//! (M2 `get_active_plain_key` 解密;同时落 per-team `auth.json`,env 注入兜底)。

use crate::codex::jsonrpc::CodexJsonRpcClient;
use crate::codex::process::build_codex_command;
use crate::codex::types::default_initialize_params;
use crate::error::{AppError, ErrorCode};
use crate::services::multitenant::api_keys;
use crate::services::multitenant::event_bus::EventBus;
use crate::services::multitenant::pool_policy;
use crate::services::multitenant::now_ms;
use axum::http::StatusCode;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

/// 进程池调度参数(从 Config 构造)。
#[derive(Clone)]
pub struct PoolConfig {
    pub max_processes_per_team: usize,
    pub max_global_processes: usize,
    pub idle_evict_secs: u64,
    pub max_concurrent_per_process: usize,
    pub process_scale_threshold: usize,
}

impl PoolConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_processes_per_team: usize,
        max_global_processes: usize,
        idle_evict_secs: u64,
        max_concurrent_per_process: usize,
        process_scale_threshold: usize,
    ) -> Self {
        Self {
            max_processes_per_team: max_processes_per_team.max(1),
            max_global_processes: max_global_processes.max(1),
            idle_evict_secs: idle_evict_secs.max(60),
            max_concurrent_per_process: max_concurrent_per_process.max(1),
            process_scale_threshold: process_scale_threshold.max(1),
        }
    }
}

/// client 租约:持有 client 引用 + semaphore permit。drop 时释放并发额度。
pub struct ClientLease {
    client: Arc<CodexJsonRpcClient>,
    _permit: OwnedSemaphorePermit,
}

impl ClientLease {
    /// 借用底层 codex JSON-RPC client。
    pub fn client(&self) -> &CodexJsonRpcClient {
        &self.client
    }
}

struct ProcessSlot {
    client: Arc<CodexJsonRpcClient>,
    semaphore: Arc<Semaphore>,
    last_active: Mutex<i64>,
}

/// 按 team 管理 codex app-server 进程池。
pub struct TeamCodexManager {
    /// 全局 CODEX_HOME(所有 team 共用;team 仅前端 UI 隔离,不隔离目录/进程)。
    codex_home: PathBuf,
    codex_bin: String,
    /// team_id → 该 team 的进程 slot 列表(多 slot 支持 per-team 扩进程)。
    team_slots: Mutex<HashMap<String, Vec<Arc<ProcessSlot>>>>,
    /// 事件总线(可选):codex notification / server_request 发布到 RedisEventBus。
    event_bus: Option<Arc<dyn EventBus>>,
    config: PoolConfig,
    /// 上一代主密钥(M6 轮转):解密 key 时当前 master 失败则回退它。
    master_previous: Option<String>,
}

impl TeamCodexManager {
    pub fn new(
        codex_home: PathBuf,
        codex_bin: String,
        event_bus: Option<Arc<dyn EventBus>>,
        config: PoolConfig,
        master_previous: Option<String>,
    ) -> Self {
        Self {
            codex_home,
            codex_bin,
            team_slots: Mutex::new(HashMap::new()),
            event_bus,
            config,
            master_previous,
        }
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// 当前活跃进程数(监控/指标用)。
    pub async fn active_count(&self) -> usize {
        self.team_slots
            .lock()
            .await
            .values()
            .map(|v| v.iter().filter(|s| !s.client.is_closed()).count())
            .sum()
    }

    /// 取 team client 租约:确保容量(按需扩进程)→ 选最闲存活 slot → 获取 permit。
    pub async fn client_for(
        &self,
        team_id: &str,
        db: &sea_orm::DatabaseConnection,
        master_key: &str,
    ) -> Result<ClientLease, AppError> {
        self.ensure_capacity(team_id, db, master_key).await?;
        let slot = self.pick_slot(team_id).await?;
        {
            let mut la = slot.last_active.lock().await;
            *la = now_ms();
        }
        // 等待该进程的并发位(semaphore 上限 = max_concurrent_per_process,自然限流)。
        let permit = slot
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::internal("semaphore closed".into()))?;
        metrics::gauge!("codex_team_active_processes").set(self.active_count().await as f64);
        Ok(ClientLease {
            client: slot.client.clone(),
            _permit: permit,
        })
    }

    /// 强制(重新)启动指定 team 的全部 codex 进程:先 evict 全部,再 client_for。
    /// 用于 key 轮换(M2 收尾)。
    pub async fn restart_team(
        &self,
        team_id: &str,
        db: &sea_orm::DatabaseConnection,
        master_key: &str,
    ) -> Result<ClientLease, AppError> {
        self.evict(team_id).await;
        self.client_for(team_id, db, master_key).await
    }

    /// 确保该 team 有可用 slot,并按"最闲 slot 并发达阈值"扩进程(受 per-team / 全局上限约束)。
    async fn ensure_capacity(
        &self,
        team_id: &str,
        db: &sea_orm::DatabaseConnection,
        master_key: &str,
    ) -> Result<(), AppError> {
        // 1. 至少一个存活 slot;无则 spawn 第一个。
        let has_alive = {
            let map = self.team_slots.lock().await;
            map.get(team_id)
                .map(|v| v.iter().any(|s| !s.client.is_closed()))
                .unwrap_or(false)
        };
        if !has_alive {
            self.spawn_slot(team_id, db, master_key).await?;
        }
        // 2. 扩进程循环:最闲 slot 忙到阈值 且 未达 per-team 上限 → 扩(受全局上限 / LRU 约束)。
        loop {
            let need_scale = {
                let map = self.team_slots.lock().await;
                let Some(v) = map.get(team_id) else { break };
                let alive: Vec<&Arc<ProcessSlot>> =
                    v.iter().filter(|s| !s.client.is_closed()).collect();
                if alive.is_empty() {
                    break;
                }
                let cap = self.config.max_concurrent_per_process;
                let min_inflight = alive
                    .iter()
                    .map(|s| cap - s.semaphore.available_permits())
                    .min()
                    .unwrap_or(cap);
                pool_policy::should_scale(
                    min_inflight,
                    self.config.process_scale_threshold,
                    v.len(),
                    self.config.max_processes_per_team,
                )
            };
            if !need_scale {
                break;
            }
            // 全局上限检查(满则跨 team LRU 腾位;无法腾则放弃扩,用现有 slot)。
            if !self.reserve_global_capacity(team_id).await? {
                break;
            }
            self.spawn_slot(team_id, db, master_key).await?;
            metrics::counter!("codex_team_scale_out_total").increment(1);
        }
        Ok(())
    }

    /// 预留一个全局进程名额:未满直接 true;满则跨 team LRU 回收一个最久未活跃 slot;
    /// 只剩当前 team 无法回收则 false(放弃扩进程)。
    async fn reserve_global_capacity(&self, exclude_team: &str) -> Result<bool, AppError> {
        let victim = {
            let map = self.team_slots.lock().await;
            let total: usize = map
                .values()
                .map(|v| v.iter().filter(|s| !s.client.is_closed()).count())
                .sum();
            if !pool_policy::global_full(total, self.config.max_global_processes) {
                return Ok(true);
            }
            // 收集其他 team 的 (team_id, 最老 slot last_active)。
            let mut candidates: Vec<(String, i64)> = Vec::new();
            for (tid, v) in map.iter() {
                if tid == exclude_team {
                    continue;
                }
                let mut oldest = i64::MAX;
                for s in v.iter() {
                    if s.client.is_closed() {
                        continue;
                    }
                    let la = s.last_active.try_lock().map(|g| *g).unwrap_or(i64::MAX);
                    if la < oldest {
                        oldest = la;
                    }
                }
                if oldest != i64::MAX {
                    candidates.push((tid.clone(), oldest));
                }
            }
            candidates.sort_by_key(|(_, la)| *la);
            candidates.first().map(|(t, _)| t.clone())
        };
        match victim {
            Some(team) => {
                tracing::info!(victim = %team, "evicting LRU team slot for global capacity");
                self.evict_oldest_slot(&team).await;
                metrics::counter!("codex_team_evicts_total", "reason" => "lru").increment(1);
                Ok(true)
            }
            None => {
                metrics::counter!("codex_team_backpressure_total").increment(1);
                tracing::warn!(team = exclude_team, "global process limit reached, scaling disabled");
                Ok(false)
            }
        }
    }

    /// 选该 team 当前可用 permit 最多(最闲)的存活 slot。
    async fn pick_slot(&self, team_id: &str) -> Result<Arc<ProcessSlot>, AppError> {
        let map = self.team_slots.lock().await;
        let Some(v) = map.get(team_id) else {
            return Err(AppError::internal(format!("no slots for team {team_id}")));
        };
        v.iter()
            .filter(|s| !s.client.is_closed())
            .max_by_key(|s| s.semaphore.available_permits())
            .cloned()
            .ok_or_else(|| AppError::internal(format!("no alive slot for team {team_id}")))
    }

    async fn spawn_slot(
        &self,
        team_id: &str,
        db: &sea_orm::DatabaseConnection,
        master_key: &str,
    ) -> Result<Arc<ProcessSlot>, AppError> {
        let plain_key = api_keys::get_active_plain_key(
            db,
            team_id,
            master_key,
            self.master_previous.as_deref(),
        )
        .await?
        .ok_or_else(|| {
            AppError::business(
                ErrorCode::AuthInvalidApiKey,
                StatusCode::BAD_REQUEST,
                "team has no active API key; owner must set one first".into(),
                None,
            )
        })?;

        let codex_home = self.codex_home.clone();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .map_err(|e| AppError::internal(format!("create codex_home: {e}")))?;

        let mut cmd = build_codex_command(&self.codex_bin);
        cmd.args(["app-server", "--listen", "stdio://"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.env("CODEX_HOME", &codex_home);
        // 全局 CODEX_HOME 共享:每 team 进程注入各自 BYOK key(env 注入;不写全局 auth.json 以免 key 串味)。
        cmd.env("OPENAI_API_KEY", &plain_key);

        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::internal(format!("spawn codex for team {team_id}: {e}")))?;

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

        let init_params = serde_json::to_value(default_initialize_params())
            .map_err(|e| AppError::internal(format!("serialize init params: {e}")))?;
        client
            .request("initialize", Some(init_params))
            .await
            .map_err(|e| AppError::internal(format!("codex initialize: {e}")))?;
        client
            .notify("initialized", Some(Value::Object(Default::default())))
            .map_err(|e| AppError::internal(format!("codex initialized notify: {e}")))?;

        metrics::counter!("codex_team_spawns_total").increment(1);
        tracing::info!(team_id, "codex app-server initialized for team");

        // notification + server_request → 事件总线(实时回流 + event_persist 持久化)。
        if let Some(bus) = self.event_bus.clone() {
            let client_for_bus = client.clone();
            let bus_for_notif = bus.clone();
            tokio::spawn(async move {
                let mut rx = client_for_bus.subscribe_notifications();
                while let Ok(msg) = rx.recv().await {
                    if let Ok(payload) = serde_json::to_string(&msg) {
                        let _ = bus_for_notif.publish("codex:events", &payload).await;
                    }
                }
            });
            let client_for_sr = client.clone();
            tokio::spawn(async move {
                let mut rx = client_for_sr.subscribe_server_requests();
                while let Ok(msg) = rx.recv().await {
                    if let Ok(payload) = serde_json::to_string(&msg) {
                        let _ = bus.publish("codex:events", &payload).await;
                    }
                }
            });
        }

        let slot = Arc::new(ProcessSlot {
            client: client.clone(),
            semaphore: Arc::new(Semaphore::new(self.config.max_concurrent_per_process.max(1))),
            last_active: Mutex::new(now_ms()),
        });
        let mut map = self.team_slots.lock().await;
        map.entry(team_id.to_string()).or_default().push(slot.clone());
        Ok(slot)
    }

    /// 回收该 team 最老的一个存活 slot(全局 LRU / 空闲回收用)。
    async fn evict_oldest_slot(&self, team_id: &str) {
        let removed = {
            let mut map = self.team_slots.lock().await;
            let Some(v) = map.get_mut(team_id) else {
                return;
            };
            // 选最老的存活 slot。
            let mut idx_opt = None;
            let mut oldest = i64::MAX;
            for (i, s) in v.iter().enumerate() {
                if s.client.is_closed() {
                    continue;
                }
                let la = s.last_active.try_lock().map(|g| *g).unwrap_or(oldest);
                if la < oldest {
                    oldest = la;
                    idx_opt = Some(i);
                }
            }
            idx_opt.and_then(|i| {
                if v.iter().filter(|s| !s.client.is_closed()).count() <= 1 {
                    None // 至少保留一个,避免清空(下次 client_for 会重建)。
                } else {
                    Some(v.remove(i))
                }
            })
        };
        if let Some(s) = removed {
            let _ = tokio::time::timeout(Duration::from_secs(5), s.client.destroy()).await;
        }
    }

    /// 从缓存移除并销毁指定 team 的全部 client(key 轮换 / 清理)。
    pub async fn evict(&self, team_id: &str) {
        let removed = self.team_slots.lock().await.remove(team_id);
        if let Some(slots) = removed {
            for s in slots {
                let _ = tokio::time::timeout(Duration::from_secs(5), s.client.destroy()).await;
            }
        }
    }

    /// 启动空闲回收后台 task(周期 = idle_evict_secs / 4,最小 60s)。
    pub fn start_idle_reaper(self: &Arc<Self>) {
        let this = self.clone();
        let idle = self.config.idle_evict_secs;
        tokio::spawn(async move {
            let interval = Duration::from_secs((idle / 4).max(60));
            loop {
                tokio::time::sleep(interval).await;
                let now = now_ms();
                let threshold = now - (idle as i64 * 1000);
                let teams: Vec<String> = this.team_slots.lock().await.keys().cloned().collect();
                for tid in teams {
                    // 该 team 最老存活 slot 是否超时。
                    let idle_slot = {
                        let map = this.team_slots.lock().await;
                        let Some(v) = map.get(&tid) else { continue };
                        v.iter().any(|s| {
                            if s.client.is_closed() {
                                return false;
                            }
                            let la = s.last_active.try_lock().map(|g| *g).unwrap_or(now);
                            la < threshold
                        })
                    };
                    if idle_slot {
                        tracing::info!(team_id = %tid, "idle-evicting team process");
                        this.evict_oldest_slot(&tid).await;
                        metrics::counter!("codex_team_evicts_total", "reason" => "idle")
                            .increment(1);
                    }
                }
                metrics::gauge!("codex_team_active_processes").set(this.active_count().await as f64);
            }
        });
    }
}

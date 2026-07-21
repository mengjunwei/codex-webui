//! 共享的应用状态，用 `Arc` 包装以便低成本克隆到各 axum handler 中。

use crate::auth::AuthService;
use crate::codex::CodexProcessManager;
use crate::services::codex_status::CodexStatusService;
use crate::services::multitenant::cluster::ClusterMembership;
use crate::services::multitenant::rpc::WorkerRpcClient;
use crate::services::multitenant::sticky::StickyStore;
use crate::services::workspace::audit_writer::AuditWriter;
use metrics_exporter_prometheus::PrometheusHandle;
use crate::services::settings::ValueSource;
use crate::services::terminal::TerminalService;
use crate::services::threads::ThreadResumeRegistry;
use sea_orm::DatabaseConnection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// settings 内存缓存条目：(value, source, updated_at)。
pub type SettingsCache = Arc<Mutex<HashMap<String, (serde_json::Value, ValueSource, Option<i64>)>>>;

#[derive(Clone)]
pub struct AppState {
    /// SeaORM 数据库连接(PG/MySQL 多方言,必选)。multitenant + 业务共用。
    pub db: DatabaseConnection,
    /// 主密钥(加密 team API key)。来自 MASTER_KEY 或回退 webui_api_key。
    pub mt_master_key: String,
    /// Redis 客户端(事件总线/限流/复制 offset;None = 未配置)。
    pub mt_redis: Option<redis::Client>,
    /// Prometheus 指标 handle(供 /metrics 暴露;None = 未启用指标)。
    pub metrics_handle: Option<PrometheusHandle>,
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    pub terminal: Arc<TerminalService>,
    /// 就绪状态聚合服务（驱动 /codex/status、/account.provider、/logs/export.runtimeStatus）。
    pub status: Arc<CodexStatusService>,
    /// H6：线程 resume 注册表（按 generation 去重，对齐 TS ThreadResumeRegistryService）。
    pub resume_registry: Arc<ThreadResumeRegistry>,
    /// 通过 POST /api/files/roots 动态注册的工作区根目录。
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
    /// settings 内存缓存（对齐 TS SettingsService.cache）。
    pub settings_cache: SettingsCache,
    // ── 多副本 HA(全局 CODEX_HOME + session 复制)─────────────────────────
    /// codex CLI 的 CODEX_HOME(sessions/ config.toml auth.json 写入位置)。
    /// 只用于:spawn codex 子进程 env、rollout 读写、hooks config 写入、
    /// user config 路径校验、snapshot。所有 team codex 子进程共用同一目录。
    pub codex_home: PathBuf,
    /// webui 文件工作区根(users/ teams/ 的父目录)。
    /// 只用于:files 浏览根、终端 cwd 沙箱、workspace 路径拼接、
    /// hook decision 边界、chat 上传根。默认 = codex_home(向后兼容)。
    pub workspace_root: PathBuf,
    /// 本节点 id。
    pub node_id: String,
    /// 集群成员 + 探活(选主副本/反亲和/晋升判定/解析节点 RPC 地址)。
    pub cluster: Arc<dyn ClusterMembership>,
    /// 节点间内网 RPC 客户端(非主节点转发到该 team 主节点)。
    pub worker_rpc: Arc<WorkerRpcClient>,
    /// 会话粘性存储(thread → worker 绑定,保证同一会话始终落到同一 worker)。
    pub sticky: Arc<dyn StickyStore>,
    /// 内网 RPC 鉴权 token(INTERNAL_RPC_TOKEN;启动必填 ≥32 字节)。
    pub internal_token: String,
    /// Hook webhook 鉴权 token(INTERNAL_HOOK_TOKEN;启动必填 ≥32 字节)。
    pub hook_token: String,
    /// Hook 审计批量写入器(per-user workspace 实施步骤 5)。
    pub audit_writer: AuditWriter,
    /// HTTP 监听端口(codex 启动时拼出 hooks URL)。
    pub http_bind_port: u16,

    // ── HA 修复(spec 2026-07-17 §2.1 / §2.2)────────────────────────
    /// 主侧:thread_id → 当前该 thread 活跃 rollout 文件绝对路径。
    /// 由 mt_create_thread / mt_start_turn 调 codex 后写入;
    /// 复制循环按此表精确读取文件,避免 UUID 子串误匹配。
    pub active_rollout: Arc<tokio::sync::Mutex<HashMap<String, PathBuf>>>,
    /// 无 Redis 时 offset fallback 存储(进程内);重启归零接受。key = (thread_id, rel_path)。
    pub local_offsets: Arc<tokio::sync::Mutex<HashMap<(String, String), u64>>>,
}

impl AppState {
    pub fn home_dir(&self) -> String {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    }

    /// 便捷方法：借用本状态中的 DB 构造一个 `SettingsReader`。
    pub fn settings_reader(&self) -> crate::services::settings::SettingsReader<'_> {
        crate::services::settings::SettingsReader::new(&self.db, Some(&self.settings_cache))
    }

    /// 清空 settings 缓存（写入后调用，对齐 TS reloadCache）。
    pub fn invalidate_settings_cache(&self) {
        if let Ok(mut cache) = self.settings_cache.lock() {
            cache.clear();
        }
    }
}

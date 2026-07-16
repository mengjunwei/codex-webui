//! 共享的应用状态，用 `Arc` 包装以便低成本克隆到各 axum handler 中。

use crate::auth::AuthService;
use crate::codex::CodexProcessManager;
use crate::codex_status::CodexStatusService;
use crate::db::Db;
use crate::multitenant::codex_pool::TeamCodexManager;
use crate::settings::ValueSource;
use crate::terminal::TerminalService;
use crate::threads::ThreadResumeRegistry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// settings 内存缓存条目：(value, source, updated_at)。
pub type SettingsCache = Arc<Mutex<HashMap<String, (serde_json::Value, ValueSource, Option<i64>)>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    /// 多租户 PG 连接池(None = 未配置 DATABASE_URL,多租户功能禁用,现有功能不受影响)。
    pub mt_pg: Option<sqlx::PgPool>,
    /// 主密钥(加密 team API key)。来自 MASTER_KEY 或回退 webui_api_key。
    pub mt_master_key: String,
    /// 按 team 启动 codex 进程的管理器(M3)。
    pub mt_team_codex: Arc<TeamCodexManager>,
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
}

impl AppState {
    pub fn home_dir(&self) -> String {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    }

    /// 便捷方法：借用本状态中的 DB 构造一个 `SettingsReader`。
    pub fn settings_reader(&self) -> crate::settings::SettingsReader<'_> {
        crate::settings::SettingsReader::new(&self.db, Some(&self.settings_cache))
    }

    /// 清空 settings 缓存（写入后调用，对齐 TS reloadCache）。
    pub fn invalidate_settings_cache(&self) {
        if let Ok(mut cache) = self.settings_cache.lock() {
            cache.clear();
        }
    }
}

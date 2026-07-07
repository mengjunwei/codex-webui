//! 共享的应用状态，用 `Arc` 包装以便低成本克隆到各 axum handler 中。

use crate::auth::AuthService;
use crate::codex::CodexProcessManager;
use crate::db::Db;
use crate::terminal::TerminalService;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    pub terminal: Arc<TerminalService>,
    /// 通过 POST /api/files/roots 动态注册的工作区根目录。
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn home_dir(&self) -> String {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    }

    /// 便捷方法：借用本状态中的 DB 构造一个 `SettingsReader`。
    pub fn settings_reader(&self) -> crate::settings::SettingsReader<'_> {
        crate::settings::SettingsReader::new(&self.db)
    }
}

//! Shared application state, wrapped in `Arc` for cheap cloning into axum handlers.

use crate::auth::AuthService;
use crate::codex::CodexProcessManager;
use crate::db::Db;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    /// Workspace roots dynamically registered via POST /api/files/roots
    /// (e.g. thread cwd). Adds to the set used by `files::resolve`.
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn home_dir(&self) -> String {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default()
    }

    /// Convenience: a `SettingsReader` borrowing this state's DB.
    pub fn settings_reader(&self) -> crate::settings::SettingsReader<'_> {
        crate::settings::SettingsReader::new(&self.db)
    }
}

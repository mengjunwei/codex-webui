//! Shared application state, wrapped in `Arc` for cheap cloning into axum handlers.

use crate::auth::AuthService;
use crate::codex::CodexProcessManager;
use crate::db::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
}

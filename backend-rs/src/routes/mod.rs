//! Route handlers and router construction.

pub mod auth;
pub mod health;

use crate::auth::middleware::require_auth;
use crate::state::AppState;
use axum::{
    routing::{get, post},
    Json, Router,
};
use tower_http::services::{ServeDir, ServeFile};
use utoipa::OpenApi;

/// OpenAPI document (basic spec; per-endpoint annotations can be added later).
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Codex WebUI",
        version = "0.1.0",
        description = "Codex WebUI API — Rust backend (migrated from NestJS)",
    ),
)]
struct ApiDoc;

/// Serve the OpenAPI JSON spec at /api/docs-json (public; for SDK generation).
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Build the application router.
///
/// Layout (parity with NestJS globalPrefix 'api'):
/// - `POST /api/auth/login`   — public (JWT login)
/// - `POST /api/onlyoffice/callback` — public (OO save callback)
/// - `GET  /api/docs-json`    — public (OpenAPI spec)
/// - Everything else under `/api/*` — protected by require_auth
pub fn build_router(state: AppState) -> Router {
    use crate::chat as chat_mod;
    use crate::codex_status_config as csc;
    use crate::files as fl;
    use crate::onlyoffice as oo;
    use crate::proxies as px;
    use crate::settings::handlers as s;
    use crate::sqlite_handlers as sq;
    use crate::threads as th;

    // Protected API sub-router.
    let api = Router::new()
        // ── Phase 0 probe (also serves as GET /api/status parity with AppController) ──
        .route("/_ping", get(health::ping))
        .route("/status", get(health::ping))
        // ── chat upload (protected; multipart) ──
        .route("/chat/upload", post(chat_mod::upload_attachment))
        // ── settings CRUD ──
        .route("/settings", get(s::list).patch(s::update_batch))
        .route("/settings/:key", get(s::get_one).patch(s::update_one).delete(s::delete_one))
        // ── auth logout (protected, parity with TS) ──
        .route("/auth/logout", post(auth::logout))
        // ── thread-scoped reads ──
        .route("/threads/:threadId/token-usage", get(sq::read_token_usage))
        .route("/threads/:threadId/turn-diffs", get(sq::read_turn_diffs))
        .route("/threads/:threadId/turn-errors", get(sq::read_turn_errors))
        // ── pending-approvals (read + respond) ──
        .route("/pending-approvals", get(sq::list_pending))
        .route(
            "/pending-approvals/:requestId/respond",
            post(sq::respond_to_request),
        )
        // ── logs ──
        .route("/logs", get(crate::logs::list_logs))
        .route("/logs/export", get(crate::logs::export_diagnostics))
        // ── account (codex proxy) ──
        .route("/account", get(px::account_read))
        .route("/account/login", post(px::account_login))
        .route("/account/login/cancel", post(px::account_login_cancel))
        .route("/account/logout", post(px::account_logout))
        .route("/account/rate-limits", get(px::account_rate_limits))
        // ── apps / models (codex proxy) ──
        .route("/apps", get(px::apps_list))
        .route("/models", get(px::models_list))
        // ── mcp-servers (codex proxy) ──
        .route("/mcp-servers", get(px::mcp_servers_list))
        .route("/mcp-servers/reload", post(px::mcp_servers_reload))
        .route("/mcp-servers/oauth/login", post(px::mcp_servers_oauth_login))
        // ── skills (codex proxy) ──
        .route("/skills", get(px::skills_list))
        .route("/skills/config", post(px::skills_config_write))
        // ── plugins (codex proxy) ──
        .route("/plugins", get(px::plugins_list))
        .route("/plugins/detail", get(px::plugins_detail))
        .route("/plugins/install", post(px::plugins_install))
        .route("/plugins/uninstall", post(px::plugins_uninstall))
        // ── threads + turns (codex proxy) ──
        .route("/threads", post(th::create_thread).get(th::list_threads))
        .route("/threads/loaded", get(th::list_loaded_threads))
        .route("/threads/:threadId", get(th::read_thread))
        .route("/threads/:threadId/resume", post(th::resume_thread))
        .route("/threads/:threadId/turns", post(th::start_turn))
        .route("/threads/:threadId/turns/:turnId/steer", post(th::steer_turn))
        .route("/threads/:threadId/turns/:turnId/interrupt", post(th::interrupt_turn))
        .route("/threads/:threadId/archive", post(th::archive_thread))
        .route("/threads/:threadId/unarchive", post(th::unarchive_thread))
        .route("/threads/:threadId/compact", post(th::compact_thread))
        .route("/threads/:threadId/fork", post(th::fork_thread))
        .route("/threads/:threadId/rollback", post(th::rollback_thread))
        .route("/threads/:threadId/name", axum::routing::patch(th::set_thread_name))
        // ── codex status + config ──
        .route("/codex/status", get(csc::status))
        .route("/codex/approval-policy", post(csc::update_approval_policy))
        .route("/codex/sandbox-mode", post(csc::update_sandbox_mode))
        .route(
            "/codex/config",
            get(csc::read_config).patch(csc::update_config),
        )
        .route("/codex/config/raw", get(csc::read_raw_config).put(csc::update_raw_config))
        // ── files (core ops; upload/serve/rename/copy/move deferred) ──
        .route("/files/roots", get(fl::get_roots).post(fl::add_root))
        .route("/files/tree", get(fl::read_tree))
        .route("/files/read", get(fl::read_file))
        .route("/files/metadata", get(fl::get_metadata))
        .route("/files/delete", axum::routing::delete(fl::delete_path))
        .route("/files/create-file", post(fl::create_file))
        .route("/files/create-directory", post(fl::create_directory))
        .route("/files/write", post(fl::write_file))
        .route("/files/serve", get(fl::serve_file))
        .route("/files/download", get(fl::download_file))
        .route("/files/rename", post(fl::rename_path))
        .route("/files/copy", post(fl::copy_path))
        .route("/files/move", post(fl::move_path))
        .route("/files/upload", post(fl::upload_files))
        .route("/files/archive/list", get(fl::archive_list))
        .route("/files/archive/entry", get(fl::archive_entry))
        // ── onlyoffice config (protected) ──
        .route("/onlyoffice/config", get(oo::get_config))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // Static file serving for the React frontend (SPA).
    // Serves `public/` directory; falls back to `public/index.html` for
    // client-side routing. In development (no public/ dir), the fallback
    // returns 404 which is fine (frontend runs on :5173 with a proxy).
    let static_files = ServeDir::new("public")
        .fallback(ServeFile::new("public/index.html"));

    Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/onlyoffice/callback", post(oo::handle_callback))
        .route("/api/docs-json", get(openapi_json))
        .nest("/api", api)
        .fallback_service(static_files)
        .with_state(state)
}

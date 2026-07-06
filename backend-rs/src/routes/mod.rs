//! Route handlers and router construction.

pub mod auth;
pub mod health;

use crate::auth::middleware::require_auth;
use crate::state::AppState;
use axum::{
    routing::{get, post},
    Router,
};

/// Build the application router.
///
/// Layout (parity with NestJS globalPrefix 'api'):
/// - `GET /`               — public root health
/// - `POST /api/auth/login` — public (JWT login)
/// - Everything else under `/api/*` — protected by require_auth
///
/// Phase 2 additions:
/// - `/api/settings` CRUD (self-contained, SQLite-backed)
/// - `/api/threads/:threadId/{token-usage,turn-diffs,turn-errors}` (SQLite read)
/// - `/api/pending-approvals` (SQLite read)
/// - `/api/{account,apps,models,mcp-servers,skills,plugins}/*` (Phase 1 stubs → 501)
pub fn build_router(state: AppState) -> Router {
    use crate::settings::handlers as s;
    use crate::sqlite_handlers as sq;
    use crate::stubs as st;

    // Protected API sub-router.
    let api = Router::new()
        // ── Phase 0 probe ──
        .route("/_ping", get(health::ping))
        // ── settings CRUD ──
        .route("/settings", get(s::list).patch(s::update_batch))
        .route("/settings/:key", get(s::get_one).patch(s::update_one).delete(s::delete_one))
        // ── thread-scoped reads ──
        .route(
            "/threads/:threadId/token-usage",
            get(sq::read_token_usage),
        )
        .route(
            "/threads/:threadId/turn-diffs",
            get(sq::read_turn_diffs),
        )
        .route(
            "/threads/:threadId/turn-errors",
            get(sq::read_turn_errors),
        )
        // ── pending-approvals (read; respond needs Phase 1) ──
        .route("/pending-approvals", get(sq::list_pending))
        .route(
            "/pending-approvals/:requestId/respond",
            post(st::pending_approvals_respond),
        )
        // ── logs ──
        .route("/logs", get(crate::logs::list_logs))
        .route("/logs/export", get(crate::logs::export_diagnostics))
        // ── Phase 1 proxy stubs (501) ──
        .route("/account", get(st::account_read))
        .route("/account/login", post(st::account_login))
        .route("/account/login/cancel", post(st::account_login_cancel))
        .route("/account/logout", post(st::account_logout))
        .route("/account/rate-limits", get(st::account_rate_limits))
        .route("/apps", get(st::apps_list))
        .route("/models", get(st::models_list))
        .route("/mcp-servers", get(st::mcp_servers_list))
        .route("/mcp-servers/reload", post(st::mcp_servers_reload))
        .route("/mcp-servers/oauth/login", post(st::mcp_servers_oauth_login))
        .route("/skills", get(st::skills_list))
        .route("/skills/config", post(st::skills_config_write))
        .route("/plugins", get(st::plugins_list))
        .route("/plugins/detail", get(st::plugins_detail))
        .route("/plugins/install", post(st::plugins_install))
        .route("/plugins/uninstall", post(st::plugins_uninstall))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    Router::new()
        .route("/", get(health::root))
        .route("/api/auth/login", post(auth::login))
        .nest("/api", api)
        .with_state(state)
}

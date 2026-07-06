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
pub fn build_router(state: AppState) -> Router {
    use crate::proxies as px;
    use crate::settings::handlers as s;
    use crate::sqlite_handlers as sq;

    // Protected API sub-router.
    let api = Router::new()
        // ── Phase 0 probe ──
        .route("/_ping", get(health::ping))
        // ── settings CRUD ──
        .route("/settings", get(s::list).patch(s::update_batch))
        .route("/settings/:key", get(s::get_one).patch(s::update_one).delete(s::delete_one))
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

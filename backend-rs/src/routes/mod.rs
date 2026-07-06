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
/// - `GET /api/_ping`       — protected probe (Phase 0 only)
pub fn build_router(state: AppState) -> Router {
    // Protected sub-router (auth required).
    let api_protected = Router::new()
        .route("/_ping", get(health::ping))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    Router::new()
        .route("/", get(health::root))
        .route("/api/auth/login", post(auth::login))
        .nest("/api", api_protected)
        .with_state(state)
}

//! Codex WebUI backend — entry point.
//!
//! Startup: .env → Config → tracing → DB.open → run_migrations →
//! reconcile_settings → AuthService → AppState → build_router → serve.
//!
//! Graceful shutdown (spec §6.7 — incremental enhancement over TS, which
//! does not enableShutdownHooks): SIGTERM (unix) or Ctrl-C → drain → close DB.

use codex_webui::{
    auth::AuthService, config::Config, db::Db, logging, routes::build_router,
    settings::reconcile_settings, state::AppState,
};
use std::sync::Arc;
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present (no-op in Docker / production).
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env()?;
    let _log_guard = logging::init(&cfg.log_level);

    tracing::info!(
        port = cfg.port,
        db = %cfg.db_path,
        "starting codex-webui (backend-rs)"
    );

    // Database.
    let db = Arc::new(Db::open(&cfg.db_path)?);
    codex_webui::db::run_migrations(&db)?;
    reconcile_settings(&db)?;

    // Shared state (auth is the only service beyond DB/settings in Phase 0).
    let state = AppState {
        db,
        auth: Arc::new(AuthService::new(&cfg.webui_api_key)),
    };

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    tracing::info!("listening on 0.0.0.0:{}", cfg.port);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("drain complete, shutting down");
    Ok(())
}

/// Wait for SIGTERM (unix) or Ctrl-C (all platforms).
/// On Windows there is no SIGTERM, so only Ctrl-C triggers shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

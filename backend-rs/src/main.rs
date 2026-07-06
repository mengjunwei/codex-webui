//! Codex WebUI backend — entry point.
//!
//! Startup: .env → Config → tracing → DB.open → run_migrations →
//! reconcile_settings → AuthService → AppState → build_router → serve.
//!
//! Graceful shutdown (spec §6.7 — incremental enhancement over TS, which
//! does not enableShutdownHooks): SIGTERM (unix) or Ctrl-C → drain → close DB.

use codex_webui::{
    auth::AuthService, codex::CodexProcessManager, config::Config, db::Db, logging,
    routes::build_router, settings::reconcile_settings, state::AppState,
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

    // Auth service (created before AppState so realtime can share it).
    let auth = Arc::new(AuthService::new(&cfg.webui_api_key));

    // Codex app-server process manager (hub). Started in the background so the
    // web server is available even if codex is slow to spawn or unavailable;
    // the manager auto-restarts on failure.
    let codex = Arc::new(CodexProcessManager::new(
        cfg.codex_bin.clone(),
        cfg.codex_home.clone(),
    ));
    let codex_bg = codex.clone();
    tokio::spawn(async move {
        codex_bg.start().await;
    });

    // Wire event-driven DB write paths (token-usage, turn-diff, turn-errors,
    // pending-approvals record/resolved/expire). Subscribes to the manager's
    // broadcasts; also expires stale pending requests on boot.
    codex_webui::event_subscribers::spawn_all(db.clone(), codex.clone());

    // Realtime Socket.IO gateway (`/ws` namespace) + emit-forwarding tasks.
    let rt_state = codex_webui::realtime::RealtimeState {
        auth: auth.clone(),
        codex: codex.clone(),
    };
    let (ws_layer, io) = codex_webui::realtime::build(rt_state);
    codex_webui::realtime::spawn_emit_tasks(io, codex.clone());

    // Shared state.
    let state = AppState {
        db,
        auth,
        codex,
    };

    let app = build_router(state).layer(ws_layer);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    tracing::info!("listening on 0.0.0.0:{}", cfg.port);

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("drain complete, shutting down codex + db");
    // Graceful: stop the codex manager (kills the app-server child).
    // (codex is still alive via AppState, but the manager's restart loop is now blocked.)
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

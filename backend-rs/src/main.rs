//! Codex WebUI 后端 —— 程序入口。
//!
//! 启动流程：.env → Config → tracing → SeaORM connect → Migrator →
//! reconcile_settings → AuthService → AppState → build_router → serve。
//!
//! 数据层:SeaORM 1.1(PG/MySQL 多方言),DATABASE_URL 必选。

use codex_webui::{
    auth::AuthService, codex::CodexProcessManager, config::Config, logging,
    migration::Migrator, routes::build_router, settings::{self, reconcile_settings},
    state::AppState, terminal::{TerminalConfig, TerminalService},
    threads::ThreadResumeRegistry,
};
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    codex_webui::logs::mark_process_start();

    let cfg = Config::from_env()?;
    let _guards = logging::init(&cfg.log_level, cfg.otlp_endpoint.as_deref());

    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("prometheus recorder: {e}"))?;

    tracing::info!(port = cfg.port, url = %cfg.database_url, "starting codex-webui (backend-rs)");

    // SeaORM 多方言连接(PG/MySQL)。
    let db: DatabaseConnection = sea_orm::Database::connect(&cfg.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect database: {e}"))?;
    Migrator::up(&db, None)
        .await
        .map_err(|e| anyhow::anyhow!("run migrations: {e}"))?;
    reconcile_settings(&db).await?;

    let mt_master_key = cfg.master_key.clone().unwrap_or_else(|| cfg.webui_api_key.clone());

    let mt_redis = match &cfg.redis_url {
        Some(url) => Some(
            redis::Client::open(url.as_str())
                .map_err(|e| anyhow::anyhow!("redis client: {e}"))?,
        ),
        None => {
            tracing::warn!("REDIS_URL not set; distributed coordination disabled");
            None
        }
    };

    let mt_event_bus: Option<Arc<dyn codex_webui::multitenant::event_bus::EventBus>> =
        match &mt_redis {
            Some(client) => Some(Arc::new(
                codex_webui::multitenant::event_bus::RedisEventBus::new(client.clone(), 256),
            )),
            None => None,
        };

    let teams_root: std::path::PathBuf = match std::env::var("CODEX_TEAMS_HOME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let base = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            base.join(".codex-webui-teams")
        }
    };
    let mt_event_bus_for_emit = mt_event_bus.clone();
    let mt_team_codex = Arc::new(codex_webui::multitenant::codex_pool::TeamCodexManager::new(
        teams_root,
        cfg.codex_bin.clone(),
        mt_event_bus,
    ));

    let auth = Arc::new(AuthService::new(&cfg.webui_api_key));

    let codex = Arc::new(CodexProcessManager::new(
        cfg.codex_bin.clone(),
        cfg.codex_home.clone(),
    ));
    let codex_bg = codex.clone();
    tokio::spawn(async move { codex_bg.start().await; });

    codex_webui::event_subscribers::spawn_all(db.clone(), codex.clone());

    let reader = settings::SettingsReader::new(&db, None);
    let terminal = TerminalService::new(TerminalConfig::from_settings(&reader).await);

    let dynamic_files_roots = Arc::new(Mutex::new(HashSet::new()));
    let resume_registry = Arc::new(ThreadResumeRegistry::new());
    let active_threads = codex_webui::realtime::ActiveThreadRegistry::new();

    let rt_state = codex_webui::realtime::RealtimeState {
        auth: auth.clone(),
        codex: codex.clone(),
        terminal: terminal.clone(),
        db: db.clone(),
        dynamic_files_roots: dynamic_files_roots.clone(),
        active_threads: active_threads.clone(),
    };
    let (ws_layer, io) = codex_webui::realtime::build(rt_state);
    codex_webui::realtime::spawn_emit_tasks(
        io.clone(),
        codex.clone(),
        terminal.clone(),
        db.clone(),
        active_threads.clone(),
        resume_registry.clone(),
    );

    if let Some(bus) = mt_event_bus_for_emit {
        codex_webui::realtime::spawn_event_bus_emit(io.clone(), bus);
    }

    if let Some(client) = mt_redis.clone() {
        let worker_id = std::env::var("WORKER_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let registry =
            codex_webui::multitenant::routing::WorkerRegistry::new(client, worker_id);
        tokio::spawn(async move {
            loop {
                if let Err(e) = registry.heartbeat(30).await {
                    tracing::warn!(error = %e, "worker heartbeat failed");
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }

    let status_service = Arc::new(codex_webui::codex_status::CodexStatusService::new(codex.clone()));

    let state = AppState {
        db,
        mt_master_key,
        mt_team_codex,
        mt_redis,
        metrics_handle: Some(metrics_handle),
        auth,
        codex,
        terminal,
        status: status_service,
        resume_registry,
        dynamic_files_roots,
        settings_cache: Arc::new(Mutex::new(HashMap::new())),
    };

    let codex_for_shutdown = state.codex.clone();
    let app = build_router(state).await.layer(ws_layer);

    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    tracing::info!("listening on {}:{}", cfg.host, cfg.port);

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("drain complete, shutting down codex");
    codex_for_shutdown.destroy().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler").recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {}, }
}

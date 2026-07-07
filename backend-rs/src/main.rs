//! Codex WebUI 后端 —— 程序入口。
//!
//! 启动流程：.env → Config → tracing → DB.open → run_migrations →
//! reconcile_settings → AuthService → AppState → build_router → serve。
//!
//! 优雅关闭（spec §6.7 —— 相比 TS 的增量增强，TS 未启用 enableShutdownHooks）：
//! 收到 SIGTERM（unix）或 Ctrl-C → 排空 → 关闭 DB。

use codex_webui::{
    auth::AuthService, codex::CodexProcessManager, config::Config, db::Db, logging,
    routes::build_router, settings::{self, reconcile_settings},
    state::AppState, terminal::{TerminalConfig, TerminalService},
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 如果存在 .env 则加载（在 Docker / 生产环境中为空操作）。
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env()?;
    let _log_guard = logging::init(&cfg.log_level);

    tracing::info!(
        port = cfg.port,
        db = %cfg.db_path,
        "starting codex-webui (backend-rs)"
    );

    // 数据库。
    let db = Arc::new(Db::open(&cfg.db_path)?);
    codex_webui::db::run_migrations(&db)?;
    reconcile_settings(&db)?;

    // 认证服务（在 AppState 之前创建，以便 realtime 模块共享）。
    let auth = Arc::new(AuthService::new(&cfg.webui_api_key));

    // Codex app-server 进程管理器（中枢）。在后台启动，这样即使 codex
    // 启动缓慢或不可用，Web 服务器仍然可用；管理器在失败时会自动重启。
    let codex = Arc::new(CodexProcessManager::new(
        cfg.codex_bin.clone(),
        cfg.codex_home.clone(),
    ));
    let codex_bg = codex.clone();
    tokio::spawn(async move {
        codex_bg.start().await;
    });

    // 接入事件驱动的 DB 写入路径（token-usage、turn-diff、turn-errors、
    // pending-approvals 的记录/解决/过期）。订阅管理器的广播；
    // 同时在启动时过期陈旧的待处理请求。
    codex_webui::event_subscribers::spawn_all(db.clone(), codex.clone());

    // 终端服务（共享 PTY 会话）。
    let reader = settings::SettingsReader::new(&db);
    let terminal = TerminalService::new(TerminalConfig::from_settings(&reader));

    // 实时 Socket.IO 网关（`/ws` 命名空间）+ emit 转发任务。
    let rt_state = codex_webui::realtime::RealtimeState {
        auth: auth.clone(),
        codex: codex.clone(),
        terminal: terminal.clone(),
    };
    let (ws_layer, io) = codex_webui::realtime::build(rt_state);
    codex_webui::realtime::spawn_emit_tasks(io, codex.clone(), terminal.clone(), db.clone());

    // 共享状态。
    let state = AppState {
        db,
        auth,
        codex,
        terminal,
        dynamic_files_roots: Arc::new(Mutex::new(HashSet::new())),
    };

    let app = build_router(state).layer(ws_layer);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    tracing::info!("listening on 0.0.0.0:{}", cfg.port);

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("drain complete, shutting down codex + db");
    // 优雅关闭：停止 codex 管理器（会终止 app-server 子进程）。
    // （codex 通过 AppState 仍然存活，但管理器的重启循环已被阻塞。）
    Ok(())
}

/// 等待 SIGTERM（unix）或 Ctrl-C（所有平台）。
/// Windows 上没有 SIGTERM，因此只有 Ctrl-C 会触发关闭。
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

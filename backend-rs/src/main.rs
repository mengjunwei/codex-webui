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
    threads::ThreadResumeRegistry,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 如果存在 .env 则加载（在 Docker / 生产环境中为空操作）。
    let _ = dotenvy::dotenv();

    // 固化进程启动基准点（供 /logs/export 的 uptimeSeconds，对齐 TS process.uptime()）。
    codex_webui::logs::mark_process_start();

    let cfg = Config::from_env()?;
    let _guards = logging::init(&cfg.log_level, cfg.otlp_endpoint.as_deref());

    // M5-B Prometheus 指标:安装全局 recorder,handle 供 /metrics 暴露。
    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("prometheus recorder: {e}"))?;

    tracing::info!(
        port = cfg.port,
        db = %cfg.db_path,
        "starting codex-webui (backend-rs)"
    );

    // 数据库。
    let db = Arc::new(Db::open(&cfg.db_path)?);
    codex_webui::db::run_migrations(&db)?;
    reconcile_settings(&db)?;

    // 多租户 PG(可选):未配置 DATABASE_URL 则禁用多租户功能,现有功能不受影响。
    let mt_pg = match &cfg.database_url {
        Some(url) => {
            tracing::info!("connecting multitenant postgres");
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(20)
                .connect(url)
                .await
                .map_err(|e| anyhow::anyhow!("connect multitenant pg: {e}"))?;
            codex_webui::multitenant::migration::run_migrations(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("multitenant migration: {e}"))?;
            tracing::info!("multitenant postgres ready");
            Some(pool)
        }
        None => {
            tracing::warn!("DATABASE_URL not set; multitenant features disabled");
            None
        }
    };

    // 主密钥:优先 MASTER_KEY,回退 webui_api_key(加密 team 的 OpenAI key 用)。
    let mt_master_key = cfg.master_key.clone().unwrap_or_else(|| cfg.webui_api_key.clone());

    // Redis(M4 分布式协调;可选):未配置则跨节点功能禁用,单机功能不受影响。
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

    // 事件总线:Redis 配置则 RedisEventBus(多机跨节点广播),否则 None。
    let mt_event_bus: Option<Arc<dyn codex_webui::multitenant::event_bus::EventBus>> =
        match &mt_redis {
            Some(client) => Some(Arc::new(
                codex_webui::multitenant::event_bus::RedisEventBus::new(client.clone(), 256),
            )),
            None => None,
        };

    // 多 team codex 进程管理器(M3):CODEX_TEAMS_HOME 或回退 ~/.codex-webui-teams。
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
    let reader = settings::SettingsReader::new(&db, None);
    let terminal = TerminalService::new(TerminalConfig::from_settings(&reader));

    // 动态工作区根目录（POST /api/files/roots 注册）；终端 cwd 沙箱与文件路由共用。
    let dynamic_files_roots = Arc::new(Mutex::new(HashSet::new()));

    // 线程 resume 注册表 + 活跃线程订阅表（codex 重启后 auto-resume 仍被订阅的线程）。
    let resume_registry = Arc::new(ThreadResumeRegistry::new());
    let active_threads = codex_webui::realtime::ActiveThreadRegistry::new();

    // 实时 Socket.IO 网关（`/ws` 命名空间）+ emit 转发任务。
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

    // M4 subscribe 端:Redis 事件总线 codex:events → socket.io emit(完成实时闭环)。
    if let Some(bus) = mt_event_bus_for_emit {
        codex_webui::realtime::spawn_event_bus_emit(io.clone(), bus);
    }

    // M4 worker 心跳(REDIS_URL 配置时):周期注册本地 worker 到 Redis,
    // 供 RedisRouter 路由 + 故障检测(心跳停 → TTL 过期 → team failover)。
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

    // codex 重启（generation 变化）时清空 resume 缓存——现由 realtime 的 lifecycle
    // emit 任务在 auto-resume 之前推进（见 realtime::spawn_lifecycle_emit），不再另起任务。

    // 就绪状态聚合服务（/codex/status、/account.provider、/logs/export 共享其缓存）。
    let status_service = Arc::new(codex_webui::codex_status::CodexStatusService::new(codex.clone()));

    // 共享状态。
    let state = AppState {
        db,
        mt_pg,
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
    let app = build_router(state).layer(ws_layer);

    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    tracing::info!("listening on {}:{}", cfg.host, cfg.port);

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("drain complete, shutting down codex");
    // 优雅关闭：销毁 codex 管理器（终止 app-server 子进程 + 阻止重启循环 +
    // 拒绝在途请求 + flush JSONL 流）。
    codex_for_shutdown.destroy().await;
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

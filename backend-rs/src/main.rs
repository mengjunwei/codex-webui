//! Codex WebUI 后端 —— 程序入口。
//!
//! 多副本 HA 模型:全局 CODEX_HOME(所有 team 共用)+ per-team codex 进程 + session 级
//! rollout 增量复制到副本 + memberlist/redis 探活 + 副本晋升。所有节点同配置(无 ingress/worker 之分)。

use codex_webui::{
    api::build_router,
    api::multitenant::internal_rpc::build_internal_router,
    auth::AuthService,
    codex::CodexProcessManager,
    config::Config,
    db::migration::Migrator,
    logging,
    services::multitenant::cluster::{ClusterMembership, RedisCluster, SingleCluster},
    services::multitenant::codex_pool::PoolConfig,
    services::multitenant::event_bus::EventBus,
    services::multitenant::replication,
    services::multitenant::rpc::WorkerRpcClient,
    services::settings::{self, reconcile_settings},
    services::terminal::{TerminalConfig, TerminalService},
    services::threads::ThreadResumeRegistry,
    state::AppState,
};
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    codex_webui::api::logs::mark_process_start();

    let cfg = Config::from_env()?;
    let _guards = logging::init(&cfg.log_level, cfg.otlp_endpoint.as_deref());

    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("prometheus recorder: {e}"))?;

    tracing::info!(port = cfg.port, url = %cfg.database_url, "starting codex-webui (multi-replica HA)");

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
            redis::Client::open(url.as_str()).map_err(|e| anyhow::anyhow!("redis client: {e}"))?,
        ),
        None => {
            tracing::warn!("REDIS_URL not set; running single-node (no replication/failover)");
            None
        }
    };

    let mt_event_bus: Option<Arc<dyn EventBus>> = mt_redis
        .as_ref()
        .map(|c| {
            Arc::new(codex_webui::services::multitenant::event_bus::RedisEventBus::new(
                c.clone(),
                256,
            )) as Arc<dyn EventBus>
        });

    // 全局 CODEX_HOME(所有 team 共用;team 仅前端 UI 隔离)。
    let codex_home: PathBuf = cfg.codex_home.clone().map(PathBuf::from).unwrap_or_else(|| {
        let base = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join(".codex-webui").join("home")
    });
    tokio::fs::create_dir_all(&codex_home)
        .await
        .map_err(|e| anyhow::anyhow!("create codex_home: {e}"))?;

    // 节点 id + 内网 RPC(均由 Config 必填校验,此处直接 clone)。
    let node_id = cfg.worker_id.clone();
    let internal_token = cfg.internal_token.clone();
    let own_rpc_url = cfg
        .worker_rpc_url
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.internal_rpc_port));

    // cluster:有 Redis → RedisCluster(gossip 心跳);无 → SingleCluster(仅自己)。
    let cluster: Arc<dyn ClusterMembership> = match &mt_redis {
        Some(c) => Arc::new(RedisCluster::new(c.clone(), node_id.clone())),
        None => Arc::new(SingleCluster::new(node_id.clone(), own_rpc_url.clone())),
    };

    let worker_rpc = Arc::new(WorkerRpcClient::new(Some(internal_token.clone())));

    let pool_config = PoolConfig::new(
        cfg.max_processes_per_team,
        cfg.max_global_processes,
        cfg.idle_evict_secs,
        cfg.max_concurrent_per_process,
        cfg.process_scale_threshold,
    );
    let mt_team_codex = Arc::new(
        codex_webui::services::multitenant::codex_pool::TeamCodexManager::new(
            codex_home.clone(),
            cfg.codex_bin.clone(),
            mt_event_bus.clone(),
            pool_config,
            cfg.master_key_previous.clone(),
        ),
    );
    mt_team_codex.start_idle_reaper();

    let auth = Arc::new(AuthService::new(&cfg.webui_api_key));
    let codex = Arc::new(CodexProcessManager::new(
        cfg.codex_bin.clone(),
        cfg.codex_home.clone(),
    ));
    let codex_bg = codex.clone();
    tokio::spawn(async move { codex_bg.start().await; });

    codex_webui::api::event_subscribers::spawn_all(db.clone(), codex.clone());

    let reader = settings::SettingsReader::new(&db, None);
    let terminal = TerminalService::new(TerminalConfig::from_settings(&reader).await);

    let dynamic_files_roots = Arc::new(Mutex::new(HashSet::new()));
    let resume_registry = Arc::new(ThreadResumeRegistry::new());
    let active_threads = codex_webui::api::realtime::ActiveThreadRegistry::new();

    let rt_state = codex_webui::api::realtime::RealtimeState {
        auth: auth.clone(),
        codex: codex.clone(),
        terminal: terminal.clone(),
        db: db.clone(),
        dynamic_files_roots: dynamic_files_roots.clone(),
        active_threads: active_threads.clone(),
    };
    let (ws_layer, io) = codex_webui::api::realtime::build(rt_state);
    codex_webui::api::realtime::spawn_emit_tasks(
        io.clone(),
        codex.clone(),
        terminal.clone(),
        db.clone(),
        active_threads.clone(),
        resume_registry.clone(),
    );
    if let Some(bus) = mt_event_bus.clone() {
        codex_webui::api::realtime::spawn_event_bus_emit(io.clone(), bus);
    }
    // team 事件持久化(审批/turn 错误/token 用量落 PG)。
    if let Some(bus) = mt_event_bus.clone() {
        codex_webui::services::multitenant::event_persist::spawn_team_event_persistor(bus, db.clone());
    }

    let status_service = Arc::new(codex_webui::services::codex_status::CodexStatusService::new(codex.clone()));

    let active_rollout = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let local_offsets = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let state = AppState {
        db: db.clone(),
        mt_master_key: mt_master_key.clone(),
        mt_team_codex: mt_team_codex.clone(),
        mt_redis: mt_redis.clone(),
        metrics_handle: Some(metrics_handle),
        auth: auth.clone(),
        codex: codex.clone(),
        terminal: terminal.clone(),
        status: status_service.clone(),
        resume_registry: resume_registry.clone(),
        dynamic_files_roots: dynamic_files_roots.clone(),
        settings_cache: Arc::new(Mutex::new(HashMap::new())),
        codex_home: codex_home.clone(),
        node_id: node_id.clone(),
        cluster: cluster.clone(),
        worker_rpc: worker_rpc.clone(),
        internal_token: internal_token.clone(),
        active_rollout,
        local_offsets,
    };

    // cluster 心跳 task(RedisCluster:周期登记本节点 + rpc 地址)。
    if let Some(client) = mt_redis.clone() {
        let rc = RedisCluster::new(client, node_id.clone());
        let rpc_url = own_rpc_url.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = rc.heartbeat(30, &rpc_url).await {
                    tracing::warn!(error = %e, "cluster heartbeat failed");
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }

    // 主副本维护 task:主续约 + 复制 rollout;副本探测主失活 → 晋升 + resume。
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            run_replica_maintenance(&st).await;
        }
    });

    // 内网 RPC server(所有节点都开;承接转发请求 + 副本 receive rollout)。
    {
        let internal_state = state.clone();
        let internal_app = build_internal_router(internal_state);
        let addr = format!("{}:{}", cfg.internal_rpc_host, cfg.internal_rpc_port);
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, addr = %addr, "bind internal rpc failed");
                    return;
                }
            };
            tracing::info!(addr = %addr, "internal rpc listening");
            let _ = axum::serve(listener, internal_app)
                .with_graceful_shutdown(shutdown_signal())
                .await;
        });
    }

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

/// 周期维护:遍历 session_replicas,主节点续约+复制;副本节点探测主失活并晋升。
async fn run_replica_maintenance(state: &AppState) {
    use sea_orm::EntityTrait;
    // 孤儿 team 认领(重启换 id 等场景;确定性,仅最低 alive id 节点执行)。
    let _ = replication::reclaim_orphan_teams(&state.db, state.cluster.as_ref(), state.mt_redis.as_ref()).await;
    let rows = match codex_webui::db::entities::session_replica::Entity::find()
        .all(&state.db)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "session_replica scan failed");
            return;
        }
    };
    for row in rows {
        let team_id = row.team_id.clone();
        // 确保副本已分配(扩容后回填 None,否则主挂无人晋升)。
        let _ = replication::ensure_replica(&state.db, &team_id, state.cluster.as_ref()).await;
        if row.primary_node == state.node_id {
            if let Err(e) = replication::renew_lease(&state.db, &team_id, &state.node_id, state.mt_redis.as_ref()).await {
                tracing::warn!(error = %e, team_id = %team_id, "renew_lease failed");
            }
            let _ = replication::replicate_team_rollouts(
                &state.db,
                &team_id,
                &state.codex_home,
                state.cluster.as_ref(),
                state.mt_redis.as_ref(),
                &state.worker_rpc,
            )
            .await;
        } else if row.replica_node.as_deref() == Some(state.node_id.as_str()) {
            match replication::promote_if_primary_down(
                &state.db,
                &team_id,
                state.cluster.as_ref(),
                state.mt_redis.as_ref(),
            )
            .await
            {
                Ok(true) => {
                    if let Err(e) = promote_resume_team(state, &team_id).await {
                        tracing::warn!(error = %e, team_id = %team_id, "promote resume failed");
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, team_id = %team_id, "promote check failed"),
            }
        }
    }
}

/// 副本晋升后:起该 team 的 codex 进程,并对所有活跃 thread 调 thread/resume 续接。
async fn promote_resume_team(state: &AppState, team_id: &str) -> Result<(), codex_webui::error::AppError> {
    use codex_webui::db::entities::thread::{Column as ThreadColumn, Entity as ThreadEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    // 起 codex 进程(全局 CODEX_HOME 已有该 team 所有 thread 的 rollout 副本)。
    let _ = state
        .mt_team_codex
        .client_for(team_id, &state.db, &state.mt_master_key)
        .await?;
    let threads = ThreadEntity::find()
        .filter(ThreadColumn::TeamId.eq(team_id.to_string()))
        .all(&state.db)
        .await
        .map_err(|e| codex_webui::error::AppError::internal(format!("query threads for resume: {e}")))?;
    for t in threads {
        let lease = state
            .mt_team_codex
            .client_for(team_id, &state.db, &state.mt_master_key)
            .await?;
        let params = serde_json::json!({ "threadId": t.id, "persistExtendedHistory": true });
        if let Err(e) = lease.client().request("thread/resume", Some(params)).await {
            tracing::warn!(error = %e, thread_id = %t.id, "resume after promote failed (non-fatal)");
        }
    }
    metrics::counter!("replica_promotion_resumed_total").increment(1);
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

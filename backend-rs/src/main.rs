//! Codex WebUI 后端 —— 程序入口。
//!
//! 多副本 HA 模型:全局 CODEX_HOME(所有 team 共用)+ per-node codex 单进程 + session 级
//! rollout 增量复制到副本 + memberlist/redis 探活 + 副本晋升。所有节点同配置(无 ingress/worker 之分)。

use codex_webui::{
    api::build_router,
    api::hooks,
    api::multitenant::internal_rpc::build_internal_router,
    auth::AuthService,
    codex::CodexProcessManager,
    config::Config,
    db::migration::Migrator,
    logging,
    services::multitenant::cluster::{ClusterMembership, RedisCluster, SingleCluster},
    services::multitenant::event_bus::EventBus,
    services::multitenant::replication,
    services::multitenant::rpc::WorkerRpcClient,
    services::multitenant::sticky::{NoopSticky, RedisSticky, StickyStore},
    services::settings::{self, reconcile_settings},
    services::terminal::{TerminalConfig, TerminalService},
    services::threads::ThreadResumeRegistry,
    state::AppState,
};
#[cfg(feature = "memberlist-backend")]
use codex_webui::services::multitenant::cluster::memberlist_impl::MemberlistCluster;
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_webui::api::logs::mark_process_start();

    let cfg = Config::load()?;
    let _guards = logging::init(&cfg.server.log_level, cfg.otel.endpoint.as_deref());

    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("prometheus recorder: {e}"))?;

    tracing::info!(
        port = cfg.server.port,
        url = %cfg.database_url(),
        driver = ?cfg.database.driver,
        "starting codex-webui (multi-replica HA)"
    );

    let db_url = cfg.database_url();
    let db: DatabaseConnection = sea_orm::Database::connect(&db_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect database: {e}"))?;
    Migrator::up(&db, None)
        .await
        .map_err(|e| anyhow::anyhow!("run migrations: {e}"))?;
    // 平台管理员 bootstrap:把 config [security] admin_emails 里的用户置 admin。
    let bootstrapped = codex_webui::services::multitenant::permissions::bootstrap_platform_admins(
        &db,
        &cfg.security.admin_emails,
    )
    .await
    .map_err(|e| anyhow::anyhow!("bootstrap platform admins: {e}"))?;
    if bootstrapped > 0 {
        tracing::info!(count = bootstrapped, "bootstrapped platform admin(s)");
    }
    reconcile_settings(&db).await?;
    // 后端重启 = 所有 codex 子进程随之退出,thread_resume_cache 全部陈旧(指向已死进程的
    // 内存 thread 状态)。清空,使首次 resume 走真实 codex 从 rollout 重新加载 thread。
    codex_webui::services::multitenant::resume_cache::clear_all(&db).await;

    let mt_master_key = cfg.effective_master_key().to_string();

    let mt_redis = match &cfg.redis_url() {
        Some(url) => Some(
            redis::Client::open(url.as_str()).map_err(|e| anyhow::anyhow!("redis client: {e}"))?,
        ),
        None => {
            tracing::warn!("redis not configured; running single-node (no replication/failover)");
            None
        }
    };

    // 事件总线:有 Redis 用 RedisEventBus(多机跨节点);无 Redis 用 InMemoryEventBus(单机)。
    // Bug8 修复:此前无 Redis 时 mt_event_bus=None → codex 进程不发布事件、event_persist 不
    // 启动 → 单节点部署(文档明确支持)静默丢失全部审批/用量/错误/diff 持久化,quota 不累加。
    // 改为总用 Some:单节点用 InMemory bus(codex 单进程事件 → event_persist 落 PG)。
    let mt_event_bus: Option<Arc<dyn EventBus>> = Some(match mt_redis.as_ref() {
        Some(c) => Arc::new(
            codex_webui::services::multitenant::event_bus::RedisEventBus::new(c.clone(), 256),
        ) as Arc<dyn EventBus>,
        None => Arc::new(
            codex_webui::services::multitenant::event_bus::InMemoryEventBus::new(256),
        ) as Arc<dyn EventBus>,
    });

    // codex CLI 的 CODEX_HOME(所有 team codex 子进程共用;rollout/sessions 写入位置)。
    let codex_home: PathBuf = cfg.codex_home().map(PathBuf::from).unwrap_or_else(|| {
        let base = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join(".codex-webui").join("home")
    });
    // webui 文件工作区根(users/ teams/ 父目录);默认 = codex_home(向后兼容)。
    let workspace_root: PathBuf = cfg
        .workspace_root()
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_home.clone());
    tokio::fs::create_dir_all(&codex_home)
        .await
        .map_err(|e| anyhow::anyhow!("create codex_home: {e}"))?;
    tokio::fs::create_dir_all(&workspace_root)
        .await
        .map_err(|e| anyhow::anyhow!("create workspace_root: {e}"))?;

    // 节点 id + 内网 RPC(均由 Config 必填校验,此处直接 clone)。
    let node_id = cfg.cluster.worker_id.clone();
    let internal_token = cfg.security.internal_rpc_token.clone();
    let own_rpc_url = cfg
        .worker_rpc_url()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.internal_rpc_port()));

    // cluster 三分支:memberlist(SEEDS 非空) → RedisCluster(有 Redis) → SingleCluster(单机)。
    let cluster: Arc<dyn ClusterMembership> = if !cfg.memberlist.memberlist_seeds.is_empty() {
        #[cfg(feature = "memberlist-backend")]
        {
            let redis = mt_redis.clone()
                .ok_or_else(|| anyhow::anyhow!("REDIS_URL required when MEMBERLIST_SEEDS is set"))?;
            let rpc = own_rpc_url.clone();
            let ml = MemberlistCluster::new(
                cfg.cluster.worker_id.clone(),
                &cfg.memberlist.memberlist_bind,
                &cfg.memberlist.memberlist_seeds,
                redis,
                rpc,
            ).await?;
            tracing::info!(seeds = ?cfg.memberlist.memberlist_seeds, "memberlist cluster started");
            Arc::new(ml)
        }
        #[cfg(not(feature = "memberlist-backend"))]
        {
            anyhow::bail!("MEMBERLIST_SEEDS set but memberlist-backend feature not enabled; \
                           rebuild with --features memberlist-backend");
        }
    } else if let Some(c) = mt_redis.clone() {
        Arc::new(RedisCluster::new(c, node_id.clone()))
    } else {
        Arc::new(SingleCluster::new(node_id.clone(), own_rpc_url.clone()))
    };

    let worker_rpc = Arc::new(WorkerRpcClient::new(Some(internal_token.clone())));

    // 会话粘性存储:有 Redis 用 RedisSticky,否则 NoopSticky。
    let sticky: Arc<dyn StickyStore> = match &mt_redis {
        Some(c) => Arc::new(RedisSticky::new(c.clone())),
        None => Arc::new(NoopSticky),
    };

    let auth = Arc::new(AuthService::new(cfg.webui_api_key()));
    let codex = Arc::new(CodexProcessManager::new(
        cfg.codex_bin().to_string(),
        cfg.codex_home().map(|s| s.to_string()),
        cfg.codex_max_concurrent(),
    ));
    let codex_bg = codex.clone();
    tokio::spawn(async move { codex_bg.start().await; });

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
        workspace_root: workspace_root.clone(),
        active_threads: active_threads.clone(),
        socket_users: Arc::new(Mutex::new(HashMap::new())),
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
    // 传 node_id:event_persist 用 primary 守门(fan-out 去重),仅 team 主节点处理该 team 事件,
    // 防多副本 HA 下审批重复 N 行 + token 配额累加 N 次。
    if let Some(bus) = mt_event_bus.clone() {
        codex_webui::services::multitenant::event_persist::spawn_team_event_persistor(
            bus,
            db.clone(),
            node_id.clone(),
        );
    }
    // codex 单进程事件 → EventBus("codex:events"):供 event_persist 落 PG(team 维度 + quota +
    // 审批双保险)+ realtime 跨节点 fan-out。等价原 codex_pool 的
    // subscribe_notifications/server_requests → bus.publish 转发(单进程化后改为转发唯一全局 codex)。
    if let Some(bus) = mt_event_bus.clone() {
        // notification 流(token 用量/turn diff/turn error/agent 事件)。
        let codex_for_notif = codex.clone();
        let bus_for_notif = bus.clone();
        let redis_for_notif = mt_redis.clone();
        tokio::spawn(async move {
            let mut rx = codex_for_notif.subscribe_notifications();
            loop {
                match rx.recv().await {
                    Ok(mut msg) => {
                        // 反向映射 threadId:codex 通知用 codex_tid,emit room(thread:{tid})/
                        // event_persist(DB 按 thread_id)需系统 thread_id。codex 尊重 threadId 时
                        // codex_tid==thread_id,映射 miss fallback 原 threadId(幂等无害)。
                        if let Some(ctid) = msg
                            .get("params")
                            .and_then(|p| p.get("threadId"))
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                        {
                            if let Some(sys_tid) = replication::get_thread_id_by_codex(
                                redis_for_notif.as_ref(), &ctid,
                            )
                            .await
                            {
                                if let Some(p) = msg.get_mut("params").and_then(|p| p.as_object_mut()) {
                                    p.insert("threadId".to_string(), serde_json::Value::String(sys_tid));
                                }
                            }
                        }
                        if let Ok(payload) = serde_json::to_string(&msg) {
                            let _ = bus_for_notif.publish("codex:events", &payload).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "codex→bus notification forward lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        // server_request 流(审批请求)。
        let codex_for_sr = codex.clone();
        let redis_for_sr = mt_redis.clone();
        tokio::spawn(async move {
            let mut rx = codex_for_sr.subscribe_server_requests();
            loop {
                match rx.recv().await {
                    Ok(mut msg) => {
                        // 反向映射 threadId(同 notification):审批按系统 thread_id 落 PG + emit。
                        if let Some(ctid) = msg
                            .get("params")
                            .and_then(|p| p.get("threadId"))
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                        {
                            if let Some(sys_tid) = replication::get_thread_id_by_codex(
                                redis_for_sr.as_ref(), &ctid,
                            )
                            .await
                            {
                                if let Some(p) = msg.get_mut("params").and_then(|p| p.as_object_mut()) {
                                    p.insert("threadId".to_string(), serde_json::Value::String(sys_tid));
                                }
                            }
                        }
                        if let Ok(payload) = serde_json::to_string(&msg) {
                            let _ = bus.publish("codex:events", &payload).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "codex→bus server_request forward lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let status_service = codex_webui::services::codex_status::CodexStatusService::new(codex.clone());

    let active_rollout = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let local_offsets = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // 启动 hook audit 写入器(per-user workspace 实施步骤 5+7)。
    let (audit_writer, audit_writer_handle) =
        codex_webui::services::workspace::audit_writer::spawn(db.clone());

    let state = AppState {
        db: db.clone(),
        mt_master_key: mt_master_key.clone(),
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
        workspace_root: workspace_root.clone(),
        node_id: node_id.clone(),
        cluster: cluster.clone(),
        worker_rpc: worker_rpc.clone(),
        sticky: sticky.clone(),
        internal_token: internal_token.clone(),
        hook_token: cfg.security.internal_hook_token.clone(),
        audit_writer: audit_writer.clone(),
        http_bind_port: cfg.server.port,
        active_rollout,
        local_offsets,
        cfg_extensions_max_file_bytes: cfg.extensions.max_file_bytes,
        cfg_extensions_plugin_enabled: cfg.extensions.plugin_enabled,
        cfg_extensions_max_extension_bytes: cfg.extensions.max_extension_bytes,
        ext_state_lock: Arc::new(tokio::sync::Mutex::new(())),
        mt_event_bus: mt_event_bus.clone(),
    };

    // cluster 心跳 task(RedisCluster:周期登记本节点 + rpc 地址)。
    if let Some(client) = mt_redis.clone() {
        let rc = RedisCluster::new(client, node_id.clone());
        let rpc_url = own_rpc_url.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = rc.heartbeat(15, &rpc_url).await {
                    tracing::warn!(error = %e, "cluster heartbeat failed");
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }

    // 主副本维护 task:主续约 + 复制 rollout;副本探测主失活 → 晋升 + resume。
    // 保存 JoinHandle:关停时 abort,否则 loop task 永久持有 state.clone()(含 audit_writer 的
    // mpsc Sender),audit_writer 的 rx 永不返回 None → audit_writer_handle.await 死锁,
    // 进程 Ctrl-C 后挂死只能 SIGKILL,审计 flush 永不发生。
    let st = state.clone();
    let replica_maintenance_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            run_replica_maintenance(&st).await;
        }
    });

    // 集群扩展同步(周期 task + 事件订阅)。两个 handle 都声明在外层作用域,shutdown 段
    // 均需 abort(同 replica_maintenance 理由:loop task 持 state.clone() 含 audit_writer
    // sender,不 abort → audit_writer rx 永不 None → flush 死锁)。
    let mut ext_sync_handle: Option<tokio::task::JoinHandle<()>> = None;
    // 事件订阅 task 同样持 mt_event_bus Arc + state.clone()(含 audit_writer sender);
    // 不保存 handle 时,shutdown 靠 bus drop 后 broadcast Closed 自然退出,但 bus 可能被
    // 其他持有方延迟 drop → task 滞留 → sender 不释放 → audit flush 只能靠 5s 超时放弃
    // (丢审计日志)。保存 handle 并显式 abort:task 在 recv().await 处被取消,释放 sender,
    // audit flush 正常完成。
    let mut ext_event_handle: Option<tokio::task::JoinHandle<()>> = None;
    if cfg.extensions.enable {
        // bootstrap:启动全量对齐(把本地 skills 拉齐到 PG 清单)。non-fatal:失败仅 warn,
        // 不阻断启动(下轮周期 task / 事件会重试)。
        if let Err(e) = codex_webui::services::extensions::sync::bootstrap(&state).await {
            tracing::warn!(error = %e, "extension bootstrap failed (non-fatal)");
        }
        // 周期同步 task:sleep sync_interval_secs → run_round。
        let st = state.clone();
        let interval = cfg.extensions.sync_interval_secs;
        ext_sync_handle = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if let Err(e) = codex_webui::services::extensions::sync::run_round(&st).await {
                    tracing::warn!(error = %e, "extension sync round failed");
                }
            }
        }));
        // 事件订阅:extensions:changed(skill 上传/删除时 Task 6 发布)→ 立即 run_round,
        // 把分钟级周期收敛到秒级事件驱动。Lagged 必须 continue(丢弃部分事件可接受,
        // 下一次事件或周期仍会补齐);Closed(bus 被 drop)才退出。
        if let Some(bus) = mt_event_bus.clone() {
            let st2 = state.clone();
            ext_event_handle = Some(tokio::spawn(async move {
                let mut rx = match bus.subscribe("extensions:changed").await {
                    Ok(rx) => rx,
                    Err(e) => {
                        tracing::warn!(error = %e, "subscribe extensions:changed failed");
                        return;
                    }
                };
                loop {
                    match rx.recv().await {
                        Ok(_) => {
                            if let Err(e) =
                                codex_webui::services::extensions::sync::run_round(&st2).await
                            {
                                tracing::warn!(error = %e, "extension sync round (event) failed");
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "extensions:changed subscriber lagged");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));
        }
    }

    // 内网 RPC server(所有节点都开;承接转发请求 + 副本 receive rollout)。
    // 保存 JoinHandle:shutdown 时 main 等其 graceful 退出,避免进程退出中断正在写的
    // rollout(receive_rollout 的 seek+write 非原子,中断可损坏副本文件)。
    let internal_rpc_handle = {
        let internal_state = state.clone();
        let internal_app = build_internal_router(internal_state);
        let addr = format!("{}:{}", cfg.cluster.internal_rpc_host, cfg.internal_rpc_port());
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
        })
    };

    let codex_for_shutdown = state.codex.clone();
    let app = build_router(state.clone()).await.layer(ws_layer);

    // 添加 CORS 中间件(开发环境允许 localhost:5173)
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::predicate(|origin, _req| {
            // 允许 localhost 的任意端口(开发环境)
            let origin_str = origin.to_str().unwrap_or("");
            origin_str.starts_with("http://localhost:")
                || origin_str.starts_with("http://127.0.0.1:")
                || origin_str.starts_with("https://")
        }))
        .allow_methods(tower_http::cors::AllowMethods::any())
        .allow_headers(tower_http::cors::AllowHeaders::any());
    let app = app.layer(cors);

    // 独立挂载 hook webhook(per-user workspace 实施步骤 9):不走 /api,不走 JWT 中间件。
    let hook_router = axum::Router::new()
        .route(
            "/hooks/codex",
            axum::routing::post(hooks::handle),
        )
        .with_state(state.clone());
    // 把 app merge 进 hook router
    let app = app.merge(hook_router);

    let listener = tokio::net::TcpListener::bind((cfg.server.host.as_str(), cfg.server.port)).await?;
    tracing::info!("listening on {}:{}", cfg.server.host, cfg.server.port);

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("drain complete, shutting down codex");
    // 等 internal RPC server graceful 退出(避免中断正在写的 rollout),再销毁 codex。
    let _ = internal_rpc_handle.await;
    // abort 副本维护 loop task:释放其持有的 state.clone()(audit_writer sender),
    // 否则 audit_writer 的 rx 永不返回 None → flush 永久阻塞(死锁)。
    replica_maintenance_handle.abort();
    // abort 扩展周期同步 loop task(同上理由:释放 state.clone())。同时 abort 事件订阅
    // task:它持 mt_event_bus Arc + state.clone()(含 audit_writer sender),abort 后 task
    // 在 recv().await 处被取消,释放 sender,audit flush 不必靠下方 5s 超时兜底(避免丢日志)。
    if let Some(h) = ext_sync_handle.take() {
        h.abort();
    }
    if let Some(h) = ext_event_handle.take() {
        h.abort();
    }
    // drop state 释放 audit_writer 的 mpsc sender(本节点最后一份,除非有 promote_resume_thread
    // 短命 task 仍在跑 —— 短命,退出后释放);后台 task flush 剩余 buf 后退出。
    drop(state);
    // audit_writer flush:带 5s 超时兜底,防 promote_resume_thread(resume 各 thread,10s/thread)
    // 在 failover 期间仍持 sender 导致短暂阻塞;超时则放弃 flush(best-effort 审计)。
    let _ = tokio::time::timeout(Duration::from_secs(5), audit_writer_handle).await;
    codex_for_shutdown.destroy().await;
    Ok(())
}

/// 周期维护:遍历 session_replicas,主节点续约+复制;副本节点探测主失活并晋升。
async fn run_replica_maintenance(state: &AppState) {
    use sea_orm::EntityTrait;
    // 节流 rebalance:每 5 分钟最多触发一次(防抖动 —— 维护循环 15s 跑一轮,频繁迁移会
    // 反复改 primary_node + 清 Redis offset,造成 rollout 全量同步风暴)。
    // static AtomicI64 进程内全局,多副本各节点独立计数(各节点各自判断是否过热)。
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_REBALANCE_MS: AtomicI64 = AtomicI64::new(0);
    const REBALANCE_INTERVAL_MS: i64 = 300_000; // 5 分钟
    let now = codex_webui::services::multitenant::now_ms();
    if now - LAST_REBALANCE_MS.load(Ordering::Relaxed) >= REBALANCE_INTERVAL_MS {
        LAST_REBALANCE_MS.store(now, Ordering::Relaxed);
        if let Err(e) =
            codex_webui::services::multitenant::rebalance::maybe_rebalance(state).await
        {
            tracing::warn!(error = %e, "rebalance failed");
        }
    }
    // 孤儿 thread 认领(重启换 id 等场景;确定性,仅最低 alive id 节点执行)。
    let _ = replication::reclaim_orphan_threads(&state.db, state.cluster.as_ref(), state.mt_redis.as_ref()).await;
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
        let thread_id = row.thread_id.clone();
        // 确保副本已分配(扩容后回填 None,否则主挂无人晋升)。
        let _ = replication::ensure_replica(&state.db, &thread_id, state.cluster.as_ref()).await;
        if row.primary_node == state.node_id {
            if let Err(e) = replication::renew_lease(&state.db, &thread_id, &state.node_id, state.mt_redis.as_ref()).await {
                tracing::warn!(error = %e, thread_id = %thread_id, "renew_lease failed");
            }
            let _ = replication::replicate_thread_rollout(
                &state.db,
                &thread_id,
                &state.codex_home,
                state.cluster.as_ref(),
                state.mt_redis.as_ref(),
                &state.worker_rpc,
                &state.active_rollout,
                &state.local_offsets,
            )
            .await;
            // per-thread workspace 文件增量同步(failover 后副本可见文件)。
            let _ = codex_webui::services::workspace::file_sync::scan_and_replicate(&state, &thread_id).await;
        } else if row.replica_node.as_deref() == Some(state.node_id.as_str()) {
            match replication::promote_if_primary_down(
                &state.db,
                &thread_id,
                state.cluster.as_ref(),
                state.mt_redis.as_ref(),
                &state.active_rollout,
                &state.local_offsets,
            )
            .await
            {
                Ok(true) => {
                    // M1 修复:promote_resume_thread spawn 独立 task,不阻塞维护循环。
                    // 即便单 thread resume 带 10s 超时,spawn 后维护循环也能继续给其他 thread 续约,
                    // 避免排在后面的 thread 拿不到 renew_lease → 副本误判主失活 → 雪崩式切主。
                    let st = state.clone();
                    let tid = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = promote_resume_thread(&st, &tid).await {
                            tracing::warn!(error = %e, thread_id = %tid, "promote resume failed");
                        }
                    });
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, thread_id = %thread_id, "promote check failed"),
            }
        }
    }
}

/// 副本晋升后:对单个 thread 调 thread/resume 续接。
///
/// 10s 超时上限(M1):单 thread resume 卡死不拖累维护循环的其他续约。
async fn promote_resume_thread(state: &AppState, thread_id: &str) -> Result<(), codex_webui::error::AppError> {
    let params = serde_json::json!({ "threadId": thread_id, "persistExtendedHistory": true });
    let resume = state.codex.request("thread/resume", Some(params));
    match tokio::time::timeout(std::time::Duration::from_secs(10), resume).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, thread_id = %thread_id, "resume after promote failed (non-fatal)"),
        Err(_) => tracing::warn!(thread_id = %thread_id, "resume after promote timed out (10s, non-fatal)"),
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

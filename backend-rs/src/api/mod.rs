//! API 层：HTTP 路由处理器、WebSocket 网关、请求/响应 DTO。

pub mod auth;
pub mod chat;
pub mod event_subscribers;
pub mod files;
pub mod health;
pub mod logs;
pub mod multitenant;
pub mod onlyoffice;
pub mod proxies;
pub mod realtime;
pub mod settings;
pub mod sqlite;
pub mod terminal;
pub mod threads;

use crate::auth::middleware::require_auth;
use crate::state::AppState;
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode, Uri},
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use crate::error::Json;
use rust_embed::RustEmbed;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// OpenAPI 文档规格。paths / schemas 随各 Phase 逐步补全。
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Codex WebUI",
        version = "0.1.0",
        description = "Codex WebUI API — Rust backend (migrated from NestJS)",
    ),
    components(schemas(
        crate::error::ErrorResponse,
        crate::error::GenericJson,
        // auth
        crate::auth::LoginRequest,
        crate::auth::LoginResponse,
        // logs
        crate::api::logs::LogEntry,
        crate::api::logs::LogsResponse,
        crate::api::logs::LogsExportResponse,
        crate::api::logs::SystemInfo,
        // sqlite
        crate::api::sqlite::BreakdownDto,
        crate::api::sqlite::TurnUsageDto,
        crate::api::sqlite::TurnTokenUsageDto,
        crate::api::sqlite::ThreadTokenUsageResponse,
        crate::api::sqlite::TurnDiffDto,
        crate::api::sqlite::ThreadTurnDiffsResponse,
        crate::api::sqlite::TurnErrorDto,
        crate::api::sqlite::ThreadTurnErrorsResponse,
        crate::api::sqlite::PendingServerRequestDto,
        crate::api::sqlite::ListPendingResponse,
        crate::api::sqlite::RespondRequestBody,
        // settings
        crate::api::settings::SettingDto,
        crate::api::settings::SettingListResponse,
        crate::api::settings::SettingBatchEntry,
        crate::api::settings::SettingBatchUpdateBody,
        crate::api::settings::UpdatePayload,
        // files
        crate::api::files::AddRootBody,
        crate::api::files::CreateFileBody,
        crate::api::files::CreateDirBody,
        crate::api::files::WriteFileBody,
        crate::api::files::RenameBody,
        crate::api::files::CopyMoveBody,
        // threads
        crate::api::threads::CreateThreadBody,
        crate::api::threads::StartTurnBody,
        crate::api::threads::RollbackBody,
        crate::api::threads::SetNameBody,
        // proxies
        crate::api::proxies::LoginBody,
        crate::api::proxies::LoginCancelBody,
        crate::api::proxies::McpOauthBody,
        crate::api::proxies::SkillConfigBody,
        crate::api::proxies::PluginInstallBody,
        crate::api::proxies::PluginUninstallBody,
        // codex_status_config
        crate::services::codex_status_config::ApprovalPolicyBody,
        crate::services::codex_status_config::SandboxModeBody,
        crate::services::codex_status_config::UpdateConfigBody,
        crate::services::codex_status_config::ConfigEdit,
        crate::services::codex_status_config::UpdateRawConfigBody,
        // onlyoffice
        crate::api::onlyoffice::CallbackBody,
    )),
    paths(
        // system
        crate::api::health::ping,
        // auth
        crate::api::auth::login,
        crate::api::auth::logout,
        // logs
        crate::api::logs::list_logs,
        crate::api::logs::export_diagnostics,
        // sqlite
        crate::api::sqlite::read_token_usage,
        crate::api::sqlite::read_latest_token_usage,
        crate::api::sqlite::read_turn_diffs,
        crate::api::sqlite::read_turn_errors,
        crate::api::sqlite::list_pending,
        crate::api::sqlite::respond_to_request,
        // settings
        crate::api::settings::list,
        crate::api::settings::get_one,
        crate::api::settings::update_batch,
        crate::api::settings::update_one,
        crate::api::settings::delete_one,
        // files
        crate::api::files::get_roots,
        crate::api::files::add_root,
        crate::api::files::read_tree,
        crate::api::files::read_file,
        crate::api::files::get_metadata,
        crate::api::files::delete_path,
        crate::api::files::create_file,
        crate::api::files::create_directory,
        crate::api::files::write_file,
        crate::api::files::serve_file,
        crate::api::files::download_file,
        crate::api::files::rename_path,
        crate::api::files::copy_path,
        crate::api::files::move_path,
        crate::api::files::upload_files,
        crate::api::files::archive_list,
        crate::api::files::archive_entry,
        // threads
        crate::api::threads::create_thread,
        crate::api::threads::list_threads,
        crate::api::threads::list_loaded_threads,
        crate::api::threads::read_thread,
        crate::api::threads::resume_thread,
        crate::api::threads::start_turn,
        crate::api::threads::steer_turn,
        crate::api::threads::interrupt_turn,
        crate::api::threads::archive_thread,
        crate::api::threads::unarchive_thread,
        crate::api::threads::compact_thread,
        crate::api::threads::fork_thread,
        crate::api::threads::rollback_thread,
        crate::api::threads::set_thread_name,
        // proxies
        crate::api::proxies::account_read,
        crate::api::proxies::account_login,
        crate::api::proxies::account_login_cancel,
        crate::api::proxies::account_logout,
        crate::api::proxies::account_rate_limits,
        crate::api::proxies::apps_list,
        crate::api::proxies::models_list,
        crate::api::proxies::mcp_servers_list,
        crate::api::proxies::mcp_servers_reload,
        crate::api::proxies::mcp_servers_oauth_login,
        crate::api::proxies::skills_list,
        crate::api::proxies::skills_config_write,
        crate::api::proxies::plugins_list,
        crate::api::proxies::plugins_detail,
        crate::api::proxies::plugins_install,
        crate::api::proxies::plugins_uninstall,
        // codex_status_config
        crate::services::codex_status_config::status,
        crate::services::codex_status_config::update_approval_policy,
        crate::services::codex_status_config::update_sandbox_mode,
        crate::services::codex_status_config::read_config,
        crate::services::codex_status_config::update_config,
        crate::services::codex_status_config::read_raw_config,
        crate::services::codex_status_config::update_raw_config,
        // onlyoffice + chat
        crate::api::onlyoffice::get_config,
        crate::api::onlyoffice::handle_callback,
        crate::api::chat::upload_attachment,
    ),
    tags(
        (name = "system", description = "健康检查 / 探针"),
        (name = "auth", description = "认证 / 授权（JWT + API key）"),
        (name = "logs", description = "日志读取与诊断导出"),
        (name = "threads", description = "会话与 turn（含 token 用量 / 差异 / 错误）"),
        (name = "approvals", description = "待处理审批"),
        (name = "settings", description = "运行时设置 CRUD"),
        (name = "files", description = "工作区文件操作（路径安全边界内）"),
        (name = "account", description = "账户登录/登出/速率限制（codex 代理）"),
        (name = "apps", description = "应用列表（codex 代理）"),
        (name = "models", description = "模型列表（codex 代理）"),
        (name = "mcp-servers", description = "MCP 服务端（codex 代理）"),
        (name = "skills", description = "技能（codex 代理）"),
        (name = "plugins", description = "插件（codex 代理）"),
        (name = "codex", description = "codex 就绪状态与配置"),
        (name = "onlyoffice", description = "OnlyOffice 文档编辑集成"),
        (name = "chat", description = "聊天附件上传"),
    )
)]
struct ApiDoc;

/// 在 /api/docs-json 提供 OpenAPI JSON 规格(公开;用于 SDK 生成)。
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// M5-B Prometheus 指标(公开,供 Prometheus 抓取)。
async fn metrics_endpoint(axum::extract::State(state): axum::extract::State<AppState>) -> String {
    state.metrics_handle.as_ref().map(|h| h.render()).unwrap_or_default()
}

/// 构建应用路由。
///
/// 布局(与 NestJS globalPrefix 'api' 对齐):
/// - `POST /api/auth/login`   — 公开(JWT 登录)
/// - `POST /api/onlyoffice/callback` — 公开(OO 保存回调)
/// - `GET  /api/docs-json`    — 公开(OpenAPI 规格)
/// - `/api/*` 下的其余路由 — 受 require_auth 保护
pub async fn build_router(state: AppState) -> Router {
    use crate::api::chat as chat_mod;
    use crate::services::codex_status_config as csc;
    use crate::api::files as fl;
    use crate::api::onlyoffice as oo;
    use crate::api::proxies as px;
    use crate::api::settings as s;
    use crate::api::sqlite as sq;
    use crate::api::threads as th;

    // 上传体积上限（取自 settings 的 `files.uploadMaxBytes`，默认 100 MB）。
    // axum 的 `Multipart` 提取器在缺少 `DefaultBodyLimit` 时回落到 2 MB 默认值，
    // 会让 /chat/upload 与 /files/upload 任何 >2 MB 的上传都被拒绝；这里显式覆盖。
    let upload_limit = state.settings_reader().get_upload_max_bytes().await as usize;

    // 受保护的 API 子路由。
    let api = Router::new()
        // ── Phase 0 探针(同时作为 GET /api/status,与 AppController 对齐)──
        .route("/_ping", get(health::ping))
        .route("/status", get(health::ping))
        // ── chat 上传(受保护;multipart)──
        .route("/chat/upload", post(chat_mod::upload_attachment).layer(axum::extract::DefaultBodyLimit::disable()))
        // ── settings 增删改查(CRUD)──
        .route("/settings", get(s::list).patch(s::update_batch))
        .route("/settings/{key}", get(s::get_one).patch(s::update_one).delete(s::delete_one))
        // ── auth 登出(受保护,与 TS 对齐)──
        .route("/auth/logout", post(auth::logout))
        // ── 线程维度读取 ──
        .route("/threads/{threadId}/token-usage/latest", get(sq::read_latest_token_usage))
        .route("/threads/{threadId}/token-usage", get(sq::read_token_usage))
        .route("/threads/{threadId}/turn-diffs", get(sq::read_turn_diffs))
        .route("/threads/{threadId}/turn-errors", get(sq::read_turn_errors))
        // ── 待审批(读取 + 响应)──
        .route("/pending-approvals", get(sq::list_pending))
        .route(
            "/pending-approvals/{requestId}/respond",
            post(sq::respond_to_request),
        )
        // ── 日志 ──
        .route("/logs", get(crate::api::logs::list_logs))
        .route("/logs/export", get(crate::api::logs::export_diagnostics))
        // ── account(codex 代理)──
        .route("/account", get(px::account_read))
        .route("/account/login", post(px::account_login))
        .route("/account/login/cancel", post(px::account_login_cancel))
        .route("/account/logout", post(px::account_logout))
        .route("/account/rate-limits", get(px::account_rate_limits))
        // ── apps / models(codex 代理)──
        .route("/apps", get(px::apps_list))
        .route("/models", get(px::models_list))
        // ── mcp-servers(codex 代理)──
        .route("/mcp-servers", get(px::mcp_servers_list))
        .route("/mcp-servers/reload", post(px::mcp_servers_reload))
        .route("/mcp-servers/oauth/login", post(px::mcp_servers_oauth_login))
        // ── skills(codex 代理)──
        .route("/skills", get(px::skills_list))
        .route("/skills/config", post(px::skills_config_write))
        // ── plugins(codex 代理)──
        .route("/plugins", get(px::plugins_list))
        .route("/plugins/detail", get(px::plugins_detail))
        .route("/plugins/install", post(px::plugins_install))
        .route("/plugins/uninstall", post(px::plugins_uninstall))
        // ── threads + turns(codex 代理)──
        .route("/threads", post(th::create_thread).get(th::list_threads))
        .route("/threads/loaded", get(th::list_loaded_threads))
        .route("/threads/{threadId}", get(th::read_thread))
        .route("/threads/{threadId}/resume", post(th::resume_thread))
        .route("/threads/{threadId}/turns", post(th::start_turn))
        .route("/threads/{threadId}/turns/{turnId}/steer", post(th::steer_turn))
        .route("/threads/{threadId}/turns/{turnId}/interrupt", post(th::interrupt_turn))
        .route("/threads/{threadId}/archive", post(th::archive_thread))
        .route("/threads/{threadId}/unarchive", post(th::unarchive_thread))
        .route("/threads/{threadId}/compact", post(th::compact_thread))
        .route("/threads/{threadId}/fork", post(th::fork_thread))
        .route("/threads/{threadId}/rollback", post(th::rollback_thread))
        .route("/threads/{threadId}/name", axum::routing::patch(th::set_thread_name))
        // ── codex 状态 + 配置 ──
        .route("/codex/status", get(csc::status))
        .route("/codex/approval-policy", post(csc::update_approval_policy))
        .route("/codex/sandbox-mode", post(csc::update_sandbox_mode))
        .route(
            "/codex/config",
            get(csc::read_config).patch(csc::update_config),
        )
        .route("/codex/config/raw", get(csc::read_raw_config).put(csc::update_raw_config))
        // ── files(完整文件操作)──
        .route("/files/roots", get(fl::get_roots).post(fl::add_root))
        .route("/files/tree", get(fl::read_tree))
        .route("/files/read", get(fl::read_file))
        .route("/files/metadata", get(fl::get_metadata))
        .route("/files/delete", axum::routing::delete(fl::delete_path))
        .route("/files/create-file", post(fl::create_file))
        .route("/files/create-directory", post(fl::create_directory))
        .route("/files/write", post(fl::write_file))
        .route("/files/serve", get(fl::serve_file))
        .route("/files/download", get(fl::download_file))
        .route("/files/rename", post(fl::rename_path))
        .route("/files/copy", post(fl::copy_path))
        .route("/files/move", post(fl::move_path))
        .route("/files/upload", post(fl::upload_files).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/files/archive/list", get(fl::archive_list))
        .route("/files/archive/entry", get(fl::archive_entry))
        // ── onlyoffice 配置(受保护)──
        .route("/onlyoffice/config", get(oo::get_config))
        .layer(axum::extract::DefaultBodyLimit::max(upload_limit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // 多租户路由(M1):/api/mt/auth/* 公开;/api/mt/teams/* 受 require_user_auth 保护。
    use crate::api::multitenant::handlers as mt;
    let mt_protected: Router<AppState> = Router::new()
        .route("/teams", post(mt::create_team).get(mt::list_teams))
        .route("/teams/join", post(mt::join_team))
        .route("/teams/{teamId}/members", get(mt::list_members))
        .route("/teams/{teamId}/invitations", post(mt::create_invitation))
        .route(
            "/teams/{teamId}/api-key",
            post(mt::set_team_api_key).get(mt::list_team_api_keys),
        )
        .route("/teams/{teamId}/audit", get(mt::list_audit))
        .route("/threads", post(mt::mt_create_thread).get(mt::mt_list_threads))
        .route("/threads/{threadId}/turns", post(mt::mt_start_turn))
        .route(
            "/teams/{teamId}/members/{userId}",
            axum::routing::delete(mt::remove_member),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::multitenant::middleware::require_user_auth,
        ));
    let mt_router: Router<AppState> = Router::new()
        .route("/auth/register", post(mt::register))
        .route("/auth/login", post(mt::login))
        .route("/auth/refresh", post(mt::refresh))
        .merge(mt_protected);

    // 为 React 前端(SPA)提供静态文件服务。
    Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/onlyoffice/callback", post(oo::handle_callback))
        .route("/api/docs-json", get(openapi_json))
        .route("/metrics", get(metrics_endpoint))
        .merge(
            SwaggerUi::new("/api/docs")
                .url("/api/openapi.json", ApiDoc::openapi())
                .config(
                    utoipa_swagger_ui::Config::default().default_model_expand_depth(-1),
                ),
        )
        .nest("/api/mt", mt_router)
        .nest("/api", api)
        .fallback(serve_asset)
        .layer(from_fn(request_logger))
        .with_state(state)
}

// ── 前端静态资源（rust-embed 嵌入）──────────────────────────────────────────

/// 前端 build 产物（vite outDir = `backend-rs/public`，相对 backend-rs 即 `public`）。
/// debug 模式从文件系统实时读，release 模式编译期嵌入二进制。
#[derive(RustEmbed)]
#[folder = "public"]
struct WebAsset;

/// 从嵌入资源提供前端静态文件；未命中则回退 `index.html`（SPA 客户端路由）。
async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = WebAsset::get(path) {
        return asset_response(path, &file);
    }
    // SPA 回退：未知路径返回 index.html，交由前端路由处理。
    if let Some(file) = WebAsset::get("index.html") {
        return asset_response("index.html", &file);
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// 构造静态文件响应：按扩展名推断 Content-Type。
fn asset_response(path: &str, file: &rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(file.data.clone().into_owned()))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to build response").into_response()
        })
}

/// 请求日志中间件（对齐 TS pino-http）：记录 method / 脱敏 path / status / 耗时。
async fn request_logger(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = crate::logging::sanitize_url(&req.uri().to_string());
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = resp.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "request"
    );
    resp
}

//! API 层：HTTP 路由处理器、WebSocket 网关、请求/响应 DTO。

pub mod chat;
pub mod files;
pub mod health;
pub mod hooks;
pub mod logs;
pub mod multitenant;
pub mod onlyoffice;
pub mod realtime;
pub mod settings;

use crate::state::AppState;
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode, Uri},
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
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
        // logs
        crate::api::logs::LogEntry,
        crate::api::logs::LogsResponse,
        crate::api::logs::LogsExportResponse,
        crate::api::logs::SystemInfo,
        // settings
        crate::api::settings::SettingDto,
        crate::api::settings::SettingListResponse,
        crate::api::settings::SettingBatchEntry,
        crate::api::settings::SettingBatchUpdateBody,
        crate::api::settings::UpdatePayload,
        // files
        crate::api::files::CreateFileBody,
        crate::api::files::CreateDirBody,
        crate::api::files::WriteFileBody,
        crate::api::files::RenameBody,
        crate::api::files::CopyMoveBody,
        // onlyoffice
        crate::api::onlyoffice::CallbackBody,
    )),
    paths(
        // system
        crate::api::health::ping,
        // logs
        crate::api::logs::list_logs,
        crate::api::logs::export_diagnostics,
        // settings
        crate::api::settings::list,
        crate::api::settings::get_one,
        crate::api::settings::update_batch,
        crate::api::settings::update_one,
        crate::api::settings::delete_one,
        // files
        crate::api::files::get_roots,
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
        // codex_status_config(只读 status/read_config)
        crate::services::codex_status_config::status,
        crate::services::codex_status_config::read_config,
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
/// - `POST /api/mt/auth/*`  — 公开(多租户注册/登录/刷新)
/// - `POST /api/onlyoffice/callback` — 公开(OO 保存回调)
/// - `GET  /api/docs-json`    — 公开(OpenAPI 规格)
/// - `/api/*` 下的其余路由 — 受 require_user_auth 保护(多租户 JWT)
pub async fn build_router(state: AppState) -> Router {
    use crate::api::chat as chat_mod;
    use crate::services::codex_status_config as csc;
    use crate::api::files as fl;
    use crate::api::onlyoffice as oo;
    use crate::api::settings as s;

    // 上传体积上限（取自 settings 的 `files.uploadMaxBytes`，默认 100 MB）。
    // axum 的 `Multipart` 提取器在缺少 `DefaultBodyLimit` 时回落到 2 MB 默认值，
    // 会让 /chat/upload 与 /files/upload 任何 >2 MB 的上传都被拒绝；这里显式覆盖。
    let upload_limit = state.settings_reader().get_upload_max_bytes().await as usize;

    // 平台管理员 gate layer(挂在 require_user_auth 之内,收紧全局敏感操作)。
    // Clone:axum 的 FromFnLayer 在 F: Clone + S: Clone 时派生 Clone,
    // 这里 F 是 fn 指针、S 是 AppState(均 Clone),可安全复用。
    let admin_layer = axum::middleware::from_fn_with_state(
        state.clone(),
        crate::multitenant::middleware::require_platform_admin_layer,
    );

    // 受保护的 API 子路由（统一使用多租户认证）。
    let api = Router::new()
        // ── Phase 0 探针(同时作为 GET /api/status,与 AppController 对齐)──
        .route("/_ping", get(health::ping))
        .route("/status", get(health::ping))
        // ── chat 上传(受保护;multipart)──
        .route("/chat/upload", post(chat_mod::upload_attachment).layer(axum::extract::DefaultBodyLimit::disable()))
        // ── settings 增删改查(CRUD)──
        // 收紧:GET(list/get_one)保持登录可读;PATCH/DELETE(写)收紧为平台管理员专属。
        // 通过 MethodRouter::merge 把 admin_layer 只套在写方法上,GET 不受影响。
        .route(
            "/settings",
            get(s::list).merge(patch(s::update_batch).layer(admin_layer.clone())),
        )
        .route(
            "/settings/{key}",
            get(s::get_one).merge(
                patch(s::update_one)
                    .merge(delete(s::delete_one))
                    .layer(admin_layer.clone()),
            ),
        )
        // ── 线程维度读取 ──
        // 注:threads 维度的 token-usage / turn-diffs / turn-errors / pending-approvals
        // 老路由已下线 —— 它们的 handler(sqlite.rs)不校验 thread→team 归属、不过滤 team_id,
        // 在统一多租户认证下构成跨租户越权(IDOR)。多租户安全版本位于 /api/mt/threads/*
        // (mt_token_usage / mt_turn_diffs / mt_turn_errors / mt_list_approvals / mt_resolve_approval),
        // 经 require_thread_team + team_id 过滤。前端请改用 /api/mt/* 路径。
        // ── 日志(全局敏感读:收紧为平台管理员专属)──
        .route(
            "/logs",
            get(crate::api::logs::list_logs).layer(admin_layer.clone()),
        )
        .route(
            "/logs/export",
            get(crate::api::logs::export_diagnostics).layer(admin_layer.clone()),
        )
        // 注:单租户 /api/account* /apps /models /mcp-servers* /skills* /plugins* 老路由已下线 ——
        // handler(api/proxies.rs)用全局 codex、不校验归属,统一多租户认证下任意普通 member 可
        // 改全局账号(account/login、logout)、改全局 MCP/skills/plugins 配置、读全局账号 email/plan
        // → 跨租户越权 IDOR(与已下线的 /api/threads*、sqlite.rs 同类)。多租户 team 应走 BYOK
        // (/api/mt/teams/{id}/api-key)而非全局 account;account/apps/models/mcp/skills/plugins 的
        // 多租户 per-team 版本待后续补全于 /api/mt/*。
        // ── codex 状态 + 配置 ──
        // codex 状态/配置(只读):写操作(approval-policy/sandbox-mode/config PATCH/config/raw PUT)
        // 已下线 —— 它们改全局 codex 配置,多租户认证下任意 member 可改全员配置(与已下线的
        // /api/proxies 同类 IDOR);read_raw_config 返回 config.toml 原文(可能含密钥)也下线。
        // 保留只读 status(就绪检查)与 read_config(redact_secrets 脱敏)。per-team 配置管理待补 /api/mt/*。
        .route("/codex/status", get(csc::status))
        .route("/codex/config", get(csc::read_config))
        // ── files(完整文件操作)──
        // 注:POST /files/roots(动态注册全局 root)已下线 —— 多租户认证下任意用户注册的
        // root 会被所有用户共享 → 文件 IDOR。files 操作改用管理员配置的公共工作区
        // (HOME + security.workspaceRoots)。多租户 per-user/team 文件操作走 workspace
        // (codex_home/users/{uid} + teams/{tid},经 hooks 决策隔离)。GET 保留(列出公共 roots)。
        //
        // 收紧:公共工作区写操作(create/delete/write/rename/copy/move/upload)收紧为平台管理员专属。
        // GET 读路由(roots/tree/read/metadata/download/archive)保持登录可读。
        .route("/files/roots", get(fl::get_roots))
        .route("/files/tree", get(fl::read_tree))
        .route("/files/read", get(fl::read_file))
        .route("/files/metadata", get(fl::get_metadata))
        .route(
            "/files/delete",
            delete(fl::delete_path).layer(admin_layer.clone()),
        )
        .route(
            "/files/create-file",
            post(fl::create_file).layer(admin_layer.clone()),
        )
        .route(
            "/files/create-directory",
            post(fl::create_directory).layer(admin_layer.clone()),
        )
        .route(
            "/files/write",
            post(fl::write_file).layer(admin_layer.clone()),
        )
        // 注:/files/serve 与 /files/archive/entry 移到 require_file_access(支持 ?access_token=
        // download token,供 OnlyOffice/内联预览加载),不在此 require_user_auth 层(只读头会 401)。
        .route("/files/download", get(fl::download_file))
        .route(
            "/files/rename",
            post(fl::rename_path).layer(admin_layer.clone()),
        )
        .route(
            "/files/copy",
            post(fl::copy_path).layer(admin_layer.clone()),
        )
        .route(
            "/files/move",
            post(fl::move_path).layer(admin_layer.clone()),
        )
        .route(
            "/files/upload",
            post(fl::upload_files)
                .layer(axum::extract::DefaultBodyLimit::disable())
                .layer(admin_layer.clone()),
        )
        .route("/files/archive/list", get(fl::archive_list))
        // ── onlyoffice 配置(受保护)──
        .route("/onlyoffice/config", get(oo::get_config))
        .layer(axum::extract::DefaultBodyLimit::max(upload_limit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::multitenant::middleware::require_user_auth,
        ));

    // 多租户路由(M1):/api/mt/auth/* 公开;/api/mt/teams/* 受 require_user_auth 保护。
    use crate::api::multitenant::handlers as mt;
    use crate::api::multitenant::extensions as mt_ext;
    let mt_protected: Router<AppState> = Router::new()
        .route("/me", get(mt::mt_me))
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
        .route("/threads/me", get(mt::mt_list_my_threads))
        .route("/threads/{threadId}", axum::routing::delete(mt::mt_delete_thread))
        .route("/threads/{threadId}/turns", post(mt::mt_start_turn))
        .route("/threads/{threadId}/invoke", post(mt::mt_invoke_thread))
        .route("/threads/{threadId}/token-usage", get(mt::mt_token_usage))
        .route("/threads/{threadId}/turn-diffs", get(mt::mt_turn_diffs))
        .route("/threads/{threadId}/turn-errors", get(mt::mt_turn_errors))
        .route("/threads/{threadId}/archive", post(mt::mt_archive_thread))
        .route(
            "/threads/{threadId}/name",
            axum::routing::patch(mt::mt_rename_thread),
        )
        .route(
            "/threads/{threadId}/approvals",
            get(mt::mt_list_approvals).post(mt::mt_resolve_approval),
        )
        .route("/teams/{teamId}/members/{userId}",            axum::routing::delete(mt::remove_member),        )
        // ── 生命周期 API(owner 转让 / team 解散 / 成员角色变更)──
        .route("/teams/{teamId}/transfer", post(mt::transfer_team_owner))
        .route("/teams/{teamId}", axum::routing::delete(mt::dissolve_team_handler))
        .route(
            "/teams/{teamId}/members/{userId}/role",
            axum::routing::patch(mt::set_member_role_handler),
        )
        .route("/user/api-key", post(mt::set_user_api_key).get(mt::list_user_api_keys))
        // ── 集群扩展分发(Task 6):skill 上传/列表/删除(单一安装入口)──
        // 鉴权收紧(spec §8):skill 是集群级共享资源,POST(上传)/DELETE(删除)收紧为平台管理员专属;
        // GET(列表)保持登录可读。写法与 settings/files 一致(admin_layer 只套在写方法上)。
        .route(
            "/extensions",
            get(mt_ext::list_extensions)
                .merge(post(mt_ext::upload_extension).layer(admin_layer.clone())),
        )
        .route(
            "/extensions/{id}",
            delete(mt_ext::delete_extension).layer(admin_layer.clone()),
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
        .route("/api/onlyoffice/callback", post(oo::handle_callback))
        .route("/api/docs-json", get(openapi_json))
        .route("/metrics", get(metrics_endpoint))
        // 文件内联预览/OnlyOffice 下载:独立鉴权层 require_file_access(支持 ?access_token=
        // download token),不能用 require_user_auth(只读 Authorization 头,query token 401)。
        .route(
            "/api/files/serve",
            get(fl::serve_file).layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::multitenant::middleware::require_file_access,
            )),
        )
        .route(
            "/api/files/archive/entry",
            get(fl::archive_entry).layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::multitenant::middleware::require_file_access,
            )),
        )
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

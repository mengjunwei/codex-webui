//! 路由处理器与路由构建。

pub mod auth;
pub mod health;

use crate::auth::middleware::require_auth;
use crate::state::AppState;
use axum::{
    routing::{get, post},
    Json, Router,
};
use tower_http::services::{ServeDir, ServeFile};
use utoipa::OpenApi;

/// OpenAPI 文档(基础规格;各端点的注解可后续补充)。
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Codex WebUI",
        version = "0.1.0",
        description = "Codex WebUI API — Rust backend (migrated from NestJS)",
    ),
)]
struct ApiDoc;

/// 在 /api/docs-json 提供 OpenAPI JSON 规格(公开;用于 SDK 生成)。
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// 构建应用路由。
///
/// 布局(与 NestJS globalPrefix 'api' 对齐):
/// - `POST /api/auth/login`   — 公开(JWT 登录)
/// - `POST /api/onlyoffice/callback` — 公开(OO 保存回调)
/// - `GET  /api/docs-json`    — 公开(OpenAPI 规格)
/// - `/api/*` 下的其余路由 — 受 require_auth 保护
pub fn build_router(state: AppState) -> Router {
    use crate::chat as chat_mod;
    use crate::codex_status_config as csc;
    use crate::files as fl;
    use crate::onlyoffice as oo;
    use crate::proxies as px;
    use crate::settings::handlers as s;
    use crate::sqlite_handlers as sq;
    use crate::threads as th;

    // 上传体积上限（取自 settings 的 `files.uploadMaxBytes`，默认 100 MB）。
    // axum 的 `Multipart` 提取器在缺少 `DefaultBodyLimit` 时回落到 2 MB 默认值，
    // 会让 /chat/upload 与 /files/upload 任何 >2 MB 的上传都被拒绝；这里显式覆盖。
    let upload_limit = state.settings_reader().get_upload_max_bytes() as usize;

    // 受保护的 API 子路由。
    let api = Router::new()
        // ── Phase 0 探针(同时作为 GET /api/status,与 AppController 对齐)──
        .route("/_ping", get(health::ping))
        .route("/status", get(health::ping))
        // ── chat 上传(受保护;multipart)──
        .route("/chat/upload", post(chat_mod::upload_attachment))
        // ── settings 增删改查(CRUD)──
        .route("/settings", get(s::list).patch(s::update_batch))
        .route("/settings/:key", get(s::get_one).patch(s::update_one).delete(s::delete_one))
        // ── auth 登出(受保护,与 TS 对齐)──
        .route("/auth/logout", post(auth::logout))
        // ── 线程维度读取 ──
        .route("/threads/:threadId/token-usage/latest", get(sq::read_latest_token_usage))
        .route("/threads/:threadId/token-usage", get(sq::read_token_usage))
        .route("/threads/:threadId/turn-diffs", get(sq::read_turn_diffs))
        .route("/threads/:threadId/turn-errors", get(sq::read_turn_errors))
        // ── 待审批(读取 + 响应)──
        .route("/pending-approvals", get(sq::list_pending))
        .route(
            "/pending-approvals/:requestId/respond",
            post(sq::respond_to_request),
        )
        // ── 日志 ──
        .route("/logs", get(crate::logs::list_logs))
        .route("/logs/export", get(crate::logs::export_diagnostics))
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
        .route("/threads/:threadId", get(th::read_thread))
        .route("/threads/:threadId/resume", post(th::resume_thread))
        .route("/threads/:threadId/turns", post(th::start_turn))
        .route("/threads/:threadId/turns/:turnId/steer", post(th::steer_turn))
        .route("/threads/:threadId/turns/:turnId/interrupt", post(th::interrupt_turn))
        .route("/threads/:threadId/archive", post(th::archive_thread))
        .route("/threads/:threadId/unarchive", post(th::unarchive_thread))
        .route("/threads/:threadId/compact", post(th::compact_thread))
        .route("/threads/:threadId/fork", post(th::fork_thread))
        .route("/threads/:threadId/rollback", post(th::rollback_thread))
        .route("/threads/:threadId/name", axum::routing::patch(th::set_thread_name))
        // ── codex 状态 + 配置 ──
        .route("/codex/status", get(csc::status))
        .route("/codex/approval-policy", post(csc::update_approval_policy))
        .route("/codex/sandbox-mode", post(csc::update_sandbox_mode))
        .route(
            "/codex/config",
            get(csc::read_config).patch(csc::update_config),
        )
        .route("/codex/config/raw", get(csc::read_raw_config).put(csc::update_raw_config))
        // ── files(完整文件操作:roots/tree/read/metadata/CRUD/serve/download/upload/归档预览)──
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
        .route("/files/upload", post(fl::upload_files))
        .route("/files/archive/list", get(fl::archive_list))
        .route("/files/archive/entry", get(fl::archive_entry))
        // ── onlyoffice 配置(受保护)──
        .route("/onlyoffice/config", get(oo::get_config))
        .layer(axum::extract::DefaultBodyLimit::max(upload_limit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // 为 React 前端(SPA)提供静态文件服务。
    // 服务 `public/` 目录;对于客户端路由,回退到 `public/index.html`。
    // 开发环境下(无 public/ 目录),回退返回 404 也无妨
    // (前端运行在 :5173 并通过代理)。
    let static_files = ServeDir::new("public")
        .fallback(ServeFile::new("public/index.html"));

    Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/onlyoffice/callback", post(oo::handle_callback))
        .route("/api/docs-json", get(openapi_json))
        .nest("/api", api)
        .fallback_service(static_files)
        .with_state(state)
}

//! 集群扩展分发 —— skill 上传/plugin 上传/列表/删除 REST API(Task 6 + plugin Task 5)。
//!
//! 这是"单一安装入口":
//! - skill:用户上传文件树(JSON base64),系统落盘本节点 + 入库 + 发事件触发集群同步。
//! - plugin:后端 spawn `codex plugin add <name>@<market>` 装好,再指纹化 cache + 入库 +
//!   发事件(其他节点订阅 "extensions:changed" 后走 Task 7 下载 + Task 8 应用)。
//!
//! 路由(挂载在 mt_protected,受 require_user_auth 保护):
//! - POST   /api/mt/extensions        上传 skill 文件树 或 装 plugin
//! - GET    /api/mt/extensions        列出 enabled 扩展
//! - DELETE /api/mt/extensions/{id}   删除扩展(清本地目录 + 删 DB 行 + 发事件)

use crate::error::{AppError, ErrorCode};
use crate::services::extensions::{apply, fingerprint, store};
use crate::services::multitenant::new_id;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 上传请求内的单个文件:相对路径 + base64 内容。
#[derive(Deserialize)]
pub struct UploadFile {
    pub rel_path: String,
    pub content_base64: String,
}

/// 上传请求体。
///
/// - skill 分支:`kind="skill"` + `files`(必填,JSON base64 文件树)。
/// - plugin 分支:`kind="plugin"` + `marketplace`(可选,默认 openai-api-curated),
///   无 `files`(后端 spawn codex 装好后自己扫描 cache)。
#[derive(Deserialize)]
pub struct UploadBody {
    pub kind: String,
    pub name: String,
    /// skill 必填;plugin 用不到(`#[serde(default)]` 让 plugin 请求可省略该字段)。
    #[serde(default)]
    pub files: Vec<UploadFile>,
    /// plugin 的市场名(skill 不用);缺省时取 "openai-api-curated"。
    #[serde(default)]
    pub marketplace: Option<String>,
}

/// 上传成功响应。
#[derive(Serialize)]
pub struct ExtResp {
    pub id: String,
    pub name: String,
    pub content_hash: String,
}

/// 列表项。
#[derive(Serialize)]
pub struct ListItem {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub enabled: bool,
}

/// POST /api/mt/extensions —— 统一上传入口(kind 分发)。
///
/// - kind="plugin" → 转发 `upload_plugin`(spawn codex plugin add + 指纹化 cache)。
/// - kind="skill"  → 走本函数剩余流程:校验 name/files → base64 解码每文件(校验单文件
///   大小上限)→ 写临时目录算指纹(scan_dir + aggregate_hash)→ 清旧同名目录 → 写
///   skills/{name}/ → 入库 upsert_extension + add_holder(本节点)→ 更新本地状态文件
///   → 发 "extensions:changed" 事件。
pub async fn upload_extension(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<UploadBody>,
) -> Result<Json<ExtResp>, AppError> {
    // plugin 分支:无 files,后端 spawn codex plugin add 装好后扫描 cache。
    if body.kind == "plugin" {
        return upload_plugin(state, body).await;
    }
    // 阶段 1 其余只支持 skill。
    if body.kind != "skill" {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "阶段1 仅支持 skill / plugin".into(),
            None,
        ));
    }
    if body.name.is_empty() || body.files.is_empty() {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "name 和 files 不能为空".into(),
            None,
        ));
    }
    // 上传总量上限:防滥用。解码后内容全驻内存,仅限制单文件不足以阻挡超大上传,
    // 故追加文件数上限与解码后总字节数上限。
    const MAX_FILE_COUNT: usize = 200;
    const MAX_TOTAL_BYTES: usize = 50 * 1024 * 1024; // 50 MB
    if body.files.len() > MAX_FILE_COUNT {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!("文件数 {} 超过上限 {}", body.files.len(), MAX_FILE_COUNT),
            None,
        ));
    }

    let max_file_bytes = state.cfg_extensions_max_file_bytes;

    // 1. base64 解码每文件,校验单文件大小,写临时目录。
    let skills_root = apply::skills_dir(&state.codex_home);
    tokio::fs::create_dir_all(&skills_root)
        .await
        .map_err(|e| AppError::internal(format!("mkdir skills root: {e}")))?;
    let tmp = tempfile::tempdir().map_err(|e| AppError::internal(format!("tmp: {e}")))?;
    // 预先把所有文件内容解码好(校验 + 供后续正式落盘复用,避免二次解码)。
    let mut decoded: Vec<(String, Vec<u8>)> = Vec::with_capacity(body.files.len());
    let mut total_bytes: usize = 0;
    for f in &body.files {
        let bytes = base64_decode(&f.content_base64).map_err(|e| {
            AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                format!("base64 解码失败: {e}"),
                None,
            )
        })?;
        if bytes.len() as u64 > max_file_bytes {
            return Err(AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                format!("文件 {} 超过单文件上限", f.rel_path),
                None,
            ));
        }
        // 累计解码后总字节,超上限即拒绝(防止内存被超大上传耗尽)。
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_TOTAL_BYTES {
            return Err(AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                format!(
                    "上传总字节数 {} 超过上限 {}",
                    total_bytes, MAX_TOTAL_BYTES
                ),
                None,
            ));
        }
        apply::write_file_safe(tmp.path(), &f.rel_path, &bytes).await?;
        decoded.push((f.rel_path.clone(), bytes));
    }

    // 2. 扫描临时目录算指纹 + 聚合 hash(与遍历顺序无关)。
    let fps = fingerprint::scan_dir(tmp.path()).await?;
    let content_hash = fingerprint::aggregate_hash(&fps);

    // 3. 落盘到正式 skills/{name}/(先清旧同名目录,全量替换)。
    apply::remove_dir_safe(&skills_root, &body.name).await?;
    for (rel_path, bytes) in &decoded {
        apply::write_file_safe(&skills_root.join(&body.name), rel_path, bytes).await?;
    }

    // 4. 入库 + 本节点登记 holder。
    //    同名重传复用现有 id:命中 → upsert 走 update 分支(created_at 保留、content_hash + files 全量替换);
    //    未命中 → new_id() 新建。避免 UNIQUE(kind,name) 冲突与旧行残留,local_state 亦按 id 写(key 不变)。
    let id = store::find_id_by_kind_name(&state.db, &body.kind, &body.name)
        .await?
        .unwrap_or_else(new_id);
    let rec = store::ExtRecord {
        id: id.clone(),
        kind: body.kind.clone(),
        name: body.name.clone(),
        content_form: "files".into(),
        content_hash: content_hash.clone(),
        enabled: true,
        // skill 上传走此分支,marketplace/version 仅 plugin 有 → None。
        marketplace: None,
        version: None,
    };
    store::upsert_extension(&state.db, &rec, &fps).await?;
    store::add_holder(&state.db, &id, &state.node_id).await?;

    // 5. 更新本地状态文件(id → {name, hash})：name 供副本删除分支定位目录(不查 PG),
    //    hash 供同步循环对齐。同名重传 id 复用,key 不变,值全量替换。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(
        id.clone(),
        apply::LocalExtEntry {
            name: body.name.clone(),
            hash: content_hash.clone(),
        },
    );
    apply::save_local_state(&state.codex_home, &st).await?;

    // 6. 发事件触发其他节点同步(无订阅者/无 bus 时静默,best-effort)。
    if let Some(bus) = &state.mt_event_bus {
        let _ = bus
            .publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}"))
            .await;
    }
    metrics::counter!("mt_extension_upload_total").increment(1);

    Ok(Json(ExtResp {
        id,
        name: body.name,
        content_hash,
    }))
}

/// POST /api/mt/extensions (kind="plugin") —— spawn codex plugin add + 指纹化 cache + 入库。
///
/// 流程:校验 name/market → spawn `codex plugin add <name>@<market>`(CODEX_HOME 指向本节点)
/// → 扫描 `plugins/cache/<market>/<name>/` 取 version 子目录(唯一/最新 mtime)
/// → 指纹化 `<version>/` → 写启用段 → 入库 upsert_extension + add_holder → 更新本地状态
/// → 发 "extensions:changed" 事件。
///
/// **codex 子进程构造**:复用 `process::build_codex_command`(处理 Windows npm 垫片
/// 的 node 直启),而非 `Command::new(codex_bin)` —— 否则 npm 全局安装的 codex 会因
/// `.cmd` 垫片 + CreateProcess 不补 `.cmd` 扩展名而失败。
async fn upload_plugin(state: AppState, body: UploadBody) -> Result<Json<ExtResp>, AppError> {
    // name 非法校验:空 / 含 @ 会破坏 codex plugin add 的 `name@market` 参数解析。
    if body.name.is_empty() || body.name.contains('@') {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "plugin name 非法(空或含 @)".into(),
            None,
        ));
    }
    // market:缺省取 openai-api-curated;非空且不含 @ 才采纳,否则回退默认。
    let market = body
        .marketplace
        .as_deref()
        .filter(|s| !s.is_empty() && !s.contains('@'))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "openai-api-curated".to_string());

    // 1. spawn codex plugin add <name>@<market>,装到本节点 CODEX_HOME。
    //    .output().await 等待退出(codex 下载可能较慢,本 task 不加超时)。
    let codex_bin = state.codex.codex_bin();
    let mut cmd = crate::codex::process::build_codex_command(codex_bin);
    cmd.arg("plugin")
        .arg("add")
        .arg(format!("{}@{}", body.name, market))
        .env("CODEX_HOME", &state.codex_home)
        .current_dir(&state.codex_home);
    let out = cmd
        .output()
        .await
        .map_err(|e| AppError::internal(format!("spawn codex plugin add: {e}")))?;
    if !out.status.success() {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!(
                "codex plugin add 失败: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
            None,
        ));
    }

    // 2. 扫描 cache/<market>/<name>/ 下 version 子目录(唯一则取,多个取最新 mtime)。
    let base = apply::plugins_cache_dir(&state.codex_home)
        .join(&market)
        .join(&body.name);
    let version = pick_version(&base).await?;

    // 3. 指纹化 cache/<market>/<name>/<version>/(段校验在 plugin_dest 内)。
    let dest = apply::plugin_dest(&state.codex_home, &market, &body.name, &version)?;
    let fps = fingerprint::scan_dir(&dest).await?;
    let content_hash = fingerprint::aggregate_hash(&fps);

    // 4. 写启用段 [plugins."<name>@<market>"] enabled=true。
    apply::enable_plugin_config(&state.codex_home, &body.name, &market).await?;

    // 5. 入库 + 本节点 holder(同名重传复用 id,同 skill 分支语义)。
    let id = store::find_id_by_kind_name(&state.db, "plugin", &body.name)
        .await?
        .unwrap_or_else(new_id);
    let rec = store::ExtRecord {
        id: id.clone(),
        kind: "plugin".into(),
        name: body.name.clone(),
        content_form: "files".into(),
        content_hash: content_hash.clone(),
        enabled: true,
        marketplace: Some(market.clone()),
        version: Some(version.clone()),
    };
    store::upsert_extension(&state.db, &rec, &fps).await?;
    store::add_holder(&state.db, &id, &state.node_id).await?;

    // 6. 本地状态文件(id → {name, hash})。
    //    Task 6 会扩 LocalExtEntry 加 kind/market/version;本 task 先用现有结构。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(
        id.clone(),
        apply::LocalExtEntry {
            name: body.name.clone(),
            hash: content_hash.clone(),
        },
    );
    apply::save_local_state(&state.codex_home, &st).await?;

    // 7. 发事件触发其他节点同步(best-effort,无 bus/订阅者时静默)。
    if let Some(bus) = &state.mt_event_bus {
        let _ = bus
            .publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}"))
            .await;
    }
    metrics::counter!("mt_extension_upload_total").increment(1);

    Ok(Json(ExtResp {
        id,
        name: body.name,
        content_hash,
    }))
}

/// 从 plugin cache 目录(`plugins/cache/<market>/<name>/`)选取 version 子目录。
///
/// - 唯一子目录:直接取之。
/// - 多个子目录:取 mtime 最新(codex 刚写入的版本通常 mtime 最新,比字符串排序更稳)。
/// - 无子目录:internal err(说明 codex plugin add 未产出预期目录结构)。
async fn pick_version(base: &Path) -> Result<String, AppError> {
    let mut rd = tokio::fs::read_dir(base)
        .await
        .map_err(|e| AppError::internal(format!("read plugin dir {}: {e}", base.display())))?;
    let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
    while let Some(e) = rd
        .next_entry()
        .await
        .map_err(|e| AppError::internal(format!("readdir: {e}")))?
    {
        // 仅收集子目录(version 是目录名,非文件)。
        if e.file_type()
            .await
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            let mtime = e
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((e.file_name().to_string_lossy().to_string(), mtime));
        }
    }
    if entries.is_empty() {
        return Err(AppError::internal(format!(
            "plugin cache 无 version 子目录: {}",
            base.display()
        )));
    }
    // 按 mtime 降序(最新优先),取第一个。
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(entries.into_iter().next().unwrap().0)
}

/// GET /api/mt/extensions —— 列出所有 enabled 扩展。
pub async fn list_extensions(State(state): State<AppState>) -> Result<Json<Vec<ListItem>>, AppError> {
    let rows = store::list_enabled(&state.db).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| ListItem {
                id: r.id,
                kind: r.kind,
                name: r.name,
                enabled: r.enabled,
            })
            .collect(),
    ))
}

/// DELETE /api/mt/extensions/{id} —— 删除扩展(清本地目录 + 删 DB 行 + 发事件)。
pub async fn delete_extension(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, AppError> {
    // 查 name 以便清本地 skill 目录;查不到也继续走 DB 删(幂等)。
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find()
        .filter(ExtCol::Id.eq(id.clone()))
        .one(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if let Some(m) = m {
        if m.kind == "skill" {
            let _ = apply::remove_dir_safe(&apply::skills_dir(&state.codex_home), &m.name).await;
        }
    }

    store::delete_extension(&state.db, &id).await?;

    // 从本地状态文件移除该扩展条目(upload 时写入了 id→{name,hash}),
    // 否则 Task 8 同步循环对齐时 local_state 会残留已删扩展。幂等:id 不在 map 也安全。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.remove(&id);
    let _ = apply::save_local_state(&state.codex_home, &st).await;

    if let Some(bus) = &state.mt_event_bus {
        let _ = bus
            .publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}"))
            .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// base64 标准解码。
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

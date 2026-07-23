//! 集群扩展分发 —— skill 上传/plugin 上传/mcp 上传/列表/删除 REST API(Task 6 + plugin Task 5 + mcp Task 4)。
//!
//! 这是"单一安装入口":
//! - skill:用户上传 zip 压缩包(base64),后端解压落盘本节点 + 入库 + 发事件触发集群同步。
//! - plugin:后端 spawn `codex plugin add <name>@<market>` 装好,再指纹化 cache + 入库 +
//!   发事件(其他节点订阅 "extensions:changed" 后走 Task 7 下载 + Task 8 应用)。
//! - mcp:用户上传 `config_text`(MCP 段内容),后端合并进 config.toml `[mcp_servers.<name>]` +
//!   入库(config_form=config,无文件指纹)+ 发事件。
//!
//! 路由(挂载在 mt_protected,受 require_user_auth 保护):
//! - POST   /api/mt/extensions        上传 skill 压缩包 / 装 plugin / 合并 mcp 配置段
//! - GET    /api/mt/extensions        列出 enabled 扩展
//! - DELETE /api/mt/extensions/{id}   删除扩展(清本地目录/段 + 删 DB 行 + 发事件)

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
/// - skill 分支:`kind="skill"` + `zip_base64`(必填,zip 压缩包 base64;后端解压 +
///   自动剥单一顶层目录 + 防 zip-slip)。旧 `files` 字段已弃用(skill 分支不再读取),
///   保留仅为不破坏旧请求反序列化。
/// - plugin 分支:`kind="plugin"` + `marketplace`(可选,默认 openai-api-curated),
///   无 `zip_base64`(后端 spawn codex 装好后自己扫描 cache)。
/// - mcp 分支:`kind="mcp"` + `config_text`(必填,MCP 段内容原文,如 `command="node"\nargs=[...]`),
///   无 `zip_base64`(配置段直接合并进 config.toml)。
#[derive(Deserialize)]
pub struct UploadBody {
    pub kind: String,
    pub name: String,
    /// skill 旧字段(文件数组,已弃用,改用 zip_base64)。保留字段以免旧前端请求被 serde 拒,
    /// skill 分支不再读取。plugin/mcp 本就不填。
    #[serde(default)]
    pub files: Vec<UploadFile>,
    /// skill 的 zip 压缩包 base64(整包,后端解压 + 自动剥顶层目录)。
    /// 缺省让 plugin/mcp 请求可省略,skill 分支取值时校验非空。
    #[serde(default)]
    pub zip_base64: Option<String>,
    /// plugin 的市场名(skill 不用);缺省时取 "openai-api-curated"。
    #[serde(default)]
    pub marketplace: Option<String>,
    /// MCP 的配置段内容(段内键值,无段头);skill/plugin 不填。
    /// 缺省让 skill/plugin 请求可省略该字段,mcp 分支取值时校验非空。
    #[serde(default)]
    pub config_text: Option<String>,
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
/// - kind="skill"  → 走本函数剩余流程:校验 name 合法性(防目录穿越)→ base64 解码
///   `zip_base64`(校验整包字节上限)→ 解压到 skills/{name}/(`unzip_to_dest`:清旧同名
///   目录 + 剥单一顶层目录 + 防 zip-slip + zip bomb 安全阀)→ 算指纹(aggregate_hash)
///   → 入库 upsert_extension + add_holder(本节点)→ 更新本地状态文件 → 发
///   "extensions:changed" 事件。
pub async fn upload_extension(
    State(state): State<AppState>,
    crate::error::Json(body): crate::error::Json<UploadBody>,
) -> Result<Json<ExtResp>, AppError> {
    // plugin 分支:无 files,后端 spawn codex plugin add 装好后扫描 cache。
    if body.kind == "plugin" {
        return upload_plugin(state, body).await;
    }
    // mcp 分支:无 files,内联 config_text(段内容),走独立配置段合并 + 入库。
    if body.kind == "mcp" {
        return upload_mcp(state, body).await;
    }
    // 阶段 1 其余只支持 skill。
    if body.kind != "skill" {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "阶段1 仅支持 skill / plugin / mcp".into(),
            None,
        ));
    }
    // skill name 合法性校验:防 skills_dir.join(name) 穿越到 skills 根之外。
    //   与 safe_join_local 口径一致:非空、无 `..`、无反斜杠、无正斜杠段分隔。
    if body.name.is_empty()
        || body.name.contains("..")
        || body.name.contains('\\')
        || body.name.contains('/')
    {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "skill name 非法".into(),
            None,
        ));
    }
    // zip_base64 必填(skill 压缩包,替代旧的 files 数组)。
    let zip_b64 = match body.zip_base64.as_deref() {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return Err(AppError::business(
                ErrorCode::HttpBadRequest,
                StatusCode::BAD_REQUEST,
                "skill 需要 zip_base64".into(),
                None,
            ));
        }
    };
    // zip 原始字节上限:防过大上传(解压后的总量/文件数由 unzip_to_dest 内部再限)。
    const MAX_ZIP_BYTES: usize = 50 * 1024 * 1024; // 50 MB
    let zip_bytes = base64_decode(zip_b64).map_err(|e| {
        AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!("zip_base64 解码失败: {e}"),
            None,
        )
    })?;
    if zip_bytes.len() > MAX_ZIP_BYTES {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!(
                "zip 字节数 {} 超过上限 {}",
                zip_bytes.len(),
                MAX_ZIP_BYTES
            ),
            None,
        ));
    }

    // 1. 解压到正式 skills/{name}/(unzip_to_dest 内部:清旧同名目录 + 剥单一顶层 +
    //    防 zip-slip + zip bomb 安全阀,一步到位,无需临时目录中转)。
    let skills_root = apply::skills_dir(&state.codex_home);
    tokio::fs::create_dir_all(&skills_root)
        .await
        .map_err(|e| AppError::internal(format!("mkdir skills root: {e}")))?;
    let dest = skills_root.join(&body.name);
    let fps = apply::unzip_to_dest(&zip_bytes, &dest).await?;
    // 2. 聚合 hash(与遍历顺序无关)。
    let content_hash = fingerprint::aggregate_hash(&fps);

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
        // skill 以文件形式存储,无内联配置 → config_text 为 None(MCP 才填)。
        config_text: None,
        content_hash: content_hash.clone(),
        enabled: true,
        // skill 上传走此分支,marketplace/version 仅 plugin 有 → None。
        marketplace: None,
        version: None,
    };
    store::upsert_extension(&state.db, &rec, &fps).await?;
    store::add_holder(&state.db, &id, &state.node_id).await?;

    // 5. 更新本地状态文件(id → {name, hash, kind, market, version})：name+kind+market+version
    //    供副本删除分支定位目录(不查 PG),hash 供同步循环对齐。同名重传 id 复用,key 不变,值全量替换。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(
        id.clone(),
        apply::LocalExtEntry {
            name: body.name.clone(),
            hash: content_hash.clone(),
            kind: "skill".into(),
            market: None,
            version: None,
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
        // plugin 以文件形式存储,无内联配置 → config_text 为 None(MCP 才填)。
        config_text: None,
        content_hash: content_hash.clone(),
        enabled: true,
        marketplace: Some(market.clone()),
        version: Some(version.clone()),
    };
    store::upsert_extension(&state.db, &rec, &fps).await?;
    store::add_holder(&state.db, &id, &state.node_id).await?;

    // 6. 本地状态文件(id → {name, hash, kind, market, version})。
    //    kind/market/version 供副本删除分支构造 plugin 路径(不查 PG),hash 供同步循环对齐。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(
        id.clone(),
        apply::LocalExtEntry {
            name: body.name.clone(),
            hash: content_hash.clone(),
            kind: "plugin".into(),
            market: Some(market.clone()),
            version: Some(version.clone()),
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

/// POST /api/mt/extensions (kind="mcp") —— 合并 MCP 配置段 + 入库(无文件树)。
///
/// 流程:取 `config_text`(段内容,无段头)→ 包头 `[mcp_servers.<name>]` 解析校验为合法 toml
/// → sha256 算 `content_hash`(对段内容本身算,不含段头,与落盘/同步对齐)→
/// `enable_mcp_config` 合并进 config.toml → 入库 upsert_extension(files 空,config_text 填充)
/// → 更新本地状态文件 → 发 "extensions:changed" 事件。
///
/// 与 skill/plugin 不同:不入 `add_holder` —— MCP 无独立文件指纹(纯配置段),
/// 同步侧按 config_text 直接写段即可,不需要 holder 表的"本节点已落盘"登记。
async fn upload_mcp(state: AppState, body: UploadBody) -> Result<Json<ExtResp>, AppError> {
    // config_text 必填且非空(MCP 段内容,如 `command="node"\nargs=[...]`)。
    let content = body.config_text.clone().ok_or_else(|| {
        AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "MCP 需要 config_text".into(),
            None,
        )
    })?;
    if body.name.is_empty() || content.trim().is_empty() {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "name 和 config_text 不能为空".into(),
            None,
        ));
    }
    // 1. 校验 content 为合法 toml 段内容:包头 `[mcp_servers.<name>]` 后整体 parse,
    //    既校验段内语法,也校验 name 作 toml 段名的合法性(如含特殊字符在此暴露)。
    let wrapped = format!("[mcp_servers.{}]\n{}\n", body.name, content);
    wrapped.parse::<toml_edit::DocumentMut>().map_err(|e| {
        AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!("config_text 非法 toml: {e}"),
            None,
        )
    })?;
    // 2. content_hash = content(段内容,不含段头)的 sha256,hex 编码。
    //    与落盘/同步侧口径一致:同步时直接比对 config_text 的指纹,无需段头参与。
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let content_hash = hex::encode(h.finalize());

    // 3. 合并到本节点 config.toml 的 `[mcp_servers.<name>]` 段(逐 key clone)。
    apply::enable_mcp_config(&state.codex_home, &body.name, &content).await?;

    // 4. 入库(kind=mcp, content_form=config, config_text 填充, files 空)。
    //    同名重传复用现有 id(命中 → upsert 走 update;未命中 → new_id() 新建),
    //    避免 UNIQUE(kind,name) 冲突与旧行残留,local_state 亦按 id 写(key 不变)。
    let id = store::find_id_by_kind_name(&state.db, "mcp", &body.name)
        .await?
        .unwrap_or_else(new_id);
    let rec = store::ExtRecord {
        id: id.clone(),
        kind: "mcp".into(),
        name: body.name.clone(),
        content_form: "config".into(),
        // MCP 以内联配置段存储 → config_text 填充(skill/plugin 此处为 None)。
        config_text: Some(content.clone()),
        content_hash: content_hash.clone(),
        enabled: true,
        // MCP 无市场/版本概念 → None。
        marketplace: None,
        version: None,
    };
    store::upsert_extension(&state.db, &rec, &[]).await?; // files 空 Vec

    // 5. 本地状态文件(id → {name, hash, kind=mcp, market/version=None})。
    //    kind="mcp" 供删除分支按类型分发(走 disable_mcp_config);hash 供同步循环对齐。
    let mut st = apply::load_local_state(&state.codex_home).await;
    st.insert(
        id.clone(),
        apply::LocalExtEntry {
            name: body.name.clone(),
            hash: content_hash.clone(),
            kind: "mcp".into(),
            market: None,
            version: None,
        },
    );
    let _ = apply::save_local_state(&state.codex_home, &st).await;

    // 6. 发事件触发其他节点同步(best-effort,无 bus/订阅者时静默)。
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
    // 查 name/kind/marketplace 以便清本地目录(本节点先物理删 PG 行、再发事件,
    // 副本侧靠本地 state 清;本节点此时 PG 行还在,可直接读)。查不到也继续走 DB 删(幂等)。
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find()
        .filter(ExtCol::Id.eq(id.clone()))
        .one(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if let Some(m) = m {
        match m.kind.as_str() {
            "skill" => {
                let _ = apply::remove_dir_safe(&apply::skills_dir(&state.codex_home), &m.name).await;
            }
            "plugin" => {
                // plugin 删整个 plugins/cache/<market>/<name>/(含所有 version),
                // 再移除 config.toml 的启用段(段不存在视为成功,best-effort)。
                let market = m.marketplace.clone().unwrap_or_default();
                if !market.is_empty() {
                    let _ = apply::remove_dir_safe(
                        &apply::plugins_cache_dir(&state.codex_home).join(&market),
                        &m.name,
                    )
                    .await;
                    let _ = apply::disable_plugin_config(&state.codex_home, &m.name, &market).await;
                }
            }
            "mcp" => {
                // MCP 无文件树,仅移除 config.toml 的 `[mcp_servers.<name>]` 段
                // (段不存在视为成功,best-effort)。
                let _ = apply::disable_mcp_config(&state.codex_home, &m.name).await;
            }
            _ => {}
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

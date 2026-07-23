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
use std::process::Stdio;
use std::time::Duration;

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
    // skill name 合法性校验(防 skills_dir.join(name) 穿越到 skills 根之外)。
    validate_ext_name(&body.name)?;
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

    // 1. 先解压到临时目录校验合法性,通过后才落盘正式目录(事务性:非法包不污染 skills/)。
    //    临时目录由 TempDir 在函数返回时 drop 自动清理;校验失败 `?` 上抛 → 既不入库,
    //    也不在 skills/ 下留残渣。
    let skills_root = apply::skills_dir(&state.codex_home);
    tokio::fs::create_dir_all(&skills_root)
        .await
        .map_err(|e| AppError::internal(format!("mkdir skills root: {e}")))?;
    let dest = skills_root.join(&body.name);
    // 1a. 临时解压 + skill 合法性校验(必须有非空 SKILL.md)。
    //     unzip_to_dest 内部:剥单一顶层 + 防 zip-slip + zip bomb 阀。
    let tmp = tempfile::tempdir()
        .map_err(|e| AppError::internal(format!("tempdir: {e}")))?;
    apply::unzip_to_dest(&zip_bytes, tmp.path()).await?;
    apply::validate_skill(tmp.path()).await?;
    // 1b. 校验通过:正式落盘 skills/{name}/(再解压一次,unzip_to_dest 开头清旧同名目录,
    //     保证全量替换),返回落盘后指纹供入库。tmp 留待 scope 结束 drop(非热路径)。
    let fps = apply::unzip_to_dest(&zip_bytes, &dest).await?;
    // 1c. 扩展总字节 / 单文件字节上限校验(config max_extension_bytes / max_file_bytes)。
    //     超限清已落盘的 skills/{name}/ + 返 400(不入库,不留半成品)。
    if let Err(e) = check_extension_size_limits(&fps, &state) {
        let _ = apply::remove_dir_safe(&skills_root, &body.name).await;
        return Err(e);
    }
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
    apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| {
        m.insert(
            id.clone(),
            apply::LocalExtEntry {
                name: body.name.clone(),
                hash: content_hash.clone(),
                kind: "skill".into(),
                market: None,
                version: None,
            },
        );
    })
    .await?;

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
    // plugin 分发开关:config [extensions] plugin_enabled=false 时拒(plugin 含可执行
    // 产物,默认关闭,需显式开启)。早于其它处理返回。
    if !state.cfg_extensions_plugin_enabled {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "plugin 分发未启用(config [extensions] plugin_enabled=false)".into(),
            None,
        ));
    }
    // name 合法性校验(与 skill 同口径:拒空/..\/\) + 额外拒 @(破坏 name@market 解析)。
    validate_ext_name(&body.name)?;
    if body.name.contains('@') {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "plugin name 非法(含 @)".into(),
            None,
        ));
    }
    // market:缺省取 openai-api-curated;非空且不含穿越字符 / @ 才采纳,否则回退默认。
    // (非法 market 回退默认而非报错——安全方向,避免用非法 market 拼路径;plugin_dest 段校验兜底。)
    let market = body
        .marketplace
        .as_deref()
        .filter(|s| {
            !s.is_empty() && !s.contains('@') && !s.contains("..") && !s.contains('\\') && !s.contains('/')
        })
        .map(|s| s.to_string())
        .unwrap_or_else(|| "openai-api-curated".to_string());

    // 1. spawn codex plugin add <name>@<market>,装到本节点 CODEX_HOME。
    //    超时保护:codex plugin add 联网下载,网络异常/registry 挂起会长期不退出 →
    //    timeout 包 wait_with_output → 超时返 Elapsed,内部 future(含已 move 的 child)被 drop。
    //    配 kill_on_drop(true):child drop 时自动 kill 子进程,防孤儿(正常完成时进程已退出,kill 无害)。
    //    stdout/stderr piped 以捕获错误输出。
    const PLUGIN_ADD_TIMEOUT_SECS: u64 = 300;
    let codex_bin = state.codex.codex_bin();
    let mut cmd = crate::codex::process::build_codex_command(codex_bin);
    cmd.arg("plugin")
        .arg("add")
        .arg(format!("{}@{}", body.name, market))
        .env("CODEX_HOME", &state.codex_home)
        .current_dir(&state.codex_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| AppError::internal(format!("spawn codex plugin add: {e}")))?;
    let out = match tokio::time::timeout(
        Duration::from_secs(PLUGIN_ADD_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    {
        Ok(r) => r.map_err(|e| AppError::internal(format!("codex plugin add wait: {e}")))?,
        Err(_) => {
            // 超时:wait_with_output future 已 drop → child drop → kill_on_drop 已杀子进程。
            // 清理可能已写入的 cache/启用段 + 返 504。
            let _ = apply::remove_dir_safe(
                &apply::plugins_cache_dir(&state.codex_home).join(&market),
                &body.name,
            )
            .await;
            let _ = apply::disable_plugin_config(&state.codex_home, &body.name, &market).await;
            return Err(AppError::status(504));
        }
    };
    if !out.status.success() {
        // 失败清理:codex plugin add 可能已把部分文件写进 cache/<market>/<name>/ 但最终退出非 0,
        // 清掉残留目录 + 移除可能已写的启用段,确保不留半装 plugin(幂等:目录/段不存在也安全)。
        let _ = apply::remove_dir_safe(
            &apply::plugins_cache_dir(&state.codex_home).join(&market),
            &body.name,
        )
        .await;
        let _ = apply::disable_plugin_config(&state.codex_home, &body.name, &market).await;
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
    // 扩展总字节 / 单文件字节上限校验;超限清理 cache + 启用段 + 400(不入库不留半成品)。
    if let Err(e) = check_extension_size_limits(&fps, &state) {
        let _ = apply::remove_dir_safe(
            &apply::plugins_cache_dir(&state.codex_home).join(&market),
            &body.name,
        )
        .await;
        let _ = apply::disable_plugin_config(&state.codex_home, &body.name, &market).await;
        return Err(e);
    }
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
    apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| {
        m.insert(
            id.clone(),
            apply::LocalExtEntry {
                name: body.name.clone(),
                hash: content_hash.clone(),
                kind: "plugin".into(),
                market: Some(market.clone()),
                version: Some(version.clone()),
            },
        );
    })
    .await?;

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
    let _ = apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| {
        m.insert(
            id.clone(),
            apply::LocalExtEntry {
                name: body.name.clone(),
                hash: content_hash.clone(),
                kind: "mcp".into(),
                market: None,
                version: None,
            },
        );
    })
    .await;

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
    let _ = apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| {
        m.remove(&id);
    })
    .await;

    if let Some(bus) = &state.mt_event_bus {
        let _ = bus
            .publish("extensions:changed", &format!("{{\"id\":\"{id}\"}}"))
            .await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// 校验扩展 name 段合法性:非空,不含 `..` / `\` / `/`(防目录穿越与段分隔)。
/// skill 目录名、plugin 名共用(与 apply::safe_join_local 口径一致)。
fn validate_ext_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() || name.contains("..") || name.contains('\\') || name.contains('/') {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            "扩展 name 非法(空或含 .. \\ /)".into(),
            None,
        ));
    }
    Ok(())
}

/// 校验扩展文件指纹的总字节 / 单文件字节是否超 config 上限(max_extension_bytes / max_file_bytes)。
/// 超限返回 400 业务错误;调用方负责清理已落盘产物(本函数只判不删)。
/// MCP 无文件,不调用本函数。
fn check_extension_size_limits(
    fps: &[fingerprint::FileFingerprint],
    state: &AppState,
) -> Result<(), AppError> {
    let total: u64 = fps.iter().map(|f| f.size as u64).sum();
    if total > state.cfg_extensions_max_extension_bytes {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!(
                "扩展总字节 {total} 超过上限 {}",
                state.cfg_extensions_max_extension_bytes
            ),
            None,
        ));
    }
    if let Some(f) = fps
        .iter()
        .find(|f| (f.size as u64) > state.cfg_extensions_max_file_bytes)
    {
        return Err(AppError::business(
            ErrorCode::HttpBadRequest,
            StatusCode::BAD_REQUEST,
            format!(
                "单文件 {} 字节 {} 超过上限 {}",
                f.rel_path, f.size, state.cfg_extensions_max_file_bytes
            ),
            None,
        ));
    }
    Ok(())
}

/// base64 标准解码。
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::validate_ext_name;
    use crate::error::AppError;

    /// validate_ext_name:合法 name 通过(skill 目录名 / plugin 名共用,与 safe_join_local 同口径)。
    #[test]
    fn validate_ext_name_accepts_legal() {
        assert!(validate_ext_name("ui-skill").is_ok());
        assert!(validate_ext_name("linear").is_ok());
        assert!(validate_ext_name("a.b.c").is_ok()); // 含点合法(段名 quoting 处理)
    }

    /// validate_ext_name:空 / `..` / `\` / `/` 一律拒(防目录穿越与段分隔)。
    #[test]
    fn validate_ext_name_rejects_traversal_and_empty() {
        for bad in ["", "..", "a/b", "a\\b", "../x", "a..b", "/abs"] {
            let err = validate_ext_name(bad).unwrap_err();
            assert!(
                matches!(err, AppError::Business { .. }),
                "name={bad:?} 应返 400 业务错误"
            );
        }
    }
}

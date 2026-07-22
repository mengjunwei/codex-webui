//! 集群扩展同步循环:把本地落盘的 skills / plugins / mcp 配置段对齐到 PG 清单。
//!
//! 每节点跑一个周期 task + 一个 "extensions:changed" 事件订阅 task。
//! - 缺(PG 有本地无/hash 变):
//!   - skill/plugin:从任一 alive holder 逐文件下载 → 落盘 → **基于落盘字节重算指纹**
//!     校验整体 hash → 更新本地状态 + add_holder(自己)完成扩散。plugin 额外写
//!     `[plugins."<name>@<market>"] enabled=true` 启用段。
//!   - mcp:无文件,直接用 PG `config_text` 写 `[mcp_servers.<name>]` 段 → 更新本地状态。
//!     不查 holder / 不下载 / 不 add_holder(MCP 无文件指纹,holder 对 MCP 无意义),
//!     在 `sync_one_extension` 顶部短路(早于 holder_candidates,否则空候选 Err 卡死)。
//! - 多(本地有 PG 无):按本地 state 的 kind/market/version/name 删目录 + 清启用段 + 清本地状态。
//! - 变(hash 不同):等同缺,重下覆盖(skill/plugin) / 重写段(mcp)。
//!
//! `run_round` 中单个扩展失败(get_files / 下载 / 写盘等异常)不影响其他扩展:包进
//! `sync_one_extension` 独立 try,失败仅 warn 跳过,下轮重试;已成功登记的 local_state
//! 在循环结束后统一落盘。整轮致命错误(如 DB 断)由调用方记日志,下一轮或下一次事件重试。

use crate::error::AppError;
use crate::services::extensions::{apply, fingerprint, store};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};

/// 单轮同步:把本地扩展对齐到 PG 清单。
///
/// 步骤:
/// 1. 读 PG `list_enabled`(期望集合)+ 本地 `.cluster-extensions.json`(实际集合)。
/// 2. 本地有、PG 无 → 用本地 state 里登记的 kind/market/version/name 构造路径删目录
///    (skill:`skills/{name}/`;plugin:`plugins/cache/{market}/{name}/` + 移除启用段)+ 清本地状态
///    (不查 PG,避免发起节点物理删行后副本查不到 name → 孤儿目录)。
/// 3. PG 有、本地无/hash 变 → 调 `sync_one_extension` 独立 try;单扩展失败仅 warn 跳过,
///    不影响其他扩展(下轮重试)。
/// 4. 落盘整份本地状态(无论 step 3 是否全部成功,已成功的必须落盘登记)。
pub async fn run_round(state: &AppState) -> Result<(), AppError> {
    let desired = store::list_enabled(&state.db).await?;
    let mut local = apply::load_local_state(&state.codex_home).await;

    let desired_ids: HashSet<&String> = desired.iter().map(|r| &r.id).collect();
    // 删除:本地有、PG 无(扩展被禁用或删除)。
    let stale: Vec<String> = local
        .keys()
        .filter(|k| !desired_ids.contains(*k))
        .cloned()
        .collect();
    for id in &stale {
        // 删除目录用**本地状态里存的 kind/market/version/name**,不查 PG。
        // 发起节点 delete_extension 先物理删 PG 行、再发事件,副本收到事件时 PG 已无该行,
        // 若靠 name_of(id) 查 PG 会返回 None → 目录成孤儿。本地 state 在 upload/sync 时已登记完整路径信息。
        // remove 同时取出条目并清理 local_state,幂等(id 不在 map 时返回 None)。
        if let Some(e) = local.remove(id) {
            let clean = cleanup_local_extension(&state.codex_home, &e).await;
            if let Err(err) = clean {
                tracing::warn!(ext = %id, error = %err, "删除孤儿目录失败,跳过");
            }
        }
    }

    // 新增/更新:每个扩展独立 try,单扩展失败不影响其他。
    for rec in &desired {
        // 同步 skill / plugin / mcp;其他未知 kind 跳过。MCP 在 sync_one_extension 顶部短路处理。
        if rec.kind != "skill" && rec.kind != "plugin" && rec.kind != "mcp" {
            continue;
        }
        // 比对 hash:本地条目 hash 与 PG content_hash 一致则跳过,否则需更新(新增/变更)。
        let need = match local.get(&rec.id) {
            Some(e) if e.hash == rec.content_hash => false,
            _ => true,
        };
        if !need {
            continue;
        }
        // 单扩展失败:warn 跳过,下轮/事件重试;不让一个扩展的异常中断整轮。
        if let Err(e) = sync_one_extension(state, rec, &mut local).await {
            tracing::warn!(ext = %rec.id, error = %e, "扩展同步失败,跳过(下轮重试)");
            continue;
        }
    }
    // 循环结束后统一落盘:已成功登记的扩展必须持久化,否则下轮会重复下载。
    apply::save_local_state(&state.codex_home, &local).await?;
    Ok(())
}

/// 按本地条目的 kind 清理落盘产物(删目录 + 移除 plugin 启用段)。
///
/// 用于 stale 删除分支(本地有、PG 无)及 delete_extension handler 失败回退路径。
/// **不查 PG**——PG 行可能已被发起节点删掉,完全依赖本地 state 的 kind/market/version/name。
///
/// - skill:删 `skills/{name}/`。
/// - plugin:删 `plugins/cache/{market}/{name}/`(整个 name 目录,含所有 version)
///   + `disable_plugin_config`(移除 `[plugins."{name}@{market}"]` 段,best-effort)。
/// - mcp:无目录,仅 `disable_mcp_config`(移除 `[mcp_servers.{name}]` 段,best-effort)。
/// - 未知 kind / market 缺失:仅尝试按 name 删 skills/(兼容旧 state)。
async fn cleanup_local_extension(
    codex_home: &std::path::Path,
    e: &apply::LocalExtEntry,
) -> Result<(), AppError> {
    match e.kind.as_str() {
        "plugin" => {
            let market = e.market.clone().unwrap_or_default();
            if market.is_empty() {
                return Ok(()); // 无 market 无法定位 plugin 目录,放弃(避免误删 skills)
            }
            let res = apply::remove_dir_safe(
                &apply::plugins_cache_dir(codex_home).join(&market),
                &e.name,
            )
            .await;
            // 启用段移除 best-effort:段不存在视为成功,失败不阻断目录删除主流程。
            let _ = apply::disable_plugin_config(codex_home, &e.name, &market).await;
            res
        }
        "mcp" => {
            // MCP 无目录可删,仅移除 config.toml `[mcp_servers.<name>]` 段
            // (best-effort,段不存在视为成功)。返回 Ok 与其他分支类型对齐。
            let _ = apply::disable_mcp_config(codex_home, &e.name).await;
            Ok(())
        }
        // skill 及未知 kind(含旧 state 无 kind 字段)默认按 name 删 skills/。
        _ => apply::remove_dir_safe(&apply::skills_dir(codex_home), &e.name).await,
    }
}

/// bootstrap:启动时全量对齐一次(等同 run_round,语义别名)。
pub async fn bootstrap(state: &AppState) -> Result<(), AppError> {
    run_round(state).await
}

/// 同步单个扩展。
///
/// **MCP 短路**:`rec.kind == "mcp"` 时在 holder 检查**之前**直接返回 —— MCP 无文件,
/// 直接用 PG `config_text` 写 `[mcp_servers.<name>]` 段 + 登记 local_state(kind="mcp"),
/// 不查 holder / 不下载 / 不 add_holder(holder 对 MCP 无意义)。必须短路在 `holder_candidates`
/// 之前,否则 MCP 无 holder 行 → 空候选 Err → 永远到不了 kind 分支。
///
/// skill/plugin 走文件流程:查候选 holder → 拉文件清单 → 清旧目录 → 逐文件从 holder 下载落盘 →
/// **基于落盘实际字节 scan_dir 重算指纹** → aggregate_hash 与 PG content_hash 比对:
/// - 匹配 → plugin 额外写启用段 → 登记 local_state(含 kind/market/version) + add_holder(自己)扩散。
/// - 不匹配 → 清半成品目录 + 结构化 warn(ext/expected/got) + 不登记、不 add_holder,
///   返回 Ok(())(本轮已处理,local 未更新 → 下轮自然重试;不走 Err 避免与 run_round
///   外层 warn 重复打日志)。
///
/// 落盘根目录按 `rec.kind` 分发:
/// - skill:`skills/{name}/`,清旧 `remove_dir_safe(skills_dir, name)`。
/// - plugin:`plugins/cache/{market}/{name}/{version}/`(段校验在 `plugin_dest`),
///   清旧删整个 `plugins/cache/{market}/{name}/`(含所有 version),root=`plugins/cache/{market}`,
///   name=plugin 名。
///
/// 下载 / 写盘 / DB 等异常返回 Err,由 run_round 外层 warn 记录后跳过。
async fn sync_one_extension(
    state: &AppState,
    rec: &store::ExtRecord,
    local: &mut HashMap<String, apply::LocalExtEntry>,
) -> Result<(), AppError> {
    // MCP 短路:必须在 holder_candidates **之前** —— MCP 无文件 / 无 holder 行,
    // 若走到下面的 holder 空候选检查会直接 Err,永远到不了 kind 分支。
    // config_text 直接从 PG 读(PG 权威),完整性由 PG 保证,不重算 hash。
    if rec.kind == "mcp" {
        let content = rec.config_text.clone().unwrap_or_default();
        apply::enable_mcp_config(&state.codex_home, &rec.name, &content).await?;
        local.insert(
            rec.id.clone(),
            apply::LocalExtEntry {
                name: rec.name.clone(),
                hash: rec.content_hash.clone(),
                kind: "mcp".into(),
                market: None,
                version: None,
            },
        );
        // MCP 无文件,不调 add_holder(holder 对 MCP 无意义)。
        return Ok(());
    }

    // 候选 holder 列表:本扩展查一次(list_holders ∩ alive_nodes 排除自己),供本轮所有
    // 文件复用,避免每文件重复查 DB(原 download_from_holder 每文件查一次)。
    let holders = holder_candidates(state, &rec.id).await?;
    if holders.is_empty() {
        return Err(AppError::internal(format!(
            "无可用 alive holder 下载扩展 name={} (ext_id={})",
            rec.name, rec.id
        )));
    }

    // 拉文件清单(仅用于知道有哪些 rel_path 要下载;hash 校验改用落盘后 scan_dir 重算,
    // 不再用这份 PG 指纹算 aggregate_hash —— 它与 rec.content_hash 同源,比对恒真)。
    let files = store::get_files(&state.db, &rec.id).await?;

    // 落盘目标 dest + 清旧 (root, name) 按 kind 分发。
    // 注意 plugin 的清旧要删 cache/<market>/<name>/ 整个 name 目录(含所有 version),
    // 而非只删 version 子目录 —— 故 root=cache/<market>, name=plugin 名。
    let (dest, clean_root, clean_name): (std::path::PathBuf, std::path::PathBuf, String) =
        match rec.kind.as_str() {
            "skill" => {
                let root = apply::skills_dir(&state.codex_home);
                (root.join(&rec.name), root, rec.name.clone())
            }
            "plugin" => {
                let market = rec.marketplace.clone().unwrap_or_default();
                let version = rec.version.clone().unwrap_or_default();
                // plugin_dest 带段校验(market/name/version 拒空 / 绝对 / `..` / 反斜杠)。
                let dest = apply::plugin_dest(&state.codex_home, &market, &rec.name, &version)?;
                let clean_root = apply::plugins_cache_dir(&state.codex_home).join(&market);
                (dest, clean_root, rec.name.clone())
            }
            _ => return Ok(()), // 真正的未知 kind 跳过(MCP 已在函数顶部短路,不会到此)
        };

    // 清旧目录(含上次失败残留的半成品)→ 建空目录。
    apply::remove_dir_safe(&clean_root, &clean_name).await?;
    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| AppError::internal(format!("mkdir {}: {e}", dest.display())))?;
    // 逐文件从候选 holder 下载落盘。
    for f in &files {
        let bytes = download_from_holder(state, &holders, &rec.id, &f.rel_path).await?;
        apply::write_file_safe(&dest, &f.rel_path, &bytes).await?;
    }
    // 基于落盘实际字节重算指纹,再 aggregate_hash 校验:防传输中字节损坏但 HTTP 层未检出
    // 时,落盘内容错误却能通过校验(原实现用 PG 清单算 hash,与 content_hash 同源 → 恒真)。
    let landed = fingerprint::scan_dir(&dest).await?;
    let got = fingerprint::aggregate_hash(&landed);
    if got == rec.content_hash {
        // plugin 落盘成功后额外写启用段 [plugins."<name>@<market>"] enabled=true。
        if rec.kind == "plugin" {
            let market = rec.marketplace.clone().unwrap_or_default();
            apply::enable_plugin_config(&state.codex_home, &rec.name, &market).await?;
        }
        // 登记 {name, hash, kind, market, version}:name+kind+market+version 供后续删除分支
        // 定位目录(不查 PG),hash 供下轮对齐。
        local.insert(
            rec.id.clone(),
            apply::LocalExtEntry {
                name: rec.name.clone(),
                hash: rec.content_hash.clone(),
                kind: rec.kind.clone(),
                market: rec.marketplace.clone(),
                version: rec.version.clone(),
            },
        );
        // 扩散:自己也成 holder,后续其他新节点可从本节点下载。
        store::add_holder(&state.db, &rec.id, &state.node_id).await?;
        Ok(())
    } else {
        // hash 不匹配:清半成品目录(避免下次 scan_dir 误读残留 / 用户看到坏文件);
        // 不登记、不 add_holder,返回 Ok(()) —— local 未更新,下轮 need=true 自然重试。
        let _ = apply::remove_dir_safe(&clean_root, &clean_name).await;
        tracing::warn!(
            ext = %rec.id,
            expected = %rec.content_hash,
            got = %got,
            "扩展落盘 hash 不匹配,清半成品,本轮跳过(下轮重试)"
        );
        Ok(())
    }
}

/// 取某扩展的候选 holder 列表:`list_holders(ext_id)` ∩ 集群 alive 节点,排除自己。
/// 结果顺序遵循 alive_nodes(与原实现一致)。每扩展查一次,供本轮所有文件复用。
async fn holder_candidates(state: &AppState, ext_id: &str) -> Result<Vec<String>, AppError> {
    let holders = store::list_holders(&state.db, ext_id).await?;
    let alive = state.cluster.alive_nodes().await;
    let me = &state.node_id;
    Ok(alive.into_iter().filter(|n| n != me && holders.contains(n)).collect())
}

/// 从候选 holder 列表中逐个尝试下载单个文件;全失败则报错(本轮跳过该扩展,下一轮/事件重试)。
///
/// `holders` 由调用方(`sync_one_extension`)每扩展查一次传入,避免每文件重复查 DB
/// 及 alive_nodes。逐个 `node_rpc_addr` → `ext_fetch`,首个成功返回。
async fn download_from_holder(
    state: &AppState,
    holders: &[String],
    ext_id: &str,
    rel_path: &str,
) -> Result<Vec<u8>, AppError> {
    for node_id in holders {
        if let Some(rpc_base) = state.cluster.node_rpc_addr(node_id).await {
            match state.worker_rpc.ext_fetch(&rpc_base, ext_id, rel_path).await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => tracing::warn!(node = %node_id, error = %e, "ext_fetch 失败,试下一个 holder"),
            }
        }
    }
    Err(AppError::internal(format!(
        "无可用 holder 下载 {ext_id}/{rel_path}"
    )))
}

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
//! `sync_one_extension` 独立 try,失败仅 warn 跳过,下轮重试;local_state 每次成功即时
//! 受锁登记(`with_local_state` load→改→save),save 失败也 warn 跳过(与 sync 主体粒度一致)。
//! 整轮致命错误(如 DB 断)由调用方记日志,下一轮或下一次事件重试。

use crate::error::AppError;
use crate::services::extensions::{apply, fingerprint, store};
use crate::state::AppState;
use std::collections::HashSet;

/// 单轮同步:把本地扩展对齐到 PG 清单。
///
/// 两阶段 + 精确锁(防 local_state 并发丢失更新):
/// 1. **阶段 1(持短锁 ~ms)**:load 全量 local → 算 stale(本地有 PG 无)与 need(hash 变/新增)。
///    持锁仅覆盖这一次 load,不阻塞同步主体。
/// 2. **阶段 2(不持锁)**:
///    - stale:每个用 `with_local_state` 受锁 remove(防并发覆盖)→ 不持锁做目录 cleanup(秒级)。
///      cleanup 用阶段 1 拿到的 entry(kind/market/version/name,不查 PG,避免发起节点删行后副本孤儿)。
///    - need:每个不持锁 `sync_one_extension`(下载/落盘/写 config,秒级)→ 成功后 `with_local_state`
///      受锁 insert。单扩展失败 warn 跳过(下轮重试)。
///
/// 每次修改即时 save(with_local_state 内部 load→改→save),不再结尾全量 save。
/// upload/delete handler 也走 `with_local_state`,与本轮的短锁互斥 → 不丢更新。
pub async fn run_round(state: &AppState) -> Result<(), AppError> {
    let desired = store::list_enabled(&state.db).await?;

    // 阶段 1(持短锁):load 全量 local,算 stale + need。
    let (stale, need): (Vec<(String, apply::LocalExtEntry)>, Vec<store::ExtRecord>) = {
        let _g = state.ext_state_lock.lock().await;
        let local = apply::load_local_state(&state.codex_home).await;
        let desired_ids: HashSet<&String> = desired.iter().map(|r| &r.id).collect();
        let stale = local
            .iter()
            .filter(|(k, _)| !desired_ids.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let need = desired
            .iter()
            .filter(|rec| rec.kind == "skill" || rec.kind == "plugin" || rec.kind == "mcp")
            .filter(|rec| match local.get(&rec.id) {
                Some(e) if e.hash == rec.content_hash => false,
                _ => true,
            })
            .cloned()
            .collect();
        (stale, need)
    };

    // 阶段 2a:stale 清理。受锁 remove + 不持锁 cleanup。
    for (id, entry) in &stale {
        // local_state remove 受锁;save 失败(磁盘满/权限等)warn 跳过该条 + 跳过本次 cleanup
        // (local 未删则盘也不删,保持一致;下轮该 id 仍 stale → 重试)。粒度与 sync 主体一致。
        if apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| m.remove(id))
            .await
            .is_err()
        {
            tracing::warn!(ext = %id, "local_state remove 失败,跳过本次清理(下轮重试)");
            continue;
        }
        if let Err(err) = cleanup_local_extension(&state.codex_home, entry).await {
            tracing::warn!(ext = %id, error = %err, "删除孤儿目录失败,跳过");
        }
    }

    // 阶段 2b:need 同步。不持锁 sync + 成功后受锁 insert。
    for rec in &need {
        match sync_one_extension(state, rec).await {
            Ok(Some(entry)) => {
                let id = rec.id.clone();
                // 已成功落盘 + add_holder,local insert 的 save 失败不连累后续扩展:
                // warn 跳过,下轮该扩展 need=true(local 无/旧)→ 重试同步。粒度与 sync 主体一致。
                if let Err(e) =
                    apply::with_local_state(&state.ext_state_lock, &state.codex_home, |m| {
                        m.insert(id, entry);
                    })
                    .await
                {
                    tracing::warn!(ext = %rec.id, error = %e, "local_state insert 失败,跳过(下轮重试)");
                }
            }
            Ok(None) => {} // hash 不匹配等,不登记,下轮重试
            Err(e) => tracing::warn!(ext = %rec.id, error = %e, "扩展同步失败,跳过(下轮重试)"),
        }
    }
    Ok(())
}

/// 按本地条目的 kind 清理落盘产物(删目录 + 移除 plugin 启用段)。
///
/// 用于 stale 删除分支(本地有、PG 无)及 delete_extension handler 失败回退路径。
/// **不查 PG**——PG 行可能已被发起节点删掉,完全依赖本地 state 的 kind/market/version/name。
///
/// **TOCTOU 边界**:`name` 取自 run_round 阶段 1 快照,删盘按 name 匹配。极端竞态下
/// (PG 删除产生 stale 与同名扩展重传紧邻发生),可能删到刚重传的同名目录。该竞态预先存在
/// (原实现同样有),M2 仅修了 local_state 侧(2a 的 with_local_state 重 load,新 id 不被误删),
/// 磁盘侧按 name 删无法区分新旧。可接受:同名扩展罕见,且重传会经 need 分支再次同步补回。
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
) -> Result<Option<apply::LocalExtEntry>, AppError> {
    // MCP 短路:必须在 holder_candidates **之前** —— MCP 无文件 / 无 holder 行,
    // 若走到下面的 holder 空候选检查会直接 Err,永远到不了 kind 分支。
    // config_text 直接从 PG 读(PG 权威),完整性由 PG 保证,不重算 hash。
    if rec.kind == "mcp" {
        let content = rec.config_text.clone().unwrap_or_default();
        apply::enable_mcp_config(&state.codex_home, &rec.name, &content).await?;
        // MCP 无文件:写段后返回 entry,由 run_round 用 with_local_state 受锁登记(不 add_holder)。
        return Ok(Some(apply::LocalExtEntry {
            name: rec.name.clone(),
            hash: rec.content_hash.clone(),
            kind: "mcp".into(),
            market: None,
            version: None,
        }));
    }

    // 候选 holder 列表:本扩展查一次(list_holders ∩ alive_nodes 排除自己),供本轮所有
    // 文件复用,避免每文件重复查 DB(原 download_from_holder 每文件查一次)。
    let holders = holder_candidates(state, &rec.id).await?;
    if holders.is_empty() {
        // 单点风险观测(M5):该扩展在所有 alive 节点上都没有(可能仅存在于已下线节点),
        // 本节点无法获取。记 metrics + 结构化 warn 便于运维发现扩散不足/单点失效。
        // 逻辑不变(仍 Err → run_round 外层 warn 跳过,下轮重试),仅增强可观测性。
        metrics::counter!("ext_no_alive_holder_total").increment(1);
        tracing::warn!(
            ext_id = %rec.id,
            name = %rec.name,
            kind = %rec.kind,
            "扩展无可用 alive holder(单点风险:可能仅在已下线节点,本节点无法获取)"
        );
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
            _ => return Ok(None), // 真正的未知 kind 跳过(MCP 已在函数顶部短路,不会到此)
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
        // 扩散:自己也成 holder(DB 写,不涉及 local_state 锁)。add_holder 是 best-effort
        // (I7 后真实错误 warn 但返 Ok,不阻断)。
        store::add_holder(&state.db, &rec.id, &state.node_id).await?;
        // 返回 entry:由 run_round 用 with_local_state 受锁登记(name+kind+market+version 供
        // 后续删除分支定位目录,hash 供下轮对齐)。不在此直接写 local_state(避免无锁写)。
        Ok(Some(apply::LocalExtEntry {
            name: rec.name.clone(),
            hash: rec.content_hash.clone(),
            kind: rec.kind.clone(),
            market: rec.marketplace.clone(),
            version: rec.version.clone(),
        }))
    } else {
        // hash 不匹配:清半成品目录(避免下次 scan_dir 误读残留 / 用户看到坏文件);
        // 返回 None(不登记、不 add_holder),下轮 need=true 自然重试。
        let _ = apply::remove_dir_safe(&clean_root, &clean_name).await;
        tracing::warn!(
            ext = %rec.id,
            expected = %rec.content_hash,
            got = %got,
            "扩展落盘 hash 不匹配,清半成品,本轮跳过(下轮重试)"
        );
        Ok(None)
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

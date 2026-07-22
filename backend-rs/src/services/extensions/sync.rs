//! 集群扩展同步循环:把本地落盘的 skills 对齐到 PG 清单。
//!
//! 每节点跑一个周期 task + 一个 "extensions:changed" 事件订阅 task。
//! - 缺(PG 有本地无/hash 变):从任一 alive holder 逐文件下载 → 落盘 → **基于落盘字节
//!   重算指纹**校验整体 hash → 更新本地状态 + add_holder(自己) 完成扩散。
//! - 多(本地有 PG 无):删目录 + 清本地状态。
//! - 变(hash 不同):等同缺,重下覆盖。
//!
//! `run_round` 中单个扩展失败(get_files / 下载 / 写盘等异常)不影响其他扩展:包进
//! `sync_one_extension` 独立 try,失败仅 warn 跳过,下轮重试;已成功登记的 local_state
//! 在循环结束后统一落盘。整轮致命错误(如 DB 断)由调用方记日志,下一轮或下一次事件重试。

use crate::error::AppError;
use crate::services::extensions::{apply, fingerprint, store};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 单轮同步:把本地扩展对齐到 PG 清单。
///
/// 步骤:
/// 1. 读 PG `list_enabled`(期望集合)+ 本地 `.cluster-extensions.json`(实际集合)。
/// 2. 本地有、PG 无 → 删目录 + 清本地状态(name 查不到则仅清 state,孤儿目录留待后续)。
/// 3. PG 有、本地无/hash 变 → 调 `sync_one_extension` 独立 try;单扩展失败仅 warn 跳过,
///    不影响其他扩展(下轮重试)。
/// 4. 落盘整份本地状态(无论 step 3 是否全部成功,已成功的必须落盘登记)。
pub async fn run_round(state: &AppState) -> Result<(), AppError> {
    let desired = store::list_enabled(&state.db).await?;
    let mut local = apply::load_local_state(&state.codex_home).await;
    let skills_root = apply::skills_dir(&state.codex_home);

    let desired_ids: HashSet<&String> = desired.iter().map(|r| &r.id).collect();
    // 删除:本地有、PG 无(扩展被禁用或删除)。
    let stale: Vec<String> = local
        .keys()
        .filter(|k| !desired_ids.contains(*k))
        .cloned()
        .collect();
    for id in &stale {
        // name 从 PG 查(扩展可能仅 enabled=false,行仍在 → name 可查到)。
        // name 查询本身失败或行已物理删除时,仅清本地 state,不阻断其他 stale 的处理。
        match name_of(state, id).await {
            Ok(Some(name)) => {
                if let Err(e) = apply::remove_dir_safe(&skills_root, &name).await {
                    tracing::warn!(ext = %id, error = %e, "删除孤儿目录失败,跳过");
                }
            }
            Ok(None) => tracing::warn!(
                ext = %id,
                "扩展 name 查不到(已物理删除?),孤儿目录暂留,仅清本地状态"
            ),
            Err(e) => tracing::warn!(ext = %id, error = %e, "查 name 失败,跳过删除"),
        }
        local.remove(id);
    }

    // 新增/更新:每个扩展独立 try,单扩展失败不影响其他。
    for rec in &desired {
        if rec.kind != "skill" {
            continue; // 阶段1 仅同步 skill
        }
        let need = match local.get(&rec.id) {
            Some(h) if h == &rec.content_hash => false,
            _ => true,
        };
        if !need {
            continue;
        }
        // 单扩展失败:warn 跳过,下轮/事件重试;不让一个扩展的异常中断整轮。
        if let Err(e) = sync_one_extension(state, rec, &skills_root, &mut local).await {
            tracing::warn!(ext = %rec.id, error = %e, "扩展同步失败,跳过(下轮重试)");
            continue;
        }
    }
    // 循环结束后统一落盘:已成功登记的扩展必须持久化,否则下轮会重复下载。
    apply::save_local_state(&state.codex_home, &local).await?;
    Ok(())
}

/// bootstrap:启动时全量对齐一次(等同 run_round,语义别名)。
pub async fn bootstrap(state: &AppState) -> Result<(), AppError> {
    run_round(state).await
}

/// 同步单个扩展:查候选 holder → 拉文件清单 → 清旧目录 → 逐文件从 holder 下载落盘 →
/// **基于落盘实际字节 scan_dir 重算指纹** → aggregate_hash 与 PG content_hash 比对:
/// - 匹配 → 登记 local_state + add_holder(自己)扩散。
/// - 不匹配 → 清半成品目录 + 结构化 warn(ext/expected/got) + 不登记、不 add_holder,
///   返回 Ok(())(本轮已处理,local 未更新 → 下轮自然重试;不走 Err 避免与 run_round
///   外层 warn 重复打日志)。
///
/// 下载 / 写盘 / DB 等异常返回 Err,由 run_round 外层 warn 记录后跳过。
async fn sync_one_extension(
    state: &AppState,
    rec: &store::ExtRecord,
    skills_root: &Path,
    local: &mut HashMap<String, String>,
) -> Result<(), AppError> {
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
    // 清旧目录(含上次失败残留的半成品)→ 建空目录。
    apply::remove_dir_safe(skills_root, &rec.name).await?;
    let dest = skills_root.join(&rec.name);
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
        local.insert(rec.id.clone(), rec.content_hash.clone());
        // 扩散:自己也成 holder,后续其他新节点可从本节点下载。
        store::add_holder(&state.db, &rec.id, &state.node_id).await?;
        Ok(())
    } else {
        // hash 不匹配:清半成品目录(避免下次 scan_dir 误读残留 / 用户看到坏文件);
        // 不登记、不 add_holder,返回 Ok(()) —— local 未更新,下轮 need=true 自然重试。
        let _ = apply::remove_dir_safe(skills_root, &rec.name).await;
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

/// 按 id 查扩展 name(删除目录时需要 name 拼路径)。
/// 返回 None:扩展行已不存在(被物理删除)→ 跳过删目录。
async fn name_of(state: &AppState, id: &str) -> Result<Option<String>, AppError> {
    use crate::db::entities::cluster_extension::{Column as ExtCol, Entity as ExtEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let m = ExtEntity::find()
        .filter(ExtCol::Id.eq(id.to_string()))
        .one(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(m.map(|m| m.name))
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

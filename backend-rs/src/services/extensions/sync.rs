//! 集群扩展同步循环:把本地落盘的 skills 对齐到 PG 清单。
//!
//! 每节点跑一个周期 task + 一个 "extensions:changed" 事件订阅 task。
//! - 缺(PG 有本地无/hash 变):从任一 alive holder 逐文件下载 → 落盘 → 校验 hash →
//!   更新本地状态 + add_holder(自己) 完成扩散。
//! - 多(本地有 PG 无):删目录 + 清本地状态。
//! - 变(hash 不同):等同缺,重下覆盖。
//!
//! `run_round` 失败任一扩展不影响其他扩展(各自独立 try);整轮失败(如 DB 断)由调用方
//! 记日志,下一轮或下一次事件重试。

use crate::error::AppError;
use crate::services::extensions::{apply, fingerprint, store};
use crate::state::AppState;
use std::collections::HashSet;

/// 单轮同步:把本地扩展对齐到 PG 清单。
///
/// 步骤:
/// 1. 读 PG `list_enabled`(期望集合)+ 本地 `.cluster-extensions.json`(实际集合)。
/// 2. 本地有、PG 无 → 删目录 + 清本地状态。
/// 3. PG 有、本地无/hash 变 → 拉文件清单 → 逐文件从 holder 下载落盘 → 校验整体 hash
///    一致后登记本地状态,并把本节点 add_holder(扩散)。
/// 4. 落盘整份本地状态。
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
        if let Some(name) = name_of(state, id).await? {
            let _ = apply::remove_dir_safe(&skills_root, &name).await;
        }
        local.remove(id);
    }

    // 新增/更新。
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
        // 拉文件清单 → 清旧目录 → 从 holder 逐文件下载落盘。
        let files = store::get_files(&state.db, &rec.id).await?;
        apply::remove_dir_safe(&skills_root, &rec.name).await?;
        let dest = skills_root.join(&rec.name);
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
        for f in &files {
            let bytes = download_from_holder(state, &rec.id, &f.rel_path).await?;
            apply::write_file_safe(&dest, &f.rel_path, &bytes).await?;
        }
        // 校验整体 hash 一致(与上传方 aggregate_hash 同算法)后再登记,避免半成品计入。
        let got = fingerprint::aggregate_hash(&files);
        if got == rec.content_hash {
            local.insert(rec.id.clone(), rec.content_hash.clone());
            // 扩散:自己也成 holder,后续其他新节点可从本节点下载。
            store::add_holder(&state.db, &rec.id, &state.node_id).await?;
        } else {
            tracing::warn!(
                ext = %rec.id,
                expected = %rec.content_hash,
                got = %got,
                "扩展整体 hash 不匹配,本轮跳过(下次重试)"
            );
        }
    }
    apply::save_local_state(&state.codex_home, &local).await?;
    Ok(())
}

/// bootstrap:启动时全量对齐一次(等同 run_round,语义别名)。
pub async fn bootstrap(state: &AppState) -> Result<(), AppError> {
    run_round(state).await
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

/// 从任一 alive holder 下载单个文件;全失败则报错(本轮跳过该扩展,下一轮/事件重试)。
///
/// holder 选择:取 `list_holders(ext_id)` ∩ 集群 alive 节点,排除自己,逐个尝试。
async fn download_from_holder(
    state: &AppState,
    ext_id: &str,
    rel_path: &str,
) -> Result<Vec<u8>, AppError> {
    let holders = store::list_holders(&state.db, ext_id).await?;
    // alive 节点 ∩ holders(过滤掉已下线的 holder,避免 RPC 超时堆积)。
    let alive: Vec<String> = state
        .cluster
        .alive_nodes()
        .await
        .into_iter()
        .filter(|n| holders.contains(n))
        .collect();
    for node_id in &alive {
        if node_id == &state.node_id {
            continue; // 不从自己下载
        }
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

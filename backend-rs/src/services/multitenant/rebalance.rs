//! 惰性 rebalance:维护循环检查本节点是否过热,过热则迁移一个 thread 到低负载节点。
//!
//! 触发条件(D2 集成到维护循环):本节点 primary thread 数 > avg * HOT_FACTOR(1.5)。
//! 迁移单位 = 单个最旧 thread(updated_at asc),目标 = 负载最低的 alive 节点。
//! 迁移后清 Redis 复制 offset + sticky 绑定,强制 target 下次从 0 全量同步 rollout+文件,
//! 并让后续请求重新解析到新 primary。

use crate::db::entities::session_replica::{Column as SRColumn, Entity as SREntity};
use crate::error::AppError;
use crate::services::multitenant::replication;
use crate::state::AppState;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use std::collections::HashMap;

/// 阈值:本节点 primary 数 > avg * 1.5 视为过热。
const HOT_FACTOR: f64 = 1.5;

/// 从 alive 节点中选负载最低的(仅考虑 load < avg 的节点;无则返回 None)。
///
/// `me` 参数为过热节点 id(目前不显式排除,因为 `me` 的 load 必 > avg 才进入此函数,
/// 自然被 `load < avg` 过滤掉;保留参数为后续策略扩展预留)。
fn pick_least_loaded(
    alive: &[String],
    load: &HashMap<String, i64>,
    me: &str,
    avg: i64,
) -> Option<String> {
    let _ = me; // 预留:当前依赖 load<avg 自然排除过热节点。
    alive
        .iter()
        .filter(|n| load.get(*n).copied().unwrap_or(0) < avg)
        .min_by_key(|n| load.get(*n).copied().unwrap_or(0))
        .cloned()
}

/// 维护循环调用:本节点过热 → 选低负载节点 → 迁移一个最旧的 thread。
///
/// 流程:
/// 1. alive 节点 ≤ 1 → 无 rebalance 空间。
/// 2. 统计每个 alive 节点 primary 数(`session_replicas` 全表扫描)。
/// 3. avg = total / alive 数;`my_load > avg * 1.5` 判定过热。
/// 4. `pick_least_loaded` 选目标(load < avg 中最低);无则放弃本轮。
/// 5. 选本节点最旧 thread(`updated_at asc` 取首条)。
/// 6. 迁移:`set_primary(db, thread, target, new_replica)` + 清 Redis offset + 清 sticky。
///
/// 安全:只迁移 primary_node = 本节点的 thread;`set_primary` 走 ActiveModel update(目标行存在)。
/// 迁移前由调用方确保 rollout 文件已同步到 target(否则 target 晋升后会从 0 全量同步,可能慢)。
pub async fn maybe_rebalance(state: &AppState) -> Result<(), AppError> {
    let alive = state.cluster.alive_nodes().await;
    if alive.len() <= 1 {
        return Ok(());
    }

    // 统计每个 alive 节点的 primary 数(未登记行忽略,不影响 alive 节点的 0 计数)。
    let mut load: HashMap<String, i64> = HashMap::new();
    for n in &alive {
        load.insert(n.clone(), 0);
    }
    let rows = SREntity::find()
        .all(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("rebalance scan: {e}")))?;
    for r in &rows {
        if let Some(c) = load.get_mut(&r.primary_node) {
            *c += 1;
        }
    }
    let total: i64 = load.values().sum();
    let avg = total / alive.len() as i64;

    let my_load = load.get(&state.node_id).copied().unwrap_or(0);
    if (my_load as f64) <= (avg as f64) * HOT_FACTOR {
        return Ok(()); // 未过热。
    }

    let Some(target) = pick_least_loaded(&alive, &load, &state.node_id, avg) else {
        return Ok(()); // 无低负载节点可迁移。
    };

    // 选本节点最旧的一个 thread 迁移(updated_at asc)。
    // 仅迁移 status='active' 的 thread:promoting/degraded 中的 thread 正在晋升流程里,
    // 迁移会干扰 promote_if_primary_down 的状态机(竞争改 primary_node + status)。
    let Some(thread) = SREntity::find()
        .filter(SRColumn::PrimaryNode.eq(state.node_id.clone()))
        .filter(SRColumn::Status.eq("active"))
        .order_by_asc(SRColumn::UpdatedAt)
        // 仅取一条,避免拉全表。
        .one(&state.db)
        .await
        .map_err(|e| AppError::internal(format!("rebalance pick: {e}")))?
    else {
        return Ok(()); // 扫描时无本节点 primary 行(并发刚迁走)。
    };

    // 迁移:把 primary 改为 target,选新 replica(反亲和,alive 中第一个 != target)。
    let new_replica = alive.iter().find(|n| n.as_str() != target).cloned();
    replication::set_primary(&state.db, &thread.thread_id, &target, new_replica.as_deref()).await?;
    // 清该 thread 复制 offset → target 下次从 0 全量同步 rollout+文件。
    if let Some(c) = state.mt_redis.as_ref() {
        replication::delete_all_thread_offsets(c, &thread.thread_id).await;
    }
    // 清 sticky:强制后续请求重新解析到新 primary。
    let _ = state.sticky.clear(&thread.thread_id).await;
    metrics::counter!("rebalance_migrations_total").increment(1);
    tracing::info!(
        thread_id = %thread.thread_id,
        from = %state.node_id,
        to = %target,
        "rebalanced thread"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pick_target_prefers_least_loaded() {
        // 3 节点;node-2 已有 5 thread,node-1 有 1,node-3 有 0。
        let load: std::collections::HashMap<String, i64> = [
            ("node-1".into(), 1),
            ("node-2".into(), 5),
            ("node-3".into(), 0),
        ]
        .into_iter()
        .collect();
        let alive = vec![
            "node-1".to_string(),
            "node-2".to_string(),
            "node-3".to_string(),
        ];
        let me = "node-2"; // 过热节点
        let avg = 6 / 3; // = 2
        // 过热(5 > 2*1.5=3)→ 找 < avg 的节点 → node-3(0)。
        let target = pick_least_loaded(&alive, &load, me, avg);
        assert_eq!(target.as_deref(), Some("node-3"));
    }
}

//! 集群共享的 thread/resume 响应缓存。
//!
//! ## 为什么用 PG 而不是进程内存
//!
//! - **跨进程共享**：集群下 invoke 可能落到任意副本转发至 owner；进程内存 HashMap
//!   只在 owner 本地可见，副本路径反而需要走 codex RPC 触发 -32600 race。
//! - **重启自愈**：进程崩溃后内存 HashMap 丢失，PG 行仍在；下次 resume 仍命中。
//! - **集群 leader 切换无感**：sticky 已路由到唯一 owner，owner 节点重启后第一
//!   次 resume 仍能命中 PG 旧响应。
//!
//! ## 数据流
//!
//! - `put_cached_resume`：mt_create_thread 成功后写入；mt_invoke_thread
//!   thread/resume 成功后 upsert 刷新。
//! - `get_cached_resume`：mt_invoke_thread + 内部 RPC handler 调 thread/resume
//!   前先查；命中直接返回避免 codex 异步落盘 race。
//!
//! ## 失效
//!
//! - 没有 TTL（codex 长期复用同一 rollout 文件，turn/start 后再调 resume 时
//!   自然会刷新本表 response 字段）。
//! - codex app-server 重启导致本节点 generation 变化时（ThreadResumeRegistry
//!   自身的 generation 机制），**这里不需要联动**——本表存的是上一次的完整响应，
//!   重启后第一次 resume 仍能用旧响应撑过去，等下一次真实 resume 再 upsert 刷新。

use sea_orm::entity::prelude::*;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::db::entities::thread_resume_cache::{ActiveModel, Column, Entity, Model};

/// 读取缓存的 thread/resume 响应(命中返回 Some(Value))。
pub async fn get_cached_resume(
    db: &DatabaseConnection,
    thread_id: &str,
) -> Option<serde_json::Value> {
    Entity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|m: Model| m.response)
}

/// upsert 缓存的 thread/resume 响应。失败仅 warn,不传播错误(非阻塞)。
pub async fn put_cached_resume(
    db: &DatabaseConnection,
    thread_id: &str,
    response: &serde_json::Value,
) {
    let now = crate::services::multitenant::now_ms();
    // 先查:存在则更新,不存在则插入。两条 SQL 但避免 sea_orm upsert 的方言差异。
    match Entity::find_by_id(thread_id.to_string()).one(db).await {
        Ok(Some(_)) => {
            if let Err(e) = Entity::update_many()
                .col_expr(Column::Response, Expr::value(response.clone()))
                .col_expr(Column::UpdatedAt, Expr::value(now))
                .filter(Column::ThreadId.eq(thread_id.to_string()))
                .exec(db)
                .await
            {
                tracing::warn!(error = %e, thread_id = %thread_id, "put_cached_resume update failed (non-fatal)");
            }
        }
        Ok(None) => {
            let am = ActiveModel {
                thread_id: Set(thread_id.to_string()),
                response: Set(response.clone()),
                updated_at: Set(now),
            };
            if let Err(e) = am.insert(db).await {
                tracing::warn!(error = %e, thread_id = %thread_id, "put_cached_resume insert failed (non-fatal)");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, thread_id = %thread_id, "put_cached_resume lookup failed (non-fatal)");
        }
    }
}
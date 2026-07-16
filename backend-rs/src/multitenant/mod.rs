//! 多租户平台数据层(M1):以 team 为隔离边界的用户体系。
//!
//! 使用 sqlx(postgres),与现有 rusqlite 业务数据并存(渐进迁移)。
//! 约定:主键 UUIDv7 字符串(VARCHAR(36)),时间 i64 UTC 毫秒。
//! 设计依据:docs/superpowers/specs/2026-07-16-multitenant-platform-design.md。

pub mod api_keys;
pub mod auth;
pub mod codex_pool;
pub mod event_bus;
pub mod handlers;
pub mod migration;
pub mod middleware;
pub mod models;
pub mod routing;
pub mod teams;

/// 当前 UTC 毫秒时间戳。
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 生成 UUIDv7 字符串。
///
/// 选 UUIDv7 的理由:时间有序 → 主键插入递增 → B-tree 友好(MySQL InnoDB 聚簇索引
/// 尤其受益,避免 UUIDv4 随机插入的页分裂);字符串形式跨库无类型差异;
/// 分布式节点独立生成不冲突(无需中央 id 服务)。
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

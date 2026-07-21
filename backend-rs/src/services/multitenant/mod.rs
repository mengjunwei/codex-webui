//! 多租户业务逻辑层。

pub mod api_keys;
pub mod audit;
pub mod auth;
pub mod cluster;
pub mod event_bus;
pub mod event_persist;
pub mod permissions;
pub mod quota;
pub mod rate_limit;
pub mod replication;
pub mod resume_cache;
pub mod rpc;
pub mod sticky;
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

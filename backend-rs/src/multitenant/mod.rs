//! 多租户中间件层（Axum 中间件 + 工具函数）。

pub mod middleware;

// Re-export：业务逻辑层已迁至 services/multitenant/，此处保留工具函数供旧路径兼容。
pub use crate::services::multitenant::{new_id, now_ms};

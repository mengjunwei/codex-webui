//! 多租户 API 层：HTTP handlers + 路由注册。

pub mod handlers;
pub mod internal_rpc;

pub use handlers::require_thread_team;

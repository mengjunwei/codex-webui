//! 集群扩展分发：指纹计算 / 应用 / 存储 / 同步。
//!
//! 本模块按 task 逐步补齐，当前仅 `fingerprint`（Task 3）。

pub mod apply;
pub mod fingerprint;
pub mod store;

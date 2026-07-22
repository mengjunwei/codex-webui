//! 集群扩展分发：指纹计算 / 应用 / 存储 / 同步。
//!
//! 模块组成:`fingerprint`(指纹计算)、`apply`(本地落盘)、`store`(PG 存取)、
//! `sync`(集群同步循环:把本地对齐到 PG 清单,缺则从 holder 下载、多则删、变则更新)。

pub mod apply;
pub mod fingerprint;
pub mod store;
pub mod sync;

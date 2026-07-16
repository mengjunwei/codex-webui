//! 路由层(M4):team → worker 的选择。
//!
//! **设计为多机预留**:`Router` trait 抽象。
//! - `LocalRouter`:单机实现,所有 team 都路由到固定的本地 worker(M3 现状/单机部署)。
//! - `ConsistentHash`:一致性哈希算法(虚拟节点),多 worker 时均匀分布、加减节点最小迁移;
//!   供多机 `RedisRouter`(读 Redis worker 列表 + 心跳)使用,M4 多 worker 启用。
//!
//! 一致性哈希是纯算法,单测可完整验证(分布、迁移最小化),不依赖运行时环境。

use crate::error::AppError;
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// 路由决策:给定 team_id 返回目标 worker_id。
#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, team_id: &str) -> Result<String, AppError>;
    /// 当前已知 worker 列表(监控/调试用)。
    async fn workers(&self) -> Vec<String>;
}

/// 单机路由器:所有 team 都路由到固定的本地 worker_id。
pub struct LocalRouter {
    worker_id: String,
}

impl LocalRouter {
    pub fn new(worker_id: String) -> Self {
        Self { worker_id }
    }
}

#[async_trait]
impl Router for LocalRouter {
    async fn route(&self, _team_id: &str) -> Result<String, AppError> {
        Ok(self.worker_id.clone())
    }
    async fn workers(&self) -> Vec<String> {
        vec![self.worker_id.clone()]
    }
}

/// 一致性哈希环。虚拟节点保证 team 在 worker 间均匀分布;
/// 加/减 worker 时只有相邻区间的 team 迁移(最小化数据搬动)。
pub struct ConsistentHash {
    /// `(hash, worker_id)`,按 hash 升序维护。
    ring: Vec<(u64, String)>,
    vnodes: usize,
}

impl ConsistentHash {
    pub fn new(vnodes: usize) -> Self {
        Self {
            ring: Vec::new(),
            vnodes: vnodes.max(1),
        }
    }

    /// 当前 worker(去重)数量。
    pub fn node_count(&self) -> usize {
        self.ring.iter().map(|(_, w)| w.as_str()).collect::<HashSet<_>>().len()
    }

    /// 加入一个 worker(撒 `vnodes` 个虚拟节点)。
    pub fn add(&mut self, worker: &str) {
        for i in 0..self.vnodes {
            let h = hash_str(&format!("{worker}#{i}"));
            self.ring.push((h, worker.to_string()));
        }
        self.ring.sort_by_key(|(h, _)| *h);
    }

    /// 移除一个 worker 及其全部虚拟节点。
    pub fn remove(&mut self, worker: &str) {
        self.ring.retain(|(_, w)| w != worker);
    }

    /// team_id → worker_id(顺时针;环空返回 None)。
    pub fn get(&self, team_id: &str) -> Option<&str> {
        if self.ring.is_empty() {
            return None;
        }
        let h = hash_str(team_id);
        // 第一个虚拟节点 hash >= h;全小于 h 则回绕到环首。
        let idx = self.ring.partition_point(|(hh, _)| *hh < h) % self.ring.len();
        Some(self.ring[idx].1.as_str())
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn empty_ring_returns_none() {
        let r = ConsistentHash::new(64);
        assert!(r.get("teamA").is_none());
    }

    #[test]
    fn single_worker_routes_all() {
        let mut r = ConsistentHash::new(64);
        r.add("w1");
        for i in 0..50 {
            assert_eq!(r.get(&format!("team{i}")), Some("w1"));
        }
        assert_eq!(r.node_count(), 1);
    }

    #[test]
    fn distribution_is_balanced() {
        let mut r = ConsistentHash::new(150);
        for w in ["w1", "w2", "w3", "w4"] {
            r.add(w);
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for i in 0..4000 {
            let w = r.get(&format!("team{i}")).unwrap();
            *counts.entry(w).or_insert(0) += 1;
        }
        // 4 worker × 4000 team → 每 worker 约 1000,允许 ±30%(虚拟节点哈希抖动)。
        for w in ["w1", "w2", "w3", "w4"] {
            let c = *counts.get(w).unwrap_or(&0);
            assert!(c > 700 && c < 1300, "worker {w} got {c}, expected ~1000");
        }
    }

    #[test]
    fn add_worker_migrates_minority() {
        // 加第 3 个 worker 后,只有约 1/3 的 team 迁移到 w3,其余原地不动(一致性)。
        let mut r = ConsistentHash::new(150);
        r.add("w1");
        r.add("w2");
        let before: Vec<String> =
            (0..2000).map(|i| r.get(&format!("team{i}")).unwrap().to_string()).collect();
        r.add("w3");
        let mut migrated = 0usize;
        for (i, b) in before.iter().enumerate() {
            if r.get(&format!("team{i}")).unwrap() != b.as_str() {
                migrated += 1;
            }
        }
        // 新 worker 承接约 1/3;区间内波动。迁移数应在合理范围(不是全部、不是极少)。
        assert!(migrated > 400 && migrated < 1000, "migrated {migrated} out of 2000");
    }

    #[test]
    fn remove_worker_reroutes_to_others() {
        let mut r = ConsistentHash::new(150);
        for w in ["w1", "w2", "w3"] {
            r.add(w);
        }
        r.remove("w2");
        assert_eq!(r.node_count(), 2);
        for i in 0..200 {
            let w = r.get(&format!("team{i}")).unwrap();
            assert!(w != "w2", "removed worker still routed");
        }
    }
}

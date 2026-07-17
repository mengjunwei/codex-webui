//! 集群成员管理 + 故障探活(`ClusterMembership` 抽象)。
//!
//! 用于:主副本分配时选 alive 节点(反亲和)、副本晋升时判定主是否失活、转发时解析节点 RPC 地址。
//!
//! 两个实现:
//! - `RedisCluster`(默认):Redis 心跳 TTL —— 节点周期 `SETEX cluster:node:{id}` = rpc_addr,
//!   `alive_nodes` 查 `cluster:nodes` + 逐个 `EXISTS` 过滤。等价 gossip 故障检测,本环境可编译可运行。
//! - `MemberlistCluster`(feature `memberlist-backend`):memberlist 0.8.5 gossip。

use crate::error::AppError;
use async_trait::async_trait;

/// 集群成员 + 探活抽象。
#[async_trait]
pub trait ClusterMembership: Send + Sync {
    /// 本节点 id。
    fn local_node_id(&self) -> &str;
    /// 当前存活节点 id 列表(含自己)。
    async fn alive_nodes(&self) -> Vec<String>;
    /// 解析某节点的内网 RPC 地址(转发/复制时用)。
    async fn node_rpc_addr(&self, node_id: &str) -> Option<String>;
}

// ── Redis 实现(默认)──────────────────────────────────────────────────────

pub struct RedisCluster {
    client: redis::Client,
    node_id: String,
}

impl RedisCluster {
    pub fn new(client: redis::Client, node_id: String) -> Self {
        Self { client, node_id }
    }

    /// 心跳:`SADD cluster:nodes {id}` + `SETEX cluster:node:{id} rpc_addr ttl`。
    pub async fn heartbeat(&self, ttl_secs: u64, rpc_addr: &str) -> Result<(), AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let _: i64 = redis::cmd("SADD")
            .arg("cluster:nodes")
            .arg(&self.node_id)
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis sadd cluster: {e}")))?;
        let _: () = redis::cmd("SET")
            .arg(format!("cluster:node:{}", self.node_id))
            .arg(rpc_addr)
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis setex cluster: {e}")))?;
        Ok(())
    }

    async fn get_node_rpc_addr(&self, node_id: &str) -> Option<String> {
        let mut conn = self.client.get_multiplexed_async_connection().await.ok()?;
        let v: Option<String> = redis::cmd("GET")
            .arg(format!("cluster:node:{node_id}"))
            .query_async(&mut conn)
            .await
            .ok()?;
        v.filter(|s| !s.is_empty())
    }
}

#[async_trait]
impl ClusterMembership for RedisCluster {
    fn local_node_id(&self) -> &str {
        &self.node_id
    }

    async fn alive_nodes(&self) -> Vec<String> {
        let Ok(mut conn) = self.client.get_multiplexed_async_connection().await else {
            return vec![];
        };
        let members: Vec<String> = redis::cmd("SMEMBERS")
            .arg("cluster:nodes")
            .query_async::<Vec<String>>(&mut conn)
            .await
            .unwrap_or_default();
        let mut alive = Vec::new();
        for n in members {
            let exists: i64 = redis::cmd("EXISTS")
                .arg(format!("cluster:node:{n}"))
                .query_async(&mut conn)
                .await
                .unwrap_or(0);
            if exists == 1 {
                alive.push(n);
            }
        }
        alive
    }

    async fn node_rpc_addr(&self, node_id: &str) -> Option<String> {
        self.get_node_rpc_addr(node_id).await
    }
}

// ── memberlist 实现(feature gate;transport/delegate 联调待部署期)─────────
#[cfg(feature = "memberlist-backend")]
pub mod memberlist_impl {
    use super::ClusterMembership;
    use async_trait::async_trait;

    pub struct MemberlistCluster {
        node_id: String,
    }

    impl MemberlistCluster {
        pub async fn new(node_id: String, _bind: &str, _join: &[String]) -> anyhow::Result<Self> {
            anyhow::bail!(
                "MemberlistCluster not yet wired: enable feature and complete transport/delegate construction"
            );
        }
    }

    #[async_trait]
    impl ClusterMembership for MemberlistCluster {
        fn local_node_id(&self) -> &str {
            &self.node_id
        }
        async fn alive_nodes(&self) -> Vec<String> {
            vec![self.node_id.clone()]
        }
        async fn node_rpc_addr(&self, _node_id: &str) -> Option<String> {
            None
        }
    }
}

/// 单节点集群(无 Redis):只自己 alive,RPC 地址 = 本机 own_rpc_url。用于单机/无 Redis 部署。
pub struct SingleCluster {
    node_id: String,
    rpc_url: String,
}

impl SingleCluster {
    pub fn new(node_id: String, rpc_url: String) -> Self {
        Self { node_id, rpc_url }
    }
}

#[async_trait]
impl ClusterMembership for SingleCluster {
    fn local_node_id(&self) -> &str {
        &self.node_id
    }
    async fn alive_nodes(&self) -> Vec<String> {
        vec![self.node_id.clone()]
    }
    async fn node_rpc_addr(&self, node_id: &str) -> Option<String> {
        if node_id == self.node_id {
            Some(self.rpc_url.clone())
        } else {
            None
        }
    }
}

/// 判定某节点是否 alive。
pub async fn is_alive<C: ClusterMembership + ?Sized>(cluster: &C, node_id: &str) -> bool {
    cluster.alive_nodes().await.iter().any(|n| n == node_id)
}

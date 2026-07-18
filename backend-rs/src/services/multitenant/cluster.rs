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
    /// 同时清理已过期的 stale 成员(SMEMBERS + EXISTS 过滤 + SREM),防止集合无限膨胀。
    pub async fn heartbeat(&self, ttl_secs: u64, rpc_addr: &str) -> Result<(), AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;

        // 先清理 stale 成员:SMEMBERS 取全部,用 Lua 原子完成 EXISTS+SREM。
        // 原 EXISTS→SREM 分两步,A 的 EXISTS(=0)与 SREM 之间 B 可能重新 SADD+SETEX,
        // A 的 SREM 会删掉刚复活的 B(10s 后才回来)。Lua 原子消除该竞态(C1 修复)。
        const CLEAN_LUA: &str = r#"
            if redis.call('EXISTS', KEYS[2]) == 0 then
                return redis.call('SREM', KEYS[1], ARGV[1])
            else
                return 0
            end
        "#;
        if let Ok(members) = redis::cmd("SMEMBERS")
            .arg("cluster:nodes")
            .query_async::<Vec<String>>(&mut conn)
            .await
        {
            for m in members {
                if m == self.node_id {
                    continue; // 不删自己
                }
                let removed: i64 = redis::cmd("EVAL")
                    .arg(CLEAN_LUA)
                    .arg(2)
                    .arg("cluster:nodes")
                    .arg(format!("cluster:node:{m}"))
                    .arg(&m)
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(0);
                if removed > 0 {
                    tracing::debug!(node = %m, "cleaned stale cluster member");
                }
            }
        }

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

// ── memberlist 实现(feature gate; memberlist 0.8.5 CompositeDelegate + TokioNetTransport)──
#[cfg(feature = "memberlist-backend")]
pub mod memberlist_impl {
    use super::ClusterMembership;
    use async_trait::async_trait;
    use memberlist::delegate::{AliveDelegate, CompositeDelegate, EventDelegate, VoidDelegate};
    use memberlist::proto::{MaybeResolvedAddress, NodeState};
    use memberlist::tokio::{TokioSocketAddrResolver, TokioTcp, TokioTcpMemberlist};
    use memberlist::Options as MlOptions;
    use memberlist::net::NetTransportOptions;
    use nodecraft::NodeId;
    use std::collections::HashSet;
    use std::net::SocketAddr;
    use std::sync::Arc;

    // ── AliveDelegate: 仅做类型占位，实际探活由 online_members() 驱动 ────────────
    pub struct HaAliveDelegate {
        alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
        _node_id: NodeId,
    }

    impl AliveDelegate for HaAliveDelegate {
        type Id = NodeId;
        type Address = SocketAddr;
        type Error = std::io::Error;

        async fn notify_alive(
            &self,
            _peer: Arc<NodeState<Self::Id, Self::Address>>,
        ) -> Result<(), Self::Error> {
            // memberlist 内部已有状态机; 我们在 EventDelegate 里跟踪 join/leave。
            Ok(())
        }
    }

    // ── EventDelegate: 跟踪节点加入/离开 ──────────────────────────────────────────
    pub struct HaEventDelegate {
        alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
    }

    impl EventDelegate for HaEventDelegate {
        type Id = NodeId;
        type Address = SocketAddr;

        async fn notify_join(&self, node: Arc<NodeState<Self::Id, Self::Address>>) {
            let id = node.id().clone();
            if let Ok(mut g) = self.alive.try_write() {
                g.insert(id);
            }
        }

        async fn notify_leave(&self, node: Arc<NodeState<Self::Id, Self::Address>>) {
            let id = node.id().clone();
            if let Ok(mut g) = self.alive.try_write() {
                g.remove(&id);
            }
        }

        async fn notify_update(&self, _node: Arc<NodeState<Self::Id, Self::Address>>) {}
    }

    // CompositeDelegate: Alive + Event 由我们提供，其余用 VoidDelegate。
    pub type HaDelegate = CompositeDelegate<
        NodeId,
        SocketAddr,
        HaAliveDelegate,
        VoidDelegate<NodeId, SocketAddr>,
        HaEventDelegate,
        VoidDelegate<NodeId, SocketAddr>,
        VoidDelegate<NodeId, SocketAddr>,
        VoidDelegate<NodeId, SocketAddr>,
    >;

    pub struct MemberlistCluster {
        pub node_id: String,
        pub memberlist: Arc<TokioTcpMemberlist<NodeId, TokioSocketAddrResolver, HaDelegate>>,
        pub alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
        pub redis: redis::Client,
        pub own_rpc_url: String,
    }

    impl MemberlistCluster {
        pub async fn new(
            node_id_str: String,
            bind: &str,
            seeds: &[String],
            redis: redis::Client,
            own_rpc_url: String,
        ) -> anyhow::Result<Self> {
            let bind_addr: SocketAddr = bind
                .parse()
                .map_err(|e| anyhow::anyhow!("parse bind {bind}: {e}"))?;

            let node_id = NodeId::new(&node_id_str)
                .map_err(|e| anyhow::anyhow!("NodeId::new({node_id_str}): {e}"))?;

            let alive = Arc::new(tokio::sync::RwLock::new(HashSet::from([node_id.clone()])));

            let alive_delegate = HaAliveDelegate {
                alive: alive.clone(),
                _node_id: node_id.clone(),
            };
            let event_delegate = HaEventDelegate {
                alive: alive.clone(),
            };
            let delegate = CompositeDelegate::new()
                .with_alive_delegate(alive_delegate)
                .with_event_delegate(event_delegate);

            // NetTransportOptions: NodeId + bind_addresses + default resolver/stream_layer
            let mut transport_opts = NetTransportOptions::<NodeId, TokioSocketAddrResolver, TokioTcp>::new(
                node_id.clone(),
            );
            transport_opts.add_bind_address(bind_addr);

            let opts = MlOptions::default();

            let m = TokioTcpMemberlist::<NodeId, TokioSocketAddrResolver, HaDelegate>::with_delegate(
                delegate,
                transport_opts,
                opts,
            )
            .await
            .map_err(|e| anyhow::anyhow!("memberlist init: {e}"))?;

            // Seed join
            for seed in seeds {
                let addr: SocketAddr = match seed.trim().parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let _ = m.join(MaybeResolvedAddress::Unresolved(addr)).await;
            }

            // Redis 心跳: 每 10s SETEX cluster:node:{id} = rpc_url, TTL 30s
            let redis_hb = redis.clone();
            let hb_node = node_id_str.clone();
            let hb_rpc = own_rpc_url.clone();
            tokio::spawn(async move {
                loop {
                    if let Ok(mut conn) = redis_hb.get_multiplexed_async_connection().await {
                        let _: Result<(), _> = redis::cmd("SET")
                            .arg(format!("cluster:node:{hb_node}"))
                            .arg(&hb_rpc)
                            .arg("EX")
                            .arg(30)
                            .query_async(&mut conn)
                            .await;
                        let _: Result<i64, _> = redis::cmd("SADD")
                            .arg("cluster:nodes")
                            .arg(&hb_node)
                            .query_async(&mut conn)
                            .await;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            });

            Ok(Self {
                node_id: node_id_str,
                memberlist: Arc::new(m),
                alive,
                redis,
                own_rpc_url,
            })
        }

        /// 通过 memberlist online_members 获取存活节点 id 列表。
        pub async fn alive_node_ids(&self) -> Vec<String> {
            // 优先用 memberlist 内置状态(gossip 已检测 dead/failed)
            let members = self.memberlist.online_members().await;
            if !members.is_empty() {
                return members.into_iter().map(|n| n.id().to_string()).collect();
            }
            // fallback: 从 EventDelegate 维护的 set 读
            let g = self.alive.read().await;
            g.iter().map(|id| id.to_string()).collect()
        }
    }

    #[async_trait]
    impl ClusterMembership for MemberlistCluster {
        fn local_node_id(&self) -> &str {
            &self.node_id
        }

        async fn alive_nodes(&self) -> Vec<String> {
            self.alive_node_ids().await
        }

        async fn node_rpc_addr(&self, node_id: &str) -> Option<String> {
            if node_id == self.node_id {
                return Some(self.own_rpc_url.clone());
            }
            let mut conn = self.redis.get_multiplexed_async_connection().await.ok()?;
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("cluster:node:{node_id}"))
                .query_async(&mut conn)
                .await
                .ok()?;
            v.filter(|s| !s.is_empty())
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "memberlist-backend")]
    #[test]
    fn memberlist_cluster_types_compile() {
        // 验证 MemberlistCluster 类型 + CompositeDelegate + NodeId 均可引用。
        use super::memberlist_impl::{HaDelegate, MemberlistCluster};
        use nodecraft::NodeId;
        let _: fn() -> NodeId = || NodeId::new("test-node").unwrap();
        let _ = std::marker::PhantomData::<MemberlistCluster>;
        let _ = std::marker::PhantomData::<HaDelegate>;
    }
}

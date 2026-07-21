//! 事件总线(M4):codex notification 跨节点广播的基础设施。
//!
//! **设计为多机预留**:`EventBus` trait 抽象。
//! - `InMemoryEventBus`:单机实现(基于 tokio broadcast),单进程内 pub/sub,单测可验证。
//! - Redis Pub/Sub 实现(多机,跨 worker/接入节点广播)按同一 trait 实现,M4 多节点时启用。
//!
//! topic 约定:`team:{team_id}` 或 `thread:{thread_id}`(按事件归属)。
//! 接入(codex 单进程启动时 spawn 转发:client.subscribe_notifications → bus.publish;
//! 接入层订阅 bus → socket.io emit)留 M4 后期,需改 codex 进程接入 + realtime。

use crate::error::AppError;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::{broadcast, Mutex};

/// 事件总线:发布/订阅字符串载荷(事件 JSON)。
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, topic: &str, payload: &str) -> Result<(), AppError>;
    async fn subscribe(&self, topic: &str) -> Result<broadcast::Receiver<String>, AppError>;
}

/// 单机内存事件总线(tokio broadcast)。
pub struct InMemoryEventBus {
    senders: Mutex<HashMap<String, broadcast::Sender<String>>>,
    capacity: usize,
}

impl InMemoryEventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            capacity: capacity.max(16),
        }
    }
}

#[async_trait]
impl EventBus for InMemoryEventBus {
    async fn publish(&self, topic: &str, payload: &str) -> Result<(), AppError> {
        let map = self.senders.lock().await;
        if let Some(tx) = map.get(topic) {
            // 无订阅者 / 队列满 → 忽略(broadcast 是 best-effort;实时流最终一致)。
            let _ = tx.send(payload.to_string());
        }
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<broadcast::Receiver<String>, AppError> {
        let mut map = self.senders.lock().await;
        let tx = map
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel::<String>(self.capacity).0);
        Ok(tx.subscribe())
    }
}

// ── Redis 实现(多机跨节点广播)─────────────────────────────────────────────
use std::time::Duration;

/// Redis Pub/Sub 事件总线(多机):`PUBLISH` 发布;`SUBSCRIBE` 后台 task 收消息转 broadcast。
///
/// 需 `REDIS_URL` 配置;单机用本地 redis-server 即可验证。多 worker 时:
/// 任意 worker publish → 所有接入节点 subscribe → emit 到各自持有的 socket。
pub struct RedisEventBus {
    client: redis::Client,
    /// topic → broadcast sender(每个 topic 一个后台订阅 task,断线自动重连)。
    senders: Mutex<HashMap<String, broadcast::Sender<String>>>,
    capacity: usize,
}

impl RedisEventBus {
    pub fn new(client: redis::Client, capacity: usize) -> Self {
        Self {
            client,
            senders: Mutex::new(HashMap::new()),
            capacity: capacity.max(16),
        }
    }
}

#[async_trait]
impl EventBus for RedisEventBus {
    async fn publish(&self, topic: &str, payload: &str) -> Result<(), AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let _: i64 = redis::cmd("PUBLISH")
            .arg(topic)
            .arg(payload)
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis publish: {e}")))?;
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<broadcast::Receiver<String>, AppError> {
        let mut map = self.senders.lock().await;
        if let Some(tx) = map.get(topic) {
            return Ok(tx.subscribe());
        }
        let (tx, _) = broadcast::channel::<String>(self.capacity);
        map.insert(topic.to_string(), tx.clone());
        drop(map);

        // 后台 task:订阅 redis channel,消息转 broadcast;断线自动重连。
        let client = self.client.clone();
        let topic_owned = topic.to_string();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            loop {
                let pubsub = match client.get_async_pubsub().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "redis pubsub connect failed, retry");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                run_pubsub(pubsub, &topic_owned, &tx2).await;
                tracing::warn!("redis pubsub stream ended, reconnecting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Ok(tx.subscribe())
    }
}

/// 单个 pubsub 连接的消息循环:订阅 topic,把消息转 broadcast;连接断开时返回。
async fn run_pubsub(
    mut pubsub: redis::aio::PubSub,
    topic: &str,
    tx: &broadcast::Sender<String>,
) {
    if let Err(e) = pubsub.subscribe(topic).await {
        tracing::warn!(error = %e, "redis subscribe failed");
        return;
    }
    use futures_util::StreamExt;
    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload().unwrap_or_default();
        let _ = tx.send(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pub_sub_delivers() {
        let bus = InMemoryEventBus::new(16);
        let mut rx = bus.subscribe("team:t1").await.unwrap();
        bus.publish("team:t1", "hello").await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn no_subscriber_publish_ok() {
        let bus = InMemoryEventBus::new(16);
        // 无订阅者 publish 不应报错。
        bus.publish("team:t1", "x").await.unwrap();
    }

    #[tokio::test]
    async fn topics_isolated() {
        let bus = InMemoryEventBus::new(16);
        let mut rx_a = bus.subscribe("team:a").await.unwrap();
        let mut rx_b = bus.subscribe("team:b").await.unwrap();
        bus.publish("team:a", "to-a").await.unwrap();
        assert_eq!(rx_a.recv().await.unwrap(), "to-a");
        // b 不应收到 a 的事件(50ms 内无消息)。
        let got = tokio::time::timeout(std::time::Duration::from_millis(50), rx_b.recv()).await;
        assert!(got.is_err(), "topic b leaked event from a");
    }
}

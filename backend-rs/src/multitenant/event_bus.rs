//! 事件总线(M4):codex notification 跨节点广播的基础设施。
//!
//! **设计为多机预留**:`EventBus` trait 抽象。
//! - `InMemoryEventBus`:单机实现(基于 tokio broadcast),单进程内 pub/sub,单测可验证。
//! - Redis Pub/Sub 实现(多机,跨 worker/接入节点广播)按同一 trait 实现,M4 多节点时启用。
//!
//! topic 约定:`team:{team_id}` 或 `thread:{thread_id}`(按事件归属)。
//! 接入(TeamCodexManager 启动时 spawn 转发:client.subscribe_notifications → bus.publish;
//! 接入层订阅 bus → socket.io emit)留 M4 后期,需改 codex_pool + realtime。

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

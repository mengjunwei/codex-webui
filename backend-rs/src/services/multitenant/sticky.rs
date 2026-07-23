//! 会话粘性存储(M4):thread → worker 的绑定,保证同一会话始终落到同一 worker。
//!
//! - `RedisSticky`:多机实现(Redis SET EX,活跃续期)。
//! - `NoopSticky`:单机/无 Redis(始终 None,由本地路由决定)。
//!
//! 粘性作用:codex 会话状态(进程内存中的对话上下文)在 worker 本地,跨 worker 迁移会
//! 丢失上下文需重新 resume。粘性让 turn 优先复用创建 thread 的 worker。

use crate::error::AppError;
use async_trait::async_trait;

/// 会话粘性存储 trait。
#[async_trait]
pub trait StickyStore: Send + Sync {
    /// 绑定 thread → worker(TTL 秒)。
    async fn bind(&self, thread_id: &str, worker_id: &str, ttl_secs: u64) -> Result<(), AppError>;
    /// 查询 thread 绑定的 worker(TTL 内有效);无则 None。
    async fn lookup(&self, thread_id: &str) -> Result<Option<String>, AppError>;
    /// 清除绑定(failover / evict / 删除会话)。
    async fn clear(&self, thread_id: &str) -> Result<(), AppError>;
}

/// 单机/无 Redis:不记录粘性,始终 None。
pub struct NoopSticky;

#[async_trait]
impl StickyStore for NoopSticky {
    async fn bind(&self, _thread_id: &str, _worker_id: &str, _ttl_secs: u64) -> Result<(), AppError> {
        Ok(())
    }
    async fn lookup(&self, _thread_id: &str) -> Result<Option<String>, AppError> {
        Ok(None)
    }
    async fn clear(&self, _thread_id: &str) -> Result<(), AppError> {
        Ok(())
    }
}

/// Redis 粘性存储:`SET sticky:thread:{tid} {worker_id} EX {ttl}`。
pub struct RedisSticky {
    client: redis::Client,
}

impl RedisSticky {
    pub fn new(client: redis::Client) -> Self {
        Self { client }
    }
    fn key(thread_id: &str) -> String {
        format!("sticky:thread:{thread_id}")
    }
}

#[async_trait]
impl StickyStore for RedisSticky {
    async fn bind(&self, thread_id: &str, worker_id: &str, ttl_secs: u64) -> Result<(), AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let _: () = redis::cmd("SET")
            .arg(Self::key(thread_id))
            .arg(worker_id)
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis set sticky: {e}")))?;
        Ok(())
    }

    async fn lookup(&self, thread_id: &str) -> Result<Option<String>, AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let v: Option<String> = redis::cmd("GET")
            .arg(Self::key(thread_id))
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis get sticky: {e}")))?;
        Ok(v.filter(|s| !s.is_empty()))
    }

    async fn clear(&self, thread_id: &str) -> Result<(), AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let _: i64 = redis::cmd("DEL")
            .arg(Self::key(thread_id))
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis del sticky: {e}")))?;
        Ok(())
    }
}

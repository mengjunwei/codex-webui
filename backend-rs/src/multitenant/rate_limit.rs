//! Redis 固定窗口限流(M6-A:防滥用)。
//!
//! 用 Redis `INCR` + `EXPIRE` 实现固定窗口:同一 key 在 `window_secs` 内最多 `max` 次。
//! 典型 key:`rl:{action}:{ip}` / `rl:{action}:{user_id}`。
//! 需要 `REDIS_URL` 配置;未配置则由调用方跳过(不禁用)。

use crate::error::AppError;

/// Redis 固定窗口限流器。
pub struct RedisRateLimiter {
    client: redis::Client,
}

impl RedisRateLimiter {
    pub fn new(client: redis::Client) -> Self {
        Self { client }
    }

    /// 判断 key 是否仍允许访问(窗口内未超 max)。返回 `true` = 允许。
    ///
    /// 实现:`INCR key`(首次为 1),首次时 `EXPIRE key window`;`count <= max` 即放行。
    /// 原子性由 Redis 单线程命令顺序保证(INCR 与 EXPIRE 非同一事务,但首计数设过期,
    /// 后续窗口内递增;极端竞态下过期可能略偏,对限流可接受)。
    pub async fn allow(&self, key: &str, max: u32, window_secs: u64) -> Result<bool, AppError> {
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AppError::internal(format!("redis connect: {e}")))?;
        let count: i64 = redis::cmd("INCR")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::internal(format!("redis incr: {e}")))?;
        if count == 1 {
            let _: () = redis::cmd("EXPIRE")
                .arg(key)
                .arg(window_secs)
                .query_async(&mut conn)
                .await
                .map_err(|e| AppError::internal(format!("redis expire: {e}")))?;
        }
        Ok(count <= max as i64)
    }
}

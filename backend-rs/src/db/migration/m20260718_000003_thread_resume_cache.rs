//! thread_resume_cache(集群共享缓存):mt_create_thread 写入 thread/start 响应,
//! mt_invoke_thread 收到 thread/resume 时优先读此表避免 codex 异步落盘 race。
//!
//! 设计要点:
//! - 跨进程共享:集群下任意 worker 都可读;不再依赖进程内 ThreadResumeRegistry。
//! - 重启自愈:进程崩溃后 HashMap 会丢,PG 行仍在 → 恢复时仍命中。
//! - 失效策略:thread/resume 成功后 upsert(response, updated_at);
    //  长期保留(无 TTL)—— codex rollout 文件变更会由 turn/start 触发新的 resume。

use super::create_index;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260718_000003_thread_resume_cache"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"CREATE TABLE IF NOT EXISTS thread_resume_cache (
                    thread_id VARCHAR(36) PRIMARY KEY,
                    response JSON NOT NULL,
                    updated_at BIGINT NOT NULL
                )"#,
            )
            .await?;
        create_index(manager, "idx_thread_resume_cache_updated", "thread_resume_cache", "updated_at").await?;
        Ok(())
    }
}
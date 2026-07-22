//! 集群扩展分发:清单 / 文件指纹 / 持有节点 三张表(集群级,无 team_id)。
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str { "m20260722_000001_cluster_extensions" }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extensions (
                id VARCHAR(36) PRIMARY KEY NOT NULL,
                kind VARCHAR(32) NOT NULL,
                name VARCHAR(128) NOT NULL,
                display_name VARCHAR(256),
                description TEXT,
                version VARCHAR(64),
                content_form VARCHAR(16) NOT NULL,
                config_text TEXT,
                content_hash VARCHAR(128) NOT NULL,
                enabled BOOLEAN NOT NULL DEFAULT TRUE,
                created_at BIGINT NOT NULL,
                updated_at BIGINT NOT NULL,
                created_by VARCHAR(36)
            )"#,
        ).await?;
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extension_files (
                id BIGINT PRIMARY KEY NOT NULL,
                extension_id VARCHAR(36) NOT NULL,
                rel_path VARCHAR(512) NOT NULL,
                size_bytes BIGINT NOT NULL,
                content_hash VARCHAR(128) NOT NULL,
                is_binary BOOLEAN NOT NULL DEFAULT FALSE
            )"#,
        ).await?;
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS cluster_extension_holders (
                extension_id VARCHAR(36) NOT NULL,
                node_id VARCHAR(36) NOT NULL,
                held_since BIGINT NOT NULL
            )"#,
        ).await?;
        crate::db::migration::create_index(manager, "idx_ext_kind_name", "cluster_extensions", "kind,name").await?;
        crate::db::migration::create_index(manager, "idx_ext_enabled", "cluster_extensions", "enabled").await?;
        crate::db::migration::create_index(manager, "idx_extfile_ext", "cluster_extension_files", "extension_id").await?;
        db.execute_unprepared("COMMENT ON TABLE cluster_extensions IS '集群扩展分发清单'").await.ok();
        db.execute_unprepared("COMMENT ON TABLE cluster_extension_files IS '扩展文件指纹(无内容)'").await.ok();
        db.execute_unprepared("COMMENT ON TABLE cluster_extension_holders IS '扩展持有节点(去单点)'").await.ok();
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS cluster_extension_holders").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS cluster_extension_files").await?;
        db.execute_unprepared("DROP TABLE IF EXISTS cluster_extensions").await?;
        Ok(())
    }
}

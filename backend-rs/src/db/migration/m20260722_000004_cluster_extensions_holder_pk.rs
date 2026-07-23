//! 补 cluster_extension_holders 复合主键 (extension_id, node_id)。
//!
//! 背景:000001 建表时 holders 表漏了 PRIMARY KEY(只有三列 extension_id/node_id/held_since,
//! 无任何约束),但 entity(mod.rs) 却声明了复合主键。store::add_holder 用
//! `ON CONFLICT DO NOTHING`(不指定冲突目标)防重复登记——但 PG 该子句在**无任何唯一约束**
//! 的表上**永不触发**,故"防重复"设计未生效;local_state 损坏/重传时会累积重复 holder 行。
//!
//! 本迁移:先去重(防现有重复行致主键建失败)→ 加复合主键。加主键后 ON CONFLICT DO NOTHING
//! 真正生效,entity 与表 schema 对齐。
//!
//! 多方言(PG/MySQL):
//! - 去重:PG 用 ctid 自连接(保留每组 ctid 最小行);MySQL 无 ctid,用临时表 DISTINCT+GROUP BY
//!   保留每组 MIN(held_since) 后 TRUNCATE+回插。
//! - 加主键:PG `ADD CONSTRAINT ... PRIMARY KEY`;MySQL `ADD PRIMARY KEY`(MySQL 主键无名)。

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260722_000004_cluster_extensions_holder_pk"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();

        // 1. 去重(防现有重复行致主键建失败)。holders 表在生产通常无数据(extensions enable
        //    默认 false),此步为防御;若有重复必先清,否则加主键失败。
        match backend {
            DbBackend::MySql => {
                // MySQL 无 ctid:临时表保留每组 (extension_id,node_id) 的 MIN(held_since) →
                // TRUNCATE 原表 → 回插去重后的行 → 删临时表。
                db.execute_unprepared(
                    r#"CREATE TEMPORARY TABLE _holder_keep AS
                       SELECT extension_id, node_id, MIN(held_since) AS held_since
                       FROM cluster_extension_holders
                       GROUP BY extension_id, node_id"#,
                )
                .await?;
                db.execute_unprepared("TRUNCATE TABLE cluster_extension_holders")
                    .await?;
                db.execute_unprepared(
                    r#"INSERT INTO cluster_extension_holders (extension_id, node_id, held_since)
                       SELECT extension_id, node_id, held_since FROM _holder_keep"#,
                )
                .await?;
                db.execute_unprepared("DROP TEMPORARY TABLE _holder_keep")
                    .await?;
            }
            _ => {
                // Postgres:ctid 自连接,保留每组 ctid 最小行,删其余重复。
                db.execute_unprepared(
                    r#"DELETE FROM cluster_extension_holders a
                       USING cluster_extension_holders b
                       WHERE a.extension_id = b.extension_id
                         AND a.node_id = b.node_id
                         AND a.ctid < b.ctid"#,
                )
                .await?;
            }
        }

        // 2. 加复合主键。迁移顺序保证首次执行时表无主键(000001 未建),无需 IF NOT EXISTS。
        //    PG 命名约束 pk_ext_holder;MySQL 主键无名(直接 ADD PRIMARY KEY)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extension_holders ADD PRIMARY KEY (extension_id, node_id)",
                )
                .await?;
            }
            _ => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extension_holders ADD CONSTRAINT pk_ext_holder PRIMARY KEY (extension_id, node_id)",
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        // down:移除主键(PG 命名约束 IF EXISTS;MySQL DROP PRIMARY KEY,.ok() 容错无主键)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared("ALTER TABLE cluster_extension_holders DROP PRIMARY KEY")
                    .await
                    .ok();
            }
            _ => {
                db.execute_unprepared(
                    "ALTER TABLE cluster_extension_holders DROP CONSTRAINT IF EXISTS pk_ext_holder",
                )
                .await?;
            }
        }
        Ok(())
    }
}

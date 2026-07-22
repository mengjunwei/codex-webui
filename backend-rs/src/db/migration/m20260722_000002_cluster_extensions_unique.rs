//! 补 UNIQUE 约束:cluster_extensions (kind,name) 与 cluster_extension_files (extension_id,rel_path)。
//!
//! 000001 误用 `create_index` 助手建普通索引;spec §4.1/§4.2 要求 UNIQUE。
//! 缺失会导致并发上传同名扩展产生重复行(DB 不拦截),后续读出多行行为未定义。
//!
//! 本迁移:先去重(防已有重复行致 unique 建失败)→ 删旧普通索引 → 建 UNIQUE 索引。
//!
//! 多方言(PG/MySQL):
//! - 去重自连接删除:PG 用 `DELETE FROM ... USING`;MySQL 用 `DELETE alias FROM ... JOIN`
//!   (各自方言的自然写法,不依赖窗口函数,兼容 MySQL 5.7)。
//! - DROP/CREATE INDEX:PG 用 `IF EXISTS` / `IF NOT EXISTS` 幂等;
//!   MySQL 旧版不支持这些修饰符 → DROP 用 `.ok()` 容错(索引不存在时静默),
//!   CREATE 跟在迁移顺序后、索引名带 `_unique` 后缀,首次执行不存在。

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260722_000002_cluster_extensions_unique"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();

        // 1. 去重(防已有重复行致 unique 建失败;现网 enable=false 基本无数据,稳妥加)。
        match backend {
            DbBackend::MySql => {
                // cluster_extensions:每 (kind,name) 保留 updated_at 最大(id 最大做 tie-break),删其余。
                // MySQL 单表自连接删除:DELETE alias FROM ... INNER JOIN。
                db.execute_unprepared(
                    r#"DELETE c1 FROM cluster_extensions c1
                       INNER JOIN cluster_extensions c2
                         ON c1.kind = c2.kind AND c1.name = c2.name
                       WHERE c1.updated_at < c2.updated_at
                          OR (c1.updated_at = c2.updated_at AND c1.id < c2.id)"#,
                )
                .await?;
                // cluster_extension_files:每 (extension_id,rel_path) 保留 id 最小,删其余。
                db.execute_unprepared(
                    r#"DELETE f1 FROM cluster_extension_files f1
                       INNER JOIN cluster_extension_files f2
                         ON f1.extension_id = f2.extension_id AND f1.rel_path = f2.rel_path
                       WHERE f1.id > f2.id"#,
                )
                .await?;
            }
            _ => {
                // Postgres:DELETE FROM ... USING 自连接。
                db.execute_unprepared(
                    r#"DELETE FROM cluster_extensions c1
                       USING cluster_extensions c2
                       WHERE c1.kind = c2.kind AND c1.name = c2.name
                         AND (c1.updated_at < c2.updated_at
                              OR (c1.updated_at = c2.updated_at AND c1.id < c2.id))"#,
                )
                .await?;
                db.execute_unprepared(
                    r#"DELETE FROM cluster_extension_files f1
                       USING cluster_extension_files f2
                       WHERE f1.extension_id = f2.extension_id AND f1.rel_path = f2.rel_path
                         AND f1.id > f2.id"#,
                )
                .await?;
            }
        }

        // 2. 删旧普通索引(PG IF EXISTS 幂等;MySQL 无该修饰符,.ok() 容错索引不存在)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared("DROP INDEX idx_ext_kind_name ON cluster_extensions")
                    .await
                    .ok();
                db.execute_unprepared("DROP INDEX idx_extfile_ext ON cluster_extension_files")
                    .await
                    .ok();
            }
            _ => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_ext_kind_name")
                    .await?;
                db.execute_unprepared("DROP INDEX IF EXISTS idx_extfile_ext")
                    .await?;
            }
        }

        // 3. 建 UNIQUE 索引(PG IF NOT EXISTS;MySQL 无,依赖迁移顺序+新索引名保证不冲突)。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared(
                    "CREATE UNIQUE INDEX idx_ext_kind_name_unique ON cluster_extensions (kind, name)",
                )
                .await?;
                db.execute_unprepared(
                    "CREATE UNIQUE INDEX idx_extfile_ext_rel_unique ON cluster_extension_files (extension_id, rel_path)",
                )
                .await?;
            }
            _ => {
                db.execute_unprepared(
                    "CREATE UNIQUE INDEX IF NOT EXISTS idx_ext_kind_name_unique ON cluster_extensions (kind, name)",
                )
                .await?;
                db.execute_unprepared(
                    "CREATE UNIQUE INDEX IF NOT EXISTS idx_extfile_ext_rel_unique ON cluster_extension_files (extension_id, rel_path)",
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        // down:仅 DROP 两个 unique 索引(不重建 000001 的普通索引,回滚后回到"无约束"中间态;
        // 如需完全回滚应连 000001 一起 down)。PG IF EXISTS;MySQL .ok() 容错。
        match backend {
            DbBackend::MySql => {
                db.execute_unprepared("DROP INDEX idx_ext_kind_name_unique ON cluster_extensions")
                    .await
                    .ok();
                db.execute_unprepared(
                    "DROP INDEX idx_extfile_ext_rel_unique ON cluster_extension_files",
                )
                .await
                .ok();
            }
            _ => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_ext_kind_name_unique")
                    .await?;
                db.execute_unprepared("DROP INDEX IF EXISTS idx_extfile_ext_rel_unique")
                    .await?;
            }
        }
        Ok(())
    }
}

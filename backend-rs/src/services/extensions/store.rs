//! 集群扩展 PG 存取：扩展清单 / 文件指纹 / 持有节点 三表的 CRUD。
//!
//! 供 Task 6（skill 上传 API）登记扩展与指纹、Task 8（同步循环）查询清单与持有者调用。
//! 三张表均为集群级（无 team_id），由 Task 1 migration 建立。

use crate::db::entities::cluster_extension::{
    ActiveModel as ExtActive, Column as ExtCol, Entity as ExtEntity, Model as ExtModel,
};
use crate::db::entities::cluster_extension_file::{
    ActiveModel as FileActive, Column as FileCol, Entity as FileEntity,
};
use crate::db::entities::cluster_extension_holder::{
    ActiveModel as HolderActive, Column as HCol, Entity as HolderEntity,
};
use crate::error::AppError;
use crate::services::extensions::fingerprint::FileFingerprint;
use crate::services::multitenant::now_ms;
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set, TransactionTrait,
};
use std::sync::atomic::{AtomicI64, Ordering};

/// 扩展清单记录（cluster_extensions 的业务视图，剥离可选展示字段）。
#[derive(Clone, Debug)]
pub struct ExtRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub content_form: String,
    pub content_hash: String,
    pub enabled: bool,
}

impl From<ExtModel> for ExtRecord {
    fn from(m: ExtModel) -> Self {
        Self {
            id: m.id,
            kind: m.kind,
            name: m.name,
            content_form: m.content_form,
            content_hash: m.content_hash,
            enabled: m.enabled,
        }
    }
}

/// 进程内单调递增序号,用于生成 cluster_extension_files.id。
///
/// 首次调用惰性把基线对齐到 `now_ms()*1000`(远高于历史 id,重启不回碰),
/// 之后每次 `fetch_add(1)` → 进程内严格单调递增。
static FILE_ID_SEQ: AtomicI64 = AtomicI64::new(0);

/// 生成 cluster_extension_files.id（BIGINT 主键，应用层赋值）。
///
/// 项目所有 BIGINT 主键均非 DB 自增（migration: `BIGINT PRIMARY KEY NOT NULL`，
/// 无 SERIAL/IDENTITY），必须应用层赋值；本表是项目首个 BIGINT 主键表，无既有先例
/// （audit_log 等主键均为 VARCHAR UUID，用 new_id()）。
///
/// 采用进程内 `AtomicI64` 单调递增生成器（替代早期 `now_ms()*1000+idx`）：
/// - 批次内、跨批次、同毫秒并发 upsert 均唯一（原子 fetch_add 保证）；
/// - 单调递增 → B-tree 插入友好；
/// - 进程内唯一：本表只由 skill 上传节点单进程写入，其余节点只读 + 本地落盘，
///   无跨进程并发写，故不需全局协调；
/// - 惰性初始化基线为 `now_ms()*1000`，重启后远高于历史 id，绝不碰撞。
/// 不用 `max(id)+1`：多一次 DB 往返、并发竞态、需额外查询。
fn next_file_id() -> i64 {
    // 首次调用：惰性把基线对齐到当前毫秒*1000（compare_exchange 保证只设一次）。
    if FILE_ID_SEQ.load(Ordering::Relaxed) == 0 {
        let init = now_ms() * 1000;
        let _ = FILE_ID_SEQ.compare_exchange(0, init, Ordering::SeqCst, Ordering::Relaxed);
    }
    FILE_ID_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 插入或更新扩展（连同文件指纹全量替换）。
///
/// - 扩展存在：update，保留原 created_at；
/// - 扩展不存在：insert；
/// - 文件指纹：先按 extension_id 全删，再 insert_many（全量替换）。
///
/// 整个流程包在单个 sea-orm 事务内（find/update-or-insert + delete_many + insert_many
/// 共用同一 `txn`）。PG READ COMMITTED 下，其他事务在 commit 前读不到中间态
/// （即「文件已删但未插回」的空窗），避免并发 `get_files` 误判扩展为空。
/// 任一步失败 → 事务回滚，不留半成品。
pub async fn upsert_extension(
    db: &DatabaseConnection,
    rec: &ExtRecord,
    files: &[FileFingerprint],
) -> Result<(), AppError> {
    // 事务闭包返回的 future 生命周期绑定到 txn('c),不能借用函数参数(rec/files 的 &) ——
    // 否则会被要求 'static。提前 clone 成 owned 再 move 进闭包,future 内除 txn 外只持有 owned 数据。
    let rec = rec.clone();
    let files: Vec<FileFingerprint> = files.to_vec();
    db.transaction::<_, (), AppError>(move |txn| {
        Box::pin(async move {
            let now = now_ms();
            let existing = ExtEntity::find_by_id(rec.id.clone())
                .one(txn)
                .await
                .map_err(|e| AppError::internal(format!("db: {e}")))?;
            let active = ExtActive {
                id: Set(rec.id.clone()),
                kind: Set(rec.kind.clone()),
                name: Set(rec.name.clone()),
                display_name: Set(None),
                description: Set(None),
                version: Set(None),
                content_form: Set(rec.content_form.clone()),
                config_text: Set(None),
                content_hash: Set(rec.content_hash.clone()),
                enabled: Set(rec.enabled),
                created_at: Set(existing.as_ref().map(|m| m.created_at).unwrap_or(now)),
                updated_at: Set(now),
                created_by: Set(None),
            };
            if existing.is_some() {
                ExtEntity::update(active)
                    .exec(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("db: {e}")))?;
            } else {
                ExtEntity::insert(active)
                    .exec(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("db: {e}")))?;
            }

            // 文件指纹全量替换：先按 extension_id 删，再 insert_many（同一 txn）。
            FileEntity::delete_many()
                .filter(FileCol::ExtensionId.eq(rec.id.clone()))
                .exec(txn)
                .await
                .map_err(|e| AppError::internal(format!("db: {e}")))?;
            if !files.is_empty() {
                // 每行用进程内单调递增 next_file_id() 赋主键，批次内/跨批次/同毫秒并发均唯一。
                let rows: Vec<FileActive> = files
                    .iter()
                    .map(|f| FileActive {
                        id: Set(next_file_id()),
                        extension_id: Set(rec.id.clone()),
                        rel_path: Set(f.rel_path.clone()),
                        size_bytes: Set(f.size),
                        content_hash: Set(f.sha256.clone()),
                        is_binary: Set(f.is_binary),
                    })
                    .collect();
                FileEntity::insert_many(rows)
                    .exec(txn)
                    .await
                    .map_err(|e| AppError::internal(format!("db: {e}")))?;
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| AppError::internal(format!("upsert tx: {e}")))?;
    Ok(())
}

/// 列出所有 enabled=true 的扩展（按 name 升序，保证输出顺序稳定）。
pub async fn list_enabled(db: &DatabaseConnection) -> Result<Vec<ExtRecord>, AppError> {
    let rows = ExtEntity::find()
        .filter(ExtCol::Enabled.eq(true))
        .order_by_asc(ExtCol::Name)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(ExtRecord::from).collect())
}

/// 取某扩展的全部文件指纹（按 rel_path 升序，同步循环比对本地落盘用）。
pub async fn get_files(
    db: &DatabaseConnection,
    ext_id: &str,
) -> Result<Vec<FileFingerprint>, AppError> {
    let rows = FileEntity::find()
        .filter(FileCol::ExtensionId.eq(ext_id.to_string()))
        .order_by_asc(FileCol::RelPath)
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|m| FileFingerprint {
            rel_path: m.rel_path,
            size: m.size_bytes,
            sha256: m.content_hash,
            is_binary: m.is_binary,
        })
        .collect())
}

/// 登记持有节点（复合主键 extension_id+node_id，重复冲突忽略）。
///
/// `on_do_nothing` 在 PG 生成 `ON CONFLICT DO NOTHING`：复合主键重复 → 不插入、不报错。
/// 其他真实 DB 错误会被 `.ok()` 吞掉（best-effort 登记，不阻断同步主流程）。
pub async fn add_holder(
    db: &DatabaseConnection,
    ext_id: &str,
    node_id: &str,
) -> Result<(), AppError> {
    HolderEntity::insert(HolderActive {
        extension_id: Set(ext_id.to_string()),
        node_id: Set(node_id.to_string()),
        held_since: Set(now_ms()),
    })
    // ON CONFLICT DO NOTHING（不指定冲突目标，PG 下对复合主键重复也忽略）。
    .on_conflict(OnConflict::new().do_nothing().to_owned())
    .exec(db)
    .await
    .ok();
    Ok(())
}

/// 列出某扩展的全部持有节点 id。
pub async fn list_holders(
    db: &DatabaseConnection,
    ext_id: &str,
) -> Result<Vec<String>, AppError> {
    let rows = HolderEntity::find()
        .filter(HCol::ExtensionId.eq(ext_id.to_string()))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(|m| m.node_id).collect())
}

/// 删除扩展：先删文件指纹 + 持有者（从表 best-effort），再删清单主行。
pub async fn delete_extension(db: &DatabaseConnection, ext_id: &str) -> Result<(), AppError> {
    let _ = FileEntity::delete_many()
        .filter(FileCol::ExtensionId.eq(ext_id.to_string()))
        .exec(db)
        .await;
    let _ = HolderEntity::delete_many()
        .filter(HCol::ExtensionId.eq(ext_id.to_string()))
        .exec(db)
        .await;
    ExtEntity::delete_by_id(ext_id.to_string())
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{next_file_id, ExtRecord};
    use crate::db::entities::cluster_extension::Model as ExtModel;

    // 说明:brief 建议 sea-orm MockDatabase 校验 SQL 形态,但 sea-orm 的 mock feature 与
    // 项目 `DatabaseConnection: Clone` 互斥 —— sea-orm 源码 db_connection.rs:
    //   `#[cfg_attr(not(feature = "mock"), derive(Clone))] pub enum DatabaseConnection`
    // 即开启 mock 后 DatabaseConnection 不再 Clone,而 AppState/RealtimeState 均 derive(Clone)
    // 且含 db 字段,全局开 mock 会让 cargo test 编译整个 lib 失败。故改用纯函数测试覆盖
    // ExtRecord 映射(list_enabled 内部依赖)与 file id 生成,SQL 形态留 Task 9 端到端验证。

    /// 校验 list_enabled 内部依赖的 ExtModel→ExtRecord 转换:不 panic + 6 业务字段对齐 +
    /// 展示字段(display_name/description/version/config_text/created_by)被正确剥离。
    #[test]
    fn ext_record_from_maps_six_business_fields() {
        let m = ExtModel {
            id: "ext-1".into(),
            kind: "skill".into(),
            name: "alpha".into(),
            display_name: Some("展示名".into()),
            description: Some("说明".into()),
            version: Some("1.2.3".into()),
            content_form: "dir".into(),
            config_text: Some("{...}".into()),
            content_hash: "abc123".into(),
            enabled: true,
            created_at: 10,
            updated_at: 20,
            created_by: Some("u-1".into()),
        };
        let r: ExtRecord = m.into();
        assert_eq!(r.id, "ext-1");
        assert_eq!(r.kind, "skill");
        assert_eq!(r.name, "alpha");
        assert_eq!(r.content_form, "dir");
        assert_eq!(r.content_hash, "abc123");
        assert!(r.enabled);
        // ExtRecord 仅 6 字段,不含 display_name/description/version 等 → 无从断言它们。
    }

    /// next_file_id 必须严格单调递增 → 同毫秒并发 / 批次内 / 跨批次 id 均唯一,不碰主键。
    #[test]
    fn next_file_id_is_strictly_monotonic_and_batch_unique() {
        let a = next_file_id();
        let b = next_file_id();
        let c = next_file_id();
        assert!(b > a, "next_file_id 必须严格单调递增 (a={a}, b={b})");
        assert!(c > b, "next_file_id 必须严格单调递增 (b={b}, c={c})");

        // 模拟一次 upsert_extension 批次内取 N 个 id:全部唯一。
        let batch: Vec<i64> = (0..50).map(|_| next_file_id()).collect();
        let mut sorted = batch.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 50, "批次内 50 个 id 必须唯一");
    }
}

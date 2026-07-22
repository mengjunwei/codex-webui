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
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

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

/// 生成 cluster_extension_files.id（BIGINT 主键，应用层赋值）。
///
/// 项目所有 BIGINT 主键均非 DB 自增（migration: `BIGINT PRIMARY KEY NOT NULL`，
/// 无 SERIAL/IDENTITY），必须应用层赋值；本表是项目首个 BIGINT 主键表，无既有先例
/// （audit_log 等主键均为 VARCHAR UUID，用 new_id()）。
///
/// 采用 `now_ms() * 1000 + 序号`：
/// - 复用项目通用 i64 生成器 now_ms（与 created_at/updated_at 同源），风格一致；
/// - 单调递增，B-tree 插入友好；
/// - 批次内（一个扩展的 N 个文件，N << 1000）序号唯一 → 主键唯一；
/// - 跨批次：upsert_extension 经 await 串行，不同扩展基数至少差 1ms；
/// - 跨节点：本表只由 skill 上传节点（Task 6）写入，其余节点只读 + 本地落盘，
///   无跨节点并发写。
/// 不用 `max(id)+1`：多一次 DB 往返、并发竞态、需额外查询。
fn file_id(base_ms: i64, idx: usize) -> i64 {
    base_ms * 1000 + idx as i64
}

/// 插入或更新扩展（连同文件指纹全量替换）。
///
/// - 扩展存在：update，保留原 created_at；
/// - 扩展不存在：insert；
/// - 文件指纹：先按 extension_id 全删，再 insert_many（全量替换）。
pub async fn upsert_extension(
    db: &DatabaseConnection,
    rec: &ExtRecord,
    files: &[FileFingerprint],
) -> Result<(), AppError> {
    let now = now_ms();
    let existing = ExtEntity::find_by_id(rec.id.clone())
        .one(db)
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
            .exec(db)
            .await
            .map_err(|e| AppError::internal(format!("db: {e}")))?;
    } else {
        ExtEntity::insert(active)
            .exec(db)
            .await
            .map_err(|e| AppError::internal(format!("db: {e}")))?;
    }

    // 文件指纹全量替换：先按 extension_id 删，再 insert_many。
    FileEntity::delete_many()
        .filter(FileCol::ExtensionId.eq(rec.id.clone()))
        .exec(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    if !files.is_empty() {
        let rows: Vec<FileActive> = files
            .iter()
            .enumerate()
            .map(|(i, f)| FileActive {
                id: Set(file_id(now, i)),
                extension_id: Set(rec.id.clone()),
                rel_path: Set(f.rel_path.clone()),
                size_bytes: Set(f.size),
                content_hash: Set(f.sha256.clone()),
                is_binary: Set(f.is_binary),
            })
            .collect();
        FileEntity::insert_many(rows)
            .exec(db)
            .await
            .map_err(|e| AppError::internal(format!("db: {e}")))?;
    }
    Ok(())
}

/// 列出所有 enabled=true 的扩展。
pub async fn list_enabled(db: &DatabaseConnection) -> Result<Vec<ExtRecord>, AppError> {
    let rows = ExtEntity::find()
        .filter(ExtCol::Enabled.eq(true))
        .all(db)
        .await
        .map_err(|e| AppError::internal(format!("db: {e}")))?;
    Ok(rows.into_iter().map(ExtRecord::from).collect())
}

/// 取某扩展的全部文件指纹（同步循环比对本地落盘用）。
pub async fn get_files(
    db: &DatabaseConnection,
    ext_id: &str,
) -> Result<Vec<FileFingerprint>, AppError> {
    let rows = FileEntity::find()
        .filter(FileCol::ExtensionId.eq(ext_id.to_string()))
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

//! Startup reconcile: ensure every `SETTINGS_DEFINITIONS` entry exists in the
//! `settings` table. Existing rows are NOT overwritten (user values preserved);
//! only the metadata columns (`type`, `category`, `description`, `default_value`,
//! `constraints`, `updated_at`) are updated to the current definition.
//!
//! Parity with `src/settings/settings.service.ts` reconcile logic.
//!
//! SeaORM async(多方言 PG/MySQL)。跨方言一致性:用"先查后插/更新",避免 ON CONFLICT
//! 方言差异。整个流程包在单个事务里(对齐 TS 的 db.transaction)。

use crate::db::entity::setting::{
    ActiveModel as SettingActiveModel, Entity as SettingEntity,
};
use crate::services::multitenant::now_ms;
use crate::services::settings::definitions::{SettingDef, SettingType, SETTINGS_DEFINITIONS};
use anyhow::Result;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};

/// 应用所有 SETTINGS_DEFINITIONS 到 settings 表:行缺失则插入(无 value),元数据变化则更新。
pub async fn reconcile_settings(db: &DatabaseConnection) -> Result<()> {
    db.transaction::<_, _, sea_orm::DbErr>(|txn| {
        Box::pin(async move {
            for def in SETTINGS_DEFINITIONS {
                let constraints_json = def.constraints.to_json();
                let constraints_str = serde_json::to_string(&constraints_json)
                    .unwrap_or_else(|_| "{}".to_string());

                // INSERT if row missing — preserves any existing user value。
                let existing = SettingEntity::find_by_id(def.key).one(txn).await?;
                if existing.is_none() {
                    SettingActiveModel {
                        key: Set(def.key.to_string()),
                        value: Set(None),
                        r#type: Set(def.ty.as_str().to_string()),
                        category: Set(def.category.as_str().to_string()),
                        description: Set(def.description.to_string()),
                        default_value: Set(def.default_value.to_string()),
                        constraints: Set(constraints_str.clone()),
                        updated_at: Set(now_ms()),
                    }
                    .insert(txn)
                    .await?;
                    continue;
                }

                // UPDATE metadata + constraints — never touches `value`。
                // 仅当元数据实际变化时才 UPDATE（对齐 TS hasMetadataChanged），避免无谓写。
                let cur = existing.unwrap();
                let changed = cur.r#type != def.ty.as_str()
                    || cur.category != def.category.as_str()
                    || cur.description != def.description
                    || cur.default_value != def.default_value
                    || cur.constraints != constraints_str;
                if changed {
                    let mut am: SettingActiveModel = cur.into();
                    am.r#type = Set(def.ty.as_str().to_string());
                    am.category = Set(def.category.as_str().to_string());
                    am.description = Set(def.description.to_string());
                    am.default_value = Set(def.default_value.to_string());
                    am.constraints = Set(constraints_str);
                    am.updated_at = Set(now_ms());
                    am.update(txn).await?;
                }
            }
            // 显式忽略 Expr 的 unused 警告（reconcile 自身不依赖 Expr，使用 update_many 时再用到）。
            let _ = Expr::value(0i64);
            Ok(())
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("reconcile settings: {e}"))?;
    tracing::info!(
        "reconciled {} settings definitions",
        SETTINGS_DEFINITIONS.len()
    );
    Ok(())
}

// 抑制 unused 警告(SettingType 引用确保 definitions 模块链接)。
#[allow(dead_code)]
fn _ensure_typed(def: &SettingDef) -> &str {
    match def.ty {
        SettingType::Number | SettingType::String | SettingType::Boolean | SettingType::Json => {
            def.ty.as_str()
        }
    }
}

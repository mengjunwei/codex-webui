//! tool_policies 表 SeaORM entity。
//! 字段对齐 migration SQL:VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT。

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
#[sea_orm(table_name = "tool_policies")]
pub struct Model {
    #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
    pub id: String,
    #[sea_orm(column_type = "String(StringLen::N(16))")]
    pub scope: String,
    #[sea_orm(column_type = "String(StringLen::N(36))", nullable)]
    pub team_id: Option<String>,
    #[sea_orm(column_type = "String(StringLen::N(16))", nullable)]
    pub role: Option<String>,
    #[sea_orm(column_type = "String(StringLen::N(16))")]
    pub rule_type: String,
    #[sea_orm(column_type = "String(StringLen::N(16))")]
    pub match_mode: String,
    #[sea_orm(column_type = "Text")]
    pub pattern: String,
    #[sea_orm(column_type = "String(StringLen::N(16))")]
    pub action: String,
    pub priority: i32,
    pub enabled: bool,
    #[sea_orm(column_type = "Text", nullable)]
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

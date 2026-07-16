//! 业务表 SeaORM entity(5 表)。字段对齐 migration m20260716_0004_business。
//! 注:settings 表 DB 列名用 `setting_key`(避免 MySQL 保留字 `key`),Rust 字段名 `key` 经 column_name 映射。

/// token 用量快照(每 turn 一行,upsert)。复合主键(thread_id, turn_id)。
pub mod token_usage_snapshot {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "token_usage_snapshots")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(64))")]
        pub turn_id: String,
        pub total_tokens: i64,
        pub input_tokens: i64,
        pub cached_input_tokens: i64,
        pub output_tokens: i64,
        pub reasoning_output_tokens: i64,
        pub last_total_tokens: i64,
        pub last_input_tokens: i64,
        pub last_cached_input_tokens: i64,
        pub last_output_tokens: i64,
        pub last_reasoning_output_tokens: i64,
        pub model_context_window: Option<i64>,
        #[sea_orm(column_type = "Text")]
        pub raw_payload: String,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// turn diff(每 turn 一行,upsert)。复合主键(thread_id, turn_id)。
pub mod turn_diff {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "turn_diffs")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(64))")]
        pub turn_id: String,
        #[sea_orm(column_type = "Text")]
        pub diff: String,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 运行时设置(key/value)。`setting_key` 列避免 MySQL 保留字 `key`。
pub mod setting {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "settings")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(128))", column_name = "setting_key")]
        pub key: String,
        #[sea_orm(column_type = "Text")]
        pub value: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub r#type: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub category: String,
        #[sea_orm(column_type = "Text")]
        pub description: String,
        #[sea_orm(column_type = "Text")]
        pub default_value: String,
        #[sea_orm(column_type = "Text")]
        pub constraints: String,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 待处理的服务端请求(审批)。复合主键(generation, request_id)。
pub mod pending_server_request {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "pending_server_requests")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub generation: i64,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(64))")]
        pub request_id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub turn_id: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub item_id: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub method: String,
        #[sea_orm(column_type = "Text")]
        pub params_json: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub status: String,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub resolved_by: Option<String>,
        pub created_at: i64,
        pub updated_at: i64,
        pub resolved_at: Option<i64>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// turn 错误(每 turn 一行)。复合主键(thread_id, turn_id)。
pub mod turn_error {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "turn_errors")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(64))")]
        pub turn_id: String,
        #[sea_orm(column_type = "Text")]
        pub message: String,
        pub created_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

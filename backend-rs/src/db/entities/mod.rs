//! 多租户 SeaORM entity(8 表)。每表一个子模块,避免 Relation/Column 类型名冲突。
//! 字段对齐 migration SQL:VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT。

/// 全局用户账号(邮箱 + 密码)。一人可属于多个 team。
pub mod user {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "users")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(255))")]
        pub email: String,
        #[sea_orm(column_type = "String(StringLen::N(255))")]
        pub password_hash: String,
        pub email_verified_at: Option<i64>,
        pub display_name: Option<String>,
        pub created_at: i64,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// team:多租户隔离边界 + codex 账号共用单元(BYOK)。
pub mod team {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "teams")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(255))")]
        pub name: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub owner_id: String,
        pub created_at: i64,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// team 成员关系(多对多)。复合主键(team_id, user_id)。
pub mod team_member {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "team_members")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub user_id: String,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub role: String,
        pub joined_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 邀请码:owner 生成,他人凭码加入 team。
pub mod invitation {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "invitations")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub code: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub created_by: String,
        pub expires_at: Option<i64>,
        pub max_uses: Option<i32>,
        pub used_count: i32,
        pub created_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// refresh token:JWT 续期,存哈希,支持撤销与一次性轮转。
pub mod refresh_token {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "refresh_tokens")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub user_id: String,
        #[sea_orm(column_type = "String(StringLen::N(255))")]
        pub token_hash: String,
        pub expires_at: i64,
        pub revoked: bool,
        pub created_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 会话元数据(rollout 内容在 worker 本地 CODEX_HOME;此处仅元数据)。
pub mod thread {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "threads")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub created_by_user_id: String,
        pub title: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub status: String,
        pub created_at: i64,
        pub updated_at: i64,
        pub last_activity_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// team 的 OpenAI API key(BYOK)。encrypted_key 为 AES-GCM 密文(hex)。
pub mod team_api_key {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "team_api_keys")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub provider: String,
        #[sea_orm(column_type = "Text")]
        pub encrypted_key: String,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub key_hint: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub set_by: String,
        pub is_active: bool,
        pub created_at: i64,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 审计日志:team owner 关键操作记录。
pub mod audit_log {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "audit_log")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub actor_user_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub action: String,
        #[sea_orm(column_type = "Text")]
        pub detail: Option<String>,
        pub created_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 计费/配额(M6 预留):per-team 配额上限 + 滑动用量。0 = 不限。
pub mod team_quota {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "team_quotas")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub plan: String,
        pub turn_quota_hourly: i64,
        pub token_quota_monthly: i64,
        pub used_turns_hour: i64,
        pub hour_bucket: i64,
        pub used_tokens_month: i64,
        #[sea_orm(column_type = "String(StringLen::N(7))")]
        pub month_bucket: String,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// team→worker 路由覆盖(M4 failover 决策记录,防节点抖动回切)。
pub mod team_route {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "team_routes")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub worker_id: String,
        pub mapped_at: i64,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub mapped_reason: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// per-team 主副本映射(active-passive HA):team_id → primary_node + replica_node。
pub mod session_replica {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "session_replicas")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub team_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub primary_node: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub replica_node: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub status: String,
        pub primary_lease_until: i64,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

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
        /// 平台超级管理员标记(可改全局配置/读全局日志)。默认 false。
        pub is_platform_admin: bool,
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
        /// workspace 归属类型:"personal"(个人 workspace) / "team"(团队 workspace)。
        #[sea_orm(column_type = "String(StringLen::N(8))")]
        pub workspace_type: String,
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

/// per-user workspace 审计:记录用户切换/创建 workspace 的事件。
pub mod thread_resume_cache {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "thread_resume_cache")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(column_type = "Json")]
        pub response: serde_json::Value,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 用户个人 OpenAI API key(BYOK)。encrypted_key 为 AES-GCM 密文(hex)。
pub mod user_api_key {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "user_api_keys")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub user_id: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub provider: String,
        #[sea_orm(column_type = "Text")]
        pub encrypted_key: String,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub key_hint: String,
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

/// per-thread 主副本映射(active-passive HA):thread_id → primary_node + replica_node。
pub mod session_replica {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "session_replicas")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
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

/// 角色权限映射(全局,无 team_id)。seed 由 migration 写入(spec §4.1 矩阵)。
pub mod role_permission {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "role_permissions")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(16))")]
        pub role: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(48))")]
        pub permission: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 集群扩展清单。
pub mod cluster_extension {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extensions")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub id: String,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub kind: String,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub name: String,
        #[sea_orm(column_type = "String(StringLen::N(256))", nullable)]
        pub display_name: Option<String>,
        #[sea_orm(column_type = "Text", nullable)]
        pub description: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(64))", nullable)]
        pub version: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub content_form: String,
        #[sea_orm(column_type = "Text", nullable)]
        pub config_text: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub content_hash: String,
        pub enabled: bool,
        pub created_at: i64,
        pub updated_at: i64,
        #[sea_orm(column_type = "String(StringLen::N(36))", nullable)]
        pub created_by: Option<String>,
        /// plugin 的市场名(skill/mcp 为空)。plugin 分发按此列检索目标市场。
        #[sea_orm(column_type = "String(StringLen::N(128))", nullable)]
        pub marketplace: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 扩展文件指纹(无内容)。
pub mod cluster_extension_file {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extension_files")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        #[sea_orm(column_type = "String(StringLen::N(36))")]
        pub extension_id: String,
        #[sea_orm(column_type = "String(StringLen::N(512))")]
        pub rel_path: String,
        pub size_bytes: i64,
        #[sea_orm(column_type = "String(StringLen::N(128))")]
        pub content_hash: String,
        pub is_binary: bool,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 扩展持有节点。
pub mod cluster_extension_holder {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "cluster_extension_holders")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub extension_id: String,
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub node_id: String,
        pub held_since: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 工具策略表:命令审查与 skill/plugin/mcp 使用限制。
pub mod tool_policy {
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
}


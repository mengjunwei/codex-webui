//! 多租户核心实体的 sqlx 行映射(FromRow)。
//!
//! 字段类型对应迁移 SQL 中的列;时间统一 i64 UTC 毫秒,主键 VARCHAR(36) UUIDv7。

use sqlx::FromRow;

/// 全局用户账号(邮箱 + 密码)。一人可属于多个 team(见 TeamMember)。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    pub email_verified_at: Option<i64>,
    pub display_name: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// team:多租户隔离边界 + codex 账号共用单元(BYOK)。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    /// 创建者(也是 owner)。
    pub owner_id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// team 成员关系(多对多:一人多 team)。role ∈ {owner, member}。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TeamMember {
    pub team_id: String,
    pub user_id: String,
    pub role: String,
    pub joined_at: i64,
}

/// 邀请码:owner 生成,他人凭码加入 team 成为 member。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Invitation {
    pub id: String,
    pub team_id: String,
    pub code: String,
    pub created_by: String,
    pub expires_at: Option<i64>,
    pub max_uses: Option<i32>,
    pub used_count: i32,
    pub created_at: i64,
}

/// refresh token:JWT 续期用。存 token 哈希(不存明文),支持撤销与一次性轮转。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct RefreshToken {
    pub id: String,
    pub user_id: String,
    pub token_hash: String,
    pub expires_at: i64,
    pub revoked: bool,
    pub created_at: i64,
}

/// 会话元数据(rollout 内容在 worker 本地 CODEX_HOME;此处仅存元数据,
/// 用于跨 team 的 list/权限/路由)。team 内完全共享,仅记 created_by 用于审计。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ThreadMeta {
    pub id: String,
    pub team_id: String,
    pub created_by_user_id: String,
    pub title: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_activity_at: i64,
}

/// team 的 OpenAI API key(BYOK)。encrypted_key 为 AES-GCM 密文(hex),
/// key_hint 为尾 4 位用于显示。一 team 同时仅一条 is_active(应用层事务保证)。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TeamApiKey {
    pub id: String,
    pub team_id: String,
    pub provider: String,
    pub encrypted_key: String,
    pub key_hint: String,
    pub set_by: String,
    pub is_active: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 审计日志(M6):team owner 关键操作记录。
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct AuditLog {
    pub id: String,
    pub team_id: String,
    pub actor_user_id: String,
    pub action: String,
    pub detail: Option<String>,
    pub created_at: i64,
}

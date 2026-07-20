//! RBAC 权限点系统 + 平台管理员字段。
//!
//! - users.is_platform_admin:平台超级管理员(可改全局配置/读全局日志)。
//! - team_members.role CHECK:只允许 owner/admin/member(消除幽灵角色)。
//! - role_permissions:角色→权限点映射,seed 三角色矩阵(spec §4.1)。

use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260720_000001_rbac_permissions"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // 1. users.is_platform_admin(全新列;默认 false)。
        db.execute_unprepared(
            r#"ALTER TABLE users ADD COLUMN is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE"#,
        ).await?;
        // COMMENT 仅 PG;MySQL 无 COMMENT ON COLUMN,.ok() 容错。
        let _ = db.execute_unprepared(
            "COMMENT ON COLUMN users.is_platform_admin IS '平台超级管理员标记（可改全局配置/读全局日志）';"
        ).await;

        // 2. team_members.role CHECK 约束(PG/MySQL 8.0+ 强制;5.7 忽略,应用层亦有校验)。
        db.execute_unprepared(
            r#"ALTER TABLE team_members ADD CONSTRAINT team_members_role_chk
               CHECK (role IN ('owner','admin','member'))"#,
        ).await?;

        // 3. role_permissions 表(全局,无 team_id)。
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS role_permissions (
                role VARCHAR(16) NOT NULL,
                permission VARCHAR(48) NOT NULL,
                PRIMARY KEY (role, permission)
            )"#,
        ).await?;

        // 4. seed 角色权限矩阵(spec §4.1)。
        //    owner=全权限; admin=owner 减 transfer/dissolve/role:write; member=4 个基础。
        db.execute_unprepared(
            r#"INSERT INTO role_permissions (role, permission) VALUES
               ('owner','team:member:list'),
               ('owner','team:thread:create'),
               ('owner','team:thread:read'),
               ('owner','team:turn:write'),
               ('owner','team:member:invite'),
               ('owner','team:member:remove'),
               ('owner','team:member:role:write'),
               ('owner','team:api_key:read'),
               ('owner','team:api_key:write'),
               ('owner','team:audit:read'),
               ('owner','team:owner:transfer'),
               ('owner','team:dissolve'),
               ('admin','team:member:list'),
               ('admin','team:thread:create'),
               ('admin','team:thread:read'),
               ('admin','team:turn:write'),
               ('admin','team:member:invite'),
               ('admin','team:member:remove'),
               ('admin','team:api_key:read'),
               ('admin','team:api_key:write'),
               ('admin','team:audit:read'),
               ('member','team:member:list'),
               ('member','team:thread:create'),
               ('member','team:thread:read'),
               ('member','team:turn:write')"#,
        ).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(r#"DROP TABLE IF EXISTS role_permissions"#).await?;
        db.execute_unprepared(
            r#"ALTER TABLE team_members DROP CONSTRAINT IF EXISTS team_members_role_chk"#,
        ).await?;
        db.execute_unprepared(r#"ALTER TABLE users DROP COLUMN is_platform_admin"#).await?;
        Ok(())
    }
}

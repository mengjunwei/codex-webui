-- ============================================================
-- Codex WebUI 数据库初始化（MySQL）
-- 来源:backend-rs/src/db/migration/ 下 7 个 SeaORM 迁移翻译。
-- 要求:MySQL ≥ 8.0.29(启用 CREATE TABLE IF NOT EXISTS)。
-- 用法:mysql -D <db> < init.sql
-- 幂等:所有 CREATE TABLE 使用 IF NOT EXISTS,可重跑。
-- 警告:假定全新空库;MySQL 不支持 ALTER TABLE ADD/DROP COLUMN IF [NOT] EXISTS,
--       因此本脚本对已存在表结构不会更新(假定 DBA 不会手工加列)。
-- 提示:本脚本未显式包裹 BEGIN/COMMIT;失败时可能只创建部分表,需修复后重跑。
-- ============================================================

-- ============================================================
-- 1/7  m20260719_000001_combined_schema
-- ============================================================

-- 1.1 users
CREATE TABLE IF NOT EXISTS users (
    id VARCHAR(36) PRIMARY KEY,
    email VARCHAR(255) NOT NULL UNIQUE,
    password_hash VARCHAR(255) NOT NULL,
    email_verified_at BIGINT,
    display_name VARCHAR(255),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE
    -- 列注释(MySQL 不支持 COMMENT ON COLUMN,以 SQL 注释形式内联):
    -- email = 登录邮箱(全局唯一约束)
    -- password_hash = bcrypt 哈希后的密码
    -- is_platform_admin = 平台超级管理员标记(可改全局配置/读全局日志)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE users COMMENT = '用户账号表:邮箱登录,一人可属于多个 team';

-- 1.2 teams
CREATE TABLE IF NOT EXISTS teams (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    owner_id VARCHAR(36) NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    FOREIGN KEY (owner_id) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE teams COMMENT = '团队表:多租户隔离边界 + codex 账号共用单元';

-- 1.3 team_members(已合并 rbac role CHECK 约束;MySQL 8.0.16+ CHECK 强制)
CREATE TABLE IF NOT EXISTS team_members (
    team_id VARCHAR(36) NOT NULL,
    user_id VARCHAR(36) NOT NULL,
    role VARCHAR(16) NOT NULL,
    joined_at BIGINT NOT NULL,
    PRIMARY KEY (team_id, user_id),
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT team_members_role_chk CHECK (role IN ('owner','admin','member'))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE team_members COMMENT = '团队成员关系(多对多):团队内角色(owner/admin/member)';
CREATE INDEX idx_team_members_user ON team_members (user_id);

-- 1.4 invitations
CREATE TABLE IF NOT EXISTS invitations (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL,
    code VARCHAR(64) NOT NULL UNIQUE,
    created_by VARCHAR(36) NOT NULL,
    expires_at BIGINT,
    max_uses INT,
    used_count INT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    FOREIGN KEY (created_by) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE invitations COMMENT = '邀请码:owner 生成,他人凭码加入 team';

-- 1.5 refresh_tokens
CREATE TABLE IF NOT EXISTS refresh_tokens (
    id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    token_hash VARCHAR(255) NOT NULL UNIQUE,
    expires_at BIGINT NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE refresh_tokens COMMENT = 'JWT 刷新令牌:存哈希,支持撤销与一次性轮转';

-- 1.6 threads
CREATE TABLE IF NOT EXISTS threads (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL,
    created_by_user_id VARCHAR(36) NOT NULL,
    title VARCHAR(255),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    workspace_type VARCHAR(8) NOT NULL DEFAULT 'team',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    last_activity_at BIGINT NOT NULL,
    FOREIGN KEY (created_by_user_id) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE threads COMMENT = '会话元数据:per-thread(rollout 内容在 worker 本地 CODEX_HOME)';
CREATE INDEX idx_threads_team ON threads (team_id);
CREATE INDEX idx_threads_status ON threads (team_id, status);

-- 1.7 team_api_keys
CREATE TABLE IF NOT EXISTS team_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL,
    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
    encrypted_key TEXT NOT NULL,
    key_hint VARCHAR(16) NOT NULL,
    set_by VARCHAR(36) NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    FOREIGN KEY (set_by) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE team_api_keys COMMENT = '团队 BYOK API Key:encrypted_key 为 AES-GCM 密文';
CREATE INDEX idx_team_api_keys_team ON team_api_keys (team_id, is_active);

-- 1.8 user_api_keys
CREATE TABLE IF NOT EXISTS user_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
    encrypted_key TEXT NOT NULL,
    key_hint VARCHAR(16) NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE user_api_keys COMMENT = '用户个人 BYOK API Key(personal workspace 使用)';
CREATE INDEX idx_user_api_keys_user ON user_api_keys (user_id, is_active);

-- 1.9 audit_log
CREATE TABLE IF NOT EXISTS audit_log (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL,
    actor_user_id VARCHAR(36) NOT NULL,
    action VARCHAR(64) NOT NULL,
    detail TEXT,
    created_at BIGINT NOT NULL,
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    FOREIGN KEY (actor_user_id) REFERENCES users(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE audit_log COMMENT = '审计日志:team owner 关键操作留痕(设 key / 邀请 / 踢除等)';
CREATE INDEX idx_audit_team ON audit_log (team_id, created_at DESC);

-- 1.10 token_usage_snapshots
CREATE TABLE IF NOT EXISTS token_usage_snapshots (
    thread_id VARCHAR(36) NOT NULL,
    turn_id VARCHAR(64) NOT NULL,
    team_id VARCHAR(36),
    total_tokens BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    cached_input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    reasoning_output_tokens BIGINT NOT NULL,
    last_total_tokens BIGINT NOT NULL,
    last_input_tokens BIGINT NOT NULL,
    last_cached_input_tokens BIGINT NOT NULL,
    last_output_tokens BIGINT NOT NULL,
    last_reasoning_output_tokens BIGINT NOT NULL,
    model_context_window BIGINT,
    raw_payload TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE token_usage_snapshots COMMENT = 'token 用量快照:每 turn 一行,upsert 更新';
CREATE INDEX idx_token_usage_thread_updated ON token_usage_snapshots (thread_id, updated_at);

-- 1.11 turn_diffs
CREATE TABLE IF NOT EXISTS turn_diffs (
    thread_id VARCHAR(36) NOT NULL,
    turn_id VARCHAR(64) NOT NULL,
    team_id VARCHAR(36),
    diff TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE turn_diffs COMMENT = 'turn diff:每 turn 一行,upsert 更新';
CREATE INDEX idx_turn_diffs_thread ON turn_diffs (thread_id);

-- 1.12 settings(setting_key 列避免 MySQL 保留字 key)
CREATE TABLE IF NOT EXISTS settings (
    setting_key VARCHAR(128) PRIMARY KEY NOT NULL,
    value TEXT,
    type VARCHAR(32) NOT NULL,
    category VARCHAR(64) NOT NULL,
    description TEXT NOT NULL,
    default_value TEXT NOT NULL,
    constraints TEXT NOT NULL,
    updated_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE settings COMMENT = '运行时设置:key/value 结构,供 onlyoffice 等子系统读取';
CREATE INDEX idx_settings_category ON settings (category);

-- 1.13 pending_server_requests
CREATE TABLE IF NOT EXISTS pending_server_requests (
    generation BIGINT NOT NULL,
    request_id VARCHAR(64) NOT NULL,
    thread_id VARCHAR(36) NOT NULL,
    team_id VARCHAR(36),
    turn_id VARCHAR(64),
    item_id VARCHAR(128),
    method VARCHAR(64) NOT NULL,
    params_json TEXT NOT NULL,
    status VARCHAR(32) NOT NULL,
    resolved_by VARCHAR(128),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    resolved_at BIGINT,
    PRIMARY KEY (generation, request_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE pending_server_requests COMMENT = '待处理服务端请求:codex 侧发起的审批请求';
CREATE INDEX idx_pending_requests_thread_status ON pending_server_requests (thread_id, status);
CREATE INDEX idx_pending_requests_status_updated ON pending_server_requests (status, updated_at);

-- 1.14 turn_errors
CREATE TABLE IF NOT EXISTS turn_errors (
    thread_id VARCHAR(36) NOT NULL,
    turn_id VARCHAR(64) NOT NULL,
    team_id VARCHAR(36),
    message TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE turn_errors COMMENT = 'turn 错误记录:每 turn 一行,记录错误消息';
CREATE INDEX idx_turn_errors_thread ON turn_errors (thread_id);

-- 1.15 team_quotas
CREATE TABLE IF NOT EXISTS team_quotas (
    team_id VARCHAR(36) PRIMARY KEY NOT NULL,
    plan VARCHAR(32) NOT NULL DEFAULT 'free',
    turn_quota_hourly BIGINT NOT NULL DEFAULT 0,
    token_quota_monthly BIGINT NOT NULL DEFAULT 0,
    used_turns_hour BIGINT NOT NULL DEFAULT 0,
    hour_bucket BIGINT NOT NULL DEFAULT 0,
    used_tokens_month BIGINT NOT NULL DEFAULT 0,
    month_bucket VARCHAR(7) NOT NULL DEFAULT '',
    updated_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE team_quotas COMMENT = 'per-team 配额与用量计数(turn 级别 + token 级别)';

-- 1.16 team_routes
CREATE TABLE IF NOT EXISTS team_routes (
    team_id VARCHAR(36) PRIMARY KEY NOT NULL,
    worker_id VARCHAR(64) NOT NULL,
    mapped_at BIGINT NOT NULL,
    mapped_reason VARCHAR(16) NOT NULL DEFAULT 'initial'
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE team_routes COMMENT = 'team→worker 路由覆盖(failover 决策记录,防节点抖动回切)';

-- 1.17 session_replicas(初版 per-team;后续 3/7 迁移会改为 per-thread,这里直接建最终形态)
CREATE TABLE IF NOT EXISTS session_replicas (
    thread_id VARCHAR(36) PRIMARY KEY NOT NULL,
    primary_node VARCHAR(64) NOT NULL,
    replica_node VARCHAR(64),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    primary_lease_until BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE session_replicas COMMENT = 'per-thread 主副本映射(active-passive HA):thread_id → primary + replica';

-- 1.18 workspace_audit
CREATE TABLE IF NOT EXISTS workspace_audit (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36),
    user_id VARCHAR(36),
    thread_id VARCHAR(36),
    event_type VARCHAR(64) NOT NULL,
    tool_name VARCHAR(64),
    payload_json TEXT NOT NULL,
    decision VARCHAR(16),
    created_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE workspace_audit COMMENT = 'hook 审计落库:codex 工具调用前后 webhook 推送的事件原样入库';
CREATE INDEX idx_workspace_audit_team_user_ts ON workspace_audit (team_id, user_id, created_at DESC);

-- 1.19 thread_resume_cache
CREATE TABLE IF NOT EXISTS thread_resume_cache (
    thread_id VARCHAR(36) PRIMARY KEY,
    response JSON NOT NULL,
    updated_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE thread_resume_cache COMMENT = 'thread/resume 集群共享缓存:mt_create_thread 写入,invoke resume 读取(避 codex 异步落盘 race)';
CREATE INDEX idx_thread_resume_cache_updated ON thread_resume_cache (updated_at);

-- ============================================================
-- 2/7  m20260720_000001_rbac_permissions
-- (users.is_platform_admin 已在 1.1 合并;team_members.role CHECK 已在 1.3 合并)
-- ============================================================

-- 2.1 role_permissions 表(全局,无 team_id)
CREATE TABLE IF NOT EXISTS role_permissions (
    role VARCHAR(16) NOT NULL,
    permission VARCHAR(48) NOT NULL,
    PRIMARY KEY (role, permission)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE role_permissions COMMENT = '角色→权限点映射,seed 三角色矩阵(spec §4.1)';

-- 2.2 seed 角色权限矩阵(INSERT IGNORE 幂等:重复主键静默跳过)
INSERT IGNORE INTO role_permissions (role, permission) VALUES
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
    ('member','team:turn:write');

-- ============================================================
-- 3/7  m20260721_000001_session_replicas_per_thread
-- 1.17 已直接建立 per-thread 主键的 session_replicas(最终形态),无操作。
-- ============================================================

-- ============================================================
-- 4/7  m20260722_000001_cluster_extensions
-- ============================================================

-- 4.1 cluster_extensions
CREATE TABLE IF NOT EXISTS cluster_extensions (
    id VARCHAR(36) PRIMARY KEY NOT NULL,
    kind VARCHAR(32) NOT NULL,
    name VARCHAR(128) NOT NULL,
    display_name VARCHAR(256),
    description TEXT,
    version VARCHAR(64),
    content_form VARCHAR(16) NOT NULL,
    config_text TEXT,
    content_hash VARCHAR(128) NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    created_by VARCHAR(36)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE cluster_extensions COMMENT = '集群扩展分发清单';

-- 4.2 cluster_extension_files
CREATE TABLE IF NOT EXISTS cluster_extension_files (
    id BIGINT PRIMARY KEY NOT NULL,
    extension_id VARCHAR(36) NOT NULL,
    rel_path VARCHAR(512) NOT NULL,
    size_bytes BIGINT NOT NULL,
    content_hash VARCHAR(128) NOT NULL,
    is_binary BOOLEAN NOT NULL DEFAULT FALSE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE cluster_extension_files COMMENT = '扩展文件指纹(无内容)';

-- 4.3 cluster_extension_holders
CREATE TABLE IF NOT EXISTS cluster_extension_holders (
    extension_id VARCHAR(36) NOT NULL,
    node_id VARCHAR(36) NOT NULL,
    held_since BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
ALTER TABLE cluster_extension_holders COMMENT = '扩展持有节点(去单点)';

-- 4.4 普通索引(后被 5/7 改为 UNIQUE)
CREATE INDEX idx_ext_kind_name ON cluster_extensions (kind, name);
CREATE INDEX idx_ext_enabled ON cluster_extensions (enabled);
CREATE INDEX idx_extfile_ext ON cluster_extension_files (extension_id);

-- ============================================================
-- 5/7  m20260722_000002_cluster_extensions_unique
-- 新建库无数据,无需 DELETE 去重;直接 DROP 旧普通索引 + CREATE UNIQUE 索引。
-- (MySQL 不支持 DROP INDEX IF EXISTS,首次执行索引存在;若残留会失败,见 README)
-- ============================================================
DROP INDEX idx_ext_kind_name ON cluster_extensions;
DROP INDEX idx_extfile_ext ON cluster_extension_files;
CREATE UNIQUE INDEX idx_ext_kind_name_unique ON cluster_extensions (kind, name);
CREATE UNIQUE INDEX idx_extfile_ext_rel_unique ON cluster_extension_files (extension_id, rel_path);

-- ============================================================
-- 6/7  m20260722_000003_cluster_extensions_marketplace
-- MySQL 不支持 ADD COLUMN IF NOT EXISTS;全新库假定 marketplace 列不存在。
-- ============================================================
ALTER TABLE cluster_extensions ADD COLUMN marketplace VARCHAR(128);
CREATE INDEX idx_ext_marketplace ON cluster_extensions (marketplace);
ALTER TABLE cluster_extensions MODIFY COLUMN marketplace VARCHAR(128) COMMENT 'plugin 的市场名(skill/mcp 为空)';

-- ============================================================
-- 7/7  m20260722_000004_cluster_extensions_holder_pk
-- 新建库无数据,无需 DELETE 去重;直接 ADD 复合主键(MySQL 主键无名)。
-- ============================================================
ALTER TABLE cluster_extension_holders ADD PRIMARY KEY (extension_id, node_id);

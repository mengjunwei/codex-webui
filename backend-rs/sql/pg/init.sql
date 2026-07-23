-- ============================================================
-- Codex WebUI 数据库初始化（PostgreSQL）
-- 来源:backend-rs/src/db/migration/ 下 7 个 SeaORM 迁移翻译。
-- 要求:PostgreSQL ≥ 13。
-- 用法:psql -d <db> -f init.sql
-- 幂等:所有 DDL 使用 IF NOT EXISTS,可重跑。
-- 警告:假定全新空库;不修改已存在表的列/索引。
-- ============================================================

BEGIN;

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
);
COMMENT ON TABLE users IS '用户账号表:邮箱登录,一人可属于多个 team';
COMMENT ON COLUMN users.id IS '主键 UUIDv7';
COMMENT ON COLUMN users.email IS '登录邮箱(全局唯一约束)';
COMMENT ON COLUMN users.password_hash IS 'bcrypt 哈希后的密码';
COMMENT ON COLUMN users.email_verified_at IS '邮箱验证时间戳(未验证为 NULL)';
COMMENT ON COLUMN users.display_name IS '显示名(可选)';
COMMENT ON COLUMN users.created_at IS '创建时间戳(毫秒)';
COMMENT ON COLUMN users.updated_at IS '更新时间戳(毫秒)';
COMMENT ON COLUMN users.is_platform_admin IS '平台超级管理员标记(可改全局配置/读全局日志)';

-- 1.2 teams
CREATE TABLE IF NOT EXISTS teams (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    owner_id VARCHAR(36) NOT NULL REFERENCES users(id),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
COMMENT ON TABLE teams IS '团队表:多租户隔离边界 + codex 账号共用单元';
COMMENT ON COLUMN teams.id IS '主键 UUIDv7';
COMMENT ON COLUMN teams.name IS '团队名称';
COMMENT ON COLUMN teams.owner_id IS '团队创建者/拥有者用户 ID(外键 users.id)';
COMMENT ON COLUMN teams.created_at IS '创建时间戳(毫秒)';
COMMENT ON COLUMN teams.updated_at IS '更新时间戳(毫秒)';

-- 1.3 team_members(此段已含 m20260720 的 role CHECK 约束,提前合并)
CREATE TABLE IF NOT EXISTS team_members (
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR(16) NOT NULL,
    joined_at BIGINT NOT NULL,
    PRIMARY KEY (team_id, user_id),
    CONSTRAINT team_members_role_chk CHECK (role IN ('owner','admin','member'))
);
COMMENT ON TABLE team_members IS '团队成员关系(多对多):团队内角色(owner/admin/member)';
COMMENT ON COLUMN team_members.team_id IS '团队 ID(外键 teams.id,级联删除)';
COMMENT ON COLUMN team_members.user_id IS '用户 ID(外键 users.id,级联删除)';
COMMENT ON COLUMN team_members.role IS '角色:owner / admin / member';
COMMENT ON COLUMN team_members.joined_at IS '加入时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_team_members_user ON team_members (user_id);

-- 1.4 invitations
CREATE TABLE IF NOT EXISTS invitations (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    code VARCHAR(64) NOT NULL UNIQUE,
    created_by VARCHAR(36) NOT NULL REFERENCES users(id),
    expires_at BIGINT,
    max_uses INT,
    used_count INT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL
);
COMMENT ON TABLE invitations IS '邀请码:owner 生成,他人凭码加入 team';
COMMENT ON COLUMN invitations.team_id IS '所属团队 ID(外键 teams.id,级联删除)';
COMMENT ON COLUMN invitations.code IS '邀请码(唯一约束)';
COMMENT ON COLUMN invitations.created_by IS '创建者用户 ID(外键 users.id)';
COMMENT ON COLUMN invitations.expires_at IS '过期时间戳(NULL 表示永不过期)';
COMMENT ON COLUMN invitations.max_uses IS '最大使用次数(NULL 表示不限)';
COMMENT ON COLUMN invitations.used_count IS '已使用次数';
COMMENT ON COLUMN invitations.created_at IS '创建时间戳(毫秒)';

-- 1.5 refresh_tokens
CREATE TABLE IF NOT EXISTS refresh_tokens (
    id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash VARCHAR(255) NOT NULL UNIQUE,
    expires_at BIGINT NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL
);
COMMENT ON TABLE refresh_tokens IS 'JWT 刷新令牌:存哈希,支持撤销与一次性轮转';
COMMENT ON COLUMN refresh_tokens.user_id IS '所属用户 ID(外键 users.id,级联删除)';
COMMENT ON COLUMN refresh_tokens.token_hash IS 'token SHA256 哈希(唯一约束)';
COMMENT ON COLUMN refresh_tokens.revoked IS '是否已撤销';
COMMENT ON COLUMN refresh_tokens.expires_at IS '过期时间戳(毫秒)';

-- 1.6 threads
CREATE TABLE IF NOT EXISTS threads (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL,
    created_by_user_id VARCHAR(36) NOT NULL REFERENCES users(id),
    title VARCHAR(255),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    workspace_type VARCHAR(8) NOT NULL DEFAULT 'team',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    last_activity_at BIGINT NOT NULL
);
COMMENT ON TABLE threads IS '会话元数据:per-thread(rollout 内容在 worker 本地 CODEX_HOME)';
COMMENT ON COLUMN threads.id IS '主键 UUIDv7';
COMMENT ON COLUMN threads.team_id IS '归属标识:团队 workspace 存 teamId,个人 workspace 存 userId';
COMMENT ON COLUMN threads.created_by_user_id IS '创建者用户 ID';
COMMENT ON COLUMN threads.title IS '会话标题(可选,首次 turn 后由 codex 自动生成)';
COMMENT ON COLUMN threads.status IS '状态:active / archived';
COMMENT ON COLUMN threads.workspace_type IS 'workspace 类型:personal(个人) / team(团队)';
COMMENT ON COLUMN threads.created_at IS '创建时间戳(毫秒)';
COMMENT ON COLUMN threads.updated_at IS '更新时间戳(毫秒)';
COMMENT ON COLUMN threads.last_activity_at IS '最后活跃时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_threads_team ON threads (team_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads (team_id, status);

-- 1.7 team_api_keys
CREATE TABLE IF NOT EXISTS team_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
    encrypted_key TEXT NOT NULL,
    key_hint VARCHAR(16) NOT NULL,
    set_by VARCHAR(36) NOT NULL REFERENCES users(id),
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
COMMENT ON TABLE team_api_keys IS '团队 BYOK API Key:encrypted_key 为 AES-GCM 密文';
COMMENT ON COLUMN team_api_keys.team_id IS '所属团队 ID(外键 teams.id,级联删除)';
COMMENT ON COLUMN team_api_keys.provider IS '提供商(默认 openai)';
COMMENT ON COLUMN team_api_keys.encrypted_key IS '加密后的 API key(AES-GCM hex)';
COMMENT ON COLUMN team_api_keys.key_hint IS '密钥提示(显示用,如 sk-abc...xyz)';
COMMENT ON COLUMN team_api_keys.set_by IS '设置者用户 ID(外键 users.id)';
COMMENT ON COLUMN team_api_keys.is_active IS '是否启用';
CREATE INDEX IF NOT EXISTS idx_team_api_keys_team ON team_api_keys (team_id, is_active);

-- 1.8 user_api_keys
CREATE TABLE IF NOT EXISTS user_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider VARCHAR(32) NOT NULL DEFAULT 'openai',
    encrypted_key TEXT NOT NULL,
    key_hint VARCHAR(16) NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT FALSE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
COMMENT ON TABLE user_api_keys IS '用户个人 BYOK API Key(personal workspace 使用)';
COMMENT ON COLUMN user_api_keys.user_id IS '所属用户 ID(外键 users.id,级联删除)';
COMMENT ON COLUMN user_api_keys.provider IS '提供商(默认 openai)';
COMMENT ON COLUMN user_api_keys.encrypted_key IS '加密后的 API key(AES-GCM hex)';
COMMENT ON COLUMN user_api_keys.key_hint IS '密钥提示';
COMMENT ON COLUMN user_api_keys.is_active IS '是否启用';
CREATE INDEX IF NOT EXISTS idx_user_api_keys_user ON user_api_keys (user_id, is_active);

-- 1.9 audit_log
CREATE TABLE IF NOT EXISTS audit_log (
    id VARCHAR(36) PRIMARY KEY,
    team_id VARCHAR(36) NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    actor_user_id VARCHAR(36) NOT NULL REFERENCES users(id),
    action VARCHAR(64) NOT NULL,
    detail TEXT,
    created_at BIGINT NOT NULL
);
COMMENT ON TABLE audit_log IS '审计日志:team owner 关键操作留痕(设 key / 邀请 / 踢除等)';
COMMENT ON COLUMN audit_log.team_id IS '操作所属团队 ID';
COMMENT ON COLUMN audit_log.actor_user_id IS '操作者用户 ID';
COMMENT ON COLUMN audit_log.action IS '操作类型(如 set_api_key / invite / remove_member)';
COMMENT ON COLUMN audit_log.detail IS '操作详情(JSON 文本,可选)';
COMMENT ON COLUMN audit_log.created_at IS '操作时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_audit_team ON audit_log (team_id, created_at DESC);

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
);
COMMENT ON TABLE token_usage_snapshots IS 'token 用量快照:每 turn 一行,upsert 更新';
COMMENT ON COLUMN token_usage_snapshots.thread_id IS '会话 ID(外键 threads.id)';
COMMENT ON COLUMN token_usage_snapshots.turn_id IS '轮次 ID';
COMMENT ON COLUMN token_usage_snapshots.team_id IS '所属团队 ID(从 threads.team_id 推导)';
COMMENT ON COLUMN token_usage_snapshots.total_tokens IS '本轮总 token 数';
COMMENT ON COLUMN token_usage_snapshots.input_tokens IS '输入 token 数';
COMMENT ON COLUMN token_usage_snapshots.cached_input_tokens IS '缓存输入 token 数';
COMMENT ON COLUMN token_usage_snapshots.output_tokens IS '输出 token 数';
COMMENT ON COLUMN token_usage_snapshots.reasoning_output_tokens IS '推理输出 token 数';
COMMENT ON COLUMN token_usage_snapshots.last_total_tokens IS '上一轮总 token 数';
COMMENT ON COLUMN token_usage_snapshots.last_input_tokens IS '上一轮输入 token 数';
COMMENT ON COLUMN token_usage_snapshots.last_cached_input_tokens IS '上一轮缓存输入 token 数';
COMMENT ON COLUMN token_usage_snapshots.last_output_tokens IS '上一轮输出 token 数';
COMMENT ON COLUMN token_usage_snapshots.last_reasoning_output_tokens IS '上一轮推理输出 token 数';
COMMENT ON COLUMN token_usage_snapshots.model_context_window IS '模型上下文窗口大小(可空)';
COMMENT ON COLUMN token_usage_snapshots.raw_payload IS '原始 payload(JSON 文本)';
COMMENT ON COLUMN token_usage_snapshots.updated_at IS '更新时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_token_usage_thread_updated ON token_usage_snapshots (thread_id, updated_at);

-- 1.11 turn_diffs
CREATE TABLE IF NOT EXISTS turn_diffs (
    thread_id VARCHAR(36) NOT NULL,
    turn_id VARCHAR(64) NOT NULL,
    team_id VARCHAR(36),
    diff TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
);
COMMENT ON TABLE turn_diffs IS 'turn diff:每 turn 一行,upsert 更新';
COMMENT ON COLUMN turn_diffs.thread_id IS '会话 ID';
COMMENT ON COLUMN turn_diffs.turn_id IS '轮次 ID';
COMMENT ON COLUMN turn_diffs.team_id IS '所属团队 ID';
COMMENT ON COLUMN turn_diffs.diff IS '本次 turn 的代码变更内容';
COMMENT ON COLUMN turn_diffs.updated_at IS '更新时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_turn_diffs_thread ON turn_diffs (thread_id);

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
);
COMMENT ON TABLE settings IS '运行时设置:key/value 结构,供 onlyoffice 等子系统读取';
COMMENT ON COLUMN settings.setting_key IS '设置键名(主键)';
COMMENT ON COLUMN settings.value IS '设置值(NULL 表示未设置,用 default_value)';
COMMENT ON COLUMN settings.type IS '值类型:string / int / bool / url';
COMMENT ON COLUMN settings.category IS '分类:general / onlyoffice / security 等';
COMMENT ON COLUMN settings.description IS '中文说明';
COMMENT ON COLUMN settings.default_value IS '默认值';
COMMENT ON COLUMN settings.constraints IS '约束描述(JSON 文本,如 {"min":0,"max":100})';
CREATE INDEX IF NOT EXISTS idx_settings_category ON settings (category);

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
);
COMMENT ON TABLE pending_server_requests IS '待处理服务端请求:codex 侧发起的审批请求';
COMMENT ON COLUMN pending_server_requests.generation IS 'codex 进程 generation(重启后递增)';
COMMENT ON COLUMN pending_server_requests.request_id IS '请求 ID(复合主键一部分)';
COMMENT ON COLUMN pending_server_requests.thread_id IS '所属会话 ID';
COMMENT ON COLUMN pending_server_requests.team_id IS '所属团队 ID';
COMMENT ON COLUMN pending_server_requests.status IS '状态:pending / approved / denied';
COMMENT ON COLUMN pending_server_requests.resolved_by IS '处理者用户 ID';
COMMENT ON COLUMN pending_server_requests.created_at IS '创建时间戳(毫秒)';
COMMENT ON COLUMN pending_server_requests.updated_at IS '更新时间戳(毫秒)';
COMMENT ON COLUMN pending_server_requests.resolved_at IS '处理时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_pending_requests_thread_status ON pending_server_requests (thread_id, status);
CREATE INDEX IF NOT EXISTS idx_pending_requests_status_updated ON pending_server_requests (status, updated_at);

-- 1.14 turn_errors
CREATE TABLE IF NOT EXISTS turn_errors (
    thread_id VARCHAR(36) NOT NULL,
    turn_id VARCHAR(64) NOT NULL,
    team_id VARCHAR(36),
    message TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
);
COMMENT ON TABLE turn_errors IS 'turn 错误记录:每 turn 一行,记录错误消息';
COMMENT ON COLUMN turn_errors.thread_id IS '会话 ID';
COMMENT ON COLUMN turn_errors.turn_id IS '轮次 ID';
COMMENT ON COLUMN turn_errors.team_id IS '所属团队 ID';
COMMENT ON COLUMN turn_errors.message IS '错误消息';
COMMENT ON COLUMN turn_errors.created_at IS '创建时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_turn_errors_thread ON turn_errors (thread_id);

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
);
COMMENT ON TABLE team_quotas IS 'per-team 配额与用量计数(turn 级别 + token 级别)';
COMMENT ON COLUMN team_quotas.plan IS '套餐计划(默认 free)';
COMMENT ON COLUMN team_quotas.turn_quota_hourly IS '每小时 turn 配额(0 = 不限)';
COMMENT ON COLUMN team_quotas.token_quota_monthly IS '每月 token 配额(0 = 不限)';
COMMENT ON COLUMN team_quotas.used_turns_hour IS '当前小时已用 turn 数';
COMMENT ON COLUMN team_quotas.hour_bucket IS '滑动小时桶(变化时重置 used_turns_hour)';
COMMENT ON COLUMN team_quotas.used_tokens_month IS '本月已用 token 数';
COMMENT ON COLUMN team_quotas.month_bucket IS '月度桶(格式 YYYY-MM)';

-- 1.16 team_routes
CREATE TABLE IF NOT EXISTS team_routes (
    team_id VARCHAR(36) PRIMARY KEY NOT NULL,
    worker_id VARCHAR(64) NOT NULL,
    mapped_at BIGINT NOT NULL,
    mapped_reason VARCHAR(16) NOT NULL DEFAULT 'initial'
);
COMMENT ON TABLE team_routes IS 'team→worker 路由覆盖(failover 决策记录,防节点抖动回切)';
COMMENT ON COLUMN team_routes.team_id IS '团队 ID(主键)';
COMMENT ON COLUMN team_routes.worker_id IS '分配的 worker 节点 ID';
COMMENT ON COLUMN team_routes.mapped_at IS '映射时间戳(毫秒)';
COMMENT ON COLUMN team_routes.mapped_reason IS '映射原因:initial / failover / manual';

-- 1.17 session_replicas(初版 per-team;后续 3/7 迁移会改为 per-thread,这里直接建最终形态)
CREATE TABLE IF NOT EXISTS session_replicas (
    thread_id VARCHAR(36) PRIMARY KEY NOT NULL,
    primary_node VARCHAR(64) NOT NULL,
    replica_node VARCHAR(64),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    primary_lease_until BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL
);
COMMENT ON TABLE session_replicas IS 'per-thread 主副本映射(active-passive HA):thread_id → primary + replica';
COMMENT ON COLUMN session_replicas.thread_id IS '会话 ID(主键)';
COMMENT ON COLUMN session_replicas.primary_node IS '跑 codex 的主节点 ID';
COMMENT ON COLUMN session_replicas.replica_node IS '存 rollout/workspace 副本的节点 ID(可空)';
COMMENT ON COLUMN session_replicas.status IS '状态:active / promoting / degraded';
COMMENT ON COLUMN session_replicas.primary_lease_until IS '主节点租约到期时间戳(毫秒)';
COMMENT ON COLUMN session_replicas.updated_at IS '更新时间戳(毫秒)';

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
);
COMMENT ON TABLE workspace_audit IS 'hook 审计落库:codex 工具调用前后 webhook 推送的事件原样入库';
COMMENT ON COLUMN workspace_audit.id IS '主键 UUIDv7';
COMMENT ON COLUMN workspace_audit.team_id IS '操作所属团队 ID(可空)';
COMMENT ON COLUMN workspace_audit.user_id IS '操作者用户 ID(可空)';
COMMENT ON COLUMN workspace_audit.thread_id IS '操作所属会话 ID(可空)';
COMMENT ON COLUMN workspace_audit.event_type IS '事件类型:PreToolUse / PostToolUse / SessionStart 等';
COMMENT ON COLUMN workspace_audit.tool_name IS '触发的工具名(可空,如 shell/write)';
COMMENT ON COLUMN workspace_audit.payload_json IS '事件原始 payload(JSON 文本)';
COMMENT ON COLUMN workspace_audit.decision IS '决策结果:allow / deny(PreToolUse 时有值)';
COMMENT ON COLUMN workspace_audit.created_at IS '创建时间戳(毫秒)';
CREATE INDEX IF NOT EXISTS idx_workspace_audit_team_user_ts ON workspace_audit (team_id, user_id, created_at DESC);

-- 1.19 thread_resume_cache
CREATE TABLE IF NOT EXISTS thread_resume_cache (
    thread_id VARCHAR(36) PRIMARY KEY,
    response JSON NOT NULL,
    updated_at BIGINT NOT NULL
);
COMMENT ON TABLE thread_resume_cache IS 'thread/resume 集群共享缓存:mt_create_thread 写入,invoke resume 读取(避 codex 异步落盘 race)';
COMMENT ON COLUMN thread_resume_cache.thread_id IS '会话 ID(主键,对应 threads.id)';
COMMENT ON COLUMN thread_resume_cache.response IS '缓存的 thread/resume 响应(JSON,codex 完整结构化响应)';
COMMENT ON COLUMN thread_resume_cache.updated_at IS '更新时间戳(毫秒,后端启动时全表清空,运行时 upsert)';
CREATE INDEX IF NOT EXISTS idx_thread_resume_cache_updated ON thread_resume_cache (updated_at);

-- ============================================================
-- 2/7  m20260720_000001_rbac_permissions
-- (users.is_platform_admin 已在 1.1 合并;team_members.role CHECK 已在 1.3 合并)
-- ============================================================

-- 2.1 role_permissions 表(全局,无 team_id)
CREATE TABLE IF NOT EXISTS role_permissions (
    role VARCHAR(16) NOT NULL,
    permission VARCHAR(48) NOT NULL,
    PRIMARY KEY (role, permission)
);
COMMENT ON TABLE role_permissions IS '角色→权限点映射,seed 三角色矩阵(spec §4.1)';
COMMENT ON COLUMN role_permissions.role IS '角色:owner / admin / member';
COMMENT ON COLUMN role_permissions.permission IS '权限点(如 team:member:list)';

-- 2.2 seed 角色权限矩阵
--    owner=全权限; admin=owner 减 transfer/dissolve/role:write; member=4 个基础。
--    ON CONFLICT DO NOTHING 幂等(同 seed 多次执行无副作用)。
INSERT INTO role_permissions (role, permission) VALUES
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
    ('member','team:turn:write')
ON CONFLICT (role, permission) DO NOTHING;

-- ============================================================
-- 3/7  m20260721_000001_session_replicas_per_thread
-- 1.17 已直接建立 per-thread 主键的 session_replicas(最终形态),
-- 无需 ALTER RENAME / 数据迁移 / DROP 旧表。
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
);
COMMENT ON TABLE cluster_extensions IS '集群扩展分发清单';
COMMENT ON COLUMN cluster_extensions.id IS '主键 UUIDv7';
COMMENT ON COLUMN cluster_extensions.kind IS '扩展类型:skill / plugin / mcp';
COMMENT ON COLUMN cluster_extensions.name IS '扩展名';

-- 4.2 cluster_extension_files
CREATE TABLE IF NOT EXISTS cluster_extension_files (
    id BIGINT PRIMARY KEY NOT NULL,
    extension_id VARCHAR(36) NOT NULL,
    rel_path VARCHAR(512) NOT NULL,
    size_bytes BIGINT NOT NULL,
    content_hash VARCHAR(128) NOT NULL,
    is_binary BOOLEAN NOT NULL DEFAULT FALSE
);
COMMENT ON TABLE cluster_extension_files IS '扩展文件指纹(无内容)';

-- 4.3 cluster_extension_holders
CREATE TABLE IF NOT EXISTS cluster_extension_holders (
    extension_id VARCHAR(36) NOT NULL,
    node_id VARCHAR(36) NOT NULL,
    held_since BIGINT NOT NULL
);
COMMENT ON TABLE cluster_extension_holders IS '扩展持有节点(去单点)';

-- 4.4 普通索引(后被 5/7 改为 UNIQUE)
CREATE INDEX IF NOT EXISTS idx_ext_kind_name ON cluster_extensions (kind, name);
CREATE INDEX IF NOT EXISTS idx_ext_enabled ON cluster_extensions (enabled);
CREATE INDEX IF NOT EXISTS idx_extfile_ext ON cluster_extension_files (extension_id);

-- ============================================================
-- 5/7  m20260722_000002_cluster_extensions_unique
-- 新建库无数据,无需 DELETE 去重;直接 DROP 旧普通索引 + CREATE UNIQUE 索引。
-- ============================================================
DROP INDEX IF EXISTS idx_ext_kind_name;
DROP INDEX IF EXISTS idx_extfile_ext;
CREATE UNIQUE INDEX IF NOT EXISTS idx_ext_kind_name_unique ON cluster_extensions (kind, name);
CREATE UNIQUE INDEX IF NOT EXISTS idx_extfile_ext_rel_unique ON cluster_extension_files (extension_id, rel_path);

-- ============================================================
-- 6/7  m20260722_000003_cluster_extensions_marketplace
-- ============================================================
ALTER TABLE cluster_extensions ADD COLUMN IF NOT EXISTS marketplace VARCHAR(128);
CREATE INDEX IF NOT EXISTS idx_ext_marketplace ON cluster_extensions (marketplace);
COMMENT ON COLUMN cluster_extensions.marketplace IS 'plugin 的市场名(skill/mcp 为空)';

-- ============================================================
-- 7/7  m20260722_000004_cluster_extensions_holder_pk
-- 新建库无数据,无需 DELETE 去重;直接 ADD 复合主键(PG 命名约束)。
-- ============================================================
ALTER TABLE cluster_extension_holders
    ADD CONSTRAINT pk_ext_holder PRIMARY KEY (extension_id, node_id);

COMMIT;

# 数据库迁移转 SQL 初始化文件 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `backend-rs/src/db/migration/` 下的 7 个 SeaORM Rust 迁移改写为可手工执行的 PostgreSQL + MySQL 初始化 SQL 脚本，并删除 Rust 实现、移除启动期 `Migrator::up` 调用、移除 `sea-orm-migration` 依赖。

**Architecture:** 一次性写两份 `init.sql`（PG 版用 `BEGIN/COMMIT` 包裹，MySQL 版带方言专属语法如 `DELETE alias FROM JOIN`、临时表去重、TRUNCATE 回插），把所有 DDL 加 `IF NOT EXISTS` 幂等；删除整目录后仅改 `mod.rs`/`main.rs`/`Cargo.toml` 三处的 5 个挂接点；测试不引用迁移，无需改测试。

**Tech Stack:** Rust / SeaORM 1.1（仅保留连接 + 实体）/ PostgreSQL ≥ 13 / MySQL ≥ 8.0.29。

**关联规格:** `docs/superpowers/specs/2026-07-23-database-migration-to-sql-design.md`

## Global Constraints

- 假定全新空库；表已存在则 `IF NOT EXISTS` 跳过但不会更新（来自规格 §4.2）。
- PG 版本 ≥ 13（规格 §7.1）；MySQL 版本 ≥ 8.0.29（启用 `CREATE TABLE IF NOT EXISTS`，规格 §4.2）。
- 测试不引用迁移（`grep -rn "Migrator\|m2026" backend-rs/tests/` 返回空，规格 §2），无需改测试。
- 业务实体（`backend-rs/src/db/entity.rs` / `backend-rs/src/db/entities/mod.rs`）不动（规格 §1.2）。
- 启动顺序变化：`Db connect → bootstrap platform admins → ...`（不再有 `Migrator::up`，规格 §4.5）。

---

## Task 1: 编写 PostgreSQL init.sql

**Files:**
- Create: `backend-rs/sql/pg/init.sql`

**Produces:** 一份完整的 PostgreSQL 初始化脚本，可被 `psql -d <db> -f` 直接执行；含全部 7 段（19 张表 + 11 索引 + 2 UNIQUE 索引 + 24 行 seed + 复合主键 + 表/列注释）；用 `BEGIN; ... COMMIT;` 包裹整脚本；所有 DDL 加 `IF NOT EXISTS` 幂等。

**步骤:**

- [ ] **Step 1: 写脚本骨架 + 第 1 段 combined_schema**

文件 `backend-rs/sql/pg/init.sql` 头部加：

```sql
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
```

继续在同一文件追加 19 张表的 `CREATE TABLE IF NOT EXISTS` + `COMMENT ON TABLE/COLUMN`，**逐字段对齐** `backend-rs/src/db/migration/m20260719_000001_combined_schema.rs` 现有定义。表清单与原文一一对应：

```sql
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
```

- [ ] **Step 2: 追加第 2 段 rbac_permissions(users.is_platform_admin 与 role_permissions seed)**

第 1 段已在 1.1 users 表内合并了 `is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE`；这里只需追加 `role_permissions` 表 + 24 行 seed：

```sql
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
```

- [ ] **Step 3: 追加第 3 段 session_replicas_per_thread(无操作,因 1.17 已建最终表)**

```sql
-- ============================================================
-- 3/7  m20260721_000001_session_replicas_per_thread
-- 1.17 已直接建立 per-thread 主键的 session_replicas(最终形态),
-- 无需 ALTER RENAME / 数据迁移 / DROP 旧表。
-- ============================================================
```

- [ ] **Step 4: 追加第 4-7 段 cluster_extensions 系列**

```sql
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
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/sql/pg/init.sql
git commit -m "feat(sql): 新增 PostgreSQL 数据库初始化脚本(7 段迁移翻译)"
```

---

## Task 2: 编写 MySQL init.sql

**Files:**
- Create: `backend-rs/sql/mysql/init.sql`

**Produces:** 一份完整的 MySQL 初始化脚本，可被 `mysql -D <db> < init.sql` 直接执行。方言差异点（来自规格 §4.3）：内联 `COMMENT` / `CHARSET=utf8mb4` / `DELETE alias FROM ... JOIN` / `CREATE TEMPORARY TABLE` + `TRUNCATE` + 回插 / `ADD PRIMARY KEY`（无名）。

**步骤:**

- [ ] **Step 1: 写脚本骨架 + 第 1 段 combined_schema 关键差异**

文件 `backend-rs/sql/mysql/init.sql` 头部加：

```sql
-- ============================================================
-- Codex WebUI 数据库初始化（MySQL）
-- 来源:backend-rs/src/db/migration/ 下 7 个 SeaORM 迁移翻译。
-- 要求:MySQL ≥ 8.0.29(启用 CREATE TABLE IF NOT EXISTS)。
-- 用法:mysql -D <db> < init.sql
-- 幂等:所有 CREATE TABLE 使用 IF NOT EXISTS,可重跑。
-- 警告:假定全新空库;MySQL 不支持 ALTER TABLE ADD/DROP COLUMN IF [NOT] EXISTS,
--       因此本脚本对已存在表结构不会更新(假定 DBA 不会手工加列)。
-- 提示:加 --single-transaction 让整批在单事务中执行(失败回滚)。
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
    is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE,
    -- 列注释(MySQL 8.0+ 支持内联 COMMENT)
    -- 注:以下为表内逐列的语义注释(用户账号表)
    -- 列:email = 登录邮箱(全局唯一约束)
    -- 列:password_hash = bcrypt 哈希后的密码
    -- 列:is_platform_admin = 平台超级管理员标记(可改全局配置/读全局日志)
    -- 表注释:见末尾 ALTER TABLE users COMMENT = ...
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
```

> **设计要点**：MySQL 不支持 `COMMENT ON COLUMN`，所以**所有列注释以 SQL `--` 行注释形式内联在 CREATE TABLE 内**（保留语义信息；DBA 看 SQL 即可理解）。表注释在 CREATE TABLE 之后用 `ALTER TABLE ... COMMENT = '...'`。

- [ ] **Step 2: 完整追加 1.1 - 1.19 表(沿用 PG 表结构,关键差异处理见下)**

逐表使用如下模板（以 1.1 users 为例）：

```sql
CREATE TABLE IF NOT EXISTS users (
    id VARCHAR(36) PRIMARY KEY,
    email VARCHAR(255) NOT NULL UNIQUE,
    password_hash VARCHAR(255) NOT NULL,
    email_verified_at BIGINT,
    display_name VARCHAR(255),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    is_platform_admin BOOLEAN NOT NULL DEFAULT FALSE
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
```

- [ ] **Step 3: 追加第 2 段 rbac_permissions + 24 行 seed(MySQL 用 INSERT IGNORE)**

```sql
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
```

- [ ] **Step 4: 追加第 4-7 段 cluster_extensions 系列(MySQL 自连接去重用 `DELETE alias FROM JOIN`;holders 主键 ADD 无名)**

```sql
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
```

- [ ] **Step 5: 提交**

```bash
git add backend-rs/sql/mysql/init.sql
git commit -m "feat(sql): 新增 MySQL 数据库初始化脚本(7 段迁移翻译)"
```

---

## Task 3: 编写 README

**Files:**
- Create: `backend-rs/sql/README.md`

**Produces:** 一份运维提示文档，说明如何选方言、版本要求、跑法、限制。

**步骤:**

- [ ] **Step 1: 写 README**

文件 `backend-rs/sql/README.md` 完整内容：

```markdown
# 数据库初始化

**挑一个方言：**

| 方言 | 脚本 | 最低版本 | 执行命令 |
|---|---|---|---|
| PostgreSQL | `pg/init.sql` | 13+ | `psql -d <db> -f pg/init.sql` |
| MySQL | `mysql/init.sql` | 8.0.29+ | `mysql -D <db> < mysql/init.sql` |

**重要：**

- 脚本是**全新空库**初始化语义，假定没有任何业务表。
- 所有 `CREATE TABLE` 使用 `IF NOT EXISTS`，可重跑。
- 重跑**不会**更新已存在表的列/索引（MySQL 不支持 `ADD COLUMN IF NOT EXISTS`）。
- 启动顺序变化：`Db connect → bootstrap platform admins → ...`，**不再有 Migrator::up**。
- 启动后端进程前必须先在 DB 上跑此脚本，否则连接正常但所有查询失败。
- MySQL 推荐加 `--single-transaction` 让整批在单事务中执行。
- 来源迁移位于 `backend-rs/src/db/migration/`（即将删除），保留追溯。
```

- [ ] **Step 2: 提交**

```bash
git add backend-rs/sql/README.md
git commit -m "docs(sql): 新增数据库初始化 README（方言选择与版本要求）"
```

---

## Task 4: 删除 Rust 迁移目录

**Files:**
- Delete: `backend-rs/src/db/migration/`（整目录：7 个 `m2026*.rs` + `mod.rs`）

**Produces:** `backend-rs/src/db/migration/` 目录消失。后续任务修改 `mod.rs`/`main.rs`/`Cargo.toml` 引用，否则 `cargo build` 失败。

**步骤:**

- [ ] **Step 1: 删除目录**

```bash
git rm -r backend-rs/src/db/migration
```

- [ ] **Step 2: 提交**

```bash
git commit -m "chore(backend-rs): 删除 SeaORM 迁移目录（已迁至 SQL 初始化脚本）"
```

---

## Task 5: 修改 db/mod.rs 移除模块挂接

**Files:**
- Modify: `backend-rs/src/db/mod.rs:5`

**Produces:** `pub mod migration;` 行删除；`pub mod entity;` 与 `pub mod entities;` 保留。

**步骤:**

- [ ] **Step 1: 验证 cargo build 当前会因目录已删而失败**

```bash
cargo build -p codex-webui
```

预期：`error[E0583]: failed to resolve module path: ... migration`（目录已删但 `mod.rs:5` 仍在引用）。

- [ ] **Step 2: 删除 `pub mod migration;` 行**

文件 `backend-rs/src/db/mod.rs` 当前内容：

```rust
//! 数据访问层：SeaORM Entity 定义 + 数据库迁移。

pub mod entity;
pub mod entities;
pub mod migration;
```

改为：

```rust
//! 数据访问层：SeaORM Entity 定义 + 数据库连接。

pub mod entity;
pub mod entities;
```

- [ ] **Step 3: 提交**

```bash
git add backend-rs/src/db/mod.rs
git commit -m "refactor(backend-rs): db/mod.rs 移除 migration 模块挂接"
```

---

## Task 6: 修改 main.rs 移除 import + Migrator::up 调用

**Files:**
- Modify: `backend-rs/src/main.rs:13`（import 块）
- Modify: `backend-rs/src/main.rs:28`（use MigratorTrait）
- Modify: `backend-rs/src/main.rs:57-59`（Migrator::up 调用）

**Produces:** `main.rs` 不再依赖 `sea_orm_migration`，启动期不再调用 `Migrator::up`。

**步骤:**

- [ ] **Step 1: 验证 cargo build 仍因 `db::migration::Migrator` 而失败**

```bash
cargo build -p codex-webui
```

预期：`error[E0432]: unresolved import 'codex_webui::db::migration::Migrator'`（`db/mod.rs` 已无 migration 模块，但 `main.rs:13` 仍引用）。

- [ ] **Step 2: 删除 `main.rs:13` 的 `db::migration::Migrator,` 整行**

文件 `backend-rs/src/main.rs` import 块当前（行 6-24）：

```rust
use codex_webui::{
    api::build_router,
    api::hooks,
    api::multitenant::internal_rpc::build_internal_router,
    auth::AuthService,
    codex::CodexProcessManager,
    config::Config,
    db::migration::Migrator,
    logging,
    services::multitenant::cluster::{ClusterMembership, RedisCluster, SingleCluster},
    services::multitenant::event_bus::EventBus,
    services::multitenant::replication,
    services::multitenant::rpc::WorkerRpcClient,
    services::multitenant::sticky::{NoopSticky, RedisSticky, StickyStore},
    services::settings::{self, reconcile_settings},
```

改为（删除 `db::migration::Migrator,` 整行；其余保留）：

```rust
use codex_webui::{
    api::build_router,
    api::hooks,
    api::multitenant::internal_rpc::build_internal_router,
    auth::AuthService,
    codex::CodexProcessManager,
    config::Config,
    logging,
    services::multitenant::cluster::{ClusterMembership, RedisCluster, SingleCluster},
    services::multitenant::event_bus::EventBus,
    services::multitenant::replication,
    services::multitenant::rpc::WorkerRpcClient,
    services::multitenant::sticky::{NoopSticky, RedisSticky, StickyStore},
    services::settings::{self, reconcile_settings},
```

- [ ] **Step 3: 删除 `main.rs:28` 的 `use sea_orm_migration::MigratorTrait;` 整行**

文件 `backend-rs/src/main.rs:28` 删除该行。

- [ ] **Step 4: 删除 `main.rs:57-59` 的 `Migrator::up(&db, None).await?;` 3 行**

文件 `backend-rs/src/main.rs:57-59` 当前：

```rust
    Migrator::up(&db, None)
        .await
        .map_err(|e| anyhow::anyhow!("run migrations: {e}"))?;
```

改为（直接删除这 3 行，保留前后行不变）。

- [ ] **Step 5: 验证 cargo build 通过**

```bash
cargo build -p codex-webui
```

预期：`Finished ...` 无错误；可能仍有 Cargo.toml 中 `sea-orm-migration` 依赖的"unused"警告（由 Task 7 处理）。

- [ ] **Step 6: 提交**

```bash
git add backend-rs/src/main.rs
git commit -m "refactor(backend-rs): main.rs 移除 Migrator::up 与 sea_orm_migration 引用"
```

---

## Task 7: 移除 Cargo.toml 中 sea-orm-migration 依赖

**Files:**
- Modify: `backend-rs/Cargo.toml:57`

**Produces:** `sea-orm-migration` 依赖从 `Cargo.toml` 移除；`Cargo.lock` 自动同步。

**步骤:**

- [ ] **Step 1: 验证 cargo build 当前有"unused manifest entry"警告**

```bash
cargo build -p codex-webui 2>&1 | grep -E "warning.*unused|warning.*sea-orm-migration"
```

预期：出现 `warning: unused dependency: sea-orm-migration`。

- [ ] **Step 2: 删除 `Cargo.toml:57` 整行**

文件 `backend-rs/Cargo.toml:54-57` 当前：

```toml
# SeaORM 全量数据层(PG/MySQL 多方言)。rusqlite 已删,无 libsqlite3-sys 冲突;sqlx 统一 0.8(sea-orm 1.1 依赖)。
# sea-orm 不开 sqlx-sqlite 后端,不牵 libsqlite3-sys。multitenant + 业务代码全部迁 sea-orm。
sea-orm = { version = "1.1", default-features = false, features = ["sqlx-postgres", "sqlx-mysql", "runtime-tokio-rustls", "macros", "with-json"] }
sea-orm-migration = { version = "1.1", default-features = false, features = ["sqlx-postgres", "sqlx-mysql", "runtime-tokio-rustls"] }
```

改为（删除 `sea-orm-migration = ...` 整行；`sea-orm` 上一行注释保留）：

```toml
# SeaORM 全量数据层(PG/MySQL 多方言)。rusqlite 已删,无 libsqlite3-sys 冲突;sqlx 统一 0.8(sea-orm 1.1 依赖)。
# sea-orm 不开 sqlx-sqlite 后端,不牵 libsqlite3-sys。multitenant + 业务代码全部迁 sea-orm。
sea-orm = { version = "1.1", default-features = false, features = ["sqlx-postgres", "sqlx-mysql", "runtime-tokio-rustls", "macros", "with-json"] }
```

- [ ] **Step 3: 验证 cargo build 无警告**

```bash
cargo build -p codex-webui 2>&1 | tail -20
```

预期：`Finished ...` 无 `warning: unused dependency`；`Cargo.lock` 自动更新（git status 应显示 `Cargo.lock` 改动）。

- [ ] **Step 4: 验证 cargo test 通过**

```bash
cargo test -p codex-webui --no-run  # 编译但不跑,确认无编译错误
```

预期：`Finished test [unexpacted]` 无错误（测试不引用迁移，规格 §2 已确认）。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/Cargo.toml backend-rs/Cargo.lock
git commit -m "chore(backend-rs): 移除 sea-orm-migration 依赖"
```

---

## Task 8: 最终验证

**Files:** 无（仅运行命令）

**Produces:** 端到端验证：cargo build/test 干净 + SQL 文件可在干净库上成功执行（如果环境有 PG/MySQL）。

**步骤:**

- [ ] **Step 1: 全量 cargo build 干净**

```bash
cargo build -p codex-webui 2>&1 | tail -5
```

预期：`Finished ... [unoptimized + debuginfo] target(s)` 无警告无错误。

- [ ] **Step 2: 全量 cargo test 跑通**

```bash
cargo test -p codex-webui 2>&1 | tail -20
```

预期：所有测试通过；无 `Migrator` 相关错误。

- [ ] **Step 3: （可选）PG 端 SQL 冒烟**

若本地有 PostgreSQL：

```bash
createdb codex_webui_smoke
psql -d codex_webui_smoke -f backend-rs/sql/pg/init.sql
psql -d codex_webui_smoke -c "\dt"
psql -d codex_webui_smoke -c "\di"
psql -d codex_webui_smoke -c "SELECT count(*) FROM users; INSERT INTO users (id, email, password_hash, created_at, updated_at) VALUES ('test-uuid', 't@t.com', 'x', 0, 0); SELECT count(*) FROM users; DELETE FROM users WHERE id='test-uuid';"
```

预期：列出 19 张业务表 + 索引（≥13 条）；最后一次 `count(*)` 1 → 0（验证 INSERT/DELETE）。

- [ ] **Step 4: （可选）MySQL 端 SQL 冒烟**

若本地有 MySQL 8.0.29+：

```bash
mysql -e "CREATE DATABASE codex_webui_smoke;"
mysql codex_webui_smoke < backend-rs/sql/mysql/init.sql
mysql codex_webui_smoke -e "SHOW TABLES;"
mysql codex_webui_smoke -e "SELECT count(*) FROM role_permissions;"  # 期望 24
```

预期：列出 19 张业务表；`role_permissions` 24 行。

- [ ] **Step 5: （可选）连跑两次验证幂等**

```bash
psql -d codex_webui_smoke -f backend-rs/sql/pg/init.sql  # 第二次跑,不应报错
```

预期：无错误；所有 CREATE TABLE / CREATE INDEX 静默跳过。

---

## 自审

- **规格覆盖**：§4.1 目录结构 → Task 1/2/3；§4.4 Rust 删除清单 → Task 4/5/6/7（4 个独立挂接点）；§6 验证策略 → Task 8；§3 7 段翻译 → Task 1/2（PG/MySQL 各 7 段）。
- **占位符扫描**：无 TBD/TODO；每条 SQL 完整给出。
- **类型一致性**：`session_replicas` 表主键在 PG/MySQL 两份 SQL 中均为 `thread_id`（per-thread 形态）；`is_platform_admin` 列在 1.1 users 内统一合并；`role_permissions` 在 2/7 段；索引名 `idx_ext_kind_name_unique` / `idx_extfile_ext_rel_unique` 两库一致。

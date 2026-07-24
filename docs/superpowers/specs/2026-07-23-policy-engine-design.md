# 策略引擎设计：命令审查与技能/插件/MCP 使用限制

## 1. 背景与目标

Codex WebUI 已具备 `PreToolUse` hook 拦截能力，当前仅实现了基于 workspace 路径的硬编码决策。本设计引入**可配置策略引擎**，让平台管理员和 team owner/admin 能够在界面上配置规则，实时限制：

- 危险命令（如 `rm -rf /`、`curl | bash`）
- 特定 skill / plugin / MCP 的调用
- 任意 codex tool 名称的调用

规则按**全局默认 → team 角色覆盖**两层生效，配置落库，并在 `PreToolUse` hook 中实时决策。

---

## 2. 设计原则

1. **Fail-open 兼容**：策略引擎异常时返回 `Allow`，不阻断 codex。
2. **最小改动现有 hook 路径**：路径安全底线保留在 `decision.rs`，策略引擎作为叠加层。
3. **集群一致**：进程内缓存 + Redis 事件广播失效 + 短 TTL 兜底。
4. **不自动迁移**：DB 变更写入 `backend-rs/sql/pg/init.sql` 与 `backend-rs/sql/mysql/init.sql`。
5. **第一期不做审批流**：动作仅 `allow` / `deny`。

---

## 3. 数据模型

### 3.1 表：`tool_policies`

```sql
-- PostgreSQL / MySQL 结构一致，详见 7.1 / 7.2
CREATE TABLE IF NOT EXISTS tool_policies (
    id VARCHAR(36) PRIMARY KEY,
    scope VARCHAR(16) NOT NULL,              -- 'global' | 'team'
    team_id VARCHAR(36) REFERENCES teams(id) ON DELETE CASCADE,
    role VARCHAR(16),                        -- NULL | 'owner' | 'admin' | 'member'
    rule_type VARCHAR(16) NOT NULL,          -- 'command' | 'tool' | 'skill' | 'plugin' | 'mcp'
    match_mode VARCHAR(16) NOT NULL,         -- 'blacklist' | 'whitelist' | 'regex' | 'exact'
    pattern TEXT NOT NULL,
    action VARCHAR(16) NOT NULL,             -- 'allow' | 'deny'
    priority INT NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    description TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
```

### 3.2 字段语义

| 字段 | 说明 |
|------|------|
| `scope` | `global` 表示全局策略；`team` 表示团队策略 |
| `team_id` | `scope='team'` 时必填；删除 team 时级联删除 |
| `role` | `NULL` 对所有角色生效；否则仅对 owner/admin/member 生效 |
| `rule_type` | `command` 匹配命令字符串；`tool/skill/plugin/mcp` 匹配对应名称 |
| `match_mode` | 命中方式：`blacklist`/`whitelist` 为子串包含；`regex` 为正则；`exact` 为完全相等 |
| `pattern` | 匹配内容 |
| `action` | 命中后的决策：`allow` 或 `deny` |
| `priority` | 越大越优先；同优先级按 `id` 升序 |

### 3.3 继承规则

按以下顺序查找第一个命中的规则，未命中则默认 `Allow`：

1. `scope='team' AND role=<当前角色>`
2. `scope='team' AND role IS NULL`
3. `scope='global' AND role=<当前角色>`
4. `scope='global' AND role IS NULL`

---

## 4. 规则匹配引擎

### 4.1 模块位置

```
backend-rs/src/services/policy_engine/
├── mod.rs       # 公共类型与 evaluate 入口
├── engine.rs    # 匹配逻辑
├── store.rs     # DB 查询与缓存
└── dto.rs       # REST DTO
```

### 4.2 核心类型

```rust
pub struct PolicyInput<'a> {
    pub team_id: &'a str,
    pub user_id: &'a str,
    pub role: &'a str,
    pub tool_name: &'a str,
    pub tool_input: Option<&'a Value>,
    pub command_text: Option<&'a str>,
    pub skill_name: Option<&'a str>,
    pub plugin_name: Option<&'a str>,
    pub mcp_name: Option<&'a str>,
}

pub enum PolicyDecision {
    Allow,
    Deny { rule_id: String, reason: String },
}
```

### 4.3 输入提取策略

| 输入 | 提取顺序 |
|------|----------|
| `command_text` | `tool_input.command` → `tool_input.cmd` → `tool_input.arguments[0]` → `tool_input.input` → `None` |
| `skill_name` | `tool_input.skill` → `tool_name` 去掉 `skill:` 前缀 → `None` |
| `plugin_name` | `tool_input.plugin` → `tool_name` 去掉 `plugin:` 前缀 → `None` |
| `mcp_name` | `tool_input.mcp_server` → `tool_input.mcp` → `tool_name` 去掉 `mcp:` 前缀 → `None` |

提取不到时该类规则不命中，不报错。

### 4.4 匹配逻辑

| match_mode | 行为 |
|------------|------|
| `exact` | 字符串完全相等 |
| `regex` | `regex::Regex::is_match`；编译失败视为不匹配 |
| `blacklist` | 子串包含即命中（不区分大小写） |
| `whitelist` | 子串包含即命中（不区分大小写） |

规则按 `priority DESC, id ASC` 排序，第一个命中即返回其 `action`。

---

## 5. Hook 集成

### 5.1 修改 `backend-rs/src/api/hooks.rs`

在 `PreToolUse` 分支中，路径决策之后增加策略引擎决策：

```rust
let path_decision = decide_pre_tool_use(&role, &tool_name, &target, &state.workspace_root);

let policy_input = PolicyInput { /* ... */ };
let policy_decision = policy_engine::evaluate(&state, &policy_input).await;

let perm = match (&path_decision, &policy_decision) {
    (_, PolicyDecision::Deny { .. }) => "deny",
    (Decision::Deny, _) => "deny",
    (Decision::Ask, _) => "ask",
    _ => "allow",
};
```

### 5.2 审计增强

`AuditEvent` 增加可选字段 `policy_rule_id`，命中策略拒绝时记录命中的规则 ID。该字段为内存结构扩展，不改动 `audit_log` 表。

### 5.3 拒绝原因

codex `PreToolUse` 响应格式为：

```json
{
  "continue": false,
  "hookSpecificOutput": {
    "permissionDecision": "deny"
  }
}
```

当前协议不支持下发自定义拒绝文案，拒绝原因写入审计日志，便于后台排查。

---

## 6. REST API

### 6.1 路由

| 路由 | 方法 | 权限 |
|------|------|------|
| `/api/policies` | GET/POST | 平台管理员 |
| `/api/policies/{id}` | PATCH/DELETE | 平台管理员 |
| `/api/mt/teams/{teamId}/policies` | GET/POST | team owner/admin |
| `/api/mt/teams/{teamId}/policies/{id}` | PATCH/DELETE | team owner/admin |

### 6.2 DTO

```rust
pub struct CreatePolicyBody {
    pub scope: PolicyScope,
    pub role: Option<String>,
    pub rule_type: RuleType,
    pub match_mode: MatchMode,
    pub pattern: String,
    pub action: PolicyAction,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
}
```

### 6.3 校验

- `pattern` 非空
- `match_mode=regex` 时 `pattern` 必须可编译为正则
- `scope='team'` 时 `team_id` 与路径一致
- `scope='global'` 时 `team_id` 必须为 NULL
- `role` 仅允许 NULL 或 `owner`/`admin`/`member`

---

## 7. 数据库初始化 SQL

### 7.1 PostgreSQL（追加到 `backend-rs/sql/pg/init.sql`）

```sql
CREATE TABLE IF NOT EXISTS tool_policies (
    id VARCHAR(36) PRIMARY KEY,
    scope VARCHAR(16) NOT NULL,
    team_id VARCHAR(36) REFERENCES teams(id) ON DELETE CASCADE,
    role VARCHAR(16),
    rule_type VARCHAR(16) NOT NULL,
    match_mode VARCHAR(16) NOT NULL,
    pattern TEXT NOT NULL,
    action VARCHAR(16) NOT NULL,
    priority INT NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    description TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CONSTRAINT tool_policies_scope_chk CHECK (scope IN ('global','team')),
    CONSTRAINT tool_policies_role_chk CHECK (role IS NULL OR role IN ('owner','admin','member')),
    CONSTRAINT tool_policies_rule_type_chk CHECK (rule_type IN ('command','tool','skill','plugin','mcp')),
    CONSTRAINT tool_policies_match_mode_chk CHECK (match_mode IN ('blacklist','whitelist','regex','exact')),
    CONSTRAINT tool_policies_action_chk CHECK (action IN ('allow','deny')),
    CONSTRAINT tool_policies_scope_team_chk CHECK (
        (scope = 'team' AND team_id IS NOT NULL) OR
        (scope = 'global' AND team_id IS NULL)
    )
);

COMMENT ON TABLE tool_policies IS '可配置策略表:命令审查与 skill/plugin/mcp 使用限制';
COMMENT ON COLUMN tool_policies.id IS '主键 UUIDv7';
COMMENT ON COLUMN tool_policies.scope IS '策略范围:global(全局) / team(团队)';
COMMENT ON COLUMN tool_policies.team_id IS '团队 ID,scope=team 时非空;级联删除';
COMMENT ON COLUMN tool_policies.role IS '作用角色:NULL(所有角色) / owner / admin / member';
COMMENT ON COLUMN tool_policies.rule_type IS '规则类型:command(命令) / tool(工具名) / skill / plugin / mcp';
COMMENT ON COLUMN tool_policies.match_mode IS '匹配模式:blacklist(黑名单子串) / whitelist(白名单子串) / regex / exact';
COMMENT ON COLUMN tool_policies.pattern IS '匹配内容:命令字符串、工具名或正则表达式';
COMMENT ON COLUMN tool_policies.action IS '命中后的动作:allow / deny';
COMMENT ON COLUMN tool_policies.priority IS '优先级,数字越大越优先,同优先级按 id 升序';
COMMENT ON COLUMN tool_policies.enabled IS '是否启用';
COMMENT ON COLUMN tool_policies.description IS '规则描述';
COMMENT ON COLUMN tool_policies.created_at IS '创建时间戳(毫秒)';
COMMENT ON COLUMN tool_policies.updated_at IS '更新时间戳(毫秒)';

CREATE INDEX IF NOT EXISTS idx_tool_policies_query ON tool_policies (scope, team_id, role, rule_type, enabled);
CREATE INDEX IF NOT EXISTS idx_tool_policies_priority ON tool_policies (priority DESC, id);
CREATE INDEX IF NOT EXISTS idx_tool_policies_team_id ON tool_policies (team_id);
```

### 7.2 MySQL（追加到 `backend-rs/sql/mysql/init.sql`）

```sql
CREATE TABLE IF NOT EXISTS tool_policies (
    id VARCHAR(36) PRIMARY KEY COMMENT '主键 UUIDv7',
    scope VARCHAR(16) NOT NULL COMMENT '策略范围:global / team',
    team_id VARCHAR(36) DEFAULT NULL COMMENT '团队 ID,scope=team 时非空',
    role VARCHAR(16) DEFAULT NULL COMMENT '作用角色:NULL / owner / admin / member',
    rule_type VARCHAR(16) NOT NULL COMMENT '规则类型:command / tool / skill / plugin / mcp',
    match_mode VARCHAR(16) NOT NULL COMMENT '匹配模式:blacklist / whitelist / regex / exact',
    pattern TEXT NOT NULL COMMENT '匹配内容',
    action VARCHAR(16) NOT NULL COMMENT '命中动作:allow / deny',
    priority INT NOT NULL DEFAULT 0 COMMENT '优先级,越大越优先',
    enabled BOOLEAN NOT NULL DEFAULT TRUE COMMENT '是否启用',
    description TEXT COMMENT '规则描述',
    created_at BIGINT NOT NULL COMMENT '创建时间戳(毫秒)',
    updated_at BIGINT NOT NULL COMMENT '更新时间戳(毫秒)',
    CONSTRAINT fk_tool_policies_team FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE,
    CONSTRAINT chk_tool_policies_scope CHECK (scope IN ('global','team')),
    CONSTRAINT chk_tool_policies_role CHECK (role IS NULL OR role IN ('owner','admin','member')),
    CONSTRAINT chk_tool_policies_rule_type CHECK (rule_type IN ('command','tool','skill','plugin','mcp')),
    CONSTRAINT chk_tool_policies_match_mode CHECK (match_mode IN ('blacklist','whitelist','regex','exact')),
    CONSTRAINT chk_tool_policies_action CHECK (action IN ('allow','deny')),
    CONSTRAINT chk_tool_policies_scope_team CHECK (
        (scope = 'team' AND team_id IS NOT NULL) OR
        (scope = 'global' AND team_id IS NULL)
    )
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='可配置策略表:命令审查与 skill/plugin/mcp 使用限制';

CREATE INDEX idx_tool_policies_query ON tool_policies (scope, team_id, role, rule_type, enabled);
CREATE INDEX idx_tool_policies_priority ON tool_policies (priority DESC, id);
CREATE INDEX idx_tool_policies_team_id ON tool_policies (team_id);
```

---

## 8. 缓存与集群一致性

### 8.1 设计

- 每个节点维护进程内 `PolicyCache`。
- 缓存内容：全局规则 + 各 team 规则。
- TTL：有 Redis 时 30 秒；无 Redis 时 5 秒。
- 写策略 API 成功后：
  1. 更新 DB
  2. invalidate 本节点缓存
  3. 若 Redis 事件总线存在，发布 `policies:changed`
- 启动时订阅 `policies:changed`，收到后清空本地缓存。

### 8.2 缓存结构

```rust
pub struct PolicyCache {
    pub global_rules: Vec<PolicyRule>,
    pub team_rules: HashMap<String, Vec<PolicyRule>>,
    pub loaded_at: Instant,
    pub ttl: Duration,
}

impl PolicyCache {
    pub fn is_fresh(&self) -> bool {
        self.loaded_at.elapsed() < self.ttl
    }

    pub fn invalidate(&mut self) {
        self.loaded_at = Instant::UNIX_EPOCH;
        self.global_rules.clear();
        self.team_rules.clear();
    }
}
```

### 8.3 查询路径

```rust
pub async fn evaluate(state: &AppState, input: &PolicyInput<'_>) -> PolicyDecision {
    // 1. 尝试读缓存
    {
        let cache = state.policy_cache.read().await;
        if cache.is_fresh() {
            let rules = collect_rules(&cache, input);
            return evaluate_against(&rules, input);
        }
    }

    // 2. 缓存过期或不存在:从 DB 加载全局+当前 team 规则
    let (global, team) = match load_rules_from_db(state, input.team_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "policy engine load failed, fail-open");
            return PolicyDecision::Allow;
        }
    };

    // 3. 写缓存
    {
        let mut cache = state.policy_cache.write().await;
        cache.global_rules = global;
        cache.team_rules.insert(input.team_id.to_string(), team);
        cache.loaded_at = Instant::now();
    }

    // 4. 用刚加载的规则决策
    let cache = state.policy_cache.read().await;
    let rules = collect_rules(&cache, input);
    evaluate_against(&rules, input)
}
```

### 8.4 Fail-open

`evaluate` 内部任何 DB/缓存/正则编译异常都返回 `PolicyDecision::Allow` 并记录 `tracing::warn`，不阻断 codex 工具调用。

---

## 9. 前端设计

### 9.1 菜单位置

- **全局策略**：平台管理员左侧导航新增入口
- **团队策略**：team 管理页面新增「策略」标签页

### 9.2 页面结构

1. 规则列表：类型 / 匹配模式 / 内容 / 动作 / 角色 / 优先级 / 启用 / 操作
2. 新建/编辑弹窗：表单包含 `rule_type`、`match_mode`、`pattern`、`action`、`role`、`priority`、`enabled`、`description`
3. 保存后提示：策略已保存，集群各节点将在数秒内生效

### 9.3 权限

- 全局策略页：路由守卫检查 `is_platform_admin`
- 团队策略页：检查当前用户在该 team 的角色

---

## 10. 实现文件清单

### 后端新增/修改

| 文件 | 动作 | 说明 |
|------|------|------|
| `backend-rs/sql/pg/init.sql` | 追加 | 创建 `tool_policies` 表 |
| `backend-rs/sql/mysql/init.sql` | 追加 | 创建 `tool_policies` 表 |
| `backend-rs/src/db/entities/tool_policy.rs` | 新增 | SeaORM entity |
| `backend-rs/src/db/entities/mod.rs` | 修改 | 注册新 entity |
| `backend-rs/src/services/policy_engine/mod.rs` | 新增 | 公共类型与入口 |
| `backend-rs/src/services/policy_engine/engine.rs` | 新增 | 匹配逻辑 |
| `backend-rs/src/services/policy_engine/store.rs` | 新增 | DB 查询与缓存 |
| `backend-rs/src/services/policy_engine/dto.rs` | 新增 | REST DTO |
| `backend-rs/src/api/policies.rs` | 新增 | 全局策略 API handler |
| `backend-rs/src/api/multitenant/policies.rs` | 新增 | team 策略 API handler |
| `backend-rs/src/api/mod.rs` | 修改 | 注册路由 |
| `backend-rs/src/api/hooks.rs` | 修改 | 集成策略引擎 |
| `backend-rs/src/services/workspace/audit_writer.rs` | 修改 | 增加 `policy_rule_id` |
| `backend-rs/src/state.rs` | 修改 | 注入 `policy_cache` |
| `backend-rs/src/main.rs` | 修改 | 初始化缓存与事件订阅 |

### 前端新增/修改

| 文件 | 动作 | 说明 |
|------|------|------|
| `web/src/routes/policies/` | 新增 | 全局策略页面 |
| `web/src/routes/team-policies/` | 新增 | 团队策略页面 |
| `web/src/components/policies/PolicyForm.tsx` | 新增 | 规则表单 |
| `web/src/components/policies/PolicyList.tsx` | 新增 | 规则列表 |
| `web/src/lib/api/policies.ts` | 新增 | API hooks |
| `web/src/routes.tsx` 或等价路由配置 | 修改 | 注册页面 |

---

## 11. 测试要点

1. 全局策略对全局 codex 调用生效。
2. team 策略仅对该 team 的调用生效。
3. role 专属规则优先级高于 `role=NULL` 规则。
4. `regex` 模式非法时规则不命中。
5. 缓存失效后新策略生效。
6. Redis 广播失效时各节点缓存一致。
7. 路径越界等硬编码安全规则不受策略影响。

---

## 12. 后续扩展

- 增加 `ask` 动作，接入现有审批流。
- 增加「策略生效时间窗口」字段。
- 增加策略命中率的 metrics 暴露。
- 支持 per-user 例外规则。

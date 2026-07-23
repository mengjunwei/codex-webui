-- tool_policies 策略表：定义工具/命令/MCP 等资源的访问控制规则
CREATE TABLE IF NOT EXISTS tool_policies (
    id VARCHAR(36) PRIMARY KEY,
    scope VARCHAR(16) NOT NULL CHECK (scope IN ('global','team')),
    team_id VARCHAR(36) REFERENCES teams(id) ON DELETE CASCADE,
    role VARCHAR(16) CHECK (role IS NULL OR role IN ('owner','admin','member')),
    rule_type VARCHAR(16) NOT NULL CHECK (rule_type IN ('command','tool','skill','plugin','mcp')),
    match_mode VARCHAR(16) NOT NULL CHECK (match_mode IN ('blacklist','whitelist','regex','exact')),
    pattern TEXT NOT NULL,
    action VARCHAR(16) NOT NULL CHECK (action IN ('allow','deny')),
    priority INT NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    description TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CONSTRAINT chk_tool_policies_scope_team CHECK (
        (scope = 'team' AND team_id IS NOT NULL) OR
        (scope = 'global' AND team_id IS NULL)
    )
);

COMMENT ON TABLE tool_policies IS '工具策略表，按 scope/team/role 控制命令、工具、skill、plugin、mcp 的访问规则';
COMMENT ON COLUMN tool_policies.id IS '策略唯一标识';
COMMENT ON COLUMN tool_policies.scope IS '策略范围：global 全局 / team 团队';
COMMENT ON COLUMN tool_policies.team_id IS '团队标识，scope=team 时必填，引用 teams.id';
COMMENT ON COLUMN tool_policies.role IS '适用角色：owner / admin / member，NULL 表示对所有角色生效';
COMMENT ON COLUMN tool_policies.rule_type IS '规则类型：command / tool / skill / plugin / mcp';
COMMENT ON COLUMN tool_policies.match_mode IS '匹配模式：blacklist / whitelist / regex / exact';
COMMENT ON COLUMN tool_policies.pattern IS '匹配模式内容';
COMMENT ON COLUMN tool_policies.action IS '命中后的动作：allow / deny';
COMMENT ON COLUMN tool_policies.priority IS '优先级，数值越大越优先，默认 0';
COMMENT ON COLUMN tool_policies.enabled IS '是否启用，默认 TRUE';
COMMENT ON COLUMN tool_policies.description IS '策略描述';
COMMENT ON COLUMN tool_policies.created_at IS '创建时间戳（毫秒）';
COMMENT ON COLUMN tool_policies.updated_at IS '更新时间戳（毫秒）';

CREATE INDEX idx_tool_policies_scope_team_role_type_enabled ON tool_policies (scope, team_id, role, rule_type, enabled);
CREATE INDEX idx_tool_policies_priority_id ON tool_policies (priority DESC, id);
CREATE INDEX idx_tool_policies_team_id ON tool_policies (team_id);

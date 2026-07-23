-- tool_policies 策略表：定义工具/命令/MCP 等资源的访问控制规则
CREATE TABLE IF NOT EXISTS tool_policies (
    id VARCHAR(36) PRIMARY KEY COMMENT '策略唯一标识',
    scope VARCHAR(16) NOT NULL COMMENT '策略范围：global 全局 / team 团队',
    team_id VARCHAR(36) NULL COMMENT '团队标识，scope=team 时必填，引用 teams.id',
    role VARCHAR(16) NULL COMMENT '适用角色：owner / admin / member，NULL 表示对所有角色生效',
    rule_type VARCHAR(16) NOT NULL COMMENT '规则类型：command / tool / skill / plugin / mcp',
    match_mode VARCHAR(16) NOT NULL COMMENT '匹配模式：blacklist / whitelist / regex / exact',
    pattern TEXT NOT NULL COMMENT '匹配模式内容',
    action VARCHAR(16) NOT NULL COMMENT '命中后的动作：allow / deny',
    priority INT NOT NULL DEFAULT 0 COMMENT '优先级，数值越大越优先，默认 0',
    enabled BOOLEAN NOT NULL DEFAULT TRUE COMMENT '是否启用，默认 TRUE',
    description TEXT NULL COMMENT '策略描述',
    created_at BIGINT NOT NULL COMMENT '创建时间戳（毫秒）',
    updated_at BIGINT NOT NULL COMMENT '更新时间戳（毫秒）',
    CONSTRAINT chk_tool_policies_scope_team CHECK (
        (scope = 'team' AND team_id IS NOT NULL) OR
        (scope = 'global' AND team_id IS NULL)
    ),
    CONSTRAINT chk_tool_policies_scope CHECK (scope IN ('global','team')),
    CONSTRAINT chk_tool_policies_role CHECK (role IS NULL OR role IN ('owner','admin','member')),
    CONSTRAINT chk_tool_policies_rule_type CHECK (rule_type IN ('command','tool','skill','plugin','mcp')),
    CONSTRAINT chk_tool_policies_match_mode CHECK (match_mode IN ('blacklist','whitelist','regex','exact')),
    CONSTRAINT chk_tool_policies_action CHECK (action IN ('allow','deny')),
    FOREIGN KEY (team_id) REFERENCES teams(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='工具策略表，按 scope/team/role 控制命令、工具、skill、plugin、mcp 的访问规则';

CREATE INDEX idx_tool_policies_scope_team_role_type_enabled ON tool_policies (scope, team_id, role, rule_type, enabled);
CREATE INDEX idx_tool_policies_priority_id ON tool_policies (priority DESC, id);
CREATE INDEX idx_tool_policies_team_id ON tool_policies (team_id);

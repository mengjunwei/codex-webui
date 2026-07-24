# 用户登录 Token 与内置管理员实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 增加可设置过期时间的用户登录 Token、用户名登录、前端 Token 管理和内置 admin 管理员。

**Architecture:** 新增独立 auth_tokens 数据表与 SeaORM entity/service/API；Token 仅保存 SHA-256 哈希，登录成功复用现有 JWT 会话。users 增加 username，初始化 SQL 幂等创建内置 admin。前端账户设置负责 Token CRUD，登录页增加 Token 登录模式。

**Tech Stack:** Rust、Axum、SeaORM 2、Argon2、SHA-256、PostgreSQL/MySQL、React、TanStack Query。

## Global Constraints

- Token 明文只在创建成功响应返回一次，数据库只保存 SHA-256 哈希。
- Token 必须有明确未来过期时间，最长不超过一年。
- 无效、过期、撤销 Token 统一返回 401。
- 内置账号为 `admin`、邮箱 `admin@codex.local`、密码 `Codex@Agent+-`、长期有效、平台管理员。
- 保持现有邮箱密码登录兼容；新增用户名登录。
- 数据库变更同步写入 PostgreSQL 和 MySQL 初始化 SQL，不自动迁移。
- 不修改与本功能无关的业务逻辑和用户现有未跟踪文件。

---

### Task 1: 数据库与 SeaORM 模型

**Files:**
- Modify: `backend-rs/sql/pg/init.sql`
- Modify: `backend-rs/sql/mysql/init.sql`
- Modify: `backend-rs/src/db/entities/user.rs`
- Create: `backend-rs/src/db/entities/auth_token.rs`
- Modify: `backend-rs/src/db/entities/mod.rs`

- [ ] 给 users 增加 username 字段与唯一索引。
- [ ] 增加 auth_tokens 表、索引、外键和撤销/过期字段。
- [ ] 增加 SeaORM entity，字段为 `id/user_id/name/token_hash/token_prefix/created_at/expires_at/revoked_at/last_used_at`。
- [ ] 增加管理员初始化 SQL，使用固定 Argon2 PHC 哈希并保持幂等。
- [ ] 运行 `cargo check` 验证 entity。

### Task 2: 认证服务与 Token API

**Files:**
- Modify: `backend-rs/src/services/multitenant/auth.rs`
- Modify: `backend-rs/src/api/multitenant/handlers.rs`
- Modify: `backend-rs/src/api/multitenant/routing.rs`
- Modify: `backend-rs/src/error.rs`

- [ ] 注册、登录 DTO 增加 identifier/username 兼容字段。
- [ ] 增加用户名校验和唯一查询。
- [ ] 增加 Token 生成、哈希、创建、列表、撤销和 token_login 函数。
- [ ] 创建时校验名称、未来过期时间和一年上限。
- [ ] 增加 Token 登录及当前用户 Token CRUD 路由。
- [ ] 成功 Token 登录返回现有 AuthResp。

### Task 3: 后端测试

**Files:**
- Modify/Create: `backend-rs/tests/multitenant_auth_test.rs`
- Modify: `backend-rs/src/services/multitenant/auth.rs`

- [ ] 测试 Token 哈希和前缀不泄露明文。
- [ ] 测试有效 Token 登录、过期 Token 拒绝、撤销 Token 拒绝。
- [ ] 测试用户名登录和 admin 标记。
- [ ] 运行 `cargo test --locked`。

### Task 4: 前端登录与账户设置

**Files:**
- Modify: `web/src/components/login.tsx`
- Modify: `web/src/components/settings/account/account-settings.tsx`
- Modify: `web/src/components/settings/account/account-login-dialog.tsx`（如共享登录组件需要）
- Modify: `web/src/lib/mt-client.ts`（仅需补充 API 类型时）

- [ ] 登录页增加密码/Token 模式，Token 模式调用 `/api/mt/auth/token`。
- [ ] 账户设置增加 Token 创建表单和日期时间输入。
- [ ] 创建成功一次性显示明文并提供复制按钮。
- [ ] 列表显示元数据和状态，支持撤销。
- [ ] 运行前端 lint/typecheck/build。

### Task 5: 最终验证与差异审查

- [ ] 运行 `cargo fmt --check`、`cargo check --locked`、`cargo check --locked --features memberlist-backend`、`cargo test --locked`。
- [ ] 运行前端测试/构建命令。
- [ ] 检查 Git diff 只包含本功能文件；删除临时构建目录。
- [ ] 确认管理员密码只出现在初始化逻辑所需位置，数据库无明文密码。

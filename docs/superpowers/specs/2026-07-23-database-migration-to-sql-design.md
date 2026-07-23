# 数据库迁移转 SQL 初始化文件 — 设计规格

> 状态：草案（待用户审阅）
> 日期：2026-07-23
> 作者：Claude（基于用户指令与代码事实）

## 1. 背景与目标

### 1.1 用户原始诉求

> 「`backend-rs/src/db/migration` 这里面的迁移都写入到 sql 文件中，我自己跑数据库初始化，然后把 rs 删除了」

即：把 7 个 SeaORM Rust 迁移文件改写为可手工执行的 SQL 初始化脚本，然后删除对应的 Rust 实现，让数据库初始化完全交给运维手工跑 SQL。

### 1.2 范围

**包含：**
- 把 7 个 `m2026*.rs` 中所有的 `up` 逻辑翻译为 2 份可手工执行的 SQL（PG + MySQL 各一份）。
- 删除 `backend-rs/src/db/migration/` 整个目录。
- 修改 `backend-rs/src/db/mod.rs` 移除 `pub mod migration;`。
- 修改 `backend-rs/src/main.rs` 移除 `Migrator::up` 调用。
- 修改 `backend-rs/Cargo.toml` 移除 `sea-orm-migration` 依赖。
- 交付 `backend-rs/sql/{pg,mysql}/init.sql` 与 `backend-rs/sql/README.md`。

**不包含：**
- 不修改 `backend-rs/src/db/entity.rs` 与 `backend-rs/src/db/entities/mod.rs`（SeaORM 实体定义不依赖迁移系统）。
- 不修改业务代码（services / routes / handlers）。
- 不调整数据库连接配置或 SeaORM 连接代码。
- 不增加新表 / 改字段语义。

## 2. 探索结论（事实纠正）

> **与 2026-07-22 之前的探索报告（drizzle + `migrations.rs`）完全不同——本任务对应的是 `cd83074` 之后的 SeaORM 多方言迁移体系。**

- **真实路径**：`backend-rs/src/db/migration/`（7 个 `m2026*.rs` + `mod.rs`）。
- **ORM**：SeaORM 1.1（`backend-rs/Cargo.toml:56`），数据库连接走 `sea_orm::Database::connect`，迁移走 `sea-orm-migration = "1.1"`。
- **支持的数据库**：PostgreSQL 与 MySQL（多方言）；旧 SQLite 链路已删除。
- **追踪表**：`schema_migrations` 由 `sea-orm-migration` 自动管理。
- **启动入口**：`backend-rs/src/main.rs:57` 的 `Migrator::up(&db, None)`。
- **业务实体**：`backend-rs/src/db/entity.rs`（5 个 SeaORM 实体，对应 token_usage_snapshot / turn_diff / setting / pending_server_request / turn_error），不依赖迁移系统。
- **测试引用**：`grep -rn "Migrator\|m2026" backend-rs/tests/` 返回空——无需改测试。

## 3. 7 个迁移文件清单与翻译要点

按 `backend-rs/src/db/migration/mod.rs:22-30` 加载顺序：

| # | 源文件 | 关键 DDL | 方言注意点 |
|---|---|---|---|
| 1 | `m20260719_000001_combined_schema.rs` | 19 张表（users / teams / team_members / invitations / refresh_tokens / threads / team_api_keys / user_api_keys / audit_log / token_usage_snapshots / turn_diffs / settings / pending_server_requests / turn_errors / team_quotas / team_routes / session_replicas / workspace_audit / thread_resume_cache）+ 11 条索引 | `JSON` 类型在 PG 译为 `JSON`，MySQL 译为 `JSON`；`COMMENT ON` 仅 PG；`BOOLEAN` 双库通用；`VARCHAR(36)` 存 UUIDv7 |
| 2 | `m20260720_000001_rbac_permissions.rs` | ALTER users ADD is_platform_admin；ALTER team_members ADD CHECK 约束；CREATE role_permissions + 24 行 seed INSERT | seed 需幂等：用 `INSERT ... ON CONFLICT DO NOTHING`（PG）/ `INSERT IGNORE`（MySQL） |
| 3 | `m20260721_000001_session_replicas_per_thread.rs` | ALTER TABLE session_replicas RENAME TO session_replicas_old；CREATE 新表 session_replicas（per-thread PK）；数据迁移 INSERT；DROP old；PG 注释 | PG 用 `ON CONFLICT DO NOTHING`；MySQL 用 `INSERT IGNORE`；`ALTER TABLE IF EXISTS` 仅 PG 支持 |
| 4 | `m20260722_000001_cluster_extensions.rs` | 3 张表（cluster_extensions / cluster_extension_files / cluster_extension_holders）+ 3 条普通索引 | 索引名 `idx_ext_kind_name` / `idx_ext_enabled` / `idx_extfile_ext`；后续 0002 会改为 UNIQUE |
| 5 | `m20260722_000002_cluster_extensions_unique.rs` | DELETE 去重（按方言分支）；DROP 旧普通索引；CREATE 2 条 UNIQUE 索引 | PG 自连接 `DELETE FROM ... USING ...`；MySQL `DELETE alias FROM ... JOIN`；新索引名带 `_unique` 后缀 |
| 6 | `m20260722_000003_cluster_extensions_marketplace.rs` | ALTER cluster_extensions ADD COLUMN marketplace；CREATE INDEX idx_ext_marketplace | PG 支持 `ADD COLUMN IF NOT EXISTS`；MySQL 不支持，依赖 `.ok()` 容错 |
| 7 | `m20260722_000004_cluster_extensions_holder_pk.rs` | DELETE 去重；ALTER cluster_extension_holders ADD PRIMARY KEY（复合） | PG 用 `ctid` 自连接去重；MySQL 用临时表 TRUNCATE+回插；PG 命名约束 `pk_ext_holder`，MySQL 主键无名 |

## 4. 设计方案（用户已选定方案 A）

### 4.1 目录结构

```
backend-rs/sql/
├── README.md                        # 一句话运维提示
├── pg/
│   └── init.sql                     # PostgreSQL 初始化脚本
└── mysql/
    └── init.sql                     # MySQL 初始化脚本
```

### 4.2 SQL 内容组织

每份 `init.sql` 内部按 7 个迁移顺序排列，每段用注释头分隔：

```sql
-- ============================================================
-- 1/7  m20260719_000001_combined_schema
-- ============================================================

-- 1.1 users
CREATE TABLE IF NOT EXISTS users (...);
COMMENT ON TABLE users IS '...';
...
```

**幂等策略：**
- 所有 DDL 加 `IF NOT EXISTS`（PG 原生支持；MySQL 8.0+ 支持 `CREATE TABLE IF NOT EXISTS`）。
- `ALTER TABLE ADD COLUMN IF NOT EXISTS`（PG）；MySQL 不支持——README 注明"假定全新库或先 DROP 库"。
- 索引：PG 用 `CREATE INDEX IF NOT EXISTS`；MySQL 在脚本注释中说明"假定全新库"。
- seed 数据用 `INSERT ... ON CONFLICT DO NOTHING`（PG）/ `INSERT IGNORE`（MySQL）。
- 不使用 `DROP`（除非要重建的索引），避免误操作清空。

**事务：**
- PG 版用 `BEGIN; ... COMMIT;` 包裹全部 7 段。
- MySQL 版：默认 autocommit，README 提示运维加 `--single-transaction` 标志或手动包事务。

### 4.3 方言差异处理

| 关注点 | PostgreSQL (`pg/init.sql`) | MySQL (`mysql/init.sql`) |
|---|---|---|
| 表/字段注释 | `COMMENT ON TABLE ... IS '...';` + `COMMENT ON COLUMN ... IS '...';` | 改写为内联 `COMMENT '...'` 形式（在 CREATE TABLE 内每列加 `COMMENT`）；表注释用 `ALTER TABLE ... COMMENT = '...'` |
| `JSON` 类型 | `JSON` | `JSON`（5.7.8+ / 8.0 通用） |
| `BOOLEAN` | `BOOLEAN` | `BOOLEAN`（= `TINYINT(1)`） |
| 字符集 | 默认 `utf8` | 显式 `CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci` |
| 字符长度 | `VARCHAR(36)` | `VARCHAR(36) CHARACTER SET utf8mb4` |
| 字符串引号 | `E'...'`（避免转义） | 普通 `'...'`，反斜杠需双写 |
| `CHECK` 约束 | `CHECK (...)` 强制 | `CHECK (...)`（8.0.16+ 强制；5.7 忽略） |
| 复合主键 | `PRIMARY KEY (a, b)` | 同左 |
| 唯一索引 | `CREATE UNIQUE INDEX IF NOT EXISTS` | `CREATE UNIQUE INDEX`（依赖全新库） |
| 自连接去重 | `DELETE FROM a USING b WHERE ...` | `DELETE a FROM a INNER JOIN b ON ...` |
| 临时表 | `CREATE TEMP TABLE` | `CREATE TEMPORARY TABLE` |
| 复合主键 ADD | `ADD CONSTRAINT pk_xxx PRIMARY KEY (a,b)` | `ADD PRIMARY KEY (a,b)`（无名） |

### 4.4 Rust 侧删除清单

| 文件/位置 | 动作 |
|---|---|
| `backend-rs/src/db/migration/` | 整目录删除（7 个 `m2026*.rs` + `mod.rs`） |
| `backend-rs/src/db/mod.rs:5` | 删除 `pub mod migration;`（保留 `pub mod entity;` 和 `pub mod entities;`） |
| `backend-rs/src/main.rs:28` | 删除 `use sea_orm_migration::MigratorTrait;` |
| `backend-rs/src/main.rs:57-59` | 删除 `Migrator::up(&db, None).await?;`（3 行） |
| `backend-rs/Cargo.toml:57` | 删除 `sea-orm-migration = { ... }` 整行 |

### 4.5 README 内容

```markdown
# 数据库初始化

1. 选方言：PostgreSQL → `pg/init.sql`；MySQL → `mysql/init.sql`。
2. 跑：`psql -d <db> -f pg/init.sql` 或 `mysql -D <db> < mysql/init.sql`。
3. 假定全新空库；已存在表会被 `IF NOT EXISTS` 跳过但不会更新。
4. 删 Rust 迁移后启动顺序：`Db connect → bootstrap platform admins → ...`（不再有 Migrator::up）。
```

## 5. 错误处理与可重入

- **幂等**：所有 DDL 加 `IF NOT EXISTS`，连跑两次不报错。
- **失败回滚**：PG 整批由 `BEGIN/COMMIT` 包；MySQL 推荐 `--single-transaction`。
- **不依赖** `schema_migrations`（按用户决定：删迁移表）。
- **不静默失败**：不在 SQL 中写 `IF NOT EXISTS` 之外需要 `.ok()` 的容错——`init.sql` 是「全新初始化」语义，重跑意义不大。

## 6. 验证策略

1. 拉一个干净 PG 库（`createdb codex_webui_test`），跑 `psql -f backend-rs/sql/pg/init.sql`，用 `\dt` 列出表，对照 §3 涉及的所有表名一一核对（共 19+1+1+3 = 24 张表）。
2. 拉一个干净 MySQL 库（`CREATE DATABASE codex_webui_test`），跑 `mysql < backend-rs/sql/mysql/init.sql`，`SHOW TABLES` 同核对。
3. 幂等测试：连跑同一份 SQL 确认无错误。
4. `cargo build` 确认无未使用导入、无悬挂引用。
5. `cargo test` 跑一次（应全部通过；测试不依赖迁移）。
6. （可选）启动一次 `backend-rs` 进程，确认 `bootstrap platform admins` 正常工作。

## 7. 风险与回退

### 7.1 风险

| 风险 | 缓解 |
|---|---|
| 已有数据库已跑过 `Migrator::up` → `schema_migrations` 残留 7 条记录 | 表已存在，IF NOT EXISTS 跳过；孤儿行无影响（按用户决定：删迁移表） |
| `CHECK` 约束在 MySQL 5.7 不强制 → 应用层需保留校验 | 现有应用层已校验；SQL 中保留 CHECK 语法仅 8.0+ 生效 |
| MySQL `ALTER TABLE ADD COLUMN IF NOT EXISTS` 不支持 | 假定全新库；README 注明 |
| PG 旧版本 < 13 不支持 `JSON` 类型的某些操作 | 现有 migration 已用 JSON；新增要求 README 注明 PG ≥ 13 |
| 删除 Rust 迁移后业务代码 panic 引用某 `Migration` 类型 | 业务代码未引用 Migration 类型（grep 已确认） |

### 7.2 回退

- PR 分两步：
  1. 先加 SQL 文件 + README，不删 Rust 迁移——便于在生产库上手工跑 SQL 验证。
  2. SQL 验证通过后再合并删 Rust 迁移的 PR。

## 8. 实施步骤（待 writing-plans 展开）

1. 创建 `backend-rs/sql/pg/init.sql` 与 `backend-rs/sql/mysql/init.sql`。
2. 创建 `backend-rs/sql/README.md`。
3. 删除 `backend-rs/src/db/migration/` 整目录。
4. 修改 `backend-rs/src/db/mod.rs:5`。
5. 修改 `backend-rs/src/main.rs:28` 和 `:57-59`。
6. 修改 `backend-rs/Cargo.toml:57`。
7. 跑 `cargo build` + `cargo test`。
8. 在干净库上跑两份 SQL 验证表结构。

## 9. 待用户确认事项

- [ ] §4.1 目录结构（`backend-rs/sql/{pg,mysql}/init.sql`）OK 吗？
- [ ] §4.3 方言差异表 OK 吗？
- [ ] §4.4 Rust 删除清单完整无遗漏？
- [ ] §6 验证策略足够吗？
- [ ] §7 风险与回退步骤接受？

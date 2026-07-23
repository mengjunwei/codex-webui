# 数据库初始化

**挑一个方言：**

| 方言 | 脚本 | 最低版本 | 执行命令 |
|---|---|---|---|
| PostgreSQL | `pg/init.sql` | 13+ | `psql -d <db> -f pg/init.sql` |
| MySQL | `mysql/init.sql` | 8.0.29+ | `mysql -D <db> < mysql/init.sql` |

## 重要

- 脚本是**全新空库**初始化语义，假定没有任何业务表。
- 所有 `CREATE TABLE` 使用 `IF NOT EXISTS`，可重跑。
- 重跑**不会**更新已存在表的列/索引，详见下方 MySQL 方言限制。
- 启动顺序变化：`Db connect → bootstrap platform admins → ...`，**不再有 `Migrator::up`**。
- 启动后端进程前必须先在 DB 上跑此脚本，否则连接正常但所有查询失败。
- `mysql` 客户端不支持 `--single-transaction`；本脚本也不保证整批原子执行，执行失败时可能只创建部分表，请修复问题后重跑。
- 来源迁移位于 `backend-rs/src/db/migration/`（即将删除），保留追溯。

## MySQL 方言限制

下列写法在 MySQL 中不被支持，本脚本通过替代方案规避：

1. **不支持 `ALTER TABLE ... ADD/DROP COLUMN IF [NOT] EXISTS`**
   - MySQL 不允许 `IF [NOT] EXISTS` 子句出现在 `ADD/DROP COLUMN` 中。
   - 影响：建表阶段不再追加列；后续如需演进 schema，请手工迁移或新建表。

2. **不支持 `DROP INDEX IF EXISTS`**
   - MySQL 不允许 `IF EXISTS` 子句出现在 `DROP INDEX` 中。
   - 影响：脚本不会清理已存在的同名索引；请确认目标库为全新空库。

3. **主键 `ADD` 无名约束**
   - `ALTER TABLE ... ADD PRIMARY KEY (...)` 在 MySQL 中无法为约束指定显式名称。
   - 影响：脚本中省略约束名，依赖 MySQL 自动生成（如 `PRIMARY`）。

4. **Seed 数据使用 `INSERT IGNORE` 而非 `INSERT ... ON CONFLICT`**
   - `ON CONFLICT DO NOTHING` 是 PostgreSQL 语法，MySQL 不支持。
   - 影响：MySQL seed 使用 `INSERT IGNORE` 实现幂等，行为等价（冲突行被丢弃）。

> PG 脚本无上述限制，可正常使用 `ADD COLUMN IF NOT EXISTS` / `DROP INDEX IF EXISTS` / `ON CONFLICT`。
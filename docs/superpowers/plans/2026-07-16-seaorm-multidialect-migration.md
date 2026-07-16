# SeaORM 全量迁移 + PG/MySQL 双方言

- **日期**:2026-07-16
- **分支**:`feat/multitenant-platform`
- **目标**:把 multitenant 现有 sqlx 代码 + 业务 rusqlite 代码**全部迁到 SeaORM**,实现"一套代码兼容 PostgreSQL + MySQL";删除 rusqlite/sqlx 原生查询/旧迁移。
- **替代**:`2026-07-16-sqlite-to-pg-migration.md`(sqlx 方案,已废弃)。
- **选型理由**:SeaORM 原生 async + `DatabaseConnection` enum 自动适配方剂;删 rusqlite 后无 `libsqlite3-sys` 冲突(设计文档 §1 已留后手"待 rusqlite 全迁后再评估 SeaORM")。

---

## 一、影响面(已查证)

### multitenant 现有 sqlx 代码(8 张表,要重写成 SeaORM entity)

文件:`auth.rs / teams.rs / api_keys.rs / audit.rs / handlers.rs`。关键 PG 方言依赖:
- **大量 `RETURNING`**(auth/teams/api_keys 的 insert 返回行)-- MySQL 不支持,SeaORM 内部按 backend 处理。
- **`ON CONFLICT (id) DO NOTHING`**(handlers.rs:376,threads 元数据双写)-- MySQL 用 `ON DUPLICATE KEY`。
- 事务 `&mut *tx`、`query_as::<_, T>` 行映射。
- 表(8 张):`users / teams / team_members / invitations / refresh_tokens / threads / team_api_keys / audit_log`。

### 业务 rusqlite 代码(5 张表)

文件:`sqlite_handlers.rs(7) / event_subscribers.rs(7) / settings/{mod,handlers,reconcile}.rs / db/{mod,migrations}.rs`。
- upsert:`event_subscribers` 的 `INSERT ... ON CONFLICT(thread_id,turn_id) DO UPDATE SET x=excluded.x`(token_usage/turn_diffs/turn_errors/pending)。
- SettingsReader 同步(`&Db`)+ 5 个非 async 难点(files.rs 工作区根计算链级联 13 handler / build_router / from_settings / update_batch 闭包 / resolve 辅助)。
- 表(5 张):`token_usage_snapshots / turn_diffs / settings / pending_server_requests / turn_errors`。
- `codex_status_config.rs` 不用 DB,不动。

### 测试现状

- multitenant 逻辑测试(api_keys/auth/event_bus/routing/teams)不连 DB。
- `migration_test.rs` / `settings_crud_test.rs` 用 SQLite in-memory(`rusqlite::Connection::open_in_memory`)-- 迁 SeaORM 后要改 PG/MySQL 夹具。

---

## 二、核心技术决策

| 编号 | 决策 | 理由 |
|---|---|---|
| D1 | **放弃 `mt` schema**:所有表放默认 schema(PG public / MySQL 默认库),表名不加前缀 | MySQL 无 schema 概念;全迁后同库只有这套表无冲突;消除 `CREATE SCHEMA` + `search_path` 的 PG 专属依赖 |
| D2 | **迁移换 `sea-orm-migration`**:用 `SchemaManager` + `sea_query` builder API(跨方言自动生成),必要时 `backend()` 分支 raw SQL | 替代手动 sqlx 迁移 + drizzle;一套迁移跑 PG/MySQL |
| D3 | **RETURNING 交 SeaORM**:`ActiveModel::insert().exec(db)` 拿主键,需完整行则 `find_by_id` 回查 | SeaORM 内部 PG 用 RETURNING、MySQL 用 last_insert_id;应用层不写 RETURNING |
| D4 | **upsert 封装 helper**:按 `db.get_database_backend()` 分支--PG/SQLite 用 `on_conflict`、MySQL 用 `on_duplicate_key_update` | 统一 threads DO NOTHING + 业务表 DO UPDATE 两处 upsert;实施时验证 sea_query `on_duplicate_key_update` API |
| D5 | **连接层 `DatabaseConnection`**:AppState 用 `sea_db: DatabaseConnection`(enum,Pg/MySql/Sqlite),`DATABASE_URL` scheme 决定方言 | 一套查询代码自动适配方剂;PG/MySQL 由连接串切换 |
| D6 | **类型约定不变**:VARCHAR(36) UUIDv7 / BIGINT i64 毫秒 / BOOLEAN / TEXT,不用 JSON/ENUM/ARRAY | 已为多方言铺路(§3.2);SeaORM `i64`/`String`/`bool` 直接映射 |
| D7 | **5 阶段渐进,过渡期 AppState 双字段**(`db` SQLite + `sea_db` SeaORM) | 避免一次性全切连锁报错;每阶段 `cargo check` + test 绿 |

---

## 三、Entity 清单(13 个)

**multitenant(8)**:`users / teams / team_members / invitations / refresh_tokens / threads / team_api_keys / audit_log`
**业务(5)**:`token_usage_snapshots / turn_diffs / settings / pending_server_requests / turn_errors`

- 复合主键表(5):`team_members`(team_id,user_id)、`token_usage_snapshots`、`turn_diffs`、`pending_server_requests`、`turn_errors`--用 SeaORM `PrimaryKey` 多列。
- 放 `src/multitenant/entity/` 与 `src/entity/`(业务)。

---

## 四、分阶段实施

### 阶段 0:依赖 + 连接层 + 迁移框架

- `Cargo.toml`:加 `sea-orm`(features:`sqlx-postgres`+`sqlx-mysql`+`runtime-tokio-rustls`+`macros`)、`sea-orm-migration`;保留 `rusqlite`、`sqlx`(过渡)。版本对齐:sea-orm 0.12 用 sqlx 0.7(与现有一致)。
- `state.rs`:新增 `pub sea_db: DatabaseConnection`(必选);保留 `db: Arc<Db>`、`mt_pg: Option<PgPool>`(过渡)。
- `config.rs`:`database_url` 改必选;`db_path` 保留(阶段 3 删)。
- `main.rs`:`DatabaseConnection::connect(&cfg.database_url)` 必选初始化;跑 sea-orm-migrator;保留 SQLite(过渡)。
- 新建 `src/migration/`(migrator + 每个表的 `MigrationTrait`,多方言建表,放弃 `CREATE SCHEMA`)。
- **验证**:`cargo check` 绿;PG + MySQL 均能建表。

### 阶段 1:multitenant entity 重写

- 建 8 个 multitenant entity。
- 重写 `auth/teams/api_keys/audit/handlers` 的 sqlx 查询为 SeaORM(`Entity::find/filter/insert/update/delete`、事务 `db.transaction`)。
- RETURNING -> insert + `find_by_id`;threads DO NOTHING -> upsert helper(D4)。
- `require_pool` 改返回 `&DatabaseConnection`;删 `mt_pg` 字段及 middleware 的 `is_none()` 守卫。
- **验证**:`cargo check` + multitenant 逻辑测试绿 + PG 手动验证(quickstart curl)。

### 阶段 2:业务代码 SeaORM + settings async 改造

- 建 5 个业务 entity。
- `sqlite_handlers`:动态 `IN` -> `Entity::find().filter(Column::X.is_in(vec))`;`parse_pending_row` -> entity 直接映射。
- `event_subscribers`:upsert -> helper(D4);`spawn_all/record_server_request` 改传 `DatabaseConnection`;时间用 `now_ms()`;`realtime.rs` 调用加 `.await`。
- `settings`:`SettingsReader` 持 `DatabaseConnection`,全 async;`write_setting` async;`reconcile` async;5 个非 async 难点处理(build_router async / compute_workspace_roots async 级联 13 handler / from_settings async / update_batch 闭包重构先 collect 再 for await / resolve 辅助 async);SQL `?1`/`strftime` -> SeaORM + `now_ms()`。
- 测试 9 处改 `#[tokio::test]`。
- **验证**:`cargo check` + test 绿;settings/业务表读写走 SeaORM。

### 阶段 3:清理 rusqlite + 旧迁移 + 旧 sqlx

- 删 `db/mod.rs`、`db/migrations.rs`、`multitenant/migration.rs`(旧手动迁移)、`drizzle/`、`drizzle.config.ts`。
- 删 `state.db`、`mt_pg`(已删)字段;`sea_db` 改名 `db`(或保留)。
- `config.rs`:删 `db_path` + `resolve_db_path` + `dirs_or_home` + 3 测试。
- `main.rs`:删 SQLite `Db::open` + 旧 `run_migrations` + 旧 `reconcile`。
- `Cargo.toml`:删 `rusqlite`;`sqlx` 仅留 SeaORM 依赖(不开 sqlite)。
- `lib.rs`:调整模块声明。
- **验证**:`grep -rn rusqlite src/` 无实际调用;`cargo check` + test 绿。

### 阶段 4:PG + MySQL 双方言测试

- 测试夹具:`docker-compose` 起 PG + MySQL;或 `testcontainers-rs` 动态起库;环境变量 `TEST_PG_URL` / `TEST_MYSQL_URL`。
- `migration_test.rs`:改 sea-orm-migrator,PG + MySQL 双验证建表 + 幂等。
- `settings_crud_test.rs`:改 SeaORM + 双方言。
- 新增 multitenant DB 集成测试(原仅逻辑测试),双方言验证关键 CRUD。
- **验证**:PG + MySQL 两套测试全绿。

---

## 五、风险与应对

| 风险 | 应对 |
|---|---|
| R1 sea-orm-migration 多方言 SQL 差异(PG `REFERENCES`/`BOOLEAN` vs MySQL 微异) | 优先 `sea_query` builder API(自动方言);raw SQL 按 `backend()` 分支 |
| R2 upsert 跨方言(threads DO NOTHING + 业务 DO UPDATE) | helper 按 backend 分支:PG `on_conflict`/MySQL `on_duplicate_key_update`;实施时验证 sea_query API,必要时改"先查后写" |
| R3 SeaORM 复合主键(5 表)API 略繁 | 用 `PrimaryKey` 多列 + `find_by_id` 多参;参考 SeaORM 文档 |
| R4 sqlx 版本对齐(sea-orm 0.12 用 sqlx 0.7) | 现有 sqlx 0.7,版本一致;sea-orm 的 sqlx-postgres/mysql feature 与现有 sqlx feature 合并 |
| R5 settings async 传导面大(13 handler 级联) | 阶段 2 单独隔离;先改底层 `compute_workspace_roots` 再逐层上传,小步编译 |
| R6 双方言测试夹具(PG+MySQL docker) | testcontainers 或 docker-compose;CI 跑两套 |
| R7 SeaORM 与 rusqlite 共存期冲突 | 阶段 0-2 共存:SeaORM 不开 sqlite 后端(只 sqlx-postgres/mysql),不牵 libsqlite3-sys,与 rusqlite bundled 共存(验证;若冲突则提前到阶段 3 删 rusqlite) |
| R8 多租户隔离缺口(4 thread 表无 team_id) | 本次不补;靠 conversation_id 全局唯一 + `require_thread_team` 成员校验;后续优化 |

---

## 六、验证标准

- **每阶段**:`cargo check` 通过 + `cargo test --lib` 全绿(现有 45 单测无回归)。
- **最终**:
  - `grep -rn rusqlite backend-rs/src/` 无实际调用
  - `Cargo.toml` 无 `rusqlite`,有 `sea-orm` + `sea-orm-migration`
  - 无 `DATABASE_URL` 启动报错;配 PG 或 MySQL 连接串均能启动 + 建表 + 跑通 `/api/*`
  - PG + MySQL 双方言集成测试全绿

---

## 七、不在本次范围

- 补 `team_id` 多租户隔离(R8,后续)
- 历史数据迁移(全新开始)
- `codex_status_config`(不用 DB)
- M4/M5/M6 其他待办(failover、Prometheus、计费/安全审计)

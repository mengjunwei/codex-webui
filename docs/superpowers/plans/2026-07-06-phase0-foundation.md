> ⚠️ **历史快照**：本文档是实施时的步骤记录，已不反映当前架构。
> 配置系统已重构为 TOML-only（无 dotenvy / .env / Config::from_env），
> 节点角色已移除（所有节点均 ingress+worker 一体）。

# Phase 0 — Foundation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 产出可启动的 Rust 二进制地基——配置 / 日志 / SQLite+迁移 / settings 读取 / 错误处理 / 认证中间件 / 健康检查 + 登录端点 / 优雅关闭——作为后续所有模块的底座。

**Architecture:** 单 crate `codex-webui`（二进制）。axum + tokio；启动时按依赖顺序构造单例 service 装入 `Arc<AppState>`，经 axum `State` 注入。NestJS 的全局 `ApiKeyGuard` → axum middleware；`@Public` → 公开路由组。TS 后端保留作行为对照基准。

**Tech Stack:** Rust 2021 · axum 0.7 · tokio · tower / tower-http · rusqlite (bundled) · serde / serde_json · thiserror / anyhow · tracing + tracing-subscriber + tracing-appender · jsonwebtoken 9 · hmac / sha2 / hex · subtle · chrono · dotenvy

**Spec:** `docs/superpowers/specs/2026-07-06-codex-webui-rust-migration-design.md`（§5 Phase 0、§6.7 日志脱敏为权威源）

**权威对照源（TS，迁移时逐条核对）：**
- `src/main.ts`（bootstrap：multipart 注册、globalPrefix `api`、Swagger 暂略）
- `src/app.module.ts`（`PINO_REDACT` 脱敏表、pino-roll 滚动参数）
- `src/database/database.service.ts`（db_path 解析、WAL/foreign_keys/busy_timeout、migrate）
- `src/common/{error-codes,business.exception,all-exceptions.filter}.ts` + `src/common/dto/api-responses.dto.ts`
- `src/auth/{auth.service,auth.controller,api-key.guard}.ts`
- `src/settings/settings.definitions.ts` + `src/settings/settings.service.ts`
- `drizzle/0000..0005.sql`（迁移源，`--> statement-breakpoint` 分隔）

---

## File Structure

```
backend-rs/
├── Cargo.toml                      # 单 crate 二进制，依赖清单
├── src/
│   ├── main.rs                     # 入口：.env → Config → tracing → DB → AppState → router → serve+graceful
│   ├── lib.rs                      # crate root（暴露模块，供集成测试）
│   ├── config.rs                   # Config：env 解析 + db_path 解析
│   ├── state.rs                    # AppState { db, auth } + 构造
│   ├── logging.rs                  # tracing 初始化（滚动 appender + access_token URL 脱敏）
│   ├── error.rs                    # AppError / ErrorCode / IntoResponse（统一错误响应）
│   ├── db/
│   │   ├── mod.rs                  # Db：Mutex<Connection> + pragmas
│   │   └── migrations.rs           # embed drizzle/*.sql，按 breakpoint 拆分执行 + 追踪表
│   ├── settings/
│   │   ├── mod.rs                  # SettingsReader：get_string/number/bool（DB>env>default）
│   │   ├── definitions.rs          # 移植 SETTINGS_DEFINITIONS（12 项）
│   │   └── reconcile.rs            # 启动 reconcile（INSERT OR IGNORE + UPDATE 元数据）
│   ├── auth/
│   │   ├── mod.rs                  # AuthService：derive_secret/sign/verify/validate_api_key/authenticate_token
│   │   ├── middleware.rs           # axum middleware：bearer 提取 + JWT 优先 + API Key fallback + @Public
│   │   └── dto.rs                  # LoginRequest / LoginResponse
│   └── routes/
│       ├── mod.rs                  # router 构建：公开组 + 认证组 + 全局错误层
│       ├── health.rs               # GET / （public，对齐 AppController 根路由）
│       └── auth.rs                 # POST /api/auth/login
└── tests/
    ├── config_test.rs              # （单元测试放 src 内 #[cfg(test)]，集成测试放此）
    ├── migration_test.rs
    ├── settings_test.rs
    ├── error_test.rs
    ├── auth_test.rs
    └── routes_test.rs              # 端到端：health / login / 受保护路由 / query token
```

**职责边界**：每个文件单一职责。`error.rs` 只管错误模型与序列化；`auth/middleware.rs` 只管鉴权流程，密钥逻辑在 `auth/mod.rs`；`db/migrations.rs` 只管迁移执行，连接在 `db/mod.rs`；`settings/reconcile.rs` 与 `settings/mod.rs`(读) 分离，因启动写与运行时读关注点不同。

---

## Chunk 1: 脚手架 + 配置 + 日志 + DB/迁移 + Settings

### Task 1.1: Cargo 脚手架（可编译的空二进制）

**Files:**
- Create: `backend-rs/Cargo.toml`
- Create: `backend-rs/src/main.rs`

- [ ] **Step 1: 写 Cargo.toml**

```toml
[package]
name = "codex-webui"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tower = { version = "0.5", features = ["util"] }
tower-http = { version = "0.6", features = ["trace"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.32", features = ["bundled"] }
thiserror = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-appender = "0.2"
jsonwebtoken = "9"
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
subtle = "2"
chrono = { version = "0.4", features = ["serde"] }
dotenvy = "0.15"
once_cell = "1"

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

> 版本以编写时已知稳定版为准；执行时如 `cargo` 报冲突，升到当前最新兼容版即可（axum/tower 系列注意 hyper 大版本一致）。

- [ ] **Step 2: 写最小 main.rs**

```rust
fn main() {
    println!("codex-webui backend-rs booting (phase 0 stub)");
}
```

- [ ] **Step 3: 验证编译**

Run: `cd backend-rs && cargo build`
Expected: 编译通过，产出 `target/debug/codex-webui`。

- [ ] **Step 4: 提交**

```bash
git add backend-rs/Cargo.toml backend-rs/Cargo.lock backend-rs/src/main.rs
git commit -m "feat(backend-rs): scaffold cargo binary"
```

---

### Task 1.2: Config（env 解析 + db_path 解析）

**Files:**
- Create: `backend-rs/src/config.rs`
- Test: `backend-rs/src/config.rs` (`#[cfg(test)]`)

对照 `database.service.ts:resolveDatabasePath` 与 `.env.example`。db_path 优先级：`WEBUI_DB_PATH` > `CODEX_HOME/codex-webui.sqlite` > `~/.codex/codex-webui.sqlite`。`WEBUI_API_KEY` 必填，缺失则启动失败。

- [ ] **Step 1: 先写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn clear() {
        for k in ["WEBUI_DB_PATH","CODEX_HOME","WEBUI_API_KEY","PORT","LOG_LEVEL","CODEX_BIN"] {
            env::remove_var(k);
        }
    }

    #[test]
    fn db_path_uses_explicit_webui_db_path() {
        clear();
        env::set_var("WEBUI_API_KEY","k");
        env::set_var("CODEX_HOME","/tmp/ignored");
        env::set_var("WEBUI_DB_PATH","/explicit/a.sqlite");
        let c = Config::from_env().unwrap();
        assert_eq!(c.db_path, "/explicit/a.sqlite");
    }

    #[test]
    fn db_path_uses_codex_home_when_no_explicit() {
        clear();
        env::set_var("WEBUI_API_KEY","k");
        env::set_var("CODEX_HOME","/codex-home");
        let c = Config::from_env().unwrap();
        assert_eq!(c.db_path, "/codex-home/codex-webui.sqlite");
    }

    #[test]
    fn db_path_falls_back_to_dotcodex() {
        clear();
        env::set_var("WEBUI_API_KEY","k");
        let c = Config::from_env().unwrap();
        assert!(c.db_path.ends_with("/.codex/codex-webui.sqlite"));
    }

    #[test]
    fn missing_api_key_is_error() {
        clear();
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn port_defaults_to_8172() {
        clear();
        env::set_var("WEBUI_API_KEY","k");
        assert_eq!(Config::from_env().unwrap().port, 8172);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test config`
Expected: 编译失败（`Config` 未定义）。

- [ ] **Step 3: 实现 Config**

```rust
use anyhow::{anyhow, Result};
use std::env;
use std::path::PathBuf;

pub struct Config {
    pub webui_api_key: String,
    pub port: u16,
    pub openai_api_key: Option<String>,
    pub log_level: String,
    pub codex_bin: String,
    pub codex_home: Option<String>,
    pub db_path: String,
}

const DEFAULT_DB_FILENAME: &str = "codex-webui.sqlite";

impl Config {
    pub fn from_env() -> Result<Self> {
        let webui_api_key = env::var("WEBUI_API_KEY")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("WEBUI_API_KEY is required"))?;

        let port = env::var("PORT").ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|p| p.parse::<u16>())
            .transpose()?
            .unwrap_or(8172);

        let codex_home = env::var("CODEX_HOME").ok()
            .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        Ok(Self {
            webui_api_key,
            port,
            openai_api_key: env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            codex_bin: env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string()),
            codex_home: codex_home.clone(),
            db_path: resolve_db_path(env::var("WEBUI_DB_PATH").ok(), codex_home.as_deref()),
        })
    }
}

fn resolve_db_path(explicit: Option<String>, codex_home: Option<&str>) -> String {
    if let Some(p) = explicit.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return p;
    }
    let base = codex_home
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_or_home().join(".codex"));
    base.join(DEFAULT_DB_FILENAME).to_string_lossy().into_owned()
}

fn dirs_or_home() -> PathBuf {
    // 避免引入 dirs crate：用 HOME / USERPROFILE
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
```

> 注：测试里设 `CODEX_HOME=/codex-home` 等绝对路径，`base.join(...)` 直接拼接，符合预期。`dirs_or_home` 仅在 `CODEX_HOME` 缺失时用。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test config`
Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/config.rs
git commit -m "feat(backend-rs): config + db_path resolution (parity with database.service.ts)"
```

---

### Task 1.3: 日志初始化（tracing + 滚动 appender + URL 脱敏）

**Files:**
- Create: `backend-rs/src/logging.rs`

对照 `app.module.ts`：pino-roll（`logs/app`，10m，count 5）。**已知差异**：`tracing-appender` 仅支持**时间维度**滚动（HOURLY/DAILY），无按大小滚动；pino-roll 的「10MB×5」无现成等价。Phase 0 取舍：用 daily 滚动 + `logs/app` 目录，按大小滚动留作后续（外部 logrotate 或自写 appender）。URL 脱敏：`access_token` query 参数从日志 URL 剥离（对照 `app.module.ts:sanitizeUrl`）。

- [ ] **Step 1: 实现 logging.rs**

```rust
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*};

/// 初始化 tracing：stdout + 滚动文件（logs/app，daily）。返回 WorkerGuard 保持文件写入存活。
pub fn init(level: &str) -> WorkerGuard {
    let file_appender = rolling::daily("logs", "app");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_writer(non_blocking))
        .init();

    guard
}

/// 剥离 URL 中的 access_token query 参数（对照 app.module.ts:sanitizeUrl）。
pub fn sanitize_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut first = true;
    for part in url.split('&') {
        if part.starts_with("access_token=") || part.contains("?access_token=") {
            continue;
        }
        if first { first = false; } else { out.push('&'); }
        out.push_str(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn strips_access_token() {
        assert_eq!(sanitize_url("/api/files/serve?access_token=abc&x=1"), "/api/files/serve?x=1");
    }
    #[test] fn keeps_url_without_token() {
        assert_eq!(sanitize_url("/api/health"), "/api/health");
    }
}
```

> `sanitize_url` 是过渡实现；正式 URL 脱敏在 Chunk 2 的 HTTP trace 层调用。`WorkerGuard` 必须在 `main` 里持有到进程结束，否则后台写线程会丢日志。

- [ ] **Step 2: 跑测试**

Run: `cd backend-rs && cargo test logging`
Expected: 2 passed。

- [ ] **Step 3: 提交**

```bash
git add backend-rs/src/logging.rs
git commit -m "feat(backend-rs): tracing init (daily rolling + access_token URL redaction)"
```

---

### Task 1.4: DB 连接 + pragmas

**Files:**
- Create: `backend-rs/src/db/mod.rs`

对照 `database.service.ts`：`WAL` / `foreign_keys=ON` / `busy_timeout=5000`。rusqlite `Connection` 是 `Send` 非 `Sync`，用 `Mutex<Connection>` 包裹（NestJS 用单连接同步执行，行为对齐）。

- [ ] **Step 1: 实现**

```rust
use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

pub mod migrations;
pub use migrations::run_migrations;

pub struct Db {
    pub conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        tracing::info!("SQLite database ready at {}", path);
        Ok(Self { conn: Mutex::new(conn) })
    }
}
```

- [ ] **Step 2: 临时在 main 验证可打开（手动）**

Run: `cd backend-rs && cargo build`（编译通过即可；端到端开库在 Task 2.5 main 接线后验证）

- [ ] **Step 3: 提交**

```bash
git add backend-rs/src/db/mod.rs
git commit -m "feat(backend-rs): sqlite connection with WAL/fk/busy_timeout pragmas"
```

---

### Task 1.5: 迁移执行器（embed drizzle SQL + breakpoint 拆分 + 追踪表）

**Files:**
- Create: `backend-rs/src/db/migrations.rs`
- Test: `backend-rs/tests/migration_test.rs`

迁移源 = `drizzle/0000..0005.sql`（相对 crate 根的 `../drizzle/*.sql`）。用 `include_str!` 嵌入。语句分隔符 `--> statement-breakpoint`。追踪：自有表 `schema_migrations(filename TEXT PRIMARY KEY, applied_at INTEGER)`。**TS-DB 兼容**：若检测到 drizzle 自带 `__drizzle_migrations` 表（TS 管理过的库），视为已全部应用、跳过执行。

- [ ] **Step 1: 写失败测试**

```rust
// tests/migration_test.rs
use codex_webui::db::{Db, run_migrations};
use rusqlite::Connection;

fn fresh() -> Db {
    let c = Connection::open_in_memory().unwrap();
    Db { conn: std::sync::Mutex::new(c) }
}

#[test]
fn creates_all_tables() {
    let db = fresh();
    run_migrations(&db).unwrap();
    let conn = db.conn.lock().unwrap();
    for t in ["token_usage_snapshots","turn_diffs","settings","pending_server_requests","turn_errors"] {
        let n: i64 = conn.query_row(
            &format!("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{}'", t),
            [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "table {} should exist", t);
    }
}

#[test]
fn idempotent_rerun() {
    let db = fresh();
    run_migrations(&db).unwrap();
    run_migrations(&db).unwrap(); // 不应报错
}

#[test]
fn skips_when_drizzle_managed() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE __drizzle_migrations(id integer); CREATE TABLE settings(x);").unwrap();
    let db = Db { conn: std::sync::Mutex::new(c) };
    run_migrations(&db).unwrap(); // 检测到 drizzle 表，跳过
}
```

> 注：测试用 `open_in_memory` + 直接构造 `Db`，因此 `Db` 的字段与 `run_migrations` 签名需对测试可见（crate 暴露 `pub mod db`，`Db`/`run_migrations` 为 `pub`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --test migration_test`
Expected: 编译失败（`run_migrations` 未定义）。

- [ ] **Step 3: 实现**

```rust
// src/db/migrations.rs
use crate::db::Db;
use anyhow::Result;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0000_init.sql", include_str!("../../../drizzle/0000_init.sql")),
    ("0001_fresh_daredevil.sql", include_str!("../../../drizzle/0001_fresh_daredevil.sql")),
    ("0002_certain_starbolt.sql", include_str!("../../../drizzle/0002_certain_starbolt.sql")),
    ("0003_mature_chameleon.sql", include_str!("../../../drizzle/0003_mature_chameleon.sql")),
    ("0004_lethal_rhodey.sql", include_str!("../../../drizzle/0004_lethal_rhodey.sql")),
    ("0005_melted_mister_sinister.sql", include_str!("../../../drizzle/0005_melted_mister_sinister.sql")),
];

const BREAKPOINT: &str = "--> statement-breakpoint";

pub fn run_migrations(db: &Db) -> Result<()> {
    let conn = db.conn.lock().unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations(filename TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);")?;

    let drizzle_managed: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='__drizzle_migrations'",
        [], |r| r.get(0)).unwrap_or(0);
    if drizzle_managed > 0 {
        tracing::info!("__drizzle_migrations present; assuming TS-managed DB, skipping Rust migrations");
        for (name, _) in MIGRATIONS {
            conn.execute("INSERT OR IGNORE INTO schema_migrations(filename, applied_at) VALUES (?1, strftime('%s','now'))", [(*name).to_string()])?;
        }
        return Ok(());
    }

    for (name, sql) in MIGRATIONS {
        let applied: i64 = conn.query_row(
            "SELECT count(*) FROM schema_migrations WHERE filename = ?1",
            [(*name).to_string()], |r| r.get::<_, i64>(0)).unwrap_or(0);
        if applied > 0 { continue; }
        for stmt in sql.split(BREAKPOINT) {
            let stmt = stmt.trim();
            if stmt.is_empty() { continue; }
            conn.execute_batch(stmt)
                .map_err(|e| anyhow::anyhow!("migration {} failed: {}", name, e))?;
        }
        conn.execute("INSERT INTO schema_migrations(filename, applied_at) VALUES (?1, strftime('%s','now'))",
            [(*name).to_string()])?;
        tracing::info!("applied migration {}", name);
    }
    Ok(())
}
```

> `src/lib.rs` 暴露 `pub mod db;` 供集成测试访问。注意此 crate 同时有 `lib.rs` 与 `main.rs`：`main.rs` 用 `mod` 引入各模块，`lib.rs` 暴露 `pub` 供测试——见 Task 2.5 统一接线，或在此步先建空 `lib.rs`（`pub mod config; pub mod db; ...`）。执行时择一：纯 binary crate（测试内联）或 binary+lib 双 target。**推荐双 target**：`lib.rs` 聚合 `pub mod`，`main.rs` 调 `codex_webui::main()` 风格。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test --test migration_test`
Expected: 3 passed。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/db/migrations.rs backend-rs/src/lib.rs backend-rs/tests/migration_test.rs
git commit -m "feat(backend-rs): drizzle SQL migration runner (breakpoint split + drizzle-DB compat)"
```

---

### Task 1.6: Settings 定义移植 + reconcile + reader

**Files:**
- Create: `backend-rs/src/settings/definitions.rs`
- Create: `backend-rs/src/settings/reconcile.rs`
- Create: `backend-rs/src/settings/mod.rs`
- Test: `backend-rs/tests/settings_test.rs`

对照 `settings.definitions.ts`（12 项定义）与 `settings.service.ts`。reconcile：每项 `INSERT OR IGNORE`（保留用户值）+ `UPDATE` 元数据（type/category/description/default_value/constraints/updated_at）。reader：`get_string` / `get_number` / `get_bool`，顺序 **DB value（非空）> envKey > defaultValue**。

- [ ] **Step 1: 写失败测试**

```rust
// tests/settings_test.rs
use codex_webui::db::{Db, run_migrations};
use codex_webui::settings::{SettingsReader, reconcile_settings};
use rusqlite::Connection;
use std::sync::Mutex;

fn db() -> Db {
    let c = Connection::open_in_memory().unwrap();
    Db { conn: Mutex::new(c) }
}

#[test]
fn reconcile_inserts_defaults() {
    let db = db(); run_migrations(&db).unwrap();
    reconcile_settings(&db).unwrap();
    let r = SettingsReader::new(&db);
    assert_eq!(r.get_number("files.uploadMaxBytes"), Some(104_857_600.0));
}

#[test]
fn db_override_wins() {
    let db = db(); run_migrations(&db).unwrap(); reconcile_settings(&db).unwrap();
    {
        let c = db.conn.lock().unwrap();
        c.execute("UPDATE settings SET value='209715200', updated_at=strftime('%s','now') WHERE key='files.uploadMaxBytes'", []).unwrap();
    }
    let r = SettingsReader::new(&db);
    assert_eq!(r.get_number("files.uploadMaxBytes"), Some(209_715_200.0));
}

#[test]
fn env_fallback_when_db_null() {
    std::env::set_var("WORKSPACE_ROOTS", "/ws1,/ws2");
    let db = db(); run_migrations(&db).unwrap(); reconcile_settings(&db).unwrap();
    let r = SettingsReader::new(&db);
    assert_eq!(r.get_string("security.workspaceRoots"), Some("/ws1,/ws2".to_string()));
    std::env::remove_var("WORKSPACE_ROOTS");
}
```

> reconcile 只 `INSERT OR IGNORE` 不写 value，故 DB 中 value 列为 NULL，触发 env/default 链。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --test settings_test`
Expected: 编译失败。

- [ ] **Step 3: 实现定义（移植 12 项）**

```rust
// src/settings/definitions.rs
#[derive(Clone, Copy, PartialEq)]
pub enum SettingType { String, Number, Boolean, Json }
impl SettingType {
    pub fn as_str(&self) -> &'static str {
        match self { Self::String=>"string", Self::Number=>"number", Self::Boolean=>"boolean", Self::Json=>"json" }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Category { Terminal, Files, Security, General }
impl Category {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Terminal=>"terminal", Self::Files=>"files", Self::Security=>"security", Self::General=>"general" }
    }
}

pub struct SettingDef {
    pub key: &'static str,
    pub ty: SettingType,
    pub category: Category,
    pub description: &'static str,
    pub default_value: &'static str,
    pub env_key: Option<&'static str>,
}

pub const SETTINGS_DEFINITIONS: &[SettingDef] = &[
    SettingDef { key:"general.maxIdleSubscriptions", ty:SettingType::Number, category:Category::General,
        description:"Maximum idle thread socket subscriptions retained in the browser before cleanup.",
        default_value:"30", env_key:None },
    SettingDef { key:"general.onlyofficeUrl", ty:SettingType::String, category:Category::General,
        description:"OnlyOffice Document Server base URL. Leave empty to use native viewers and disable PPTX preview.",
        default_value:"", env_key:None },
    SettingDef { key:"general.onlyofficeJwtSecret", ty:SettingType::String, category:Category::General,
        description:"JWT secret for signing OnlyOffice editor config and verifying save callbacks. Must match the Document Server browser/outbox secret for edit mode.",
        default_value:"", env_key:None },
    SettingDef { key:"general.onlyofficeSaveMaxBytes", ty:SettingType::Number, category:Category::General,
        description:"Maximum file size in bytes accepted from OnlyOffice save callback. Increase for large Office documents.",
        default_value:"104857600", env_key:None },
    SettingDef { key:"general.publicBaseUrl", ty:SettingType::String, category:Category::General,
        description:"Public base URL of this WebUI instance (e.g. https://codex.example.com). Used to build document URLs reachable by OnlyOffice. Auto-detected from request headers when empty.",
        default_value:"", env_key:None },
    SettingDef { key:"terminal.maxSessions", ty:SettingType::Number, category:Category::Terminal,
        description:"Maximum concurrent terminal sessions retained by the server.",
        default_value:"10", env_key:Some("WEBUI_TERMINAL_MAX_SESSIONS") },
    SettingDef { key:"terminal.graceMs", ty:SettingType::Number, category:Category::Terminal,
        description:"Milliseconds to keep a detached terminal alive before cleanup.",
        default_value:"45000", env_key:Some("WEBUI_TERMINAL_GRACE_MS") },
    SettingDef { key:"terminal.scrollback", ty:SettingType::Number, category:Category::Terminal,
        description:"Scrollback lines retained by new terminal buffers.",
        default_value:"5000", env_key:Some("WEBUI_TERMINAL_SCROLLBACK") },
    SettingDef { key:"terminal.defaultCwd", ty:SettingType::String, category:Category::Terminal,
        description:"Default working directory for new terminals. Must be an existing directory within workspace roots. Empty to use thread cwd or home.",
        default_value:"", env_key:Some("DEFAULT_TERMINAL_CWD") },
    SettingDef { key:"files.uploadMaxBytes", ty:SettingType::Number, category:Category::Files,
        description:"Maximum file upload size in bytes.",
        default_value:"104857600", env_key:Some("WEBUI_UPLOAD_MAX_BYTES") },
    SettingDef { key:"files.excludedDirs", ty:SettingType::String, category:Category::Files,
        description:"Comma-separated directory/file names excluded from file tree listings.",
        default_value:"node_modules,.git,.next,dist,__pycache__,.DS_Store", env_key:None },
    SettingDef { key:"security.workspaceRoots", ty:SettingType::String, category:Category::Security,
        description:"Comma-separated list of allowed workspace root directories. Home directory is always included.",
        default_value:"", env_key:Some("WORKSPACE_ROOTS") },
];
```

> TS 的 `constraints`（min/max/enum/integer）Phase 0 暂以 JSON 字符串存空 `{}`（DB 列 `constraints` 为 text notNull）。完整约束校验随 settings CRUD（Phase 2）补。

- [ ] **Step 4: 实现 reconcile**

```rust
// src/settings/reconcile.rs
use crate::db::Db;
use crate::settings::definitions::SETTINGS_DEFINITIONS;
use anyhow::Result;

pub fn reconcile_settings(db: &Db) -> Result<()> {
    let conn = db.conn.lock().unwrap();
    for d in SETTINGS_DEFINITIONS {
        conn.execute(
            "INSERT OR IGNORE INTO settings(key, value, type, category, description, default_value, constraints, updated_at)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, '{}', strftime('%s','now'))",
            rusqlite::params![d.key, d.ty.as_str(), d.category.as_str(), d.description, d.default_value])?;
        conn.execute(
            "UPDATE settings SET type=?1, category=?2, description=?3, default_value=?4, updated_at=strftime('%s','now') WHERE key=?5",
            rusqlite::params![d.ty.as_str(), d.category.as_str(), d.description, d.default_value, d.key])?;
    }
    Ok(())
}
```

- [ ] **Step 5: 实现 reader**

```rust
// src/settings/mod.rs
pub mod definitions;
pub mod reconcile;
pub use reconcile::reconcile_settings;

use crate::db::Db;
use definitions::{SettingDef, SETTINGS_DEFINITIONS};

fn find_def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS_DEFINITIONS.iter().find(|d| d.key == key)
}

pub struct SettingsReader<'a> { db: &'a Db }
impl<'a> SettingsReader<'a> {
    pub fn new(db: &'a Db) -> Self { Self { db } }

    fn raw_value(&self, key: &str) -> Option<String> {
        let def = find_def(key)?;
        let conn = self.db.conn.lock().unwrap();
        let db_val: Option<String> = conn.query_row(
            "SELECT value FROM settings WHERE key=?1", [key],
            |r| r.get::<_, Option<String>>(0)).ok().flatten();
        db_val.filter(|s| !s.is_empty())
            .or_else(|| def.env_key.and_then(std::env::var).filter(|s| !s.is_empty()))
            .or_else(|| Some(def.default_value.to_string()))
    }

    pub fn get_string(&self, key: &str) -> Option<String> { self.raw_value(key) }
    pub fn get_number(&self, key: &str) -> Option<f64> {
        let _ = find_def(key)?;
        self.raw_value(key).and_then(|s| s.parse::<f64>().ok())
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        let _ = find_def(key)?;
        self.raw_value(key).and_then(|s| match s.to_ascii_lowercase().as_str() {
            "1"|"true"|"yes"|"on" => Some(true),
            "0"|"false"|"no"|"off"|"" => Some(false),
            _ => None,
        })
    }
    pub fn get_upload_max_bytes(&self) -> u64 {
        self.get_number("files.uploadMaxBytes").map(|n| n as u64).unwrap_or(104_857_600)
    }
}
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cd backend-rs && cargo test --test settings_test`
Expected: 3 passed。

- [ ] **Step 7: 提交**

```bash
git add backend-rs/src/settings backend-rs/tests/settings_test.rs
git commit -m "feat(backend-rs): settings definitions + reconcile + reader (DB>env>default)"
```

---

## Chunk 2: 错误处理 + 认证 + 路由/健康 + main 接线 + 优雅关闭

### Task 2.1: 错误模型（ErrorCode + AppError + IntoResponse）

**Files:**
- Create: `backend-rs/src/error.rs`
- Test: `backend-rs/tests/error_test.rs`

对照 `error-codes.ts` / `business.exception.ts` / `all-exceptions.filter.ts` / `api-responses.dto.ts`。响应体：`{statusCode: number, errorCode: string, message: string|string[], params?: Record<string,string|number>}`。**ErrorCode 字符串逐字保留**（前端 i18n key）。status fallback 表：400→`http.bad_request`、401→`http.unauthorized`、403→`http.forbidden`、404→`http.not_found`、409→`http.conflict`、413→`http.payload_too_large`、500→`http.internal_error`，其余 ≥500→`http.internal_error`、其余→`http.request_failed`（带 `params:{status}`）。

- [ ] **Step 1: 写失败测试**

```rust
// tests/error_test.rs
use codex_webui::error::{AppError, ErrorCode};
use axum::{http::StatusCode, response::IntoResponse};

#[test]
fn business_error_status() {
    let resp = AppError::business(ErrorCode::AuthInvalidApiKey, StatusCode::UNAUTHORIZED, "Invalid API key".into(), None)
        .into_response();
    assert_eq!(resp.status(), 401);
}

#[test]
fn status_fallback_request_failed() {
    let resp = AppError::status(418).into_response();
    assert_eq!(resp.status(), 418);
}

#[test]
fn unknown_is_500() {
    let resp = AppError::internal("boom".into()).into_response();
    assert_eq!(resp.status(), 500);
}

#[test]
fn error_code_strings_preserved() {
    assert_eq!(ErrorCode::AuthInvalidApiKey.as_str(), "auth.invalid_api_key");
    assert_eq!(ErrorCode::HttpBadRequest.as_str(), "http.bad_request");
    assert_eq!(ErrorCode::HttpRequestFailed.as_str(), "http.request_failed");
}
```

> 断言 error code 字符串逐字等于 TS 版（前端 i18n 命门）。`error-codes.ts` 全量移植时逐条核对（此处抽样代表）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --test error_test`
Expected: 编译失败。

- [ ] **Step 3: 实现 ErrorCode + AppError**

```rust
// src/error.rs
use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// 前端 i18n error code —— 字符串必须与 src/common/error-codes.ts 逐字一致。
#[derive(Clone, Copy)]
pub enum ErrorCode {
    HttpBadRequest, HttpUnauthorized, HttpForbidden, HttpNotFound, HttpConflict,
    HttpPayloadTooLarge, HttpRequestFailed, HttpInternalError,
    ValidationFieldRequired, ValidationBodyRequired, ValidationTypeMismatch, ValidationFieldInvalid,
    AuthMissingToken, AuthInvalidToken, AuthMissingHeader, AuthInvalidApiKey,
    // files.* / codex.* / terminal.* 等：随各 Phase 补全
}
impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HttpBadRequest => "http.bad_request",
            Self::HttpUnauthorized => "http.unauthorized",
            Self::HttpForbidden => "http.forbidden",
            Self::HttpNotFound => "http.not_found",
            Self::HttpConflict => "http.conflict",
            Self::HttpPayloadTooLarge => "http.payload_too_large",
            Self::HttpRequestFailed => "http.request_failed",
            Self::HttpInternalError => "http.internal_error",
            Self::ValidationFieldRequired => "validation.field_required",
            Self::ValidationBodyRequired => "validation.body_required",
            Self::ValidationTypeMismatch => "validation.type_mismatch",
            Self::ValidationFieldInvalid => "validation.field_invalid",
            Self::AuthMissingToken => "auth.missing_token",
            Self::AuthInvalidToken => "auth.invalid_token",
            Self::AuthMissingHeader => "auth.missing_header",
            Self::AuthInvalidApiKey => "auth.invalid_api_key",
        }
    }
    pub fn fallback_for(status: u16) -> Self {
        match status {
            400 => Self::HttpBadRequest, 401 => Self::HttpUnauthorized, 403 => Self::HttpForbidden,
            404 => Self::HttpNotFound, 409 => Self::HttpConflict, 413 => Self::HttpPayloadTooLarge,
            500 => Self::HttpInternalError,
            s if s >= 500 => Self::HttpInternalError,
            _ => Self::HttpRequestFailed,
        }
    }
}

pub type Params = BTreeMap<String, serde_json::Number>;

pub enum AppError {
    Business { code: ErrorCode, status: StatusCode, message: Value, params: Option<Params> },
    Status { status: StatusCode },
    Internal(String),
}

impl AppError {
    pub fn business(code: ErrorCode, status: StatusCode, message: String, params: Option<Params>) -> Self {
        Self::Business { code, status, message: Value::String(message), params }
    }
    pub fn status(code: u16) -> Self {
        Self::Status { status: StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR) }
    }
    pub fn internal(msg: String) -> Self { Self::Internal(msg) }
    pub fn unauthorized(code: ErrorCode, msg: &str) -> Self {
        Self::business(code, StatusCode::UNAUTHORIZED, msg.into(), None)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, params) = match self {
            Self::Business { code, status, message, params } => (status, code, message, params),
            Self::Status { status } => {
                let code = ErrorCode::fallback_for(status.as_u16());
                let mut params = None;
                if matches!(code, ErrorCode::HttpRequestFailed) {
                    let mut m = Params::new();
                    m.insert("status".into(), serde_json::Number::from(status.as_u16()));
                    params = Some(m);
                }
                (status, code, Value::String(format!("Request failed ({})", status.as_u16())), params)
            }
            Self::Internal(msg) => {
                tracing::error!(error = %msg, "unhandled exception");
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::HttpInternalError,
                 Value::String("Internal server error".into()), None)
            }
        };
        let mut body = json!({
            "statusCode": status.as_u16(),
            "errorCode": code.as_str(),
            "message": message,
        });
        if let Some(p) = params { body["params"] = json!(p); }
        (status, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self { Self::Internal(e.to_string()) }
}
```

> `error-codes.ts` 全量字符串（files.* / codex.* / terminal.* 等）在各自 Phase 移植模块时补到 enum + `as_str`。Phase 0 覆盖 http/validation/auth 足够支撑地基测试。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test --test error_test`
Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/error.rs backend-rs/tests/error_test.rs
git commit -m "feat(backend-rs): error model (ErrorCode i18n strings + unified response, parity with all-exceptions.filter)"
```

---

### Task 2.2: AuthService（JWT 派生 / sign / verify / validate_api_key / authenticate_token）

**Files:**
- Create: `backend-rs/src/auth/mod.rs`
- Test: `backend-rs/tests/auth_test.rs`

对照 `auth.service.ts`：密钥 = `HMAC-SHA256(key=WEBUI_API_KEY, msg='codex-webui-jwt').hexdigest`；HS256；TTL 86400s；`sub='webui'`。`validateApiKey` 常量时间比较。`authenticateToken`：JWT 优先 → 成功即 jwt；否则 `looksLikeJwt`(3 段) 记 warn；再 API key fallback；否则 invalidToken。

- [ ] **Step 1: 写失败测试**

```rust
// tests/auth_test.rs
use codex_webui::auth::AuthService;
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn expected_secret(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).unwrap();
    mac.update(b"codex-webui-jwt");
    hex::encode(mac.finalize().into_bytes())
}

#[test]
fn secret_derivation_matches_ts() {
    let svc = AuthService::new("my-secret-key");
    assert_eq!(svc.jwt_secret(), expected_secret("my-secret-key"));
}

#[test]
fn sign_verify_roundtrip() {
    let svc = AuthService::new("k");
    let t = svc.sign_jwt().unwrap();
    assert!(svc.verify_jwt(&t.access_token).unwrap());
    assert_eq!(t.expires_in, 86400);
}

#[test]
fn wrong_key_rejects_jwt() {
    let a = AuthService::new("k1");
    let b = AuthService::new("k2");
    let t = a.sign_jwt().unwrap();
    assert!(!b.verify_jwt(&t.access_token).unwrap());
}

#[test]
fn validate_api_key_correct_and_wrong() {
    let svc = AuthService::new("correct-horse");
    assert!(svc.validate_api_key("correct-horse"));
    assert!(!svc.validate_api_key("wrong"));
    assert!(!svc.validate_api_key(""));
}

#[test]
fn authenticate_token_flows() {
    let svc = AuthService::new("k");
    let jwt = svc.sign_jwt().unwrap().access_token;
    assert_eq!(svc.authenticate_token(Some(&jwt), None).auth_type.as_deref(), Some("jwt"));
    assert_eq!(svc.authenticate_token(Some("k"), None).auth_type.as_deref(), Some("apiKey"));
    assert!(!svc.authenticate_token(Some("nope"), None).ok);
    assert!(!svc.authenticate_token(None, None).ok);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --test auth_test`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
// src/auth/mod.rs
use crate::error::AppError;
use hmac::{Hmac, Mac};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const SUBJECT: &str = "webui";
const TTL_SECONDS: i64 = 24 * 60 * 60;
const SECRET_CONTEXT: &[u8] = b"codex-webui-jwt";

#[derive(Serialize, Deserialize)]
struct Claims { sub: String, exp: usize, iat: usize }

pub struct AuthResult { pub ok: bool, pub auth_type: Option<String> }

pub struct AuthService { api_key: String, jwt_secret: String }
impl AuthService {
    pub fn new(api_key: &str) -> Self {
        Self { api_key: api_key.to_string(), jwt_secret: derive_secret(api_key) }
    }
    pub fn jwt_secret(&self) -> String { self.jwt_secret.clone() }

    pub fn validate_api_key(&self, candidate: &str) -> bool {
        if candidate.is_empty() { return false; }
        let a = candidate.as_bytes();
        let b = self.api_key.as_bytes();
        if a.len() != b.len() { return false; }
        a.ct_eq(b).into()
    }

    pub fn sign_jwt(&self) -> Result<LoginResponse, AppError> {
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims { sub: SUBJECT.into(), iat: now, exp: now + TTL_SECONDS as usize };
        let token = encode(&jsonwebtoken::Header::new(Algorithm::HS256), &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()))
            .map_err(|e| AppError::internal(format!("jwt sign: {e}")))?;
        Ok(LoginResponse { access_token: token, expires_in: TTL_SECONDS })
    }

    pub fn verify_jwt(&self, token: &str) -> Result<bool, AppError> {
        let mut v = Validation::new(Algorithm::HS256);
        v.sub = Some(SUBJECT.to_string());
        match decode::<Claims>(token, &DecodingKey::from_secret(self.jwt_secret.as_bytes()), &v) {
            Ok(data) => Ok(data.claims.sub == SUBJECT),
            Err(_) => Ok(false),
        }
    }

    pub fn authenticate_token(&self, token: Option<&str>, _request_id: Option<&str>) -> AuthResult {
        let token = match token { Some(t) if !t.trim().is_empty() => t.trim(), _ => return AuthResult { ok:false, auth_type:None } };
        if self.verify_jwt(token).unwrap_or(false) {
            return AuthResult { ok:true, auth_type:Some("jwt".into()) };
        }
        if looks_like_jwt(token) {
            tracing::warn!(auth_type = "jwt", reason = "verifyFailed", "auth");
        }
        if self.validate_api_key(token) {
            tracing::info!(auth_type = "apiKey", reason = "fallbackAccepted", "auth");
            return AuthResult { ok:true, auth_type:Some("apiKey".into()) };
        }
        AuthResult { ok:false, auth_type:None }
    }
}

fn derive_secret(api_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_key.as_bytes()).expect("hmac key");
    mac.update(SECRET_CONTEXT);
    hex::encode(mac.finalize().into_bytes())
}
fn looks_like_jwt(t: &str) -> bool { t.split('.').count() == 3 }

#[derive(Serialize)]
pub struct LoginResponse { #[serde(rename = "accessToken")] pub access_token: String, #[serde(rename = "expiresIn")] pub expires_in: i64 }
#[derive(Deserialize)]
pub struct LoginRequest { #[serde(rename = "apiKey")] pub api_key: String }
```

> `expiresIn` / `accessToken` / `apiKey` 用 camelCase 序列化对齐前端（对照 `dto/auth.dto.ts`）。`jsonwebtoken 9` 的 `Validation::sub` 校验 sub；`validate_exp` 默认开。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test --test auth_test`
Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/auth/mod.rs backend-rs/tests/auth_test.rs
git commit -m "feat(backend-rs): auth service (jwt HMAC derivation + sign/verify + api-key fallback, parity with auth.service.ts)"
```

---

### Task 2.3: AppState + 认证 middleware

**Files:**
- Create: `backend-rs/src/state.rs`
- Create: `backend-rs/src/auth/middleware.rs`

`AppState { db, auth }`（`Arc` 包裹经 axum State 注入；settings 在需要处用 `SettingsReader::new(&db)` 临时构造，Phase 0 不缓存）。middleware 对照 `api-key.guard.ts`：提取 bearer（header `Authorization: Bearer …`，或 query `access_token` 仅限 GET `/api/files/serve` 与 `/api/files/archive/entry` 且须是 JWT）；JWT 优先 + API key fallback；失败 → 401 `auth.invalid_token`/`auth.missing_header`。

- [ ] **Step 1: 实现 state.rs**

```rust
// src/state.rs
use crate::auth::AuthService;
use crate::db::Db;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub auth: Arc<AuthService>,
}
```

- [ ] **Step 2: 实现 middleware.rs**

```rust
// src/auth/middleware.rs
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};

pub async fn require_auth(State(state): State<AppState>, req: Request<Body>, next: Next) -> Result<Response, AppError> {
    let (token, is_query) = extract_token(&req);
    let token = match token {
        Some(t) => t,
        None => return Err(AppError::unauthorized(ErrorCode::AuthMissingHeader, "Missing or invalid Authorization header")),
    };
    let ok = if is_query {
        state.auth.verify_jwt(&token).unwrap_or(false) // query token 必须 JWT，跳过 API key fallback
    } else {
        state.auth.authenticate_token(Some(&token), None).ok
    };
    if !ok {
        return Err(AppError::unauthorized(ErrorCode::AuthInvalidToken, "Invalid authentication token"));
    }
    Ok(next.run(req).await)
}

/// 返回 (token, is_query_source)
fn extract_token(req: &Request<Body>) -> (Option<String>, bool) {
    if let Some(h) = req.headers().get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(rest) = h.strip_prefix("Bearer ") {
            let t = rest.trim();
            if !t.is_empty() { return (Some(t.to_string()), false); }
        }
    }
    if req.method() == axum::http::Method::GET && allows_query_token(req.uri().path()) {
        if let Some(q) = req.uri().query() {
            for pair in q.split('&') {
                if let Some(v) = pair.strip_prefix("access_token=") {
                    let v = v.trim();
                    if !v.is_empty() && v.split('.').count() == 3 {
                        return (Some(v.to_string()), true);
                    }
                }
            }
        }
    }
    (None, false)
}

fn allows_query_token(path: &str) -> bool {
    path == "/api/files/serve" || path.starts_with("/api/files/serve")
        || path == "/api/files/archive/entry" || path.starts_with("/api/files/archive/entry")
}
```

> `allows_query_token` 仅放行 inline 预览端点（Phase 3 实现）；Phase 0 先具备放行逻辑即可。

- [ ] **Step 3: 临时编译验证**

Run: `cd backend-rs && cargo build`
Expected: 编译通过（middleware 在 Task 2.4 接入 router）。

- [ ] **Step 4: 提交**

```bash
git add backend-rs/src/state.rs backend-rs/src/auth/middleware.rs
git commit -m "feat(backend-rs): app state + auth middleware (jwt-first + api-key fallback + query-token gating)"
```

---

### Task 2.4: 路由（health + login）+ router 构建

**Files:**
- Create: `backend-rs/src/routes/health.rs`
- Create: `backend-rs/src/routes/auth.rs`
- Create: `backend-rs/src/routes/mod.rs`
- Test: `backend-rs/tests/routes_test.rs`

`GET /`（public，对齐 `AppController` 根路由 `{ok:true}`）+ `POST /api/auth/login`（public）。认证组挂 `require_auth`。`_ping` 是 Phase 0 临时受保护探针（认证组下 `GET /api/_ping → {ok:true}`），Phase 1 起由真实端点替换。

- [ ] **Step 1: 写失败测试（router 级 oneshot）**

```rust
// tests/routes_test.rs
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use codex_webui::routes::build_router;
use codex_webui::state::AppState;
use codex_webui::auth::AuthService;
use codex_webui::db::Db;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

fn state() -> AppState {
    let c = Connection::open_in_memory().unwrap();
    AppState { db: Arc::new(Db { conn: Mutex::new(c) }), auth: Arc::new(AuthService::new("topsecret")) }
}

#[tokio::test]
async fn root_health_is_public() {
    let app = build_router(state());
    let resp = app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn protected_route_requires_auth() {
    let app = build_router(state());
    let resp = app.oneshot(Request::builder().uri("/api/_ping").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_route_accepts_api_key() {
    let app = build_router(state());
    let resp = app.oneshot(Request::builder().uri("/api/_ping")
        .header("authorization", "Bearer topsecret").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_returns_jwt() {
    let app = build_router(state());
    let resp = app.oneshot(Request::builder().method("POST").uri("/api/auth/login")
        .header("content-type","application/json").body(Body::from(r#"{"apiKey":"topsecret"}"#)).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_wrong_key_is_401() {
    let app = build_router(state());
    let resp = app.oneshot(Request::builder().method("POST").uri("/api/auth/login")
        .header("content-type","application/json").body(Body::from(r#"{"apiKey":"nope"}"#)).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --test routes_test`
Expected: 编译失败（`build_router` 未定义）。

- [ ] **Step 3: 实现 routes**

```rust
// src/routes/health.rs
use axum::Json;
use serde_json::{json, Value};
pub async fn root() -> Json<Value> { Json(json!({ "ok": true })) }
pub async fn ping() -> Json<Value> { Json(json!({ "ok": true })) }
```

```rust
// src/routes/auth.rs
use axum::{extract::State, Json};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use crate::auth::{LoginRequest, LoginResponse};

pub async fn login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Result<Json<LoginResponse>, AppError> {
    if !state.auth.validate_api_key(&req.api_key) {
        tracing::warn!(auth_type = "apiKeyLogin", reason = "invalidApiKey", "auth");
        return Err(AppError::business(ErrorCode::AuthInvalidApiKey, axum::http::StatusCode::UNAUTHORIZED, "Invalid API key".into(), None));
    }
    tracing::info!(auth_type = "apiKeyLogin", reason = "loginSuccess", "auth");
    Ok(Json(state.auth.sign_jwt()?))
}
```

```rust
// src/routes/mod.rs
pub mod health;
pub mod auth;

use crate::auth::middleware::require_auth;
use crate::state::AppState;
use axum::{routing::{get, post}, Router};

pub fn build_router(state: AppState) -> Router {
    // 受保护组：/api/* 之下除 login 外全部鉴权
    let api_protected = Router::new()
        .route("/_ping", get(crate::routes::health::ping))
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/", get(crate::routes::health::root))                // public 根
        .route("/api/auth/login", post(crate::routes::auth::login))  // public
        .nest("/api", api_protected)                                 // 其余 /api/* 受保护
        .with_state(state)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test --test routes_test`
Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/routes backend-rs/tests/routes_test.rs
git commit -m "feat(backend-rs): router (public root+login, authed /api group, _ping probe)"
```

---

### Task 2.5: lib.rs 聚合 + main 接线 + 优雅关闭

**Files:**
- Create: `backend-rs/src/lib.rs`
- Modify: `backend-rs/src/main.rs`

对照 spec §6.7（优雅关闭是**增量增强**：TS 版未 enableShutdownHooks）。Rust 版：`with_graceful_shutdown` 监听 SIGTERM/SIGINT → drain → （Phase 0 无 codex/PTY，仅关 DB）。启动顺序：`.env` → Config → tracing → Db.open → run_migrations → reconcile_settings → AuthService → AppState → build_router → serve。

**crate 结构决策**：双 target——`lib.rs` 聚合 `pub mod {config, db, settings, error, auth, routes, state, logging}`，供集成测试 `use codex_webui::*`；`main.rs` 仅做启动编排。

- [ ] **Step 1: 写 lib.rs**

```rust
// src/lib.rs
pub mod config;
pub mod db;
pub mod settings;
pub mod error;
pub mod auth;
pub mod routes;
pub mod state;
pub mod logging;
```

> `auth/mod.rs` 内新增 `pub mod middleware;`。`db/mod.rs` 已有 `pub mod migrations;`。

- [ ] **Step 2: 实现 main.rs**

```rust
// src/main.rs
use std::sync::Arc;
use tokio::signal;
use codex_webui::{auth::AuthService, config::Config, db::Db, routes::build_router, settings::reconcile_settings, state::AppState, logging};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let cfg = Config::from_env()?;
    let _log_guard = logging::init(&cfg.log_level);
    tracing::info!(port = cfg.port, db = %cfg.db_path, "starting codex-webui (backend-rs)");

    let db = Arc::new(Db::open(&cfg.db_path)?);
    db::run_migrations(&db)?;
    reconcile_settings(&db)?;

    let state = AppState {
        db: db.clone(),
        auth: Arc::new(AuthService::new(&cfg.webui_api_key)),
    };
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    tracing::info!("listening on 0.0.0.0:{}", cfg.port);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("drain complete, closing db");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl = async { signal::ctrl_c().await.expect("ctrl_c"); };
    #[cfg(unix)]
    let term = async { signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("term").recv().await; };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl => {}, _ = term => {} }
}
```

- [ ] **Step 3: 手动端到端冒烟（开发机）**

```bash
cd backend-rs
WEBUI_API_KEY=test123 cargo run &
# 等监听后：
curl -s http://127.0.0.1:8172/                                       # → {"ok":true}
curl -s -X POST http://127.0.0.1:8172/api/auth/login \
  -H 'content-type: application/json' -d '{"apiKey":"test123"}'      # → {"accessToken":"...","expiresIn":86400}
TOKEN=...   # 取上一步 accessToken
curl -s http://127.0.0.1:8172/api/_ping                               # → 401
curl -s http://127.0.0.1:8172/api/_ping -H "authorization: Bearer $TOKEN"  # → {"ok":true}
```
Expected: 全部符合。DB 文件出现在解析后的 db_path（默认 `~/.codex/codex-webui.sqlite`），含 5 张业务表 + `schema_migrations`。

- [ ] **Step 4: 跑全量测试**

Run: `cd backend-rs && cargo test`
Expected: 全部 passed（含 Chunk 1+2 所有测试）。

- [ ] **Step 5: 提交**

```bash
git add backend-rs/src/lib.rs backend-rs/src/main.rs
git commit -m "feat(backend-rs): wire main (config→tracing→db→migrate→settings→auth→router) + graceful shutdown"
```

---

## Phase 0 完成标准（DoD）

- [ ] `cargo build` 与 `cargo test` 全绿。
- [ ] 手动冒烟 4 条 curl 全部通过（root / login / 受保护 401 / bearer 200）。
- [ ] 迁移在全新 DB 创建 5 张业务表；TS 管理过的库（`__drizzle_migrations`）被识别跳过。
- [ ] settings reconcile 写入 12 项；reader 的 DB>env>default 顺序正确。
- [ ] JWT 密钥派生与 TS `auth.service.ts` 逐字节一致（测试断言）。
- [ ] 错误响应体形状 `{statusCode, errorCode, message, params?}` 与 TS 一致；errorCode 字符串逐字保留。
- [ ] 优雅关闭：Ctrl+C / SIGTERM 能 drain 并退出（Phase 0 仅关 DB）。

**对照验证清单**：保留 TS 后端，对同一前端（或 curl 回放）比对 `/`、`/api/auth/login` 响应体字段名/值是否一致。

---

## 后续阶段（各自独立 plan，此处仅占位）

- Phase 1 — codex 核心（JSON-RPC client + 进程管理）
- Phase 2 — 简单 CRUD 批量
- Phase 3 — 实时 gateway（threads / files）
- Phase 4 — chat / archive / onlyoffice
- Phase 5 — 终端（简化重连）
- Phase 6 — 静态服务 / OpenAPI / parity 校验 / 切换

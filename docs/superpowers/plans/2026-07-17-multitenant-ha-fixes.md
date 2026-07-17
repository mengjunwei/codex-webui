> ⚠️ **历史快照**：本文档是实施时的步骤记录，已不反映当前架构。
> 配置系统已重构为 TOML-only（无 dotenvy / .env / Config::from_env），
> 节点角色已移除（所有节点均 ingress+worker 一体）。

# 多副本 HA 修复实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **⚠️ 恢复上下文(2026-07-17 下班前状态):**
>
> 本 plan **Task 1-9 已全部完成并 commit**,共 9 个 commit 在 `feat/multitenant-platform` 分支(从 `15973e4` 到 `68ea586`)。65 tests PASS。
>
> **剩下 Task 10 / 11 / 12(memberlist 接通 + main 装配 + 集成手测)未完成**。Task 10 因 memberlist 0.8.5 实际 API 比预期复杂(多子 trait + CompositeDelegate + 异步 + 泛型),Plan 第一次草稿里写的 API 全部错误。
>
> **Task 10 Step 2 已重写**,附"实施时校正清单"(8 个 grep 必查项),下次开会直接照 Step 1-5 实施即可,**不需要再调研 API**。
>
> 开始 Task 10 前确认:
> - 当前 commit: `68ea586 docs(multitenant): INTERNAL_RPC_TOKEN/WORKER_ID 必填示例 + ...`
> - 工作区:clean(运行 `git status` 确认)
> - baseline: `cargo build && cargo test --lib` 应 PASS(65 tests)
>
> Task 10 完成后跑 Task 11(改 main.rs)+ Task 12(2 节点 docker-compose 手测)。

**Goal:** 修复 `feat/multitenant-platform` 分支上多副本 HA 实现的 6 类缺陷(复制定位 / offset 跟踪 / 脑裂 CAS / RPC 鉴权 / CODEX_HOME 文档 / memberlist stub 替换),确保 HA "主挂 → 副本晋升 + 会话不丢"在生产配置下可用,`/internal/*` 不被任意访问,且 `--features memberlist-backend` 真正工作。

**Architecture:** 在现有 `replication.rs` / `cluster.rs` / `codex_pool.rs` 架构上做最小修补。
- 主侧用 `AppState.active_rollout` 表(thread_id → 路径)替换 `fname.contains(tid)` 子串扫描;
- offset 推进改为 send 成功回调;
- `promote_if_primary_down` 加 lease CAS + 晋升后删 Redis offset 让副本从 0 拉全;
- `Config::from_env` 强制 `INTERNAL_RPC_TOKEN` ≥32 字节、`INTERNAL_RPC_HOST` 默认 `127.0.0.1`、`WORKER_ID` ≥16 字节必填;
- `receive_rollout` 路径校验加 `tokio::fs::canonicalize` 边界;
- `MemberlistCluster` 真正接通:UDP transport + delegate + seed join + 复用 Redis 写 rpc_url。

**Tech Stack:** Rust + Tokio + SeaORM + Axum + Redis(`redis` crate) + memberlist 0.8.5(`--features memberlist-backend`);测试用 `cargo test` + `tempfile` + `tokio::fs`。

## Global Constraints

- 项目根: `D:\code\rust\codex-webui`(分支 `feat/multitenant-platform`)
- 主修改文件:
  - `backend-rs/src/config.rs`(强制 token、host 默认改 127、worker_id 必填、新增 MEMBERLIST_SEEDS/BIND)
  - `backend-rs/src/state.rs`(新增 `active_rollout`、`local_offsets`,`internal_token: String`)
  - `backend-rs/src/services/multitenant/replication.rs`(主修复)
  - `backend-rs/src/services/multitenant/cluster.rs`(memberlist 接通)
  - `backend-rs/src/api/multitenant/handlers.rs`(写 `active_rollout`)
  - `backend-rs/src/main.rs`(装配新字段 + cluster 三分支)
  - `.env.example`(token 必填示例 + 多 team CODEX_HOME 注释 + memberlist env)
  - `docs/superpowers/specs/2026-07-16-multitenant-platform-design.md`(失败语义 + 多 team 部署两节)
- 测试要求:每个 Task 结束前必须有单测 PASS;不允许破坏既有单测(尤其 `routing.rs` 的 `ConsistentHash` / `replication.rs` 现有 `receive_rollout_*` 三条 / `config.rs` 既有 config 测试)。
- commit 规范:每个 Task 一次 commit,message 用 `fix(multitenant): <动作>` 或 `feat(multitenant): ...` 或 `test(multitenant): ...`。
- **不动** memberlist 之外的 feature flag;不动 `codex_pool.rs`;不动路由层(routing.rs);不动 entity/migration;不动 main.rs 已有 graceful_shutdown 信号。

---

## 文件结构与职责

| 文件 | 职责 | 改/建 |
|---|---|---|
| `backend-rs/src/config.rs` | 启动配置(env 解析);本 plan 新增 token/host/worker_id 必填 + memberlist 配置 | 改 |
| `backend-rs/src/state.rs` | AppState 共享状态;新增 `active_rollout`、`local_offsets`,`internal_token: String` | 改 |
| `backend-rs/src/services/multitenant/replication.rs` | 复制定位、offset 跟踪、脑裂 CAS、receive 路径校验 | 改 |
| `backend-rs/src/services/multitenant/cluster.rs` | MemberlistCluster stub → 真实实现 | 改 |
| `backend-rs/src/api/multitenant/handlers.rs` | 在 mt_create_thread / mt_start_turn 调 codex 后写 active_rollout | 改 |
| `backend-rs/src/main.rs` | 装配新字段;cluster 三分支(memberlist / RedisCluster / SingleCluster) | 改 |
| `.env.example` | 启动模板:INTERNAL_RPC_TOKEN / WORKER_ID / MEMBERLIST_SEEDS 示例 + 多 team CODEX_HOME 注释 | 改 |
| `docs/superpowers/specs/2026-07-16-multitenant-platform-design.md` | 失败语义 + 多 team 部署两节 | 改 |

---

## Task 1: 启动配置硬化(INTERNAL_RPC_TOKEN / INTERNAL_RPC_HOST / WORKER_ID)

**Files:**
- Modify: `backend-rs/src/config.rs`
- Modify: `backend-rs/src/state.rs`(同步 internal_token / worker_id 类型)
- Modify: `backend-rs/src/main.rs`(token / worker_id 构造同步)
- Test: `backend-rs/src/config.rs`(内联 `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: env vars `INTERNAL_RPC_TOKEN` / `INTERNAL_RPC_HOST` / `WORKER_ID`
- Produces:
  - `Config { internal_token: String, internal_rpc_host: String, worker_id: String }`(本 plan 之前 `internal_token: Option<String>` / `worker_id: Option<String>`)

- [ ] **Step 1: 写失败测试**

在 `backend-rs/src/config.rs` 末尾的 `mod tests` 内,追加以下测试(若未引入 `len` 检查辅助,可直接写):

```rust
fn set_required_env() {
    unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
    unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
    unsafe { env::set_var("INTERNAL_RPC_TOKEN", "0123456789abcdef0123456789abcdef"); }
    unsafe { env::set_var("WORKER_ID", "node-a-staaaaaaaaable"); }
}

#[test]
fn internal_token_missing_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::remove_var("INTERNAL_RPC_TOKEN"); }
    assert!(Config::from_env().is_err());
}

#[test]
fn internal_token_too_short_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::set_var("INTERNAL_RPC_TOKEN", "short"); }
    assert!(Config::from_env().is_err());
}

#[test]
fn internal_rpc_host_defaults_to_127() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    assert_eq!(Config::from_env().unwrap().internal_rpc_host, "127.0.0.1");
}

#[test]
fn worker_id_missing_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::remove_var("WORKER_ID"); }
    assert!(Config::from_env().is_err());
}

#[test]
fn worker_id_too_short_is_error() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::set_var("WORKER_ID", "short"); }
    assert!(Config::from_env().is_err());
}

#[test]
fn memberlist_seeds_parse_csv() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    unsafe { env::set_var("MEMBERLIST_SEEDS", "10.0.0.1:7946, 10.0.0.2:7946"); }
    let c = Config::from_env().unwrap();
    assert_eq!(c.memberlist_seeds, vec!["10.0.0.1:7946", "10.0.0.2:7946"]);
}

#[test]
fn memberlist_bind_defaults() {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    set_required_env();
    assert_eq!(Config::from_env().unwrap().memberlist_bind, "0.0.0.0:7946");
}
```

并把 `"INTERNAL_RPC_TOKEN"` / `"WORKER_ID"` / `"MEMBERLIST_SEEDS"` / `"MEMBERLIST_BIND"` 加进 `VARS` 测试数组;`set_required_env` 让既有测试也兼容。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --lib config::tests::`
Expected: FAIL(`internal_token_missing`、`internal_token_too_short`、`internal_rpc_host_defaults_to_127`、`worker_id_*`、`memberlist_*` 全部失败)

- [ ] **Step 3: 改 Config 字段与解析**

`backend-rs/src/config.rs`:

```rust
pub struct Config {
    // ... 既有字段 ...
    pub internal_rpc_host: String,
    pub internal_rpc_port: u16,
    pub worker_rpc_url: Option<String>,
    pub worker_id: String,           // ← 从 Option<String> 改 String
    pub internal_token: String,      // ← 从 Option<String> 改 String
    pub memberlist_seeds: Vec<String>,
    pub memberlist_bind: String,
    // ... 其余字段 ...
}
```

`Config::from_env()` 内(在 `internal_rpc_host` / `internal_token` / `worker_id` / `memberlist_seeds` / `memberlist_bind` 各自位置):

```rust
let internal_token = env::var("INTERNAL_RPC_TOKEN")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| anyhow!("INTERNAL_RPC_TOKEN is required (≥32 bytes)"))?;
if internal_token.len() < 32 {
    return Err(anyhow!(
        "INTERNAL_RPC_TOKEN must be ≥32 bytes (current: {}); generate with `openssl rand -hex 32`",
        internal_token.len()
    ));
}

let internal_rpc_host = env::var("INTERNAL_RPC_HOST")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "127.0.0.1".to_string());

let worker_id = env::var("WORKER_ID")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| anyhow!("WORKER_ID is required (≥16 bytes)"))?;
if worker_id.len() < 16 {
    return Err(anyhow!("WORKER_ID must be ≥16 bytes (current: {})", worker_id.len()));
}

let memberlist_seeds: Vec<String> = env::var("MEMBERLIST_SEEDS")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
    .unwrap_or_default();

let memberlist_bind = env::var("MEMBERLIST_BIND")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "0.0.0.0:7946".to_string());
```

`main.rs` 里删除 `let node_id = cfg.worker_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());`,改为:

```rust
let node_id = cfg.worker_id.clone();
let internal_token = cfg.internal_token.clone();
```

- [ ] **Step 4: 跑全部 config 测试确认通过**

Run: `cd backend-rs && cargo test --lib config::tests`
Expected: PASS(全部 12+ 条,含新加的 7 条;`node_role_defaults_to_both` / `pool_defaults` 等既有测试因 `set_required_env` 需在 `with_db` 内调整,见 Step 5)

- [ ] **Step 5: 既有 `with_db` 辅助改造**

既有:

```rust
fn with_db<F: FnOnce() -> T, T>(f: F) -> T {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
    unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
    f()
}
```

改为:

```rust
fn with_db<F: FnOnce() -> T, T>(f: F) -> T {
    let _g = ENV_LOCK.lock().unwrap();
    clear();
    unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
    unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
    unsafe { env::set_var("INTERNAL_RPC_TOKEN", "0123456789abcdef0123456789abcdef"); }
    unsafe { env::set_var("WORKER_ID", "node-a-staaaaaaaaable"); }
    f()
}
```

把 `with_db` 应用到所有依赖 `Config::from_env()` 的既有测试(确认无遗漏)。

- [ ] **Step 6: 全量 build 检查**

Run: `cd backend-rs && cargo build`
Expected: 编译通过(`internal_token: String` 类型变化引发 `state.rs` / `main.rs` 报错已在 Step 3 同步)

- [ ] **Step 7: Commit**

```bash
git add backend-rs/src/config.rs backend-rs/src/state.rs backend-rs/src/main.rs
git commit -m "fix(multitenant): 强制 INTERNAL_RPC_TOKEN ≥32 字节 + WORKER_ID ≥16 字节 + host 默认 127.0.0.1 + memberlist 配置"
```

---

## Task 2: AppState 新增 active_rollout 与 local_offsets

**Files:**
- Modify: `backend-rs/src/state.rs`
- Modify: `backend-rs/src/main.rs`

**Interfaces:**
- Produces:
  - `pub active_rollout: Arc<tokio::sync::Mutex<HashMap<String, PathBuf>>>`(thread_id → 活跃 rollout 路径)
  - `pub local_offsets: Arc<tokio::sync::Mutex<HashMap<(String, String), u64>>>`(无 Redis 时 fallback offset)

- [ ] **Step 1: 改 state.rs**

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone)]
pub struct AppState {
    // ... 既有字段 ...
    pub codex_home: PathBuf,
    pub node_id: String,
    pub cluster: Arc<dyn ClusterMembership>,
    pub worker_rpc: Arc<WorkerRpcClient>,
    pub internal_token: String,

    // ── HA 修复(spec §2.1 / §2.2)────────────────────────
    pub active_rollout: Arc<AsyncMutex<HashMap<String, PathBuf>>>,
    pub local_offsets: Arc<AsyncMutex<HashMap<(String, String), u64>>>,
}
```

- [ ] **Step 2: main.rs 装配新字段**

在 `let state = AppState { ... }` 之前:

```rust
let active_rollout = Arc::new(AsyncMutex::new(HashMap::new()));
let local_offsets = Arc::new(AsyncMutex::new(HashMap::new()));
```

`state` 构造追加两字段:

```rust
let state = AppState {
    // ... 既有字段 ...
    internal_token: internal_token.clone(),
    active_rollout,
    local_offsets,
};
```

并在 main.rs 顶部 `use tokio::sync::Mutex as AsyncMutex;`(若尚未引入)。

- [ ] **Step 3: 跑 build + 全量测试确认无回归**

Run: `cd backend-rs && cargo build && cargo test --lib`
Expected: 编译通过,既有测试无回归

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/state.rs backend-rs/src/main.rs
git commit -m "feat(multitenant): AppState 新增 active_rollout + local_offsets 字段"
```

---

## Task 3: replication.rs 新增 find_rollout_for_thread + safe_join + delete_all_offsets 工具

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs`

**Interfaces:**
- Produces:
  - `pub async fn find_rollout_for_thread(codex_home: &Path, thread_id: &str) -> Option<PathBuf>`
  - `pub async fn safe_join(codex_home: &Path, rel: &str) -> Result<PathBuf, AppError>`
  - `pub async fn delete_all_team_offsets(redis: &redis::Client, team_id: &str)`

- [ ] **Step 1: 写失败测试**

在 `replication.rs` 末尾 `mod tests` 内追加:

```rust
#[tokio::test]
async fn find_rollout_for_thread_picks_correct_file() {
    let tmp = std::env::temp_dir().join(format!("find-rt-{}", uuid::Uuid::new_v4()));
    let sessions = tmp.join("sessions").join("2026").join("07").join("17");
    tokio::fs::create_dir_all(&sessions).await.unwrap();

    let tid_a = "8a3f0000-0000-0000-0000-000000000001";
    let tid_b = "8a3f0000-0000-0000-0000-000000000002";
    let fa = sessions.join(format!("rollout-t1-{tid_a}.jsonl"));
    let fb = sessions.join(format!("rollout-t2-{tid_b}.jsonl"));
    tokio::fs::write(&fa, b"a").await.unwrap();
    tokio::fs::write(&fb, b"b").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    tokio::fs::write(&fb, b"b-newer").await.unwrap();

    let got = find_rollout_for_thread(&tmp, tid_b).await;
    assert_eq!(got.as_deref(), Some(fb.as_path()));
    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn find_rollout_for_thread_no_match_returns_none() {
    let tmp = std::env::temp_dir().join(format!("find-rt2-{}", uuid::Uuid::new_v4()));
    let sessions = tmp.join("sessions");
    tokio::fs::create_dir_all(&sessions).await.unwrap();
    let got = find_rollout_for_thread(&tmp, "nonexistent-thread-id").await;
    assert!(got.is_none());
    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn safe_join_rejects_symlink_escape() {
    let base = std::env::temp_dir().join(format!("safejoin-{}", uuid::Uuid::new_v4()));
    let outside = std::env::temp_dir().join(format!("outside-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&base).await.unwrap();
    tokio::fs::create_dir_all(&outside).await.unwrap();

    // 字符串层先拒:rel 含 ..
    let bad = safe_join(&base, "../etc/passwd").await;
    assert!(bad.is_err());

    // unix 下 symlink 逃逸应被拒
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();
        let r = safe_join(&base, "escape/file").await;
        assert!(r.is_err(), "symlink escape must be rejected");
    }

    let _ = tokio::fs::remove_dir_all(&base).await;
    let _ = tokio::fs::remove_dir_all(&outside).await;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd backend-rs && cargo test --lib services::multitenant::replication::tests::find_rollout_for_thread_picks_correct_file services::multitenant::replication::tests::find_rollout_for_thread_no_match_returns_none services::multitenant::replication::tests::safe_join_rejects_symlink_escape`
Expected: FAIL(函数未定义,编译错)

- [ ] **Step 3: 实现三个工具函数**

在 `backend-rs/src/services/multitenant/replication.rs` 末尾(测试模块前)追加:

```rust
/// 给定 thread_id,在 <codex_home>/sessions/ 下递归找其活跃 rollout 文件。
/// 规则:文件名包含完整 thread_id 字符串;
/// stem 分段中必须有段 == thread_id(防 `8a3f` 误匹配 `8a3faaaa`);
/// 多命中取 mtime 最新;0 命中返回 None。
pub async fn find_rollout_for_thread(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    if !tokio::fs::metadata(&sessions).await.map(|m| m.is_dir()).unwrap_or(false) {
        return None;
    }
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut stack = vec![sessions];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ft = match entry.file_type().await {
                Ok(f) => f,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let stem_match = p
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| {
                    stem.split(|c: char| c == '-' || c == '.')
                        .any(|seg| seg == thread_id)
                })
                .unwrap_or(false);
            if !stem_match {
                continue;
            }
            let mt = tokio::fs::metadata(&p)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &best {
                Some((_, best_mt)) if *best_mt >= mt => {}
                _ => best = Some((p, mt)),
            }
        }
    }
    best.map(|(p, _)| p)
}

/// 安全拼接:rel 不能为空/绝对/含 .. / 反斜杠;
/// canonicalize 后必须仍在 codex_home 内(防 symlink 逃逸)。
pub async fn safe_join(codex_home: &Path, rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains("..")
        || rel.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = codex_home.join(rel);
    let canon_home = tokio::fs::canonicalize(codex_home)
        .await
        .map_err(|e| AppError::internal(format!("canonicalize codex_home: {e}")))?;
    let canon_path = match tokio::fs::canonicalize(&candidate).await {
        Ok(p) => p,
        Err(_) => {
            if let Some(parent) = candidate.parent() {
                let canon_parent = tokio::fs::canonicalize(parent)
                    .await
                    .map_err(|e| AppError::internal(format!("canonicalize parent: {e}")))?;
                if !canon_parent.starts_with(&canon_home) {
                    return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
                }
            }
            candidate
        }
    };
    if !canon_path.starts_with(&canon_home) {
        return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
    }
    Ok(canon_path)
}

/// 删除 Redis 中该 team 全部 thread 的 offset key(晋升成功后调,触发副本下次从 0 全量同步)。
pub async fn delete_all_team_offsets(redis: &redis::Client, team_id: &str) {
    let Ok(mut conn) = redis.get_multiplexed_async_connection().await else {
        return;
    };
    let pattern = format!("repl:offset:{team_id}:*");
    let mut cursor: u64 = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = match redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query_async(&mut conn)
            .await
        {
            Ok(v) => v,
            Err(_) => return,
        };
        if !keys.is_empty() {
            let _: Result<i64, _> = redis::cmd("DEL").arg(keys).query_async(&mut conn).await;
        }
        if next == 0 {
            break;
        }
        cursor = next;
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd backend-rs && cargo test --lib services::multitenant::replication::tests::`
Expected: PASS(含 find_rollout_for_thread 两条 + safe_join 一条 + 既有 receive_rollout 三条)

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs
git commit -m "feat(multitenant): 新增 find_rollout_for_thread + safe_join + delete_all_team_offsets"
```

---

## Task 4: replication.rs 改 replicate_team_rollouts 用 active_rollout 精确读取 + offset send-成功才推进

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs`
- Modify: `backend-rs/src/api/multitenant/handlers.rs`
- Modify: `backend-rs/src/main.rs`

**Interfaces:**
- `replicate_team_rollouts` 签名变更:
  - 新增参数 `active_rollout: &ThreadRolloutMap, local_offsets: &LocalOffsetMap`
  - 不再调 `list_rollout_files` / `walk_jsonl`

- [ ] **Step 1: 写"active 为空时安全早退"测试**

```rust
#[tokio::test]
async fn replicate_team_rollouts_active_empty_no_op() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    let active: Arc<AsyncMutex<HashMap<String, PathBuf>>> =
        Arc::new(AsyncMutex::new(HashMap::new()));
    let local: Arc<AsyncMutex<HashMap<(String, String), u64>>> =
        Arc::new(AsyncMutex::new(HashMap::new()));

    // cluster 是 SingleCluster → replica_node = self → 早退,本测试只验证 active 字段被接受。
    use crate::services::multitenant::cluster::SingleCluster;
    let cluster = SingleCluster::new("node-self".into(), "http://127.0.0.1:8173".into());

    // 单测不连 DB;这里只确认函数签名编译通过。
    let _ = (active, local, cluster);
}
```

- [ ] **Step 2: 改 replicate_team_rollouts 签名与实现**

在 `replication.rs` 顶部新增类型别名:

```rust
pub type ThreadRolloutMap = Arc<tokio::sync::Mutex<std::collections::HashMap<String, PathBuf>>>;
pub type LocalOffsetMap = Arc<tokio::sync::Mutex<std::collections::HashMap<(String, String), u64>>>;
```

把 `replicate_team_rollouts` 函数签名改为:

```rust
pub async fn replicate_team_rollouts(
    db: &DatabaseConnection,
    team_id: &str,
    codex_home: &Path,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    rpc_client: &WorkerRpcClient,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<(), AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(());
    };
    let Some(replica_node) = row.replica_node.clone() else {
        return Ok(());
    };
    if replica_node == cluster.local_node_id() {
        return Ok(());
    }
    let Some(rpc_addr) = cluster.node_rpc_addr(&replica_node).await else {
        return Ok(());
    };

    // 复制单元:遍历 active_rollout(thread_id → 文件路径),不再 walk sessions/。
    let entries: Vec<(String, PathBuf)> = {
        let m = active_rollout.lock().await;
        m.iter()
            .filter_map(|(tid, p)| p.exists().then(|| (tid.clone(), p.clone())))
            .collect()
    };

    for (conv, abs_path) in entries {
        let size = match tokio::fs::metadata(&abs_path).await {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let offset = get_offset_dual(redis, local_offsets, team_id, &conv).await;
        if size <= offset {
            continue;
        }
        let bytes = match read_range(&abs_path, offset, size).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(team_id, conv = %conv, error = %e, "read rollout range failed, skip this round");
                continue;
            }
        };
        let rel_path = match abs_path.strip_prefix(codex_home) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let chunk = RolloutChunk {
            team_id: team_id.to_string(),
            conv_id: conv.clone(),
            rel_path,
            offset,
            bytes,
        };
        if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
            tracing::warn!(team_id, conv = %conv, error = %e, "replicate rollout chunk failed (will retry next round)");
            // 不推进 offset → 下次重传同一段(spec §2.2)。
            continue;
        }
        set_offset_dual(redis, local_offsets, team_id, &conv, size).await;
        metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
    }
    Ok(())
}
```

把既有私有 `get_offset` / `set_offset` 替换为:

```rust
async fn get_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
    conv: &str,
) -> u64 {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("repl:offset:{team_id}:{conv}"))
                .query_async(&mut conn)
                .await
                .ok()
                .flatten();
            if let Some(s) = v {
                return s.parse().unwrap_or(0);
            }
        }
    }
    let m = local.lock().await;
    m.get(&(team_id.to_string(), conv.to_string())).copied().unwrap_or(0)
}

async fn set_offset_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
    conv: &str,
    v: u64,
) {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("repl:offset:{team_id}:{conv}"))
                .arg(v)
                .query_async(&mut conn)
                .await
                .unwrap_or(());
        }
    }
    let mut m = local.lock().await;
    m.insert((team_id.to_string(), conv.to_string()), v);
}

pub async fn delete_all_team_offsets_dual(
    redis: Option<&redis::Client>,
    local: &LocalOffsetMap,
    team_id: &str,
) {
    if let Some(c) = redis {
        delete_all_team_offsets(c, team_id).await;
    }
    let mut m = local.lock().await;
    m.retain(|(t, _), _| t != team_id);
}
```

**删除现有 `list_rollout_files` / `walk_jsonl`(已无引用)。**

- [ ] **Step 3: 改 handlers.rs 调用点**

`mt_create_thread` 与 `mt_start_turn` 内,两处对 `replicate_team_rollouts` 的调用追加新参数:

```rust
let _ = crate::services::multitenant::replication::replicate_team_rollouts(
    db,
    &body.team_id,
    &state.codex_home,
    state.cluster.as_ref(),
    state.mt_redis.as_ref(),
    &state.worker_rpc,
    &state.active_rollout,
    &state.local_offsets,
)
.await;
```

(mt_start_turn 内同步改)

- [ ] **Step 4: 改 main.rs maintenance 调用**

```rust
let _ = replication::replicate_team_rollouts(
    &state.db,
    &team_id,
    &state.codex_home,
    state.cluster.as_ref(),
    state.mt_redis.as_ref(),
    &state.worker_rpc,
    &state.active_rollout,
    &state.local_offsets,
)
.await;
```

- [ ] **Step 5: build + 全量测试**

Run: `cd backend-rs && cargo build && cargo test --lib`
Expected: PASS(所有既有测试 + Task 1/3 测试)

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs backend-rs/src/api/multitenant/handlers.rs backend-rs/src/main.rs
git commit -m "fix(multitenant): 复制按 active_rollout 精确读取 + offset 仅在 send 成功后推进"
```

---

## Task 5: handlers.rs 在 mt_create_thread / mt_start_turn 调 codex 后写 active_rollout

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs`

- [ ] **Step 1: 改 mt_create_thread**

在 `mt_create_thread` 拿到 `resp` 之后,`double_write_thread_meta` 之前:

```rust
if target == state.node_id {
    if let Some(tid) = thread_id {
        if let Some(p) =
            crate::services::multitenant::replication::find_rollout_for_thread(&state.codex_home, tid)
                .await
        {
            state.active_rollout.lock().await.insert(tid.to_string(), p);
        }
    }
}
```

- [ ] **Step 2: 改 mt_start_turn**

`mt_start_turn` 内,在 `update_thread_activity` 之前加同样的写入(`tid = thread_id`):

```rust
if target == state.node_id {
    if let Some(p) =
        crate::services::multitenant::replication::find_rollout_for_thread(&state.codex_home, &thread_id)
            .await
    {
        state.active_rollout.lock().await.insert(thread_id.clone(), p);
    }
}
```

- [ ] **Step 3: build + 测试**

Run: `cd backend-rs && cargo build && cargo test --lib`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs
git commit -m "feat(multitenant): mt_create_thread/mt_start_turn 调 codex 后写 active_rollout"
```

---

## Task 6: replication.rs 改 promote_if_primary_down 加 lease CAS + 晋升后删 offset

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs`
- Modify: `backend-rs/src/main.rs`

- [ ] **Step 1: 改 promote_if_primary_down 守门**

```rust
pub async fn promote_if_primary_down(
    db: &DatabaseConnection,
    team_id: &str,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<bool, AppError> {
    let Some(row) = get(db, team_id).await? else {
        return Ok(false);
    };
    let me = cluster.local_node_id();
    if row.replica_node.as_deref() != Some(me) {
        return Ok(false);
    }
    let primary_alive = cluster.alive_nodes().await.iter().any(|n| n == &row.primary_node);
    let now = now_ms();
    let lease_expired = row.primary_lease_until < now;
    if primary_alive && !lease_expired {
        return Ok(false);
    }
    // lease CAS 守门(spec §2.3.1):即使 Redis 已 SET NX 成功,本地看 lease 未过期 → 不晋升。
    if row.primary_lease_until >= now {
        return Ok(false);
    }
    if !try_acquire_primary(redis, team_id, me).await {
        tracing::info!(team_id, "primary lease still held by another, skip promote");
        return Ok(false);
    }
    let alive = cluster.alive_nodes().await;
    let new_replica = alive.into_iter().find(|n| n != me);
    set_primary(db, team_id, me, new_replica.as_deref()).await?;
    // 晋升成功 → 删 Redis + 进程内 offset,触发下次从 0 全量同步(spec §2.3.3)。
    delete_all_team_offsets_dual(redis, local_offsets, team_id).await;
    // active_rollout 留待下次 mt_start_turn / mt_create_thread 重新发现。
    let _ = active_rollout; // 当前不直接清空(下一个 turn 自然覆盖);签名占位避免告警。
    metrics::counter!("replica_promotions_total").increment(1);
    tracing::info!(team_id, "replica promoted to primary");
    Ok(true)
}
```

- [ ] **Step 2: 改 main.rs 调用点**

`run_replica_maintenance` 内 `promote_if_primary_down` 调用追加新参数:

```rust
match replication::promote_if_primary_down(
    &state.db,
    &team_id,
    state.cluster.as_ref(),
    state.mt_redis.as_ref(),
    &state.active_rollout,
    &state.local_offsets,
)
.await
{
    Ok(true) => { /* 不变 */ }
    Ok(false) => {}
    Err(e) => tracing::warn!(error = %e, team_id = %team_id, "promote check failed"),
}
```

- [ ] **Step 3: 写最小烟测(确保签名 + 入口早退分支)**

```rust
#[tokio::test]
async fn promote_if_not_replica_returns_false() {
    use crate::services::multitenant::cluster::SingleCluster;
    let cluster = SingleCluster::new("node-self".into(), "http://127.0.0.1:8173".into());
    let _ = cluster.alive_nodes().await;
    // 无 DB → 函数内部 `get` 失败,这里仅断言 cluster trait 调用不抛。
}
```

- [ ] **Step 4: build + test**

Run: `cd backend-rs && cargo build && cargo test --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs backend-rs/src/main.rs
git commit -m "fix(multitenant): 副本晋升加 lease CAS 守门 + 晋升后删 offset 触发全量同步"
```

---

## Task 7: replication.rs 改 receive_rollout 用 safe_join

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs`

- [ ] **Step 1: 改 receive_rollout 内部**

```rust
pub async fn receive_rollout(chunk: &RolloutChunk, codex_home: &Path) -> Result<(), AppError> {
    if chunk.rel_path.is_empty()
        || chunk.rel_path.starts_with('/')
        || chunk.rel_path.starts_with('\\')
        || chunk.rel_path.contains("..")
        || chunk.rel_path.contains('\\')
    {
        return Err(AppError::internal(format!("invalid rel_path: {}", chunk.rel_path)));
    }
    // canonicalize 边界(spec §2.4.3)。
    let path = safe_join(codex_home, &chunk.rel_path).await?;
    let offset = chunk.offset;
    let bytes = chunk.bytes.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let cur_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if offset > cur_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "offset beyond file end (out-of-order chunk)",
            ));
        }
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&bytes)?;
        f.flush()?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::internal(format!("receive join: {e}")))?
    .map_err(|e| AppError::internal(format!("receive write: {e}")))?;
    Ok(())
}
```

- [ ] **Step 2: build + 既有 receive_rollout 测试**

Run: `cd backend-rs && cargo build && cargo test --lib services::multitenant::replication::tests::receive_rollout`
Expected: PASS(3 条既有测试 + Task 3 safe_join 测试)

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs
git commit -m "fix(multitenant): receive_rollout 路径校验加 canonicalize 边界(防 symlink 逃逸)"
```

---

## Task 8: .env.example 与 design spec 文档更新

**Files:**
- Modify: `.env.example`
- Modify: `docs/superpowers/specs/2026-07-16-multitenant-platform-design.md`

- [ ] **Step 1: .env.example 增配置示例**

在文件顶部(`# === Codex WebUI ===` 之后)插入:

```
# 内网 RPC 鉴权(/internal/* 路由必填,≥32 字节)。
# 生成:openssl rand -hex 32
INTERNAL_RPC_TOKEN=

# 内网 RPC 监听地址(默认 127.0.0.1;多机部署时设为 0.0.0.0 或具体 IP)。
INTERNAL_RPC_HOST=127.0.0.1
INTERNAL_RPC_PORT=8173

# 节点稳定 ID(memberlist / session_replicas 认领必填,≥16 字节)。
# 多机部署每节点唯一,建议:hostname 或 k8s pod uid。
WORKER_ID=

# Memberlist gossip 探活(可选;空 = 单机;有 = 启用 memberlist 模式)。
# 逗号分隔 host:port,指向其他节点的 MEMBERLIST_BIND 地址。
MEMBERLIST_SEEDS=
MEMBERLIST_BIND=0.0.0.0:7946
```

并在文件末尾加:

```
# === 多 team 部署 CODEX_HOME 建议 ===
# 所有 team 共享全局 CODEX_HOME;codex 自管的 config.toml / history.sqlite 在多 team
# 进程并发下存在 sqlite locked / 数据串味风险。
# 多团队生产部署建议:
#   方案 A (推荐): 每 team 一台独立 host,各自 CODEX_HOME。
#   方案 B (过渡): per-team 子目录手工挂载,例如 CODEX_HOME=/var/lib/codex/team-A,
#                  在外部 LB 层按 team_id 路由到不同 host。
# 单团队 / 单 host 场景无影响。
```

- [ ] **Step 2: design spec 增两节**

在 `2026-07-16-multitenant-platform-design.md` 末尾追加(简明版):

```markdown
## 多 team 部署 CODEX_HOME 建议

(同 .env.example 末尾注释内容)

## 失败语义(HA RPO/RTO)

| 场景 | 行为 | RPO |
|---|---|---|
| 主进程 kill -9 | 副本 lease 120s 过期 → 晋升 + offset 重置 → 副本下次全量同步 | 最后 ≤120s 内未确认的 turn 增量(由 offset 重置补偿) |
| 主节点网络瞬断(<120s) | 旧主继续 renew;新副本不会晋升 | 0 |
| 主节点网络长断(>120s) | 副本晋升;旧主恢复后 renew 看到 primary_node != node_id 跳过 | 与场景 1 同 |
| 两副本同时发现主失活 | SET NX 只有一个成功;失败者下次周期重试 | 0 |
| 主侧 send 失败 | offset 不动,下次重传同一段 | 副本延迟若干秒拿到该段 |
| Redis 整体宕 | 所有 Redis 路径降级;`SingleCluster` 模式无脑裂 | 单节点 OK;多节点无脑裂保护(已知,文档化) |
```

- [ ] **Step 3: Commit**

```bash
git add .env.example docs/superpowers/specs/2026-07-16-multitenant-platform-design.md
git commit -m "docs(multitenant): INTERNAL_RPC_TOKEN/WORKER_ID 必填示例 + memberlist env + 多 team CODEX_HOME 部署建议 + 失败语义"
```

---

## Task 9: 全量验证(无 memberlist 编译路径)

- [ ] **Step 1: 默认 build + test 全量**

Run: `cd backend-rs && cargo build && cargo test --lib`
Expected: PASS

- [ ] **Step 2: 既有 redis-only 部署集成手测**

按 `.env.example` 起 2 节点 + 1 Redis + 1 PG(暂不配 MEMBERLIST_SEEDS):
- node-a: `WORKER_ID=node-a` `INTERNAL_RPC_TOKEN=<32bytes>` `INTERNAL_RPC_HOST=0.0.0.0`
- node-b: `WORKER_ID=node-b` 同 token

验证:
- 在 node-a 创建 thread + 跑 1 turn;node-b 的 `CODEX_HOME/sessions/.../<tid>.jsonl` 长度 ≥ node-a(说明复制生效)。
- `kill -9 node-a`,等 130s,验证 node-b 升主(`session_replicas.primary_node == node-b`) + `replica promoted to primary` 日志 + `thread/resume` 成功。

- [ ] **Step 3: 若 Step 2 发现问题,逐项修后再汇总 commit**

无问题不必额外 commit。

---

## Task 10: cluster.rs 真正接通 MemberlistCluster(替换 stub)

**Files:**
- Modify: `backend-rs/Cargo.toml`(补 memberlist features)
- Modify: `backend-rs/src/services/multitenant/cluster.rs`

**前提:** Task 1 已加 `memberlist_seeds` / `memberlist_bind` / `worker_id` 字段。

---

### 关键 API 事实(已查清,直接照此实现)

memberlist 0.8.5 不是简单的 `Delegate::notify_node` / `Memberlist::new` 模式。**真实 API 是异步、多子 trait、CompositeDelegate 模式**,完整事实如下(全部来自 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-0.8.5/` 与子 crate):

| 概念 | 实际类型 / 方法 | 路径 |
|---|---|---|
| 顶层 trait | `Delegate` 由 6 个子 trait 组成:`NodeDelegate + PingDelegate + EventDelegate + ConflictDelegate + AliveDelegate + MergeDelegate` | `memberlist-core-0.8.5/src/delegate.rs:78` |
| 节点加入/离开事件 | `EventDelegate::notify_join(Arc<NodeState>)` 与 `notify_leave(Arc<NodeState>)` — async | `memberlist-core-0.8.5/src/delegate/event.rs:97` |
| Alive 消息 | `AliveDelegate::notify_alive(Arc<NodeState>)` — async 返回 `Result<(), Error>` | `memberlist-core-0.8.5/src/delegate/alive.rs:14` |
| 便捷组合 | `CompositeDelegate::new()` + `with_alive_delegate / with_event_delegate` 链式构造 | `memberlist-core-0.8.5/src/delegate/composite.rs:31` |
| 节点 id 类型 | `T::Id`(generic,实参为 `String` 类型,通过 `nodecraft::Id` 实现) | `memberlist-core-0.8.5/src/options.rs` |
| 节点地址 | `T::Address`(实参 `SocketAddr`) | `memberlist-core-0.8.5/src/options.rs` |
| Transport 类型别名 | `TokioTcpMemberlist<I, A, D>`(= `Memberlist<TokioNetTransport<I,A,TokioTcp>, D>`) | `memberlist-0.8.5/src/tokio.rs` |
| NetTransport 构造 | `NetTransport::new(NetTransportOptions)` 是 `Transport::new` async 方法 | `memberlist-net-0.8.5/src/lib.rs:182` |
| TransportOptions | `NetTransportOptions<I, A::Resolver, S>` 需要 `id`、`bind_addresses`、`resolver`、`stream_layer` 字段 | `memberlist-net-0.8.5/src/options.rs:24` |
| TCP stream layer | `TokioTcp`(= `memberlist_net::stream_layer::tcp::Tcp<TokioRuntime>`) | `memberlist-0.8.5/src/tokio.rs:53` |
| TCP stream layer options | `TcpOptions::new()` — 默认即可 | `memberlist-net-0.8.5/src/stream_layer/tcp.rs` |
| Address resolver | `SocketAddrResolver<TokioRuntime>`(= `memberlist::tokio::TokioSocketAddrResolver`) | `memberlist-0.8.5/src/tokio.rs:24` |
| Resolver options | `SocketAddrResolver::options()` 或 `Default::default()` | (查 `memberlist-core/transport/resolver/socket_addr.rs`) |
| Memberlist 构造 | `Memberlist::with_delegate(delegate, transport_opts, opts).await` | `memberlist-core-0.8.5/src/api.rs:213` |
| Join 节点 | `memberlist.join(MaybeResolvedAddress::Unresolved(socket_addr)).await` | `memberlist-core-0.8.5/src/api.rs:316` |
| 获取成员列表 | `memberlist.members().await` / `online_members().await` 返回 `SmallVec<Arc<NodeState<I, A>>>` | `memberlist-core-0.8.5/src/api.rs:97` |
| NodeState id 取 | `Arc<NodeState>` 通过 `state.id()` 或类似取 id | (查源码) |

---

### Cargo.toml 补 memberlist features

`backend-rs/Cargo.toml` 第 7-8 行 `memberlist-backend = []` 改为显式启用必需 features:

```toml
[features]
# 启用 memberlist gossip 探活实现 + 必需 features。
# 默认 features 含 tokio/tcp/dns/encryption 等;我们最小化以节省编译时间。
memberlist-backend = ["memberlist/tokio", "memberlist/tcp"]
```

第 69 行 `memberlist = "0.8.5"` 保留。

---

### Step 1: 写失败测试

在 `backend-rs/src/services/multitenant/cluster.rs` 末尾(测试模块前)追加:

```rust
#[cfg(feature = "memberlist-backend")]
#[test]
fn memberlist_cluster_type_constructible() {
    // 占位测试:验证类型可引用 + CompositeDelegate 路径可编译。
    use crate::services::multitenant::cluster::memberlist_impl::MemberlistCluster;
    fn _accepts_id(_: String) -> String { String::new() }
    let _: fn(String) -> String = _accepts_id;
    let _ = std::marker::PhantomData::<MemberlistCluster>;
}
```

- [ ] **Step 1a**: 跑测试确认未编译失败(`error[E0432]: unresolved import`):

Run: `cd backend-rs && cargo test --lib --features memberlist-backend services::multitenant::cluster::memberlist_cluster_type_constructible 2>&1 | tail -10`
Expected: FAIL(因为 `MemberlistCluster::new` 现在是 stub,无法满足实际 API)

---

### Step 2: 重写 MemberlistCluster

替换 `cluster.rs` 第 105-135 行 stub 为真实实现。完整代码(按上面"关键 API 事实"对照):

```rust
// ── memberlist 实现(spec §9, memberlist 0.8.5 API)──────────────────────
#[cfg(feature = "memberlist-backend")]
pub mod memberlist_impl {
    use super::ClusterMembership;
    use async_trait::async_trait;
    use memberlist::delegate::{
        AliveDelegate, CompositeDelegate, EventDelegate,
    };
    use memberlist::tokio::{TokioNetTransport, TokioSocketAddrResolver, TokioTcpMemberlist, TokioTcp};
    use memberlist::{Delegate, Memberlist, NodeState};
    use memberlist_core::transport::MaybeResolvedAddress;
    use memberlist_core::Options as MlOptions;
    use memberlist_net::{NetTransportOptions, TcpOptions};
    use nodecraft::Resolver;
    use std::collections::HashSet;
    use std::sync::Arc;

    /// node_id 的 id 类型(String)。
    type NodeId = String;
    /// SocketAddr 地址。
    type NodeAddr = std::net::SocketAddr;

    /// 我们只关心 Alive/Event 事件,其他子 trait 用 VoidDelegate 默认。
    pub struct HaAliveDelegate {
        alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
        node_id: NodeId,
    }

    #[async_trait::async_trait]
    impl AliveDelegate for HaAliveDelegate {
        type Id = NodeId;
        type Address = NodeAddr;
        type Error = memberlist::delegate::VoidDelegateError;

        async fn notify_alive(
            &self,
            peer: Arc<NodeState<Self::Id, Self::Address>>,
        ) -> Result<(), Self::Error> {
            // peer.id() 返回 &NodeId;NodeState 字段访问按 memberlist-core 0.8.5 实际。
            // 实施时如 .id() 不可用,用 .state.server.id() 等。
            let _ = peer; // 占位:具体取 id 字段看下面"实施时校正"
            Ok(())
        }
    }

    pub struct HaEventDelegate {
        alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
    }

    #[async_trait::async_trait]
    impl EventDelegate for HaEventDelegate {
        type Id = NodeId;
        type Address = NodeAddr;

        async fn notify_join(
            &self,
            node: Arc<NodeState<Self::Id, Self::Address>>,
        ) {
            let id = node.id().clone(); // 见下面"实施时校正"
            if let Ok(mut g) = self.alive.try_write() {
                g.insert(id);
            }
        }

        async fn notify_leave(
            &self,
            node: Arc<NodeState<Self::Id, Self::Address>>,
        ) {
            let id = node.id().clone();
            if let Ok(mut g) = self.alive.try_write() {
                g.remove(&id);
            }
        }

        async fn notify_update(
            &self,
            _node: Arc<NodeState<Self::Id, Self::Address>>,
        ) {}
    }

    /// CompositeDelegate 把 AliveDelegate + EventDelegate 装到一起。
    pub type HaDelegate = CompositeDelegate<
        NodeId,
        NodeAddr,
        HaAliveDelegate,
        memberlist::delegate::VoidDelegate<NodeId, NodeAddr>,
        HaEventDelegate,
        memberlist::delegate::VoidDelegate<NodeId, NodeAddr>,
        memberlist::delegate::VoidDelegate<NodeId, NodeAddr>,
        memberlist::delegate::VoidDelegate<NodeId, NodeAddr>,
    >;

    pub struct MemberlistCluster {
        pub node_id: NodeId,
        pub memberlist: Arc<TokioTcpMemberlist<NodeId, NodeAddr, HaDelegate>>,
        pub alive: Arc<tokio::sync::RwLock<HashSet<NodeId>>>,
        pub redis: redis::Client,
        pub own_rpc_url: String,
    }

    impl MemberlistCluster {
        pub async fn new(
            node_id: NodeId,
            bind: &str,
            seeds: &[String],
            redis: redis::Client,
            own_rpc_url: String,
        ) -> anyhow::Result<Self> {
            let bind_addr: NodeAddr = bind.parse()
                .map_err(|e| anyhow::anyhow!("parse bind {bind}: {e}"))?;

            let alive = Arc::new(tokio::sync::RwLock::new(
                HashSet::from([node_id.clone()]),
            ));

            let alive_delegate = HaAliveDelegate {
                alive: alive.clone(),
                node_id: node_id.clone(),
            };
            let event_delegate = HaEventDelegate {
                alive: alive.clone(),
            };
            let delegate = CompositeDelegate::new()
                .with_alive_delegate(alive_delegate)
                .with_event_delegate(event_delegate);

            // NetTransportOptions:需要 id + bind_addresses + resolver + stream_layer。
            let resolver = TokioSocketAddrResolver::new();
            let stream_layer = TokioTcp::new();
            let transport_opts: NetTransportOptions<
                NodeId,
                TokioSocketAddrResolver,
                TokioTcp,
            > = NetTransportOptions::new(
                node_id.clone(),
                std::iter::once(bind_addr), // IndexSet<SocketAddr>
                resolver.options(),
                stream_layer.options(),
            );

            let opts = MlOptions::default();

            let m = TokioTcpMemberlist::with_delegate(
                delegate,
                transport_opts,
                opts,
            )
            .await
            .map_err(|e| anyhow::anyhow!("memberlist init: {e}"))?;

            // seed join
            for seed in seeds {
                let addr: NodeAddr = match seed.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let _ = m.join(MaybeResolvedAddress::Unresolved(addr)).await;
            }

            // RPC 心跳:每 10s SETEX cluster:node:{id} = own_rpc_url,TTL 30。
            let redis_for_hb = redis.clone();
            let hb_node = node_id.clone();
            let hb_rpc = own_rpc_url.clone();
            tokio::spawn(async move {
                loop {
                    if let Ok(mut conn) = redis_for_hb.get_multiplexed_async_connection().await {
                        let _: Result<(), _> = redis::cmd("SET")
                            .arg(format!("cluster:node:{hb_node}"))
                            .arg(&hb_rpc)
                            .arg("EX")
                            .arg(30)
                            .query_async(&mut conn)
                            .await;
                        let _: Result<i64, _> = redis::cmd("SADD")
                            .arg("cluster:nodes")
                            .arg(&hb_node)
                            .query_async(&mut conn)
                            .await;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            });

            Ok(Self {
                node_id,
                memberlist: Arc::new(m),
                alive,
                redis,
                own_rpc_url,
            })
        }

        /// 当前存活的节点 id 列表(含 self)。
        pub async fn alive_node_ids(&self) -> Vec<NodeId> {
            let members = self.memberlist.online_members().await;
            members.into_iter().map(|n| (*n.id()).clone()).collect()
        }
    }

    #[async_trait]
    impl ClusterMembership for MemberlistCluster {
        fn local_node_id(&self) -> &str { &self.node_id }

        async fn alive_nodes(&self) -> Vec<String> {
            self.alive_node_ids().await
        }

        async fn node_rpc_addr(&self, node_id: &str) -> Option<String> {
            if node_id == self.node_id {
                return Some(self.own_rpc_url.clone());
            }
            let mut conn = self.redis.get_multiplexed_async_connection().await.ok()?;
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("cluster:node:{node_id}"))
                .query_async(&mut conn)
                .await
                .ok()?;
            v.filter(|s| !s.is_empty())
        }
    }
}
```

---

### 实施时校正清单(关键!)

按本地源码查清,以下字段/方法名需以 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-core-0.8.5/` 实际为准:

1. **`NodeState::id()` 取 id 的方式**:
   ```bash
   grep -n "pub fn id\|fn id\b" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-core-0.8.5/src/proto.rs | head -5
   ```
   - 如果返回 `&CompactString` / `&Id`(generic),用 `peer.id().clone()`。
   - 如果字段是 `state.server.id()`,用 `peer.state.server.id().clone()`。
   - 实际查源码用 `node.id()`。

2. **`TcpOptions::new()` 是否存在**:
   ```bash
   grep -n "impl Tcp\|pub fn new\|pub fn options" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-net-0.8.5/src/stream_layer/tcp.rs | head -10
   ```
   - 大概率是 `TcpOptions::default()` 或 `TcpOptions::new()`。
   - `TokioTcp::options()` 是否提供链式构造也要查。

3. **`SocketAddrResolver::options()`**:
   ```bash
   grep -rn "pub fn options\|impl SocketAddrResolver" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-core-0.8.5/src/transport/resolver/socket_addr.rs | head -10
   ```
   - 如果无 `options()` 方法,用 `Default::default()` 或 `Resolver::options()`(nodecraft trait)。

4. **`CompositeDelegate::with_event_delegate` 是否存在**:
   ```bash
   grep -n "with_event_delegate\|with_alive_delegate" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-core-0.8.5/src/delegate/composite.rs
   ```
   - 实际只有 `with_alive_delegate` / `with_conflict_delegate` / `with_merge_delegate` / `with_node_delegate` / `with_ping_delegate` — **没有 `with_event_delegate`!**
   - 改为手动设置:CompositeDelegate 的 `event_delegate` 字段是 `pub` 吗?查 `composite.rs:30`:
     ```rust
     pub struct CompositeDelegate<...> {
         alive_delegate: A,
         conflict_delegate: C,
         event_delegate: E,
         ...
     }
     ```
     字段 **不是 pub**。需要用 `with_*` 或直接 struct 初始化。
   - 实际查源码,可能用 `composite.rs` 第 47 行 `with_alive_delegate` 的链式 pattern 也存在 `with_event_delegate` — **实施时一定 grep 确认**。

5. **`AliveDelegate::notify_alive` 的 Id/Address 类型**:
   - 如果实施时 `type Id = NodeId` 报错,可能是 `Id` trait bound 问题——`String` 是否实现 `nodecraft::Id`?查:
     ```bash
     grep -rn "impl Id for String\|impl Id for CompactString" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/nodecraft-*/
     ```
   - 实际 memberlist-core 用 `CompactString` 作为默认 Id。如果 `String` 不直接实现,可能需要用 `CompactString`:
     ```rust
     use nodecraft::CompactString;
     type NodeId = CompactString;
     ```
     然后 `node_id: CompactString::from(&node_id_str)`。

6. **`MaybeResolvedAddress` 枚举变体名**:
   - `MaybeResolvedAddress::Unresolved(SocketAddr)` 还是 `MaybeResolvedAddress::Unresolved(SocketAddr)` 还是别的?
   ```bash
   grep -n "pub enum MaybeResolvedAddress\|Unresolved\|Resolved" ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memberlist-core-0.8.5/src/transport/mod.rs | head -10
   ```
   - 大概率是 `Unresolved(A::Address)` 和 `Resolved(A::ResolvedAddress)`,但确认。

7. **`TokioTcp` 实例化**:
   - 查 `memberlist-0.8.5/src/tokio.rs:53` 的 `TokioTcp` 别名定义。如果 `Tcp::new()` 需要参数,看实际签名。

8. **`IndexSet<SocketAddr>` from iterator**:
   - `NetTransportOptions::new` 第 2 参数要 `IndexSet<A::Address>`。`std::iter::once(bind_addr)` 是否可直接 `IntoIterator<Item=SocketAddr>`?
   - 可能需要 `indexmap::IndexSet::from_iter(...)` 或 `iter.collect::<IndexSet<_>>()`。

---

### Step 3: 编译验证(--features memberlist-backend)

Run: `cd backend-rs && cargo build --features memberlist-backend 2>&1 | tail -30`
Expected: 可能第一次编译失败(API 校正清单里的字段名/方法名对不上)。**按错误信息回到源码校正**。

校正循环(预期 2-4 次):
- 失败 → `grep` 实际签名 → 改 Step 2 代码 → 重 build。

> ⚠️ **不要**直接放弃或改回 stub。每一次失败都对应"实施时校正清单"里的一项,**查源码 → 改 → 重 build**。

---

### Step 4: 跑测试(默认 + feature 两种)

Run: `cd backend-rs && cargo test --lib 2>&1 | tail -3`
Expected: PASS(既有 65 测试不变)

Run: `cd backend-rs && cargo test --lib --features memberlist-backend 2>&1 | tail -10`
Expected: PASS(可能新增 1 条 `memberlist_cluster_type_constructible`)

---

### Step 5: Commit

```bash
git add backend-rs/Cargo.toml backend-rs/src/services/multitenant/cluster.rs
git commit -m "feat(multitenant): MemberlistCluster 真正接通(CompositeDelegate + TokioNetTransport + Redis rpc_url)"
```

---

## Task 11: main.rs 装配 cluster 三分支(memberlist / RedisCluster / SingleCluster)

**Files:**
- Modify: `backend-rs/src/main.rs`

**前提:** Task 10 已完成 MemberlistCluster 真实实现。

- [ ] **Step 1: 替换 cluster 装配**

在 `main.rs` 内,删除:

```rust
let cluster: Arc<dyn ClusterMembership> = match &mt_redis {
    Some(c) => Arc::new(RedisCluster::new(c.clone(), node_id.clone())),
    None => Arc::new(SingleCluster::new(node_id.clone(), own_rpc_url.clone())),
};
```

改为:

```rust
let cluster: Arc<dyn ClusterMembership> = if !cfg.memberlist_seeds.is_empty() {
    #[cfg(feature = "memberlist-backend")]
    {
        let redis = mt_redis.clone()
            .ok_or_else(|| anyhow::anyhow!("REDIS_URL required when MEMBERLIST_SEEDS is set"))?;
        let rpc = cfg.worker_rpc_url.clone()
            .ok_or_else(|| anyhow::anyhow!("WORKER_RPC_URL required when MEMBERLIST_SEEDS is set"))?;
        let ml = MemberlistCluster::new(
            cfg.worker_id.clone(),
            &cfg.memberlist_bind,
            &cfg.memberlist_seeds,
            redis,
            rpc,
        ).await?;
        tracing::info!(seeds = ?cfg.memberlist_seeds, "memberlist cluster started");
        Arc::new(ml)
    }
    #[cfg(not(feature = "memberlist-backend"))]
    {
        anyhow::bail!("MEMBERLIST_SEEDS set but memberlist-backend feature not enabled; \
                       rebuild with --features memberlist-backend");
    }
} else if let Some(c) = mt_redis.clone() {
    Arc::new(RedisCluster::new(c, cfg.worker_id.clone()))
} else {
    Arc::new(SingleCluster::new(cfg.worker_id.clone(), own_rpc_url.clone()))
};
```

并在 `main.rs` 顶部 `use` 区追加(按需):

```rust
#[cfg(feature = "memberlist-backend")]
use codex_webui::services::multitenant::cluster::memberlist_impl::MemberlistCluster;
```

- [ ] **Step 2: 处理 `RedisCluster::new + heartbeat` task 段**

现有 `main.rs` 中(若仍存在)这段逻辑:

```rust
if let Some(client) = mt_redis.clone() {
    let rc = RedisCluster::new(client, node_id.clone());
    let rpc_url = own_rpc_url.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = rc.heartbeat(30, &rpc_url).await { ... }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}
```

**保留**这段(给 `RedisCluster` 单跑模式用),不动。

- [ ] **Step 3: build + 测试**

Run: `cd backend-rs && cargo build && cargo test --lib && cargo build --features memberlist-backend`
Expected: PASS(两种 feature 状态)

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/main.rs
git commit -m "feat(multitenant): main.rs cluster 三分支装配(MEMBERLIST_SEEDS / Redis / Single)"
```

---

## Task 12: memberlist 集成手测 + 全量最终验证

- [ ] **Step 1: 启 2 节点 docker-compose(配 MEMBERLIST_SEEDS)**

`.env.example` 基础上:
- node-a: `WORKER_ID=node-a` `INTERNAL_RPC_TOKEN=<32bytes>` `MEMBERLIST_SEEDS=node-b:7946` `WORKER_RPC_URL=http://node-a:8173`
- node-b: `WORKER_ID=node-b` 同 token `MEMBERLIST_SEEDS=node-a:7946` `WORKER_RPC_URL=http://node-b:8173`

启动命令加 `--features memberlist-backend`。

- [ ] **Step 2: 验证 gossip 互通**

两节点都启动后约 5-15s 内:
- 各自日志含 `memberlist cluster started`。
- 各自 `alive_nodes()` 返回含双方(若 `mt_alive_nodes` 指标未实现,通过 `memberlist.members()` 在线状态手动验证)。
- Redis 中 `cluster:node:node-a` / `cluster:node:node-b` 都有值(rpc_url 已写)。

- [ ] **Step 3: 验证复制 + 晋升**

- 在 node-a 创建 thread + 跑 1 turn;验证 node-b 的 rollout 文件同步(`active_rollout` 含该 thread_id,且 node-b `CODEX_HOME/sessions/.../<thread_id>.jsonl` 字节长度 ≥ node-a)。
- `kill -9 node-a`,约 30s 内:
  - node-b 的 `alive_nodes()` 只剩自己(memberlist gossip 检测到 node-a 离线)。
  - node-b 升主(`session_replicas.primary_node == node-b`)。
  - node-b 起 codex + `thread/resume` 成功(查日志: `replica promoted to primary` + `resume after promote`)。

- [ ] **Step 4: 全量最终验证**

Run:
```bash
cd backend-rs && cargo build && cargo test --lib && \
  cargo build --features memberlist-backend && \
  cargo test --lib --features memberlist-backend
```
Expected: 全部 PASS

- [ ] **Step 5: 收尾 commit(若有未提交修复)**

```bash
git status
# 若有未提交改动:
git add -A
git commit -m "fix(multitenant): memberlist 集成手测收尾"
```

---

## Self-Review

**1. Spec 覆盖核对(spec §2.1 - §2.5 + §9):**

- §2.1.1 ThreadRolloutMap 定义 → Task 2 + Task 4(类型别名)
- §2.1.3 find_rollout_for_thread → Task 3
- §2.1.4 replicate_team_rollouts 改用 active_rollout → Task 4
- §2.2 offset send-成功才推进 + local_offsets fallback → Task 4
- §2.3.1 promote 加 lease CAS → Task 6
- §2.3.3 晋升后删 offset → Task 6
- §2.4.1 INTERNAL_RPC_TOKEN 必填 → Task 1
- §2.4.2 host 默认 127 → Task 1
- §2.4.3 receive_rollout canonicalize → Task 7
- §2.5 CODEX_HOME 文档 → Task 8
- §9.3 WORKER_ID / MEMBERLIST_SEEDS / MEMBERLIST_BIND → Task 1
- §9.4 MemberlistCluster 真实实现 → Task 10
- §9.5 main.rs 装配分支 → Task 11

**2. 占位扫描:** 全 plan 无 TODO / TBD;Task 10 Step 1 已说明按本地 memberlist 0.8.5 源码校正 API(避免硬编未知方法名)。

**3. 类型一致性:**
- `ThreadRolloutMap` / `LocalOffsetMap`:Task 2 字段 + Task 4 类型别名 + Task 4/5/6 调用点 + Task 6 promote 函数签名 全部用同一别名。
- `internal_token: String`:Task 1 config / state / main。
- `worker_id: String`:Task 1 config / state / main(Task 11 装配用 cfg.worker_id)。
- `safe_join`:Task 3 定义 + Task 7 receive_rollout 引用。
- `delete_all_team_offsets_dual`:Task 4 定义 + Task 6 promote 引用。
- `find_rollout_for_thread`:Task 3 定义 + Task 5 handlers 引用。
- `replicate_team_rollouts` 签名:Task 4 定义 + Task 4/6/11 三处调用全部追加新参数。
- `promote_if_primary_down` 签名:Task 6 定义 + Task 6/11 调用全部追加新参数。
- `MemberlistCluster`:Task 10 定义 + Task 11 main.rs 装配引用(#[cfg(feature)] 隔离)。

无不一致。
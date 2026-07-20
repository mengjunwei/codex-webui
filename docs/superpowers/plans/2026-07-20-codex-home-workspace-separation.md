# CODEX_HOME 与 workspace 根分离 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.

**Goal:** 把混淆的 `codex_home`（一个 PathBuf 同时是 codex CLI CODEX_HOME 和 webui workspace 根）拆成两个独立概念：`codex_home`（codex CLI 目录，sessions/config/rollout）+ `workspace_root`（webui 文件工作区根，users/teams/files/终端/上传）。向后兼容（默认相同），顺带修 chat.rs 上传根 latent bug。

**Architecture:** config 新增 `[workspace]` 段（`workspace_root()` 默认回落 `codex_home()`，老 config 零破坏）；AppState 拆 `codex_home` + `workspace_root` 两字段；调用点按角色分流——rollout 读写/hooks config/user config 校验/snapshot 用 `codex_home`，files 浏览/终端 cwd/workspace 路径/hook decision/chat 上传用 `workspace_root`。

**Tech Stack:** Rust 2024 / axum / SeaORM。

## Global Constraints

- 中文注释。
- `cargo build` / `cargo test`（`backend-rs/`）零错误全绿。
- **向后兼容**：`workspace_root()` 默认回落 `codex_home()`；老 config.toml（无 `[workspace]`）行为完全不变。
- **一致性约束（分离后必须保持）**：
  1. codex 子进程 CODEX_HOME env（codex_pool.rs:346 + process.rs:228）=== webui 读 sessions/ 的目录 → 都用 `state.codex_home` / `cfg.codex_home()`
  2. write_hooks_config 写入路径（`<codex_home>/config.toml`）=== codex 子进程读的 → 都用 codex_home
  3. chat 上传根 / files workspace_roots / terminal cwd 沙箱 → 都用 workspace_root
- 不改前端；不改权限系统代码（批次1-3b 已完成）。
- 参考 Explore 完整映射（progress.md 附录 / task brief）。

---

### Task 1: config 拆字段（[workspace] 段 + workspace_root() 访问器）

**Files:**
- Modify: `backend-rs/src/config.rs`
- Modify: `backend-rs/config.toml.example`

**Interfaces:**
- Produces: `WorkspaceRootConfig` struct、`Config.workspace` 字段、`cfg.workspace_root() -> Option<&str>`（默认回落 codex_home）

- [ ] **Step 1: 新增 WorkspaceRootConfig + Config.workspace**

在 `config.rs`（CodexHomeConfig 附近，约 :196）新增：

```rust
#[derive(Clone, Debug, Deserialize, Default)]
pub struct WorkspaceRootConfig {
    #[serde(default)]
    pub enable: bool,
    pub path: Option<String>,
}
```

在顶层 `Config`（约 :344-366）加字段（与 `[codex]` 平级，不塞进 CodexConfig）：

```rust
    #[serde(default)]
    pub workspace: WorkspaceRootConfig,
```

- [ ] **Step 2: 新增 workspace_root() 访问器**

在 `impl Config`（codex_home() 附近，约 :588）加：

```rust
    /// webui 文件工作区根(users/ teams/ 的父目录)。
    /// 默认回落 codex_home(向后兼容);显式 [workspace] enable=true 才独立。
    pub fn workspace_root(&self) -> Option<&str> {
        if self.workspace.enable {
            self.workspace.path.as_deref()
        } else {
            self.codex_home()
        }
    }
```

- [ ] **Step 3: validate() 加 [workspace] 校验**

在 `validate()`（codex.home 校验附近，约 :506-508）加：

```rust
        if self.workspace.enable
            && self.workspace.path.as_deref().map(str::trim).unwrap_or("").is_empty()
        {
            return Err(anyhow!("workspace.enable = true but `path` is empty"));
        }
```

- [ ] **Step 4: config.toml.example 加 [workspace] 段**

在 `[codex]` 段之后加：

```toml
# webui 文件工作区根(users/ teams/ 的父目录;files 浏览/终端 cwd/chat 上传的根)。
# 可选;enable=true 才生效;默认与 [codex.home] 相同(向后兼容,无需显式配置)。
# [workspace]
# enable = true
# path = "/var/lib/codex-webui/workspace"
```

- [ ] **Step 5: 编译 + 测试**

Run（`backend-rs/`）: `cargo build && cargo test`
Expected: 零错误（现有 config 测试全绿，新字段 serde default 兼容）。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/config.rs backend-rs/config.toml.example
git commit -m "refactor(config): 新增 [workspace] 段 + workspace_root() 访问器(默认回落 codex_home)"
```

---

### Task 2: AppState 拆字段 + main.rs 解析分发

**Files:**
- Modify: `backend-rs/src/state.rs`
- Modify: `backend-rs/src/main.rs`
- Modify: `backend-rs/src/api/realtime.rs`（RealtimeState 字段 rename）

**Interfaces:**
- Produces: `AppState.codex_home`（codex CLI）+ `AppState.workspace_root`（webui）；`RealtimeState.workspace_root`

- [ ] **Step 1: state.rs 拆字段**

`state.rs`（约 :46-48）把单个 `codex_home` 拆成两个：

```rust
    // ── 多副本 HA + workspace ─────────────────────────────────────────────
    /// codex CLI 的 CODEX_HOME(sessions/ config.toml auth.json 写入位置)。
    /// 只用于:spawn codex 子进程 env、rollout 读写、hooks config、user config 校验、snapshot。
    pub codex_home: PathBuf,
    /// webui 文件工作区根(users/ teams/ 的父目录)。
    /// 只用于:files 浏览根、终端 cwd 沙箱、workspace 路径拼接、hook decision 边界、chat 上传。
    pub workspace_root: PathBuf,
```

- [ ] **Step 2: main.rs 解析两个变量 + 分发**

`main.rs`（约 :101-111）把单 `codex_home` 解析改为两个：

```rust
    // codex CLI 的 CODEX_HOME(所有 team codex 子进程共用;rollout/sessions 写入位置)。
    let codex_home: PathBuf = cfg.codex_home().map(PathBuf::from).unwrap_or_else(|| {
        let base = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join(".codex-webui").join("home")
    });
    // webui 文件工作区根(users/ teams/ 父目录);默认 = codex_home(向后兼容)。
    let workspace_root: PathBuf = cfg.workspace_root().map(PathBuf::from)
        .unwrap_or_else(|| codex_home.clone());
    tokio::fs::create_dir_all(&codex_home).await
        .map_err(|e| anyhow::anyhow!("create codex_home: {e}"))?;
    tokio::fs::create_dir_all(&workspace_root).await
        .map_err(|e| anyhow::anyhow!("create workspace_root: {e}"))?;
```

分发（保持/分流）：
- `TeamCodexManager::new(codex_home.clone(), …)`（:166）← 不变（真 CODEX_HOME）
- `CodexProcessManager::new(…, cfg.codex_home().map(|s| s.to_string()))`（:178）← 不变
- `RealtimeState { workspace_root: workspace_root.clone(), … }`（:198）← rename 字段
- `AppState { codex_home: codex_home.clone(), workspace_root: workspace_root.clone(), … }`（:247）← 加 workspace_root

> main.rs:382 `replicate_team_rollouts(…, &state.codex_home, …)` 保持 codex_home（rollout 用真 CODEX_HOME）。

- [ ] **Step 3: RealtimeState 字段 rename**

`realtime.rs`（约 :106-117）`codex_home` 字段 rename 为 `workspace_root`（终端 cwd 沙箱用 workspace 根）；构造点（main.rs:198）同步；使用点（realtime.rs:318 传给 resolve_terminal_cwd）保持传 `state.workspace_root`（字段名变，值来源变 workspace_root）。

- [ ] **Step 4: 编译**

Run（`backend-rs/`）: `cargo build`
Expected: 此时调用点（files/workspace/decision/chat 用 state.codex_home）会编译失败或警告——Task 3 修复。若编译失败太多无法定位，先在本 task 把所有 `state.codex_home` 当 workspace 用的点临时改 `state.workspace_root`（见 Task 3 映射），让编译过。

> 实操：Task 2 后很可能编译不过（state.codex_home 语义变了但调用点没分流）。建议 Task 2 + Task 3 合并由一个 implementer 连续做，编译通了再 commit。下方 Task 3 给精确映射。

- [ ] **Step 5: Commit（与 Task 3 合并提交或本 task 先 commit 半成品）**

```bash
git add backend-rs/src/state.rs backend-rs/src/main.rs backend-rs/src/api/realtime.rs
```

---

### Task 3: 调用点按角色分流 + chat.rs latent bug 修复

**Files（按 Explore 映射）:**
- workspace_root 侧：`services/files.rs`、`api/files.rs`、`api/chat.rs`、`workspace/mod.rs`、`workspace/decision.rs`、`api/hooks.rs`
- codex_home 侧（保持）：`replication.rs`、`internal_rpc.rs`、`codex_status_config.rs`、`workspace/hooks_config.rs`、`snapshot.rs`、`handlers.rs` 的 rollout 调用

**分流映射（关键）:**

| 调用点 | 改成 |
|---|---|
| `services/files.rs:183,238,254` compute_workspace_roots / is_path_in_workspace / resolve_terminal_cwd 形参 `codex_home` | rename 形参 → `workspace_root` |
| `api/files.rs:236` `Some(&state.codex_home)` | `Some(&state.workspace_root)` |
| `workspace/mod.rs:27,32,37,53,62,79` personal_path/team_shared_path/team_member_path 形参 | rename → `workspace_root` |
| `handlers.rs:551,553` workspace::personal_path/team_shared_path `&state.codex_home` | `&state.workspace_root` |
| `workspace/decision.rs:65` decide_pre_tool_use 形参 | rename → `workspace_root` |
| `api/hooks.rs:132` `&state.codex_home` | `&state.workspace_root` |
| `replication.rs` 全部（find_rollout_for_thread/replicate_team_rollouts/receive_rollout/safe_join） | **保持** codex_home |
| `handlers.rs:619,779,847` find_rollout_for_thread `&state.codex_home` | **保持**（rollout） |
| `handlers.rs:631,857` replicate_team_rollouts `&state.codex_home` | **保持** |
| `internal_rpc.rs:173` receive_rollout `&state.codex_home` | **保持** |
| `codex_status_config.rs:645,666` validate_user_config_path `&state.codex_home` | **保持**（user config 在 CODEX_HOME） |
| `workspace/hooks_config.rs` write_hooks_config | **保持**（写 `<CODEX_HOME>/config.toml`） |
| `snapshot.rs:26-27` backup/restore 形参 | **保持** codex_home |

- [ ] **Step 1: chat.rs 修 latent bug**

`api/chat.rs:152-183` `ensure_upload_root` 改为读 AppState（上传根放 workspace_root，与 files/终端一致）。把 `ensure_upload_root()` 改成 `ensure_upload_root(workspace_root: &Path)`，调用方（upload_attachment handler）传 `&state.workspace_root`：

```rust
fn ensure_upload_root(workspace_root: &Path) -> Result<PathBuf, AppError> {
    let upload_root = workspace_root.join(CHAT_UPLOAD_DIR_NAME);
    tokio::fs::create_dir_all(&upload_root)
        .await
        .map_err(|e| AppError::internal(format!("create upload root: {e}")))?;
    Ok(upload_root)
}
```

`resolve_stored_upload_path`（chat.rs:191 附近）的边界校验同样用 workspace_root join webui-uploads（保持与 ensure_upload_root 一致）。调用方 `upload_attachment`（chat.rs handler）取 `&state.workspace_root` 传入。

- [ ] **Step 2: 按映射表分流所有调用点**

逐文件按上表改。grep `state.codex_home` / `codex_home` 形参，按角色分到 workspace_root 或保持 codex_home。

- [ ] **Step 3: 编译 + 全量测试**

Run（`backend-rs/`）: `cargo build && cargo test`
Expected: 零错误全绿（行为不变，仅命名分离 + chat.rs bug 修复）。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/services/files.rs backend-rs/src/api/files.rs backend-rs/src/api/chat.rs backend-rs/src/services/workspace/mod.rs backend-rs/src/services/workspace/decision.rs backend-rs/src/api/hooks.rs backend-rs/src/api/multitenant/handlers.rs
git commit -m "refactor: codex_home/workspace_root 调用点按角色分流 + 修 chat 上传根 latent bug"
```

---

### Task 4: 文档/注释同步

**Files:**
- Modify: `backend-rs/ARCHITECTURE.md`（CODEX_HOME 按角色区分）
- Modify: `config.toml.example:14`（删 $CODEX_HOME/config.toml 误导）
- Modify: 各文件注释（state.rs 字段、codex_pool.rs 注释）

- [ ] **Step 1: ARCHITECTURE.md 按角色区分 CODEX_HOME**

`ARCHITECTURE.md` 的 "CODEX_HOME" 描述：sessions/config 段说 CODEX_HOME（codex CLI），users/teams 布局段说 workspace_root。

- [ ] **Step 2: config.toml.example 删误导**

config.toml.example:14 的 "$CODEX_HOME/config.toml" webui config 查找候选删除（locate_config_file 实际只读 CODEX_WEBUI_CONFIG，避免误导）。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/ARCHITECTURE.md backend-rs/config.toml.example
git commit -m "docs: CODEX_HOME/workspace_root 按角色区分文档"
```

---

## Self-Review 结果

**1. 覆盖**：config 拆字段（Task1）+ AppState 拆（Task2）+ 调用点分流+chat bug（Task3）+ 文档（Task4）。✅
**2. 向后兼容**：workspace_root() 默认回落 codex_home()，老 config 零破坏。✅
**3. 一致性约束**：rollout/hooks_config/validate_user_config/snapshot 保持 codex_home；files/workspace/decision/chat 用 workspace_root。✅
**4. chat.rs bug**：从读 env（默认 .codex）改为读 workspace_root，与 files/终端一致。✅
**5. 测试**：cargo build + test 全绿（行为不变）；手动验证留重启后端。

## 完成后

- 重启后端验证（config.toml 不变，行为应完全不变；可选加 [workspace] enable=true 测试独立）
- 回到批次4（限流加固 + 权限回归测试）

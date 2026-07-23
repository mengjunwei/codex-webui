# 集群扩展分发设计（Cluster Extension Distribution）

- 日期：2026-07-22
- 状态：设计已确认，待实现
- 分支：feat/multitenant-platform

## 1. 背景与目标

### 1.1 问题
codex-webui 集群中每个节点运行独立的 codex app-server 进程，各自读取本地 `CODEX_HOME`。在某节点添加的 codex 扩展（技能 / 插件 / MCP）只存在于该节点本地，会话被路由或故障转移到其他节点后无法使用。现有跨节点同步只覆盖会话 rollout 和 `threads/{id}/` 工作文件，不涉及扩展本身。

### 1.2 目标
**平台级统一**：在一个节点添加扩展后，集群所有节点自动获得该扩展，任何会话落到任何节点都能使用。

### 1.3 约束（已与用户确认）
- 支持三种部署形态：同机多节点、多机、容器（均有持久卷可用）
- 扩展可能含大二进制（plugin 含编译好的 `.exe`）→ **文件内容不进数据库**
- 没有对象存储
- 阶段一为**集群级共享**（无租户隔离），未来演进多租户

## 2. 现状

### 2.1 集群架构（代码实测）
- 对等节点，无 master/worker 角色分流；共享 PostgreSQL + Redis；各节点独立本地盘
- 节点间通信：HTTP/JSON 内网 RPC（`x-internal-token` 鉴权）+ Redis pub/sub
- `CODEX_HOME` 每节点独立（默认 `~/.codex-webui/home`）
- 关键文件：`backend-rs/src/services/multitenant/{cluster,event_bus,replication,file_sync}.rs`、`backend-rs/src/api/multitenant/internal_rpc.rs`

### 2.2 codex 扩展机制（codex 0.142.5 实测）
三种扩展机制不同：

| 类型 | 存储 | 管理 | 含二进制 |
|---|---|---|---|
| 技能 skill | `$CODEX_HOME/skills/{name}/` 目录（`SKILL.md` + 子目录） | 无 CLI 命令，放目录即装 | 否 |
| MCP | `config.toml` 的 `[mcp_servers.xxx]` 段 | `codex mcp` | 否 |
| 插件 plugin | `$CODEX_HOME/plugins/`（`cache/{市场}/{plugin}/` + `.plugin-appserver/*.exe` + 启用记录在 `.codex-global-state.json`） | `codex plugin add/list/remove/marketplace`（基于 marketplace） | **是**（`.exe`） |

加载时机：codex **新会话**扫描发现（非进程级热加载）。

需排除的系统/内置内容：`skills/.system/`、`plugins/cache/openai-bundled`、`plugins/cache/openai-primary-runtime`、`plugins/.plugin-appserver`（共享运行时，随 codex 版本绑定）。

### 2.3 webui 脚手架（代码实测）
- **DB 迁移**：sea-orm-migration（Rust 代码，非 `.sql`），目录 `backend-rs/src/db/migration/`，命名 `m{YYYYMMDD}_000001_{desc}.rs`，注册于 `mod.rs`，执行于 `main.rs:57`
- **类型约定**：`VARCHAR(36)` / `BIGINT`(i64 毫秒) / `BOOLEAN` / `TEXT`，不用 JSON/ENUM/ARRAY；raw SQL `CREATE TABLE IF NOT EXISTS`；PG/MySQL 双方言
- **周期 task**：`tokio::spawn + loop{sleep;do}`，保存 `JoinHandle`，shutdown 时 abort（参照 `main.rs:415`）
- **Redis 事件总线**：`publish/subscribe`，channel 模式如 `codex:events`；订阅消费参照 `event_persist.rs`
- **配置**：`config.rs`，TOML + serde，段为独立 struct + `#[serde(default)]`，`Config` 有自定义 `Debug`
- **AppState**（`state.rs`）：持有 `db/mt_redis/codex_home/node_id/cluster` 等；EventBus 当前不在 AppState（`main.rs` 局部）
- **集群级无 team_id 表参照**：`settings`、`thread_resume_cache`
- **CodexProcessManager**（`process.rs`）：公开方法 `new/start/destroy/request/...`；**无 restart**，重启 = `destroy()` + `start()`（中断会话，重操作）

## 3. 总体架构

**权威源 = 共享 PG（扩展清单）；文件实体 = 持有节点本地盘；分发 = 节点间 RPC 下载。**

```
管理员 ──上传扩展──▶ 节点X API
  ① X 落盘到生效区(skills/ | plugins/ | config.toml)
  ② 算文件指纹 → 写 PG：清单① + 指纹② + holders③(含X)
  ③ 发 Redis "extensions:changed"

各节点(事件触发 / 周期兜底 / 启动 bootstrap):
  读 PG 清单 + 指纹②，比对本地 hash
  缺/变 → 选 alive holder → RPC /internal/ext-fetch 下载变化文件 → 写生效区
  本地有 PG 无 → 删本地生效区
  下载成功 → 写入 holders③(扩散，去单点)
生效区 = codex 真正读取处
```

**为什么不用现有主节点 push（filesync 模式）**：filesync 服务于 per-thread 单副本工作文件；扩展需要**全节点都有**。PG 清单 + 自治 pull + holder 扩散更匹配，且天然部署形态无关（只依赖集群已有的 PG + Redis + 节点间 RPC）。

## 4. 数据模型

遵循 sea-orm-migration + 类型约定 + 集群级无 team_id。

### 4.1 `cluster_extensions`（清单主表，无文件内容）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | VARCHAR(36) PK | **UUIDv7**（`Uuid::now_v7()`，写入时由 API 生成），集群唯一稳定标识 |
| `kind` | VARCHAR(32) | `skill` / `mcp` / `plugin` |
| `name` | VARCHAR(128) | 业务名 = 落盘名 |
| `display_name` | VARCHAR(256) NULL | |
| `description` | TEXT NULL | |
| `version` | VARCHAR(64) NULL | 扩展语义版本 |
| `content_form` | VARCHAR(16) | `files` / `config` |
| `config_text` | TEXT NULL | `content_form=config` 时（mcp 配置段 toml 文本） |
| `content_hash` | VARCHAR(128) | 整体校验和（快速判定要不要更新） |
| `enabled` | BOOLEAN default TRUE | |
| `created_at` / `updated_at` | BIGINT | 毫秒时间戳 |
| `created_by` | VARCHAR(36) NULL | 操作者 |

约束：`UNIQUE(kind, name)` + `INDEX(enabled)`

### 4.2 `cluster_extension_files`（文件指纹，**无 content 列**）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | BIGINT PK | 自增 |
| `extension_id` | VARCHAR(36) | → `cluster_extensions.id` |
| `rel_path` | VARCHAR(512) | 文件相对路径 |
| `size` | BIGINT | 字节数 |
| `content_hash` | VARCHAR(128) | 单文件 hash |
| `is_binary` | BOOLEAN | 是否二进制 |

约束：`UNIQUE(extension_id, rel_path)` + `INDEX(extension_id)`

### 4.3 `cluster_extension_holders`（持有节点，**去单点关键**）

| 字段 | 类型 | 说明 |
|---|---|---|
| `extension_id` | VARCHAR(36) | 复合 PK |
| `node_id` | VARCHAR(36) | 复合 PK |
| `held_since` | BIGINT | 持有时间 |

### 4.4 本地落盘
- **生效区**（codex 读取处）：
  - skill → `$CODEX_HOME/skills/{name}/`
  - mcp → 合并进 `$CODEX_HOME/config.toml` 的 `[mcp_servers.{name}]`（`toml_edit`，复用 `hooks_config.rs` 手法）
  - plugin → `$CODEX_HOME/plugins/`（产物：`cache/` + 启用记录合并进 `.codex-global-state.json`）
- **本地状态**：`$CODEX_HOME/.cluster-extensions.json`，记录 `{id → {kind, name, hash, applied_paths}}`
- 容器形态：`CODEX_HOME` 挂持久卷（PVC），扩展文件随之持久

## 5. 三种扩展的分发处理

### 5.1 技能（skill）— `content_form=files`
上传节点把 skill 目录文件写入本地 `skills/{name}/`，登记每个文件指纹。其他节点按指纹增量下载缺失/变化文件，写入 `skills/{name}/`。删除时清空该目录。

### 5.2 MCP — `content_form=config`，`config_text` 存 PG
各节点用 `toml_edit` 把 `[mcp_servers.{name}]` 段合并进本地 `config.toml`。删除时移除该段。

### 5.3 插件（plugin）— 整体产物同步
plugin 含共享二进制 + marketplace 启用记录，按「整体同步安装产物」处理：
- 上传节点：`codex plugin marketplace add` + `codex plugin add <name>` 装好（codex 自动下 `.exe` 和文件）
- 产物指纹化：扫描 `plugins/` 下该 plugin 相关产物（`cache/{市场}/{plugin}/` + `.codex-global-state.json` 中该 plugin 的启用字段），登记为文件指纹
- 其他节点：下载产物写入对应位置；启用记录合并进本地 `.codex-global-state.json` 的 plugin 段
- **排除**：`.plugin-appserver`（共享运行时，随 codex 自带）、`openai-bundled` / `primary-runtime`（内置）

## 6. 分发流程

### 6.1 同步循环（每节点一个）
spawn 周期 task（~15s，参照 `main.rs:349`），并订阅 Redis `extensions:changed`（参照 `event_persist.rs`）：
1. 读 PG：所有 `enabled` 扩展 + 指纹② + holders③
2. 对每个扩展，比对本地 `.cluster-extensions.json` + 实际文件 hash：
   - 本地无 → **新增**：从 alive holder 下载，写生效区
   - hash 变 → **更新**：只下载变化文件（靠指纹②），重写
   - 一致 → 跳过
   - 本地有 PG 无 → **删除**：清生效区
3. 下载成功 → upsert holders③（本节点加入，扩散）
4. 触发生效检查（§7）

### 6.2 下载 RPC：`/internal/ext-fetch`
新增端点（参照 `internal_rpc.rs`，`x-internal-token` 鉴权）：
- 请求：`{ext_id, rel_path, range?}`
- 响应：文件内容**流式**（支持大文件 / range 断点）
- 仅 holder 节点响应（非 holder 返回 404 或 holder 列表）

### 6.3 holder 选择
`holders③ ∩ alive 节点`（Redis `cluster:nodes`），选一个；失败轮询下一个。只要有 ≥1 个 alive holder 就能下载。

### 6.4 启动 bootstrap
节点启动时（`main.rs:328` AppState 构造后）无条件全量对齐一次，再 spawn 周期循环。容器重启友好。

## 7. 生效机制

**设计假设**：codex 新会话扫描发现扩展，**优先不重启 codex 进程**，新会话自动生效。

**兜底**：若实测发现 app-server 常驻进程内新会话不重扫（见 §13 验证项），则提供「按需重启 codex」：`CodexProcessManager.destroy()` + `start()`。重启会中断进行中会话，仅作为低峰或显式触发手段，**不自动频繁重启**。

## 8. 安装入口（单一入口原则）

**决策：所有要分发的扩展，统一通过 webui 入口安装——安装即「落盘本机 + 写入数据库 + 广播分发」一步到位。**

理由：单一入口保证数据库清单与磁盘永远一致、不会漏登；用户无需「先装再想办法入库」。

### 8.1 REST API（`/api/mt/extensions/*`，阶段一集群级）
- `POST` 添加扩展：
  - **skill**：上传打包目录 → 解压到本机 `skills/{name}/` → 指纹化 → 入库
  - **mcp**：提交配置段 → 写本机 `config.toml [mcp_servers.{name}]` → 配置存库
  - **plugin**：指定 marketplace + 插件名 → **后端在本机执行 `codex plugin marketplace add` + `codex plugin add`**（codex 自带下载 exe / 文件）→ 扫描产物指纹化 → 入库
- `GET` 列表 / `GET` 详情
- `PATCH` 启用/禁用 / `DELETE` 删除（删除时同步清理各节点生效区）
- 鉴权：复用现有 mt 鉴权 + RBAC（管理员角色）

上传成功 → 写 PG（清单 + 指纹 + holders 含本节点）+ 发 Redis `extensions:changed`。

### 8.2 手工安装的处理
直接在节点上手工操作（`codex plugin add`、手放 skill 文件夹、手改 config）**视为未托管内容**：不进数据库清单、不分发。阶段一**不提供「导入」功能**；如确需分发，删掉手工版本后改走 webui 入口重新安装。

前端管理页阶段一可后于后端（先打通 API）。

## 9. 安全与边界

- **白名单**：只写 `skills/`(用户) / `plugins/`(产物) / `config.toml[mcp]`，绝不碰 `auth.json` / `*.sqlite` / `installation_id` / `sessions/` / `.codex-global-state.json`(非 plugin 段)
- **路径校验**：`rel_path` 禁止 `..` 和绝对路径（防目录穿越）
- **大小/数量限制**：`[extensions]` 配置单扩展/单文件上限
- **下载鉴权**：`/internal/ext-fetch` 走 `x-internal-token`
- **幂等**：以 id + hash 对齐，重复下载安全
- **多租户预留**：表结构无 team_id（阶段一），未来加 `team_id` 列 + 索引即可演进

## 10. 配置 `[extensions]`

`config.rs` 新增 `ExtensionsConfig`（参照 `SnapshotConfig`）：
- `enable`: bool（总开关）
- `sync_interval_secs`: u64（默认 15）
- `max_extension_bytes` / `max_file_bytes`
- `plugin_enabled`: bool（plugin 分发较复杂，单独开关）

## 11. 可观测性
- tracing 日志：每次同步循环 / 下载 / 生效
- Prometheus 指标：每节点已同步扩展数、同步延迟、下载失败率
- 审计：扩展增删改写 `audit_logs`

## 12. 测试策略
- **单元**：指纹计算、`rel_path` 校验、toml 合并、对齐算法（增/删/改/跳）
- **集成**：双节点 PG + Redis，上传 → 另一节点同步 → 生效区文件正确
- **三形态**：同机 / 多机验证；容器形态用 docker-compose 或 k8s 验证 PVC
- **端到端（三种各装一个，必须验证）**：
  - 装一个真实 **skill** → 验证全集群各节点 `skills/{name}/` 一致、开新会话该技能可用
  - 装一个真实 **mcp** → 验证各节点 `config.toml [mcp_servers]` 段一致、codex 能连上该 MCP
  - 装一个真实 **plugin**（走 marketplace）→ 验证各节点 `codex plugin list` 显示已装、产物（含 `.exe`）齐全且可运行
- **故障**：holder 全挂时降级（不影响已同步节点）、holder 恢复后继续

## 13. 待实现时验证的点（附方法，非空 TODO）

1. **plugin 启用记录字段**：`.codex-global-state.json` 中 plugin/marketplace 启用字段的确切结构。方法：测试节点 `codex plugin marketplace add` + `codex plugin add <某 plugin>`，diff `global-state.json` 前后逆向。
2. **app-server 新会话是否重扫扩展**：方法：常驻 app-server 下运行中新增一个 skill，开新会话验证是否可用；若否，则确定需走 §7 重启兜底。
3. **plugin 同步边界**：`.plugin-appserver`（共享运行时）是否随 plugin 同步、还是各节点随 codex 自带。方法：跨节点验证。
4. **codex 版本一致性前提**：plugin 产物与 codex 版本绑定，**要求集群所有节点 codex 版本一致**（部署约束，写入运维文档）。

## 14. 未来演进
- **多租户**：`cluster_extensions` 加 `team_id`，分发按 team 隔离
- **大文件对象存储**：若 plugin 体积增长，holders 改为对象存储引用
- **plugin 细粒度**：从整体产物同步演进到逐 plugin 条目（当启用记录结构稳定后）

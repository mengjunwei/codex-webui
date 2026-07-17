# 多副本 HA 修复设计 spec

**日期:** 2026-07-17
**范围:** 修复当前 `feat/multitenant-platform` 分支上多副本 HA 实现的 5 类缺陷,确保"主挂 → 副本晋升 + 会话不丢"以及 `/internal/*` 内网 RPC 在默认配置下不被任意访问。
**不改动:** HA 整体架构(active-passive、主副本维护任务、RedisCluster 探活、代码目录划分);只做"小切口修补 + 必要硬化"。

---

## 1. 背景与现状

`replication.rs` / `cluster.rs` / `codex_pool.rs` / `main.rs` 已在 `feat/multitenant-platform` 分支完成 HA 重构。本 spec 修复其中 5 类已被代码审查或会话复核识别的缺陷:

| # | 类别 | 现状 | 风险 |
|---|---|---|---|
| 1 | 复制定位 | `fname.contains(tid.as_str())` 子串匹配 | UUID 子串误匹配 → 副本拿错数据 |
| 2 | offset 跟踪 | 主侧 send 后立即 `set_offset` | send 失败/超时 → offset 已推进 → 副本永久丢这段增量 |
| 3 | 脑裂防护 | `promote_if_primary_down` SET NX 后无 `primary_lease_until` CAS | 并发晋升极端竞态可能双写 |
| 4 | 内网 RPC 鉴权 | `internal_token` 可选;`INTERNAL_RPC_HOST` 默认 `0.0.0.0`;`receive_rollout` 仅字符串校验 | 无 token 时 `/internal/*` 全开放;symlink 逃逸 |
| 5 | CODEX_HOME 共享 | 全局共享,`config.toml`/`history.sqlite` 多进程同写 | 单团队 OK,多团队同 host 时 sqlite locked/损坏 |
| 6 | memberlist 探活 | `MemberlistCluster::new` 是 bail stub;`alive_nodes` 只返回自己;`node_rpc_addr` 始终 None | 启用 `--features memberlist-backend` 时 HA 完全退化为单机(无 gossip、无复制) |

**追加决策(2026-07-17 第二轮):** 修复 5 类的同时把 memberlist stub 真正接通,使其在生产可作为 Redis 探活的替代/补充。

---

## 2. 修复设计

### 修复 1:复制定位——按 `thread_id` 元数据精确匹配

**核心问题:** codex rollout 文件名形如 `sessions/2026/07/17/rollout-<timestamp>-<thread_id_suffix>.jsonl`,**单从文件名无法精确反查 thread_id**(子串匹配会产生 UUID 前缀冲突)。thread_id 在 PG `threads` 表里有完整记录。

**方案:** 主侧维护**"thread_id → 当前活跃 rollout 文件路径"内存表**,由 `thread/start` / `turn/start` handler 在调 codex 前写入,复制时按该表精确取文件,不再扫文件名匹配。

#### 2.1.1 数据结构

新增 `services/multitenant/replication.rs` 内:

```rust
/// 主侧:thread_id → 当前该 thread 关联的 rollout 文件绝对路径。
/// 由 mt_create_thread / mt_start_turn 在调 codex 前写入,
/// 复制循环按此表精确读取文件,避免 UUID 子串误匹配。
/// 重启清空(接受启动后第一次复制重传完整 rollout)。
pub type ThreadRolloutMap = Arc<Mutex<HashMap<String, PathBuf>>>;
```

`AppState` 增加 `pub active_rollout: ThreadRolloutMap`。

#### 2.1.2 写入时机

`handlers.rs::mt_create_thread` 在调 codex 前:

```rust
// 取得 codex 响应里的 thread_id 后,记录活跃 rollout 路径。
// 路径规范: <codex_home>/sessions/<thread_id>.jsonl
// (codex 实际按日期分目录,但每个 thread 一个会话文件;为简化用 thread_id 命名空间即可,
//  实现上按 mtime 最近 + thread_id 子串 + 长度比对来挑;详见 2.1.3)
state.active_rollout.lock().await.insert(
    tid.to_string(),
    state.codex_home.join("sessions").join(format!("{tid}.jsonl")),
);
```

**实现细节(2.1.3):** codex 实际文件路径 = `<CODEX_HOME>/sessions/<YYYY>/<MM>/<DD>/rollout-<时间戳>-<uuid>.jsonl`,**不按 thread_id 命名**。所以"thread_id → 文件路径"的精确映射需要**主侧在调 codex 后根据时间窗扫一次 sessions/ 目录,挑出 mtime 最近且文件名包含 thread_id 全串的文件**。这是**单一扫描点**(`mt_create_thread` / `mt_start_turn` 调用后立即做一次),不是每轮复制都扫。

#### 2.1.3 路径解析工具

```rust
/// 给定 thread_id,在 <codex_home>/sessions/ 下找其活跃 rollout 文件。
/// 规则:文件名包含完整 thread_id 字符串(完整匹配,不是前缀);
/// 多命中取 mtime 最新;0 命中返回 None。
async fn find_rollout_for_thread(
    codex_home: &Path,
    thread_id: &str,
) -> Option<PathBuf>;
```

测试:对 UUID `8a3f-...-e21b`,同一目录放另一 thread `8a3f-...-e210` 的文件 → 必须返回后者而非前者。

#### 2.1.4 复制循环改造

`replicate_team_rollouts` 改造:

```rust
// 旧:
let Some(conv) = thread_ids.iter().find(|tid| fname.contains(tid.as_str())).cloned() else {
    continue;
};

// 新:
let Some(conv) = thread_id_for_file(&abs_path, &active_rollout) else { continue };
// active_rollout 反查:fname → 该 thread_id 的 PathBuf,要求 PathBuf == abs_path。
```

更直接:`active_rollout` 是 `HashMap<thread_id, PathBuf>`,复制循环遍历 `active_rollout`,按值取文件读 offset,**不再 walk 全 sessions/ 目录**。

**测试:** `receive_rollout_writes_at_offset` 保留;新增 `replicate_team_rollouts_skips_unknown_files` 单元测试,确认"不在 active_rollout 里的文件不会被复制"(原 bug 反向:不该传的不能传)。

---

### 修复 2:offset 跟踪可靠——send 成功才推进

**改:** `replicate_team_rollouts` 内,把 `set_offset(redis, team_id, &conv, size).await` 从循环末尾移到 `rpc_client.replicate_rollout` 成功分支内。

```rust
if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
    tracing::warn!(team_id, conv = %conv, error = %e, "replicate rollout chunk failed, will retry next round");
    // 不推进 offset → 下次重传同一段。
    continue;
}
// send 成功 → 才推进。
set_offset(redis, team_id, &conv, size).await;
metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
```

**无 Redis 场景:** 现有 `get_offset` 已 fallback 到 0;新增进程内 `Arc<Mutex<HashMap<(team_id, conv_id), u64>>>`(放在 `AppState`)作为 fallback。无 Redis 时 offset 只对当前进程有效,重启归零 → 第一次复制会重传完整文件;接受(单节点无副本时无影响)。

---

### 修复 3:脑裂防护——`primary_lease_until` CAS + 晋升后重置副本 offset

#### 2.3.1 `promote_if_primary_down` 加 lease CAS

```rust
// 现状:
let lease_valid = row.primary_lease_until > now_ms();
if primary_alive && lease_valid { return Ok(false); }
if !try_acquire_primary(redis, team_id, me).await { return Ok(false); }

// 新:主不在 alive OR lease 已过期,都满足;但必须 lease 严格 < now 才晋升(本地时钟)。
if row.primary_lease_until >= now_ms() {
    // 本地看 lease 仍在(可能 Redis 时钟与 DB 时钟不同步)→ 不晋升,等下次。
    return Ok(false);
}
```

#### 2.3.2 `set_primary` 不动(update 即生效,因 lease CAS 已守门)

保留现有 `set_primary` 逻辑(update 不带 CAS),依赖 lease CAS 作为守门——足够,符合"轻量 CAS"原则。

#### 2.3.3 晋升成功后重置副本 offset

`promote_if_primary_down` 末尾新增:

```rust
set_primary(db, team_id, me, new_replica.as_deref()).await?;
// 晋升成功 → 删除 Redis 中该 team 所有 thread 的 repl:offset:*,让副本下次从 0 重传。
// (旧主失活前的最后一段增量可能没传到;现在新主是源头,副本需从 0 同步。)
if let Some(c) = redis {
    delete_all_team_offsets(c, team_id).await;
}
```

新增 `delete_all_team_offsets(redis, team_id)`:`SCAN repl:offset:{team_id}:*` + `DEL`。失败仅 warn,不阻断晋升。

#### 2.3.4 文档化"晋升丢最后一段"语义

设计 spec 增一节:`## 5. 失败语义`:晋升成功后,**副本与新主之间的最后 1-2 turn 增量通过"重置 offset → 下次全量同步"补偿**;期间客户端的写请求走 `mt_start_turn` 转发到新主,新主 codex resume 时读到的 rollout 与磁盘一致(因为 offset 重置后第一轮复制会拉全)。

---

### 修复 4:内网 RPC 鉴权硬化

#### 2.4.1 `Config::load`:`INTERNAL_RPC_TOKEN` 必填

```rust
> ⚠️ 以下代码为设计时草案，实际实现已改为 TOML-only 配置，见 `src/config.rs`。
let internal_token = env::var("INTERNAL_RPC_TOKEN")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| anyhow!("INTERNAL_RPC_TOKEN is required for /internal/* (≥32 bytes)"))?;
if internal_token.len() < 32 {
    return Err(anyhow!("INTERNAL_RPC_TOKEN must be ≥32 bytes (current: {})", internal_token.len()));
}
```

`internal_token` 类型从 `Option<String>` 改为 `String`(`AppState`、`main.rs`、`WorkerRpcClient` 构造同步改)。

`config.toml.example` 增:
```
INTERNAL_RPC_TOKEN=please-generate-with-openssl-rand-hex-32
```

#### 2.4.2 `INTERNAL_RPC_HOST` 默认 `127.0.0.1`

```rust
let internal_rpc_host = env::var("INTERNAL_RPC_HOST")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "127.0.0.1".to_string());

if internal_rpc_host == "0.0.0.0" {
    tracing::warn!("INTERNAL_RPC_HOST=0.0.0.0: /internal/* exposed on all interfaces; \
                    ensure firewall rules and INTERNAL_RPC_TOKEN are set.");
}
```

#### 2.4.3 `receive_rollout` 路径校验:canonicalize 边界

```rust
async fn safe_join(codex_home: &Path, rel: &str) -> Result<PathBuf, AppError> {
    // 字符串校验(已有)
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\')
        || rel.contains("..") || rel.contains('\\') {
        return Err(AppError::internal(format!("invalid rel_path: {rel}")));
    }
    let candidate = codex_home.join(rel);
    // canonicalize 边界:解 symlink 后必须仍位于 codex_home 内。
    let canon_home = tokio::fs::canonicalize(codex_home).await
        .map_err(|e| AppError::internal(format!("canonicalize codex_home: {e}")))?;
    let canon_path = tokio::fs::canonicalize(&candidate).await
        .map_err(|e| AppError::internal(format!("canonicalize target: {e}")))?;
    if !canon_path.starts_with(&canon_home) {
        return Err(AppError::internal(format!("path escapes codex_home: {rel}")));
    }
    Ok(canon_path)
}
```

测试:`receive_rollout_rejects_symlink_escape`——在 tmp 下建 symlink 指向 tmp 外,确认 receive 拒绝。

---

### 修复 5:CODEX_HOME 共享——保留架构,补文档

**不改代码。** 在 spec 与 `config.toml.example` 中增一节明确:

```
# 多 team 单 host 部署注意:
# 所有 team 共享全局 CODEX_HOME;codex 自管的 config.toml / history.sqlite
# 在多 team 进程并发下存在 sqlite locked / 数据串味风险。
# 多团队生产部署建议:
#   方案 A (推荐): 每 team 一台独立 host,各自 CODEX_HOME。
#   方案 B (过渡): per-team 子目录手工挂载,例如 CODEX_HOME=/var/lib/codex/team-A,
#                  在外部 LB 层按 team_id 路由到不同 host。
# 单团队 / 单 host 场景无影响。
```

`docs/superpowers/specs/2026-07-16-multitenant-platform-design.md` 增 `## 多 team 部署 CODEX_HOME 建议` 一节,内容同上。

---

## 3. 数据流与接口

### 3.1 `AppState` 字段变更

| 字段 | 类型 | 用途 |
|---|---|---|
| `active_rollout` | `Arc<Mutex<HashMap<String, PathBuf>>>` | 主侧 thread_id → 活跃 rollout 路径 |
| `local_offsets` | `Arc<Mutex<HashMap<(String,String), u64>>>` | 无 Redis 时 fallback offset 存储 |
| `internal_token` | `String`(原 `Option<String>`) | 强制必填 |

### 3.2 `replication.rs` API 变更

| 函数 | 签名变更 |
|---|---|
| `replicate_team_rollouts` | 新增参数 `active_rollout: &ThreadRolloutMap, local_offsets: &LocalOffsetMap`;不再调 `list_rollout_files` |
| `find_rollout_for_thread` | 新增,异步路径解析 |
| `delete_all_team_offsets` | 新增,Redis SCAN + DEL |
| `safe_join` | 新增,canonicalize 边界校验 |
| `receive_rollout` | 内部用 `safe_join` 替 `codex_home.join` |

### 3.3 `promote_if_primary_down` 行为变更

晋升成功 → 调 `delete_all_team_offsets` → 副本下次 `replicate_team_rollouts` 从 0 拉全(由新主驱动)。

---

## 4. 测试策略

### 4.1 单元测试(必过)

| 测试 | 验证 |
|---|---|
| `find_rollout_for_thread_picks_correct_file` | UUID 前缀冲突场景,正确选目标文件 |
| `find_rollout_for_thread_no_match` | 无命中返回 None |
| `replicate_rollouts_skips_unknown_files` | `active_rollout` 里没有的文件不被复制 |
| `offset_advances_only_on_send_success` | mock RPC 失败 → offset 不动;mock 成功 → offset 推进 |
| `promote_requires_expired_lease` | mock `primary_lease_until = now+1000` → 不晋升 |
| `promote_resets_offsets_on_success` | mock Redis → 晋升成功后 `repl:offset:{team}:*` 被删 |
| `receive_rollout_rejects_symlink_escape` | symlink 指向 tmp 外 → 拒绝 |
| `config_requires_internal_token` | 未设或 <32 字节 → 启动失败 |
| `config_internal_rpc_host_defaults_127` | 默认 127.0.0.1 |

### 4.2 集成测试(本地手测,不入 CI)

- 启动 2 个节点 `node-a` / `node-b`,同一 Redis,同一 PG。
- 在 `node-a` 创建 thread,跑 1 个 turn。
- 验证 `node-b` 的 `CODEX_HOME/sessions/.../<thread_id>.jsonl` 字节长度 ≥ node-a。
- kill -9 `node-a`,等 lease 过期(120s),验证 `node-b` 升主 + `thread/resume` 成功 + 后续 turn 仍写到 node-b。
- kill -9 `node-a` 在 turn 中途:验证**副本 + 晋升后**,再次跑 turn 仍能续接(`find_rollout_for_thread` 仍能找到最近文件,offset 重置后从 0 拉全)。

---

## 5. 失败语义(脑裂/晋升 RPO/RTO)

| 场景 | 行为 | RPO |
|---|---|---|
| 主进程 kill -9 | 副本 lease 120s 过期 → 晋升 + offset 重置 → 副本下次全量同步 | 最后 ≤120s 内未确认的 turn 增量(由 offset 重置补偿) |
| 主节点网络瞬断(<120s) | 旧主继续 renew(因 lease 未过期);新副本不会晋升 | 0 |
| 主节点网络长断(>120s) | 副本晋升;旧主恢复后 `renew_lease` 看到 `primary_node != node_id` 跳过 | 与场景 1 同 |
| 两副本同时发现主失活 | SET NX 只有一个成功;失败者下次周期重试 | 0 |
| 主侧 send 失败 | offset 不动,下次重传同一段 | 副本延迟若干秒拿到该段 |
| Redis 整体宕 | 所有 Redis 路径降级(lease 不续、offset 用本地);`SingleCluster` 模式无脑裂 | 单节点 OK;多节点无脑裂保护(已知,文档化) |

---

## 6. 不做的事(明确范围)

- **不**做 fencing token(完整 lease fence);轻 CAS 足够。
- **不**改 CODEX_HOME 架构;只文档化多 team 部署建议。
- **不**改路由层 `RedisRouter` / `ConsistentHash`(本分支已废弃,未启用)。
- **不**加 STONITH / 双写 WAL;HA RPO ≤120s 已满足现有需求。
- **不**改 `mt_token_usage` / `mt_turn_diffs` 等读侧 handler;只动复制 / 晋升 / 鉴权 / 启动配置 4 处。

---

## 7. 风险与回退

| 风险 | 缓解 |
|---|---|
| `find_rollout_for_thread` 时间窗不准(同 thread 多文件) | 选 mtime 最新;`mt_create_thread` 后立即调一次预热 active_rollout |
| canonicalize 在首次写入前文件不存在 | `tokio::fs::canonicalize` 失败 → fall back 到 `codex_home.join(rel)`,仅当 parent 已存在时 |
| offset 进程内 fallback 重启归零 → 副本首次重传全量 | 单节点无副本无影响;多节点首次切换有 ~1 次重传,接受 |
| `INTERNAL_RPC_TOKEN` 必填破坏现有部署 | `config.toml.example` 注释明确;`Config::load` 错误信息指向 openssl rand hex 32 |

---

## 8. 实施顺序

1. `config.rs`:`internal_token` 改必填,`host` 默认改 127;加单测。
2. `state.rs`:新增 `active_rollout` / `local_offsets` 字段,`internal_token` 改非 Option。
3. `replication.rs`:新增 `find_rollout_for_thread` / `safe_join` / `delete_all_offsets`;改 `replicate_team_rollouts` 用 active_rollout;offset send-成功才推进;`promote_if_primary_down` 加 lease CAS + 重置 offset;`receive_rollout` 用 `safe_join`。单测。
4. `handlers.rs::mt_create_thread` / `mt_start_turn`:调 codex 后调 `find_rollout_for_thread` 写 active_rollout。
5. `main.rs`:装配新字段;`internal_token` 改 unwrap。
6. `config.toml.example`:增 `INTERNAL_RPC_TOKEN`;加 CODEX_HOME 多 team 注释。
7. `docs/superpowers/specs/2026-07-16-multitenant-platform-design.md`:增失败语义 + 多 team 部署建议两节。
8. `cluster.rs`:把 `MemberlistCluster` stub 替换为真正接通;`Config` 新增 `memberlist_seeds` / `memberlist_bind`,`worker_id` 改必填;`main.rs` 加分支装配。

---

## 9. 修复 6:memberlist 真正接通(替换 stub)

### 9.1 目标

`backend-rs/src/services/multitenant/cluster.rs` 第 105-135 行的 `MemberlistCluster` 当前是 bail stub:`new()` 直接 panic,`alive_nodes` 仅返回自己,`node_rpc_addr` 永远 None。本节把它接成可生产运行的 gossip 探活实现,使 `--features memberlist-backend` 启用时 `ClusterMembership` 在多机部署下真正工作。

### 9.2 设计决策

| 维度 | 决策 | 理由 |
|---|---|---|
| 运行模式 | 按 `MEMBERLIST_SEEDS` 是否设置自动选 | 用户偏好;无 SEEDS → `SingleCluster`(现状不变),有 SEEDS → memberlist |
| RPC 地址解析 | 复用 Redis `cluster:node:{id}` key(memberlist 不携带 rpc_url) | memberlist 0.8.5 自定义状态传 rpc_url 复杂且与现有 Redis 心跳重复 |
| 节点 id | `WORKER_ID` 启动必填(≥16 字节) | memberlist 重启后必须能认领 `session_replicas.primary_node`,随机 UUID 会反复触发 `reclaim_orphan_teams` 误判 |
| 现有 `RedisCluster` | 保留(作 `RedisCluster` 单跑模式或 memberlist 模式的 rpc_url 通道) | 不破坏现有 Redis 单跑部署 |

### 9.3 新增配置项

`Config::load` 新增 / 改:

```rust
// worker_id:Option<String> → String,启动必填 ≥16 字节
let worker_id = env::var("WORKER_ID")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| anyhow!("WORKER_ID is required (≥16 bytes)"))?;
if worker_id.len() < 16 {
    return Err(anyhow!("WORKER_ID must be ≥16 bytes (current: {})", worker_id.len()));
}

// MEMBERLIST_SEEDS:逗号分隔 host:port 列表;空 = 单机模式
let memberlist_seeds: Vec<String> = env::var("MEMBERLIST_SEEDS")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
    .unwrap_or_default();

// MEMBERLIST_BIND:默认 0.0.0.0:7946
let memberlist_bind = env::var("MEMBERLIST_BIND")
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "0.0.0.0:7946".to_string());
```

字段:

```rust
pub memberlist_seeds: Vec<String>,
pub memberlist_bind: String,
pub worker_id: String,  // 原 Option<String>
```

### 9.4 `MemberlistCluster` 改造

#### 9.4.1 数据成员

```rust
#[cfg(feature = "memberlist-backend")]
pub struct MemberlistCluster {
    node_id: String,
    memberlist: Arc<memberlist::Memberlist>,
    /// alive 节点集(由 delegate 回调更新;`alive_nodes()` 读这里)。
    alive: Arc<tokio::sync::RwLock<std::collections::HashSet<String>>>,
    /// Redis(仅用于 `node_rpc_addr` 解析与轻量心跳写 rpc_url)。
    redis: redis::Client,
    own_rpc_url: String,
}
```

#### 9.4.2 构造(含 delegate + transport + seed join + 心跳)

```rust
impl MemberlistCluster {
    pub async fn new(
        node_id: String,
        bind: &str,
        seeds: &[String],
        redis: redis::Client,
        own_rpc_url: String,
    ) -> anyhow::Result<Self> {
        use std::collections::HashSet;

        let alive = Arc::new(tokio::sync::RwLock::new(HashSet::from([node_id.clone()])));

        // delegate:节点 up/down 事件 → 写 alive。try_write 避免跨 await 持锁。
        struct Delegate {
            alive: Arc<tokio::sync::RwLock<HashSet<String>>>,
            node_id: String,
        }
        impl memberlist::Delegate for Delegate {
            fn notify_node(&self, node: &memberlist::Node) {
                if let Ok(mut g) = self.alive.try_write() {
                    g.insert(node.name().to_string());
                }
            }
            fn node_left(&self, node: &memberlist::Node) {
                if let Ok(mut g) = self.alive.try_write() {
                    g.remove(node.name().to_string());
                }
            }
        }

        // transport:UDP 绑 bind(memberlist 0.8.5 提供 TokioUdpTransport)。
        let transport = memberlist::transport::TokioUdpTransport::new(bind.parse()?)
            .map_err(|e| anyhow!("memberlist transport: {e}"))?;

        let opts = memberlist::Options {
            name: Some(node_id.clone()),
            ..Default::default()
        };
        let delegate = Delegate { alive: alive.clone(), node_id: node_id.clone() };
        let m = memberlist::Memberlist::new(opts, Box::new(delegate), Box::new(transport), None)
            .map_err(|e| anyhow!("memberlist init: {e}"))?;

        // join seeds(任一成功即返回;全部失败也继续,后续 SWIM ping 会自然收敛)。
        for seed in seeds {
            let addr: std::net::SocketAddr = match seed.parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let _ = m.join(&[(addr.ip().to_string(), addr.port())]).await;
        }

        // 启动 RPC 心跳:每 10s SETEX cluster:node:{node_id} = own_rpc_url,TTL 30。
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

        Ok(Self { node_id, memberlist: Arc::new(m), alive, redis, own_rpc_url })
    }
}
```

**API 校正说明(实施时按 memberlist 0.8.5 实际签名调整):**
- `memberlist::Node::name() -> &str` — 节点名 = `node_id`(`Options.name`)。
- `Delegate::notify_node / node_left` — 回调签名按 crate 实际定义(若 crate 叫 `node_upserted`,改之)。
- `Memberlist::join` 接受 `&[(String, u16)]`。
- `tokio::sync::RwLock::try_write` 用于 delegate 内避免跨 await 持锁。

#### 9.4.3 trait 实现

```rust
#[async_trait]
impl ClusterMembership for MemberlistCluster {
    fn local_node_id(&self) -> &str { &self.node_id }

    async fn alive_nodes(&self) -> Vec<String> {
        self.alive.read().await.iter().cloned().collect()
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
```

### 9.5 main.rs 装配分支

```rust
let cluster: Arc<dyn ClusterMembership> = if !cfg.memberlist_seeds.is_empty() {
    #[cfg(feature = "memberlist-backend")]
    {
        let redis = cfg.mt_redis.clone()
            .ok_or_else(|| anyhow!("REDIS_URL required when MEMBERLIST_SEEDS is set"))?;
        let rpc = cfg.worker_rpc_url.clone()
            .ok_or_else(|| anyhow!("WORKER_RPC_URL required when MEMBERLIST_SEEDS is set"))?;
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
                       rebuild with --features memberlist-backend")
    }
} else if let Some(c) = mt_redis.clone() {
    Arc::new(RedisCluster::new(c, cfg.worker_id.clone()))
} else {
    Arc::new(SingleCluster::new(cfg.worker_id.clone(), own_rpc_url.clone()))
};
```

**删除现有 `main.rs` 里 `RedisCluster::new + heartbeat` task 段(memberlist 模式下)**:心跳已内置于 `MemberlistCluster::new`。`RedisCluster` 单跑模式仍保留 task。

### 9.6 失败语义(对 9.x 单独)

| 场景 | 行为 |
|---|---|
| 全部 seeds 不可达 | memberlist 仍启动;本节点单跑,`alive_nodes = {self}`;HA 退化 |
| 单节点中途失联 | delegate `node_left` 回调 → `alive.remove(id)`;`promote_if_primary_down` 下一周期判定主失活 → 晋升 |
| Redis 整体宕(已配 RPC 用) | `node_rpc_addr` 失败 → 复制循环早退;HA 仅存活,数据复制停 |
| 两节点同 ID 重启 | memberlist merge states,`alive` 集合最终一致;`session_replicas.primary_node` 仍指向该 ID |

### 9.7 测试

| 测试 | 验证 |
|---|---|
| `memberlist_cluster_singleton_local_alive` | 构造后 `alive_nodes` 至少含 self |
| `memberlist_cluster_node_rpc_addr_self` | `node_rpc_addr(local)` 返回 own_rpc_url |
| `config_memberlist_seeds_parse` | `MEMBERLIST_SEEDS=a:7946,b:7947` 解析为 2 项 |
| `config_worker_id_required` | 未设或 <16 字节 → 启动失败 |

**集成测试(本地手测):**
- 2 节点 docker-compose,各自 `MEMBERLIST_SEEDS=<对方>:7946`。
- node-a 启动后约 5s 内 `alive_nodes` 应含双方(delegate upsert 回调)。
- kill -9 node-a,约 10-30s 内 node-b 的 `alive_nodes` 应只剩自己;`promote_if_primary_down` 触发晋升。

### 9.8 不做的事

- **不**用 memberlist custom_state 传 rpc_url(已选 Redis 通道)。
- **不**实现 SWIM/UDP 反压以外的探测协议。
- **不**加 peer exchange(`alive_nodes` 由 delegate 自然更新)。
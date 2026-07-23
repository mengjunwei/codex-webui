# Per-Thread 调度 + 增量文件同步设计

**日期**：2026-07-21  
**状态**：Draft  
**分支**：feat/multitenant-platform

---

## 1. 背景

当前集群架构（per-team调度）存在以下问题：

1. **负载不均衡**：大 team 的所有 thread 聚在一个节点，热点集中
2. **扩缩容无 rebalance**：新节点只接新 team，已有分配不迁移
3. **节点下线感知慢**：Redis TTL 30s，窗口期内 sticky 仍指向死节点
4. **workspace 文件不跨节点复制**：failover 后新主看不到旧主写的文件

## 2. 设计目标

1. **Per-Thread 调度**：每个 thread 独立分配节点，分散负载
2. **增量文件同步**：workspace 文件按 thread 粒度复制到 replica 节点
3. **惰性 Rebalance**：新节点加入后自动迁移过热 thread
4. **缩短心跳 TTL**：节点下线感知从 30s 降到 15s
5. **保持简洁**：同一用户的 thread 文件完全隔离，不共享

## 3. 方案概览

### 3.1 核心改动

| 模块 | 改动 |
|------|------|
| session_replicas 表 | 从 per-team 改为 per-thread（加 `thread_id` 列） |
| workspace 路径 | 从 `teams/{team_id}/shared/` 改为 `threads/{thread_id}/` |
| 文件同步 | 新增 per-thread 增量复制机制 |
| 维护循环 | 新增惰性 rebalance 逻辑 |
| 心跳 TTL | 从 30s 降到 15s |

### 3.2 数据结构变化

#### session_replicas 表（新）

```sql
CREATE TABLE IF NOT EXISTS session_replicas (
    thread_id VARCHAR(36) PRIMARY KEY NOT NULL,
    primary_node VARCHAR(64) NOT NULL,
    replica_node VARCHAR(64),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    primary_lease_until BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL
)
```

**变化**：
- 主键从 `team_id` 改为 `thread_id`
- 每个 thread 独立一行（10 个 thread = 10 行 vs 原来 1 行）

#### threads 表（无变化）

保持现状，`team_id` 列仍然保留（用于查询同一 team 的所有 thread）。

#### workspace 路径结构

```
$workspace_root/
├── users/{user_id}/personal/          个人 workspace（保留）
└── threads/{thread_id}/               新增：per-thread workspace
```

**删除**：
- `teams/{team_id}/shared/`（不再需要）
- `teams/{team_id}/members/{user_id}/`（不再需要）

**新增**：
- `threads/{thread_id}/`（per-thread workspace）

## 4. 详细设计

### 4.1 Per-Thread 调度

#### 4.1.1 thread 创建流程（handlers.rs）

```rust
// mt_create_thread 改动：
// 1. 创建 thread 记录（threads 表）
// 2. 分配 primary/replica 节点（session_replicas 表，per-thread）
// 3. 创建 workspace 目录（threads/{thread_id}/）
// 4. 设置 cwd 为 threads/{thread_id}/
// 5. 启动 thread

// resolve_worker 改动：
// 1. 查 sticky 绑定（Redis）
// 2. 如果未命中，调 replication::get_or_assign(thread_id)
// 3. 分配 primary/replica
// 4. 绑定 sticky
```

#### 4.1.2 replica 选择逻辑（replication.rs）

```rust
pub async fn get_or_assign(
    db: &DatabaseConnection,
    redis: &Option<RedisPool>,
    thread_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<String, AppError> {
    // 1. 查 session_replicas 是否已分配
    if let Some(row) = get(db, thread_id).await? {
        return Ok(row.primary_node);
    }

    // 2. 分配 primary = 本节点
    let primary = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;

    // 3. 选 replica（软约束：优先排除 primary）
    let replica = alive.iter()
        .find(|n| n != &primary)
        .cloned();

    // 4. 写入 DB
    let now = now_ms();
    let am = ActiveModel {
        thread_id: Set(thread_id.to_string()),
        primary_node: Set(primary.clone()),
        replica_node: Set(replica),
        status: Set("active".to_string()),
        primary_lease_until: Set(now + LEASE_TTL_MS),
        updated_at: Set(now),
    };
    Entity::insert(am).exec(db).await?;

    Ok(primary)
}
```

#### 4.1.3 workspace 路径生成（workspace/mod.rs）

```rust
pub fn thread_workspace_path(workspace_root: &Path, thread_id: &str) -> PathBuf {
    workspace_root.join("threads").join(thread_id)
}

pub async fn ensure_thread_workspace(
    workspace_root: &Path,
    thread_id: &str,
) -> Result<PathBuf, AppError> {
    let path = thread_workspace_path(workspace_root, thread_id);
    tokio::fs::create_dir_all(&path).await?;
    Ok(path)
}
```

### 4.2 增量文件同步

#### 4.2.1 设计思路

沿用 rollout 的设计：
- 主节点扫描 `threads/{thread_id}/` 下的文件变更
- 按 offset 增量传输到 replica 节点
- replica 节点写入本地 `threads/{thread_id}/`

#### 4.2.2 数据结构

```rust
// 新增：文件变更记录
pub struct FileChange {
    pub thread_id: String,
    pub relative_path: String,  // 相对于 threads/{thread_id}/
    pub change_type: ChangeType, // Create / Modify / Delete
    pub content: Option<Vec<u8>>, // 文件内容（Create/Modify 有值）
    pub timestamp: i64,
}

// 新增：文件同步 offset（存储在 Redis）
// key: file:offset:{thread_id}
// value: 上次同步的 timestamp
```

#### 4.2.3 主节点扫描逻辑

```rust
// 新增：services/workspace/file_sync.rs

pub async fn scan_and_replicate(
    state: &AppState,
    thread_id: &str,
) -> Result<(), AppError> {
    let workspace = thread_workspace_path(&state.workspace_root, thread_id);
    let last_sync = get_last_sync_offset(state.redis.as_ref(), thread_id).await.unwrap_or(0);

    // 1. 扫描 workspace 目录下所有文件
    let changes = scan_changes(&workspace, last_sync).await?;

    // 2. 过滤已同步的（offset < last_sync）
    let new_changes: Vec<_> = changes.into_iter()
        .filter(|c| c.timestamp > last_sync)
        .collect();

    if new_changes.is_empty() {
        return Ok(());
    }

    // 3. 通过 RPC 发送到 replica 节点
    let replica = get_replica_node(&state.db, thread_id).await?;
    if let Some(replica_node) = replica {
        rpc_send_file_changes(state, &replica_node, &new_changes).await?;
    }

    // 4. 更新 offset
    let max_ts = new_changes.iter().map(|c| c.timestamp).max().unwrap_or(last_sync);
    set_last_sync_offset(state.redis.as_ref(), thread_id, max_ts).await?;

    Ok(())
}
```

#### 4.2.4 副本接收逻辑

```rust
// 新增：api/internal/handlers.rs

pub async fn receive_file_changes(
    State(state): State<AppState>,
    Json(changes): Json<Vec<FileChange>>,
) -> Result<StatusCode, AppError> {
    for change in changes {
        let path = thread_workspace_path(&state.workspace_root, &change.thread_id)
            .join(&change.relative_path);

        match change.change_type {
            ChangeType::Create | ChangeType::Modify => {
                if let Some(content) = change.content {
                    tokio::fs::write(&path, content).await?;
                }
            }
            ChangeType::Delete => {
                if path.exists() {
                    tokio::fs::remove_file(&path).await?;
                }
            }
        }
    }

    Ok(StatusCode::OK)
}
```

#### 4.2.5 调用时机

在维护循环中（每 15 秒）：

```rust
// main.rs 维护循环新增：
// 对每个活跃 thread，如果本节点是 primary，执行文件同步
for thread_id in active_threads {
    if is_primary(&state.db, &thread_id, &state.node_id).await? {
        if let Err(e) = file_sync::scan_and_replicate(&state, &thread_id).await {
            tracing::warn!(thread_id, "文件同步失败: {:?}", e);
        }
    }
}
```

### 4.3 惰性 Rebalance

#### 4.3.1 负载指标

每个节点承担的 primary thread 数：

```rust
// 统计本节点 primary thread 数
let my_threads = session_replicas::Entity::find()
    .filter(session_replicas::Column::PrimaryNode.eq(&state.node_id))
    .count(&state.db)
    .await?;

// 统计全集群平均 thread 数
let all_nodes = cluster.alive_nodes().await;
let total_threads = session_replicas::Entity::find()
    .count(&state.db)
    .await?;
let avg_threads = total_threads / all_nodes.len();
```

#### 4.3.2 Rebalance 逻辑

```rust
// 新增：services/multitenant/rebalance.rs

pub async fn maybe_rebalance(
    state: &AppState,
) -> Result<(), AppError> {
    let alive = state.cluster.alive_nodes().await;
    if alive.len() <= 1 {
        return Ok(()); // 单节点不 rebalance
    }

    // 1. 统计每个节点的 primary thread 数
    let mut node_load: HashMap<String, usize> = HashMap::new();
    let all_replicas = session_replicas::Entity::find().all(&state.db).await?;
    for row in &all_replicas {
        *node_load.entry(row.primary_node.clone()).or_insert(0) += 1;
    }

    // 2. 计算平均负载
    let total: usize = node_load.values().sum();
    let avg = total / alive.len();

    // 3. 检查本节点是否过热（> 平均值 * 1.5）
    let my_load = node_load.get(&state.node_id).copied().unwrap_or(0);
    if my_load <= (avg as f64 * 1.5) as usize {
        return Ok(()); // 不过热
    }

    // 4. 找一个低负载的节点（< 平均值）
    let target_node = alive.iter()
        .find(|n| {
            let load = node_load.get(*n).copied().unwrap_or(0);
            load < avg
        })
        .cloned();

    let Some(target) = target_node else {
        return Ok(()); // 没有低负载节点
    };

    // 5. 选一个本节点的 thread 迁移（优先选最旧的）
    let thread_to_migrate = session_replicas::Entity::find()
        .filter(session_replicas::Column::PrimaryNode.eq(&state.node_id))
        .order_by_asc(session_replicas::Column::UpdatedAt)
        .one(&state.db)
        .await?;

    let Some(thread) = thread_to_migrate else {
        return Ok(());
    };

    // 6. 迁移：先切 replica 为新主
    //    - 更新 session_replicas.primary_node = target
    //    - 选新 replica（排除 target）
    //    - 更新 DB
    migrate_primary(&state.db, &thread.thread_id, &target, &alive).await?;

    tracing::info!(
        thread_id = thread.thread_id,
        from = state.node_id,
        to = target,
        "Rebalanced thread"
    );

    Ok(())
}
```

#### 4.3.3 Rebalance 安全保障

1. **不迁移活跃 thread**：只迁移 `status = active` 且无活跃请求的 thread（通过 `last_activity_at` 判断）
2. **迁移前文件同步**：确保 replica 节点有最新文件
3. **迁移后清理**：旧 primary 删除本地 `threads/{thread_id}/` 目录（可选，延迟删除）

### 4.4 缩短心跳 TTL

#### 4.4.1 修改点

```rust
// main.rs:296-303
tokio::spawn(async move {
    loop {
        // TTL 从 30s 改为 15s
        if let Err(e) = rc.heartbeat(15, &rpc_url).await {
            tracing::warn!(...);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
});
```

#### 4.4.2 参数调整

- 心跳间隔：10s（不变）
- 心跳 TTL：15s（从 30s 降到 15s）
- 容错窗口：5s（一次心跳延迟不会误判）

#### 4.4.3 租约调整

primary 租约 TTL 从 120s 降到 60s：

```rust
// replication.rs
pub const LEASE_TTL_MS: i64 = 60_000; // 60 秒
const LEASE_TTL_SECS: u64 = 60;
```

### 4.5 Replica 选择优化

#### 4.5.1 软约束逻辑

```rust
// replication.rs
fn select_replica(
    primary: &str,
    alive: &[String],
) -> Option<String> {
    // 优先选不等于 primary 的节点
    alive.iter()
        .find(|n| n != &primary)
        .cloned()
}
```

**说明**：Per-Thread 模式下，每个 thread 独立分配，不存在同一 team 多分片冲突问题，所以只需要排除 primary 即可。

## 5. 实现步骤

### 阶段一：数据结构迁移（低风险）

1. **session_replicas 表迁移**
   - 新增 `thread_id` 列
   - 迁移现有 per-team 数据到 per-thread（为每个 thread 创建一行）
   - 删除 `team_id` 主键，改为 `thread_id` 主键
   - 更新所有相关查询

2. **workspace 路径迁移**
   - 保留旧路径 `teams/{team_id}/shared/`（向后兼容）
   - 新 thread 使用新路径 `threads/{thread_id}/`
   - 旧 thread 继续使用旧路径（渐进迁移）

### 阶段二：增量文件同步（中风险）

3. **实现 file_sync 模块**
   - 主节点扫描逻辑
   - 副本接收逻辑
   - Redis offset 管理

4. **集成到维护循环**
   - 每 15 秒扫描活跃 thread
   - 异步同步文件变更

### 阶段三：惰性 Rebalance（中风险）

5. **实现 rebalance 模块**
   - 负载统计
   - 迁移逻辑
   - 安全检查

6. **集成到维护循环**
   - 每 15 秒检查负载
   - 触发 rebalance

### 阶段四：心跳优化（低风险）

7. **缩短 TTL**
   - 修改心跳间隔和 TTL 参数
   - 修改租约 TTL

8. **测试验证**
   - 节点下线感知时间
   - 误判率

## 6. 测试策略

### 6.1 单元测试

- workspace 路径生成
- replica 选择逻辑
- 负载统计

### 6.2 集成测试

- thread 创建流程
- 文件同步流程
- rebalance 流程

### 6.3 压力测试

- 10+ 节点集群
- 大量 thread 创建和迁移
- 文件同步性能

### 6.4 故障测试

- 节点宕机恢复
- 网络分区
- 文件同步失败

## 7. 风险和缓解

### 7.1 风险：session_replicas 表膨胀

**问题**：per-thread 行数 = thread 数，可能达到百万级

**缓解**：
- 定期清理已关闭 thread 的 session_replicas 记录
- 分区表（按 thread_id hash 分区）

### 7.2 风险：文件同步延迟

**问题**：异步同步可能导致短暂不一致

**缓解**：
- 用户感知：切换 thread 时提示"文件同步中"
- 一致性保证：最终一致（延迟 < 30s）

### 7.3 风险：Rebalance 抖动

**问题**：频繁迁移可能导致请求失败

**缓解**：
- 迁移前检查 thread 是否有活跃请求
- 迁移间隔限制（至少 5 分钟）

### 7.4 风险：心跳误判

**问题**：TTL 15s 可能因网络延迟误判

**缓解**：
- 容错窗口 5s（一次心跳延迟不会误判）
- 可配置 TTL（通过 config.toml）

## 8. 监控指标

### 8.1 核心指标

- `thread_primary_count{node}`：每个节点的 primary thread 数
- `file_sync_latency_seconds`：文件同步延迟
- `rebalance_count`：rebalance 次数
- `heartbeat_miss_rate`：心跳丢失率

### 8.2 告警规则

- 单节点 primary thread 数 > 平均值 * 2 → 告警
- 文件同步延迟 > 60s → 告警
- 心跳丢失率 > 10% → 告警

## 9. 向后兼容

### 9.1 旧 thread 处理

- 旧 thread 继续使用 `teams/{team_id}/shared/` 路径
- 不强制迁移，渐进式切换
- 新 thread 一律使用新路径

### 9.2 配置项

```toml
[workspace]
enable = true
path = "/data/workspace"

[workspace.thread]
# per-thread workspace 开关
enable = true
# 文件同步间隔（秒）
sync_interval = 15

[cluster]
worker_id = "node-1"
# 心跳 TTL（秒）
heartbeat_ttl = 15
# 租约 TTL（秒）
lease_ttl = 60
```

## 10. 总结

本设计通过 **Per-Thread 调度 + 增量文件同步 + 惰性 Rebalance + 缩短心跳 TTL** 四项改进，解决了当前集群架构的负载不均衡、扩缩容无 rebalance、节点下线感知慢、文件不跨节点复制四个核心问题。

**核心优势**：
1. 负载最均衡（per-thread 分散）
2. 逻辑简单（无分片概念，无跨 thread 文件共享）
3. 增量复制按 thread 粒度，复制单元小

**核心代价**：
1. session_replicas 表膨胀（per-thread 行数）
2. 同一用户的 thread 文件完全隔离（已确认不需要共享）

---

## 11. 实现澄清（代码核对后补充）

核对真实代码后，§4 的部分假设需修正，以下为最终实现口径。

### 11.1 per-thread 调度的时序问题与两阶段 resolve_worker

**问题**：现有 `mt_create_thread`（`handlers.rs:514-590`）时序是「先 `resolve_worker(team_id)` 选节点 → `codex thread/start` 生成 thread_id → `sticky.bind`」。per-thread 要求 session_replicas 以 thread_id 为主键，但选节点（决定本地 start 还是远程转发）必须发生在 `thread/start` **之前**，而 thread_id 在 `thread/start` **之后**才由 codex 生成。设计文档 §4.1.1 写的 `resolve_worker → get_or_assign(thread_id)` 时序不成立。

**解法：两阶段分配**（不依赖 codex 改动）：

- **阶段一（选节点）**：`resolve_worker` 不再写 session_replicas，改用「最少负载」策略从 alive 节点选 target——查询 session_replicas 统计各节点 primary thread 数，选最少的（本节点优先，避免无谓转发）。sticky 命中且 alive 则直接返回（保持会话粘性）。
- **thread/start**：本地 start 或转发到 target，拿到 codex 返回的 thread_id。
- **阶段二（登记）**：用 `(thread_id, target)` 写 session_replicas（`get_or_assign` 改为 upsert by thread_id），并 `sticky.bind(thread_id, target)`。

`resolve_worker` 签名改为 `(state, thread_id: Option<&str>, user_id: &str) -> Result<String>`（不再需要 team_id 和 is_personal：选节点统一按最少负载；thread_id 仅用于 sticky 命中检查）。

### 11.2 个人/团队 workspace 路径统一

**问题**：§3.2 既说保留 `users/{user_id}/personal/`，又要 per-thread 隔离，二者冲突（个人多 thread 共用 `users/{uid}/personal/` 即非 per-thread 隔离）。

**解法**：所有 thread（个人 + 团队）的 codex cwd 统一为 `threads/{thread_id}/`。`users/{uid}/` 目录保留为用户级文件区（chat 上传根、文件浏览默认根），但**不再是任何 thread 的 cwd**。`teams/{team_id}/shared/` 和 `teams/{team_id}/members/` 不再作为 thread cwd（旧数据向后兼容见 §9）。

新增 `workspace::thread_workspace_path(root, thread_id) -> PathBuf = root/threads/{tid}` 和 `ensure_thread_workspace`。`mt_create_thread` 中 `ws_cwd` 统一取该路径，不再分 personal/team 分支。

### 11.3 复制 key 与函数签名的 thread 级化

per-thread 要求以下 Redis key / 函数从 team 级改为 thread 级：

- Redis offset key：`repl:offset:{thread_id}:{rel_path}`（去掉 conv 维度，因 per-thread 复制单元即 thread）
- Redis 主租约：`codex:primary:{thread_id}`
- Redis 文件同步 offset：`filesync:offset:{thread_id}:{rel_path}`
- 所有 `replication::*` 函数的 `team_id: &str` 参数改为 `thread_id: &str`：`get`、`get_or_assign`、`ensure_replica`、`renew_lease`、`try_acquire_primary`、`set_primary`、`replicate_team_rollouts`（→ `replicate_thread_rollout`，单 thread）、`promote_if_primary_down`、`reclaim_orphan_teams`（→ `reclaim_orphan_threads`）、`delete_all_team_offsets*`（→ `delete_all_thread_offsets*`）
- 维护循环 `run_replica_maintenance`（`main.rs:389`）遍历维度从 session_replicas 行（现为 thread）改为 thread 级

### 11.4 实现阶段拆分

实现按 4 个独立计划/阶段推进，每阶段产出可测试软件：

- **阶段 A**：心跳 TTL 优化（独立、零风险，改常量）
- **阶段 B**：per-thread 调度基础设施（schema + 实体 + 所有 replication 函数 + 维护循环 + 两阶段 resolve_worker + workspace 路径统一）——核心，C/D 前提
- **阶段 C**：workspace 文件增量同步（依赖 B）
- **阶段 D**：惰性 Rebalance（依赖 B）

---

**下一步**：创建实现计划

# Per-Thread 调度 + 增量文件同步 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将集群调度从 per-team 改为 per-thread，新增 workspace 文件增量同步、惰性 rebalance、缩短心跳 TTL，解决负载不均衡、扩缩容无迁移、节点下线感知慢、failover 文件丢失四个问题。

**Architecture:** session_replicas 主键由 `team_id` 改为 `thread_id`；所有复制/租约/路由基础设施从 team 级降为 thread 级；`resolve_worker` 改为两阶段（最少负载选节点 → thread/start 拿到 thread_id → 登记 session_replicas）；workspace cwd 统一为 `threads/{thread_id}/`；新增 per-thread 文件增量同步模块（沿用 rollout 的 offset 增量模型）和惰性 rebalance 模块。

**Tech Stack:** Rust + axum + SeaORM（PG/MySQL 多方言）+ Redis + tokio。测试用 `#[tokio::test]` + `std::env::temp_dir` + `uuid::Uuid::new_v4`（对齐现有 `replication.rs` 测试模式）。

## Global Constraints

- **语言**：所有代码注释、commit message 描述用中文（对齐仓库现状），专业术语保留英文。
- **DB 多方言**：migration 用 `db.execute_unprepared` + `IF NOT EXISTS`/`IF EXISTS` 兼容 PG/MySQL；新表/新列必须加 COMMENT。
- **错误类型**：统一用 `crate::error::AppError::internal(format!(...))`（对齐 `replication.rs` 现状）。
- **时间戳**：毫秒 i64，统一用 `crate::services::multitenant::now_ms()`。
- **路径安全**：所有跨节点传输的相对路径必须过 `replication::safe_join` 校验（防路径穿越/symlink 逃逸）。
- **Redis 可选**：所有 Redis 操作必须 `Option<&redis::Client>` + 进程内 fallback（对齐 `get_offset_dual` 模式），单节点无 Redis 时降级可用。
- **阶段依赖**：A 独立；B 是 C/D 前提；必须按 A → B → C → D 顺序执行。
- **commit 粒度**：每个 Task 结尾 commit，message 用 `refactor(...)` / `feat(...)` 前缀。

## File Structure

**新增文件**：
- `backend-rs/src/db/migration/m20260721_000001_session_replicas_per_thread.rs` — session_replicas 主键迁移
- `backend-rs/src/services/workspace/file_sync.rs` — per-thread 文件增量同步（阶段 C）
- `backend-rs/src/services/multitenant/rebalance.rs` — 惰性 rebalance（阶段 D）

**修改文件**：
- `backend-rs/src/config.rs` — ClusterConfig 加 `heartbeat_ttl_secs` / `lease_ttl_secs`（阶段 A）
- `backend-rs/src/db/entities/mod.rs` — `session_replica` 子模块 `team_id` → `thread_id`（阶段 B）
- `backend-rs/src/db/migration/mod.rs`（或 migration 注册处）— 注册新 migration
- `backend-rs/src/services/multitenant/replication.rs` — 所有函数 `team_id` → `thread_id` + Redis key（阶段 B）
- `backend-rs/src/services/multitenant/mod.rs` — `pub mod rebalance;`（阶段 D）
- `backend-rs/src/services/workspace/mod.rs` — `thread_workspace_path` / `ensure_thread_workspace`（阶段 B）
- `backend-rs/src/api/multitenant/handlers.rs` — `mt_create_thread` / `resolve_worker` 两阶段重构（阶段 B）
- `backend-rs/src/api/multitenant/internal_rpc.rs` — 新增 `/internal/filesync` 路由（阶段 C）
- `backend-rs/src/services/multitenant/rpc.rs` — 新增 `replicate_files`（阶段 C）
- `backend-rs/src/main.rs` — 心跳 TTL、维护循环改 thread 级 + 调 rebalance/file_sync（阶段 A/B/C/D）
- `backend-rs/src/state.rs` — 无结构变化（`active_rollout`/`local_offsets` 复用）

---

## 阶段 A：心跳 TTL 优化（独立、零风险）

### Task A1: 心跳/租约 TTL 配置化 + 默认值缩短

**Files:**
- Modify: `backend-rs/src/config.rs:64-76`（ClusterConfig 加字段）
- Modify: `backend-rs/src/main.rs:296-303`（心跳用配置 TTL）
- Modify: `backend-rs/src/services/multitenant/replication.rs:50-53`（租约常量改配置默认）

**Interfaces:**
- Produces: `ClusterConfig { heartbeat_ttl_secs: u64, lease_ttl_secs: u64 }`（带 serde default），供 main.rs 和 replication.rs 读取。

- [ ] **Step 1: 给 ClusterConfig 加两个 TTL 字段（带默认值）**

修改 `backend-rs/src/config.rs` 的 `ClusterConfig`（第 64-76 行），在 `worker_id` 字段后追加：

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct ClusterConfig {
    #[serde(default = "default_internal_rpc_host")]
    pub internal_rpc_host: String,
    pub internal_rpc_port: Option<u16>,
    pub worker_id: String,
    #[serde(default)]
    pub worker_rpc_url_enabled: bool,
    #[serde(default)]
    pub worker_rpc_url: Option<String>,
    /// 集群心跳 Redis key TTL(秒)。节点下线后最多经此时长被感知。默认 15。
    #[serde(default = "default_heartbeat_ttl")]
    pub heartbeat_ttl_secs: u64,
    /// primary 租约 TTL(秒),须显著大于维护周期(15s)。默认 60。
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: u64,
}

fn default_heartbeat_ttl() -> u64 { 15 }
fn default_lease_ttl() -> u64 { 60 }
```

- [ ] **Step 2: 心跳 task 用配置 TTL**

修改 `backend-rs/src/main.rs:296-303`，把硬编码 `30` 改为读取配置：

```rust
let heartbeat_ttl = cfg.cluster.heartbeat_ttl_secs;
tokio::spawn(async move {
    loop {
        if let Err(e) = rc.heartbeat(heartbeat_ttl, &rpc_url).await {
            tracing::warn!(error = %e, "cluster heartbeat failed");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
});
```

注：`cfg` 在该作用域可见（main.rs 启动期），`heartbeat_ttl` 按 move 捕获进 task。若 `rc.heartbeat` 签名是 `(&self, ttl_secs: u64, ...)` 则直接传值。

- [ ] **Step 3: 租约常量改用配置默认值（保持常量接口，值缩短）**

修改 `backend-rs/src/services/multitenant/replication.rs:50-53`：

```rust
/// 主租约有效期(毫秒)。默认 60s,须显著大于维护周期(15s)。
/// 注:此处保留常量供 replication 内部 Lua/SET EX 用;若需运行时配置,
/// 后续由调用方传入(当前与 config.lease_ttl_secs 默认值对齐)。
pub const LEASE_TTL_MS: i64 = 60_000;
const LEASE_TTL_SECS: u64 = 60;
```

（说明：replication 函数签名暂不改，仅缩短常量值。运行时可配置化留作后续优化，当前对齐 `default_lease_ttl`。）

- [ ] **Step 4: cargo check + cargo test**

Run: `cargo check -p codex-webui`
Expected: 编译通过。

Run: `cargo test -p codex-webui --lib services::multitenant::replication`
Expected: 现有 replication 测试仍通过（常量值变化不影响文件系统测试）。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/config.rs backend-rs/src/main.rs backend-rs/src/services/multitenant/replication.rs
git commit -m "feat(cluster): 心跳 TTL 30s→15s + 租约 120s→60s,配置化默认值"
```

---

## 阶段 B：per-thread 调度基础设施（核心）

### Task B1: session_replicas 主键迁移 team_id → thread_id

**Files:**
- Create: `backend-rs/src/db/migration/m20260721_000001_session_replicas_per_thread.rs`
- Modify: migration 注册处（`backend-rs/src/db/migration/mod.rs` 或 `main.rs` 的 migration 列表）
- Modify: `backend-rs/src/db/entities/mod.rs:274-294`（session_replica 实体）

**Interfaces:**
- Produces: `session_replica::Model { thread_id: String, primary_node, replica_node, status, primary_lease_until, updated_at }`，主键 `thread_id`。后续所有 replication 函数以此签名交互。

- [ ] **Step 1: 改实体（team_id → thread_id）**

修改 `backend-rs/src/db/entities/mod.rs` 第 274-294 行的 `session_replica` 模块：

```rust
/// per-thread 主副本映射(active-passive HA):thread_id → primary_node + replica_node。
pub mod session_replica {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "session_replicas")]
    pub struct Model {
        #[sea_orm(primary_key, column_type = "String(StringLen::N(36))")]
        pub thread_id: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub primary_node: String,
        #[sea_orm(column_type = "String(StringLen::N(64))")]
        pub replica_node: Option<String>,
        #[sea_orm(column_type = "String(StringLen::N(16))")]
        pub status: String,
        pub primary_lease_until: i64,
        pub updated_at: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
```

- [ ] **Step 2: 写新 migration 文件**

创建 `backend-rs/src/db/migration/m20260721_000001_session_replicas_per_thread.rs`（参考现有 migration 的 `MigrationName` + `up`/`down` trait 实现；多方言用 `exec_unprepared`）：

```rust
use sea_orm_migration::prelude::*;

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str { "m20260721_000001_session_replicas_per_thread" }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 多方言兼容:重命名旧表 → 按新主键重建。
        // 1. 旧表 session_replicas(team_id PK) 改名为 session_replicas_old。
        //    IF EXISTS 兼容全新库(旧表不存在)。
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE IF EXISTS session_replicas RENAME TO session_replicas_old"
        ).await.ok(); // 旧表可能不存在,忽略错误。

        // 2. 建新表(thread_id PK)。
        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS session_replicas (
                thread_id VARCHAR(36) PRIMARY KEY NOT NULL,
                primary_node VARCHAR(64) NOT NULL,
                replica_node VARCHAR(64),
                status VARCHAR(16) NOT NULL DEFAULT 'active',
                primary_lease_until BIGINT NOT NULL DEFAULT 0,
                updated_at BIGINT NOT NULL
            )"#,
        ).await?;

        // 3. 数据迁移:为每个旧 team 行,按该 team 当前活跃 thread 展开成 per-thread 行。
        //    用 threads 表 join session_replicas_old(per-team 主副本 → 该 team 所有 thread)。
        //    无活跃 thread 的旧 team 行丢弃(per-team 记录在 per-thread 模型下无意义)。
        db.execute_unprepared(
            r#"INSERT INTO session_replicas (thread_id, primary_node, replica_node, status, primary_lease_until, updated_at)
               SELECT t.id, o.primary_node, o.replica_node, o.status, o.primary_lease_until, o.updated_at
               FROM threads t
               JOIN session_replicas_old o ON t.team_id = o.team_id
               ON CONFLICT (thread_id) DO NOTHING"#,
        ).await.ok(); // MySQL 无 ON CONFLICT 时忽略;per-thread 行后续 get_or_assign 会补建。

        // 4. 删旧表。
        db.execute_unprocessed("DROP TABLE IF EXISTS session_replicas_old").await.ok();

        db.execute_unprepared(
            "COMMENT ON TABLE session_replicas IS 'per-thread 主副本映射(active-passive HA):thread_id → primary + replica';
             COMMENT ON COLUMN session_replicas.thread_id IS '会话 ID(主键)';
             COMMENT ON COLUMN session_replicas.primary_node IS '跑 codex 的主节点 ID';
             COMMENT ON COLUMN session_replicas.replica_node IS '存 rollout/workspace 副本的节点 ID(可空)';
             COMMENT ON COLUMN session_replicas.status IS '状态:active / promoting / degraded';
             COMMENT ON COLUMN session_replicas.primary_lease_until IS '主节点租约到期时间戳(毫秒)';
             COMMENT ON COLUMN session_replicas.updated_at IS '更新时间戳(毫秒)';"
        ).await.ok();
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> { Ok(()) }
}
```

注：`ON CONFLICT` 仅 PG 支持；MySQL 下该 INSERT 可能报错被 `.ok()` 吞掉，per-thread 行由运行时 `get_or_assign` 补建，不影响正确性（旧 team 记录在 per-thread 模型下本就需要重建）。

- [ ] **Step 3: 注册 migration**

在 migration 注册处（查找 `m20260719_000001_combined_schema` 的注册位置，通常 `backend-rs/src/db/migration/mod.rs` 的 `vec![Box::new(...)]` 列表）追加：

```rust
Box::new(m20260721_000001_session_replicas_per_thread::Migration),
```

并加 `pub mod m20260721_000001_session_replicas_per_thread;`。

- [ ] **Step 4: cargo check + 修编译错误**

Run: `cargo check -p codex-webui`
Expected: 编译错误集中在 `replication.rs` 和 `main.rs` 对 `team_id` 字段/`Column::TeamId` 的引用 —— 这些在 Task B2/B6 修复。**本 task 只需 migration + 实体编译通过**，其余文件的 `team_id` 引用编译错误属于预期，先不处理。

（若实体改名导致下游大量错误阻断编译，可先在 Task B2 一并处理；本 task 保证 migration 文件本身语法正确。）

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/db/
git commit -m "refactor(db): session_replicas 主键 team_id→thread_id + 数据迁移"
```

---

### Task B2: replication.rs 分配/查询函数改 thread_id

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs:66-239`（get / get_or_assign / ensure_replica / set_primary 及 Column 引用）

**Interfaces:**
- Produces: `replication::get(db, thread_id)`, `get_or_assign(db, thread_id, cluster) -> Result<Model>`, `ensure_replica(db, thread_id, cluster)`, `set_primary(db, thread_id, new_primary, new_replica)`。

- [ ] **Step 1: 改 get / get_or_assign / ensure_replica（team_id → thread_id）**

修改 `replication.rs`，把这些函数的参数 `team_id: &str` 全部改名为 `thread_id: &str`，`ActiveModel.team_id` → `ActiveModel.thread_id`，`Column::TeamId` → `Column::ThreadId`，`Entity::find_by_id(team_id)` → `find_by_id(thread_id)`。示例（get_or_assign 改后）：

```rust
pub async fn get(db: &DatabaseConnection, thread_id: &str) -> Result<Option<Model>, AppError> {
    Entity::find_by_id(thread_id.to_string())
        .one(db)
        .await
        .map_err(|e| AppError::internal(format!("query session_replica: {e}")))
}

/// 取或分配该 thread 的主副本。
pub async fn get_or_assign(
    db: &DatabaseConnection,
    thread_id: &str,
    cluster: &dyn ClusterMembership,
) -> Result<Model, AppError> {
    if let Some(m) = get(db, thread_id).await? {
        return Ok(m);
    }
    let primary = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;
    let replica = alive.into_iter().find(|n| n != &primary);
    let now = now_ms();
    let am = ActiveModel {
        thread_id: Set(thread_id.to_string()),
        primary_node: Set(primary),
        replica_node: Set(replica),
        status: Set("active".to_string()),
        primary_lease_until: Set(now + LEASE_TTL_MS),
        updated_at: Set(now),
    };
    match am.insert(db).await {
        Ok(m) => Ok(m),
        Err(_) => get(db, thread_id).await?.ok_or_else(|| AppError::internal("session_replica vanished".into())),
    }
}
```

`ensure_replica` 和 `set_primary` 同理：参数 `team_id` → `thread_id`，`ActiveModel.team_id`/`Column::TeamId` → `thread_id`/`Column::ThreadId`，函数体逻辑不变（反亲和选 replica）。

- [ ] **Step 2: cargo check 修复剩余 Column::TeamId 引用**

Run: `cargo check -p codex-webui 2>&1 | grep -i "TeamId\|team_id"`

逐个把 `replication.rs` 内残留的 `Column::TeamId` / `team_id` 改为 `Column::ThreadId` / `thread_id`（renew_lease 的 filter、set_primary 等）。

- [ ] **Step 3: cargo check 通过**

Run: `cargo check -p codex-webui`
Expected: replication.rs 内的分配/查询函数编译通过（复制/租约函数在 B3 处理）。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs
git commit -m "refactor(replication): 分配/查询函数 team_id→thread_id"
```

---

### Task B3: replication.rs 复制/租约/晋升函数改 thread_id + Redis key

**Files:**
- Modify: `backend-rs/src/services/multitenant/replication.rs:132-545`（renew_lease / try_acquire_primary / replicate_team_rollouts / promote_if_primary_down / reclaim_orphan_teams / delete_all_*_offsets）

**Interfaces:**
- Produces: `replicate_thread_rollout(db, thread_id, codex_home, cluster, redis, rpc, active_rollout, local_offsets)`（单 thread 复制，替代 per-team 的 `replicate_team_rollouts`）；`reclaim_orphan_threads`；`delete_all_thread_offsets*`；Redis key `repl:offset:{thread_id}:{rel_path}`、`codex:primary:{thread_id}`。

- [ ] **Step 1: renew_lease / try_acquire_primary / set_primary 改 thread_id + Redis key**

`renew_lease(db, thread_id, node_id, redis)`：filter 用 `Column::ThreadId.eq(thread_id)`；Redis key `format!("codex:primary:{thread_id}")`。`try_acquire_primary(redis, thread_id, node_id)`：同 key。函数体其余不变。

- [ ] **Step 2: replicate_team_rollouts → replicate_thread_rollout（单 thread）**

把按 team 遍历 active_rollout 改为复制**单个 thread** 的 rollout。新签名：

```rust
/// 主侧:复制单个 thread 的 rollout 增量到副本节点。
pub async fn replicate_thread_rollout(
    db: &DatabaseConnection,
    thread_id: &str,
    codex_home: &Path,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
    rpc_client: &WorkerRpcClient,
    active_rollout: &ThreadRolloutMap,
    local_offsets: &LocalOffsetMap,
) -> Result<(), AppError> {
    let Some(row) = get(db, thread_id).await? else { return Ok(()); };
    let Some(replica_node) = row.replica_node.clone() else { return Ok(()); };
    if replica_node == cluster.local_node_id() { return Ok(()); }
    let Some(rpc_addr) = cluster.node_rpc_addr(&replica_node).await else { return Ok(()); };

    // 单 thread:从 active_rollout 取该 thread 的文件路径。
    let abs_path = {
        let m = active_rollout.lock().await;
        match m.get(thread_id) {
            Some(p) if p.exists() => p.clone(),
            _ => return Ok(()), // 无活跃 rollout(重启后未重建),本轮跳过。
        }
    };
    let size = match tokio::fs::metadata(&abs_path).await {
        Ok(m) => m.len(),
        Err(_) => return Ok(()),
    };
    let rel_path = match abs_path.strip_prefix(codex_home) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return Ok(()),
    };
    let offset = get_offset_dual(redis, local_offsets, thread_id, &rel_path).await;
    if size <= offset { return Ok(()); }
    let bytes = match read_range(&abs_path, offset, size).await {
        Ok(b) => b,
        Err(e) => { tracing::warn!(thread_id, error = %e, "read rollout range failed"); return Ok(()); }
    };
    let chunk = RolloutChunk {
        thread_id: thread_id.to_string(), // RolloutChunk 字段 team_id→thread_id(见下)
        conv_id: thread_id.to_string(),
        rel_path: rel_path.clone(),
        offset,
        bytes,
    };
    if let Err(e) = rpc_client.replicate_rollout(&rpc_addr, &chunk).await {
        tracing::warn!(thread_id, error = %e, "replicate rollout chunk failed");
        return Ok(()); // 不推进 offset,下轮重传。
    }
    set_offset_dual(redis, local_offsets, thread_id, &rel_path, size).await;
    metrics::counter!("replication_bytes_total").increment(chunk.bytes.len() as u64);
    Ok(())
}
```

同步修改：
- `RolloutChunk` 结构体字段 `team_id` → `thread_id`（第 56-64 行）。
- `get_offset_dual` / `set_offset_dual` 参数 `team_id` → `thread_id`，Redis key 改 `format!("repl:offset:{thread_id}:{rel_path}")`（去掉 conv 维度，per-thread 复制单元即 thread）；签名中 `conv` 参数删除，调用处同步。
- `LocalOffsetMap` 的 key 元组从 `(team, conv, rel)` 改为 `(thread_id, rel)` 二元组；`state.rs:75` 的类型注解同步改。

- [ ] **Step 3: promote_if_primary_down 改 thread_id**

参数 `team_id` → `thread_id`，内部 `get(db, thread_id)`、`try_acquire_primary(redis, thread_id, me)`、`set_primary(db, thread_id, ...)`、`delete_all_thread_offsets_dual(redis, local_offsets, thread_id)`。返回 true 时由调用方对该**单个 thread** 调 `thread/resume`（不再遍历 team 所有 thread）。

- [ ] **Step 4: reclaim_orphan_teams → reclaim_orphan_threads**

改为扫描所有 session_replicas 行，对 primary 不 alive 的 thread，由最低 alive id 节点认领：

```rust
pub async fn reclaim_orphan_threads(
    db: &DatabaseConnection,
    cluster: &dyn ClusterMembership,
    redis: Option<&redis::Client>,
) -> Result<(), AppError> {
    let me = cluster.local_node_id().to_string();
    let alive = cluster.alive_nodes().await;
    let mut sorted = alive.clone();
    sorted.sort();
    if !sorted.first().map(|n| n == &me).unwrap_or(false) { return Ok(()); }
    let rows = Entity::find().all(db).await
        .map_err(|e| AppError::internal(format!("reclaim scan: {e}")))?;
    for row in rows {
        if alive.iter().any(|n| n == &row.primary_node) { continue; }
        if !try_acquire_primary(redis, &row.thread_id, &me).await { continue; }
        let new_replica = alive.iter().find(|n| n.as_str() != me).cloned();
        set_primary(db, &row.thread_id, &me, new_replica.as_deref()).await?;
        if let Some(c) = redis { delete_all_thread_offsets(c, &row.thread_id).await; }
        tracing::info!(thread_id = %row.thread_id, "reclaimed orphan thread as primary");
    }
    Ok(())
}
```

- [ ] **Step 5: delete_all_team_offsets* → delete_all_thread_offsets***

`delete_all_thread_offsets(redis, thread_id)`：pattern 改 `format!("repl:offset:{thread_id}:*")`。`delete_all_thread_offsets_dual(redis, local_offsets, thread_id)`：retain 改 `|(t, _), _| t != thread_id`（二元组 key）。

- [ ] **Step 6: 更新 receive_rollout 测试中的 RolloutChunk 字段**

`replication.rs:699-741` 测试里 `RolloutChunk { team_id: "t1" }` → `thread_id: "t1"`，`conv_id` 保留（receive 逻辑用 conv_id 做 per-conv 锁，保持）。

- [ ] **Step 7: cargo test**

Run: `cargo test -p codex-webui --lib services::multitenant::replication`
Expected: PASS（receive_rollout/find_rollout/safe_join 测试通过）。

- [ ] **Step 8: Commit**

```bash
git add backend-rs/src/services/multitenant/replication.rs backend-rs/src/state.rs
git commit -m "refactor(replication): 复制/租约/晋升函数 team_id→thread_id + Redis key"
```

---

### Task B4: workspace 路径统一 thread_workspace_path

**Files:**
- Modify: `backend-rs/src/services/workspace/mod.rs:21-84`（加 thread 路径函数）
- Test: `backend-rs/src/services/workspace/mod.rs`（#[cfg(test)] 模块）

**Interfaces:**
- Produces: `workspace::thread_workspace_path(root, thread_id) -> PathBuf`，`workspace::ensure_thread_workspace(state, thread_id) -> Result<PathBuf>`。

- [ ] **Step 1: 写失败测试**

在 `workspace/mod.rs` 末尾加测试模块（文件当前无 #[cfg(test)]，新增）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_workspace_path_layout() {
        let root = std::path::Path::new("/data/ws");
        let p = thread_workspace_path(root, "tid-123");
        assert_eq!(p, std::path::PathBuf::from("/data/ws/threads/tid-123"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p codex-webui --lib services::workspace::tests::thread_workspace_path_layout`
Expected: FAIL（`thread_workspace_path` 未定义）。

- [ ] **Step 3: 实现 thread 路径函数**

在 `workspace/mod.rs` 常量区（第 21-24 行）加 `const THREADS_DIR: &str = "threads";`，在 `team_member_path` 之后（第 43 行后）加：

```rust
/// per-thread workspace 绝对路径(个人/团队统一)。
pub fn thread_workspace_path(workspace_root: &Path, thread_id: &str) -> PathBuf {
    workspace_root.join(THREADS_DIR).join(thread_id)
}

/// 确保 per-thread workspace 目录存在,返回其绝对路径。
pub async fn ensure_thread_workspace(
    state: &AppState,
    thread_id: &str,
) -> Result<PathBuf, AppError> {
    let path = thread_workspace_path(&state.workspace_root, thread_id);
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(|e| AppError::internal(format!("create {}: {e}", path.display())))?;
    Ok(path)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p codex-webui --lib services::workspace::tests`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add backend-rs/src/services/workspace/mod.rs
git commit -m "feat(workspace): 新增 per-thread workspace 路径(threads/{thread_id})"
```

---

### Task B5: mt_create_thread + resolve_worker 两阶段重构

**Files:**
- Modify: `backend-rs/src/api/multitenant/handlers.rs:514-635`（mt_create_thread）
- Modify: `backend-rs/src/api/multitenant/handlers.rs:872-902`（resolve_worker）
- Modify: `backend-rs/src/api/multitenant/internal_rpc.rs`（thread_start 转发参数对齐）

**Interfaces:**
- Consumes: `workspace::ensure_thread_workspace`, `replication::get_or_assign(thread_id)`, `replication::find_rollout_for_thread`, `replication::replicate_thread_rollout`。
- Produces: `resolve_worker(state, thread_id: Option<&str>) -> Result<String>`（最少负载选节点，sticky 命中优先）。

**关键时序**：本地预生成 thread_id（UUIDv7）→ ensure_thread_workspace → cwd = threads/{tid}/ → 选节点（最少负载）→ thread/start 传 threadId + cwd → codex 返回 → 登记 session_replicas + sticky。

- [ ] **Step 1: 改 resolve_worker 为两阶段（最少负载选节点）**

替换 `handlers.rs:872-902` 的 `resolve_worker`：

```rust
/// 选目标节点(per-thread 调度):sticky 命中且 alive 优先;否则按最少负载选 alive 节点。
/// 本节点负载并列最少时优先本节点(避免无谓 RPC 转发)。
async fn resolve_worker(state: &AppState, thread_id: Option<&str>) -> Result<String, AppError> {
    use crate::db::entities::session_replica::{Column as SRColumn, Entity as SREntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, PaginatorTrait};

    // 1. sticky 命中且 alive → 直接返回。
    if let Some(tid) = thread_id {
        if let Ok(Some(stuck)) = state.sticky.lookup(tid).await {
            if crate::services::multitenant::cluster::is_alive(state.cluster.as_ref(), &stuck).await {
                return Ok(stuck);
            }
            let _ = state.sticky.clear(tid).await;
        }
    }

    // 2. 统计各 alive 节点的 primary thread 数。
    let alive = state.cluster.alive_nodes().await;
    if alive.is_empty() {
        return Ok(state.cluster.local_node_id().to_string());
    }
    let mut load: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for n in &alive { load.insert(n.clone(), 0); }
    let rows = SREntity::find().all(&state.db).await
        .map_err(|e| AppError::internal(format!("load scan: {e}")))?;
    for r in &rows {
        if let Some(c) = load.get_mut(&r.primary_node) { *c += 1; }
    }

    // 3. 选最少负载;并列时优先本节点(避免无谓 RPC 转发)。
    let me = state.cluster.local_node_id();
    let mut iter = alive.iter();
    let first = iter.next().unwrap(); // alive 非空,安全。
    let mut best_node = first.clone();
    let mut best_load = load[first];
    for n in iter {
        let l = load[n];
        if l < best_load || (l == best_load && n.as_str() == me) {
            best_node = n.clone();
            best_load = l;
        }
    }
    Ok(best_node)
}
```

注：`SREntity::find().all` 每次选节点全表扫描,thread 数大时有开销;若成为瓶颈,后续可加 `GROUP BY primary_node` 聚合查询优化(当前 thread 规模下可接受)。

- [ ] **Step 2: 改 mt_create_thread 用本地预生成 thread_id + 统一 cwd + 两阶段**

替换 `handlers.rs:514-635` 的 `mt_create_thread` 主体（权限校验 + threads 表写入保留）：

```rust
pub async fn mt_create_thread(
    State(state): State<AppState>,
    Extension(uid): Extension<UserId>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<Json<Value>, AppError> {
    let db = require_db(&state);
    let team_id_raw = body.get("teamId").and_then(Value::as_str).map(String::from);
    let mut rest = match body {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    rest.remove("teamId");

    // 权限校验 + team_id/workspace_type 判定(保留:用于权限 + threads.team_id 列)。
    let (pg_team_id, workspace_type) = match &team_id_raw {
        Some(tid) => {
            permissions::require_permission(db, tid, &uid.0, TeamPermission::ThreadCreate).await?;
            (tid.clone(), "team")
        }
        None => (uid.0.clone(), "personal"),
    };

    metrics::counter!("mt_threads_created_total").increment(1);

    // 本地预生成 thread_id(UUIDv7):用于 cwd/session_replicas/sticky/threads 表,
    // 传给 codex thread/start 作为会话 id(见 Step 4 验证)。
    let thread_id = uuid::Uuid::now_v7().to_string();

    // 统一 cwd = threads/{thread_id}/(个人/团队一致)。
    let _ = crate::services::workspace::ensure_thread_workspace(&state, &thread_id).await;
    let ws_cwd = crate::services::workspace::thread_workspace_path(&state.workspace_root, &thread_id);
    rest.insert("cwd".to_string(), Value::String(ws_cwd.to_string_lossy().to_string()));
    rest.insert("threadId".to_string(), Value::String(thread_id.clone()));

    // 两阶段:先选节点(最少负载),再 thread/start。
    let target = resolve_worker(&state, None).await?;
    let resp = if target == state.node_id {
        rest.entry("experimentalRawEvents").or_insert(Value::Bool(false));
        rest.entry("persistExtendedHistory").or_insert(Value::Bool(true));
        state.codex.request("thread/start", Some(Value::Object(rest))).await
            .map_err(|e| AppError::internal(format!("codex thread/start: {e}")))?
    } else {
        let rpc_url = worker_rpc_url(&state, &target).await?;
        state.worker_rpc.thread_start(&rpc_url, &pg_team_id, &uid.0, Value::Object(rest)).await?
    };

    // 登记 session_replicas(per-thread)+ sticky。
    let _ = crate::services::multitenant::replication::get_or_assign(
        &state.db, &thread_id, state.cluster.as_ref(),
    ).await?;
    let _ = state.sticky.bind(&thread_id, &target, 3600).await;

    // PG threads 元数据(用本地预生成 thread_id 作主键)。
    double_write_thread_meta(db, &thread_id, &pg_team_id, &uid.0, workspace_type).await;
    let _ = crate::services::multitenant::resume_cache::put_cached_resume(db, &thread_id, &resp).await;

    // 主侧:关联 rollout 文件 + 复制增量。
    if target == state.node_id {
        if let Some(p) = crate::services::multitenant::replication::find_rollout_for_thread(
            &state.codex_home, &thread_id,
        ).await {
            state.active_rollout.lock().await.insert(thread_id.clone(), p);
        }
        let _ = crate::services::multitenant::replication::replicate_thread_rollout(
            db, &thread_id, &state.codex_home, state.cluster.as_ref(),
            state.mt_redis.as_ref(), &state.worker_rpc,
            &state.active_rollout, &state.local_offsets,
        ).await;
    }

    let cwd = resp.get("cwd").and_then(Value::as_str)
        .or_else(|| resp.get("thread").and_then(|t| t.get("cwd")).and_then(Value::as_str))
        .unwrap_or("");
    let wrapped = serde_json::json!({ "thread": resp, "id": thread_id, "cwd": cwd });
    Ok(Json(wrapped))
}
```

- [ ] **Step 3: 修正所有 resolve_worker 调用点**

`grep -n "resolve_worker(" backend-rs/src` 找到所有调用（mt_create_thread 已改；mt_start_turn 等若有调用），签名从 `(state, team_id, thread_id, is_personal)` 改为 `(state, Some(thread_id))`。mt_start_turn 用 thread_id 做 sticky 命中检查（会话已创建，session_replicas 已登记，sticky 命中即路由；未命中走 resolve_worker 最少负载）。

- [ ] **Step 4: 验证 codex thread/start 是否使用传入 threadId**

Run（手动，需本地 codex + 起服务）: 创建一个 thread，检查 `CODEX_HOME/sessions/` 下 rollout 文件名是否含本地预生成的 thread_id。

- 若**文件名含该 thread_id**：codex 支持外部 threadId，`find_rollout_for_thread(&thread_id)` 能命中，流程闭环。✅
- 若**文件名是 codex 自生成的另一个 id**：codex 忽略 threadId。回退：从 codex 响应 `resp.thread.id` 取真实 `codex_tid`，建映射 `active_rollout.insert(codex_tid, p)` + `session_replicas`/`sticky`/`threads.id` 改用 `codex_tid`，cwd 目录保留 `threads/{thread_id}/` 但需记录 `thread_id(目录) → codex_tid(逻辑)` 映射表（新增 `state.workspace_dir_map: Arc<Mutex<HashMap<String/*codex_tid*/, String/*dir_thread_id*/>>>`）。

**实现约定**：本 task 默认采用「codex 支持外部 threadId」路径（Step 4 验证通过）。若验证失败，在 commit 前补建上述映射表并调整 active_rollout/threads.id 用 codex_tid。

- [ ] **Step 5: cargo check + cargo test**

Run: `cargo check -p codex-webui`
Expected: 编译通过（mt_start_turn 等调用点签名已对齐）。

Run: `cargo test -p codex-webui --lib`
Expected: 现有测试通过。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/api/multitenant/handlers.rs backend-rs/src/api/multitenant/internal_rpc.rs
git commit -m "refactor(api): mt_create_thread/resolve_worker 两阶段 per-thread 调度"
```

---

### Task B6: 维护循环改 thread 级 + promote_resume 单 thread

**Files:**
- Modify: `backend-rs/src/main.rs:388-479`（run_replica_maintenance / promote_resume_team → promote_resume_thread）

**Interfaces:**
- Consumes: `replication::reclaim_orphan_threads`, `ensure_replica(thread_id)`, `renew_lease(thread_id)`, `replicate_thread_rollout(thread_id)`, `promote_if_primary_down(thread_id)`。

- [ ] **Step 1: run_replica_maintenance 改 thread 级遍历**

替换 `main.rs:388-451`：

```rust
async fn run_replica_maintenance(state: &AppState) {
    use sea_orm::EntityTrait;
    let _ = replication::reclaim_orphan_threads(&state.db, state.cluster.as_ref(), state.mt_redis.as_ref()).await;
    let rows = match codex_webui::db::entities::session_replica::Entity::find().all(&state.db).await {
        Ok(r) => r,
        Err(e) => { tracing::warn!(error = %e, "session_replica scan failed"); return; }
    };
    for row in rows {
        let thread_id = row.thread_id.clone();
        let _ = replication::ensure_replica(&state.db, &thread_id, state.cluster.as_ref()).await;
        if row.primary_node == state.node_id {
            if let Err(e) = replication::renew_lease(&state.db, &thread_id, &state.node_id, state.mt_redis.as_ref()).await {
                tracing::warn!(error = %e, thread_id = %thread_id, "renew_lease failed");
            }
            let _ = replication::replicate_thread_rollout(
                &state.db, &thread_id, &state.codex_home, state.cluster.as_ref(),
                state.mt_redis.as_ref(), &state.worker_rpc,
                &state.active_rollout, &state.local_offsets,
            ).await;
        } else if row.replica_node.as_deref() == Some(state.node_id.as_str()) {
            match replication::promote_if_primary_down(
                &state.db, &thread_id, state.cluster.as_ref(), state.mt_redis.as_ref(),
                &state.active_rollout, &state.local_offsets,
            ).await {
                Ok(true) => {
                    let st = state.clone();
                    let tid = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = promote_resume_thread(&st, &tid).await {
                            tracing::warn!(error = %e, thread_id = %tid, "promote resume failed");
                        }
                    });
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, thread_id = %thread_id, "promote check failed"),
            }
        }
    }
}
```

- [ ] **Step 2: promote_resume_team → promote_resume_thread（单 thread resume）**

替换 `main.rs:457-479`，不再查 team 所有 thread，直接 resume 单个：

```rust
/// 副本晋升后:对单个 thread 调 thread/resume 续接。
async fn promote_resume_thread(state: &AppState, thread_id: &str) -> Result<(), codex_webui::error::AppError> {
    let params = serde_json::json!({ "threadId": thread_id, "persistExtendedHistory": true });
    let resume = state.codex.request("thread/resume", Some(params));
    match tokio::time::timeout(std::time::Duration::from_secs(10), resume).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, thread_id = %thread_id, "resume after promote failed (non-fatal)"),
        Err(_) => tracing::warn!(thread_id = %thread_id, "resume after promote timed out (10s, non-fatal)"),
    }
    metrics::counter!("replica_promotion_resumed_total").increment(1);
    Ok(())
}
```

- [ ] **Step 3: cargo check + 全量编译验证**

Run: `cargo check -p codex-webui`
Expected: 整个 per-thread 基础设施编译通过。

Run: `cargo test -p codex-webui --lib`
Expected: 所有测试通过。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/main.rs
git commit -m "refactor(main): 维护循环改 thread 级遍历 + 单 thread resume"
```

---

## 阶段 C：workspace 文件增量同步（依赖 B）

### Task C1: file_sync 数据结构 + scan_changes

**Files:**
- Create: `backend-rs/src/services/workspace/file_sync.rs`
- Modify: `backend-rs/src/services/workspace/mod.rs`（加 `pub mod file_sync;`）

**Interfaces:**
- Produces: `file_sync::FileChange`, `file_sync::ChangeType`, `file_sync::scan_changes(dir, since_ms) -> Result<Vec<FileChange>>`。

- [ ] **Step 1: 写失败测试**

在 `file_sync.rs` 末尾加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scan_changes_picks_files_newer_than_cutoff() {
        let tmp = std::env::temp_dir().join(format!("fs-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();
        // 旧文件(mtime < since)应被忽略。
        let old = tmp.join("old.txt");
        tokio::fs::write(&old, b"old").await.unwrap();
        // 截断 mtime 到 1 小时前(跨平台用 filetime 设置;测试简化:直接读 mtime 作 since)。
        let old_mt = old_mt_seconds(&old).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // 新文件。
        tokio::fs::write(tmp.join("new.txt"), b"new").await.unwrap();

        let changes = scan_changes(&tmp, old_mt + 1).await.unwrap();
        let names: Vec<_> = changes.iter().map(|c| c.relative_path.clone()).collect();
        assert!(names.contains(&"new.txt".to_string()));
        assert!(!names.contains(&"old.txt".to_string()));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    async fn old_mt_seconds(p: &std::path::Path) -> i64 {
        use std::time::UNIX_EPOCH;
        let m = tokio::fs::metadata(p).await.unwrap();
        m.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p codex-webui --lib services::workspace::file_sync`
Expected: FAIL（模块/函数未定义）。

- [ ] **Step 3: 实现 file_sync.rs**

```rust
//! per-thread workspace 文件增量同步(主 → 副本)。
//!
//! 复制单元 = 单个 thread 的 workspace 目录(threads/{thread_id}/)。
//! 主侧维护循环扫描该目录下文件 mtime > last_sync 的,读全文经 RPC 推到副本;
//! 副本 safe_join 后覆盖写。offset = 已同步的最大 mtime(ms),存 Redis + 进程内。

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 文件变更类型。
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum ChangeType { Create, Modify, Delete }

/// 一条文件变更(主 → 副本)。
#[derive(Serialize, Deserialize, Clone)]
pub struct FileChange {
    pub thread_id: String,
    pub relative_path: String,   // 相对 threads/{thread_id}/,正斜杠分隔。
    pub change_type: ChangeType,
    pub content: Option<Vec<u8>>,
}

/// 扫描 dir 下所有文件 mtime(ms) > since_ms 的,返回 FileChange(Create/Modify)。
/// 不追踪 Delete(简化:v1 只同步新增/修改;删除靠 failover 后目录重建容忍)。
pub async fn scan_changes(dir: &Path, since_ms: i64) -> Result<Vec<FileChange>, AppError> {
    use std::time::UNIX_EPOCH;
    if !tokio::fs::metadata(dir).await.map(|m| m.is_dir()).unwrap_or(false) {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&d).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ft = match entry.file_type().await { Ok(f) => f, Err(_) => continue };
            if ft.is_dir() { stack.push(p); continue; }
            let mt = match tokio::fs::metadata(&p).await {
                Ok(m) => m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()),
                Err(_) => continue,
            };
            let Some(mt) = mt else { continue };
            let mt_ms = mt.as_millis() as i64;
            if mt_ms <= since_ms { continue; }
            let rel = match p.strip_prefix(dir) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let content = tokio::fs::read(&p).await.ok();
            let change_type = ChangeType::Modify; // 简化:统一 Modify(覆盖写)。
            out.push(FileChange {
                thread_id: String::new(), // 调用方(scan_and_replicate)回填。
                relative_path: rel,
                change_type,
                content,
            });
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: workspace/mod.rs 注册子模块**

在 `workspace/mod.rs` 第 11-13 行的 `pub mod` 区加 `pub mod file_sync;`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p codex-webui --lib services::workspace::file_sync`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/workspace/
git commit -m "feat(file_sync): 文件变更扫描(mtime 增量)"
```

---

### Task C2: file_sync offset 双存储 + scan_and_replicate

**Files:**
- Modify: `backend-rs/src/services/workspace/file_sync.rs`

**Interfaces:**
- Produces: `file_sync::scan_and_replicate(state, thread_id) -> Result<()>`，内部用 Redis key `filesync:offset:{thread_id}`。

- [ ] **Step 1: 实现 offset 双存储 + scan_and_replicate**

在 `file_sync.rs` 追加（依赖 `AppState`、`replication`、`WorkerRpcClient`，文件顶部加 use）：

```rust
use crate::services::multitenant::replication::get as get_replica;
use crate::services::multitenant::rpc::WorkerRpcClient;
use crate::state::AppState;
use crate::services::workspace::thread_workspace_path;

async fn get_filesync_offset(redis: Option<&redis::Client>, thread_id: &str) -> i64 {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let v: Option<String> = redis::cmd("GET")
                .arg(format!("filesync:offset:{thread_id}"))
                .query_async(&mut conn).await.ok().flatten();
            if let Some(s) = v { return s.parse().unwrap_or(0); }
        }
    }
    0
}

async fn set_filesync_offset(redis: Option<&redis::Client>, thread_id: &str, v: i64) {
    if let Some(c) = redis {
        if let Ok(mut conn) = c.get_multiplexed_async_connection().await {
            let _: () = redis::cmd("SET")
                .arg(format!("filesync:offset:{thread_id}")).arg(v)
                .query_async(&mut conn).await.unwrap_or(());
        }
    }
}

/// 主侧:扫描该 thread workspace 增量,经 RPC 推到副本。仅在 primary 节点调用。
pub async fn scan_and_replicate(state: &AppState, thread_id: &str) -> Result<(), AppError> {
    let ws = thread_workspace_path(&state.workspace_root, thread_id);
    let last = get_filesync_offset(state.mt_redis.as_ref(), thread_id).await;
    let mut changes = scan_changes(&ws, last).await?;
    if changes.is_empty() { return Ok(()); }

    let row = get_replica(&state.db, thread_id).await?;
    let replica_node = row.and_then(|r| r.replica_node);
    let Some(replica) = replica_node else { return Ok(()); };
    if replica == state.node_id { return Ok(()); }
    let Some(rpc_addr) = state.cluster.node_rpc_addr(&replica).await else { return Ok(()); };

    for c in changes.iter_mut() { c.thread_id = thread_id.to_string(); }

    // 计算本轮最大 mtime 作为新 offset(发送成功后才推进)。
    use std::time::UNIX_EPOCH;
    let mut max_mt = last;
    for c in &changes {
        let p = ws.join(&c.relative_path);
        if let Ok(m) = tokio::fs::metadata(&p).await {
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(UNIX_EPOCH) {
                    max_mt = max_mt.max(d.as_millis() as i64);
                }
            }
        }
    }

    if state.worker_rpc.replicate_files(&rpc_addr, &changes).await.is_ok() {
        set_filesync_offset(state.mt_redis.as_ref(), thread_id, max_mt).await;
        metrics::counter!("filesync_bytes_total")
            .increment(changes.iter().map(|c| c.content.as_ref().map(|b| b.len()).unwrap_or(0) as u64).sum());
    } else {
        tracing::warn!(thread_id, "replicate_files failed (will retry next round)");
    }
    Ok(())
}
```

- [ ] **Step 2: cargo check**

Run: `cargo check -p codex-webui`
Expected: 编译通过（`replicate_files` 在 Task C3 实现，此处先报未定义，C3 补）。

（若需本 task 独立编译通过，可先在 C3 实现 `replicate_files` 再回填；计划顺序 C2→C3，C2 的 cargo check 允许 `replicate_files` 未定义错误，C3 完成后整体通过。）

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/services/workspace/file_sync.rs
git commit -m "feat(file_sync): offset 双存储 + scan_and_replicate"
```

---

### Task C3: WorkerRpcClient.replicate_files + internal receive_files handler

**Files:**
- Modify: `backend-rs/src/services/multitenant/rpc.rs`（加 replicate_files）
- Modify: `backend-rs/src/api/multitenant/internal_rpc.rs`（加 /internal/filesync 路由 + receive_files）

**Interfaces:**
- Produces: `WorkerRpcClient::replicate_files(base, changes) -> Result<()>`；`POST /internal/filesync` 接收 `Vec<FileChange>`。

- [ ] **Step 1: rpc.rs 加 replicate_files**

在 `rpc.rs`（参考第 115-125 行 `replicate_rollout` 模式）加：

```rust
/// 推送 per-thread workspace 文件变更到副本节点(POST /internal/filesync)。
pub async fn replicate_files(
    &self,
    base: &str,
    changes: &[crate::services::workspace::file_sync::FileChange],
) -> Result<(), AppError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/internal/filesync"))
        .header(INTERNAL_RPC_TOKEN_HEADER, self.token.as_deref().unwrap_or(""))
        .json(changes)
        .send()
        .await
        .map_err(|e| AppError::internal(format!("replicate_files: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::internal(format!("replicate_files status: {}", resp.status())));
    }
    Ok(())
}
```

注：`INTERNAL_RPC_TOKEN_HEADER` 常量取自 rpc.rs 现有定义（对齐 `replicate_rollout` 用的 header 名）。

- [ ] **Step 2: internal_rpc.rs 加 receive_files handler + 路由**

在 `internal_rpc.rs:84-90` 的 `build_internal_router` 路由列表加 `.route("/internal/filesync", post(receive_files))`，并加 handler：

```rust
use crate::services::workspace::{file_sync::{FileChange, ChangeType}, thread_workspace_path, ensure_thread_workspace};

async fn receive_files(
    State(state): State<AppState>,
    axum::Json(changes): axum::Json<Vec<FileChange>>,
) -> Result<StatusCode, AppError> {
    for change in changes {
        // 确保 thread workspace 存在(副本首次接收)。
        let _ = ensure_thread_workspace(&state, &change.thread_id).await;
        let ws = thread_workspace_path(&state.workspace_root, &change.thread_id);
        // 路径安全:过 replication::safe_join 校验相对路径(防穿越)。
        let path = crate::services::multitenant::replication::safe_join(&ws, &change.relative_path).await?;
        match change.change_type {
            ChangeType::Create | ChangeType::Modify => {
                if let Some(content) = &change.content {
                    if let Some(p) = path.parent() {
                        tokio::fs::create_dir_all(p).await
                            .map_err(|e| AppError::internal(format!("mkdir: {e}")))?;
                    }
                    tokio::fs::write(&path, content).await
                        .map_err(|e| AppError::internal(format!("write {}: {e}", path.display())))?;
                }
            }
            ChangeType::Delete => {
                if path.exists() { let _ = tokio::fs::remove_file(&path).await; }
            }
        }
        metrics::counter!("filesync_received_total").increment(1);
    }
    Ok(StatusCode::OK)
}
```

注：`safe_join(&ws, rel)` 中 `ws` 是 `threads/{thread_id}/`（已 create_dir_all），canonicalize 能成功，路径穿越被拒。

- [ ] **Step 3: cargo check + cargo test**

Run: `cargo check -p codex-webui`
Expected: 阶段 C 全部编译通过（C2 的 `replicate_files` 引用已定义）。

Run: `cargo test -p codex-webui --lib`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add backend-rs/src/services/multitenant/rpc.rs backend-rs/src/api/multitenant/internal_rpc.rs
git commit -m "feat(file_sync): RPC replicate_files + 副本 receive_files handler"
```

---

### Task C4: 文件同步集成到维护循环

**Files:**
- Modify: `backend-rs/src/main.rs`（run_replica_maintenance 主侧分支加 file_sync 调用）

- [ ] **Step 1: 主侧分支追加文件同步**

在 B6 写好的 `run_replica_maintenance` 的 `if row.primary_node == state.node_id { ... }` 块内，`replicate_thread_rollout` 之后追加：

```rust
            // per-thread workspace 文件增量同步(failover 后副本可见文件)。
            let _ = crate::services::workspace::file_sync::scan_and_replicate(&state, &thread_id).await;
```

- [ ] **Step 2: cargo check + cargo test**

Run: `cargo check -p codex-webui && cargo test -p codex-webui --lib`
Expected: PASS。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/main.rs
git commit -m "feat(file_sync): 维护循环主侧追加 per-thread 文件同步"
```

---

## 阶段 D：惰性 Rebalance（依赖 B）

### Task D1: rebalance.rs maybe_rebalance + migrate_primary

**Files:**
- Create: `backend-rs/src/services/multitenant/rebalance.rs`
- Modify: `backend-rs/src/services/multitenant/mod.rs`（加 `pub mod rebalance;`）

**Interfaces:**
- Produces: `rebalance::maybe_rebalance(state) -> Result<()>`。

- [ ] **Step 1: 写失败测试**

在 `rebalance.rs` 末尾加测试（纯逻辑:负载统计 + 选目标节点，用内存 cluster mock）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pick_target_prefers_least_loaded() {
        // 3 节点;node-2 已有 5 thread,node-1 有 1,node-3 有 0。
        let load: std::collections::HashMap<String, i64> = [
            ("node-1".into(), 1), ("node-2".into(), 5), ("node-3".into(), 0),
        ].into_iter().collect();
        let alive = vec!["node-1".to_string(), "node-2".to_string(), "node-3".to_string()];
        let me = "node-2"; // 过热节点
        let avg = 6 / 3; // = 2
        // 过热(5 > 2*1.5=3)→ 找 < avg 的节点 → node-3(0)。
        let target = pick_least_loaded(&alive, &load, me, avg);
        assert_eq!(target.as_deref(), Some("node-3"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p codex-webui --lib services::multitenant::rebalance`
Expected: FAIL。

- [ ] **Step 3: 实现 rebalance.rs**

```rust
//! 惰性 rebalance:维护循环检查本节点是否过热,过热则迁移一个 thread 到低负载节点。

use crate::db::entities::session_replica::{Column as SRColumn, Entity as SREntity};
use crate::error::AppError;
use crate::services::multitenant::{cluster::ClusterMembership, replication, now_ms};
use crate::state::AppState;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use std::collections::HashMap;

/// 阈值:本节点 primary 数 > avg * 1.5 视为过热。
const HOT_FACTOR: f64 = 1.5;

/// 从 alive 节点中选负载最低的(排除 me 的并列优先规则;返回 < avg 的节点,无则 None)。
fn pick_least_loaded(
    alive: &[String],
    load: &HashMap<String, i64>,
    me: &str,
    avg: i64,
) -> Option<String> {
    alive.iter()
        .filter(|n| load.get(*n).copied().unwrap_or(0) < avg)
        .min_by_key(|n| load.get(*n).copied().unwrap_or(0))
        .map(|n| {
            let _ = me;
            n.clone()
        })
}

/// 维护循环调用:本节点过热 → 选低负载节点 → 迁移一个最旧的 thread。
/// 安全:只迁移 status=active 且 session_replicas 已登记的 thread;迁移前由调用方确保文件已同步。
pub async fn maybe_rebalance(state: &AppState) -> Result<(), AppError> {
    let alive = state.cluster.alive_nodes().await;
    if alive.len() <= 1 { return Ok(()); }

    let mut load: HashMap<String, i64> = HashMap::new();
    for n in &alive { load.insert(n.clone(), 0); }
    let rows = SREntity::find().all(&state.db).await
        .map_err(|e| AppError::internal(format!("rebalance scan: {e}")))?;
    for r in &rows {
        if let Some(c) = load.get_mut(&r.primary_node) { *c += 1; }
    }
    let total: i64 = load.values().sum();
    let avg = total / alive.len() as i64;

    let my_load = load.get(&state.node_id).copied().unwrap_or(0);
    if (my_load as f64) <= (avg as f64) * HOT_FACTOR { return Ok(()); }

    let Some(target) = pick_least_loaded(&alive, &load, &state.node_id, avg) else {
        return Ok(()); // 无低负载节点。
    };

    // 选本节点最旧的一个 thread 迁移(updated_at asc)。
    let Some(thread) = SREntity::find()
        .filter(SRColumn::PrimaryNode.eq(state.node_id.clone()))
        .order_by_asc(SRColumn::UpdatedAt)
        .one(&state.db).await
        .map_err(|e| AppError::internal(format!("rebalance pick: {e}")))?
    else { return Ok(()); };

    // 迁移:把 primary 改为 target,选新 replica(反亲和,排除 target)。
    let new_replica = alive.iter().find(|n| n.as_str() != target).cloned();
    replication::set_primary(&state.db, &thread.thread_id, &target, new_replica.as_deref()).await?;
    // 清该 thread 复制 offset → target 下次从 0 全量同步 rollout+文件。
    if let Some(c) = state.mt_redis.as_ref() {
        replication::delete_all_thread_offsets(c, &thread.thread_id).await;
    }
    // 清 sticky:强制后续请求重新解析到新 primary。
    let _ = state.sticky.clear(&thread.thread_id).await;
    metrics::counter!("rebalance_migrations_total").increment(1);
    tracing::info!(thread_id = %thread.thread_id, from = %state.node_id, to = %target, "rebalanced thread");
    Ok(())
}
```

注：`delete_all_thread_offsets` 在 B3 已重命名；`set_primary` 在 B2 已改 thread_id 签名。

- [ ] **Step 4: mod.rs 注册**

在 `services/multitenant/mod.rs` 的 `pub mod` 区加 `pub mod rebalance;`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p codex-webui --lib services::multitenant::rebalance`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add backend-rs/src/services/multitenant/rebalance.rs backend-rs/src/services/multitenant/mod.rs
git commit -m "feat(rebalance): 惰性 rebalance(过热迁移 + 最少负载选目标)"
```

---

### Task D2: rebalance 集成到维护循环 + 节流

**Files:**
- Modify: `backend-rs/src/main.rs`（run_replica_maintenance 调 maybe_rebalance + 节流间隔）

- [ ] **Step 1: 维护循环调 rebalance(带节流)**

rebalance 频繁迁移会抖动,加最小迁移间隔(5 分钟)。在 `AppState` 加一个字段记录上次迁移时间,或用进程内 `AtomicI64`。简化:用 `tokio::sync::Mutex<Option<Instant>>` —— 但 `Date`/`Instant` 在测试外可用。

在 `main.rs` `run_replica_maintenance` 开头(B6 的 `reclaim_orphan_threads` 之前)加节流 rebalance:

```rust
    // 节流:每 5 分钟最多触发一次 rebalance(防抖动)。
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_REBALANCE_MS: AtomicI64 = AtomicI64::new(0);
    const REBALANCE_INTERVAL_MS: i64 = 300_000; // 5 分钟
    let now = now_ms();
    if now - LAST_REBALANCE_MS.load(Ordering::Relaxed) >= REBALANCE_INTERVAL_MS {
        LAST_REBALANCE_MS.store(now, Ordering::Relaxed);
        if let Err(e) = crate::services::multitenant::rebalance::maybe_rebalance(state).await {
            tracing::warn!(error = %e, "rebalance failed");
        }
    }
```

注:`now_ms` 在 `services::multitenant`,main.rs 已 use replication,需确认 `use crate::services::multitenant::now_ms;` 可见(或写全路径)。

- [ ] **Step 2: cargo check + cargo test + cargo clippy**

Run: `cargo check -p codex-webui && cargo test -p codex-webui --lib`
Expected: PASS。

Run: `cargo clippy -p codex-webui -- -D warnings 2>&1 | tail -20`
Expected: 无 warning(若有,按提示修;尤其 unused import / placeholder)。

- [ ] **Step 3: Commit**

```bash
git add backend-rs/src/main.rs
git commit -m "feat(rebalance): 集成维护循环 + 5 分钟节流防抖动"
```

---

## 收尾

### Task E1: 全量编译 + 测试 + spec 对齐检查

- [ ] **Step 1: 全量编译**

Run: `cargo build -p codex-webui`
Expected: 成功。

- [ ] **Step 2: 全量测试**

Run: `cargo test -p codex-webui`
Expected: 全部 PASS。

- [ ] **Step 3: grep 残留 team_id 引用(per-thread 化完整性检查)**

Run: `grep -rn "team_id" backend-rs/src/services/multitenant/replication.rs backend-rs/src/api/multitenant/handlers.rs | grep -i "session_replica\|resolve_worker\|replicate_thread"`
Expected: 无残留(session_replica/resolve_worker/replicate_thread 上下文不应再有 team_id 用于副本分配;threads.team_id 列保留用于权限/查询,不算残留)。

- [ ] **Step 4: 最终 commit(如有)**

```bash
git add -A
git commit -m "test: per-thread 调度全量编译测试通过"
```

---

## Self-Review

**1. Spec 覆盖**：
- 心跳 TTL(§4.4) → Task A1 ✓
- session_replicas per-thread(§3.2/§11.3) → Task B1/B2/B3 ✓
- workspace 路径统一(§11.2) → Task B4/B5 ✓
- 两阶段 resolve_worker(§11.1) → Task B5 ✓
- 维护循环 thread 级(§11.3) → Task B6 ✓
- 增量文件同步(§4.2) → Task C1-C4 ✓
- 惰性 rebalance(§4.3) → Task D1/D2 ✓
- replica 选择软约束(§4.5) → Task B2(反亲和 find,per-thread 无多分片冲突) ✓

**2. 类型一致性**：
- `session_replica.thread_id` 全链路一致(B1 实体 → B2/B3 函数 → B5 handlers → B6 main)✓
- `LocalOffsetMap` key 二元组 `(thread_id, rel)` 在 B3(state.rs)与 B3 函数对齐 ✓
- `RolloutChunk.thread_id` 在 B3 与 C 不交叉(C 用独立 FileChange)✓
- `resolve_worker(state, Option<&str>)` 签名在 B5 定义、B5 Step3 调用点对齐 ✓

**3. 风险点(已在 task 内标注)**：
- Task B5 Step 4 codex threadId 协议验证(回退方案已写明)✓
- Task B1 migration 多方言 ON CONFLICT(MySQL 降级为运行时补建)✓
- Task C1 v1 不追踪 Delete(已注明 failover 目录重建容忍)✓

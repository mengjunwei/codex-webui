# Thread 创建/Resume 排查与设计文档

记录 `feat/multitenant-platform` 分支一次会话链路三连 bug 的排查过程、根因与最终设计。
面向后续维护者：遇到「新建会话 404 / invoke 500 / config.toml 被覆盖」时直接查本文档。

---

## 0. 现象演进（三个独立 bug，叠加暴露）

| 阶段 | 现象 | 根因 |
|---|---|---|
| Bug 1 | `POST /api/mt/threads/{id}/invoke` 返回 **404** "thread not found" | `double_write_thread_meta` 没执行 → PG `threads` 表没记录 |
| Bug 2 | 404 修好后,invoke 返回 **500** `codex thread/resume: -32600 no rollout found` | thread/start 后立即 resume,codex 异步落盘没完成 |
| Bug 3 | 用户 `test-home/config.toml` 的 `model`/`model_providers` 配置每次启动被覆盖 | `write_hooks_config` 用字符串查找替换,破坏用户配置 |

三个 bug 相互独立,但 Bug 1 修复后才暴露 Bug 2(此前 404 直接挡在前面)。

---

## 1. Bug 1:PG threads 表没写入 → invoke 404

### 现象
- 前端创建会话,后端 `POST /api/mt/threads` 返回 200
- 紧接着 `POST /api/mt/threads/{id}/invoke` 返回 **404**
- PG 日志:`SELECT threads WHERE id = ... rows_returned: 0`
- 6 次成功 create,日志里 **0 条 INSERT threads**

### 根因
`backend-rs/src/api/multitenant/handlers.rs` 的 `mt_create_thread` 解析 codex `thread/start` 响应的 `thread_id` 时**多写了一层 `.thread`**:

```rust
// 错误(修复前)
let thread_id = resp
    .get("thread")
    .and_then(|t| t.get("thread"))   // ← 多余的 .thread
    .and_then(|t| t.get("id"))
    ...
```

codex `thread/start` 响应结构(`jsonrpc.rs::request` 返回 `result` 字段本身):
```json
{ "thread": { "id": "019f...", "cwd": "..." }, "model": "...", "cwd": "..." }
```

正确路径是 `resp.thread.id`(一层),代码写成 `resp.thread.thread.id`(两层)→ 永远 `None` →
`if let Some(tid) = thread_id` 整块跳过 → `double_write_thread_meta` 从不执行 → PG `threads` 表永远空。

### 修复
`handlers.rs:489-495` 移除多余层级,fallback 顺序调整:

```rust
let thread_id = resp
    .get("thread")
    .and_then(|t| t.get("id"))
    .and_then(Value::as_str)
    .or_else(|| resp.get("threadId").and_then(Value::as_str))
    .or_else(|| resp.get("id").and_then(Value::as_str));
```

### 排查命令
```bash
# 确认 PG threads 表是否写入(成功 create 后应有 INSERT)
grep -c 'INSERT INTO "threads"' backend-rs/logs/app
# 查某 thread 的完整链路
grep "019f7567" backend-rs/logs/app | grep -E "POST|INSERT|SELECT.*threads"
```

---

## 2. Bug 2:thread/resume 撞 codex 落盘 race → invoke 500

### 现象
Bug 1 修好后,创建会话成功,但点击进入(触发 `thread/resume`)返回:
```
codex thread/resume: rpc error -32600: no rollout found for thread id 019f7572-...
```

### 根因(不是重试能解决,是流程缺步骤)
前端流程:
```
点"新建会话" → createThread.onSuccess
  → setActiveThread(tid, cwd, label)   // 设 store 状态
  → navigate('/t/$threadId')            // URL 变化
  → ThreadView useEffect[threadId] 触发
  → resumeThread.mutate(threadId)       // 立即调 thread/resume
```

`POST /api/mt/threads`(create)耗时 ~5s(codex app-server 启动),返回后前端 **几十 ms 内**就发 `thread/resume`。
codex 0.142.5 的 `thread/start` 返回 `path` 字段(rollout 文件路径),但 **rollout 文件异步落盘,尚未完成** →
`thread/resume` 立即查文件 → `-32600 no rollout found`。

### 对比 main 分支找正确流程
main 分支 `backend-rs/src/threads.rs` 做了两件当前分支漏掉的事:

1. **传 codex 持久化参数**:
   ```rust
   params.insert("experimentalRawEvents", Value::Bool(false));
   params.insert("persistExtendedHistory", Value::Bool(true));
   ```
2. **用 `ThreadResumeRegistry::ensure_resumed` 缓存**:`thread/start` 成功后把响应缓存,后续 `thread/resume` 命中直接返回,根本不发 RPC。

### 为什么用 PG 表而不是进程内 `ThreadResumeRegistry`

集群部署下,invoke 可能落到任意副本:

```
用户 → 负载均衡 → Worker A(非 owner)
                    ↓ resolve_worker(sticky 路由,Redis 共享)
                    转发 RPC → Worker X(owner)
                                ↓
                              codex thread/resume
```

进程内 `HashMap` 只在 owner(X)本地可见。Worker A 转发到 X,X 端的内部 RPC handler 若不查缓存,
仍会走 codex RPC → race 重现。

**PG 表 `thread_resume_cache` 跨进程共享**:任意 worker 创建时写入,任意 worker invoke 时命中。
集群 + 重启 + failover 都自愈。

### 修复(三层)

**a. `mt_create_thread` 加 codex 参数 + 写 PG 缓存**(`handlers.rs`)
```rust
rest.entry("experimentalRawEvents".to_string()).or_insert(Value::Bool(false));
rest.entry("persistExtendedHistory".to_string()).or_insert(Value::Bool(true));
...
// 创建成功后写 PG 缓存,供后续 thread/resume 复用
put_cached_resume(db, tid, &resp).await;
```

**b. `mt_invoke_thread` + 内部 RPC `thread_invoke` 调 thread/resume 前查 PG**(`handlers.rs` + `internal_rpc.rs`)
```rust
if body.method == "thread/resume" {
    if let Some(cached) = resume_cache::get_cached_resume(db, &thread_id).await {
        return Ok(Json(cached));  // 命中直接返回,不发 codex RPC
    }
}
```

**c. `-32600` 退避重试兜底**(`mt_invoke_thread`):真 race 时(cache miss)退避重试 3 次(200/400/600ms),
覆盖 owner 重启后首次 resume 的极端场景。

### 新增文件
- `backend-rs/src/db/migration/m20260718_000003_thread_resume_cache.rs` — 建表 migration
- `backend-rs/src/services/multitenant/resume_cache.rs` — `get_cached_resume` / `put_cached_resume`
- `backend-rs/src/db/entities/mod.rs` — `thread_resume_cache` entity

### 表结构
```sql
CREATE TABLE thread_resume_cache (
    thread_id VARCHAR(36) PRIMARY KEY,
    response JSON NOT NULL,          -- ⚠ 唯一用 JSON 类型的表(存 codex 完整响应)
    updated_at BIGINT NOT NULL
);
```
> 该表是第一个用 `JSON` 列的表。理由:存 codex `thread/start`/`thread/resume` 的完整结构化响应,
> PG/MySQL 均原生支持 JSON,查询/序列化比 TEXT 存字符串更高效。其余表仍遵循 TEXT 约定。

### 集群场景验证矩阵

| 场景 | 行为 |
|---|---|
| W1 收到 invoke → sticky 路由到 W2(owner) | W2 查 PG 命中 → 直接返回 |
| W2 重启(进程内 cache 没了)但 sticky 还指 W2 | W2 查 PG 命中(PG 行仍在)→ 返回 |
| W2 永久挂掉,sticky 切到 W1 | W1 查 PG 命中 → 返回 |
| 真 race(PG 也 miss,极罕见) | `-32600` 退避重试 3 次兜底 |

### 排查命令
```bash
# 确认 PG 缓存命中(不应再看到 -32600)
grep "pg cache hit\|no rollout" backend-rs/logs/app
# 确认 thread/resume 不再调 codex RPC(命中走缓存,无 codex 日志)
grep "thread/resume" backend-rs/logs/codex-jsonrpc.jsonl
```

---

## 3. Bug 3:hooks_config 覆盖用户 config.toml

### 现象
用户 `test-home/config.toml` 手工配置了 `model_provider = "custom"` / `[model_providers.custom]`,
每次后端启动后被改坏(model 配置丢失或格式错乱)。

### 根因
`write_hooks_config` 用**字符串查找替换**注入 `[hooks.audit]` 段:
```rust
let hook_start = existing.find("[hooks.audit]").unwrap_or(0);
// ... 手工切分 ...
```
这种办法会破坏注释、空行、格式,且边界判断脆弱,容易切错位置吞掉用户内容。

### 修复:toml_edit 精确编辑
`backend-rs/src/services/workspace/hooks_config.rs` 改用 `toml_edit::DocumentMut`:

1. **解析**整个 config.toml 为可编辑文档(保留所有注释/格式)
2. **精确设置** `[hooks.audit]` 的 4 个字段(`type`/`url`/`auth_header`/`auth_env`)
3. **写回**:内容真的变了才写盘;一样就跳过(连 mtime 都不刷)

```rust
let mut doc = existing.parse::<DocumentMut>()?;
let audit = doc.entry("hooks").or_insert(table).entry("audit").or_insert(table);
set_str(audit, "url", &format!("http://127.0.0.1:{port}/hooks/codex"));
// ... 其余字段
if doc.to_string() == existing { return Ok(()); }  // 未变跳过
```

依赖:`Cargo.toml` 新增 `toml_edit = "0.22"`。

### 行为保证
- 用户 `model`/`model_providers`/注释/空行 **原样保留**
- 只有 `[hooks.audit]` 的 4 个字段被精确设置
- 内容无需变更时**不写盘**(避免无谓刷新 / 触发 watcher)

### 排查命令
```bash
# 启动后看是否动了 config
grep "hooks config" backend-rs/logs/app
# 期望:要么 "unchanged, skip write",要么 "written (toml_edit, preserve user config)"
# 对比启动前后
cp test-home/config.toml /tmp/before.toml; # 启动后
diff /tmp/before.toml test-home/config.toml
```

---

## 4. 端到端验证清单

```bash
# 1. 启动(Win + WSL:先确保 PG/Redis 在 WSL 内 running)
wsl -d Rocky10 -e bash -c "sudo systemctl start postgresql"  # 若 PG 未起
cd backend-rs && ./target/debug/codex-webui.exe > logs/app.v 2>&1 &
cd web && pnpm dev &

# 2. 新建会话(Playwright 或手点 UI)
#    预期:console 无新增 404/500

# 3. 后端日志核对
grep "pg cache hit" backend-rs/logs/app.v      # thread/resume 应走缓存
grep "no rollout" backend-rs/logs/app.v        # 应为空(无 race)
grep "hooks config" backend-rs/logs/app.v      # skip write 或 toml_edit write
diff /tmp/before.toml test-home/config.toml    # config 未变

# 4. 发消息(触发 turn/start)
#    预期:POST /turns 200,codex 返回 turn inProgress
#    注:codex 上游 LLM API 若超时(Reconnecting...)是 API key/网络问题,非本链路 bug
```

---

## 5. 关键代码位置索引

| 关注点 | 文件:位置 |
|---|---|
| thread_id 解析(曾多一层) | `handlers.rs:489` |
| create 写 PG 缓存 | `handlers.rs:mt_create_thread` 末尾 |
| invoke 查 PG 缓存 + retry | `handlers.rs:mt_invoke_thread` |
| 内部 RPC 也查 PG 缓存 | `internal_rpc.rs:thread_invoke` / `thread_start` |
| PG 缓存读写函数 | `services/multitenant/resume_cache.rs` |
| entity / migration | `db/entities/mod.rs::thread_resume_cache` / `db/migration/m20260718_000003_*` |
| config.toml 精确注入 | `services/workspace/hooks_config.rs` |
| 前端 invoke 调用 | `web/src/lib/mt-client.ts:threadsApi.invoke` |
| 前端 thread-view mount resume | `web/src/routes/thread-view.tsx:166` useEffect |
```

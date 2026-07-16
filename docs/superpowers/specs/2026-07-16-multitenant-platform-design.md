# 多租户分布式 Codex 平台设计

- **日期**:2026-07-16
- **分支**:`feat/multitenant-platform`
- **状态**:设计已确认,M1 实施中
- **目标**:把现有"单用户自用"的 codex-webui 重构为"上千用户、To B、多租户、多机横向扩展"的 SaaS 平台

---

## 0. 背景与现状问题

现有架构经三路 agent 查证,本质是**单用户单机自用**设计,存在以下硬伤:

| 维度 | 现状 | 问题 |
|---|---|---|
| 账号 | 全局唯一 codex 进程 + 单 `CODEX_HOME` | 所有用户共享一个 OpenAI 账号、一个 rate limit |
| 隔离 | JWT `sub` 硬编码 `"webui"`,无 users 表,handler 不传 user_id | 零隔离:知道 threadId 就能读/删别人会话 |
| 数据库 | SQLite 单文件 + `Mutex<Connection>` 同步阻塞 | 阻塞 tokio worker,连锁导致 notification 丢消息 |
| 事件 | 单进程 broadcast(256) + 单任务串行 emit | emit 慢就 `Lagged` 丢前端消息;审批请求会丢 |
| 通信 | `write_tx` 是 unbounded channel | 突发并发无背压,OOM 崩溃 |
| 部署 | 单进程单机 | 无法水平扩展、单点故障 |

---

## 1. 已确认决策

| 维度 | 决定 |
|---|---|
| 定位 | 对外 SaaS,自助注册 |
| 隔离边界 | **team**(组织/团队) |
| team 内会话 | **完全共享**(成员互相可见,对应设计选项 A) |
| 用户↔team | **一人多 team**(`user_members` 多对多 + 前端 team 切换器) |
| 账号来源 | **BYOK**:team owner 填自己的 OpenAI/Codex key,team 内成员共用 |
| 规模 | 中:几百 team、在线 1000~几千、并发推理 ~250 |
| 认证 | 邮箱 + 密码(+ 邮箱验证);OAuth 留二期 |
| 历史数据 | 不迁移,全新开始 |
| 部署形态 | **多机横向扩展**(分布式) |
| CODEX_HOME 放置 | **方案甲**:一致性哈希亲和性 + worker 本地盘 + 快照备份(否决 NFS) |
| 高可用 | Redis 主从+Sentinel、PG 主从+Patroni、worker 故障迁移 + 快照恢复;RPO 5 分钟 |
| 数据层 | **sqlx**(async + 多方言 PG/MySQL + 内置迁移,与现有 rusqlite 共存);SeaORM 1.0 与 rusqlite 的 `libsqlite3-sys` links 冲突([#2725](https://github.com/SeaQL/sea-orm/issues/2725))无法共存,改用 sqlx,待 rusqlite 全迁后再评估 SeaORM |
| 主键 | **UUIDv7 字符串**(VARCHAR(36),应用层生成,时间有序,分布式独立生成不冲突) |
| 类型约定 | 通用类型兼容 PG+MySQL,避免特殊类型(见 §3) |

---

## 2. 系统全景与组件拓扑

```
                     用户(浏览器) — HTTPS/WSS
                              │
                      ┌─── 负载均衡 (LB) ───┐
                      ▼                      ▼
               ┌─ 接入节点 A ─┐        ┌─ 接入节点 B ─┐   无状态 ×N
               │ HTTP/WS、认证、socket.io、路由决策 │
               └────────┬────────┘    └────────┬────────┘
                        └────────┬─────────────┘
                          ┌──────┴──────┐
                          │   Redis     │  路由表/粘性表/Pub-Sub/缓存/限流
                          └──────┬──────┘
              ┌──────────────────┼──────────────────┐
              ▼                  ▼                  ▼
        ┌─ worker 1 ─┐    ┌─ worker 2 ─┐    ┌─ worker N ─┐
        │ agent + codex 进程池(按需启动) + 本地盘 CODEX_HOME │
        └─────┬──────┘    └─────┬──────┘    └─────┬──────┘
              └──── 对象存储(CODEX_HOME 快照) ────┘
              ┌──── PostgreSQL(主从):全局元数据 ────┐
```

- **接入节点**:无状态可扩,Web/WS/认证/socket.io/路由决策(查 Redis 定 worker)
- **worker**:本地盘存 CODEX_HOME;跑 codex 进程池;agent 管启停/健康/快照
- **Redis**:路由表/粘性/Pub-Sub/缓存
- **PostgreSQL**:全部多租户元数据
- **对象存储**:CODEX_HOME 快照

---

## 3. 数据模型

### 3.1 存储分工

| 存储 | 内容 |
|---|---|
| PostgreSQL(主从) | 持久化元数据:users/teams/team_members/invitations/team_api_keys/threads/team_routes |
| Redis | 实时状态:会话粘性、worker 心跳负载、Pub-Sub、限流计数 |

### 3.2 通用类型约定(兼容 PG + MySQL)

| 数据 | 类型 |
|---|---|
| 主键/外键 | `VARCHAR(36)` UUIDv7 |
| 时间 | `BIGINT`(i64,UTC 毫秒时间戳),应用层 chrono 互转(sqlx 的 `chrono`/`mysql`/`migrate` feature 会经 `sqlx-sqlite?/...` 牵出 sqlx-sqlite,与 rusqlite 的 `libsqlite3-sys` links 冲突,故 sqlx 仅开 `runtime-tokio-rustls+postgres`,时间用 i64) |
| 布尔 | `BOOLEAN` |
| 文本 | `VARCHAR(n)` / `TEXT` |
| 枚举 | `VARCHAR` + 应用层校验(不用 DB ENUM) |
| 加密 key | `TEXT`(base64) |
| **不用** | JSONB/JSON/ARRAY/DB ENUM |

### 3.3 核心表

```
users(id, email unique, password_hash, email_verified_at, display_name, created_at, updated_at)
teams(id, name, owner_id→users, created_at, updated_at)
team_members(team_id, user_id, role[owner|member], joined_at)  PK(team_id,user_id)
invitations(id, team_id, code unique, created_by, expires_at, max_uses, used_count, created_at)
team_api_keys(id, team_id, provider, encrypted_key TEXT, key_hint, set_by, is_active, created_at, updated_at)
threads(id=codex conversation_id, team_id, created_by_user_id, title, status, created_at, updated_at, last_activity_at)
team_routes(team_id, worker_id, mapped_at, mapped_reason[initial|failover|rebalance])
refresh_tokens(...)、audit_log(...)  辅助
```

### 3.4 关键设计点

1. 多租户隔离:业务表全带 `team_id`,鉴权铁律"当前 user 必须是 team 的 member"
2. team 内共享:`threads` 无 owner 隔离列,只记 `created_by_user_id` 审计
3. 一人多 team:`team_members` 多对多
4. BYOK 加密:`encrypted_key` AES-GCM(主密钥 `MASTER_KEY`),`key_hint` 尾 4 位
5. **元数据/内容分离**:`threads` 元数据在 PG(唯一真相源,list/权限/路由);rollout 内容在 worker 本地 CODEX_HOME
6. 兼容坑:`team_api_keys`"一 team 一 active"用应用层事务保证(MySQL 无部分唯一索引);不依赖 PG 的 `RETURNING`/`ON CONFLICT`(id 应用层生成)

---

## 4. 路由 + 一致性哈希 + 进程池(分布式核心)

### 4.1 两层路由

```
请求(team_id, thread_id?)
 ├─ 第一层 team→worker  一致性哈希 + team_routes 覆盖   [接入节点]
 └─ 第二层 thread→process 会话粘性                       [worker agent]
```

### 4.2 一致性哈希(team→worker)

- worker 撒 ~150 虚拟节点;`hash(team_id)` 顺时针找归属;加减 worker 最小迁移
- worker 成员 = 心跳活着的机器(5s 心跳,30s 判死)
- `team_routes` 覆盖表:记录故障迁移决策,防节点抖动回切;查询先 team_routes 后哈希

### 4.3 会话粘性

```
Redis: thread:{tid} → {worker_id, process_id, generation}  (TTL,活跃续期)
```

### 4.4 worker agent 进程池

- 懒启动、每 team 按需扩进程(并发≥8 扩,上限 4)、空闲 LRU 回收(15min)、全局上限(~25/32G 机)、全满背压
- **同 team 多进程共享同机 CODEX_HOME 安全**:rollout 每会话独立文件 + 会话粘性保证不跨进程;SQLite WAL 设计支持同机多进程(WAL 跨机才危险)

### 4.5 故障迁移

worker 心跳超时 → 哈希环摘除 → team 顺时针落新 worker → team_routes 记 failover → 新 worker 从快照恢复 CODEX_HOME → 懒启动 + thread/resume。RPO=快照间隔。

### 4.6 关键参数(默认)

| 参数 | 默认 |
|---|---|
| 每机进程上限 | ~25 |
| 每 team 进程上限 | 4 |
| 空闲回收 | 15min |
| 心跳/判死 | 5s/30s |
| 快照间隔(RPO) | 5min |
| 冷启动 | 接受几秒延迟 |

---

## 5. 跨节点事件广播(根治 P0 丢消息)

```
codex(worker) → agent 读 notification → publish Redis(codex:events)
  → 接入节点订阅 → io.to("thread:{tid}").emit()  [socket.io Redis adapter 跨节点]
  → 浏览器
```

- worker agent:持有 codex JSON-RPC 客户端,提取 token_usage/turn_diff/turn_errors 直接写 PG,前端事件 publish Redis
- 接入节点:订阅 Redis,emit 到 room(adapter 跨节点)
- **根治 P0**:不再单进程 broadcast(256)+单任务串行;Redis 高吞吐 + 多接入节点并行
- **审批双保险**:实时推送 + 持久化 PG `pending_server_requests`,前端重连拉取未处理(绝不丢)
- 可靠性分级:审批强保证、DB 记录强保证(agent 直写)、notification 最终一致(thread/read 兜底)

> 实现验证点:socketioxide 的 Redis adapter 成熟度,不成熟则自实现薄适配层。

---

## 6. 认证 + 权限 + BYOK

- 注册:邮箱+密码(argon2)+邮箱验证;登录发 access JWT(15min, sub=user_id)+refresh(7d,一次性轮转)
- **JWT sub 从 "webui" 改真实 user_id**;中间件注入 user_id(旧架构完全不注入)
- token 认 user;team_id 请求带(header);**铁律校验成员关系**(堵死跨 team 访问)
- 角色:owner(全权:加人/邀请/key/设置/解散) / member(仅使用)
- 邀请码:owner 生成(长随机防枚举,可设过期/次数),凭码加入成 member
- BYOK key 生命周期:① 验证(调 OpenAI)→ AES-GCM 加密存储 + key_hint;② agent 启动进程时解密注入 CODEX_HOME/auth.json(明文只在内存,不日志);③ 轮换:新 key active + 标记进程待重启

---

## 7. 高可用 + 扩缩容

| 组件 | HA |
|---|---|
| LB | 云 LB / keepalived |
| 接入节点 | 无状态多副本 + WS 自动重连(状态从 Redis/PG 恢复) |
| Redis | 主从 + Sentinel(起步)/ Cluster |
| PG | 主从 + Patroni + PgBouncer |
| worker | 多台 + 故障迁移 + 快照恢复 |
| 对象存储 | S3 / MinIO |

- 加 worker 主要承接**新 team**(本地盘,不自动搬老 team);rebalance 主动迁移;drain 逐个迁后下线
- 快照:sqlite 用 backup API / `VACUUM INTO`(不能直接 cp);rollout append-only 可直接 cp;auth.json 从 PG 重注入

---

## 8. 背压 + 限流 + 可观测(根治 OOM)

- **背压全链路有界**:`write_tx` 改有界(~1024)+ 每进程 semaphore(max~20)+ 接入节点/RPC 并发上限;满了向上传压力(排队/429),不在内存堆积 → 根治 OOM
- 限流:接入节点(user/team 频率,Redis 令牌桶)+ worker(进程并发)+ team(推理上限)+ key 额度提示
- 可观测:Prometheus 指标(关键 `write_tx_queue_depth`)+ tracing/OTLP(已有)+ OpenTelemetry 全链路追踪 + 告警
- 健康检查:接入节点 /health、agent 心跳、codex ping

---

## 9. 分阶段上线(M1~M6)

开发原则:**先单机把多租户+进程池逻辑跑通(M1~M3),再多机分布式(M4~M5)**。最终产物仍是多机。

| M | 内容 |
|---|---|
| M1 地基 | SeaORM+PG/SQLite、users/teams/team_members/invitations、邮箱注册登录、JWT(sub=user_id)、team 上下文+成员校验、owner/member 权限、邀请码 |
| M2 BYOK | team_api_keys 加密存储+验证+注入 codex、per-team CODEX_HOME、轮换 |
| M3 进程池+路由(单机) | agent、按需启停回收、会话粘性、一致性哈希骨架、write_tx 有界+semaphore、event 订阅迁 agent |
| M4 分布式 | 接入/worker 分离、内网 RPC、Redis 路由/粘性/Pub-Sub、socket.io adapter、多 worker 哈希+故障迁移、快照 |
| M5 HA+可观测+扩缩容 | Redis 主从、PG 主从、Prometheus+OTLP+OTel、rebalance/drain、压测调参 |
| M6 打磨上线 | 防滥用、计费预留、安全审计、灰度 |

---

## 10. M1 实施清单(当前进行)

### 务实策略(双库并存,渐进)

- **新增多租户数据**用 **sqlx**(仅 postgres+mysql 方言,与现有 rusqlite 共存);开发用 docker 起 PG
- **现有 rusqlite 业务数据暂不动**,两套并存,后续里程碑再迁
- 保证每步 `cargo check` 通过、不破坏现有功能

### 步骤

1. **M1.1 加依赖**:sea-orm(sqlite+postgres+mysql)、sea-orm-migration、argon2、uuid(v7)、rand
2. **M1.2 实体+迁移**:entities(users/teams/team_members/invitations) + sea-orm-migration 建表
3. **M1.3 认证核心**:argon2 哈希、JWT(access sub=user_id + refresh)、注册/登录/refresh、middleware 注入 user_id
4. **M1.4 team 模块**:创建(owner)、列出、成员管理、角色、邀请码生成/加入
5. **M1.5 集成**:AppState 加 SeaORM 连接、main 初始化+迁移、routes 注册新路由 + team 上下文校验中间件
6. **M1.6 验证**:cargo check 全绿 + 关键单测(密码/JWT/成员校验/邀请码),现有功能不受影响

### ✅ M1 完成状态(2026-07-16)

M1 已完成并通过验证:lib + tests 编译通过,34 个 lib 单测全绿(含 multitenant 5 个:password/JWT/refresh-hash/email/邀请码)。

实际实现与原计划的差异(均已在前文记录):
- **数据层**:SeaORM → **sqlx**(SeaORM 1.0 与 rusqlite 的 libsqlite3-sys links 冲突 [#2725](https://github.com/SeaQL/sea-orm/issues/2725)),仅开 `runtime-tokio-rustls + postgres + macros`
- **时间**:DATETIME → **BIGINT(i64 UTC 毫秒)**(sqlx 的 chrono/mysql/migrate feature 也会牵出 sqlx-sqlite 同源冲突)
- **迁移**:不用 sqlx::migrate!,改 **手动迁移**(`schema_migrations` 表 + `raw_sql` 多语句)
- MySQL 连接 feature 留后续(M1 生产用 PG)

已交付(`src/multitenant/`):`mod / auth / models / migration / middleware / handlers / teams`;`/api/mt/*` 路由(register/login/refresh + team CRUD/成员/邀请码);AppState 加可选 `mt_pg`(未配 `DATABASE_URL` 则禁用多租户,现有功能不受影响);main 初始化 PG + 跑迁移。

**验证方式**:配 `DATABASE_URL=postgres://...` 启动后自动建表迁移;用 `/api/docs` Swagger UI 或 curl 测 `/api/mt/auth/register` → `/api/mt/auth/login` → `/api/mt/teams` 等。

### ✅ M2 完成状态(2026-07-16,key 管理层)

M2 的 **key 管理层**已完成(team_api_keys 表 + AES-256-GCM 加密存储 + OpenAI 验证 + set/list/轮换 handler)。8 个 multitenant 单测全绿(含 api_keys 加解密 3 个)。

已交付:
- `src/multitenant/api_keys.rs`:`encrypt_key`/`decrypt_key`(AES-256-GCM,主密钥经 SHA-256 派生 32 字节,密文 hex(nonce‖ct))、`key_hint`、`validate_openai_key`(reqwest 调 `/v1/models`)、`set_team_api_key`(验证→加密→旧 key 失活→插新 active,事务)、`list_team_api_keys`、`get_active_plain_key`(供 M3 注入)
- 迁移 `2026071602_api_keys`:`team_api_keys` 表
- handler `/api/mt/teams/{teamId}/api-key` POST(set/轮换,owner) / GET(list,owner,**只返回 hint**)
- `Config`/`AppState` 加 `master_key`/`mt_master_key`(`MASTER_KEY` 或回退 `webui_api_key`)
- 响应只暴露 `key_hint`,**绝不返回密文/明文**

**M2 未完成(留 M3)**:注入 codex —— 把 team active key 写入 per-team `CODEX_HOME/auth.json` 并按 team 启动 codex 进程。依赖 per-team CODEX_HOME 改造,与 M3 进程池一起做。

### ✅ M3 完成状态(2026-07-16,单机核心)

M3 的**单机多 team 核心**已完成:`TeamCodexManager` + 多租户 thread 路由。37 个 lib 单测全过(无回归,现有功能不受影响)。

已交付:
- `src/multitenant/codex_pool.rs`(`TeamCodexManager`):per-team `CODEX_HOME`(本地目录 `{teams_root}/{team_id}/.codex`)、按需启动 codex(注入 `OPENAI_API_KEY`,来自 M2 `get_active_plain_key` 解密)、复用 `CodexJsonRpcClient` + `process::build_codex_command`、进程表缓存、`evict`/`restart_team`
- handler `/api/mt/threads` POST(create:成员校验→启动 codex→`thread/start`→PG `threads` 元数据双写)/ GET(list:从 PG,team 内共享)、`/api/mt/threads/{id}/turns` POST(team 校验→`turn/start`→更新活跃时间)
- team 校验铁律:thread 操作前 `require_member` / `require_thread_team`(查 PG `threads.team_id` + 成员校验)
- `AppState.mt_team_codex`;main 初始化(`CODEX_TEAMS_HOME` 或回退 `~/.codex-webui-teams`)
- 放开 `process::build_codex_command`(pub(crate))、`jsonrpc::is_closed`(pub)供复用

**M3 未完成(留 M4/M5)**:
- `write_tx` 背压(改 jsonrpc.rs `unbounded→有界` + semaphore,根治 OOM)——`CodexJsonRpcClient` 现有/多租户共用,改动影响面大,留 M5 统一做
- 空闲回收 / 全局进程上限 / 会话粘性细化 / 一致性哈希骨架
- mt thread 的 notification/审批**实时回流前端**还没接(需事件总线,M4 跨节点广播时一起做);目前 turn/start 返回 codex 同步响应,流式事件待 M4

### ✅ M4/M5 部分完成(2026-07-16,基础设施 + 背压)

**M4 路由 + 事件总线基础设施**(trait 抽象,多机预留,单机验证):
- `routing.rs`:`Router` trait + `LocalRouter`(单机) + `ConsistentHash`(一致性哈希,虚拟节点)。5 单测验证分布均匀 / 加减节点迁移最小化
- `event_bus.rs`:`EventBus` trait + `InMemoryEventBus`(单机) + Redis Pub/Sub 预留。3 单测验证 pub/sub / topic 隔离
- 多机实现(`RedisRouter` / `RedisEventBus`)按同 trait,M4 多节点启用;接入 TeamCodexManager(notification 流)留 M4 后期

**M5-A write_tx 背压**(根治 OOM,§7):
- `jsonrpc.rs` write_tx:`unbounded_channel` → 有界 `channel(1024)` + `RpcError::WriteQueueFull` + `try_send`
- 4 处 `send → try_send`(满了 → WriteQueueFull,调用方退避/限流,而非内存堆积)。`CodexJsonRpcClient` 共用,现有 codex / 多租户都受益
- 45 lib 单测全过,无回归

**待办**:M5-B Prometheus 指标、M6-A 限流/防滥用、M4 接入(notification 实时流 + failover + 接入/worker 分离)

---

## 11. codex 文件系统查证结论(关键参考)

查证 `openai/codex`(codex-rs)源码,确认 CODEX_HOME 行为:

1. **codex 不用任何文件锁**(无 flock/fs2/fd-lock/lockfile/单实例锁);`config_lock.rs` 是配置快照非文件锁(命名陷阱)
2. **rollout 是 JSONL 追加写**,每行 `write_all + flush` 但**不 fsync**;路径 `sessions/YYYY/MM/DD/rollout-{date}-{conv_id}.jsonl`;每会话独立文件;同会话两进程操作会损坏
3. **codex 硬编码 SQLite WAL**,而 **SQLite WAL 官方不支持网络文件系统**(需共享内存,跨主机做不到)→ 多机共享 CODEX_HOME 的 .sqlite 会损坏
4. **逃生口**:`CODEX_SQLITE_HOME` 环境变量可把 SQLite 库独立挪走
5. auth.json 非原子写(truncate+write);config.toml 原子写(tempfile+rename)
6. 官方对本地盘/NFS **无声明**

**结论**:CODEX_HOME 不能放 NFS 共享给多机。用方案甲(本地盘 + 同机亲和),team 多进程跑同机共享 CODEX_HOME 是安全的(rollout 独立文件 + 粘性 + SQLite WAL 同机多进程支持)。

---

## 12. 待办与开放项

- socketioxide Redis adapter 成熟度(M4 实施时验证)
- 计费/订阅(M6 视需要)
- 主密钥 `MASTER_KEY` 轮转机制(起步单 env)
- 现有 rusqlite 业务表 → SeaORM 迁移(M3 之后逐步)

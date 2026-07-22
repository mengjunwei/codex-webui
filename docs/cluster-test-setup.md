# 集群本地测试准备文档（3 节点）

> 本文是多副本 HA 集群在**单机本地**起 3 节点（node-a/b/c）做端到端测试的完整准备与验证手册。
> 生产多机部署见 [`cluster-deploy.md`](./cluster-deploy.md)；集群机制见 [`../backend-rs/ARCHITECTURE.md`](../backend-rs/ARCHITECTURE.md) §10。

## 1. 环境准备（WSL Rocky10）

### 1.1 安装 PostgreSQL + Redis

WSL（Rocky Linux 10）默认无 PG/Redis，需手动安装（root）。

**PostgreSQL 16（dnf）**：
```bash
dnf install -y postgresql-server postgresql-contrib
# 初始化
postgresql-setup --initdb
# 配 port 55432 + 监听 0.0.0.0（/var/lib/pgsql/data/postgresql.conf）
#   port = 55432
#   listen_addresses = '0.0.0.0'
# pg_hba.conf 允许 md5（/var/lib/pgsql/data/pg_hba.conf）
#   local   all   all   trust
#   host    all   all   127.0.0.1/32   md5
#   host    all   all   0.0.0.0/0      md5
systemctl enable --now postgresql
sudo -u postgres psql -p 55432 -c "CREATE ROLE codex WITH LOGIN PASSWORD 'codex' SUPERUSER;"
sudo -u postgres createdb -p 55432 -O codex codex
```

**Redis（源码编译，RHEL 10 无 dnf 包 / EPEL 未就绪）**：
```bash
dnf install -y gcc make tar curl
cd /tmp
curl -fsSL -o redis-stable.tar.gz https://download.redis.io/redis-stable.tar.gz
tar xzf redis-stable.tar.gz && cd redis-stable && make -j$(nproc) && make install
# systemd unit（监听 0.0.0.0:6379）
cat > /etc/systemd/system/redis.service <<'UNIT'
[Unit]
Description=Redis
After=network.target
[Service]
Type=simple
ExecStart=/usr/local/bin/redis-server --port 6379 --bind 0.0.0.0 --protected-mode no --daemonize no --dir /var/lib/redis
Restart=always
[Install]
WantedBy=multi-user.target
UNIT
mkdir -p /var/lib/redis
systemctl daemon-reload && systemctl enable --now redis
```

验证：
```bash
PGPASSWORD=codex psql -h 127.0.0.1 -p 55432 -U codex -d codex -tAc 'SELECT version()'
redis-cli -h 127.0.0.1 -p 6379 ping   # PONG
```

### 1.2 Windows ↔ WSL 端口转发（portproxy）

WSL2 NAT 模式下，Windows 访问 WSL 服务需 portproxy（监听 Windows localhost → WSL IP）。**以管理员身份**在 Windows PowerShell：
```powershell
# WSL IP（每次 WSL 重启可能变，需更新）
$wslIp = (wsl -d Rocky10 -- bash -c "ip -4 addr show eth0 | grep -oP 'inet \K[0-9.]+' | head -1")
netsh interface portproxy set v4tov4 listenport=55432 listenaddress=127.0.0.1 connectport=55432 connectaddress=$wslIp
netsh interface portproxy set v4tov4 listenport=56379 listenaddress=127.0.0.1 connectport=6379   connectaddress=$wslIp
# 查看
netsh interface portproxy show all
```
- PG：Windows `127.0.0.1:55432` → WSL `55432`
- Redis：Windows `127.0.0.1:56379` → WSL `6379`

> ⚠️ WSL 重启后 IP 可能变，portproxy 的 connectaddress 需更新（或改用 WSL2 mirrored networking 免转发）。

---

## 2. 集群配置（3 节点）

### 2.1 节点规划

| 节点 | HTTP 端口 | 内网 RPC 端口 | worker_id | CODEX_HOME |
|------|----------|--------------|-----------|------------|
| node-a | 8182 | 8183 | node-a-cluster-test | `<repo>/.superpowers/test-run/cluster/home-a` |
| node-b | 8184 | 8185 | node-b-cluster-test | `<repo>/.superpowers/test-run/cluster/home-b` |
| node-c | 8186 | 8187 | node-c-cluster-test | `<repo>/.superpowers/test-run/cluster/home-c` |

共享：PG（55432）+ Redis（56379）+ `webui_api_key` + `internal_rpc_token`（节点间 RPC 鉴权必须一致）。

### 2.2 config.toml 模板（每节点一份）

```toml
[server]
host = "127.0.0.1"
port = 8182                       # node-a=8182 / node-b=8184 / node-c=8186
log_level = "info,codex_webui=debug"

[server.api]
webui_api_key = "test-webui-api-key-1234567890123456"

[cluster]
internal_rpc_host = "127.0.0.1"
internal_rpc_port = 8183          # = HTTP port + 1
worker_id = "node-a-cluster-test" # 每节点唯一

[database]
host = "127.0.0.1"
port = 55432
user = "codex"
password = "codex"
name = "codex"

[redis]
enable = true
host = "127.0.0.1"
port = 56379

[codex]
bin = "C:/Users/Administrator/AppData/Local/Programs/OpenAI/Codex/bin/codex.exe"  # Windows codex 全路径

[codex.home]
enable = true
path = "D:/code/rust/codex-webui/.superpowers/test-run/cluster/home-a"  # 每节点独立

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"   # 三节点必须一致
internal_hook_token = "hook-token-0123456789abcdef0123456789abcdef"

[quota]
default_turn_quota_hourly = 0
```

> **不写 `[memberlist]` 段** → 默认 enable=false → 走 **Redis 心跳**探活（免 `--features memberlist-backend` 编译）。
> 注意：config struct `deny_unknown_fields`，**不要写 `[process_pool]`** 等未定义段（会启动报错）。

---

## 3. CODEX_HOME 准备（⚠️ 关键）

每节点 CODEX_HOME 必须**独立**（多节点同机各自 spawn codex app-server，共享会冲突）。

### 3.1 只放「认证 / provider 配置」，其余 codex 自动生成

CODEX_HOME 只需预先放 3 个**认证与 provider 配置**文件：

| 文件 | 作用 | 来源 |
|------|------|------|
| `auth.json` | codex 登录认证（access token） | 从已登录的 codex home 复制 |
| `config.toml` | codex provider 配置（model_provider / base_url / bearer_token） | 从已配置的 codex home 复制 |
| `cc-switch-model-catalog.json` | 模型列表（若用 cc-switch 模型目录） | 从已配置的 codex home 复制 |

```bash
# 假设已有可用的 codex 认证 home（如 test-home，曾用 codex CLI 登录过）
for h in home-a home-b home-c; do
  mkdir -p .superpowers/test-run/cluster/$h
  cp test-home/{auth.json,config.toml,cc-switch-model-catalog.json} .superpowers/test-run/cluster/$h/
done
```

### 3.2 ❌ 不要复制的（codex 启动自动生成）

以下都是 codex 运行时自动创建，**预先放反而冲突/脏数据**：

- `sessions/`（rollout 会话文件，每 thread 独立）
- `threads/`（per-thread workspace）
- `goals_1.sqlite` / `logs_2.sqlite` / `memories_1.sqlite` / `state_5.sqlite`（codex 内部状态库）
- `installation_id`（codex 安装标识）
- `skills/` `tmp/`

> 数据库（threads 表 / session_replicas 表）在 **PostgreSQL**（外部共享），由 codex-webui migration 创建，与 CODEX_HOME 无关。

### 3.3 codex config.toml 的 provider 段（示例）

```toml
model_provider = "custom"
model = "mimo-v2.5-pro"
model_reasoning_effort = "high"
disable_response_storage = true
model_catalog_json = "cc-switch-model-catalog.json"

[model_providers.custom]
name = "custom"
wire_api = "responses"
requires_openai_auth = true
base_url = "http://127.0.0.1:15721/v1"          # cc-switch proxy（三节点共享）
experimental_bearer_token = "PROXY_MANAGED"

[hooks.audit]   # ⚠️ 此段会被 codex-webui 启动时按各节点 HTTP port 自动改写,无需手改
type = "http"
url = "http://127.0.0.1:8172/hooks/codex"
auth_header = "X-Hook-Token"
auth_env = "INTERNAL_HOOK_TOKEN"
```

> `cc-switch` 代理需单独启动（监听 15721），三节点共享同一 cc-switch（模型调用）。

---

## 4. 启动步骤

### 4.1 编译
```bash
cargo build --manifest-path backend-rs/Cargo.toml -p codex-webui
```

### 4.2 清旧测试数据（每次重测前）
```bash
# 停旧节点（释放 exe 锁,Windows 改代码重编译前必做）
taskkill /F /IM codex.exe; taskkill /F /IM codex-webui.exe

# 清 DB + Redis + 各节点 sessions（保留 auth/config）
wsl -d Rocky10 -- bash -c "PGPASSWORD=codex psql -h 127.0.0.1 -p 55432 -U codex -d codex -c 'DELETE FROM session_replicas'"
wsl -d Rocky10 -- bash -c "redis-cli -h 127.0.0.1 -p 6379 flushdb"
rm -rf .superpowers/test-run/cluster/home-*/sessions
```

### 4.3 启动 3 节点（各一个后台进程）
```bash
CODEX_WEBUI_CONFIG="<repo>/.superpowers/test-run/cluster/node-a.toml" backend-rs/target/debug/codex-webui.exe > .superpowers/test-run/cluster/log-a.log 2>&1 &
CODEX_WEBUI_CONFIG="<repo>/.superpowers/test-run/cluster/node-b.toml" backend-rs/target/debug/codex-webui.exe > .superpowers/test-run/cluster/log-b.log 2>&1 &
CODEX_WEBUI_CONFIG="<repo>/.superpowers/test-run/cluster/node-c.toml" backend-rs/target/debug/codex-webui.exe > .superpowers/test-run/cluster/log-c.log 2>&1 &
```

### 4.4 确认集群形成（~25s）
```bash
sleep 25
# 端口
for p in 8182 8184 8186; do timeout 2 bash -c "echo >/dev/tcp/127.0.0.1/$p" && echo "$p OPEN"; done
# Redis 心跳注册（应 3 节点）
wsl -d Rocky10 -- bash -c "redis-cli -h 127.0.0.1 -p 6379 smembers cluster:nodes | sort"
# 期望:node-a-cluster-test / node-b-cluster-test / node-c-cluster-test
```

---

## 5. 功能测试矩阵

### 5.1 集群形成 ✅
见 4.4（cluster:nodes = 3 节点）。

### 5.2 per-thread 调度（thread 分散）
```bash
TOKEN=$(curl -s -X POST http://127.0.0.1:8184/api/mt/auth/login -H "Content-Type: application/json" \
  -d '{"email":"test@local.com","password":"Test12345!"}' | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).accessToken))')
# 并发创建 6 thread
for i in 1 2 3 4 5 6; do curl -s -X POST http://127.0.0.1:8184/api/mt/threads \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d '{}' -o /dev/null -w "%{http_code} " & done; wait
# 查 primary 分布（应分散到多节点）
wsl -d Rocky10 -- bash -c "PGPASSWORD=codex psql -h 127.0.0.1 -p 55432 -U codex -d codex -tAc \
  'SELECT primary_node, count(*) FROM session_replicas GROUP BY primary_node'"
```

### 5.3 failover（primary 死 → 副本晋升）
```bash
TID="<某 thread_id>"
# 停其 primary 节点（TaskStop 或 taskkill）
# 等 lease 过期(60s)+晋升周期(15s),约 75-90s
sleep 80
# 查 primary 是否切到 replica
wsl -d Rocky10 -- bash -c "PGPASSWORD=codex psql -h 127.0.0.1 -p 55432 -U codex -d codex -tAc \
  \"SELECT primary_node FROM session_replicas WHERE thread_id='$TID'\""
# 期望:primary 从死节点 → 原 replica 节点
```

### 5.4 rebalance（过热迁移）
```bash
# 使某节点过热(primary 数 > avg×1.5),等 5 分钟节流周期
grep -h "rebalanced thread" .superpowers/test-run/cluster/log-*.log
# 期望:过热节点 thread 迁到低负载节点
```

### 5.5 rollout 复制（primary → replica）
```bash
TID="<thread_id>"; CTID=$(wsl -d Rocky10 -- bash -c "redis-cli -h 127.0.0.1 -p 6379 get codex:tid:$TID")
# primary turn 后等 15s 复制周期
for h in home-a home-b home-c; do
  echo "$h: $(find .superpowers/test-run/cluster/$h/sessions -name "*${CTID}*jsonl" | wc -l)"
done
# 期望:primary + replica 各 1,其他 0
```

### 5.6 sticky + RPC 转发（非主节点转发）
```bash
# thread primary=node-b,从 node-a(非主)发 turn
curl -s -X POST http://127.0.0.1:8182/api/mt/threads/$TID/turns -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" -d '{"input":[{"type":"text","text":"hi"}]}' -o /dev/null -w "%{http_code}\n"
# 期望:200(sticky 命中 node-b,node-a 转发)
```

### 5.7 文件同步（workspace 文件）
```bash
TID="<thread_id>"
# 在 primary home 的 threads/$TID/ 写文件
echo "test" > .superpowers/test-run/cluster/home-b/threads/$TID/sync-test.txt
sleep 20  # file_sync 周期
cat .superpowers/test-run/cluster/home-a/threads/$TID/sync-test.txt  # 期望:replica 收到
```

### 5.8 扩缩容（节点重启 rejoin）
```bash
# 停 node-b → cluster:nodes 移除 node-b;重启 node-b → 自动 rejoin
wsl -d Rocky10 -- bash -c "redis-cli -h 127.0.0.1 -p 6379 smembers cluster:nodes | sort"
```

### 5.9 跨节点 turn 恢复（failover 后会话连续）
```bash
# failover 后(thread 已切新主),直接 turn(无需前端先 resume)
curl -s -X POST http://127.0.0.1:8182/api/mt/threads/$TID/turns ... -w "%{http_code}\n"
# 期望:200(自动 thread/resume 加载会话 → retry turn)
# 日志:turn/start -32600, resuming thread then retry
```

### 5.10 孤儿认领（无主 thread）
```bash
# 造孤儿:primary 设死节点 + replica=NULL
wsl -d Rocky10 -- bash -c "PGPASSWORD=codex psql ... -c \
  \"UPDATE session_replicas SET primary_node='dead-xxx', replica_node=NULL, primary_lease_until=0 WHERE thread_id='$TID'\""
sleep 25
# 期望:最低 alive id 节点认领为 primary + 分配新副本
grep -h "reclaimed orphan thread as primary" .superpowers/test-run/cluster/log-*.log
```

---

## 6. 关键修复记录（per-thread + codex 0.142.5 兼容）

测试中发现并修复的缺陷（供排障参考）：

| commit | 修复 |
|--------|------|
| `ff23ce9` | codex_tid 正向映射（turn/invoke/resume 传 codex_tid 给 codex） |
| `e865054` | codex 0.142.5 忽略外部 threadId：不传 threadId + normalize id + 强制 codex_tid + 反向映射（socket emit） |
| `7b063f8` | 多节点 C1：sys_thread_id 经 RPC body 单独传（不依赖 params.threadId） |
| `007ec71` | rollout 复制：lazy fill active_rollout + turn_start 补登记 + safe_join 建 parent + 移除 canonicalize（Windows 误判） |
| `4283ef2` | turn/start -32600 自动 thread/resume 加载会话 + retry（failover 后会话连续） |

### 6.1 核心约束（排障必读）
- **codex 0.142.5 忽略外部 threadId**：`thread/start` 不能传 threadId（否则会话创建异常，rollout 不写）；系统用预生成 thread_id，codex 调用经 `codex_tid` 映射（正向 thread_id→codex_tid 给 codex 调用；反向 codex_tid→thread_id 给 socket emit/DB）。
- **Windows canonicalize 误判**：`safe_join` 不能用 `canonicalize` + `Path::starts_with`（`\\?\` / UNC / 8.3 短名误判 escapes），改字符串归一化校验。
- **active_rollout 延迟插入**：createThread/turn 时 find_rollout 太早（rollout 延迟未写），`replicate_thread_rollout` 需 lazy 补 find_rollout。
- **failover RPO**：turn 后需等 15s 复制周期再 failover，否则最新 turn 的 rollout 未复制到副本，resume 找不到。

---

## 7. 清理

```bash
# 停所有节点
taskkill /F /IM codex.exe; taskkill /F /IM codex-webui.exe
# 测试数据在 .superpowers/test-run/cluster/(gitignored),无需 git 清理
# PG/Redis 在 WSL(systemd 自启),持久保留,下次直接用
```

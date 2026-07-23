# 集群部署指南（多机 HA）

> 单机部署见 [`../DEPLOY.md`](../DEPLOY.md)；集群机制详见 [`../backend-rs/ARCHITECTURE.md` §10](../backend-rs/ARCHITECTURE.md)。

## 1. 架构概览

Codex WebUI 集群为**对等节点**架构：每个节点同时是 ingress（对外 HTTP/WebSocket）+ worker（codex 进程池 + 内网 RPC），无角色分流。所有节点：

- 共享同一外部 **PostgreSQL**（多租户元数据 + `thread_resume_cache`）
- 共享同一外部 **Redis**（事件总线 / 限流 / rollout 复制 offset / 集群心跳）
- 各自运行 TeamCodexManager（per-team codex 进程池 + LRU 扩缩）

请求路由：LB 打到任一节点；若该 team 的主进程不在本节点，节点间内网 RPC 转发到主节点。主节点故障时副本自动晋升，无需人工干预。

## 2. 前置条件

- 外部 **PostgreSQL 16+**（所有节点连同一个库）
- 外部 **Redis 7+**（所有节点连同一个实例）
- ≥2 台 Linux 机器（同一份 config.toml，仅 `worker_id` / `worker_rpc_url` 不同）
- 前置负载均衡（Nginx / HAProxy / 云 LB），支持 WebSocket（Socket.IO）
- 每台机器已按 [`../DEPLOY.md`](../DEPLOY.md) 跑过 `install.sh`（具备二进制 + config.toml 雏形）

## 3. 每节点 config.toml

在单机 `install.sh` 生成的 `config.toml` 基础上，调整以下段：

```toml
[server]
port = 8172

[cluster]
# ⚠️ 每节点必须唯一（≥16 字节；推荐 hostname 或 k8s pod uid）
worker_id = "node-a-prod-aaaaaaaaaa"
# 本节点对内可达的 RPC 地址（供其他节点转发请求 / 复制 rollout）
worker_rpc_url_enabled = true
worker_rpc_url = "http://10.0.0.1:8173"
# internal_rpc_port 默认 = server.port + 1（即 8173）

# 共享外部 PG（所有节点指向同一实例）
[database]
host = "pg.internal"
port = 5432
user = "codex"
password = "..."
name = "codex"

# ⚠️ 集群必须启用 Redis（所有节点指向同一实例）
[redis]
enable = true
host = "redis.internal"
port = 6379
password = "..."
```

### 可选：Memberlist gossip 探活

默认走 **Redis 心跳**（无需特殊编译，推荐起步用）。若要更强的 gossip 探活（更快收敛、更强去 Redis 依赖），启用 memberlist：

```toml
[memberlist]
enable = true
memberlist_seeds = ["node-a:7946", "node-b:7946", "node-c:7946"]
memberlist_bind = "0.0.0.0:7946"
```

> ⚠️ memberlist 需**编译时**启用 feature：`cargo build --release --features memberlist-backend`。
> 若打包脚本（pack.sh）产出的二进制未带此 feature，保持 `[memberlist] enable = false` 走 Redis 心跳即可，不影响集群功能。

## 4. 部署步骤

每台机器执行：

```bash
# 1. 安装（生成 config.toml，含随机 webui_api_key / token / worker_id）
sudo bash install.sh

# 2. 改 config.toml：worker_id 唯一 + [database]/[redis] 指向共享外部实例 + worker_rpc_url
vi ~/Mnet/config.toml

# 3. 开放端口（供 LB 与其他节点访问）
sudo firewall-cmd --add-port=8172/tcp --permanent   # HTTP（LB 直连）
sudo firewall-cmd --add-port=8173/tcp --permanent   # 内网 RPC（节点间）
# 若启用 memberlist，再开放 7946/tcp

# 4. 启动 + 确认
bash ~/Mnet/bin/start.sh
bash ~/Mnet/bin/start.sh status
```

重复于每个节点。务必保证所有节点的 `[database]` / `[redis]` 指向**同一** PG / Redis 实例，且 `worker_id` 互不相同。

## 5. 负载均衡

LB 前置轮询所有节点。Nginx 示例：

```nginx
upstream codex_webui {
    least_conn;
    server 10.0.0.1:8172;
    server 10.0.0.2:8172;
    server 10.0.0.3:8172;
}

server {
    listen 443 ssl http2;
    server_name codex.example.com;

    location / {
        proxy_pass http://codex_webui;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # Socket.IO WebSocket
    location /socket.io/ {
        proxy_pass http://codex_webui;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 300s;
        # 建议加 ip_hash 或 cookie 粘性：thread → worker 绑定，减少跨节点转发
    }
}
```

> 无 sticky 也能工作（非主节点内部转发到主节点），但 sticky 可降低跨节点 RPC 转发延迟。

## 6. 高可用机制（自动，无需干预）

- **探活**：每 10s 节点向 Redis 注册心跳（`SADD cluster:nodes` + `SET cluster:node:{id} rpc_url EX 30`），stale 节点自动清理
- **路由**：team → worker 一致性哈希 + Redis 路由表；请求到非主节点时经内网 RPC 转发到主节点
- **rollout 复制**：主节点 15s 周期扫描活动会话，按 offset 增量复制到副本（offset 双存储 Redis + 进程内 fallback）
- **副本晋升**：主节点故障（不在 alive 且 lease 过期）→ 副本 `SET NX` 抢占 → 自动晋升为新主
- **孤儿认领**：最低 alive id 节点认领无主 team
- **thread/resume 缓存**：`thread_resume_cache` 表（PG）集群共享，进程重启 / failover 后自愈

机制详见 [`../backend-rs/ARCHITECTURE.md` §10](../backend-rs/ARCHITECTURE.md)。

## 7. 扩缩容 / 滚动升级

- **扩容**：新机器 `install.sh` + config.toml（`worker_id` 唯一 + 同 PG/Redis）+ `start.sh`；启动后自动被 Redis 心跳发现并加入集群
- **缩容**：`start.sh stop` 停止节点；其 team 由其他节点认领 / 晋升。保留 ≥2 节点以保证 failover
- **滚动升级**：逐节点 `start.sh stop` → 替换 `target/` 下二进制 → `start.sh`；节点间确认 `status` 正常后再处理下一台

## 8. 监控与排障

- **指标**：`GET /metrics`（Prometheus 格式，公开端点，供 Prometheus 抓取）
- **日志**：`tail -f ~/Mnet/logs/codex/codex-webui.log`
- **链路追踪**（可选）：config.toml `[otel] enable = true endpoint = "http://otel-collector:4317"`
- **节点存活自查**：`redis-cli SMEMBERS cluster:nodes` 查看注册节点；`redis-cli GET cluster:node:{id}` 查节点 RPC 地址

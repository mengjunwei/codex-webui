# Codex WebUI Linux 部署指南

> 从 WSL Ubuntu 打包 → 部署到任意 Linux 机器的完整流程

## 目录结构

### 打包产物（tar.gz 内容）

```
codex-webui-deploy.tar.gz
├── install.sh                    # 一键安装脚本（目标机器执行）
├── example_provider.md           # cc-switch provider 添加指南
├── bin/
│   └── start.sh                  # 启动/停止/重启脚本
└── target/
    ├── codex-webui               # 后端 Rust 二进制（端口 8172）
    ├── codex                     # Codex CLI（由 codex-webui 启动子进程）
    ├── cc-switch                 # API 代理（端口 15722）
    └── public/                   # 前端 vite build 产物
        ├── index.html
        └── assets/               # ~630 个 JS/CSS chunk
```

### 部署后完整目录（运行时）

```
/home/master/Mnet/
├── config.toml                   # 后端 TOML 配置（install.sh 自动生成）
├── config.toml.example           # 完整字段参考（若打包带入）
├── bin/
│   └── start.sh                  # 启动/停止/重启脚本
├── target/
│   ├── codex-webui               # 后端二进制
│   ├── codex                     # codex CLI
│   ├── cc-switch                 # API 代理
│   └── public/                   # 前端产物
├── logs/                         # 运行时生成
│   ├── codex-webui.log
│   └── codex-webui.pid
├── example_provider.md           # provider 添加指南
├── install.sh                    # 安装脚本
└── pack.sh                       # 打包脚本（WSL 开发机用）
```

## 一、打包（WSL 中执行）

```bash
# 进入 WSL
wsl -d Ubuntu -u master

# 执行打包
cd /home/master/Mnet
bash pack.sh

# 产出
# /home/master/Mnet/codex-webui-deploy.tar.gz  (~126M)
```

`pack.sh` 做了什么：
1. 收集 `target/` 下三个二进制 + `public/` 前端
2. 收集 `bin/start.sh` 和 `install.sh`
3. 打成 tar.gz，暂存目录自动清理

## 二、部署到目标机器

```bash
# 1. 传输
scp /home/master/Mnet/codex-webui-deploy.tar.gz user@目标机器:/tmp/

# 2. 解压 + 安装（需要 root）
ssh user@目标机器
mkdir -p /tmp/mnet-deploy
tar xzf /tmp/codex-webui-deploy.tar.gz -C /tmp/mnet-deploy
sudo bash /tmp/mnet-deploy/install.sh

# 3. 切换用户、编辑配置
su - master
vi ~/Mnet/config.toml     # ⚠️ 必改 [database] 为外部 PG 连接

# 4. 启动
bash ~/Mnet/bin/start.sh
```

`install.sh` 自动完成：
- 创建 `master` 用户（如不存在）
- 配置免密 sudo（`/etc/sudoers.d/master-nopasswd`）
- 部署文件到 `/home/master/Mnet/`
- 生成 `config.toml`（随机 webui_api_key / token / worker_id）
- 设置文件权限（chown master:master）

支持参数：
```bash
sudo bash install.sh                      # 默认 master 用户
sudo bash install.sh --user myuser        # 指定用户
sudo bash install.sh --prefix /opt/mnet   # 指定安装目录
```

## 三、配置说明 (config.toml)

后端**只从 TOML 配置读取**（不读业务环境变量）。`install.sh` 自动生成 `~/Mnet/config.toml`，
其中 `webui_api_key` / `internal_rpc_token` / `internal_hook_token` / `worker_id` 已随机生成。

**⚠️ 启动前必做**：编辑 `[database]` 为你的外部 PostgreSQL 连接：

```bash
vi ~/Mnet/config.toml
```

```toml
[database]
host = "your-pg-host"
port = 5432
user = "codex"
password = "your-password"
name = "codex"
```

> PostgreSQL / Redis 是**外部依赖**，部署脚本不代管。单机可不配 Redis（`[redis]` 默认注释）；
> 集群部署需 `enable = true` 并指向外部 Redis。完整字段参考 `config.toml.example`
> （或仓库 `backend-rs/config.toml.example`）。

**由 start.sh 自动注入的运行时 env**（无需手动设置）：

- `CODEX_WEBUI_CONFIG` → `~/Mnet/config.toml`（后端定位配置）
- `CODEX_BIN` → `/home/master/Mnet/target/codex`
- `OPENAI_BASE_URL` → cc-switch 代理 `http://127.0.0.1:15722/v1`（codex 子进程走代理）
- `OPENAI_API_KEY` → `PROXY_MANAGED`（由 cc-switch 管理实际 key）
- `WEBUI_LOG_DIR` → `~/Mnet/logs/codex`

## 四、启停管理

```bash
bash ~/Mnet/bin/start.sh              # 启动全部
bash ~/Mnet/bin/start.sh stop         # 停止全部
bash ~/Mnet/bin/start.sh restart      # 重启 codex-webui
bash ~/Mnet/bin/start.sh restart-all  # 重启全部
bash ~/Mnet/bin/start.sh status       # 查看状态
bash ~/Mnet/bin/start.sh switch xiaomi # 切换 provider
bash ~/Mnet/bin/start.sh logs         # tail 日志
```

**启动链路**：
```
浏览器 → codex-webui(8172) → codex app-server 子进程
  → cc-switch proxy(15722) → 小米/minimax API
```

## 五、cc-switch Provider 管理

cc-switch 负责代理 Codex 的 API 请求，支持多个 provider 切换。

### 添加 provider

```bash
# 示例：添加小米 mimo
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name xiaomi \
  --id xiaomi \
  --base-url https://token-plan-cn.xiaomimimo.com/v1 \
  --api-key tp-caaopghxs5fkvr09lbqst66qmg9 \
  --model mimo-v2.5-pro \
  --api-format chat
```

### 切换 provider

```bash
# 切换到小米
/home/master/Mnet/target/cc-switch -a codex use xiaomi

# 切换到 minimax
/home/master/Mnet/target/cc-switch -a codex use minimax

# 或通过 start.sh 切换（会自动重启服务）
bash ~/Mnet/bin/start.sh switch xiaomi
```

### 查看 / 删除

```bash
# 列出所有 provider
/home/master/Mnet/target/cc-switch -a codex provider list

# 删除指定 provider
/home/master/Mnet/target/cc-switch -a codex provider delete <id>
```

## 六、编译（如需重新编译二进制）

```bash
# codex-webui（Rust backend）
cd /path/to/codex-webui/backend-rs
cargo build --release
# 产物：target/release/codex-webui

# codex CLI
cd /path/to/codex
cargo build --release
# 产物：target/release/codex

# 编译后拷贝到 Mnet/target/
cp target/release/codex-webui /home/master/Mnet/target/
cp target/release/codex /home/master/Mnet/target/
```

## 七、故障排查

```bash
# 查看日志
tail -f /home/master/Mnet/logs/codex-webui.log

# 检查进程
bash ~/Mnet/bin/start.sh status

# 端口检查
ss -tlnp | grep -E '(8172|15722)'

# 前端探活（/api/status 需多租户 JWT，不能匿名 curl）
curl -sf http://127.0.0.1:8172/ >/dev/null && echo "前端可达"

# 杀残留进程
pkill -x codex-webui
pkill -x codex-app-server
```

## 八、脚本文件清单

| 文件 | 位置 | 说明 |
|------|------|------|
| `pack.sh` | 项目根目录 | WSL 打包脚本 |
| `install.sh` | 项目根目录 | 目标机器安装脚本 |
| `bin/start.sh` | 项目根目录/bin/ | 部署版启动脚本 |
| `start-wsl.sh` | 项目根目录 | 原 WSL 启动脚本（已废弃，改用 bin/start.sh） |
| `e2e-check.sh` | 项目根目录 | 端到端验证脚本 |
| `fix-cc-model.sh` | 项目根目录 | 一次性修复 cc-switch 模型配置 |
| `wsl-master-nopasswd.sh` | 项目根目录 | WSL 专用：mw 用户免密 sudo 到 master |

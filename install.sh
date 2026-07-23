#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Codex WebUI 一键安装脚本
#
# 在目标 Linux 机器上执行，完成以下工作：
#   1. 创建 master 用户（如不存在）
#   2. 配置 master 免密 sudo
#   3. 部署二进制 + 前端 + 脚本到 /home/master/MNet/
#   4. 生成默认 config.toml 配置文件
#   5. 设置文件权限
#
# 用法：
#   sudo bash install.sh                     # 默认安装
#   sudo bash install.sh --user myuser       # 指定用户
#   sudo bash install.sh --prefix /opt/mnet  # 指定安装目录
#
# 要求：root 权限（直接 root 或 sudo）
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── 参数解析 ─────────────────────────────────────────────────────────────────
INSTALL_USER="master"
INSTALL_PREFIX="/home/master/MNet"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user)    INSTALL_USER="$2"; shift 2 ;;
    --prefix)  INSTALL_PREFIX="$2"; shift 2 ;;
    --help|-h)
      echo "用法: sudo bash install.sh [--user <用户>] [--prefix <安装目录>]"
      echo "  --user    安装用户（默认: master）"
      echo "  --prefix  安装目录（默认: /home/master/MNet）"
      exit 0
      ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
done

INSTALL_HOME="$(dirname "$INSTALL_PREFIX")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── 颜色 ─────────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_CYAN='\033[36m'; C_GREEN='\033[32m'; C_YELLOW='\033[33m'
  C_RED='\033[31m'; C_RST='\033[0m'
else
  C_CYAN=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_RST=''
fi
log()  { printf "${C_CYAN}[install]${C_RST} %s\n" "$*"; }
ok()   { printf "${C_GREEN}[   ok ]${C_RST} %s\n" "$*"; }
warn() { printf "${C_YELLOW}[ warn ]${C_RST} %s\n" "$*"; }
err()  { printf "${C_RED}[ fail ]${C_RST} %s\n" "$*" >&2; }

# ── 权限检查 ─────────────────────────────────────────────────────────────────
if [[ $EUID -ne 0 ]]; then
  err "需要 root 权限，请使用 sudo 执行"
  exit 1
fi

# ── 前置依赖检查 ─────────────────────────────────────────────────────────────
check_deps() {
  log "检查系统依赖"
  local missing=()
  for cmd in tar gzip; do
    if command -v "$cmd" >/dev/null 2>&1; then
      ok "$cmd"
    else
      missing+=("$cmd")
    fi
  done
  if [[ ${#missing[@]} -gt 0 ]]; then
    err "缺失依赖: ${missing[*]}"
    err "请先安装: apt-get install -y ${missing[*]}"
    exit 1
  fi
}

# ── 创建用户 ─────────────────────────────────────────────────────────────────
ensure_user() {
  log "检查用户 $INSTALL_USER"
  if id "$INSTALL_USER" >/dev/null 2>&1; then
    ok "用户 $INSTALL_USER 已存在"
  else
    log "创建用户 $INSTALL_USER"
    useradd -m -s /bin/bash "$INSTALL_USER"
    ok "用户 $INSTALL_USER 已创建"
  fi
  INSTALL_HOME="$(eval echo "~${INSTALL_USER}")"
  INSTALL_PREFIX="$INSTALL_HOME/MNet"
}

# ── 配置免密 sudo ──────────────────────────────────────────────────────────
setup_sudo() {
  log "配置 $INSTALL_USER 免密 sudo"
  local sudoers_file="/etc/sudoers.d/${INSTALL_USER}-nopasswd"
  local rule="$INSTALL_USER ALL=(ALL) NOPASSWD:ALL"

  if [[ -f "$sudoers_file" ]] && grep -qF "$rule" "$sudoers_file" 2>/dev/null; then
    ok "免密 sudo 已配置"
    return 0
  fi

  echo "$rule" > "$sudoers_file"
  chmod 0440 "$sudoers_file"

  # 验证语法
  if visudo -cf "$sudoers_file" >/dev/null 2>&1; then
    ok "免密 sudo 已配置 ($sudoers_file)"
  else
    err "sudoers 语法错误，回滚"
    rm -f "$sudoers_file"
    exit 1
  fi
}

# ── 部署文件 ─────────────────────────────────────────────────────────────────
deploy_files() {
  log "部署文件到 $INSTALL_PREFIX"

  # 创建目录结构（含 codex 默认 cwd / workspace 目录）
  mkdir -p "$INSTALL_PREFIX"/{target,bin,logs,data/codex/cwd,data/codex/workspace,logs/codex}

  # 部署二进制
  log "  部署二进制文件"
  for bin in codex-webui codex cc-switch; do
    if [[ -f "$SCRIPT_DIR/target/$bin" ]]; then
      cp "$SCRIPT_DIR/target/$bin" "$INSTALL_PREFIX/target/$bin"
      chmod +x "$INSTALL_PREFIX/target/$bin"
      ok "  $bin"
    else
      warn "  $bin 未找到，跳过"
    fi
  done

  # 部署启动脚本
  if [[ -f "$SCRIPT_DIR/bin/start.sh" ]]; then
    cp "$SCRIPT_DIR/bin/start.sh" "$INSTALL_PREFIX/bin/start.sh"
    chmod +x "$INSTALL_PREFIX/bin/start.sh"
    ok "  bin/start.sh"
  fi

  # 部署 config.toml.example（完整字段参考；打包未带则跳过）
  if [[ -f "$SCRIPT_DIR/config.toml.example" ]]; then
    cp "$SCRIPT_DIR/config.toml.example" "$INSTALL_PREFIX/config.toml.example"
    ok "  config.toml.example"
  fi

  # 创建便捷符号链接
  ln -sf "$INSTALL_PREFIX/bin/start.sh" "$INSTALL_HOME/start.sh" 2>/dev/null || true
}

# ── 生成 config.toml ───────────────────────────────────────────────────────
generate_config() {
  local cfg_file="$INSTALL_PREFIX/config.toml"
  if [[ -f "$cfg_file" ]]; then
    warn "config.toml 已存在，跳过生成（保留现有配置）"
    return 0
  fi

  log "生成默认 config.toml"

  # 随机密钥 / token（满足后端长度校验：webui_api_key≥16 / rpc_token≥32 / hook_token≥32）
  local api_key rpc_token hook_token
  if command -v openssl >/dev/null 2>&1; then
    api_key="$(openssl rand -hex 16)"
    rpc_token="$(openssl rand -hex 32)"
    hook_token="$(openssl rand -hex 32)"
  else
    api_key="$(head -c 32 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 32)"
    rpc_token="$(head -c 64 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 64)"
    hook_token="$(head -c 64 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 64)"
  fi

  # worker_id：主机名（≥16 字节，不足补齐；多机部署每节点必须唯一）
  local worker_id
  worker_id="$(hostname 2>/dev/null || echo codex-webui)"
  while [[ ${#worker_id} -lt 16 ]]; do worker_id="${worker_id}-"; done

  cat > "$cfg_file" <<EOF
# Codex WebUI 配置（后端只读此 TOML，不读业务环境变量）
# 生成时间: $(date '+%Y-%m-%d %H:%M:%S')
# ⚠️ 启动前请修改下方 [database] 为你的外部 PostgreSQL 连接信息。
#    完整字段参考同目录 config.toml.example（若已部署）。

[server]
host = "0.0.0.0"
port = 8172
log_level = "info"

[server.api]
webui_api_key = "$api_key"

[cluster]
worker_id = "$worker_id"

# ⚠️ 改为你的外部 PostgreSQL 连接（部署脚本不代管 PG/Redis）
[database]
host = "127.0.0.1"
port = 5432
user = "codex"
password = "CHANGE_ME"
name = "codex"

# Redis（可选；单机可不启用。集群部署需 enable=true 并指向外部 Redis）
# [redis]
# enable = true
# host = "127.0.0.1"
# port = 6379
# password = "CHANGE_ME"

[codex]
bin = "$INSTALL_PREFIX/target/codex"

[security]
internal_rpc_token = "$rpc_token"
internal_hook_token = "$hook_token"

[process_pool]
max_processes_per_team = 4
max_global_processes = 25
idle_evict_secs = 900
max_concurrent_per_process = 20
process_scale_threshold = 8

[snapshot]
interval_secs = 300

[quota]
default_turn_quota_hourly = 0
EOF

  chmod 600 "$cfg_file"
  ok "config.toml 已生成"
  # 将 key 保存到全局变量供 print_summary 使用
  GENERATED_API_KEY="$api_key"
}

# ── 设置权限 ─────────────────────────────────────────────────────────────────
fix_permissions() {
  log "设置文件权限"
  chown -R "$INSTALL_USER:$INSTALL_USER" "$INSTALL_PREFIX"
  ok "所有文件归属 $INSTALL_USER"
}

# ── 打印摘要 ─────────────────────────────────────────────────────────────────
print_summary() {
  local key_hint=""
  if [[ -n "${GENERATED_API_KEY:-}" ]]; then
    key_hint="$GENERATED_API_KEY"
  else
    key_hint="（config.toml 已存在，grep webui_api_key $INSTALL_PREFIX/config.toml 查看）"
  fi

  cat <<EOF

┌──────────────────────────────────────────────────────────────────────────┐
│  安装完成                                                               │
│                                                                          │
│  安装目录 : $INSTALL_PREFIX                                              │
│  用户     : $INSTALL_USER                                                │
│  二进制   : $INSTALL_PREFIX/target/{codex-webui,codex,cc-switch}         │
│  脚本     : $INSTALL_PREFIX/bin/start.sh                                 │
│  配置     : $INSTALL_PREFIX/config.toml                                  │
│  日志     : $INSTALL_PREFIX/logs/                                        │
│                                                                          │
│  ⚠️ 启动前必做：编辑 [database] 为你的外部 PostgreSQL 连接              │
│     vi $INSTALL_PREFIX/config.toml                                       │
│                                                                          │
│  webui_api_key: $key_hint                                                │
│                                                                          │
│  下一步:                                                                 │
│    1. 改数据库:  vi $INSTALL_PREFIX/config.toml   （[database] 段）      │
│    2. 切换用户:  su - $INSTALL_USER                                      │
│    3. 启动服务:  bash ~/MNet/bin/start.sh                                │
│    4. 查看状态:  bash ~/MNet/bin/start.sh status                         │
└──────────────────────────────────────────────────────────────────────────┘
EOF
}

# ── 主流程 ───────────────────────────────────────────────────────────────────
main() {
  log "========== Codex WebUI 安装 =========="
  check_deps
  ensure_user
  setup_sudo
  deploy_files
  generate_config
  fix_permissions
  print_summary
  ok "安装完成！"
}

main "$@"

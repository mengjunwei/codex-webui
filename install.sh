#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Codex WebUI 一键安装脚本
#
# 在目标 Linux 机器上执行，完成以下工作：
#   1. 创建 master 用户（如不存在）
#   2. 配置 master 免密 sudo
#   3. 部署二进制 + 前端 + 脚本到 /home/master/MNet/
#   4. 生成默认 .env 配置文件
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

  # 部署前端
  if [[ -d "$SCRIPT_DIR/target/public" ]]; then
    log "  部署前端 public/（$(find "$SCRIPT_DIR/target/public" -type f | wc -l) 个文件）"
    cp -r "$SCRIPT_DIR/target/public" "$INSTALL_PREFIX/target/public"
    ok "  public/"
  else
    warn "  public/ 目录未找到，跳过"
  fi

  # 部署启动脚本
  if [[ -f "$SCRIPT_DIR/bin/start.sh" ]]; then
    cp "$SCRIPT_DIR/bin/start.sh" "$INSTALL_PREFIX/bin/start.sh"
    chmod +x "$INSTALL_PREFIX/bin/start.sh"
    ok "  bin/start.sh"
  fi

  # 创建便捷符号链接
  ln -sf "$INSTALL_PREFIX/bin/start.sh" "$INSTALL_HOME/start.sh" 2>/dev/null || true
}

# ── 生成 .env ────────────────────────────────────────────────────────────────
generate_env() {
  local env_file="$INSTALL_PREFIX/.env"
  if [[ -f "$env_file" ]]; then
    warn ".env 已存在，跳过生成（保留现有配置）"
    return 0
  fi

  log "生成默认 .env"

  # 生成随机 WEBUI_API_KEY（32 字符）
  local api_key
  if command -v openssl >/dev/null 2>&1; then
    api_key="$(openssl rand -hex 16)"
  else
    api_key="$(head -c 32 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 32)"
  fi

  cat > "$env_file" <<EOF
# Codex WebUI 配置文件
# 生成时间: $(date '+%Y-%m-%d %H:%M:%S')

# 必填：WebUI API 认证密钥（≥ 16 字符）
WEBUI_API_KEY=$api_key

# 可选：后端监听端口（默认 8172）
PORT=8172

# 可选：日志级别（debug/info/warn/error，默认 info）
LOG_LEVEL=info

# 可选：Codex home 目录（默认 ~/.codex）
# CODEX_HOME=

# 可选：SQLite 数据库路径（默认 CODEX_HOME/codex-webui.sqlite）
# WEBUI_DB_PATH=

# 终端/codex 命令默认工作目录（须位于 WORKSPACE_ROOTS 内且为已存在目录）
DEFAULT_TERMINAL_CWD=$INSTALL_PREFIX/data/codex/cwd

# workspace 根目录（逗号分隔，家目录恒包含）
WORKSPACE_ROOTS=$INSTALL_PREFIX/data/codex/workspace,$INSTALL_PREFIX/data/codex/cwd

# Codex 启动默认配置（仅当 codex config 缺失对应键时写入，不覆盖已有值）
CODEX_DEFAULT_SANDBOX_MODE=danger-full-access
CODEX_DEFAULT_APPROVAL_POLICY=never
EOF

  chmod 600 "$env_file"
  ok ".env 已生成"
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
    key_hint="（已存在，运行 grep WEBUI_API_KEY $INSTALL_PREFIX/.env 查看）"
  fi

  cat <<EOF

┌──────────────────────────────────────────────────────────────────────────┐
│  安装完成                                                               │
│                                                                          │
│  安装目录 : $INSTALL_PREFIX                                              │
│  用户     : $INSTALL_USER                                                │
│  二进制   : $INSTALL_PREFIX/target/{codex-webui,codex,cc-switch}         │
│  脚本     : $INSTALL_PREFIX/bin/start.sh                                 │
│  配置     : $INSTALL_PREFIX/.env                                         │
│  日志     : $INSTALL_PREFIX/logs/                                        │
│                                                                          │
│  WEBUI_API_KEY:                                                          │
│  $key_hint                                                               │
│                                                                          │
│  下一步:                                                                 │
│    1. 编辑配置:  vi $INSTALL_PREFIX/.env                                 │
│    2. 切换用户:  su - $INSTALL_USER                                      │
│    3. 启动服务:  bash ~/MNet/bin/start.sh                                │
│    4. 查看状态:  bash ~/MNet/bin/start.sh status                         │
│                                                                          │
│  后续查看 key:                                                            │
│    grep WEBUI_API_KEY $INSTALL_PREFIX/.env                               │
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
  generate_env
  fix_permissions
  print_summary
  ok "安装完成！"
}

main "$@"

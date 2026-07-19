#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Codex WebUI + cc-switch 一键启动脚本（部署版）
#
# 目录布局（/home/master/MNet）：
#   target/codex-webui          backend-rs release 二进制
#   target/codex                codex CLI 多调用二进制
#   target/cc-switch            cc-switch-cli
#   config.toml                 后端 TOML 配置（webui_api_key / database / ...）
#   logs/                       运行日志 + pid 文件
#
# 链路：
#   浏览器 → codex-webui(8172) → codex app-server 子进程
#     → cc-switch proxy(15722) → 小米/minimax API
#
# 用法：
#   bash bin/start.sh              # 启动全部（cc-switch + codex-webui）
#   bash bin/start.sh stop         # 停止全部
#   bash bin/start.sh restart      # 重启 codex-webui（cc-switch 不动）
#   bash bin/start.sh restart-all  # 重启全部（cc-switch + codex-webui）
#   bash bin/start.sh status       # 查看状态
#   bash bin/start.sh switch xiaomi # 切换 provider（xiaomi/minimax）
#   bash bin/start.sh logs         # tail -f codex-webui 日志
#
# 注意：非 master 用户运行时会自动切换到 master（su - master）
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── 用户校验：必须以 master 运行，否则自动切换 ──────────────────────────────
if [[ "$(whoami)" != "master" ]]; then
  printf '[codex] 当前用户 %s，自动切换到 master 重新执行\n' "$(whoami)"
  SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
  exec su - master -c "bash '${SCRIPT_PATH}' $(printf "'%s' " "$@")"
fi

# ── 路径常量 ─────────────────────────────────────────────────────────────────
# 自动推算 DEPLOY_HOME：脚本所在目录的上级
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_HOME="$(cd "$SCRIPT_DIR/.." && pwd)"

TARGET_DIR="$DEPLOY_HOME/target"
LOG_DIR="$DEPLOY_HOME/logs"
CONFIG_FILE="$DEPLOY_HOME/config.toml"

CODEX_WEBUI_BIN="$TARGET_DIR/codex-webui"
CODEX_BIN="$TARGET_DIR/codex"
CC_SWITCH_BIN="$TARGET_DIR/cc-switch"

# 后端监听端口由 config.toml [server].port 决定；此处仅用于端口探活/日志显示，
# 若改了 config.toml 的 port 请同步修改。
CODEX_WEBUI_PORT=8172
CC_SWITCH_PORT=15722

WEBUI_LOG_DIR="$LOG_DIR/codex"
CODEX_WEBUI_PID_FILE="$LOG_DIR/codex-webui.pid"
CODEX_WEBUI_LOG="$WEBUI_LOG_DIR/codex-webui.log"

# cc-switch daemon 由 cc-switch 自己管理 pid，不额外存 pidfile

# ── 颜色 ─────────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_CYAN='\033[36m'; C_GREEN='\033[32m'; C_YELLOW='\033[33m'
  C_RED='\033[31m'; C_RST='\033[0m'
else
  C_CYAN=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_RST=''
fi
log()  { printf "${C_CYAN}[codex]${C_RST} %s\n" "$*"; }
ok()   { printf "${C_GREEN}[  ok ]${C_RST} %s\n" "$*"; }
warn() { printf "${C_YELLOW}[warn]${C_RST} %s\n" "$*"; }
err()  { printf "${C_RED}[fail]${C_RST} %s\n" "$*" >&2; }

have() { command -v "$1" >/dev/null 2>&1; }

# ── 工具函数 ─────────────────────────────────────────────────────────────────
pid_alive() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

port_pid() {
  local port="$1"
  local result=""
  if have ss; then
    result="$(ss -ltnp 2>/dev/null | awk -v p=":${port}$" '$4 ~ p {print $0}' \
      | grep -oE 'pid=[0-9]+' | cut -d= -f2 | head -1 || true)"
  elif have lsof; then
    result="$(lsof -ti :"$port" 2>/dev/null | head -1 || true)"
  fi
  echo "$result"
}

# ── 停止 codex-webui（不动 cc-switch）────────────────────────────────────────
stop_codex_webui() {
  log "停止 codex-webui"
  # 1. pidfile
  if [[ -f "$CODEX_WEBUI_PID_FILE" ]]; then
    local pid
    pid="$(cat "$CODEX_WEBUI_PID_FILE" 2>/dev/null || true)"
    if pid_alive "$pid"; then
      log "  kill pid=$pid"
      kill "$pid" 2>/dev/null || true
      for _ in 1 2 3 4 5; do pid_alive "$pid" || break; sleep 1; done
      if pid_alive "$pid"; then
        warn "  强杀 pid=$pid"
        kill -9 "$pid" 2>/dev/null || true
      fi
    fi
    rm -f "$CODEX_WEBUI_PID_FILE"
  fi
  # 2. 端口兜底
  local p
  p="$(port_pid "$CODEX_WEBUI_PORT")"
  if [[ -n "$p" ]]; then
    log "  端口 $CODEX_WEBUI_PORT 被 pid=$p 占用，强杀"
    kill -9 "$p" 2>/dev/null || true
  fi
  # 3. codex app-server 子进程残留
  pids="$(pgrep -x codex-app-server 2>/dev/null || true)"
  [[ -n "$pids" ]] && { log "  杀残留 codex-app-server: $pids"; kill -9 $pids 2>/dev/null || true; }
  ok "codex-webui 已停止"
}

# ── 停止 cc-switch ──────────────────────────────────────────────────────────
stop_cc_switch() {
  log "停止 cc-switch daemon + worker"
  "$CC_SWITCH_BIN" daemon stop -a codex 2>/dev/null || true
  sleep 1
  # 端口兜底
  local p
  p="$(port_pid "$CC_SWITCH_PORT")"
  if [[ -n "$p" ]]; then
    log "  端口 $CC_SWITCH_PORT 被 pid=$p 占用，强杀"
    kill -9 "$p" 2>/dev/null || true
  fi
  # 清 stale socket
  rm -f "/run/user/$(id -u)/cc-switch/daemon.sock" \
        "/run/user/$(id -u)/cc-switch/daemon.pid" 2>/dev/null || true
  ok "cc-switch 已停止"
}

# ── 启动 cc-switch ──────────────────────────────────────────────────────────
start_cc_switch() {
  log "启动 cc-switch（daemon + proxy，端口 $CC_SWITCH_PORT）"

  # 如果已在跑，跳过
  if pid_alive "$(port_pid "$CC_SWITCH_PORT" 2>/dev/null)"; then
    ok "cc-switch 已在运行，跳过"
    return 0
  fi

  # 清 stale socket
  rm -f "/run/user/$(id -u)/cc-switch/daemon.sock" \
        "/run/user/$(id -u)/cc-switch/daemon.pid" 2>/dev/null || true

  # proxy enable 会自动启 daemon + spawn worker
  if ! "$CC_SWITCH_BIN" proxy enable -a codex 2>&1 | tail -1; then
    err "cc-switch proxy enable 失败"
    return 1
  fi

  # 等端口就绪
  for i in 1 2 3 4 5; do
    sleep 1
    if pid_alive "$(port_pid "$CC_SWITCH_PORT" 2>/dev/null)"; then
      ok "cc-switch 端口 $CC_SWITCH_PORT 就绪（${i}s）"
      return 0
    fi
  done
  err "cc-switch 端口 $CC_SWITCH_PORT 未就绪"
  return 1
}

# ── 体检 ─────────────────────────────────────────────────────────────────────
check_prereqs() {
  log "前置条件检查"
  local failed=0

  # 二进制
  for f in "$CODEX_WEBUI_BIN" "$CODEX_BIN" "$CC_SWITCH_BIN"; do
    if [[ -x "$f" ]]; then
      ok "$(basename "$f")"
    else
      err "缺失：$f"
      failed=1
    fi
  done

  # config.toml（后端业务配置唯一入口；不读业务环境变量）
  if [[ -f "$CONFIG_FILE" ]]; then
    ok "config.toml"
  else
    err "缺失：$CONFIG_FILE（运行 install.sh 生成，或参考 config.toml.example 手动创建）"
    failed=1
  fi

  if (( failed > 0 )); then
    err "前置条件未满足，中止"
    exit 1
  fi
}

# ── 启动 codex-webui ────────────────────────────────────────────────────────
start_codex_webui() {
  log "启动 codex-webui（config=$CONFIG_FILE，端口 $CODEX_WEBUI_PORT）"

  # 后端定位 config.toml（业务配置只从 TOML 读，不读业务 env）
  export CODEX_WEBUI_CONFIG="$CONFIG_FILE"

  # 运行时 env（后端辅助 + codex 子进程继承父环境）：
  # 关键：让 codex app-server 子进程走 cc-switch proxy
  export OPENAI_BASE_URL="http://127.0.0.1:${CC_SWITCH_PORT}/v1"
  export OPENAI_API_KEY="PROXY_MANAGED"
  # 指定 codex 二进制路径（target 目录下；后端 codex 子进程 + logs 读取）
  export CODEX_BIN="$CODEX_BIN"
  # 日志统一目录（backend-rs tracing/jsonrpc 日志 + stdout 重定向）
  export WEBUI_LOG_DIR="$WEBUI_LOG_DIR"

  : > "$CODEX_WEBUI_LOG"
  nohup "$CODEX_WEBUI_BIN" >>"$CODEX_WEBUI_LOG" 2>&1 &
  local pid=$!
  echo "$pid" >"$CODEX_WEBUI_PID_FILE"
  ok "codex-webui pid=$pid"

  # 等端口就绪
  for i in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    if (echo > "/dev/tcp/127.0.0.1/${CODEX_WEBUI_PORT}") 2>/dev/null; then
      ok "端口 $CODEX_WEBUI_PORT 就绪（${i}s）"
      # 注：/api/status、/api/_ping 已受多租户 JWT 保护，不再用 API key 匿名探活；
      # TCP 端口就绪即视为启动成功。
      return 0
    fi
    if ! pid_alive "$pid"; then
      err "codex-webui 进程已退出"
      tail -n 30 "$CODEX_WEBUI_LOG" >&2 || true
      return 1
    fi
  done
  err "codex-webui 端口 $CODEX_WEBUI_PORT 未就绪"
  tail -n 30 "$CODEX_WEBUI_LOG" >&2 || true
  return 1
}

# ── 状态 ─────────────────────────────────────────────────────────────────────
show_status() {
  echo ""
  log "=== 服务状态 ==="

  # codex-webui
  local wp
  wp="$(cat "$CODEX_WEBUI_PID_FILE" 2>/dev/null || true)"
  if pid_alive "$wp"; then
    ok "codex-webui  pid=$wp  端口=$CODEX_WEBUI_PORT"
  else
    warn "codex-webui  未运行"
  fi

  # cc-switch daemon
  if "$CC_SWITCH_BIN" daemon status -a codex 2>/dev/null | grep -q "running"; then
    local cp
    cp="$(port_pid "$CC_SWITCH_PORT")"
    ok "cc-switch    pid=$cp  端口=$CC_SWITCH_PORT"
  else
    warn "cc-switch    未运行"
  fi

  # 当前 provider
  local current
  current="$("$CC_SWITCH_BIN" provider list -a codex 2>/dev/null | grep '✓' | awk '{print $4}' || true)"
  [[ -n "$current" ]] && ok "当前 provider: $current"

  echo ""
}

# ── 切换 provider ────────────────────────────────────────────────────────────
switch_provider() {
  local target="${1:-}"
  if [[ -z "$target" ]]; then
    err "用法：bash $DEPLOY_HOME/bin/start.sh switch <provider-id>"
    err "可用 provider："
    "$CC_SWITCH_BIN" provider list -a codex 2>&1 | head -15
    exit 1
  fi

  log "切换 provider → $target"
  "$CC_SWITCH_BIN" use -a codex "$target" 2>&1 | tail -3

  # 重启 cc-switch 让新 provider 生效
  stop_cc_switch
  sleep 1
  start_cc_switch

  # 重启 codex-webui 让子进程读新 config
  stop_codex_webui
  sleep 1
  start_codex_webui

  ok "已切换到 $target"
}

# ── 日志 ─────────────────────────────────────────────────────────────────────
show_logs() {
  log "tail -f $CODEX_WEBUI_LOG"
  tail -f "$CODEX_WEBUI_LOG"
}

# ── 打印摘要 ─────────────────────────────────────────────────────────────────
print_summary() {
  local current
  current="$("$CC_SWITCH_BIN" provider list -a codex 2>/dev/null | grep '✓' | awk '{print $4}' || true)"

  cat <<EOF

┌──────────────────────────────────────────────────────────────────────────┐
│  启动完成                                                               │
│                                                                          │
│  前端    : http://127.0.0.1:${CODEX_WEBUI_PORT}/                         │
│  Proxy   : http://127.0.0.1:${CC_SWITCH_PORT}/                          │
│  Provider: ${current:-unknown}                                           │
│                                                                          │
│  日志    : tail -f $CODEX_WEBUI_LOG                                      │
│  停止    : bash $DEPLOY_HOME/bin/start.sh stop                           │
│  切换    : bash $DEPLOY_HOME/bin/start.sh switch xiaomi                  │
└──────────────────────────────────────────────────────────────────────────┘
EOF
}

# ── 主入口 ───────────────────────────────────────────────────────────────────
mkdir -p "$LOG_DIR" "$WEBUI_LOG_DIR" "$DEPLOY_HOME/data/codex/cwd" "$DEPLOY_HOME/data/codex/workspace"

case "${1:-start}" in
  start)
    check_prereqs
    start_cc_switch
    start_codex_webui
    print_summary
    ;;
  stop)
    stop_codex_webui
    stop_cc_switch
    ;;
  restart)
    # 只重启 codex-webui，cc-switch 是长驻服务不动
    stop_codex_webui
    sleep 1
    check_prereqs
    start_cc_switch
    start_codex_webui
    print_summary
    ;;
  restart-all)
    # 同时重启 cc-switch + codex-webui
    stop_codex_webui
    stop_cc_switch
    sleep 1
    check_prereqs
    start_cc_switch
    start_codex_webui
    print_summary
    ;;
  status)
    show_status
    ;;
  switch)
    switch_provider "${2:-}"
    ;;
  logs)
    show_logs
    ;;
  *)
    echo "用法：$0 {start|stop|restart|restart-all|status|switch <id>|logs}" >&2
    exit 2
    ;;
esac

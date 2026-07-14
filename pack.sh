#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Codex WebUI 打包脚本
#
# 在 WSL 中运行，收集编译产物并打成可平移的 tar.gz 安装包
# 生成的包可以在任意 Linux 机器上用 install.sh 一键部署
#
# 用法：
#   bash pack.sh                        # 默认从 /home/master/MNet 收集
#   bash pack.sh --source /path/to/src  # 指定源目录
#
# 产出：
#   /home/master/MNet/codex-webui-deploy.tar.gz
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

# ── 参数解析 ─────────────────────────────────────────────────────────────────
DEPLOY_HOME="/home/master/MNet"
SOURCE_DIR="$DEPLOY_HOME"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)  SOURCE_DIR="$2"; shift 2 ;;
    --help|-h)
      echo "用法: bash pack.sh [--source <源目录>]"
      echo "  --source  编译产物所在目录（默认: /home/master/MNet）"
      exit 0
      ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_FILE="$DEPLOY_HOME/codex-webui-deploy.tar.gz"

# ── 颜色 ─────────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  C_CYAN='\033[36m'; C_GREEN='\033[32m'; C_RED='\033[31m'; C_RST='\033[0m'
else
  C_CYAN=''; C_GREEN=''; C_RED=''; C_RST=''
fi
log()  { printf "${C_CYAN}[pack]${C_RST} %s\n" "$*"; }
ok()   { printf "${C_GREEN}[ ok ]${C_RST} %s\n" "$*"; }
err()  { printf "${C_RED}[fail]${C_RST} %s\n" "$*" >&2; }

# ── 临时目录 ─────────────────────────────────────────────────────────────────
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
log "暂存目录: $STAGING"

# ── 收集二进制 ───────────────────────────────────────────────────────────────
log "收集二进制文件"
mkdir -p "$STAGING/target"

count=0
for bin in codex-webui codex cc-switch; do
  src="$SOURCE_DIR/target/$bin"
  if [[ -f "$src" ]]; then
    cp "$src" "$STAGING/target/$bin"
    chmod +x "$STAGING/target/$bin"
    ok "  $bin ($(du -h "$src" | cut -f1))"
    count=$((count + 1))
  else
    err "  $bin 未找到: $src"
  fi
done

if [[ $count -eq 0 ]]; then
  err "没有任何二进制文件，中止"
  exit 1
fi

# ── 前端 ────────────────────────────────────────────────────────────────────
# 前端产物已在编译期嵌入 codex-webui 二进制（rust-embed），无需单独收集。

# ── 收集启动脚本 ─────────────────────────────────────────────────────────────
log "收集启动脚本"
mkdir -p "$STAGING/bin"

if [[ -f "$SCRIPT_DIR/bin/start.sh" ]]; then
  cp "$SCRIPT_DIR/bin/start.sh" "$STAGING/bin/start.sh"
  chmod +x "$STAGING/bin/start.sh"
  ok "  bin/start.sh"
else
  err "  bin/start.sh 未找到: $SCRIPT_DIR/bin/start.sh"
  exit 1
fi

# ── 收集安装脚本 ─────────────────────────────────────────────────────────────
log "收集安装脚本"
if [[ -f "$SCRIPT_DIR/install.sh" ]]; then
  cp "$SCRIPT_DIR/install.sh" "$STAGING/install.sh"
  chmod +x "$STAGING/install.sh"
  ok "  install.sh"
else
  err "  install.sh 未找到: $SCRIPT_DIR/install.sh"
  exit 1
fi

# ── 收集文档 ─────────────────────────────────────────────────────────────────
log "收集文档"
for doc in example_provider.md DEPLOY.md; do
  if [[ -f "$SCRIPT_DIR/$doc" ]]; then
    cp "$SCRIPT_DIR/$doc" "$STAGING/$doc"
    ok "  $doc"
  fi
done

# ── 打包 ─────────────────────────────────────────────────────────────────────
log "打包 → $OUTPUT_FILE"
tar czf "$OUTPUT_FILE" -C "$STAGING" .

size="$(du -h "$OUTPUT_FILE" | cut -f1)"
file_count="$(tar tzf "$OUTPUT_FILE" | wc -l)"

ok "打包完成"
echo ""
echo "  文件: $OUTPUT_FILE"
echo "  大小: $size"
echo "  文件数: $file_count"
echo ""
echo "  部署到目标机器:"
echo "    scp $OUTPUT_FILE user@target:/tmp/"
echo "    ssh user@target 'cd /tmp && tar xzf codex-webui-deploy.tar.gz -C /tmp/mnet-deploy && sudo bash /tmp/mnet-deploy/install.sh'"

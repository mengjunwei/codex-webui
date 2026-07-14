# ── 阶段 1：构建 Rust 后端 ────────────────────────────────────────────
FROM rust:1-bookworm AS rust-builder

WORKDIR /app
COPY backend-rs/Cargo.toml backend-rs/Cargo.lock ./
# 先创建一个占位 main.rs 以缓存依赖编译（依赖不变时后续构建直接命中缓存）。
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY backend-rs/src ./src
# 前端构建产物（rust-embed 在编译期嵌入二进制，必须在此阶段就位）。
COPY --from=frontend-builder /app/backend-rs/public ./public
# 触碰 main.rs，让 cargo 重新编译真正的二进制（而不只是依赖）。
RUN touch src/main.rs && cargo build --release

# ── 阶段 2：构建前端 ──────────────────────────────────────────────────
FROM node:22-bookworm-slim AS frontend-builder

RUN npm install -g pnpm@10
WORKDIR /app/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# ── 阶段 3：运行时镜像 ────────────────────────────────────────────────
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV HOME=/root

# codex CLI（Node.js 工具）所需的系统依赖。
# Rust 二进制本身只需要 libc + libssl，无需额外运行时。
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    bash \
    tar \
    gzip \
    ripgrep \
    fd-find \
    jq \
    less \
    file \
    openssh-client \
    p7zip-full \
    unzip \
    nodejs \
    npm \
 && rm -rf /var/lib/apt/lists/*

# 安装 codex CLI
ARG CODEX_CLI_VERSION=0.142.5
RUN npm install -g @openai/codex@${CODEX_CLI_VERSION}

# 创建应用目录
RUN mkdir -p /root/.codex /workspaces /app/logs

# 拷贝 Rust 二进制（单一自包含可执行文件 —— 后端本身不依赖 Node 运行时；
# 只有 codex CLI 需要 Node.js）。
COPY --from=rust-builder /app/target/release/codex-webui /usr/local/bin/codex-webui

# 前端资源已在编译期由 rust-embed 嵌入二进制，运行时无需额外拷贝。

WORKDIR /app

EXPOSE 8172

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -sf -H "Authorization: Bearer ${WEBUI_API_KEY}" http://localhost:8172/api/status || exit 1

CMD ["codex-webui"]

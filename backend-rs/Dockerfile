# ── Stage 1: Build Rust backend ────────────────────────────────────────
FROM rust:1-bookworm AS rust-builder

WORKDIR /app
COPY backend-rs/Cargo.toml backend-rs/Cargo.lock ./
# Cache dependency compilation by creating a dummy main first.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY backend-rs/src ./src
# Touch main.rs so cargo knows to rebuild the binary (not just deps).
RUN touch src/main.rs && cargo build --release

# ── Stage 2: Build frontend ───────────────────────────────────────────
FROM node:22-bookworm-slim AS frontend-builder

RUN npm install -g pnpm@10
WORKDIR /app/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# ── Stage 3: Runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV HOME=/root

# System dependencies needed by codex CLI (Node.js tool).
# The Rust binary itself needs nothing beyond libc + libssl.
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

# Install codex CLI
ARG CODEX_CLI_VERSION=0.142.5
RUN npm install -g @openai/codex@${CODEX_CLI_VERSION}

# Create app directories
RUN mkdir -p /root/.codex /workspaces /app/logs

# Copy the Rust binary (single self-contained executable — no Node runtime
# for the backend itself; only the codex CLI needs Node.js).
COPY --from=rust-builder /app/target/release/codex-webui /usr/local/bin/codex-webui

# Copy the frontend build output (served as static files by the Rust backend).
COPY --from=frontend-builder /app/public /app/public

WORKDIR /app

EXPOSE 8172

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD curl -sf http://localhost:8172/ || exit 1

CMD ["codex-webui"]

# ── Stage 1: Frontend build ──────────────────────────────────────────
FROM node:22-bookworm-slim AS frontend-builder
RUN corepack enable && corepack prepare pnpm@latest --activate
WORKDIR /app/web
COPY web/package.json web/pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

# ── Stage 2: Backend build ───────────────────────────────────────────
FROM node:22-bookworm-slim AS backend-builder
# node-pty requires native compilation tools
RUN apt-get update \
  && apt-get install -y --no-install-recommends python3 make g++ \
  && rm -rf /var/lib/apt/lists/*
RUN corepack enable && corepack prepare pnpm@latest --activate
# Install pinned Codex CLI for schema generation during build
ARG CODEX_CLI_VERSION=0.123.0
RUN npm install -g @openai/codex@${CODEX_CLI_VERSION}
WORKDIR /app
COPY package.json pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile
COPY src/ ./src/
COPY tsconfig*.json nest-cli.json ./
COPY --from=frontend-builder /app/public ./public/
RUN pnpm build

# ── Stage 3: Runtime ─────────────────────────────────────────────────
FROM debian:trixie-slim AS runtime
# Runtime dependencies: git, bash (terminal), node-pty native rebuild deps
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
    procps \
    bubblewrap \
    build-essential \
    pkg-config \
    python3 \
    make \
    g++ \
    libssl-dev \
    zlib1g-dev \
    libbz2-dev \
    libreadline-dev \
    libsqlite3-dev \
    libffi-dev \
    liblzma-dev \
    tk-dev \
    uuid-dev \
    xz-utils \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://mise.run | sh

RUN grep -q 'mise activate bash' /root/.bashrc 2>/dev/null || \
    printf '\n# mise\nexport PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"\neval "$(mise activate bash)"\n' >> /root/.bashrc

RUN mise use -g node@22 uv@latest python@3.14

RUN node --version \
 && npm --version \
 && uv --version \
 && python --version \
 && mise --version

RUN npm install -g \
    @openai/codex@latest \
    mcp-safe-proxy \
    mcp-remote

# Install pinned Codex CLI version (must match builder for schema compat)
ARG CODEX_CLI_VERSION=0.123.0
RUN npm install -g @openai/codex@${CODEX_CLI_VERSION}

WORKDIR /app
ENV NODE_ENV=production

# Copy package manifests and install production dependencies
RUN corepack enable && corepack prepare pnpm@latest --activate
COPY package.json pnpm-lock.yaml* ./
RUN pnpm install --frozen-lockfile --prod
# Rebuild native addons for this runtime environment
RUN npx --yes node-gyp rebuild --directory=node_modules/node-pty || true \
  && npx --yes node-gyp rebuild --directory=node_modules/better-sqlite3 || true

# Copy built assets and drizzle migrations
COPY --from=backend-builder /app/dist ./dist/
COPY --from=backend-builder /app/public ./public/
COPY drizzle/ ./drizzle/

# Create volume mount points and set ownership for non-root user
RUN mkdir -p /workspaces /codex-home /app/logs \
  && chown -R node:node /workspaces /codex-home /app/logs /app
USER node

EXPOSE 8172
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
  CMD curl -sf -H "Authorization: Bearer ${WEBUI_API_KEY}" http://localhost:8172/api/status || exit 1

CMD ["node", "dist/main.js"]

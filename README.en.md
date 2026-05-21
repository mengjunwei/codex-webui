# Codex WebUI

[![GHCR](https://img.shields.io/badge/GHCR-codex--webui-blue?logo=github)](https://github.com/LimLLL/codex-webui/pkgs/container/codex-webui)
[![Docker](https://img.shields.io/badge/docker-multi--arch-brightgreen?logo=docker)](./Dockerfile)

A web frontend for [OpenAI Codex CLI](https://github.com/openai/codex). It brings the CLI experience into the browser with multi-thread concurrency, a file manager, a shared terminal, and plugin management.

The backend (NestJS) talks to `codex app-server` over stdio JSON-RPC and pushes real-time events to a React frontend via Socket.IO.

[简体中文](./README.md)

![Main UI](./images/main-en.png)

## Features

**Chat & Threads**
- Run multiple threads concurrently, grouped by workspace
- Archive, fork, rollback, rename threads
- Markdown rendering + Shiki syntax highlighting
- `@` file mentions, image paste
- Steer/stop running turns

**Approval Flow**
- In-page cards for command execution and file change approvals
- Security policy switching (sandbox levels)
- Multi-device CAS conflict prevention

**File Management**

![File Management](./images/sidebar-file-en.png)

- Tree browser with drag-and-drop (dnd-kit)
- Monaco code viewer
- Git diff split view (@git-diff-view)
- Upload / download / rename / copy / move / mkdir

**Terminal**

![Terminal](./images/sidebar-terminal-en.png)

- Multi-tab shared terminal (node-pty + xterm.js)
- Reconnect with no output loss
- Headless VT replay

**Integrations & Plugins**

![Integrations](./images/side-integration-en.png)

**Other**
- JWT + API Key authentication
- Plugin / MCP server management
- Dark / light theme, i18n (en / zh-CN)
- Responsive layout (mobile + tablet)
- Docker deployment

## Tech Stack

```
Browser
  React 19 · Vite 8 · TanStack (Router + Query + Virtual)
  Zustand · Socket.IO Client · Monaco Editor · xterm.js
  Tailwind CSS 4 · shadcn/ui · Framer Motion · dnd-kit
     ↕  REST + WebSocket
Server
  NestJS 11 · Fastify 5 · Socket.IO · node-pty
  SQLite (better-sqlite3 + Drizzle ORM) · Pino
     ↕  stdio JSON-RPC
  codex app-server (child process)
```

## Quick Start

### Prerequisites

- Node.js >= 20
- pnpm >= 9
- [Codex CLI](https://github.com/openai/codex) installed and available in PATH

### Docker Deployment (Recommended)

Pull the pre-built multi-arch image from GHCR:

```bash
# Create .env
cat <<EOF > .env
WEBUI_API_KEY=your-secret-key
OPENAI_API_KEY=sk-xxx
EOF

# Start (auto-pulls multi-arch image)
docker compose up -d
```

Or run directly:

```bash
docker run -d --name codex-webui \
  -p 8172:8172 \
  -e WEBUI_API_KEY=your-secret-key \
  -e OPENAI_API_KEY=sk-xxx \
  -v codex_root:/root \
  -v codex_workspaces:/workspaces \
  ghcr.io/limlll/codex-webui:latest
```

The app runs at `http://localhost:8172`.

> The `/root` volume persists codex/claude/MCP configs and the runtime toolchain. The built-in seed is automatically extracted on first start.

### Local Development

```bash
git clone https://github.com/LimLLL/codex-webui.git
cd codex-webui
pnpm install

cp .env.example .env
# Edit .env — at minimum set WEBUI_API_KEY

# Start the backend (default port 8172)
pnpm start:dev

# In another terminal, start the frontend (port 5173, proxies to backend)
cd web && pnpm dev
```

Open `http://localhost:5173`.

## Environment Variables

| Variable                       | Required | Default                        | Description                                    |
| ------------------------------ | :------: | ------------------------------ | ---------------------------------------------- |
| `WEBUI_API_KEY`                |   Yes    | —                              | Login key; also derives JWT signing secret      |
| `PORT`                         |    No    | `8172`                         | Backend listen port                            |
| `OPENAI_API_KEY`               |    No    | —                              | OpenAI API key used by Codex                   |
| `CODEX_BIN`                    |    No    | `codex`                        | Path to codex CLI binary                       |
| `CODEX_HOME`                   |    No    | `~/.codex`                     | Codex home directory                           |
| `LOG_LEVEL`                    |    No    | `info`                         | Pino log level                                 |
| `WEBUI_DB_PATH`                |    No    | `CODEX_HOME/codex-webui.sqlite`| SQLite database path                           |

### Runtime Settings

`security.workspaceRoots`, `files.uploadMaxBytes`, `terminal.defaultCwd`, `terminal.maxSessions`, `terminal.graceMs`, and `terminal.scrollback` now live in SQLite runtime settings and can be changed from Settings or `/api/settings`; the legacy env vars still work as fallbacks when no DB value is set.
Docker Compose keeps `WORKSPACE_ROOTS=/workspaces` as a bootstrap fallback for the mounted `/workspaces` volume on first startup.

## Project Structure

```
├── src/                  # NestJS backend
│   ├── codex/            # Process manager, JSON-RPC client
│   ├── threads/          # Thread CRUD, WebSocket gateway
│   ├── files/            # File ops, path security
│   ├── terminal/         # Multi-tab terminal (node-pty)
│   ├── auth/             # JWT + API Key auth
│   ├── database/         # SQLite + Drizzle ORM
│   └── ...               # Other modules
├── web/                  # React frontend
│   └── src/
│       ├── routes/       # TanStack Router pages
│       ├── components/   # UI components
│       ├── stores/       # Zustand state management
│       ├── hooks/        # Custom hooks
│       └── generated/    # Hey API SDK (auto-generated)
├── Dockerfile            # Multi-stage build + seed root
└── docker-compose.yml
```

## Commands

```bash
pnpm start:dev          # Backend dev server
pnpm build              # Compile backend
pnpm test               # Run tests
pnpm lint               # ESLint
pnpm db:generate        # Generate DB migration
pnpm db:migrate         # Run migrations
cd web && pnpm dev      # Frontend dev server
cd web && pnpm build    # Build frontend (outputs to public/)
```

## License

[AGPL-3.0](./LICENSE)

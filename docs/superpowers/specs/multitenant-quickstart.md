# 多租户平台 快速验证(M1 + M2)

当前已完成 M1(用户体系/多租户/认证/team)与 M2(BYOK key 管理)。本指南让你本地跑通并验证。
设计全貌见 `2026-07-16-multitenant-platform-design.md`。

## 前置

- **PostgreSQL**(M1/M2 数据层;未配置则多租户功能禁用,现有功能不受影响)

```bash
docker run -d --name mt-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=codexmt \
  -p 5432:5432 postgres:16
```

## 配置 `.env`(backend-rs/.env 或环境变量)

```env
WEBUI_API_KEY=change-me-to-a-long-random-string-32+chars
DATABASE_URL=postgres://postgres:postgres@localhost:5432/codexmt
MASTER_KEY=another-long-random-string-32+chars
CODEX_BIN=codex
HOST=0.0.0.0
PORT=8172
```

> `MASTER_KEY` 未设时回退用 `WEBUI_API_KEY`(加密 team 的 OpenAI key)。生产务必设独立的 `MASTER_KEY`。

## 启动

```bash
cargo run --manifest-path backend-rs/Cargo.toml
```

日志出现 `multitenant postgres ready` 即自动建表 + 迁移完成。

## 验证(curl)

```bash
BASE=http://localhost:8172

# 1) 注册(返回 user + accessToken + refreshToken)
curl -s -X $BASE/api/mt/auth/register -H 'Content-Type: application/json' \
  -d '{"email":"alice@example.com","password":"password123"}'
#   → 拿 accessToken 复用下面

TOK=<accessToken>

# 2) 创建 team(自己成为 owner)
curl -s -X $BASE/api/mt/teams -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"name":"My Team"}'
#   → 拿 teamId

TID=<teamId>

# 3) 列出我的 team
curl -s $BASE/api/mt/teams -H "Authorization: Bearer $TOK"

# 4) 生成邀请码(owner)
curl -s -X $BASE/api/mt/teams/$TID/invitations -H "Authorization: Bearer $TOK" -d '{}'

# 5) 列出成员
curl -s $BASE/api/mt/teams/$TID/members -H "Authorization: Bearer $TOK"

# 6) 设置 team 的 OpenAI key(owner;会先调 OpenAI 验证有效性)
curl -s -X $BASE/api/mt/teams/$TID/api-key -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d '{"key":"sk-你的真实key"}'

# 7) 列出 key(只返回 hint,如 …1a2b;绝不返回密文)
curl -s $BASE/api/mt/teams/$TID/api-key -H "Authorization: Bearer $TOK"

# 8) 创建会话(M3:会按 team 启动 codex 进程,需 team 已设 key + codex 可用)
curl -s -X $BASE/api/mt/threads -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' -d "{\"teamId\":\"$TID\"}"
#   → 拿 threadId(codex thread/start 响应);元数据已写入 PG threads 表

# 9) 列出 team 会话(从 PG,team 内共享)
curl -s "$BASE/api/mt/threads?teamId=$TID" -H "Authorization: Bearer $TOK"

# 10) 发 turn(会真正调 OpenAI,需真实 key;codex 进程按 team 复用)
curl -s -X $BASE/api/mt/threads/<threadId>/turns -H "Authorization: Bearer $TOK" \
  -H 'Content-Type: application/json' \
  -d '{"input":[{"type":"message","content":[{"type":"input_text","text":"hi"}]}]}'

# 11) 邀请另一个用户:对方先注册,再用邀请码加入
curl -s -X $BASE/api/mt/teams/join -H "Authorization: Bearer <对方accessToken>" \
  -H 'Content-Type: application/json' -d '{"code":"<邀请码>"}'
```

## Swagger UI

http://localhost:8172/api/docs(`/api/mt/*` 接口可在线试)。

## 单元测试

```bash
cargo test --manifest-path backend-rs/Cargo.toml --lib multitenant::
# 8 个测试:password 哈希/JWT/refresh-hash/email/邀请码 + AES 加解密×3
```

## 当前进度

- ✅ **M1** 用户体系 + 多租户 + 认证 + team(创建/成员/邀请码/角色)
- ✅ **M2** BYOK key 管理(AES-256-GCM 加密存储 + OpenAI 验证 + set/list/轮换)
- ✅ **M3** TeamCodexManager + 多租户 thread 路由(per-team codex 进程 + team 校验 + PG 元数据双写)
- ⏳ **M4** 分布式(接入/worker 分离 + Redis + 跨节点广播)—— 规模超大、需多机环境验证,后续会话推进
- ⏳ **M5** 高可用 + 可观测 + 扩缩容(Redis/PG 主从 + Prometheus + OpenTelemetry)
- ⏳ **M6** 打磨上线(防滥用 + 计费 + 安全审计)

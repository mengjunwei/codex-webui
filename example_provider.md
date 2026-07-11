# cc-switch Provider 添加指南

cc-switch 是 API 代理，负责将 Codex 的请求转发到不同的大模型服务。本文档说明如何添加和管理 provider。

## 快速示例：添加小米 mimo

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name xiaomi \
  --id xiaomi \
  --base-url https://token-plan-cn.xiaomimimo.com/v1 \
  --api-key tp-caaopghxs5fkvr09lbqst66qmg9 \
  --model mimo-v2.5-pro \
  --api-format chat
```

## 参数说明

| 参数 | 必填 | 说明 |
|------|------|------|
| `-a codex` | 是 | 指定应用名，固定为 `codex` |
| `--template custom` | 是 | 使用自定义模板 |
| `--name` | 是 | provider 显示名称 |
| `--id` | 是 | provider 唯一标识（用于切换/删除） |
| `--base-url` | 是 | API 地址（OpenAI 兼容格式，以 `/v1` 结尾） |
| `--api-key` | 是 | API 密钥 |
| `--model` | 是 | 模型名称 |
| `--api-format` | 是 | API 格式，通常为 `chat` |

## 常用 Provider 示例

### OpenAI

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name openai \
  --id openai \
  --base-url https://api.openai.com/v1 \
  --api-key sk-xxx \
  --model gpt-4.1 \
  --api-format chat
```

### Anthropic（Claude）

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name anthropic \
  --id anthropic \
  --base-url https://api.anthropic.com/v1 \
  --api-key sk-ant-xxx \
  --model claude-sonnet-4-20250514 \
  --api-format chat
```

### MiniMax

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name minimax \
  --id minimax \
  --base-url https://api.minimax.chat/v1 \
  --api-key xxx \
  --model MiniMax-M3 \
  --api-format chat
```

### Azure OpenAI

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name azure \
  --id azure \
  --base-url https://your-resource.openai.azure.com/openai/deployments/your-deployment \
  --api-key xxx \
  --model gpt-4.1 \
  --api-format chat
```

### 本地模型（Ollama / vLLM / LiteLLM）

```bash
/home/master/Mnet/target/cc-switch provider add -a codex \
  --template custom \
  --name local \
  --id local \
  --base-url http://127.0.0.1:11434/v1 \
  --api-key dummy \
  --model qwen2.5-coder-32b \
  --api-format chat
```

## 管理命令

```bash
CC=/home/master/Mnet/target/cc-switch

# 列出所有 provider
$CC -a codex provider list

# 切换 provider
$CC -a codex use xiaomi
$CC -a codex use minimax

# 删除 provider
$CC -a codex provider delete <id>

# 通过启动脚本切换（会自动重启 cc-switch + codex-webui）
bash ~/Mnet/bin/start.sh switch xiaomi
```

## 添加后的验证

```bash
# 1. 切换到新 provider
/home/master/Mnet/target/cc-switch -a codex use xiaomi

# 2. 重启服务使配置生效
bash ~/Mnet/bin/start.sh restart-all

# 3. 查看当前 provider
/home/master/Mnet/target/cc-switch -a codex provider list

# 4. 测试 API 代理连通性
curl -s http://127.0.0.1:15722/v1/models | head -5
```

## 注意事项

- `--base-url` 必须是 OpenAI 兼容格式（`/v1` 结尾）
- `--api-format` 通常填 `chat`，部分旧 API 填 `completion`
- 切换 provider 后需要重启服务：`bash ~/Mnet/bin/start.sh restart-all`
- provider 配置存储在 `~/.cc-switch/cc-switch.db`（SQLite）

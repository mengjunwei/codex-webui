//! 启动 codex 前向 $CODEX_HOME/config.toml 写入 hooks 配置(per-user workspace 实施步骤 11)。
//!
//! codex 0.142.5 hooks 配置 schema 在实施时按真实 schema 校正;
//! 当前写最小可用版:`[hooks.audit]` 段指向本进程 /hooks/codex。

use crate::error::AppError;
use std::path::Path;

/// 写入 $CODEX_HOME/config.toml 的 hooks 段(幂等:每次覆盖写,codex 启动重读)。
/// 失败不抛 — 启动时记录 warn,codex 仍能跑(只是不回调 webhook)。
pub async fn write_hooks_config(codex_home: &Path, port: u16) -> Result<(), AppError> {
    let cfg_path = codex_home.join("config.toml");
    let body = format!(
        "# 由 codex-webui 自动注入(per-user workspace 实施步骤 11)\n\
         # 工具/技能/插件/MCP 调用前后的回调地址;不要手工改本段,重启 backend 会重写。\n\
         [hooks.audit]\n\
         type = \"http\"\n\
         url = \"http://127.0.0.1:{port}/hooks/codex\"\n\
         auth_header = \"X-Hook-Token\"\n\
         auth_env = \"INTERNAL_HOOK_TOKEN\"\n"
    );
    tokio::fs::write(&cfg_path, body)
        .await
        .map_err(|e| AppError::internal(format!("write {}: {e}", cfg_path.display())))?;
    tracing::info!(path = %cfg_path.display(), port, "hooks config written");
    Ok(())
}
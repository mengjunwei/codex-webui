//! 启动 codex 前向 $CODEX_HOME/config.toml 写入 hooks 配置(per-user workspace 实施步骤 11)。
//!
//! codex 0.142.5 hooks 配置 schema 在实施时按真实 schema 校正;
//! 当前写最小可用版:`[hooks.audit]` 段指向本进程 /hooks/codex。
//!
//! 采用 **merge 模式**(保留现有内容,只更新 hooks 段):覆盖写会丢失用户的模型配置
//! (model_provider/model 等),导致 codex 重启后无法连接 AI API。

use crate::error::AppError;
use std::path::Path;

/// 写入 $CODEX_HOME/config.toml 的 hooks 段(merge:保留现有内容,只更新 hooks 段)。
/// 失败不抛 — 启动时记录 warn,codex 仍能跑(只是不回调 webhook)。
pub async fn write_hooks_config(codex_home: &Path, port: u16) -> Result<(), AppError> {
    let cfg_path = codex_home.join("config.toml");

    // 读现有配置(保留用户配置如 model/model_providers 等)。
    let existing = match tokio::fs::read_to_string(&cfg_path).await {
        Ok(c) => c,
        Err(_) => String::new(), // 不存在则新建
    };

    // 构建 hooks 段。
    let hooks_section = format!(
        "# 由 codex-webui 自动注入(per-user workspace 实施步骤 11)\n\
         # 工具/技能/插件/MCP 调用前后的回调地址;不要手工改本段,重启 backend 会重写。\n\
         [hooks.audit]\n\
         type = \"http\"\n\
         url = \"http://127.0.0.1:{port}/hooks/codex\"\n\
         auth_header = \"X-Hook-Token\"\n\
         auth_env = \"INTERNAL_HOOK_TOKEN\"\n"
    );

    // merge:保留现有内容(去旧 hooks 段后追加新 hooks 段)。
    // 简单策略:如果文件已含 [hooks.audit] → 替换该段;否则追加。
    let merged = if existing.contains("[hooks.audit]") {
        // 替换旧 hooks 段(简单:去旧块 + 追加新块)。
        // 找 [hooks.audit] 开头到下一个 [ 开头(或文件尾)之间的内容。
        let hook_start = existing.find("[hooks.audit]").unwrap_or(0);
        let after_hook = &existing[hook_start + "[hooks.audit]".len()..];
        let hook_end = after_hook
            .find('\n')
            .and_then(|i| after_hook[i..].find('['))
            .map(|i| hook_start + "[hooks.audit]".len() + i + after_hook.find('\n').unwrap_or(0))
            .unwrap_or(existing.len());
        let mut new_content = existing[..hook_start].to_string();
        new_content.push_str(&hooks_section);
        new_content.push_str(&existing[hook_end..]);
        new_content
    } else {
        // 追加。
        format!("{}\n{}", existing, hooks_section)
    };

    tokio::fs::write(&cfg_path, merged)
        .await
        .map_err(|e| AppError::internal(format!("write {}: {e}", cfg_path.display())))?;
    tracing::info!(path = %cfg_path.display(), port, "hooks config written (merge mode)");
    Ok(())
}
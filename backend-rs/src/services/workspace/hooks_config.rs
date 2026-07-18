//! 启动 codex 前向 $CODEX_HOME/config.toml 注入 [hooks.audit] 段(per-user workspace 实施步骤 11)。
//!
//! codex 0.142.5 hooks 配置 schema 在实施时按真实 schema 校正;
//! 当前写最小可用版:`[hooks.audit]` 段指向本进程 /hooks/codex。
//!
//! 采用 **toml_edit 精确修改**:解析整个 config.toml → 只动 [hooks.audit] 几个字段 →
//! 写回。保留用户其余所有配置(model/model_providers/注释/空行/格式)原样。

use crate::error::AppError;
use std::path::Path;
use toml_edit::{DocumentMut, value, Item};

/// 解析 $CODEX_HOME/config.toml,精确设置 [hooks.audit] 段的几个字段,写回。
/// 其余配置(model/model_providers/注释/格式)原样保留。失败不抛 — 启动时记录 warn,
/// codex 仍能跑(只是不回调 webhook)。
pub async fn write_hooks_config(codex_home: &Path, port: u16) -> Result<(), AppError> {
    let cfg_path = codex_home.join("config.toml");

    // 读现有配置(保留用户配置如 model/model_providers/注释等)。
    let existing = tokio::fs::read_to_string(&cfg_path).await.unwrap_or_default();

    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| AppError::internal(format!("parse {}: {e}", cfg_path.display())))?;

    // 确保 [hooks] 表存在(空表也行,若已是非表类型则视为非法不动它)。
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let hooks_tbl = hooks
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [hooks] 不是表,无法注入: {}", cfg_path.display())))?;

    // 确保 [hooks.audit] 子表存在。
    let audit = hooks_tbl
        .entry("audit")
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let audit_tbl = audit
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [hooks.audit] 不是表,无法注入: {}", cfg_path.display())))?;

    // 精确设置 4 个字段(已有则覆盖值,保留键的装饰/注释)。
    set_str(audit_tbl, "type", "http");
    set_str(audit_tbl, "url", &format!("http://127.0.0.1:{port}/hooks/codex"));
    set_str(audit_tbl, "auth_header", "X-Hook-Token");
    set_str(audit_tbl, "auth_env", "INTERNAL_HOOK_TOKEN");

    let merged = doc.to_string();
    // 内容未变则跳过写盘(避免刷新 mtime / 触发 watcher)。
    if merged == existing {
        tracing::debug!(path = %cfg_path.display(), port, "hooks config unchanged, skip write");
        return Ok(());
    }
    tokio::fs::write(&cfg_path, merged)
        .await
        .map_err(|e| AppError::internal(format!("write {}: {e}", cfg_path.display())))?;
    tracing::info!(path = %cfg_path.display(), port, "hooks config written (toml_edit, preserve user config)");
    Ok(())
}

/// 设置表中某 string 字段:已有则只更新 value(保留键前后的注释/装饰),不存在则追加。
fn set_str(tbl: &mut toml_edit::Table, key: &str, val: &str) {
    if let Some(item) = tbl.get_mut(key) {
        // 已是值类型 → 原地替换为新的 string value(保留键的装饰/注释)。
        if item.is_value() {
            *item = value(val);
            return;
        }
    }
    tbl.insert(key, value(val));
}

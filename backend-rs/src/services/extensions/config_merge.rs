//! 通用 toml_edit 段合并/移除工具。
//!
//! plugin 启用记录写进 `[plugins."id@market"]` 段;未来 MCP 复用 `[mcp_servers."id"]`。
//! 采用 toml_edit 精确修改:解析整个 config.toml → 只动目标段 → 写回,
//! 保留其余所有配置(其他段/注释/空行/格式)原样。

use crate::error::AppError;
use std::path::Path;
use toml_edit::{DocumentMut, Item, value};

/// 确保 config.toml 有 [section] 段且 key=value;段不存在则建,已存在则更新 key(保留其他 key)。
/// section 形如 `plugins."foo@bar"` 或 `mcp_servers.xxx`(含引号/点按 TOML 规则)。
pub async fn ensure_section_kv(cfg_path: &Path, section: &str, key: &str, value: &str) -> Result<(), AppError> {
    let existing = tokio::fs::read_to_string(cfg_path).await.unwrap_or_default();
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| AppError::internal(format!("parse config: {e}")))?;
    let (parent, leaf) = split_section(section); // `plugins."foo@bar"` -> ("plugins", "foo@bar")
    let p = doc
        .entry(parent.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let tbl = p
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{parent}] 不是表")))?;
    let leaf_tbl = tbl
        .entry(leaf.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let lt = leaf_tbl
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{section}] 不是表")))?;
    set_kv(lt, key, value);
    let merged = doc.to_string();
    // 内容未变跳过写盘(避免刷新 mtime / 触发 watcher)。
    if merged == existing {
        return Ok(());
    }
    tokio::fs::write(cfg_path, merged)
        .await
        .map_err(|e| AppError::internal(format!("write config: {e}")))?;
    Ok(())
}

/// 同 `ensure_section_kv` 但写 boolean 值(`enabled = true`,无引号)。
///
/// plugin 启用段 `[plugins."id@market"] enabled = true` 的 `enabled` 在 codex schema 里是
/// **boolean**;若用 `ensure_section_kv` 写字符串 `enabled = "true"`,codex 加载 config 会
/// 报 "invalid type: string \"true\", expected a boolean" 并拒绝整个 config → plugin 不可用。
pub async fn ensure_section_bool(
    cfg_path: &Path,
    section: &str,
    key: &str,
    val: bool,
) -> Result<(), AppError> {
    let existing = tokio::fs::read_to_string(cfg_path).await.unwrap_or_default();
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| AppError::internal(format!("parse config: {e}")))?;
    let (parent, leaf) = split_section(section);
    let p = doc
        .entry(parent.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let tbl = p
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{parent}] 不是表")))?;
    let leaf_tbl = tbl
        .entry(leaf.as_str())
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let lt = leaf_tbl
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{section}] 不是表")))?;
    set_kv_bool(lt, key, val);
    let merged = doc.to_string();
    if merged == existing {
        return Ok(());
    }
    tokio::fs::write(cfg_path, merged)
        .await
        .map_err(|e| AppError::internal(format!("write config: {e}")))?;
    Ok(())
}

/// 移除 config.toml 的 [section] 段;不存在/解析失败视为成功。
pub async fn remove_section(cfg_path: &Path, section: &str) -> Result<(), AppError> {
    let existing = match tokio::fs::read_to_string(cfg_path).await {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let mut doc = match existing.parse::<DocumentMut>() {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    let (parent, leaf) = split_section(section);
    if let Some(Item::Table(tbl)) = doc.get_mut(parent.as_str()) {
        tbl.remove(leaf.as_str());
    }
    let merged = doc.to_string();
    if merged != existing {
        let _ = tokio::fs::write(cfg_path, merged).await;
    }
    Ok(())
}

/// 把 content_toml(段内容,无 [parent.leaf] 头) 合并进 config.toml 的 [parent.leaf] 段。
/// 实现:包头 parse 成 doc,取其 [parent.leaf] table,逐 key clone 到目标 doc 的 [parent.leaf]。
/// 支持嵌套值(env = { ... } 等),因 toml_edit::Item clone 递归。
pub async fn merge_full_section(
    cfg_path: &Path,
    parent: &str,
    leaf: &str,
    content_toml: &str,
) -> Result<(), AppError> {
    // 包头 parse:[parent.leaf] + content_toml → DocumentMut,再取其 [parent][leaf] table。
    let wrapped = format!("[{parent}.{leaf}]\n{content_toml}\n");
    let src = wrapped
        .parse::<DocumentMut>()
        .map_err(|e| AppError::internal(format!("parse mcp content: {e}")))?;
    let src_table = src
        .get(parent)
        .and_then(|i| i.as_table())
        .and_then(|t| t.get(leaf))
        .and_then(|i| i.as_table())
        .ok_or_else(|| AppError::internal("merge_full_section: 解析后取不到段 table".into()))?;

    // 读现有 config(不存在视作空),解析成 doc。
    let existing = tokio::fs::read_to_string(cfg_path)
        .await
        .unwrap_or_default();
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| AppError::internal(format!("parse config: {e}")))?;
    // 确保 [parent] 存在且是表。
    let p = doc
        .entry(parent)
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let ptbl = p
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{parent}] 不是表")))?;
    // 确保 [parent.leaf] 存在且是表。
    let leaf_item = ptbl
        .entry(leaf)
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    let ltbl = leaf_item
        .as_table_mut()
        .ok_or_else(|| AppError::internal(format!("config [{parent}.{leaf}] 不是表")))?;
    // 逐 key clone 进目标段(支持嵌套,因 Item clone 递归);同 key 后写覆盖前写。
    for (k, v) in src_table.iter() {
        ltbl.insert(k, v.clone());
    }
    let merged = doc.to_string();
    // 内容未变跳过写盘。
    if merged == existing {
        return Ok(());
    }
    tokio::fs::write(cfg_path, merged)
        .await
        .map_err(|e| AppError::internal(format!("write config: {e}")))?;
    Ok(())
}

/// `plugins."foo@bar"` -> ("plugins", "foo@bar");`mcp_servers.xxx` -> ("mcp_servers", "xxx")。
/// leaf 用 `trim_matches('"')` 去掉 quoted 形式的引号(toml_edit 写回时会按需自动再加引号)。
fn split_section(section: &str) -> (String, String) {
    if let Some(dot) = section.find('.') {
        (
            section[..dot].to_string(),
            section[dot + 1..].trim_matches('"').to_string(),
        )
    } else {
        (section.to_string(), String::new())
    }
}

/// 设置表中某 string 字段:已有则只更新 value(保留键前后的注释/装饰),不存在则追加。
fn set_kv(tbl: &mut toml_edit::Table, key: &str, val: &str) {
    if let Some(item) = tbl.get_mut(key) {
        if item.is_value() {
            *item = value(val);
            return;
        }
    }
    tbl.insert(key, value(val));
}

/// 同 `set_kv` 但写 boolean(toml_edit `value(bool)` 产裸 `true`/`false`,无引号)。
fn set_kv_bool(tbl: &mut toml_edit::Table, key: &str, val: bool) {
    if let Some(item) = tbl.get_mut(key) {
        if item.is_value() {
            *item = value(val);
            return;
        }
    }
    tbl.insert(key, value(val));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ensure_creates_and_updates_quoted_section() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[model_providers.custom]\nname = \"x\"\n").await.unwrap();
        // 建 [plugins."foo@bar"] enabled=true (boolean 无引号 —— codex schema 要求)
        ensure_section_bool(&p, "plugins.\"foo@bar\"", "enabled", true).await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("[plugins.\"foo@bar\"]"));
        assert!(s.contains("enabled = true"));
        assert!(!s.contains("enabled = \"true\"")); // 必须不是字符串,否则 codex 拒绝加载
        assert!(s.contains("[model_providers.custom]")); // 原段保留
    }
    #[tokio::test]
    async fn remove_drops_section_keeps_others() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[plugins.\"a@m\"]\nenabled = \"true\"\n[model_providers.x]\nname=\"y\"\n").await.unwrap();
        remove_section(&p, "plugins.\"a@m\"").await.unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(!s.contains("plugins.\"a@m\""));
        assert!(s.contains("[model_providers.x]"));
    }
}

#[cfg(test)]
mod merge_full_tests {
    use super::*;
    #[tokio::test]
    async fn merges_multifield_section_preserving_others() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        tokio::fs::write(&p, "[model_providers.custom]\nname = \"x\"\n")
            .await
            .unwrap();
        let content = "command = \"node\"\nargs = [\"s.js\"]\n";
        merge_full_section(&p, "mcp_servers", "myserver", content)
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("[mcp_servers.myserver]") || s.contains("[mcp_servers.\"myserver\"]"));
        assert!(s.contains("command"));
        assert!(s.contains("s.js"));
        assert!(s.contains("[model_providers.custom]")); // 原段保留
    }
    #[tokio::test]
    async fn merge_is_idempotent_and_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        merge_full_section(&p, "mcp_servers", "s", "command = \"a\"\n")
            .await
            .unwrap();
        // 再合并不同内容,应更新
        merge_full_section(&p, "mcp_servers", "s", "command = \"b\"\n")
            .await
            .unwrap();
        let s = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(s.contains("b"));
    }
}

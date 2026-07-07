//! 结构化日志读取器 + 脱敏诊断信息导出。
//!
//! 与 `src/logs/logs.service.ts` 保持对齐。从 `./logs/` 目录读取 JSON 日志行
//! （文件名为 `app` 或 `app.*`），将其解析为 `LogEntry` 记录，按时间倒序排序，
//! 按级别/来源过滤、分页，并对敏感字段进行脱敏处理。
//!
//! 日志格式：由我们自己的 tracing JSON 文件层写入。解析器同时兼容旧版 pino JSON
//! （数字级别、`time`、`msg`、`context`），用于解析 TS 后端产生的日志。

use crate::error::AppError;
use crate::state::AppState;
use axum::{
    extract::{Query, State},
    Json,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const EXPORT_LIMIT: usize = 100;
const MAX_ENTRIES_CAP: usize = 10_000;
const LOG_FILE_PREFIX: &str = "app";

static SENSITIVE_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new("(?i)(authorization|cookie|token|apikey|api_key|password|secret|credential)").unwrap());
static BEARER_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"Bearer\s+[A-Za-z0-9._~+/=-]+").unwrap());

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

// ── 数据传输对象（DTOs）─────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub source: String,
    pub message: String,
    pub fields: Value,
}

#[derive(Deserialize)]
pub struct LogsQuery {
    pub offset: Option<String>,
    pub limit: Option<String>,
    pub level: Option<String>,
    pub source: Option<String>,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub data: Vec<LogEntry>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

#[derive(Serialize)]
pub struct LogsExportResponse {
    #[serde(rename = "exportedAt")]
    pub exported_at: String,
    pub system: SystemInfo,
    #[serde(rename = "runtimeStatus")]
    pub runtime_status: Value,
    pub logs: Vec<LogEntry>,
}

#[derive(Serialize)]
pub struct SystemInfo {
    #[serde(rename = "nodeVersion")]
    pub node_version: String,
    pub platform: String,
    pub arch: String,
    #[serde(rename = "uptimeSeconds")]
    pub uptime_seconds: u64,
    #[serde(rename = "codexVersion")]
    pub codex_version: String,
}

// ── 处理函数 ─────────────────────────────────────────────────────────────────

pub async fn list_logs(
    State(_state): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<LogsResponse>, AppError> {
    let offset = clamp_usize(q.offset.as_deref(), 0, usize::MAX, 0);
    let limit = clamp_usize(q.limit.as_deref(), 1, MAX_LIMIT, DEFAULT_LIMIT);
    let level = normalize_filter(q.level.as_deref());
    let source = normalize_filter(q.source.as_deref());

    let entries = read_all_entries(Path::new("logs"));
    let filtered: Vec<LogEntry> = entries
        .into_iter()
        .filter(|e| {
            let level_ok = level.as_deref().map_or(true, |l| e.level == l);
            let source_ok = source
                .as_deref()
                .map_or(true, |s| e.source.to_lowercase().contains(s));
            level_ok && source_ok
        })
        .collect();
    let total = filtered.len();
    let page_end = offset.saturating_add(limit).min(total);
    let data: Vec<LogEntry> = filtered
        .get(offset..page_end)
        .map(|slice| slice.to_vec())
        .unwrap_or_default();
    let has_more = offset + limit < total;

    Ok(Json(LogsResponse {
        data,
        offset,
        limit,
        total,
        has_more,
    }))
}

pub async fn export_diagnostics(
    State(state): State<AppState>,
) -> Result<Json<LogsExportResponse>, AppError> {
    let entries = read_all_entries(Path::new("logs"));
    let logs: Vec<LogEntry> = entries.into_iter().take(EXPORT_LIMIT).collect();

    Ok(Json(LogsExportResponse {
        exported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        system: SystemInfo {
            node_version: format!(
                "backend-rs {} (rust)",
                env!("CARGO_PKG_VERSION")
            ),
            platform: platform_name(),
            arch: arch_name(),
            uptime_seconds: uptime_seconds(),
            codex_version: codex_version_async(&state).await,
        },
        // 运行时就绪状态（来自 CodexStatusService，对齐 TS statusService.getStatus()）。
        runtime_status: state.status.get_status().await,
        logs,
    }))
}

// ── 文件读取 ─────────────────────────────────────────────────────────────────

/// 读取并解析所有日志条目（优先读取最新文件，每个文件从尾部向头部读取）。
pub fn read_all_entries(log_dir: &Path) -> Vec<LogEntry> {
    let files = get_log_files(log_dir);
    let mut entries: Vec<LogEntry> = Vec::new();
    for file in files {
        let remaining = MAX_ENTRIES_CAP.saturating_sub(entries.len());
        if remaining == 0 {
            break;
        }
        entries.extend(read_entries_from_file(&file, remaining));
    }
    entries.truncate(MAX_ENTRIES_CAP);
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    entries
}

fn get_log_files(log_dir: &Path) -> Vec<PathBuf> {
    let read = match std::fs::read_dir(log_dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name != LOG_FILE_PREFIX && !name.starts_with(&format!("{}.", LOG_FILE_PREFIX)) {
            continue;
        }
        let path = entry.path();
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((path, mtime));
    }
    // 按修改时间倒序排列（最新优先）。
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.into_iter().map(|(p, _)| p).collect()
}

fn read_entries_from_file(file: &Path, max_entries: usize) -> Vec<LogEntry> {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut entries = Vec::new();
    // 从尾部向头部读取，以便优先捕获最新的条目。
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(entry) = parse_line(trimmed, file) {
            entries.push(entry);
            if entries.len() >= max_entries {
                break;
            }
        }
    }
    entries
}

// ── 解析（tracing JSON + 旧版 pino JSON）────────────────────────────────────

fn parse_line(line: &str, file: &Path) -> Option<LogEntry> {
    let record: Value = serde_json::from_str(line).ok()?;
    let sanitized = sanitize_value(record);
    let obj = sanitized.as_object()?;

    Some(LogEntry {
        timestamp: to_timestamp(obj),
        level: to_level(obj),
        source: to_source(obj, file),
        message: to_message(obj),
        fields: Value::Object(obj.clone()),
    })
}

fn to_timestamp(obj: &Map<String, Value>) -> String {
    // tracing 写入 `timestamp`（ISO 字符串）；pino 写入 `time`（毫秒数值）。
    if let Some(t) = obj.get("timestamp").and_then(Value::as_str) {
        return t.to_string();
    }
    if let Some(t) = obj.get("time") {
        match t {
            Value::Number(n) => {
                if let Some(ms) = n.as_i64() {
                    if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
                        return dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    }
                }
            }
            Value::String(s) => return s.clone(),
            _ => {}
        }
    }
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn to_level(obj: &Map<String, Value>) -> String {
    match obj.get("level") {
        Some(Value::String(s)) => s.to_ascii_lowercase(),
        Some(Value::Number(n)) => {
            let v = n.as_i64().unwrap_or(0);
            if v >= 60 {
                "fatal".into()
            } else if v >= 50 {
                "error".into()
            } else if v >= 40 {
                "warn".into()
            } else if v >= 30 {
                "info".into()
            } else if v >= 20 {
                "debug".into()
            } else if v >= 10 {
                "trace".into()
            } else {
                "unknown".into()
            }
        }
        _ => "unknown".into(),
    }
}

fn to_source(obj: &Map<String, Value>, file: &Path) -> String {
    // tracing 写入 `target`（模块路径）；pino 写入 `context`/`source`/`name`。
    for key in &["target", "context", "source", "name"] {
        if let Some(Value::String(s)) = obj.get(*key) {
            return s.clone();
        }
    }
    file.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(LOG_FILE_PREFIX)
        .to_string()
}

fn to_message(obj: &Map<String, Value>) -> String {
    // tracing 嵌套在 `fields.message` 下；pino 使用 `msg`/`message`。
    if let Some(Value::Object(fields)) = obj.get("fields") {
        if let Some(Value::String(s)) = fields.get("message") {
            return s.clone();
        }
    }
    for key in &["msg", "message"] {
        if let Some(Value::String(s)) = obj.get(*key) {
            return s.clone();
        }
    }
    String::new()
}

// ── 脱敏处理 ─────────────────────────────────────────────────────────────────

fn sanitize_value(value: Value) -> Value {
    match value {
        Value::Array(arr) => Value::Array(arr.into_iter().map(sanitize_value).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if SENSITIVE_KEY.is_match(&k) {
                    out.insert(k, Value::String("[Redacted]".into()));
                } else {
                    out.insert(k, sanitize_value(v));
                }
            }
            Value::Object(out)
        }
        Value::String(s) => Value::String(BEARER_TOKEN.replace_all(&s, "Bearer [Redacted]").into()),
        other => other,
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

fn clamp_usize(value: Option<&str>, min: usize, max: usize, fallback: usize) -> usize {
    let parsed = value
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n >= 0);
    match parsed {
        Some(n) => (n as usize).clamp(min, max),
        None => fallback,
    }
}

fn normalize_filter(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

fn uptime_seconds() -> u64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed().as_secs()
}

fn platform_name() -> String {
    // 映射为 Node 约定以保持与前端一致：macos→darwin，windows→win32。
    match std::env::consts::OS {
        "macos" => "darwin".into(),
        "windows" => "win32".into(),
        other => other.into(),
    }
}

fn arch_name() -> String {
    // 映射为 Node 约定：x86_64→x64，aarch64→arm64。
    match std::env::consts::ARCH {
        "x86_64" => "x64".into(),
        "aarch64" => "arm64".into(),
        other => other.into(),
    }
}

async fn codex_version_async(state: &AppState) -> String {
    // H4 修复：放在 spawn_blocking 中执行，避免阻塞 tokio worker 线程
    // （TS 端使用带 2 秒超时的 execFileAsync）。
    let bin = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let _ = state;
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty())
    })
    .await
    .ok()
    .flatten();
    result.unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tracing_json_line() {
        let entry = parse_line(
            r#"{"timestamp":"2026-07-06T12:00:00.000Z","level":"INFO","target":"codex_webui::db","fields":{"message":"db ready","count":1}}"#,
            Path::new("app.2026-07-06"),
        )
        .unwrap();
        assert_eq!(entry.timestamp, "2026-07-06T12:00:00.000Z");
        assert_eq!(entry.level, "info");
        assert_eq!(entry.source, "codex_webui::db");
        assert_eq!(entry.message, "db ready");
    }

    #[test]
    fn parse_legacy_pino_line() {
        let entry = parse_line(
            r#"{"level":40,"time":1783000000000,"msg":"warn here","context":"AppService"}"#,
            Path::new("app"),
        )
        .unwrap();
        assert_eq!(entry.level, "warn");
        assert!(entry.timestamp.contains("202"));
        assert_eq!(entry.source, "AppService");
        assert_eq!(entry.message, "warn here");
    }

    #[test]
    fn sanitize_redacts_sensitive_keys() {
        let v = serde_json::json!({"authorization": "Bearer abc", "path": "/ws", "password": "secret"});
        let s = sanitize_value(v);
        assert_eq!(s["authorization"], "[Redacted]");
        assert_eq!(s["password"], "[Redacted]");
        assert_eq!(s["path"], "/ws");
    }

    #[test]
    fn sanitize_scrubs_bearer_in_strings() {
        let v = Value::String("token: Bearer xyz123".into());
        let s = sanitize_value(v);
        assert_eq!(s, Value::String("token: Bearer [Redacted]".into()));
    }

    #[test]
    fn level_normalization() {
        let mk = |lvl: i64| {
            let obj: Map<String, Value> =
                serde_json::from_str(&format!(r#"{{"level":{}}}"#, lvl)).unwrap();
            to_level(&obj)
        };
        assert_eq!(mk(60), "fatal");
        assert_eq!(mk(50), "error");
        assert_eq!(mk(40), "warn");
        assert_eq!(mk(30), "info");
        assert_eq!(mk(20), "debug");
        assert_eq!(mk(10), "trace");
        assert_eq!(mk(5), "unknown");
    }

    #[test]
    fn clamp_parses_and_clamps() {
        assert_eq!(clamp_usize(Some("5"), 0, 200, 50), 5);
        assert_eq!(clamp_usize(Some("1000"), 0, 200, 50), 200);
        assert_eq!(clamp_usize(Some("abc"), 0, 200, 50), 50);
        assert_eq!(clamp_usize(None, 0, 200, 50), 50);
    }

    #[test]
    fn read_all_entries_from_dir() {
        let dir = std::env::temp_dir().join(format!("codex-webui-logs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("app"),
            "{\"timestamp\":\"2026-07-06T10:00:00.000Z\",\"level\":\"INFO\",\"target\":\"a\",\"fields\":{\"message\":\"old\"}}\n{\"timestamp\":\"2026-07-06T11:00:00.000Z\",\"level\":\"ERROR\",\"target\":\"b\",\"fields\":{\"message\":\"new\"}}\n",
        ).unwrap();
        std::fs::write(dir.join("not-a-log.txt"), "ignore me").unwrap();

        let entries = read_all_entries(&dir);
        assert_eq!(entries.len(), 2);
        // 按时间倒序排列（最新优先）。
        assert_eq!(entries[0].message, "new");
        assert_eq!(entries[1].message, "old");

        std::fs::remove_dir_all(&dir).ok();
    }
}

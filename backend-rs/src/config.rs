//! 进程级配置：环境变量 + db_path 解析。
//!
//! 与 `src/database/database.service.ts:resolveDatabasePath`
//! 以及 `.env.example` 保持对齐。db_path 优先级：`WEBUI_DB_PATH`（去除首尾空格后非空）
//! > `CODEX_HOME/codex-webui.sqlite` > `~/.codex/codex-webui.sqlite`。
//! `WEBUI_API_KEY` 为必填项；缺失或为空时启动失败。

use anyhow::{anyhow, Result};
use std::env;
use std::path::PathBuf;

pub struct Config {
    pub webui_api_key: String,
    pub host: String,
    pub port: u16,
    pub openai_api_key: Option<String>,
    pub log_level: String,
    pub codex_bin: String,
    pub codex_home: Option<String>,
    pub db_path: String,
    /// OTLP collector endpoint (e.g. `http://localhost:4317`). When set,
    /// tracing spans are exported via gRPC to an OpenTelemetry-compatible
    /// backend (Jaeger, Tempo, Grafana, Datadog, OTel Collector, ...).
    /// When `None`, the OTLP layer is not installed (zero overhead).
    pub otlp_endpoint: Option<String>,
    /// 多租户数据库连接串(postgres);未设置则禁用多租户功能。
    pub database_url: Option<String>,
}

const DEFAULT_DB_FILENAME: &str = "codex-webui.sqlite";

impl Config {
    pub fn from_env() -> Result<Self> {
        let webui_api_key = env::var("WEBUI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("WEBUI_API_KEY is required"))?;
        // 强制最小长度：该 key 同时用作 bearer 回退凭据与 JWT 派生种子，
        // 过短会被暴力破解。16 字符为下限（建议 32+）。
        if webui_api_key.len() < 16 {
            return Err(anyhow!(
                "WEBUI_API_KEY must be at least 16 characters (current: {}); \
                 use a long random secret",
                webui_api_key.len()
            ));
        }

        let port: u16 = env::var("PORT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|p| p.parse::<u16>())
            .transpose()?
            .unwrap_or(8172);

        let host = env::var("HOST")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let codex_home = env::var("CODEX_HOME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let openai_api_key = env::var("OPENAI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // 对齐 src/app.module.ts：LOG_LEVEL 未设置时开发态默认 "debug"、发布态默认 "info"。
        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| {
            if cfg!(debug_assertions) {
                "debug".to_string()
            } else {
                "info".to_string()
            }
        });
        let codex_bin = env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());

        // Optional: OTLP collector endpoint for distributed tracing export.
        // Standard env var name recognized by OpenTelemetry SDKs.
        let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let database_url = env::var("DATABASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Ok(Self {
            webui_api_key,
            host,
            port,
            openai_api_key,
            log_level,
            codex_bin,
            codex_home: codex_home.clone(),
            db_path: resolve_db_path(env::var("WEBUI_DB_PATH").ok(), codex_home.as_deref()),
            otlp_endpoint,
            database_url,
        })
    }
}

fn resolve_db_path(explicit: Option<String>, codex_home: Option<&str>) -> String {
    if let Some(p) = explicit.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return p;
    }
    let base = codex_home
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_or_home().join(".codex"));
    base.join(DEFAULT_DB_FILENAME).to_string_lossy().into_owned()
}

fn dirs_or_home() -> PathBuf {
    // 对齐 Node 的 os.homedir()：Windows 上优先用 USERPROFILE，其他平台用 HOME。
    // 对于这台 Windows 开发机至关重要，因为 Git Bash 会把 HOME 导出为 POSIX 路径。
    #[cfg(windows)]
    {
        if let Some(p) = std::env::var("USERPROFILE").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            return PathBuf::from(p);
        }
    }
    if let Some(p) = std::env::var("HOME").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        // Windows 上的兜底方案：HOMEDRIVE + HOMEPATH。
        let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
        let path = std::env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() && !path.is_empty() {
            return PathBuf::from(format!("{}{}", drive, path));
        }
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 自 Rust 1.86 起 env::set_var/remove_var 是 unsafe 操作；串行化测试访问。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VARS: &[&str] = &[
        "WEBUI_DB_PATH",
        "CODEX_HOME",
        "WEBUI_API_KEY",
        "PORT",
        "HOST",
        "LOG_LEVEL",
        "CODEX_BIN",
        "OPENAI_API_KEY",
    ];

    fn clear() {
        for k in VARS {
            unsafe { env::remove_var(k); }
        }
    }

    #[test]
    fn db_path_uses_explicit_webui_db_path() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("CODEX_HOME", "/tmp/ignored"); }
        unsafe { env::set_var("WEBUI_DB_PATH", "/explicit/a.sqlite"); }
        let c = Config::from_env().unwrap();
        assert_eq!(c.db_path, "/explicit/a.sqlite");
    }

    #[test]
    fn db_path_uses_codex_home_when_no_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("CODEX_HOME", "/codex-home"); }
        let c = Config::from_env().unwrap();
        assert!(c.db_path.contains("codex-home"),
            "expected CODEX_HOME in path, got {}", c.db_path);
        assert!(c.db_path.ends_with("codex-webui.sqlite"),
            "expected sqlite filename, got {}", c.db_path);
    }

    #[test]
    fn db_path_falls_back_to_dotcodex() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        let c = Config::from_env().unwrap();
        assert!(c.db_path.ends_with("codex-webui.sqlite"),
            "expected codex-webui.sqlite suffix, got {}", c.db_path);
        assert!(c.db_path.contains(".codex"),
            "expected .codex in path, got {}", c.db_path);
    }

    #[test]
    fn missing_api_key_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn empty_api_key_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "   "); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn port_defaults_to_8172() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        assert_eq!(Config::from_env().unwrap().port, 8172);
    }

    #[test]
    fn port_parses_when_set() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("PORT", "9000"); }
        assert_eq!(Config::from_env().unwrap().port, 9000);
    }

    #[test]
    fn host_defaults_to_0_0_0_0() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        assert_eq!(Config::from_env().unwrap().host, "0.0.0.0");
    }

    #[test]
    fn host_parses_when_set() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("HOST", "127.0.0.1"); }
        assert_eq!(Config::from_env().unwrap().host, "127.0.0.1");
    }
}

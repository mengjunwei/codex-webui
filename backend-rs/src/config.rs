//! 进程级配置：环境变量。
//!
//! `WEBUI_API_KEY` 为必填项；`DATABASE_URL` 必填(postgres:// 或 mysql://)。
//! 全 SeaORM 迁移后,数据库连接由 `DatabaseConnection::connect` 处理,无降级。

use anyhow::{anyhow, Result};
use std::env;

pub struct Config {
    pub webui_api_key: String,
    pub host: String,
    pub port: u16,
    pub openai_api_key: Option<String>,
    pub log_level: String,
    pub codex_bin: String,
    pub codex_home: Option<String>,
    /// OTLP collector endpoint (e.g. `http://localhost:4317`)。When set, OTLP gRPC 导出。
    pub otlp_endpoint: Option<String>,
    /// 多方言数据库连接串(postgres:// 或 mysql://)。**必选**(全 SeaORM 迁移后无降级)。
    pub database_url: String,
    /// 主密钥(加密 team API key);未设置则回退用 webui_api_key。
    pub master_key: Option<String>,
    /// Redis 连接串(M4 分布式协调);未设置则禁用跨节点功能。
    pub redis_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let webui_api_key = env::var("WEBUI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("WEBUI_API_KEY is required"))?;
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

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| {
            if cfg!(debug_assertions) {
                "debug".to_string()
            } else {
                "info".to_string()
            }
        });
        let codex_bin = env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());

        let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let database_url = env::var("DATABASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("DATABASE_URL is required (postgresql:// or mysql://)"))?;

        let master_key = env::var("MASTER_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let redis_url = env::var("REDIS_URL")
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
            otlp_endpoint,
            database_url,
            master_key,
            redis_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VARS: &[&str] = &[
        "CODEX_HOME",
        "WEBUI_API_KEY",
        "PORT",
        "HOST",
        "LOG_LEVEL",
        "CODEX_BIN",
        "OPENAI_API_KEY",
        "DATABASE_URL",
    ];

    fn clear() {
        for k in VARS {
            unsafe { env::remove_var(k); }
        }
    }

    /// 测试前必须设置 DATABASE_URL(必选)。
    fn with_db<F: FnOnce() -> T, T>(f: F) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
        f()
    }

    #[test]
    fn missing_api_key_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn empty_api_key_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "   "); }
        unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn missing_database_url_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn port_defaults_to_8172() {
        with_db(|| assert_eq!(Config::from_env().unwrap().port, 8172));
    }

    #[test]
    fn port_parses_when_set() {
        with_db(|| {
            unsafe { env::set_var("PORT", "9000"); }
            assert_eq!(Config::from_env().unwrap().port, 9000);
        });
    }

    #[test]
    fn host_defaults_to_0_0_0_0() {
        with_db(|| assert_eq!(Config::from_env().unwrap().host, "0.0.0.0"));
    }

    #[test]
    fn host_parses_when_set() {
        with_db(|| {
            unsafe { env::set_var("HOST", "127.0.0.1"); }
            assert_eq!(Config::from_env().unwrap().host, "127.0.0.1");
        });
    }

    #[test]
    fn database_url_is_required() {
        with_db(|| {
            assert!(Config::from_env().unwrap().database_url.contains("postgres://"));
        });
    }
}

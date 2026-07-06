//! Process-level configuration: env vars + db_path resolution.
//!
//! Parity with `src/database/database.service.ts:resolveDatabasePath`
//! and `.env.example`. db_path priority: `WEBUI_DB_PATH` (trimmed, non-empty)
//! > `CODEX_HOME/codex-webui.sqlite` > `~/.codex/codex-webui.sqlite`.
//! `WEBUI_API_KEY` is required; absent/empty fails startup.

use anyhow::{anyhow, Result};
use std::env;
use std::path::PathBuf;

pub struct Config {
    pub webui_api_key: String,
    pub port: u16,
    pub openai_api_key: Option<String>,
    pub log_level: String,
    pub codex_bin: String,
    pub codex_home: Option<String>,
    pub db_path: String,
}

const DEFAULT_DB_FILENAME: &str = "codex-webui.sqlite";

impl Config {
    pub fn from_env() -> Result<Self> {
        let webui_api_key = env::var("WEBUI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("WEBUI_API_KEY is required"))?;

        let port: u16 = env::var("PORT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|p| p.parse::<u16>())
            .transpose()?
            .unwrap_or(8172);

        let codex_home = env::var("CODEX_HOME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let openai_api_key = env::var("OPENAI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        let codex_bin = env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());

        Ok(Self {
            webui_api_key,
            port,
            openai_api_key,
            log_level,
            codex_bin,
            codex_home: codex_home.clone(),
            db_path: resolve_db_path(env::var("WEBUI_DB_PATH").ok(), codex_home.as_deref()),
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
    // Mirror Node's os.homedir(): USERPROFILE first on Windows, HOME elsewhere.
    // Critical for this Windows dev box where Git Bash exports HOME as a POSIX path.
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
        // Last-resort Windows fallback: HOMEDRIVE + HOMEPATH.
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

    // env::set_var/remove_var are unsafe since Rust 1.86; serialize test access.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VARS: &[&str] = &[
        "WEBUI_DB_PATH",
        "CODEX_HOME",
        "WEBUI_API_KEY",
        "PORT",
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
        unsafe { env::set_var("WEBUI_API_KEY", "k"); }
        unsafe { env::set_var("CODEX_HOME", "/tmp/ignored"); }
        unsafe { env::set_var("WEBUI_DB_PATH", "/explicit/a.sqlite"); }
        let c = Config::from_env().unwrap();
        assert_eq!(c.db_path, "/explicit/a.sqlite");
    }

    #[test]
    fn db_path_uses_codex_home_when_no_explicit() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "k"); }
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
        unsafe { env::set_var("WEBUI_API_KEY", "k"); }
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
        unsafe { env::set_var("WEBUI_API_KEY", "k"); }
        assert_eq!(Config::from_env().unwrap().port, 8172);
    }

    #[test]
    fn port_parses_when_set() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "k"); }
        unsafe { env::set_var("PORT", "9000"); }
        assert_eq!(Config::from_env().unwrap().port, 9000);
    }
}
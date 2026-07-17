//! 进程级配置：环境变量。
//!
//! `WEBUI_API_KEY` 为必填项；`DATABASE_URL` 必填(postgres:// 或 mysql://)。
//! 全 SeaORM 迁移后,数据库连接由 `DatabaseConnection::connect` 处理,无降级。
//!
//! 多机(M4)相关:`NODE_ROLE`(ingress/worker/both)、内网 RPC 监听地址、本 worker 对外
//! 可达 RPC 地址。进程池调度参数(上限/LRU/扩进程/并发)与快照参数在此统一定义。

use anyhow::{anyhow, Result};
use std::env;

/// 节点角色:接入层(路由 + 转发)/ 工作层(进程池 + 内网 RPC)/ 兼容单机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    Ingress,
    Worker,
    /// 单机兼容:既路由又本地执行。
    Both,
}

impl NodeRole {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ingress" => Self::Ingress,
            "worker" => Self::Worker,
            _ => Self::Both,
        }
    }

    pub fn is_ingress(self) -> bool {
        matches!(self, Self::Ingress | Self::Both)
    }

    pub fn is_worker(self) -> bool {
        matches!(self, Self::Worker | Self::Both)
    }
}

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
    /// 上一代主密钥(M6 轮转):若设置,解密优先用 master_key,失败再用本密钥回退。
    pub master_key_previous: Option<String>,
    /// Redis 连接串(M4 分布式协调);未设置则禁用跨节点功能。
    pub redis_url: Option<String>,
    // ── 多机拓扑(M4)──────────────────────────────────────────────────────
    /// 节点角色:ingress / worker / both(默认)。
    pub node_role: NodeRole,
    /// 内网 RPC 监听地址(worker 对 ingress 暴露;ingress 转发用)。
    pub internal_rpc_host: String,
    /// 内网 RPC 监听端口(默认 port + 1)。
    pub internal_rpc_port: u16,
    /// 本 worker 对外可达的内网 RPC base url(如 http://10.0.0.5:8173)。
    /// 未设置则由 internal_rpc_host:port 推导;多机部署必须显式配置(避免 0.0.0.0 不可达)。
    pub worker_rpc_url: Option<String>,
    /// 本节点稳定 ID(默认随机 UUID;多机部署建议显式设置保证跨重启一致)。
    pub worker_id: Option<String>,
    // ── 进程池调度(M3/M4)───────────────────────────────────────────────
    /// 每 team 进程上限(默认 4)。
    pub max_processes_per_team: usize,
    /// 全局进程上限(默认 25)。
    pub max_global_processes: usize,
    /// 空闲回收阈值秒(默认 900 = 15min)。
    pub idle_evict_secs: u64,
    /// 单进程并发上限 semaphore(默认 20)。
    pub max_concurrent_per_process: usize,
    /// 并发达到该阈值触发"按 team 扩进程"(默认 8)。
    pub process_scale_threshold: usize,
    // ── 快照(M4 故障恢复)────────────────────────────────────────────────
    /// 快照周期秒(RPO;默认 300 = 5min)。
    pub snapshot_interval_secs: u64,
    /// 快照存储根目录(默认 ~/.codex-webui-snapshots)。
    pub snapshots_root: Option<String>,
    // ── 计费/配额(M6 预留)───────────────────────────────────────────────
    /// 每 team 每小时 turn 配额(0 = 不限;注册 team 时写入 team_quotas 默认值)。
    pub default_turn_quota_hourly: i64,
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

        let master_key_previous = env::var("MASTER_KEY_PREVIOUS")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let redis_url = env::var("REDIS_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // ── 多机拓扑 ──
        let node_role = NodeRole::parse(&env::var("NODE_ROLE").unwrap_or_else(|_| "both".into()));

        let internal_rpc_host = env::var("INTERNAL_RPC_HOST")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let internal_rpc_port: u16 = env::var("INTERNAL_RPC_PORT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|p| p.parse::<u16>())
            .transpose()?
            .unwrap_or_else(|| port.saturating_add(1).max(1));

        let worker_rpc_url = env::var("WORKER_RPC_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let worker_id = env::var("WORKER_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // ── 进程池调度 ──
        fn parse_usize(name: &str, default: usize) -> Result<usize> {
            env::var(name)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<usize>().map_err(|e| anyhow!("{name} parse error: {e}"))
                })
                .transpose()
                .map(|v| v.unwrap_or(default))
        }
        fn parse_u64(name: &str, default: u64) -> Result<u64> {
            env::var(name)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<u64>().map_err(|e| anyhow!("{name} parse error: {e}"))
                })
                .transpose()
                .map(|v| v.unwrap_or(default))
        }

        let max_processes_per_team = parse_usize("MAX_PROCESSES_PER_TEAM", 4)?;
        let max_global_processes = parse_usize("MAX_GLOBAL_PROCESSES", 25)?;
        let idle_evict_secs = parse_u64("IDLE_EVICT_SECS", 900)?;
        let max_concurrent_per_process = parse_usize("MAX_CONCURRENT_PER_PROCESS", 20)?;
        let process_scale_threshold = parse_usize("PROCESS_SCALE_THRESHOLD", 8)?;
        let snapshot_interval_secs = parse_u64("SNAPSHOT_INTERVAL_SECS", 300)?;
        let default_turn_quota_hourly = env::var("DEFAULT_TURN_QUOTA_HOURLY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<i64>().map_err(|e| anyhow!("DEFAULT_TURN_QUOTA_HOURLY parse error: {e}"))
            })
            .transpose()?
            .unwrap_or(0);

        let snapshots_root = env::var("SNAPSHOTS_ROOT")
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
            master_key_previous,
            redis_url,
            node_role,
            internal_rpc_host,
            internal_rpc_port,
            worker_rpc_url,
            worker_id,
            max_processes_per_team,
            max_global_processes,
            idle_evict_secs,
            max_concurrent_per_process,
            process_scale_threshold,
            snapshot_interval_secs,
            snapshots_root,
            default_turn_quota_hourly,
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
        "MASTER_KEY",
        "MASTER_KEY_PREVIOUS",
        "REDIS_URL",
        "NODE_ROLE",
        "INTERNAL_RPC_HOST",
        "INTERNAL_RPC_PORT",
        "WORKER_RPC_URL",
        "WORKER_ID",
        "INTERNAL_RPC_TOKEN",
        "MEMBERLIST_SEEDS",
        "MEMBERLIST_BIND",
        "MAX_PROCESSES_PER_TEAM",
        "MAX_GLOBAL_PROCESSES",
        "IDLE_EVICT_SECS",
        "MAX_CONCURRENT_PER_PROCESS",
        "PROCESS_SCALE_THRESHOLD",
        "SNAPSHOT_INTERVAL_SECS",
        "SNAPSHOTS_ROOT",
        "DEFAULT_TURN_QUOTA_HOURLY",
    ];

    fn clear() {
        for k in VARS {
            unsafe { env::remove_var(k); }
        }
    }

    /// 测试前必须设置必选 env。
    fn with_db<F: FnOnce() -> T, T>(f: F) -> T {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
        unsafe { env::set_var("INTERNAL_RPC_TOKEN", "0123456789abcdef0123456789abcdef"); }
        unsafe { env::set_var("WORKER_ID", "node-a-staaaaaaaaable"); }
        f()
    }

    fn set_required_env() {
        unsafe { env::set_var("WEBUI_API_KEY", "0123456789abcdef"); }
        unsafe { env::set_var("DATABASE_URL", "postgres://dummy"); }
        unsafe { env::set_var("INTERNAL_RPC_TOKEN", "0123456789abcdef0123456789abcdef"); }
        unsafe { env::set_var("WORKER_ID", "node-a-staaaaaaaaable"); }
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

    #[test]
    fn node_role_defaults_to_both() {
        with_db(|| assert_eq!(Config::from_env().unwrap().node_role, NodeRole::Both));
    }

    #[test]
    fn node_role_parses_variants() {
        with_db(|| {
            unsafe { env::set_var("NODE_ROLE", "ingress"); }
            assert_eq!(Config::from_env().unwrap().node_role, NodeRole::Ingress);
            unsafe { env::set_var("NODE_ROLE", "Worker"); }
            assert_eq!(Config::from_env().unwrap().node_role, NodeRole::Worker);
            unsafe { env::set_var("NODE_ROLE", "nonsense"); }
            assert_eq!(Config::from_env().unwrap().node_role, NodeRole::Both);
        });
    }

    #[test]
    fn internal_rpc_port_defaults_to_port_plus_one() {
        with_db(|| {
            assert_eq!(Config::from_env().unwrap().internal_rpc_port, 8173);
            unsafe { env::set_var("PORT", "9000"); }
            assert_eq!(Config::from_env().unwrap().internal_rpc_port, 9001);
        });
    }

    #[test]
    fn pool_defaults() {
        with_db(|| {
            let c = Config::from_env().unwrap();
            assert_eq!(c.max_processes_per_team, 4);
            assert_eq!(c.max_global_processes, 25);
            assert_eq!(c.idle_evict_secs, 900);
            assert_eq!(c.max_concurrent_per_process, 20);
            assert_eq!(c.process_scale_threshold, 8);
        });
    }

    #[test]
    fn snapshot_defaults() {
        with_db(|| {
            let c = Config::from_env().unwrap();
            assert_eq!(c.snapshot_interval_secs, 300);
            assert!(c.snapshots_root.is_none());
        });
    }

    // ── 多副本 HA 修复(spec §2.4 / §9.3)──────────────────────────

    #[test]
    fn internal_token_missing_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        unsafe { env::remove_var("INTERNAL_RPC_TOKEN"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn internal_token_too_short_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        unsafe { env::set_var("INTERNAL_RPC_TOKEN", "short"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn internal_rpc_host_defaults_to_127() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        assert_eq!(Config::from_env().unwrap().internal_rpc_host, "127.0.0.1");
    }

    #[test]
    fn worker_id_missing_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        unsafe { env::remove_var("WORKER_ID"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn worker_id_too_short_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        unsafe { env::set_var("WORKER_ID", "short"); }
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn memberlist_seeds_parse_csv() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        unsafe { env::set_var("MEMBERLIST_SEEDS", "10.0.0.1:7946, 10.0.0.2:7946"); }
        let c = Config::from_env().unwrap();
        assert_eq!(c.memberlist_seeds, vec!["10.0.0.1:7946", "10.0.0.2:7946"]);
    }

    #[test]
    fn memberlist_bind_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        set_required_env();
        assert_eq!(Config::from_env().unwrap().memberlist_bind, "0.0.0.0:7946");
    }
}

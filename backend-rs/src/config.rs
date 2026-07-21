//! 进程级配置:**仅从 TOML 文件加载**。
//!
//! ## 设计原则
//!
//! - **无环境变量回退路径**(避免与 secret manager / k8s ConfigMap 混用出错)
//! - **分块结构**:按业务域组织(server / cluster / database / redis / codex / auth /
//!   security / memberlist / snapshot / quota / otel);DB/Redis 用结构化字段
//!   (host/port/user/password)而非平铺 URL,运维侧可拆开管理。
//! - **可选段显式 enable 开关**:`enable = true/false`,默认 false。`enable = false` →
//!   字段忽略(忽略 Option<T>,未提供视为 None);`enable = true` → 校验内部必填字段并启用。
//!
//! ## 实现
//!
//! 所有子结构 `#[derive(serde::Deserialize)]`,直接 `toml::from_str` 反序列化;
//! 字段长度校验、端口范围、必填检查在 `validate()` 里集中做,不再写手写 helper。

use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────
// 节点角色
// ─────────────────────────────────────────────────────────────

// 注:历史上 NodeRole 分 ingress / worker / both 三种,但实际部署中**每个节点
// 都是 ingress + worker 一体**(每个节点同时跑 HTTP 路由、内网 RPC、codex 进程池、
// memberlist/redis 探活),不再按角色分流。详见 docs/superpowers/specs/2026-07-16-...
// 如未来需要单角色节点(纯 ingress 路由或纯 worker 执行),再恢复 NodeRole 字段。

// ─────────────────────────────────────────────────────────────
// 子结构:全部 #[derive(Deserialize)] 直接 TOML 映射
// ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
pub struct ApiConfig {
    pub webui_api_key: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    pub api: ApiConfig,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8172
}
fn default_log_level() -> String {
    if cfg!(debug_assertions) {
        "debug".to_string()
    } else {
        "info".to_string()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClusterConfig {
    #[serde(default = "default_internal_rpc_host")]
    pub internal_rpc_host: String,
    /// 默认 = `server.port + 1`(运行时派生,toml 里可显式覆盖)。
    pub internal_rpc_port: Option<u16>,
    pub worker_id: String,
    /// 多机部署 worker_rpc_url(显式 enable=true 才用,否则 None)。
    #[serde(default)]
    pub worker_rpc_url_enabled: bool,
    #[serde(default)]
    pub worker_rpc_url: Option<String>,
}

fn default_internal_rpc_host() -> String {
    "127.0.0.1".to_string()
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct MemberlistConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub memberlist_seeds: Vec<String>,
    #[serde(default = "default_memberlist_bind")]
    pub memberlist_bind: String,
}

fn default_memberlist_bind() -> String {
    "0.0.0.0:7946".to_string()
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseDriver {
    /// PostgreSQL(默认;当前 backend 唯一支持的方言)。
    #[default]
    #[serde(alias = "pg", alias = "postgres", alias = "postgresql")]
    Postgres,
    /// MySQL(预留;待 backend 适配 sqlx-mysql 后启用)。
    #[serde(alias = "mariadb")]
    Mysql,
}

impl DatabaseDriver {
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub driver: DatabaseDriver,
    pub host: String,
    #[serde(default = "default_db_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub name: String,
    #[serde(default)]
    pub ssl_mode: Option<String>,
}

fn default_db_port() -> u16 {
    5432
}

impl DatabaseConfig {
    pub fn url(&self) -> String {
        let mut url = format!(
            "{}://{}:{}@{}:{}/{}",
            self.driver.scheme(),
            self.user,
            self.password,
            self.host,
            self.port,
            self.name,
        );
        if let Some(ssl) = &self.ssl_mode {
            url.push_str("?sslmode=");
            url.push_str(ssl);
        }
        url
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct RedisConfig {
    #[serde(default)]
    pub enable: bool,
    pub host: Option<String>,
    #[serde(default = "default_redis_port")]
    pub port: u16,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub db: Option<u8>,
}

fn default_redis_port() -> u16 {
    6379
}

impl RedisConfig {
    pub fn url(&self) -> Option<String> {
        if !self.enable {
            return None;
        }
        let host = self.host.as_deref()?.trim();
        if host.is_empty() {
            return None;
        }
        let auth = match &self.password {
            Some(p) if !p.is_empty() => format!(":{}@", p),
            _ => String::new(),
        };
        let db_suffix = match self.db {
            Some(d) => format!("/{}", d),
            None => String::new(),
        };
        Some(format!(
            "redis://{}{}:{}{}",
            auth, host, self.port, db_suffix
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct CodexHomeConfig {
    #[serde(default)]
    pub enable: bool,
    pub path: Option<String>,
}

/// webui 文件工作区根(users/ teams/ 的父目录)。
/// 默认回落 codex_home(向后兼容);显式 [workspace] enable=true 才独立。
#[derive(Clone, Debug, Deserialize, Default)]
pub struct WorkspaceRootConfig {
    #[serde(default)]
    pub enable: bool,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct CodexOpenaiConfig {
    #[serde(default)]
    pub enable: bool,
    pub key: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexConfig {
    #[serde(default = "default_codex_bin")]
    pub bin: String,
    #[serde(default)]
    pub home: CodexHomeConfig,
    #[serde(default)]
    pub openai_api_key: CodexOpenaiConfig,
    /// 单进程全局并发上限(单 stdin/stdout 管道防过载;默认 32)。
    /// 每节点只跑一个 codex 进程,所有 thread 的请求都走同一管道,
    /// 故需在管理器层用信号量限流,避免 stdin/stdout 被打爆。
    #[serde(default = "default_codex_max_concurrent")]
    pub max_concurrent: usize,
}

fn default_codex_bin() -> String {
    "codex".to_string()
}

/// CodexConfig.max_concurrent 的默认值(32):单进程全局并发上限,
/// 替代已删除的 per-team 进程池并发控制。
fn default_codex_max_concurrent() -> usize {
    32
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AuthMasterKeyConfig {
    #[serde(default)]
    pub enable: bool,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AuthMasterKeyPreviousConfig {
    #[serde(default)]
    pub enable: bool,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AuthConfig {
    #[serde(default)]
    pub master_key: AuthMasterKeyConfig,
    #[serde(default)]
    pub master_key_previous: AuthMasterKeyPreviousConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SecurityConfig {
    pub internal_rpc_token: String,
    pub internal_hook_token: String,
    /// 平台超级管理员邮箱列表(启动期 bootstrap:把这些已存在用户置 is_platform_admin=true)。
    /// 仅用于初始化首个管理员;之后以 DB 为准(不在列表里的不会被撤销)。
    #[serde(default)]
    pub admin_emails: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SnapshotRootConfig {
    #[serde(default)]
    pub enable: bool,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SnapshotConfig {
    #[serde(default = "default_snapshot_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub root: SnapshotRootConfig,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_snapshot_interval(),
            root: SnapshotRootConfig::default(),
        }
    }
}

fn default_snapshot_interval() -> u64 {
    300
}

#[derive(Clone, Debug, Deserialize)]
pub struct QuotaConfig {
    #[serde(default)]
    pub default_turn_quota_hourly: i64,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            default_turn_quota_hourly: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct OtelConfig {
    #[serde(default)]
    pub enable: bool,
    pub endpoint: Option<String>,
}

// ─────────────────────────────────────────────────────────────
// 顶层 Config
// ─────────────────────────────────────────────────────────────

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub memberlist: MemberlistConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub redis: RedisConfig,
    pub codex: CodexConfig,
    /// webui 文件工作区根(顶层,与 [codex] 平级;默认回落 codex_home)。
    #[serde(default)]
    pub workspace: WorkspaceRootConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    pub security: SecurityConfig,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub otel: OtelConfig,
}

// 自定义 Debug(redact 密钥/secret 字段,避免日志泄露)。
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("cluster", &self.cluster)
            .field("memberlist", &self.memberlist)
            .field("database", &self.database)
            .field("redis", &self.redis)
            .field("codex", &self.codex)
            .field("workspace", &self.workspace)
            .field("auth", &"<redacted>")
            .field("security", &"<redacted>")
            .field("snapshot", &self.snapshot)
            .field("quota", &self.quota)
            .field("otel", &self.otel)
            .finish()
    }
}

impl Config {
    /// 主入口:从默认路径加载 TOML。
    pub fn load() -> Result<Self> {
        let path = Self::locate_config_file().ok_or_else(|| {
            anyhow!(
                "config file not found in any of: \
                 $CODEX_WEBUI_CONFIG / $CODEX_HOME/config.toml / ./config.toml / $HOME/.codex-webui/config.toml. \
                 Copy backend-rs/config.toml.example to one of these paths and fill in values."
            )
        })?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(anyhow!(
                "config file not found: {}. Copy backend-rs/config.toml.example to this path.",
                path.display()
            ));
        }
        let raw =
            std::fs::read_to_string(path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .map_err(|e| anyhow!("config file {} parse error: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn locate_config_file() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("CODEX_WEBUI_CONFIG") {
            let path = PathBuf::from(p.trim());
            if !path.as_os_str().is_empty() && path.exists() {
                return Some(path);
            }
        }
        None
    }

    /// 集中校验长度/范围/必填,serde 不做的语义检查放这里。
    fn validate(&self) -> Result<()> {
        if self.server.api.webui_api_key.len() < 16 {
            return Err(anyhow!(
                "server.api.webui_api_key must be ≥16 characters (current: {})",
                self.server.api.webui_api_key.len()
            ));
        }
        if self.cluster.worker_id.len() < 16 {
            return Err(anyhow!(
                "cluster.worker_id must be ≥16 bytes (current: {}); recommend hostname or k8s pod uid",
                self.cluster.worker_id.len()
            ));
        }
        if self.security.internal_rpc_token.len() < 32 {
            return Err(anyhow!(
                "security.internal_rpc_token must be ≥32 bytes (current: {}); generate with `openssl rand -hex 32`",
                self.security.internal_rpc_token.len()
            ));
        }
        if self.security.internal_hook_token.len() < 32 {
            return Err(anyhow!(
                "security.internal_hook_token must be ≥32 bytes (current: {}); generate with `openssl rand -hex 32`",
                self.security.internal_hook_token.len()
            ));
        }
        // enabled=true 但 url 为空
        if self.cluster.worker_rpc_url_enabled
            && self
                .cluster
                .worker_rpc_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!(
                "cluster.worker_rpc_url_enabled = true but `worker_rpc_url` is empty"
            ));
        }
        // memberlist enable=true 但 seeds 为空 → 仍然 ok(可能用户故意空 seeds 走 fallback)
        // redis enable=true 但 host 缺失
        if self.redis.enable
            && self
                .redis
                .host
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!(
                "redis.enable = true but `host` is empty (omit the [redis] section entirely or set enable = false)"
            ));
        }
        // otel enable=true 但 endpoint 缺失
        if self.otel.enable
            && self
                .otel
                .endpoint
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!("otel.enable = true but `endpoint` is empty"));
        }
        // codex.home / openai_api_key / auth.master_key 启用但 value 缺失
        if self.codex.home.enable
            && self
                .codex
                .home
                .path
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!("codex.home.enable = true but `path` is empty"));
        }
        // workspace.enable=true 但 path 缺失
        if self.workspace.enable
            && self
                .workspace
                .path
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!("workspace.enable = true but `path` is empty"));
        }
        if self.codex.openai_api_key.enable
            && self
                .codex
                .openai_api_key
                .key
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!(
                "codex.openai_api_key.enable = true but `key` is empty"
            ));
        }
        if self.auth.master_key.enable
            && self
                .auth
                .master_key
                .value
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!(
                "auth.master_key.enable = true but `value` is empty"
            ));
        }
        if self.auth.master_key_previous.enable
            && self
                .auth
                .master_key_previous
                .value
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(anyhow!(
                "auth.master_key_previous.enable = true but `value` is empty"
            ));
        }
        Ok(())
    }

    pub fn database_url(&self) -> String {
        self.database.url()
    }

    pub fn redis_url(&self) -> Option<String> {
        self.redis.url()
    }

    /// 最终生效的 internal_rpc_port(server.port + 1 兜底)。
    pub fn internal_rpc_port(&self) -> u16 {
        self.cluster
            .internal_rpc_port
            .unwrap_or_else(|| self.server.port.saturating_add(1).max(1))
    }

    /// 便捷访问器(常用顶层字段):webui_api_key。
    pub fn webui_api_key(&self) -> &str {
        &self.server.api.webui_api_key
    }

    /// 最终生效的 master_key(显式启用优先,否则回退 webui_api_key)。
    pub fn effective_master_key(&self) -> &str {
        if self.auth.master_key.enable {
            if let Some(k) = &self.auth.master_key.value {
                return k;
            }
        }
        &self.server.api.webui_api_key
    }

    /// 便捷访问器:codex home(启用才返回,否则 None)。
    pub fn codex_home(&self) -> Option<&str> {
        if self.codex.home.enable {
            self.codex.home.path.as_deref()
        } else {
            None
        }
    }

    /// webui 文件工作区根(users/ teams/ 的父目录)。
    /// 默认回落 codex_home(向后兼容);显式 [workspace] enable=true 才独立。
    pub fn workspace_root(&self) -> Option<&str> {
        if self.workspace.enable {
            self.workspace.path.as_deref()
        } else {
            self.codex_home()
        }
    }

    /// 便捷访问器:codex bin(总有值,有默认值)。
    pub fn codex_bin(&self) -> &str {
        &self.codex.bin
    }

    /// 便捷访问器:单进程全局并发上限(默认 32)。
    /// CodexProcessManager 据此构造 `Semaphore::new(max_concurrent)`,
    /// 在 request() 入口 acquire 许可,防止单 stdin/stdout 管道过载。
    pub fn codex_max_concurrent(&self) -> usize {
        self.codex.max_concurrent
    }

    /// 便捷访问器:codex 全局 OpenAI key(启用才返回)。
    pub fn codex_openai_api_key(&self) -> Option<&str> {
        if self.codex.openai_api_key.enable {
            self.codex.openai_api_key.key.as_deref()
        } else {
            None
        }
    }

    /// 便捷访问器:master_key_previous(启用才返回)。
    pub fn master_key_previous(&self) -> Option<&str> {
        if self.auth.master_key_previous.enable {
            self.auth.master_key_previous.value.as_deref()
        } else {
            None
        }
    }

    /// 便捷访问器:worker_rpc_url(启用才返回)。
    pub fn worker_rpc_url(&self) -> Option<&str> {
        if self.cluster.worker_rpc_url_enabled {
            self.cluster.worker_rpc_url.as_deref()
        } else {
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试间无需并发锁:每次用 `temp_dir + uuid` 唯一路径,不冲突。
    fn write_cfg(content: &str) -> std::path::PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("codex-webui-cfg-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, content).unwrap();
        tmp
    }

    fn minimal_toml() -> &'static str {
        r#"
[server]
host = "0.0.0.0"
port = 8182

[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "127.0.0.1"
port = 5432
user = "codex"
password = "codex"
name = "codex"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#
    }

    #[test]
    fn load_minimal_required_fields() {
        let path = write_cfg(minimal_toml());
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.server.host, "0.0.0.0");
        assert_eq!(c.server.port, 8182);
        assert_eq!(c.server.api.webui_api_key, "0123456789abcdef");
        assert_eq!(c.cluster.worker_id, "node-a-staaaaaaaaable");
        assert_eq!(c.internal_rpc_port(), 8183); // port+1 兜底
        assert_eq!(c.database.host, "127.0.0.1");
        assert_eq!(
            c.database_url(),
            "postgres://codex:codex@127.0.0.1:5432/codex"
        );
        // 默认值
        assert_eq!(c.snapshot.interval_secs, 300);
        assert_eq!(c.quota.default_turn_quota_hourly, 0);
        // 可选段默认 disable
        assert!(!c.memberlist.enable);
        assert!(!c.redis.enable);
        assert!(c.redis_url().is_none());
        assert!(!c.otel.enable);
        assert!(!c.codex.home.enable);
        assert!(!c.codex.openai_api_key.enable);
        assert!(!c.auth.master_key.enable);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn redis_enable_explicit() {
        let toml = format!(
            r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[redis]
enable = true
host = "127.0.0.1"
port = 6379
password = "redispwd"
db = 1

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#
        );
        let path = write_cfg(&toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.redis.enable);
        assert_eq!(
            c.redis_url().as_deref(),
            Some("redis://:redispwd@127.0.0.1:6379/1")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn redis_enable_false_ignores_host() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[redis]
enable = false
host = "127.0.0.1"
port = 6379

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(!c.redis.enable);
        // enable=false,字段仍被反序列化但 url() 返回 None
        assert!(c.redis_url().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn redis_enable_true_missing_host_is_error() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[redis]
enable = true
port = 6379

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let r = Config::load_from(&path);
        assert!(r.is_err());
        let msg = format!("{:?}", r.unwrap_err());
        assert!(
            msg.contains("redis.enable = true but `host` is empty"),
            "msg={msg}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn otel_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[otel]
enable = true
endpoint = "http://otel.example.com:4317"

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.otel.enable);
        assert_eq!(
            c.otel.endpoint.as_deref(),
            Some("http://otel.example.com:4317")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn auth_master_key_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "webui-api-key-default"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[auth.master_key]
enable = true
value = "real-master-key"

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.auth.master_key.enable);
        assert_eq!(c.auth.master_key.value.as_deref(), Some("real-master-key"));
        assert_eq!(c.effective_master_key(), "real-master-key");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn auth_master_key_falls_back_to_webui_api_key() {
        let path = write_cfg(minimal_toml());
        let c = Config::load_from(&path).unwrap();
        assert!(!c.auth.master_key.enable);
        assert_eq!(c.effective_master_key(), "0123456789abcdef");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn codex_home_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex.home]
enable = true
path = "/var/codex"

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.codex.home.enable);
        assert_eq!(c.codex.home.path.as_deref(), Some("/var/codex"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn codex_openai_api_key_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex.openai_api_key]
enable = true
key = "sk-global"

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.codex.openai_api_key.enable);
        assert_eq!(c.codex.openai_api_key.key.as_deref(), Some("sk-global"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn memberlist_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[memberlist]
enable = true
memberlist_seeds = ["a:7946", "b:7946"]
memberlist_bind = "0.0.0.0:7946"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.memberlist.enable);
        assert_eq!(c.memberlist.memberlist_seeds, vec!["a:7946", "b:7946"]);
        assert_eq!(c.memberlist.memberlist_bind, "0.0.0.0:7946");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mysql_database() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
driver = "mysql"
host = "mysql.example.com"
port = 3306
user = "app"
password = "secret"
name = "production"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.database.driver, DatabaseDriver::Mysql);
        assert_eq!(
            c.database_url(),
            "mysql://app:secret@mysql.example.com:3306/production"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn webui_api_key_too_short_is_error() {
        let toml = r#"
[server.api]
webui_api_key = "short"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let r = Config::load_from(&path);
        assert!(r.is_err());
        let msg = format!("{:?}", r.unwrap_err());
        assert!(msg.contains("webui_api_key must be ≥16"), "msg={msg}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn worker_id_too_short_is_error() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "short"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let r = Config::load_from(&path);
        assert!(r.is_err());
        let msg = format!("{:?}", r.unwrap_err());
        assert!(msg.contains("worker_id must be ≥16"), "msg={msg}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn internal_rpc_token_too_short_is_error() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "short"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let r = Config::load_from(&path);
        assert!(r.is_err());
        let msg = format!("{:?}", r.unwrap_err());
        assert!(msg.contains("internal_rpc_token must be ≥32"), "msg={msg}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn internal_rpc_port_default_is_port_plus_one() {
        let toml = r#"
[server]
port = 9000

[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.internal_rpc_port(), 9001);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn internal_rpc_port_explicit_overrides_default() {
        let toml = r#"
[server]
port = 9000

[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"
internal_rpc_port = 9999

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.internal_rpc_port(), 9999);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_invalid_toml_is_error() {
        let path = write_cfg("this is = not valid = toml [[[");
        let r = Config::load_from(&path);
        assert!(r.is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_missing_file_is_error() {
        let path = std::env::temp_dir().join("definitely-not-exists.toml");
        let r = Config::load_from(&path);
        assert!(r.is_err());
    }

    #[test]
    fn cluster_worker_rpc_url_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"
worker_rpc_url_enabled = true
worker_rpc_url = "http://10.0.0.1:3334"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.cluster.worker_rpc_url_enabled);
        assert_eq!(
            c.cluster.worker_rpc_url.as_deref(),
            Some("http://10.0.0.1:3334")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn snapshot_root_enable_explicit() {
        let toml = r#"
[server.api]
webui_api_key = "0123456789abcdef"

[cluster]
worker_id = "node-a-staaaaaaaaable"

[database]
host = "h"
user = "u"
name = "n"

[codex]

[snapshot]
interval_secs = 600

[snapshot.root]
enable = true
path = "/var/snapshots"

[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(toml);
        let c = Config::load_from(&path).unwrap();
        assert!(c.snapshot.root.enable);
        assert_eq!(c.snapshot.root.path.as_deref(), Some("/var/snapshots"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn security_admin_emails_default_empty_and_explicit() {
        // 默认:[security] 段不写 admin_emails → 反序列化为空 Vec。
        let base = r#"
[server.api]
webui_api_key = "0123456789abcdef"
[cluster]
worker_id = "node-a-staaaaaaaaable"
[database]
host = "h"
user = "u"
name = "n"
[codex]
[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
"#;
        let path = write_cfg(base);
        let c = Config::load_from(&path).unwrap();
        assert!(c.security.admin_emails.is_empty(), "default should be empty");
        std::fs::remove_file(&path).ok();

        // 显式:admin_emails 放进 [security] 段内 → 解析为非空 Vec。
        let with_admins = r#"
[server.api]
webui_api_key = "0123456789abcdef"
[cluster]
worker_id = "node-a-staaaaaaaaable"
[database]
host = "h"
user = "u"
name = "n"
[codex]
[security]
internal_rpc_token = "0123456789abcdef0123456789abcdef"
internal_hook_token = "0123456789abcdef0123456789abcdef"
admin_emails = ["a@example.com", "b@example.com"]
"#;
        let path = write_cfg(with_admins);
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.security.admin_emails, vec!["a@example.com", "b@example.com"]);
        std::fs::remove_file(&path).ok();
    }
}

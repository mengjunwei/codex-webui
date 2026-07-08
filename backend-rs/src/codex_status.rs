//! Codex 就绪状态聚合服务 —— 对齐 `src/codex/codex-status.service.ts`。
//!
//! 并行执行 `account/read` + `config/read{includeLayers}` + `model/list` 探针，
//! 结合进程管理器的 initialize 结果，聚合出 `/codex/status` 响应：
//! appServer / initialize / account / config / provider / models / runtime。
//!
//! 缓存：就绪/降级 30s，不可用 5s；并发未命中共享一次刷新（single-flight）。
//! 配置写入 / 登录登出后调用 `invalidate()` 失效缓存。

use crate::codex::jsonrpc::CodexJsonRpcClient;
use crate::codex::CodexProcessManager;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const READY_CACHE_TTL_MS: u64 = 30_000;
const UNAVAILABLE_CACHE_TTL_MS: u64 = 5_000;

/// 内置 provider → 环境变量名映射（对齐 TS PROVIDER_ENV_KEYS）。
fn builtin_provider_env_key(id: &str) -> Option<&'static str> {
    match id.trim().to_ascii_lowercase().as_str() {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "azure" => Some("AZURE_OPENAI_API_KEY"),
        "gemini" => Some("GOOGLE_API_KEY"),
        _ => None,
    }
}

/// 单个探针的结果（成功带数据，失败带错误对象）。
struct ProbeResult {
    ok: bool,
    data: Option<Value>,
    error: Option<Value>,
}

impl ProbeResult {
    fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(json!({ "code": "RPC_ERROR", "message": message.into() })),
        }
    }
}

/// Skipped 错误（app-server 不可用时跳过的探针）。
fn skipped_error() -> Value {
    json!({ "code": "SKIPPED", "message": "Skipped because Codex app-server is unavailable" })
}

pub struct CodexStatusService {
    process: Arc<CodexProcessManager>,
    /// `(响应, 过期时刻)`。
    cache: std::sync::Mutex<Option<(Value, Instant)>>,
    /// single-flight：并发未命中串行化，第二个调用者拿到锁后重检缓存。
    inflight: tokio::sync::Mutex<()>,
}

impl CodexStatusService {
    pub fn new(process: Arc<CodexProcessManager>) -> Self {
        Self {
            process,
            cache: std::sync::Mutex::new(None),
            inflight: tokio::sync::Mutex::new(()),
        }
    }

    /// 返回聚合状态（带 TTL 缓存 + single-flight）。
    pub async fn get_status(&self) -> Value {
        if let Some(v) = self.fresh_cache() {
            return v;
        }
        let _guard = self.inflight.lock().await;
        // 拿到锁后重检：可能已有其他调用者完成刷新。
        if let Some(v) = self.fresh_cache() {
            return v;
        }
        let value = self.build_status().await;
        let ttl = value
            .get("runtime")
            .and_then(|r| r.get("cacheTtlMs"))
            .and_then(Value::as_u64)
            .unwrap_or(UNAVAILABLE_CACHE_TTL_MS);
        let expiry = Instant::now() + Duration::from_millis(ttl.max(1));
        *self.cache.lock().unwrap() = Some((value.clone(), expiry));
        value
    }

    /// 失效缓存，使下次查询强制刷新。
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }

    /// 仅读取 provider 元数据（对齐 TS getProviderStatus，AccountModule 使用）。
    /// 每次发起 config/read 探针（不走缓存）。
    pub async fn provider_status(&self) -> Value {
        let client = self.process.client().await;
        let config_probe = match client.as_ref() {
            Some(c) => probe(c, "config/read", json!({ "includeLayers": false })).await,
            None => ProbeResult::err("codex app-server is not connected"),
        };
        build_provider_status(&config_probe)
    }

    fn fresh_cache(&self) -> Option<Value> {
        let guard = self.cache.lock().unwrap();
        match guard.as_ref() {
            Some((v, exp)) if *exp > Instant::now() => Some(v.clone()),
            _ => None,
        }
    }

    async fn build_status(&self) -> Value {
        let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let client = self.process.client().await;
        let init_result = self.process.init_result().await;

        let has_client = client.is_some();
        let has_init = init_result.is_some();
        if !has_client || !has_init {
            let (reason, message) = if has_client {
                (
                    "appServerInitializing",
                    "Codex app-server is connected but not initialized",
                )
            } else {
                ("appServerUnavailable", "Codex app-server is not connected")
            };
            return build_unavailable(has_client, has_init, &checked_at, reason, message);
        }

        let client = client.unwrap();
        let (account_probe, config_probe, models_probe) = tokio::join!(
            probe(&client, "account/read", json!({ "refreshToken": false })),
            probe(&client, "config/read", json!({ "includeLayers": true })),
            probe(&client, "model/list", json!({})),
        );

        let account = to_section(&account_probe);
        let config = to_config_section(&config_probe);
        let provider = build_provider_status(&config_probe);
        let models = build_models_status(&models_probe, &config_probe);
        let runtime = build_runtime_status(&checked_at, &account_probe, &config_probe, &provider, &models);

        json!({
            "appServer": { "ok": true, "connected": true, "initialized": true },
            "initialize": { "ok": true, "data": init_result },
            "account": account,
            "config": config,
            "provider": provider,
            "models": models,
            "runtime": runtime,
        })
    }
}

/// 向 app-server 发起一次探针，失败转为 ProbeResult::err。
async fn probe(client: &CodexJsonRpcClient, method: &str, params: Value) -> ProbeResult {
    match client.request(method, Some(params)).await {
        Ok(data) => ProbeResult { ok: true, data: Some(data), error: None },
        Err(e) => ProbeResult::err(e.to_string()),
    }
}

fn to_section(probe: &ProbeResult) -> Value {
    if probe.ok {
        json!({ "ok": true, "data": probe.data.clone().unwrap_or(Value::Null) })
    } else {
        json!({ "ok": false, "error": probe.error.clone().unwrap_or(Value::Null) })
    }
}

/// config 段：从 config/read 的 config 对象中提炼摘要。
fn to_config_section(probe: &ProbeResult) -> Value {
    if !probe.ok {
        return json!({ "ok": false, "error": probe.error.clone().unwrap_or(Value::Null) });
    }
    let config = probe.data.as_ref().and_then(|d| d.get("config")).cloned().unwrap_or(Value::Null);
    let summary = json!({
        "sandboxMode": config.get("sandbox_mode").cloned().unwrap_or(Value::Null),
        "sandboxNetworkAccess": config
            .get("sandbox_workspace_write")
            .and_then(|s| s.get("network_access"))
            .cloned()
            .unwrap_or(Value::Null),
        "approvalPolicy": config.get("approval_policy").cloned().unwrap_or(Value::Null),
        "model": config.get("model").cloned().unwrap_or(Value::Null),
        "modelProvider": config.get("model_provider").cloned().unwrap_or(Value::Null),
    });
    json!({ "ok": true, "data": summary })
}

fn build_provider_status(probe: &ProbeResult) -> Value {
    if !probe.ok {
        return json!({
            "ok": false,
            "id": null, "name": null, "baseUrlMasked": null, "envKey": null, "envPresent": null,
            "error": probe.error.clone().unwrap_or(Value::Null),
        });
    }
    let config = probe.data.as_ref().and_then(|d| d.get("config")).cloned().unwrap_or(Value::Null);
    let provider_id = config.get("model_provider").and_then(Value::as_str);
    let env_key = provider_id.and_then(|id| lookup_provider_env_key(id, &config));
    let base_url = provider_id.and_then(|id| lookup_provider_base_url(id, &config));
    let name = provider_id.and_then(|id| lookup_provider_name(id, &config));
    let env_present = env_key.as_deref().map(is_env_present);
    json!({
        "ok": true,
        "id": provider_id,
        "name": name,
        "baseUrlMasked": mask_base_url(base_url.as_deref()),
        "envKey": env_key,
        "envPresent": env_present,
    })
}

fn build_models_status(models_probe: &ProbeResult, config_probe: &ProbeResult) -> Value {
    if !models_probe.ok {
        return json!({
            "ok": false, "listable": false, "defaultModel": null, "count": 0,
            "error": models_probe.error.clone().unwrap_or(Value::Null),
        });
    }
    let data_arr = models_probe
        .data
        .as_ref()
        .and_then(|d| d.get("data"))
        .and_then(Value::as_array);
    let count = data_arr.map(|a| a.len()).unwrap_or(0);
    let default_model = data_arr.and_then(|arr| find_default_model(arr, config_probe));
    json!({
        "ok": true,
        "listable": count > 0,
        "defaultModel": default_model,
        "count": count,
    })
}

fn build_runtime_status(
    checked_at: &str,
    account_probe: &ProbeResult,
    config_probe: &ProbeResult,
    provider: &Value,
    models: &Value,
) -> Value {
    let mut reasons: Vec<String> = Vec::new();
    let mut blocking = false;

    if !account_probe.ok {
        reasons.push("accountReadFailed".into());
    }
    if !config_probe.ok {
        reasons.push("configReadFailed".into());
    }
    let models_ok = models.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let models_count = models.get("count").and_then(Value::as_u64).unwrap_or(0);
    if !models_ok {
        reasons.push("modelListFailed".into());
        blocking = true;
    } else if models_count == 0 {
        reasons.push("noModelsAvailable".into());
        blocking = true;
    }

    let account_obj = account_probe.data.as_ref().and_then(|d| d.get("account"));
    let has_account = account_probe.ok && account_obj.map(|a| !a.is_null()).unwrap_or(false);
    let login_required = account_probe.ok
        && account_obj.map(|a| a.is_null()).unwrap_or(false)
        && account_probe
            .data
            .as_ref()
            .and_then(|d| d.get("requiresOpenaiAuth"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

    let runtime_override = has_runtime_override(provider, models);

    if login_required && !runtime_override {
        reasons.push("accountLoginRequired".into());
        blocking = true;
    }

    let provider_id = provider.get("id").and_then(Value::as_str);
    if config_probe.ok && !has_account && provider_id.is_none() {
        reasons.push("missingProviderConfig".into());
        blocking = true;
    }

    let env_key = provider.get("envKey").and_then(Value::as_str);
    let env_present = provider.get("envPresent").and_then(Value::as_bool);
    if !has_account && env_key.is_some() && env_present == Some(false) {
        reasons.push("missingEnvKey".into());
        blocking = true;
    }

    if provider_id.is_some() && env_key.is_none() {
        reasons.push("unknownProviderEnvKey".into());
    }

    let status = if blocking {
        "unavailable"
    } else if !reasons.is_empty() {
        "degraded"
    } else {
        "ready"
    };
    let cache_ttl = if status == "unavailable" {
        UNAVAILABLE_CACHE_TTL_MS
    } else {
        READY_CACHE_TTL_MS
    };

    json!({
        "status": status,
        "reasons": reasons,
        "checkedAt": checked_at,
        "cacheTtlMs": cache_ttl,
    })
}

fn has_runtime_override(provider: &Value, models: &Value) -> bool {
    models.get("ok").and_then(Value::as_bool).unwrap_or(false)
        && models.get("listable").and_then(Value::as_bool).unwrap_or(false)
        && provider.get("id").and_then(Value::as_str).is_some()
        && (provider.get("envPresent").and_then(Value::as_bool) == Some(true)
            || provider.get("envKey").and_then(Value::as_str).is_none())
}

fn find_default_model(models: &[Value], config_probe: &ProbeResult) -> Option<Value> {
    if let Some(m) = models.iter().find(|m| m.get("isDefault").and_then(Value::as_bool).unwrap_or(false)) {
        return m.get("model").cloned();
    }
    if let Some(config) = config_probe.data.as_ref().and_then(|d| d.get("config")) {
        if let Some(model) = config.get("model") {
            if !model.is_null() {
                return Some(model.clone());
            }
        }
    }
    if let Some(m) = models.iter().find(|m| !m.get("hidden").and_then(Value::as_bool).unwrap_or(false)) {
        return m.get("model").cloned();
    }
    models.first().and_then(|m| m.get("model").cloned())
}

/// 从 config.model_providers[id] 读取 provider 配置对象。
fn lookup_provider_config(provider_id: &str, config: &Value) -> Option<Value> {
    let providers = config.get("model_providers")?;
    providers.get(provider_id).filter(|v| v.is_object()).cloned()
}

fn lookup_provider_env_key(provider_id: &str, config: &Value) -> Option<String> {
    let configured = lookup_provider_config(provider_id, config)
        .and_then(|p| p.get("env_key").and_then(Value::as_str).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    if configured.is_some() {
        return configured;
    }
    builtin_provider_env_key(provider_id).map(|s| s.to_string())
}

fn lookup_provider_name(provider_id: &str, config: &Value) -> Option<Value> {
    let configured = lookup_provider_config(provider_id, config)
        .and_then(|p| p.get("name").and_then(Value::as_str).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    match configured {
        Some(n) => Some(Value::String(n)),
        None => Some(Value::String(provider_id.to_string())),
    }
}

fn lookup_provider_base_url(provider_id: &str, config: &Value) -> Option<String> {
    lookup_provider_config(provider_id, config)
        .and_then(|p| p.get("base_url").and_then(Value::as_str).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn mask_base_url(value: Option<&str>) -> Option<Value> {
    let v = value?;
    // 简化但等效：剥除 userinfo/query/fragment，掩码 host。
    let parsed = url::Url::parse(v).ok();
    match parsed {
        Some(mut u) => {
            let username = u.username();
            let password = u.password();
            if !username.is_empty() || password.is_some() {
                let _ = u.set_username("");
                let _ = u.set_password(None);
            }
            u.set_query(None);
            u.set_fragment(None);
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            let path = if u.path() == "/" { String::new() } else { u.path().to_string() };
            Some(Value::String(format!("{}//{}{}{}", u.scheme(), mask_host(u.host_str().unwrap_or("")), port, path)))
        }
        None => Some(Value::String(mask_raw_string(v))),
    }
}

fn mask_host(host: &str) -> String {
    if host == "localhost"
        || host.split('.').all(|p| p.parse::<u8>().is_ok()) // IPv4
        || host.len() <= 12
    {
        return host.to_string();
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() >= 3 {
        return format!("{}…{}", parts[0], parts[parts.len() - 2..].join("."));
    }
    format!("{}…{}", &host[..host.len().min(4)], &host[host.len().saturating_sub(4)..])
}

fn mask_raw_string(value: &str) -> String {
    // 对齐 TS maskRawString：value.length > 16 时截断为首 8 字符 + "…" + 末 6 字符。
    if value.len() <= 16 {
        value.to_string()
    } else {
        format!("{}…{}", &value[..8], &value[value.len() - 6..])
    }
}

fn is_env_present(env_key: &str) -> bool {
    std::env::var(env_key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// 构造"不可用"状态响应（对齐 TS buildUnavailableStatus）。
fn build_unavailable(
    connected: bool,
    initialized: bool,
    checked_at: &str,
    reason: &str,
    message: &str,
) -> Value {
    let error = json!({ "code": "APP_SERVER_UNAVAILABLE", "message": message });
    let skipped = skipped_error();
    json!({
        "appServer": { "ok": false, "connected": connected, "initialized": initialized, "error": error.clone() },
        "initialize": { "ok": false, "data": null, "error": error },
        "account": { "ok": false, "error": skipped.clone() },
        "config": { "ok": false, "error": skipped.clone() },
        "provider": {
            "ok": false, "id": null, "name": null, "baseUrlMasked": null, "envKey": null, "envPresent": null,
            "error": skipped.clone(),
        },
        "models": { "ok": false, "listable": false, "defaultModel": null, "count": 0, "error": skipped },
        "runtime": {
            "status": "unavailable",
            "reasons": [reason],
            "checkedAt": checked_at,
            "cacheTtlMs": UNAVAILABLE_CACHE_TTL_MS,
        },
    })
}

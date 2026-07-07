//! Phase 1 所需的最小 Codex JSON-RPC 类型。
//!
//! 完整的 v2 类型化 DTO（TS 中的 `src/codex/dto/v2/`）会在后续阶段按需移植；
//! Phase 1 只需要 initialize 握手以及通用的通知/服务端请求转发
//! （method + params 以 `serde_json::Value` 形式处理）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde_json::Value;

/// JSON-RPC 请求 id。Codex 协议使用从 1 开始递增的整数。
pub type RequestId = u64;

/// `initialize` 请求参数 —— 与 `codex-jsonrpc-client.ts:initialize` 保持对齐。
#[derive(Serialize)]
pub struct InitializeParams {
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    pub capabilities: Capabilities,
}

#[derive(Serialize)]
pub struct ClientInfo {
    pub name: &'static str,
    pub title: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct Capabilities {
    #[serde(rename = "experimentalApi")]
    pub experimental_api: bool,
}

/// `initialize` 响应 —— 仅对需要记录日志的字段做了类型化，其余字段一并捕获。
#[derive(Deserialize, Debug, Clone)]
pub struct InitializeResponse {
    #[serde(rename = "codexHome", default)]
    pub codex_home: Option<String>,
    #[serde(rename = "platformOs", default)]
    pub platform_os: Option<String>,
    /// 用于前向兼容字段的兜底捕获。
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

pub fn default_initialize_params() -> InitializeParams {
    InitializeParams {
        client_info: ClientInfo {
            name: "codex_webui",
            title: "Codex WebUI",
            version: "0.1.0",
        },
        capabilities: Capabilities { experimental_api: true },
    }
}

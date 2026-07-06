//! Minimal Codex JSON-RPC types for Phase 1.
//!
//! Full typed v2 DTOs (`src/codex/dto/v2/` in TS) are ported as needed by later
//! phases; Phase 1 only needs the initialize handshake and generic notification/
//! server-request forwarding (method + params as `serde_json::Value`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde_json::Value;

/// JSON-RPC request id. The Codex protocol uses incrementing integers from 1.
pub type RequestId = u64;

/// `initialize` request params — parity with `codex-jsonrpc-client.ts:initialize`.
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

/// `initialize` response — only the fields we log are typed; the rest is captured.
#[derive(Deserialize, Debug, Clone)]
pub struct InitializeResponse {
    #[serde(rename = "codexHome", default)]
    pub codex_home: Option<String>,
    #[serde(rename = "platformOs", default)]
    pub platform_os: Option<String>,
    /// Catch-all for forward-compatible fields.
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

//! Codex app-server integration — JSON-RPC client + process lifecycle manager.
//!
//! This is the system hub (Phase 1). Everything that talks to the Codex CLI goes
//! through here.

pub mod jsonrpc;
pub mod process;
pub mod types;

pub use jsonrpc::{CodexJsonRpcClient, RpcError};
pub use process::{CodexProcessManager, LifecycleEvent};
pub use types::{default_initialize_params, InitializeParams, InitializeResponse, RequestId};

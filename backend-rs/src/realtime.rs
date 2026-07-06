//! Realtime WebSocket gateway — Socket.IO namespace `/ws`.
//!
//! Parity with `src/threads/threads.gateway.ts` + the (stub) files gateway.
//!
//! - On connect: validate JWT/API key from the auth payload `{token}` (mirrors
//!   ApiKeyGuard ws branch); reject if invalid.
//! - `thread.subscribe` / `thread.unsubscribe` → join/leave rooms `thread:<id>`.
//! - `fs.subscribe` / `fs.unsubscribe` → ack `{ok:true}` (no-op parity; chokidar removed).
//! - Emit tasks forward codex notifications (`codex.notification`), server requests
//!   (`codex.serverRequest`), and lifecycle events (`codex.lifecycle`) to the right rooms.

use crate::auth::AuthService;
use crate::codex::{CodexProcessManager, LifecycleEvent};
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data as SocketData, SocketRef, State};
use socketioxide::SocketIo;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Shared realtime state injected into socketioxide handlers.
#[derive(Clone)]
pub struct RealtimeState {
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
}

/// Build the Socket.IO layer + handle, wiring the `/ws` namespace.
/// The returned `SocketIo` is used to spawn the emit-forwarding tasks.
pub fn build(rt_state: RealtimeState) -> (socketioxide::layer::SocketIoLayer, SocketIo) {
    let (layer, io) = SocketIo::builder()
        .with_state(rt_state)
        .build_layer();
    io.ns("/ws", on_connect);
    (layer, io)
}

/// Per-connection handler: auth + register message handlers.
/// `Data<Value>` extracts the connection auth payload (the client sends `{token}`).
fn on_connect(
    s: SocketRef,
    State(state): State<RealtimeState>,
    SocketData(auth): SocketData<Value>,
) {
    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| strip_bearer(t).to_string());
    let result = state.auth.authenticate_token(token.as_deref(), Some(s.id.as_str()));
    if !result.ok {
        tracing::warn!(socket = %s.id, "rejected unauthenticated socket");
        let _ = s.disconnect();
        return;
    }
    tracing::debug!(socket = %s.id, "client connected");

    s.on("thread.subscribe", on_thread_subscribe);
    s.on("thread.unsubscribe", on_thread_unsubscribe);
    s.on("fs.subscribe", on_ack);
    s.on("fs.unsubscribe", on_ack);
    s.on("codex.serverResponse", on_server_response);
}

fn on_thread_subscribe(s: SocketRef, SocketData(data): SocketData<Value>) {
    let thread_id = data
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if thread_id.is_empty() {
        return;
    }
    let room = format!("thread:{thread_id}");
    let _ = s.join(room.clone());
    tracing::debug!(socket = %s.id, room = %room, "subscribed");
}

fn on_thread_unsubscribe(s: SocketRef, SocketData(data): SocketData<Value>) {
    let thread_id = data
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if thread_id.is_empty() {
        return;
    }
    let room = format!("thread:{thread_id}");
    let _ = s.leave(room.clone());
    tracing::debug!(socket = %s.id, room = %room, "unsubscribed");
}

/// files gateway stub: acknowledge with `{ok:true}` (chokidar watcher removed in TS).
fn on_ack(_: SocketRef, ack: AckSender) {
    let _ = ack.send(&json!({ "ok": true }));
}

/// Legacy WS approval-response path. The authoritative path is the REST endpoint
/// (CAS + forward); this is kept for backward compatibility (detailed wiring deferred).
fn on_server_response(s: SocketRef, SocketData(data): SocketData<Value>) {
    tracing::info!(
        socket = %s.id,
        id = ?data.get("id"),
        "codex.serverResponse via WS (REST respond endpoint preferred)"
    );
}

fn strip_bearer(s: &str) -> &str {
    s.strip_prefix("Bearer ").unwrap_or(s).trim()
}

// ── emit-forwarding tasks ────────────────────────────────────────────────────

/// Spawn tasks that forward codex events to Socket.IO clients.
pub fn spawn_emit_tasks(io: SocketIo, codex: Arc<CodexProcessManager>) {
    spawn_notification_emit(io.clone(), codex.clone());
    spawn_server_request_emit(io.clone(), codex.clone());
    spawn_lifecycle_emit(io, codex);
}

fn spawn_notification_emit(io: SocketIo, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_notifications();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let thread_id = msg
                        .get("params")
                        .and_then(|p| p.get("threadId"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                    let Some(ns) = io.of("/ws") else { continue };
                    let res = if let Some(tid) = thread_id.as_deref() {
                        ns.within(format!("thread:{tid}")).emit("codex.notification", &msg)
                    } else {
                        ns.broadcast().emit("codex.notification", &msg)
                    };
                    if let Err(e) = res {
                        tracing::warn!("emit codex.notification failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("notification emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_server_request_emit(io: SocketIo, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_server_requests();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(req) => {
                    let thread_id = req
                        .get("params")
                        .and_then(|p| p.get("threadId"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                    let out = json!({
                        "id": req.get("id"),
                        "method": req.get("method"),
                        "params": req.get("params"),
                    });
                    let Some(ns) = io.of("/ws") else { continue };
                    let res = if let Some(tid) = thread_id.as_deref() {
                        ns.within(format!("thread:{tid}")).emit("codex.serverRequest", &out)
                    } else {
                        ns.broadcast().emit("codex.serverRequest", &out)
                    };
                    if let Err(e) = res {
                        tracing::warn!("emit codex.serverRequest failed: {e}");
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("server-request emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_lifecycle_emit(io: SocketIo, codex: Arc<CodexProcessManager>) {
    let mut rx = codex.subscribe_lifecycle();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let payload = match &event {
                        LifecycleEvent::Restarting { generation, delay_ms } => json!({
                            "type": "appServerRestarting", "generation": generation, "delayMs": delay_ms
                        }),
                        LifecycleEvent::Ready { generation, restarted } => json!({
                            "type": "appServerReady", "generation": generation, "restarted": restarted
                        }),
                        LifecycleEvent::Unavailable { generation, message } => json!({
                            "type": "appServerUnavailable", "generation": generation, "message": message
                        }),
                    };
                    if let Some(ns) = io.of("/ws") {
                        if let Err(e) = ns.broadcast().emit("codex.lifecycle", &payload) {
                            tracing::warn!("emit codex.lifecycle failed: {e}");
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("lifecycle emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

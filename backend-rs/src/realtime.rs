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
use crate::terminal::TerminalService;
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
    pub terminal: Arc<TerminalService>,
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
    // ── terminal events ──
    s.on("terminal.config", on_term_config);
    s.on("terminal.list", on_term_list);
    s.on("terminal.open", on_term_open);
    s.on("terminal.reconnect", on_term_reconnect);
    s.on("terminal.input", on_term_input);
    s.on("terminal.resize", on_term_resize);
    s.on("terminal.detach", on_term_detach);
    // Detach from all terminals on disconnect.
    let term = state.terminal.clone();
    let sid = s.id.clone();
    s.on_disconnect(move || {
        term.detach(sid.as_str(), None);
    });
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

// ── Terminal handlers ────────────────────────────────────────────────────────

fn on_term_config(_s: SocketRef, State(state): State<RealtimeState>, ack: AckSender) {
    let _ = ack.send(&json!({ "ok": true, "config": state.terminal.get_config_json() }));
}

fn on_term_list(_s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("global");
    let terminals = state.terminal.list(ctx);
    let _ = ack.send(&json!({ "ok": true, "terminals": terminals, "config": state.terminal.get_config_json() }));
}

fn on_term_open(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("global").to_string();
    let cwd = data.get("cwd").and_then(Value::as_str);
    let cols = data.get("cols").and_then(Value::as_u64).map(|n| n as u16);
    let rows = data.get("rows").and_then(Value::as_u64).map(|n| n as u16);
    let title = data.get("title").and_then(Value::as_str);
    match state.terminal.open(s.id.as_str(), &ctx, cwd, cols, rows, title) {
        Ok(meta) => { let _ = ack.send(&json!({ "ok": true, "terminal": meta, "config": state.terminal.get_config_json() })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_reconnect(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    match state.terminal.reconnect(s.id.as_str(), &ctx, &tid) {
        Ok((meta, buffer)) => {
            let state_str: String = buffer.concat();
            let _ = ack.send(&json!({ "ok": true, "terminal": meta, "state": state_str }));
        }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_input(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    let input = data.get("data").and_then(Value::as_str).unwrap_or("");
    match state.terminal.write_input(s.id.as_str(), &ctx, &tid, input) {
        Ok(()) => { let _ = ack.send(&json!({ "ok": true })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_resize(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    let cols = data.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
    let rows = data.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
    match state.terminal.resize(s.id.as_str(), &ctx, &tid, cols, rows) {
        Ok(meta) => { let _ = ack.send(&json!({ "ok": true, "terminal": meta })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_detach(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let tid = data.get("terminalId").and_then(Value::as_str).map(|s| s.to_string());
    state.terminal.detach(s.id.as_str(), tid.as_deref());
    let _ = ack.send(&json!({ "ok": true }));
}

fn strip_bearer(s: &str) -> &str {
    s.strip_prefix("Bearer ").unwrap_or(s).trim()
}

// ── emit-forwarding tasks ────────────────────────────────────────────────────

/// Spawn tasks that forward codex + terminal events to Socket.IO clients.
pub fn spawn_emit_tasks(io: SocketIo, codex: Arc<CodexProcessManager>, terminal: Arc<TerminalService>) {
    spawn_notification_emit(io.clone(), codex.clone());
    spawn_server_request_emit(io.clone(), codex.clone());
    spawn_lifecycle_emit(io.clone(), codex);
    spawn_terminal_output_emit(io.clone(), terminal.clone());
    spawn_terminal_exit_emit(io.clone(), terminal.clone());
    spawn_terminal_closed_emit(io, terminal);
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

// ── terminal emit tasks ──────────────────────────────────────────────────────

fn spawn_terminal_output_emit(io: SocketIo, terminal: Arc<TerminalService>) {
    let mut rx = terminal.subscribe_output();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Some(_ns) = io.of("/ws") else { continue };
                    let payload = json!({ "terminalId": event.terminal_id, "data": event.data });
                    for sid in &event.socket_ids {
                        if let Some(ns) = io.of("/ws") { let _ = ns.within(sid.clone()).emit("terminal.output", &payload); }
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("terminal output emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_terminal_exit_emit(io: SocketIo, terminal: Arc<TerminalService>) {
    let mut rx = terminal.subscribe_exit();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Some(_ns) = io.of("/ws") else { continue };
                    let payload = json!({ "terminal": event.terminal, "closed": false });
                    for sid in &event.socket_ids {
                        if let Some(ns) = io.of("/ws") { let _ = ns.within(sid.clone()).emit("terminal.exit", &payload); }
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("terminal exit emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_terminal_closed_emit(io: SocketIo, terminal: Arc<TerminalService>) {
    let mut rx = terminal.subscribe_closed();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Some(_ns) = io.of("/ws") else { continue };
                    let payload = json!({
                        "terminalId": event.terminal_id,
                        "contextKey": event.context_key,
                        "closed": true,
                    });
                    for sid in &event.socket_ids {
                        if let Some(ns) = io.of("/ws") { let _ = ns.within(sid.clone()).emit("terminal.exit", &payload); }
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("terminal closed emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

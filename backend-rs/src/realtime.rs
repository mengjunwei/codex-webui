//! Realtime WebSocket 网关 — Socket.IO 命名空间 `/ws`。
//!
//! 与 `src/threads/threads.gateway.ts` 及(占位)files 网关对齐。
//!
//! - 连接时:从 auth 负载 `{token}` 中校验 JWT/API key(对应 ApiKeyGuard 的 ws 分支);无效则拒绝。
//! - `thread.subscribe` / `thread.unsubscribe` → 加入/离开房间 `thread:<id>`。
//! - `fs.subscribe` / `fs.unsubscribe` → 回应 `{ok:true}`(空操作对齐;chokidar 已移除)。
//! - emit 任务将 codex 通知(`codex.notification`)、server 请求
//!   (`codex.serverRequest`)和生命周期事件(`codex.lifecycle`)转发到对应房间。

use crate::auth::AuthService;
use crate::codex::{CodexProcessManager, LifecycleEvent};
use crate::db::Db;
use crate::terminal::{TerminalMetadataEvent, TerminalService};
use serde_json::{json, Value};
use socketioxide::extract::{AckSender, Data as SocketData, SocketRef, State};
use socketioxide::SocketIo;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast::error::RecvError;

/// 注入到 socketioxide 处理器中的共享 realtime 状态。
#[derive(Clone)]
pub struct RealtimeState {
    pub auth: Arc<AuthService>,
    pub codex: Arc<CodexProcessManager>,
    pub terminal: Arc<TerminalService>,
    /// 用于终端 cwd 沙箱校验（工作区根目录）。
    pub db: Arc<Db>,
    pub dynamic_files_roots: Arc<Mutex<HashSet<String>>>,
}

/// 构建 Socket.IO 层与句柄,挂接 `/ws` 命名空间。
/// 返回的 `SocketIo` 用于派生 emit 转发任务。
pub fn build(rt_state: RealtimeState) -> (socketioxide::layer::SocketIoLayer, SocketIo) {
    let (layer, io) = SocketIo::builder()
        .with_state(rt_state)
        .build_layer();
    io.ns("/ws", on_connect);
    (layer, io)
}

/// 单连接处理器:鉴权并注册消息处理器。
/// `Data<Value>` 提取连接 auth 负载(客户端发送 `{token}`)。
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

    // 关键修复:socketioxide 0.15 不会自动把 socket 加入到以自身 SID 命名的房间
    // (与 JS socket.io 不同)。否则针对单 socket 的 emit(终端输出/退出/关闭)
    // 会指向一个空房间并被静默丢弃。
    let _ = s.join(s.id.to_string());

    s.on("thread.subscribe", on_thread_subscribe);
    s.on("thread.unsubscribe", on_thread_unsubscribe);
    s.on("fs.subscribe", on_ack);
    s.on("fs.unsubscribe", on_ack);
    s.on("codex.serverResponse", on_server_response);
    // ── terminal 事件 ──
    s.on("terminal.config", on_term_config);
    s.on("terminal.list", on_term_list);
    s.on("terminal.open", on_term_open);
    s.on("terminal.reconnect", on_term_reconnect);
    s.on("terminal.input", on_term_input);
    s.on("terminal.resize", on_term_resize);
    s.on("terminal.rename", on_term_rename);
    s.on("terminal.detach", on_term_detach);
    s.on("terminal.download", on_term_download);
    s.on("terminal.close", on_term_close);
    // 断开连接时从所有终端分离。
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

/// files 网关占位:以 `{ok:true}` 回应(TS 中已移除 chokidar 监视器)。
fn on_ack(_: SocketRef, ack: AckSender) {
    let _ = ack.send(&json!({ "ok": true }));
}

/// 旧版 WS 审批响应路径。权威路径为 REST 端点
/// (CAS + 转发);此处保留用于向后兼容(详细接线暂缓)。
fn on_server_response(s: SocketRef, SocketData(data): SocketData<Value>) {
    tracing::info!(
        socket = %s.id,
        id = ?data.get("id"),
        "codex.serverResponse via WS (REST respond endpoint preferred)"
    );
}

// ── Terminal 处理器 ────────────────────────────────────────────────────────

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
    let cwd_in = data.get("cwd").and_then(Value::as_str);
    let cols = data.get("cols").and_then(Value::as_u64).map(|n| n as u16);
    let rows = data.get("rows").and_then(Value::as_u64).map(|n| n as u16);
    let title = data.get("title").and_then(Value::as_str);
    // 终端 cwd 沙箱化：按 TS resolveTerminalCwd 解析候选并强制限定在工作区根目录内。
    let default_cwd = state.terminal.default_cwd();
    let dyn_roots: HashSet<String> = state
        .dynamic_files_roots
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    let cwd = match crate::files::resolve_terminal_cwd(
        &state.db,
        &dyn_roots,
        &ctx,
        cwd_in,
        default_cwd.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = ack.send(&json!({ "ok": false, "error": e.to_string() }));
            return;
        }
    };
    match state.terminal.open(s.id.as_str(), &ctx, Some(&cwd), cols, rows, title) {
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

fn on_term_rename(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    let title = data.get("title").and_then(Value::as_str).unwrap_or("");
    match state.terminal.rename(s.id.as_str(), &ctx, &tid, title) {
        Ok(meta) => { let _ = ack.send(&json!({ "ok": true, "terminal": meta })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_download(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    match state.terminal.download(s.id.as_str(), &ctx, &tid) {
        Ok((filename, content)) => { let _ = ack.send(&json!({ "ok": true, "data": { "filename": filename, "content": content } })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn on_term_close(s: SocketRef, State(state): State<RealtimeState>, SocketData(data): SocketData<Value>, ack: AckSender) {
    let ctx = data.get("contextKey").and_then(Value::as_str).unwrap_or("").to_string();
    let tid = data.get("terminalId").and_then(Value::as_str).unwrap_or("").to_string();
    match state.terminal.close(s.id.as_str(), &ctx, &tid) {
        Ok(()) => { let _ = ack.send(&json!({ "ok": true })); }
        Err(e) => { let _ = ack.send(&json!({ "ok": false, "error": e.to_string() })); }
    }
}

fn strip_bearer(s: &str) -> &str {
    s.strip_prefix("Bearer ").unwrap_or(s).trim()
}

// ── emit 转发任务 ────────────────────────────────────────────────────

/// 派生任务,将 codex + terminal 事件转发给 Socket.IO 客户端。
/// M1 修复:pending-record 与 WS emit 合并,以防止 TOCTOU(DB 记录
/// 必须在 WS 投递之前完成,以便 respond 端点能找到该行)。
pub fn spawn_emit_tasks(io: SocketIo, codex: Arc<CodexProcessManager>, terminal: Arc<TerminalService>, db: Arc<crate::db::Db>) {
    spawn_notification_emit(io.clone(), codex.clone());
    spawn_server_request_record_and_emit(io.clone(), codex.clone(), db);
    spawn_lifecycle_emit(io.clone(), codex);
    spawn_terminal_output_emit(io.clone(), terminal.clone());
    spawn_terminal_exit_emit(io.clone(), terminal.clone());
    spawn_terminal_closed_emit(io.clone(), terminal.clone());
    spawn_terminal_metadata_emit(io, terminal);
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

/// M1 修复:记录与 emit 合并 — DB 记录在 WS 投递之前完成。
fn spawn_server_request_record_and_emit(io: SocketIo, codex: Arc<CodexProcessManager>, db: Arc<crate::db::Db>) {
    let mut rx = codex.subscribe_server_requests();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(req) => {
                    // 阶段 1:记录到 DB(必须在 WS emit 之前完成)。
                    // MEDIUM-1 修复:若 DB 记录失败,则完全跳过 emit
                    // (防止出现无法响应的幽灵请求)。
                    if let Err(e) = crate::event_subscribers::record_server_request(&db, &codex, &req) {
                        tracing::error!("record server request failed, skipping emit: {e}");
                        continue;
                    }
                    // 阶段 2:emit 到 WS。
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
                Err(RecvError::Lagged(n)) => tracing::warn!("server-request record+emit lagged {n}"),
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

// ── terminal emit 任务 ──────────────────────────────────────────────────────

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

// M2: 终端元数据广播（resize/open 时通知所有已附着客户端）。
fn spawn_terminal_metadata_emit(io: SocketIo, terminal: Arc<TerminalService>) {
    let mut rx = terminal.subscribe_metadata();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let TerminalMetadataEvent { terminal: meta, socket_ids } = event;
                    let payload = json!({ "terminal": meta });
                    for sid in &socket_ids {
                        if let Some(ns) = io.of("/ws") { let _ = ns.within(sid.clone()).emit("terminal.metadata", &payload); }
                    }
                }
                Err(RecvError::Lagged(n)) => tracing::warn!("terminal metadata emit lagged {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

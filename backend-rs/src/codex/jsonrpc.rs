//! JSON-RPC client for communicating with `codex app-server` over stdio.
//!
//! Parity with `src/codex/codex-jsonrpc-client.ts`. Handles:
//! - request/response correlation (id → oneshot), with per-request timeout
//! - server-initiated requests (has id + method, no result/error)
//! - notifications (has method, no id)
//! - bidirectional JSONL logging to `logs/codex-jsonrpc.jsonl` as `{ts, dir, msg}`
//!
//! **Wire format note**: the Codex protocol OMITS the `jsonrpc: "2.0"` field.
//! Messages are `{method, id, params}` (request), `{method, params}` (notification),
//! `{id, result}` / `{id, error}` (response). Do NOT use a generic JSON-RPC crate
//! that injects `jsonrpc`.

use crate::codex::types::RequestId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Errors that can arise from a JSON-RPC request.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("client is closed")]
    Closed,
    #[error("request {method} (id={id}) timed out")]
    Timeout { method: String, id: RequestId },
    #[error("rpc error {code}: {message}")]
    ServerError { code: i64, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("process exited (code={code:?}, signal={signal:?})")]
    ProcessExited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

type PendingMap = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct CodexJsonRpcClient {
    next_id: AtomicU64, // initialized to 1; fetch_add returns 1, 2, 3, …
    pending: PendingMap,
    write_tx: mpsc::UnboundedSender<String>,
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    close_tx: broadcast::Sender<CloseReason>,
    closed: Arc<AtomicBool>,
    request_timeout: Duration,
    _reader_task: JoinHandle<()>,
    _writer_task: JoinHandle<()>,
    /// Held to keep the child process alive (kill_on_drop would kill it on
    /// drop otherwise). Killed explicitly in `destroy`.
    child: Mutex<Option<Child>>,
}

/// Why the client closed (for the close broadcast).
#[derive(Debug, Clone)]
pub enum CloseReason {
    StdoutEof,
    Destroy,
}

impl CodexJsonRpcClient {
    /// Construct the client around an already-spawned `codex app-server` child.
    /// The child must have piped stdin/stdout. Spawns reader + writer tasks.
    pub fn new(
        mut child: Child,
        request_timeout_ms: Option<u64>,
    ) -> Result<Self, std::io::Error> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "stdout not piped"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (write_tx, write_rx) = mpsc::unbounded_channel::<String>();
        let (notify_tx, _) = broadcast::channel::<Value>(256);
        let (server_request_tx, _) = broadcast::channel::<Value>(256);
        let (close_tx, _) = broadcast::channel::<CloseReason>(8);
        let closed = Arc::new(AtomicBool::new(false));

        // JSONL log channel (best-effort bidirectional logging).
        let (jsonl_tx, jsonl_rx) = mpsc::unbounded_channel::<String>();

        let request_timeout = Duration::from_millis(
            request_timeout_ms.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        );

        // Reader task: parse stdout lines, dispatch responses/notifications/server-requests.
        let reader_pending = pending.clone();
        let reader_notify = notify_tx.clone();
        let reader_server_req = server_request_tx.clone();
        let reader_close = close_tx.clone();
        let reader_closed = closed.clone();
        let reader_jsonl = jsonl_tx.clone();
        let reader_task = tokio::spawn(async move {
            read_loop(
                stdout,
                reader_pending,
                reader_notify,
                reader_server_req,
                reader_close,
                reader_closed,
                reader_jsonl,
            )
            .await;
        });

        // Writer task: drain write_tx → stdin (logs each outbound line to jsonl).
        let writer_jsonl = jsonl_tx.clone();
        let writer_task = tokio::spawn(async move {
            write_loop(stdin, write_rx, writer_jsonl).await;
        });

        // JSONL appender task (detaches; ends when all jsonl senders drop).
        tokio::spawn(jsonl_loop(jsonl_rx));

        Ok(Self {
            next_id: AtomicU64::new(1),
            pending,
            write_tx,
            notify_tx,
            server_request_tx,
            close_tx,
            closed,
            request_timeout,
            _reader_task: reader_task,
            _writer_task: writer_task,
            child: Mutex::new(Some(child)),
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send a JSON-RPC request and await the correlated response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String(method.into()));
        msg.insert("id".into(), Value::Number(id.into()));
        if let Some(p) = params {
            msg.insert("params".into(), p);
        }
        let line = serde_json::to_string(&Value::Object(msg))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if self.write_tx.send(line).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(RpcError::Closed);
        }

        match timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Closed), // sender dropped (destroy/close)
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout {
                    method: method.into(),
                    id,
                })
            }
        }
    }

    /// Fire-and-forget notification.
    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String(method.into()));
        if let Some(p) = params {
            msg.insert("params".into(), p);
        }
        let line = serde_json::to_string(&Value::Object(msg))?;
        self.write_tx.send(line).map_err(|_| RpcError::Closed)
    }

    /// Respond to a server-initiated request (e.g. approval decision).
    /// `id` is forwarded verbatim to preserve number-vs-string type (codex
    /// correlates responses by id value AND type).
    pub fn respond_to_server_request(
        &self,
        id: Value,
        result: Value,
    ) -> Result<(), RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let mut msg = serde_json::Map::new();
        msg.insert("id".into(), id);
        msg.insert("result".into(), result);
        let line = serde_json::to_string(&Value::Object(msg))?;
        self.write_tx.send(line).map_err(|_| RpcError::Closed)
    }

    /// Subscribe to server notifications (method + params, no id).
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.notify_tx.subscribe()
    }

    /// Subscribe to server-initiated requests (id + method + params).
    pub fn subscribe_server_requests(&self) -> broadcast::Receiver<Value> {
        self.server_request_tx.subscribe()
    }

    /// Subscribe to close events.
    pub fn subscribe_close(&self) -> broadcast::Receiver<CloseReason> {
        self.close_tx.subscribe()
    }

    /// Mark closed, reject all pending, and kill the child process.
    pub async fn destroy(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().await.clear(); // drops senders → requests get Closed
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
        let _ = self.close_tx.send(CloseReason::Destroy);
    }
}

// ── Reader / writer / jsonl tasks ────────────────────────────────────────────

async fn read_loop(
    stdout: ChildStdout,
    pending: PendingMap,
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    close_tx: broadcast::Sender<CloseReason>,
    closed: Arc<AtomicBool>,
    jsonl_tx: mpsc::UnboundedSender<String>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                jsonl_log(&jsonl_tx, "in", &line);
                dispatch_line(&line, &pending, &notify_tx, &server_request_tx).await;
            }
            Ok(None) => break, // EOF
            Err(e) => {
                tracing::warn!("codex stdout read error: {}", e);
                break;
            }
        }
    }
    // stdout closed: reject all pending + signal close.
    closed.store(true, Ordering::SeqCst);
    pending.lock().await.clear();
    let _ = close_tx.send(CloseReason::StdoutEof);
}

async fn write_loop(
    mut stdin: ChildStdin,
    mut write_rx: mpsc::UnboundedReceiver<String>,
    jsonl_tx: mpsc::UnboundedSender<String>,
) {
    while let Some(line) = write_rx.recv().await {
        jsonl_log(&jsonl_tx, "out", &line);
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.write_all(b"\n").await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn jsonl_loop(mut jsonl_rx: mpsc::UnboundedReceiver<String>) {
    let path = std::path::Path::new("logs").join("codex-jsonrpc.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    while let Some(line) = jsonl_rx.recv().await {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
        }
    }
}

fn jsonl_log(jsonl_tx: &mpsc::UnboundedSender<String>, dir: &str, raw_line: &str) {
    // Re-parse to embed under {ts, dir, msg}; fall back to raw string.
    let msg: Value = serde_json::from_str(raw_line).unwrap_or(Value::String(raw_line.into()));
    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "dir": dir,
        "msg": msg,
    });
    let _ = jsonl_tx.send(entry.to_string());
}

/// Parse one inbound line and route it. Extracted for unit testing.
async fn dispatch_line(
    line: &str,
    pending: &PendingMap,
    notify_tx: &broadcast::Sender<Value>,
    server_request_tx: &broadcast::Sender<Value>,
) {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!("failed to parse JSON-RPC message: {}", &line[..line.len().min(200)]);
            return;
        }
    };

    let has_id = msg.get("id").is_some();
    let has_result = msg.get("result").is_some();
    let has_error = msg.get("error").is_some();
    let has_method = msg.get("method").is_some();

    if has_id && (has_result || has_error) {
        // Response to a client request.
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(tx) = pending.lock().await.remove(&id) {
                let result = if let Some(err) = msg.get("error") {
                    Err(RpcError::ServerError {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            }
        }
    } else if has_id && has_method {
        // Server-initiated request (e.g. approval).
        let _ = server_request_tx.send(msg);
    } else if has_method {
        // Notification.
        let _ = notify_tx.send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_pending() -> PendingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn dispatch_response_resolves_pending() {
        let pending = empty_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(42, tx);
        let (notify_tx, _) = broadcast::channel(16);
        let (server_req_tx, _) = broadcast::channel(16);

        dispatch_line(
            r#"{"id":42,"result":{"codexHome":"/home/.codex"}}"#,
            &pending,
            &notify_tx,
            &server_req_tx,
        )
        .await;

        let result = rx.await.unwrap().unwrap();
        assert_eq!(result["codexHome"], "/home/.codex");
        assert!(pending.lock().await.is_empty(), "pending entry removed");
    }

    #[tokio::test]
    async fn dispatch_error_response_rejects_pending() {
        let pending = empty_pending();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);
        let (notify_tx, _) = broadcast::channel(16);
        let (server_req_tx, _) = broadcast::channel(16);

        dispatch_line(
            r#"{"id":7,"error":{"code":-32000,"message":"nope"}}"#,
            &pending,
            &notify_tx,
            &server_req_tx,
        )
        .await;

        let err = rx.await.unwrap().unwrap_err();
        match err {
            RpcError::ServerError { code, message } => {
                assert_eq!(code, -32000);
                assert_eq!(message, "nope");
            }
            other => panic!("expected ServerError, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn dispatch_notification_emits() {
        let pending = empty_pending();
        let (notify_tx, _) = broadcast::channel(16);
        let mut rx = notify_tx.subscribe();
        let (server_req_tx, _) = broadcast::channel(16);

        dispatch_line(
            r#"{"method":"thread/updated","params":{"threadId":"t1"}}"#,
            &pending,
            &notify_tx,
            &server_req_tx,
        )
        .await;

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg["method"], "thread/updated");
    }

    #[tokio::test]
    async fn dispatch_server_request_emits() {
        let pending = empty_pending();
        let (notify_tx, _) = broadcast::channel(16);
        let (server_req_tx, _) = broadcast::channel(16);
        let mut rx = server_req_tx.subscribe();

        dispatch_line(
            r#"{"id":99,"method":"approval/request","params":{"threadId":"t1"}}"#,
            &pending,
            &notify_tx,
            &server_req_tx,
        )
        .await;

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg["method"], "approval/request");
        assert_eq!(msg["id"], 99);
    }

    #[tokio::test]
    async fn dispatch_invalid_json_is_ignored() {
        let pending = empty_pending();
        let (notify_tx, _) = broadcast::channel(16);
        let (server_req_tx, _) = broadcast::channel(16);

        // Should not panic.
        dispatch_line("not json at all", &pending, &notify_tx, &server_req_tx).await;
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn request_message_omits_jsonrpc_field() {
        // Verify serialization shape via the same map construction logic.
        let id: u64 = 1;
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String("initialize".into()));
        msg.insert("id".into(), Value::Number(id.into()));
        msg.insert("params".into(), serde_json::json!({"clientInfo": {}}));
        let serialized = serde_json::to_string(&Value::Object(msg)).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["id"], 1);
        assert!(parsed.get("jsonrpc").is_none(), "must NOT include jsonrpc field");
    }
}

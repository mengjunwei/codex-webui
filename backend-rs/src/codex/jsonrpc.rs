//! 通过 stdio 与 `codex app-server` 通信的 JSON-RPC 客户端。
//!
//! 与 `src/codex/codex-jsonrpc-client.ts` 保持对齐。负责：
//! - 请求/响应关联（id → oneshot），每个请求独立超时
//! - 服务端主动发起的请求（有 id + method，无 result/error）
//! - 通知（有 method，无 id）
//! - 双向 JSONL 日志写入 `logs/codex-jsonrpc.jsonl`，格式为 `{ts, dir, msg}`
//!
//! **传输格式说明**：Codex 协议省略了 `jsonrpc: "2.0"` 字段。
//! 消息格式为 `{method, id, params}`（请求）、`{method, params}`（通知）、
//! `{id, result}` / `{id, error}`（响应）。不要使用会自动注入
//! `jsonrpc` 字段的通用 JSON-RPC 库。

use crate::codex::types::RequestId;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// 出站写队列容量(有界背压):codex 处理或 stdin 写跟不上时队列满 → WriteQueueFull,
/// 调用方应退避/限流,而非在内存无限堆积(M5-A:根治 unbounded channel 的 OOM 风险)。
const WRITE_QUEUE_CAP: usize = 1024;

/// JSON-RPC 请求可能产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("client is closed")]
    Closed,
    /// 写队列已满(有界背压)。
    #[error("write queue full (backpressure)")]
    WriteQueueFull,
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

/// 有界写队列 try_send 错误 → RpcError(Full → WriteQueueFull,Closed → Closed)。
fn map_try_send_err(e: mpsc::error::TrySendError<String>) -> RpcError {
    match e {
        mpsc::error::TrySendError::Full(_) => RpcError::WriteQueueFull,
        mpsc::error::TrySendError::Closed(_) => RpcError::Closed,
    }
}

type PendingMap = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct CodexJsonRpcClient {
    next_id: AtomicU64, // 初始化为 1；fetch_add 依次返回 1、2、3……
    pending: PendingMap,
    write_tx: mpsc::Sender<String>,
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    close_tx: broadcast::Sender<CloseReason>,
    closed: Arc<AtomicBool>,
    request_timeout: Duration,
    _reader_task: JoinHandle<()>,
    _writer_task: JoinHandle<()>,
    /// 持有以保持子进程存活（否则 kill_on_drop 会在 drop 时将其杀死）。
    /// 在 `destroy` 中显式杀死。
    child: Mutex<Option<Child>>,
}

/// 客户端关闭的原因（用于关闭事件的广播）。
#[derive(Debug, Clone)]
pub enum CloseReason {
    StdoutEof,
    Destroy,
    /// stdin 写入失败(子进程不读/管道断):client 不可用,需标记 closed 促清理。
    WriteFailed,
}

impl CodexJsonRpcClient {
    /// 基于已启动的 `codex app-server` 子进程构造客户端。
    /// 子进程必须使用管道化的 stdin/stdout。会启动读取 + 写入任务。
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
        let (write_tx, write_rx) = mpsc::channel::<String>(WRITE_QUEUE_CAP);
        let (notify_tx, _) = broadcast::channel::<Value>(256);
        let (server_request_tx, _) = broadcast::channel::<Value>(1024);
        let (close_tx, _) = broadcast::channel::<CloseReason>(8);
        let closed = Arc::new(AtomicBool::new(false));

        // JSONL 日志通道（尽力而为的双向日志记录）。
        // T8：有界 channel + try_send 背压 —— 慢盘下消费速度跟不上时丢弃日志（best-effort），
        //        避免 unbounded 通道无界积压致 OOM。
        let (jsonl_tx, jsonl_rx) = mpsc::channel::<String>(4096);

        let request_timeout = Duration::from_millis(
            request_timeout_ms.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        );

        // 读取任务：解析 stdout 行，分发响应/通知/服务端请求。
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

        // 写入任务：从 write_tx 取出数据写入 stdin（每条出站行都记录到 jsonl）。
        let writer_jsonl = jsonl_tx.clone();
        let writer_closed = closed.clone();
        let writer_close = close_tx.clone();
        let writer_task = tokio::spawn(async move {
            write_loop(stdin, write_rx, writer_jsonl, writer_closed, writer_close).await;
        });

        // JSONL 追加任务（spawn_blocking：同步文件 IO 不占 tokio worker；
        // 所有 jsonl 发送端 drop 后 blocking_recv 返回 None 结束）。
        tokio::task::spawn_blocking(move || jsonl_loop_blocking(jsonl_rx));

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

    pub fn is_closed(&self) -> bool {
        // Acquire:确保看到最新的 closed 状态和之前的修改。
        self.closed.load(Ordering::Acquire)
    }

    /// 发送 JSON-RPC 请求并等待关联的响应。
    ///
    /// ## 时序与并发保证
    ///
    /// 1. **id 分配**：`fetch_add` 原子递增，从 1 开始（与 Codex 协议预期一致）。
    /// 2. **pending 插入**：oneshot 发送端先放进 `pending`，再投递到写队列。
    ///    顺序必须如此 —— 否则读端可能在 pending 插入前收到响应，导致响应"丢失"。
    /// 3. **写队列**：`mpsc::unbounded_channel` 无容量限制（写端是单线程串行化）。
    ///    写队列 send 失败意味着 writer task 已退出 → 立刻回退并清理 pending。
    /// 4. **超时**：`tokio::time::timeout` 包裹 `oneshot::Receiver`。
    ///    超时发生时主动从 pending 移除，避免后续到达的响应误派发。
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        // Relaxed:id 只用于关联请求和响应,不需要严格的顺序。
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String(method.into()));
        msg.insert("id".into(), Value::Number(id.into()));
        if let Some(p) = params {
            msg.insert("params".into(), p);
        }
        let line = serde_json::to_string(&Value::Object(msg))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(e) = self.write_tx.try_send(line) {
            self.pending.lock().await.remove(&id);
            return Err(map_try_send_err(e));
        }

        match timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Closed), // 发送端被 drop（destroy/关闭）
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout {
                    method: method.into(),
                    id,
                })
            }
        }
    }

    /// 发后即忘（fire-and-forget）的通知。
    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        // closed 时静默丢弃（对齐 TS：fire-and-forget，不因通道关闭而报错）。
        if self.is_closed() {
            return Ok(());
        }
        let mut msg = serde_json::Map::new();
        msg.insert("method".into(), Value::String(method.into()));
        if let Some(p) = params {
            msg.insert("params".into(), p);
        }
        let line = serde_json::to_string(&Value::Object(msg))?;
        self.write_tx.try_send(line).map_err(map_try_send_err)
    }

    /// 响应服务端主动发起的请求（例如审批决定）。
    /// `id` 原样转发以保持数字/字符串类型（codex 通过 id 的值和类型
    /// 共同关联响应）。
    ///
    /// closed 时返回 Err(Closed) 而非静默 Ok:审批响应是"需确认到达"的语义
    /// (不是 fire-and-forget notify),静默成功会让调用方误判已处理 →
    /// DB 标记 resolved 但 codex 从未收到 → 审批卡死且无法自愈。
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
        self.write_tx.try_send(line).map_err(map_try_send_err)
    }

    /// 用错误码响应服务端请求（审批拒绝等场景）。
    /// 对齐 TS `respondToServerRequestWithError(id, code, message)`。
    /// 与 respond_to_server_request 一致:closed 时返回 Err(避免静默成功导致状态不一致)。
    pub fn respond_to_server_request_with_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
    ) -> Result<(), RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let mut msg = serde_json::Map::new();
        msg.insert("id".into(), id);
        msg.insert(
            "error".into(),
            Value::Object({
                let mut m = serde_json::Map::new();
                m.insert("code".into(), Value::Number(code.into()));
                m.insert("message".into(), Value::String(message.to_string()));
                m
            }),
        );
        let line = serde_json::to_string(&Value::Object(msg))?;
        self.write_tx.try_send(line).map_err(map_try_send_err)
    }

    /// 订阅服务端通知（method + params，无 id）。
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.notify_tx.subscribe()
    }

    /// 订阅服务端主动发起的请求（id + method + params）。
    pub fn subscribe_server_requests(&self) -> broadcast::Receiver<Value> {
        self.server_request_tx.subscribe()
    }

    /// 订阅关闭事件。
    pub fn subscribe_close(&self) -> broadcast::Receiver<CloseReason> {
        self.close_tx.subscribe()
    }

    /// 标记为关闭，拒绝所有待处理请求，并杀死子进程。
    pub async fn destroy(&self) {
        // Release:确保其他线程看到 closed=true 和之前的修改。
        self.closed.store(true, Ordering::Release);
        self.pending.lock().await.clear(); // drop 发送端 → 请求收到 Closed
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
        let _ = self.close_tx.send(CloseReason::Destroy);
    }
}

// ── 读取 / 写入 / jsonl 任务 ────────────────────────────────────────────

async fn read_loop(
    stdout: ChildStdout,
    pending: PendingMap,
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    close_tx: broadcast::Sender<CloseReason>,
    closed: Arc<AtomicBool>,
    jsonl_tx: mpsc::Sender<String>,
) {
    // BugC 修复:BufReader::lines().next_line() 无最大行长度,单行超大响应(恶意 marketplace
    // /codex 异常)会整行读入 String → OOM。改为手动 chunk 读 + 累积上限,超限丢弃该行。
    const MAX_LINE_BYTES: usize = 64 * 1024 * 1024; // 64MB:正常 JSON-RPC 响应远低于此。
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("codex stdout read error: {}", e);
                break;
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        // 累积超限(一行未遇 \n 却已超 64MB)→ 丢弃,防 OOM。
        if buf.len() > MAX_LINE_BYTES {
            tracing::warn!(
                size = buf.len(),
                "codex stdout line exceeds {} bytes, discarding to avoid OOM",
                MAX_LINE_BYTES
            );
            buf.clear();
            continue;
        }
        // 处理所有完整行(以 \n 分隔)。
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            jsonl_log(&jsonl_tx, "in", &line);
            dispatch_line(&line, &pending, &notify_tx, &server_request_tx).await;
        }
    }
    // stdout 已关闭：拒绝所有待处理请求 + 发送关闭信号。
    // Release:确保其他线程看到 closed=true 和之前的修改。
    closed.store(true, Ordering::Release);
    pending.lock().await.clear();
    let _ = close_tx.send(CloseReason::StdoutEof);
}

async fn write_loop(
    mut stdin: ChildStdin,
    mut write_rx: mpsc::Receiver<String>,
    jsonl_tx: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
    close_tx: broadcast::Sender<CloseReason>,
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
    // write 失败(stdin 关闭/子进程不读):标记 closed + 发 close 信号。
    // 否则 is_closed() 仍 false,pick_slot 会继续选这个已死的 client(codex_pool),
    // 所有 request 因 write_rx dropped 返回 Closed,但 slot 不被清理 → team 请求持续失败。
    closed.store(true, Ordering::Release);
    let _ = close_tx.send(CloseReason::WriteFailed);
}

/// JSONL 追加循环（同步，运行在 spawn_blocking 线程上）。
/// 用 `blocking_recv` + 持久文件句柄，避免在 tokio worker 上做同步 IO
/// 以及每条消息 open/close 的开销（原实现在 async 任务里每条都重新打开文件）。
fn jsonl_loop_blocking(mut jsonl_rx: mpsc::Receiver<String>) {
    use std::io::Write;
    let path = crate::logging::log_dir().join("codex-jsonrpc.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    while let Some(line) = jsonl_rx.blocking_recv() {
        if let Some(f) = file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
        }
    }
}

fn jsonl_log(jsonl_tx: &mpsc::Sender<String>, dir: &str, raw_line: &str) {
    // 重新解析以嵌入到 {ts, dir, msg} 下；失败时退回原始字符串。
    let msg: Value = serde_json::from_str(raw_line).unwrap_or(Value::String(raw_line.into()));
    // BugB 修复:出站消息可能含凭据(account/login 的 apiKey/accessToken、设备码 userCode 等),
    // 明文落盘 logs/codex-jsonrpc.jsonl 会泄露给任何能读该文件的人(运维/备份/日志采集)。
    // 脱敏后再写盘。
    let msg = redact_secrets_for_log(&msg);
    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "dir": dir,
        "msg": msg,
    });
    let _ = jsonl_tx.try_send(entry.to_string()); // 满（慢盘）则丢弃
}

/// 递归脱敏 JSON-RPC 日志中的敏感字段(key 名匹配 token/password/api[_-]?key/secret/
/// authorization/accessToken/apiKey/userCode/verificationUrl 时,value 替换为 [redacted])。
/// 防凭据明文落盘 jsonl 日志。
fn redact_secrets_for_log(value: &Value) -> Value {
    match value {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, v) in m {
                if is_sensitive_key(k) && !v.is_null() {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else {
                    out.insert(k.clone(), redact_secrets_for_log(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(redact_secrets_for_log).collect()),
        other => other.clone(),
    }
}

/// 判断 key 名是否敏感(小写匹配)。
fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("token")
        || k.contains("password")
        || k.contains("apikey")
        || k.contains("api_key")
        || k.contains("secret")
        || k.contains("authorization")
        || k == "accesstoken"
        || k == "usercode"
        || k == "verificationurl"
}

/// 解析单条入站行并路由。为单元测试而抽取出来。
///
/// ## 判别优先级（重要）
///
/// 1. `id + result/error` → 响应：必须用 `as_u64` 取出数字 id（hash 查找）。
/// 2. `id + method` → 服务端请求：保留原始 id（含类型）原样转发。
/// 3. 仅 `method` → 通知。
/// 4. JSON 解析失败 → 截断 200 字符（按 chars 而非 bytes 截断，避免多字节 UTF-8
///    落点 panic）后 warn，继续读下一行。
/// 5. 都无法分类的奇怪结构 → 静默忽略。
///
/// ## 注意
///
/// `has_error` 使用 `is_null()` 判定 —— codex 协议中 `error: null` 等价于缺省，
/// 视为"非错误响应"。同样，`has_id` 仅判定字段是否存在而非非空（响应中 id 必为数值）。
async fn dispatch_line(
    line: &str,
    pending: &PendingMap,
    notify_tx: &broadcast::Sender<Value>,
    server_request_tx: &broadcast::Sender<Value>,
) {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            // T5：按字符边界截断，避免多字节 UTF-8（如中文）落在字节边界 panic 致 read_loop 终止。
            let preview: String = line.chars().take(200).collect();
            tracing::warn!("failed to parse JSON-RPC message: {}", preview);
            return;
        }
    };

    let has_id = msg.get("id").is_some();
    let has_result = msg.get("result").is_some();
    let has_error = msg.get("error").map_or(false, |v| !v.is_null());
    let has_method = msg.get("method").is_some();

    if has_id && (has_result || has_error) {
        // 对客户端请求的响应。
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
        // 服务端主动发起的请求（例如审批）。
        let _ = server_request_tx.send(msg);
    } else if has_method {
        // 通知。
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

        // 不应 panic。
        dispatch_line("not json at all", &pending, &notify_tx, &server_req_tx).await;
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn request_message_omits_jsonrpc_field() {
        // 通过相同的 map 构造逻辑验证序列化结构。
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

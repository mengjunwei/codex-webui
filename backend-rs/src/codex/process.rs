//! Codex app-server 进程生命周期管理器。
//!
//! 与 `src/codex/codex-process-manager.service.ts` 保持对齐。持有 JSON-RPC
//! 客户端，执行 initialize 握手，跨重启跟踪 generation，退出时自动重启
//! （3000ms 退避），并通过管理器级别的通道重新广播通知/服务端请求，
//! 这些通道**在重启后依然保持有效**（订阅者在重启后仍可继续收到事件）。

use crate::codex::jsonrpc::{CodexJsonRpcClient, RpcError};
use crate::codex::types::{default_initialize_params, InitializeResponse};
use serde_json::Value;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};
use tokio::time::sleep;

const RESTART_DELAY_MS: u64 = 3000;

#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    Restarting { generation: u64, delay_ms: u64 },
    Ready { generation: u64, restarted: bool },
    Unavailable { generation: u64, message: String },
}

pub struct CodexProcessManager {
    codex_bin: String,
    codex_home: Option<String>,
    /// `(generation, client)` —— generation 让关闭监视器避免
    /// 覆盖较新的子进程。
    current: Mutex<Option<(u64, Arc<CodexJsonRpcClient>)>>,
    generation: AtomicU64,
    restarting: AtomicBool,
    destroyed: AtomicBool,
    /// 连续重启失败次数（用于指数退避；start 成功时重置为 0）。
    consecutive_failures: AtomicU64,
    /// 管理器级别的通知/服务端请求通道 —— 跨重启保持有效。
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
    /// 最近一次 initialize 握手结果（原始 JSON），供 CodexStatusService
    /// 暴露为 `initialize.data`。握手失败或进程退出后为 None。
    init_result: Mutex<Option<Value>>,
}

impl CodexProcessManager {
    /// 构造但不立即启动（懒加载）。调用 `start()` 才会启动 + 初始化。
    pub fn new(codex_bin: String, codex_home: Option<String>) -> Self {
        let (notify_tx, _) = broadcast::channel::<Value>(256);
        let (server_request_tx, _) = broadcast::channel::<Value>(1024);
        let (lifecycle_tx, _) = broadcast::channel::<LifecycleEvent>(32);
        Self {
            codex_bin,
            codex_home,
            current: Mutex::new(None),
            generation: AtomicU64::new(0),
            restarting: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            notify_tx,
            server_request_tx,
            lifecycle_tx,
            init_result: Mutex::new(None),
        }
    }

    /// 订阅 app-server 通知（跨重启保持有效）。
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.notify_tx.subscribe()
    }

    /// 订阅服务端主动发起的请求（跨重启保持有效）。
    pub fn subscribe_server_requests(&self) -> broadcast::Receiver<Value> {
        self.server_request_tx.subscribe()
    }

    /// 订阅生命周期事件。
    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle_tx.subscribe()
    }

    /// 当前的 JSON-RPC 客户端，未连接则返回 `None`。
    pub async fn client(&self) -> Option<Arc<CodexJsonRpcClient>> {
        match self.current.lock().await.as_ref() {
            Some((_, c)) => Some(c.clone()),
            None => None,
        }
    }

    /// 当前 generation（首次成功初始化之前为 0）。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// 最近一次 initialize 握手结果（原始 JSON），未初始化时为 None。
    pub async fn init_result(&self) -> Option<Value> {
        self.init_result.lock().await.clone()
    }

    /// 向当前 app-server 发送 JSON-RPC 请求，不可用则返回错误。
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match self.client().await {
            Some(c) => c.request(method, params).await,
            None => Err(RpcError::Closed),
        }
    }

    /// 启动 + 初始化。任何失败都会调度一次重启。基于 `restarting`
    /// 标志的幂等保护由调用方 / `restart` 处理。
    pub async fn start(self: Arc<Self>) {
        if self.destroyed.load(Ordering::SeqCst) {
            return;
        }

        // 第一阶段：启动子进程 + 创建客户端（尚未初始化）。
        let client = match self.spawn_child().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to spawn codex app-server: {}", e);
                if !self.destroyed.load(Ordering::SeqCst) {
                    self.clone().restart().await;
                }
                return;
            }
        };

        // 第二阶段：在初始化之前挂载转发器 + 关闭监视器（M1 修复：
        // 关闭监视器竞态 —— 如果子进程在初始化期间退出，监视器必须
        // 已经订阅才能捕获到该事件）。
        let new_generation = self.generation.load(Ordering::SeqCst) + 1;
        self.attach_forwarders(&client);
        self.spawn_close_watcher(&client, new_generation);

        // 第三阶段：initialize 握手。
        match self.initialize_client(&client).await {
            Ok((init, init_value)) => {
                // 重启成功，重置指数退避计数。
                self.consecutive_failures.store(0, Ordering::SeqCst);
                self.generation.fetch_add(1, Ordering::SeqCst);
                let restarted = new_generation > 1;
                // M1 修复：复查 destroyed 与写入 current 在同一把锁内原子完成。
                // spawn 之后、写 current 之前若 destroy 被调用，它 take 到 None；
                // 此处锁内看到 destroyed=true 就地销毁新 client，避免孤儿子进程。
                {
                    let mut current = self.current.lock().await;
                    if self.destroyed.load(Ordering::SeqCst) {
                        drop(current);
                        client.destroy().await;
                        return;
                    }
                    *current = Some((new_generation, client));
                }
                *self.init_result.lock().await = Some(init_value);

                tracing::info!(
                    codex_home = ?init.codex_home,
                    platform = ?init.platform_os,
                    generation = new_generation,
                    "codex app-server initialized"
                );
                let _ = self.lifecycle_tx.send(LifecycleEvent::Ready {
                    generation: new_generation,
                    restarted,
                });
            }
            Err(e) => {
                tracing::error!("failed to initialize codex app-server: {}", e);
                // 清理半初始化的客户端。
                client.destroy().await;
                if !self.destroyed.load(Ordering::SeqCst) {
                    self.clone().restart().await;
                }
            }
        }
    }

    /// 启动子进程并创建 JSON-RPC 客户端（不执行握手）。
    async fn spawn_child(&self) -> Result<Arc<CodexJsonRpcClient>, RpcError> {
        tracing::info!("spawning {} app-server (stdio)", self.codex_bin);

        let mut cmd = build_codex_command(&self.codex_bin);
        cmd.args(["app-server", "--listen", "stdio://"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(home) = &self.codex_home {
            cmd.env("CODEX_HOME", home);
        }

        let mut child = cmd.spawn().map_err(RpcError::Io)?;

        // stderr 读取：将 app-server 的诊断输出作为 warning 记录。
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!("codex stderr: {}", line.trim());
                }
            });
        }

        let client = CodexJsonRpcClient::new(child, None)?;
        Ok(Arc::new(client))
    }

    /// 在已启动的客户端上执行 initialize 握手。
    /// 返回 `(结构化结果, 原始 JSON)` —— 原始 JSON 保留全部字段，供状态聚合暴露。
    async fn initialize_client(
        &self,
        client: &Arc<CodexJsonRpcClient>,
    ) -> Result<(InitializeResponse, Value), RpcError> {
        let params = serde_json::to_value(default_initialize_params())?;
        let init_value = client.request("initialize", Some(params)).await?;
        let init: InitializeResponse = serde_json::from_value(init_value.clone())?;
        client.notify("initialized", Some(Value::Object(Default::default())))?;
        Ok((init, init_value))
    }

    /// 将新客户端的事件重新广播到管理器级别的通道中。
    fn attach_forwarders(&self, client: &Arc<CodexJsonRpcClient>) {
        let mut notify_rx = client.subscribe_notifications();
        let mgr_notify = self.notify_tx.clone();
        tokio::spawn(async move {
            loop {
                match notify_rx.recv().await {
                    Ok(msg) => {
                        let _ = mgr_notify.send(msg);
                    }
                    // H1 修复：Lagged 表示消费方落后、旧消息被丢弃，但通道仍存活，必须 continue；
                    // 原 Err(_) => break 会把 Lagged 误当 Closed，突发通知下转发永久静默、前端断流。
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "notify forwarder lagged, skipping");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break, // 客户端已关闭
                }
            }
        });

        let mut server_req_rx = client.subscribe_server_requests();
        let mgr_server_req = self.server_request_tx.clone();
        tokio::spawn(async move {
            loop {
                match server_req_rx.recv().await {
                    Ok(msg) => {
                        let _ = mgr_server_req.send(msg);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "server-request forwarder lagged, skipping");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn spawn_close_watcher(self: &Arc<Self>, client: &Arc<CodexJsonRpcClient>, generation: u64) {
        let mut close_rx = client.subscribe_close();
        let self_clone = Arc::clone(self);
        tokio::spawn(async move {
            if close_rx.recv().await.is_ok() {
                self_clone.handle_close(generation).await;
            }
        });
    }

    async fn handle_close(self: Arc<Self>, generation: u64) {
        // 仅当仍是该 generation 对应的活跃客户端时才清除。
        let stale = {
            let mut current = self.current.lock().await;
            let stale = matches!(current.as_ref(), Some((g, _)) if *g == generation);
            if stale {
                *current = None;
            }
            stale
        };

        // 与 TS 版本对齐：仅当此次关闭针对的是当前活跃客户端时，
        // 才发出 Unavailable（不是陈旧/重复的关闭，也不是 destroy ——
        // 后者会在监视器唤醒前已将 `current` 置空）。
        if stale {
            *self.init_result.lock().await = None;
            let _ = self.lifecycle_tx.send(LifecycleEvent::Unavailable {
                generation,
                message: "codex app-server exited".into(),
            });
            if !self.destroyed.load(Ordering::SeqCst) {
                self.restart().await;
            }
        }
    }

    async fn restart(self: Arc<Self>) {
        if self.restarting.swap(true, Ordering::SeqCst) || self.destroyed.load(Ordering::SeqCst) {
            return;
        }
        // 指数退避：3s → 6s → 12s → 24s → 48s → 60s（上限），避免持续失败时日志爆炸 + 空转。
        let attempt = self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        let delay_ms = std::cmp::min(
            RESTART_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt.min(10) as u32)),
            60_000,
        );
        let generation = self.generation.load(Ordering::SeqCst);
        tracing::warn!(generation, delay_ms, attempt, "restarting codex app-server");
        let _ = self.lifecycle_tx.send(LifecycleEvent::Restarting {
            generation,
            delay_ms,
        });
        sleep(Duration::from_millis(delay_ms)).await;
        self.restarting.store(false, Ordering::SeqCst);
        if !self.destroyed.load(Ordering::SeqCst) {
            // 装箱以打破 start ↔ restart 的异步递归。
            Box::pin(self.start()).await;
        }
    }

    /// 停止管理器：销毁客户端并阻止重启。
    pub async fn destroy(&self) {
        self.destroyed.store(true, Ordering::SeqCst);
        // H8 修复：先 take 再释放锁，避免持有 current 锁跨 client.destroy().await
        // （destroy 含 kill 子进程等耗时操作，会长时间阻塞 request/handle_close 等热点路径）。
        let taken = self.current.lock().await.take();
        if let Some((_, client)) = taken {
            client.destroy().await;
        }
    }
}

/// 构造用于启动 codex 的 Command。
///
/// 在 Windows 上，npm 安装的 CLI 以 `.cmd`/`.bat` 垫片形式发布。通过
/// `cmd.exe /c` 启动这些垫片不会将 stdio 管道继承到内部的 node 孙进程
/// （stdout 会立即关闭）。因此，对于 npm 垫片，我们解析出底层的
/// `node + codex.js` 并直接启动 `node` —— node 直接继承管道，没有孙进程。
/// 非 npm 的 `.cmd`/`.bat` 退回到 `cmd.exe /c`。真正的 `.exe` 可执行文件
/// 以及非 Windows 平台直接启动。
#[cfg(windows)]
fn build_codex_command(bin: &str) -> Command {
    let lower = bin.to_ascii_lowercase();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        if let Some(cmd) = resolve_node_script(bin) {
            return cmd;
        }
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(bin);
        c
    } else {
        Command::new(bin)
    }
}

/// 将 npm 的 `.cmd` 垫片解析为直接调用 `node <script>` 的 Command。
/// 查找标准 npm 目录布局 `<cmd_dir>/node_modules/@openai/codex/bin/codex.js`。
#[cfg(windows)]
fn resolve_node_script(cmd_path: &str) -> Option<Command> {
    let dir = std::path::Path::new(cmd_path).parent()?;
    let script = dir
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("bin")
        .join("codex.js");
    if !script.exists() {
        return None;
    }
    tracing::info!("resolved npm shim {} -> node {}", cmd_path, script.display());
    let mut c = Command::new("node");
    c.arg(script);
    Some(c)
}

#[cfg(not(windows))]
fn build_codex_command(bin: &str) -> Command {
    Command::new(bin)
}

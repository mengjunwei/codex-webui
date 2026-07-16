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
    ///
    /// ## 三阶段启动（防"初始化期间关闭"竞态）
    ///
    /// 1. **spawn_child**：仅启动子进程 + 创建 JSON-RPC 客户端（不初始化）。
    /// 2. **attach_forwarders + spawn_close_watcher**：在初始化之前就挂上
    ///    通知/服务端请求转发器 + 关闭监视器 —— 否则子进程若在握手期间退出，
    ///    监视器可能错过该事件。
    /// 3. **initialize_client**：发 `initialize` + `initialized` 通知。
    ///
    /// ## 写 `current` 与 destroy 的原子性
    ///
    /// 写 `current` 字段前先检查 `destroyed`：
    /// - spawn 之后、写 `current` 之前若 destroy 被调用，destroy 会 take 到 None；
    /// - 此刻在锁内重检 destroyed=true → 立即销毁刚刚启动的客户端，避免孤儿进程。
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
                // 启动时补齐缺失的默认 codex 配置（环境变量可控，仅缺失键才写，不覆盖用户配置）。
                crate::services::codex_status_config::apply_defaults_if_absent(&client).await;
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

    /// 指数退避重启逻辑。
    ///
    /// ## 退避公式
    ///
    /// `delay_ms = min(3000 × 2^attempt, 60_000)`，其中 attempt 是连续失败次数。
    /// 上限 60 秒防止长时间空转，下限 3 秒防止代码抖动。
    ///
    /// ## 关键不变量
    ///
    /// - `restarting` 标志：`swap(true)` 原子抢占，第二个并发重启调用立即返回。
    /// - `destroyed` 检查：避免优雅关闭后还试图重启。
    /// - `Box::pin(self.start()).await`：start 是 async fn，直接 await 会
    ///   形成 start ↔ restart 的无穷递归（restart → start → fail → restart）。
    ///   用 `Box::pin` 装箱后强制分配到堆上，栈帧不再是无限递归。
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
/// ## Windows 上的两层特判（npm 垫片陷阱）
///
/// npm 安装的 CLI 以 `.cmd`/`.bat` 垫片形式发布。通过 `cmd.exe /c` 启动这些
/// 垫片不会将 stdio 管道继承到内部的 node 孙进程（stdout 会立即关闭）。
/// 因此，对于 npm 垫片，我们解析出底层的 `node + codex.js` 并直接启动 `node`
/// —— node 直接继承管道，没有孙进程。非 npm 的 `.cmd`/`.bat` 退回到 `cmd.exe /c`
/// （使用绝对路径避免在 Git Bash 等进程 PATH 不含 system32 的环境下找不到 cmd.exe）。
/// 真正的 `.exe` 可执行文件以及非 Windows 平台直接启动。
#[cfg(windows)]
pub(crate) fn build_codex_command(bin: &str) -> Command {
    // 裸名解析：Windows 上 `Command::new("codex")` 走 CreateProcess，搜 PATH 时
    // 只自动补 `.exe`、不补 `.cmd`/`.bat`，因此 npm 全局安装的 codex（`codex.cmd`
    // 垫片，没有 `codex.exe`）会被判为 "program not found"。先用 `where` 把裸名
    // 解析成带扩展名的绝对路径，让下面的 `.cmd`/`.bat` 分支接管，进而由
    // `resolve_node_script` 直接启动 `node codex.js`（避免 cmd.exe /c 的 stdio
    // 孙进程问题）。解析失败则保持原样，交给 CreateProcess（兼容旧行为）。
    let bin: String = if is_bare_program_name(bin) {
        match resolve_program_path(bin) {
            Some(resolved) => {
                tracing::info!(
                    original = bin,
                    resolved = %resolved,
                    "resolved bare codex bin via where"
                );
                resolved
            }
            None => bin.to_string(),
        }
    } else {
        bin.to_string()
    };
    let lower = bin.to_ascii_lowercase();

    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        if let Some(cmd) = resolve_node_script(&bin) {
            return cmd;
        }
        // 兜底：使用绝对路径的 cmd.exe，避免 Rust 在 PATH 不含
        // C:\Windows\system32 的环境（Git Bash、msys2 等）中找不到。
        let comspec = std::env::var("COMSPEC")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "C:\\Windows\\system32\\cmd.exe".to_string());
        tracing::warn!(
            bin = %bin,
            comspec = %comspec,
            "falling back to cmd.exe /c (could not resolve npm shim to node script)"
        );
        let mut c = Command::new(comspec);
        c.arg("/c").arg(&bin);
        c
    } else {
        Command::new(&bin)
    }
}

/// 判断是否为"裸程序名"：不含路径分隔符，且没有可执行扩展名。
///
/// 这类名字交给 Windows `CreateProcess` 时只会按 `.exe` 搜索 PATH，
/// 无法命中 npm 的 `.cmd`/`.bat` 垫片，是 `Command::new("codex")` 报
/// "program not found" 的根因。
#[cfg(windows)]
fn is_bare_program_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains('\\') || lower.contains('/') {
        return false;
    }
    !(lower.ends_with(".exe")
        || lower.ends_with(".cmd")
        || lower.ends_with(".bat")
        || lower.ends_with(".ps1"))
}

/// 用 `where` 将裸名解析为绝对路径：优先 `.exe`，其次 `.cmd`/`.bat`。
///
/// 跳过无扩展名项（Git Bash 风格的 shell 垫片，`CreateProcess` 不认）与
/// `.ps1`（需 PowerShell 解释）。`where codex` 通常返回多行（`codex`、
/// `codex.cmd`），这里只挑能被直接 spawn 的带扩展名条目。
#[cfg(windows)]
fn resolve_program_path(name: &str) -> Option<String> {
    let out = std::process::Command::new("where").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut exe: Option<String> = None;
    let mut cmd_bat: Option<String> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let low = line.to_ascii_lowercase();
        if low.ends_with(".exe") {
            exe = Some(line.to_string());
            break;
        }
        if (low.ends_with(".cmd") || low.ends_with(".bat")) && cmd_bat.is_none() {
            cmd_bat = Some(line.to_string());
        }
    }
    exe.or(cmd_bat)
}

/// 将 npm 的 `.cmd` 垫片解析为直接调用 `node <script>` 的 Command。
/// 查找标准 npm 目录布局 `<cmd_dir>/node_modules/@openai/codex/bin/codex.js`。
#[cfg(windows)]
fn resolve_node_script(cmd_path: &str) -> Option<Command> {
    // Git Bash 启动的进程可能传入正斜杠混合的 Windows 路径，先做轻量规范化
    // 让 Path::parent 与 exists 行为稳定。失败时返回 None。
    let p = std::path::Path::new(cmd_path);
    let dir = p.parent()?;
    let script = dir
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("bin")
        .join("codex.js");
    tracing::debug!(
        cmd_path = cmd_path,
        dir = %dir.display(),
        script = %script.display(),
        exists = script.exists(),
        "resolve_node_script probe"
    );
    if !script.exists() {
        return None;
    }
    // 优先用绝对路径调用 node.exe，避免 Git Bash 启动的进程在
    // PATH 中找不到 node 时 `Command::new("node")` 报 program not found。
    let node_bin = locate_node_binary().unwrap_or_else(|| "node".to_string());
    tracing::info!(
        "resolved npm shim {} -> {} {}",
        cmd_path,
        node_bin,
        script.display()
    );
    let mut c = Command::new(node_bin);
    c.arg(script);
    Some(c)
}

#[cfg(not(windows))]
pub(crate) fn build_codex_command(bin: &str) -> Command {
    Command::new(bin)
}

/// 查找 `node` 可执行文件的绝对路径。
///
/// Windows 上 Rust 的 `Command::new("node")` 走 `CreateProcess`，需要
/// `node.exe` 出现在当前进程 PATH 中。Git Bash / MSYS2 / 某些 CI 启动
/// 的进程可能不满足该条件，导致 "program not found"。这里显式探测：
/// 1. 尝试 `where node.exe` 抓取 PATH 中第一个匹配；
/// 2. 回退到几个常见安装目录（`D:\Program Files\nodejs\node.exe`、
///    `C:\Program Files\nodejs\node.exe`）；
/// 3. 最后用 `which node`（来自 Git Bash）抓绝对路径；
/// 4. 全部失败返回 None，调用方兜底用 `"node"`。
fn locate_node_binary() -> Option<String> {
    #[cfg(windows)]
    {
        // 1) where node.exe
        if let Some(p) = run_where_first("node.exe") {
            return Some(p);
        }
        // 2) 常见安装目录
        for cand in [
            r"D:\Program Files\nodejs\node.exe",
            r"C:\Program Files\nodejs\node.exe",
            r"C:\Program Files (x86)\nodejs\node.exe",
        ] {
            if std::path::Path::new(cand).exists() {
                return Some(cand.to_string());
            }
        }
        // 3) which node（Git Bash 提供）
        if let Some(p) = run_cmd_capture("which node") {
            let p = p.trim().replace("/", "\\");
            if !p.is_empty() && std::path::Path::new(&p).exists() {
                return Some(p);
            }
        }
        // 4) NVM 风格：%APPDATA%\nvm\<version>\node.exe
        if let Ok(appdata) = std::env::var("APPDATA") {
            let nvm = std::path::Path::new(&appdata).join("nvm");
            if let Ok(rd) = std::fs::read_dir(&nvm) {
                for entry in rd.flatten() {
                    let p = entry.path().join("node.exe");
                    if p.exists() {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        // 类 Unix 平台 which 通常可用
        if let Ok(out) = std::process::Command::new("which").arg("node").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }
}

#[cfg(windows)]
fn run_where_first(name: &str) -> Option<String> {
    let out = std::process::Command::new("where").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

#[cfg(windows)]
fn run_cmd_capture(cmd: &str) -> Option<String> {
    let out = std::process::Command::new("cmd")
        .args(["/C", cmd])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn bare_program_name_detection() {
        // 裸名：需要 where 解析（CreateProcess 只补 .exe，命中不了 .cmd 垫片）
        assert!(is_bare_program_name("codex"));
        assert!(is_bare_program_name("CODEX"));
        assert!(is_bare_program_name("node"));

        // 带可执行扩展名：无需解析，直接交给对应分支
        assert!(!is_bare_program_name("codex.exe"));
        assert!(!is_bare_program_name("codex.cmd"));
        assert!(!is_bare_program_name("codex.bat"));
        assert!(!is_bare_program_name("codex.ps1"));
        assert!(!is_bare_program_name("CODEX.CMD"));

        // 含路径分隔符：已是路径，不应当作裸名解析
        assert!(!is_bare_program_name(r"C:\Users\me\codex"));
        assert!(!is_bare_program_name("./codex"));
        assert!(!is_bare_program_name("bin/codex"));
    }

    /// 宿主机装了 npm 全局 codex 时，resolve_program_path 应返回带扩展名的路径；
    /// 未安装时 where 失败返回 None，测试自动跳过（不断言）。
    #[test]
    fn resolve_program_path_picks_executable_extension() {
        if let Some(p) = resolve_program_path("codex") {
            let low = p.to_ascii_lowercase();
            assert!(
                low.ends_with(".exe") || low.ends_with(".cmd") || low.ends_with(".bat"),
                "expected an .exe/.cmd/.bat path, got {p}"
            );
        }
    }
}

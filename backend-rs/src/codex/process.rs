//! Codex app-server process lifecycle manager.
//!
//! Parity with `src/codex/codex-process-manager.service.ts`. Owns the JSON-RPC
//! client, performs the initialize handshake, tracks generation across restarts,
//! auto-restarts on exit (3000ms backoff), and re-broadcasts notifications /
//! server-requests through manager-level channels that **persist across restarts**
//! (so subscribers keep receiving events after a restart).

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
    /// `(generation, client)` — generation lets the close watcher avoid
    /// clobbering a newer spawn.
    current: Mutex<Option<(u64, Arc<CodexJsonRpcClient>)>>,
    generation: AtomicU64,
    restarting: AtomicBool,
    destroyed: AtomicBool,
    /// Manager-level notification/server-request channels — persist across restarts.
    notify_tx: broadcast::Sender<Value>,
    server_request_tx: broadcast::Sender<Value>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
}

impl CodexProcessManager {
    /// Construct without spawning (lazy). Call `start()` to spawn + initialize.
    pub fn new(codex_bin: String, codex_home: Option<String>) -> Self {
        let (notify_tx, _) = broadcast::channel::<Value>(256);
        let (server_request_tx, _) = broadcast::channel::<Value>(256);
        let (lifecycle_tx, _) = broadcast::channel::<LifecycleEvent>(32);
        Self {
            codex_bin,
            codex_home,
            current: Mutex::new(None),
            generation: AtomicU64::new(0),
            restarting: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
            notify_tx,
            server_request_tx,
            lifecycle_tx,
        }
    }

    /// Subscribe to app-server notifications (persist across restarts).
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Value> {
        self.notify_tx.subscribe()
    }

    /// Subscribe to server-initiated requests (persist across restarts).
    pub fn subscribe_server_requests(&self) -> broadcast::Receiver<Value> {
        self.server_request_tx.subscribe()
    }

    /// Subscribe to lifecycle events.
    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle_tx.subscribe()
    }

    /// Current JSON-RPC client, or `None` if not connected.
    pub async fn client(&self) -> Option<Arc<CodexJsonRpcClient>> {
        match self.current.lock().await.as_ref() {
            Some((_, c)) => Some(c.clone()),
            None => None,
        }
    }

    /// Current generation (0 before first successful init).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Send a JSON-RPC request to the current app-server, or error if unavailable.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match self.client().await {
            Some(c) => c.request(method, params).await,
            None => Err(RpcError::Closed),
        }
    }

    /// Spawn + initialize. On any failure, schedule a restart. Idempotent guard
    /// via the `restarting` flag is handled by callers / `restart`.
    pub async fn start(self: Arc<Self>) {
        if self.destroyed.load(Ordering::SeqCst) {
            return;
        }

        // Phase 1: spawn child process + create client (no init yet).
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

        // Phase 2: attach forwarders + close watcher BEFORE init (M1 FIX:
        // close-watcher race — if the child exits during init, the watcher
        // must already be subscribed to catch it).
        let new_generation = self.generation.load(Ordering::SeqCst) + 1;
        self.attach_forwarders(&client);
        self.spawn_close_watcher(&client, new_generation);

        // Phase 3: initialize handshake.
        match self.initialize_client(&client).await {
            Ok(init) => {
                self.generation.fetch_add(1, Ordering::SeqCst);
                let restarted = new_generation > 1;
                *self.current.lock().await = Some((new_generation, client));

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
                // Clean up the half-initialized client.
                client.destroy().await;
                if !self.destroyed.load(Ordering::SeqCst) {
                    self.clone().restart().await;
                }
            }
        }
    }

    /// Spawn the child process and create the JSON-RPC client (no handshake).
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

        // stderr reader: log app-server diagnostics as warnings.
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

    /// Perform the initialize handshake on an already-spawned client.
    async fn initialize_client(&self, client: &Arc<CodexJsonRpcClient>) -> Result<InitializeResponse, RpcError> {
        let params = serde_json::to_value(default_initialize_params())?;
        let init_value = client.request("initialize", Some(params)).await?;
        let init: InitializeResponse = serde_json::from_value(init_value)?;
        client.notify("initialized", Some(Value::Object(Default::default())))?;
        Ok(init)
    }

    /// Re-broadcast a fresh client's events into the manager-level channels.
    fn attach_forwarders(&self, client: &Arc<CodexJsonRpcClient>) {
        let mut notify_rx = client.subscribe_notifications();
        let mgr_notify = self.notify_tx.clone();
        tokio::spawn(async move {
            loop {
                match notify_rx.recv().await {
                    Ok(msg) => {
                        let _ = mgr_notify.send(msg);
                    }
                    Err(_) => break, // client closed
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
                    Err(_) => break,
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
        // Only clear if still the active client for this generation.
        let stale = {
            let mut current = self.current.lock().await;
            let stale = matches!(current.as_ref(), Some((g, _)) if *g == generation);
            if stale {
                *current = None;
            }
            stale
        };

        // Parity with TS: only emit Unavailable when this close is for the
        // active client (not a stale/duplicate close, and not destroy — which
        // already nulled `current` before the watcher wakes).
        if stale {
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
        let generation = self.generation.load(Ordering::SeqCst);
        tracing::warn!(generation, delay_ms = RESTART_DELAY_MS, "restarting codex app-server");
        let _ = self.lifecycle_tx.send(LifecycleEvent::Restarting {
            generation,
            delay_ms: RESTART_DELAY_MS,
        });
        sleep(Duration::from_millis(RESTART_DELAY_MS)).await;
        self.restarting.store(false, Ordering::SeqCst);
        if !self.destroyed.load(Ordering::SeqCst) {
            // Boxed to break the start ↔ restart async recursion.
            Box::pin(self.start()).await;
        }
    }

    /// Stop the manager: destroy the client and prevent restarts.
    pub async fn destroy(&self) {
        self.destroyed.store(true, Ordering::SeqCst);
        if let Some((_, client)) = self.current.lock().await.take() {
            client.destroy().await;
        }
    }
}

/// Build the Command for spawning codex.
///
/// On Windows, npm-installed CLIs ship as `.cmd`/`.bat` shims. Spawning those
/// via `cmd.exe /c` does NOT inherit stdio pipes to the inner node grandchild
/// process (stdout closes immediately). Instead, for npm shims we resolve the
/// underlying `node + codex.js` and spawn `node` directly — node inherits the
/// pipes with no grandchild. Non-npm `.cmd`/`.bat` fall back to `cmd.exe /c`.
/// Real `.exe` binaries and non-Windows platforms spawn directly.
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

/// Resolve an npm `.cmd` shim to a direct `node <script>` Command.
/// Looks for the standard npm layout `<cmd_dir>/node_modules/@openai/codex/bin/codex.js`.
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

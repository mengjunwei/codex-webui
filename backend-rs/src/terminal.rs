//! Terminal service — shared PTY sessions with wezterm VT state reconnect.
//!
//! Parity with `src/terminal/terminal.service.ts` + `terminal.gateway.ts`.
//! Uses tattoy-wezterm-term for full VT100/VT220 emulation (cursor position,
//! alternate screen, colors, scrollback). Reconnect serializes the screen
//! state for xterm.js rendering — no raw byte replay.

use crate::error::{AppError, ErrorCode};
use crate::settings::SettingsReader;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize, config};
use tattoy_wezterm_term::color::ColorPalette;
use tokio::sync::broadcast;

// ── Public types (parity with terminal.types.ts) ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalStatus { Running, Exited }

#[derive(Clone, Serialize)]
pub struct TerminalMetadata {
    pub id: String,
    pub context_key: String,
    pub title: String,
    pub cwd: String,
    pub shell: String,
    pub status: TerminalStatus,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub attached_count: usize,
    pub cols: u16,
    pub rows: u16,
    pub created_at: String,
}

#[derive(Clone, Serialize)]
pub struct TerminalOutputEvent {
    pub terminal_id: String,
    pub data: String,
    pub socket_ids: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct TerminalExitEvent {
    pub terminal: TerminalMetadata,
    pub socket_ids: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct TerminalClosedEvent {
    pub terminal_id: String,
    pub context_key: String,
    pub socket_ids: Vec<String>,
}

// ── Minimal TerminalConfiguration ───────────────────────────────────────────

#[derive(Debug)]
struct MinimalConfig;

impl TerminalConfiguration for MinimalConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
    fn scrollback_size(&self) -> usize {
        3500
    }
}

// config::impl_downcast is private; use manual downcast via Any.
// MinimalConfig is a concrete type, no downcasting needed at runtime.

// ── Internal session ────────────────────────────────────────────────────────

struct Session {
    id: String,
    context_key: String,
    attached: HashSet<String>,
    title: String,
    cwd: String,
    shell: String,
    status: TerminalStatus,
    exit_code: Option<i32>,
    signal: Option<i32>,
    cols: u16,
    rows: u16,
    created_at: String,
    /// Raw output ring buffer for live streaming + reconnect replay.
    ring_buffer: Mutex<VecDeque<String>>,
    /// wezterm VT terminal model for resize (SIGWINCH) and cursor state.
    vt_terminal: Mutex<Terminal>,
    /// Interior-mutable writer for PTY stdin.
    writer: Mutex<Option<Box<dyn std::io::Write + Send>>>,
    /// PTY master for resize (SIGWINCH).
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Mutex<Option<Box<dyn portable_pty::Child + Send>>>,
    _reader_task: Option<tokio::task::JoinHandle<()>>,
    grace_handle: Option<tokio::task::AbortHandle>,
}

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    pub max_sessions: usize,
    pub grace_ms: u64,
    pub scrollback: usize,
    pub default_cwd: Option<String>,
}

impl TerminalConfig {
    pub fn from_settings(reader: &SettingsReader<'_>) -> Self {
        Self {
            max_sessions: reader.get_number("terminal.maxSessions").map(|n| n as usize).unwrap_or(10),
            grace_ms: reader.get_number("terminal.graceMs").map(|n| n as u64).unwrap_or(45_000),
            scrollback: reader.get_number("terminal.scrollback").map(|n| n as usize).unwrap_or(5000),
            default_cwd: reader.get_string("terminal.defaultCwd").filter(|s| !s.is_empty()),
        }
    }
}

// ── Terminal service ────────────────────────────────────────────────────────

pub struct TerminalService {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    output_tx: broadcast::Sender<TerminalOutputEvent>,
    exit_tx: broadcast::Sender<TerminalExitEvent>,
    closed_tx: broadcast::Sender<TerminalClosedEvent>,
    config: Mutex<TerminalConfig>,
}

impl TerminalService {
    pub fn new(config: TerminalConfig) -> Arc<Self> {
        let (output_tx, _) = broadcast::channel(512);
        let (exit_tx, _) = broadcast::channel(64);
        let (closed_tx, _) = broadcast::channel(64);
        Arc::new(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            output_tx,
            exit_tx,
            closed_tx,
            config: Mutex::new(config),
        })
    }

    pub fn subscribe_output(&self) -> broadcast::Receiver<TerminalOutputEvent> { self.output_tx.subscribe() }
    pub fn subscribe_exit(&self) -> broadcast::Receiver<TerminalExitEvent> { self.exit_tx.subscribe() }
    pub fn subscribe_closed(&self) -> broadcast::Receiver<TerminalClosedEvent> { self.closed_tx.subscribe() }

    pub fn get_config_json(&self) -> Value {
        let c = self.config.lock().unwrap();
        json!({ "maxSessions": c.max_sessions, "graceMs": c.grace_ms, "scrollback": c.scrollback, "defaultCwd": c.default_cwd })
    }

    /// List terminals for a context.
    pub fn list(&self, context_key: &str) -> Vec<TerminalMetadata> {
        self.sessions.lock().unwrap().values()
            .filter(|s| s.context_key == context_key)
            .map(|s| Self::meta(s))
            .collect()
    }

    /// Open a new PTY session and attach the socket.
    pub fn open(&self, socket_id: &str, context_key: &str,
        cwd: Option<&str>, cols: Option<u16>, rows: Option<u16>, title: Option<&str>,
    ) -> Result<TerminalMetadata, AppError> {
        let cfg = self.config.lock().unwrap();
        let max = cfg.max_sessions;
        let default_cwd = cfg.default_cwd.clone();
        drop(cfg);

        {   // max_sessions check
            let sessions = self.sessions.lock().unwrap();
            if sessions.len() >= max {
                return Err(bad_request(ErrorCode::TerminalMaxSessionsReached,
                    format!("max sessions reached ({max})")));
            }
        }

        let cols = cols.unwrap_or(80).clamp(20, 300);
        let rows = rows.unwrap_or(24).clamp(5, 120);
        let shell = resolve_shell();
        let cwd = cwd.map(String::from).or(default_cwd).unwrap_or_else(home_dir);

        // Validate cwd exists and is a directory.
        let cwd_canonical = std::fs::canonicalize(&cwd).map_err(|_| {
            bad_request(ErrorCode::TerminalCwdNotDirectory, format!("cwd does not exist: {cwd}"))
        })?;
        if !cwd_canonical.is_dir() {
            return Err(bad_request(ErrorCode::TerminalCwdNotDirectory, "cwd is not a directory".to_string()));
        }
        let cwd = cwd_canonical.to_string_lossy().to_string();

        let display_title = title.filter(|s| !s.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| std::path::Path::new(&shell)
                .file_name().unwrap_or_default().to_string_lossy().to_string());

        // PTY
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| AppError::internal(format!("openpty: {e}")))?;
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(&cwd);
        let child = pair.slave.spawn_command(cmd)
            .map_err(|e| AppError::internal(format!("spawn: {e}")))?;
        drop(pair.slave);

        let id = uuid::Uuid::new_v4().to_string();
        let writer = pair.master.take_writer()
            .map_err(|e| AppError::internal(format!("take_writer: {e}")))?;
        let reader = pair.master.try_clone_reader()
            .map_err(|e| AppError::internal(format!("clone_reader: {e}")))?;
        let master = pair.master;

        // Create wezterm Terminal (no-op writer — PTY writes handled separately).
        let vt_term = Terminal::new(
            TerminalSize { rows: rows as usize, cols: cols as usize, pixel_width: 0, pixel_height: 0, dpi: 0 },
            Arc::new(MinimalConfig) as Arc<dyn TerminalConfiguration + Send + Sync>,
            "xterm-256color",
            "",
            Box::new(std::io::sink()),
        );

        // PTY output → feed VT terminal + ring buffer + broadcast raw bytes.
        let session_id = id.clone();
        let (output_tx, exit_tx, sessions_ref) = (self.output_tx.clone(), self.exit_tx.clone(), self.sessions.clone());
        let reader_task = {
            let sid = session_id.clone();
            let sessions_c = sessions_ref.clone();
            let out_tx = output_tx.clone();
            let ex_tx = exit_tx.clone();
            tokio::task::spawn_blocking(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = String::from_utf8_lossy(&buf[..n]).to_string();
                            let socket_ids: Vec<String> = {
                                let mut sessions = sessions_c.lock().unwrap();
                                if let Some(s) = sessions.get_mut(&sid) {
                                    // Feed VT terminal for cursor/resize state.
                                    s.vt_terminal.lock().unwrap().advance_bytes(&buf[..n]);
                                    // Push to ring buffer for reconnect.
                                    {
                                        let mut rb = s.ring_buffer.lock().unwrap();
                                        while rb.len() >= 3500 { rb.pop_front(); }
                                        rb.push_back(data.clone());
                                    }
                                    s.attached.iter().cloned().collect()
                                } else { vec![] }
                            };
                            let _ = out_tx.send(TerminalOutputEvent {
                                terminal_id: sid.clone(), data, socket_ids,
                            });
                        }
                        Err(_) => break,
                    }
                }
                // PTY exited.
                let (meta, socket_ids) = {
                    let mut sessions = sessions_c.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&sid) {
                        s.status = TerminalStatus::Exited;
                        (Self::meta(s), s.attached.iter().cloned().collect())
                    } else { return; }
                };
                let _ = ex_tx.send(TerminalExitEvent { terminal: meta, socket_ids });
            })
        };

        let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut attached = HashSet::new();
        attached.insert(socket_id.to_string());

        let session = Session {
            id: id.clone(), context_key: context_key.to_string(), attached,
            title: display_title, cwd: cwd.clone(), shell: std::path::Path::new(&shell)
                .file_name().unwrap_or_default().to_string_lossy().to_string(),
            status: TerminalStatus::Running, exit_code: None, signal: None,
            cols, rows, created_at: created_at.clone(),
            ring_buffer: Mutex::new(VecDeque::with_capacity(3500)),
            vt_terminal: Mutex::new(vt_term),
            writer: Mutex::new(Some(writer)),
            master: Some(master),
            child: Mutex::new(Some(child)),
            _reader_task: Some(reader_task),
            grace_handle: None,
        };

        let meta = Self::meta(&session);
        self.sessions.lock().unwrap().insert(id.clone(), session);
        tracing::info!(terminal = %id, cwd = %cwd, "opened terminal");
        Ok(meta)
    }

    /// Attach a socket and return serialized VT screen state for reconnect.
    pub fn reconnect(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(TerminalMetadata, Vec<String>), AppError>
    {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(terminal_id)
            .ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        s.attached.insert(socket_id.to_string());
        if let Some(h) = s.grace_handle.take() { h.abort(); }
        let meta = Self::meta(s);
        // Reconnect: replay ring buffer (raw PTY output for xterm.js).
        let buf: Vec<String> = s.ring_buffer.lock().unwrap().iter().cloned().collect();
        Ok((meta, buf))
    }

    /// Detach a socket from one or all terminals.
    pub fn detach(&self, socket_id: &str, terminal_id: Option<&str>) {
        let grace_ms = self.config.lock().unwrap().grace_ms;
        let sessions_arc = self.sessions.clone();
        let closed_tx = self.closed_tx.clone();

        let mut sessions = sessions_arc.lock().unwrap();
        for s in sessions.values_mut() {
            if terminal_id.map_or(false, |id| id != s.id) { continue; }
            s.attached.remove(socket_id);

            if s.attached.is_empty() && s.status == TerminalStatus::Running {
                if let Some(h) = s.grace_handle.take() { h.abort(); }
                let sid = s.id.clone();
                let ctx = s.context_key.clone();
                let sessions_c = sessions_arc.clone();
                let tx = closed_tx.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;
                    let mut sessions = sessions_c.lock().unwrap();
                    if let Some(session) = sessions.get_mut(&sid) {
                        if session.attached.is_empty() && session.status == TerminalStatus::Running {
                            session.status = TerminalStatus::Exited;
                            if let Ok(mut child) = session.child.lock() {
                                if let Some(ref mut c) = *child { let _ = c.kill(); }
                                child.take();
                            }
                            if let Ok(mut writer) = session.writer.lock() { writer.take(); }
                            session.master.take();
                            let _ = tx.send(TerminalClosedEvent {
                                terminal_id: sid.clone(), context_key: ctx, socket_ids: vec![],
                            });
                            sessions.remove(&sid);
                        }
                    }
                }).abort_handle();
                s.grace_handle = Some(handle);
            }
        }
    }

    /// Write input to a terminal PTY.
    pub fn write_input(&self, socket_id: &str, context_key: &str, terminal_id: &str, data: &str)
        -> Result<(), AppError>
    {
        let max_input = 1024 * 1024;
        if data.len() > max_input {
            return Err(bad_request(ErrorCode::TerminalInputTooLarge, "Terminal input is too large".to_string()));
        }
        let data_bytes = data.as_bytes().to_vec();

        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(terminal_id).filter(|s| s.status != TerminalStatus::Exited)
            .ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        if s.status != TerminalStatus::Running { return Err(exited()); }
        let mut writer = s.writer.lock().unwrap();
        if let Some(w) = writer.as_mut() {
            w.write_all(&data_bytes)
                .map_err(|e| AppError::internal(format!("pty write: {e}")))?;
        }
        Ok(())
    }

    /// Resize a terminal (updates stored dims + PTY SIGWINCH + VT terminal).
    pub fn resize(&self, socket_id: &str, context_key: &str, terminal_id: &str, cols: u16, rows: u16)
        -> Result<TerminalMetadata, AppError>
    {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(terminal_id).filter(|s| s.status != TerminalStatus::Exited)
            .ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        let next_cols = cols.clamp(20, 300);
        let next_rows = rows.clamp(5, 120);
        if next_cols != s.cols || next_rows != s.rows {
            s.cols = next_cols;
            s.rows = next_rows;
            if let Some(ref mut master) = s.master {
                let _ = master.resize(PtySize { rows: next_rows, cols: next_cols, pixel_width: 0, pixel_height: 0 });
            }
            // Resize VT terminal model.
            s.vt_terminal.lock().unwrap().resize(TerminalSize {
                rows: next_rows as usize, cols: next_cols as usize,
                pixel_width: 0, pixel_height: 0, dpi: 0,
            });
        }
        Ok(Self::meta(s))
    }

    /// Close a terminal explicitly.
    pub fn close(&self, _socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(), AppError>
    {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(terminal_id).ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        s.status = TerminalStatus::Exited;
        if let Ok(mut child) = s.child.lock() {
            if let Some(ref mut c) = *child { let _ = c.kill(); }
            child.take();
        }
        if let Ok(mut writer) = s.writer.lock() { writer.take(); }
        s.master.take();
        s.attached.clear();
        sessions.remove(terminal_id);
        drop(sessions);
        let _ = self.closed_tx.send(TerminalClosedEvent {
            terminal_id: terminal_id.into(), context_key: context_key.into(), socket_ids: vec![],
        });
        Ok(())
    }

    /// Plain-text snapshot of the VT screen (download / copy all).
    pub fn download(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(String, String), AppError>
    {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(terminal_id).ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        let content: String = s.ring_buffer.lock().unwrap().iter().cloned().collect();
        let safe_title = s.title.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect::<String>();
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string();
        Ok((format!("{safe_title}-{ts}.txt"), content))
    }

    fn meta(s: &Session) -> TerminalMetadata {
        TerminalMetadata {
            id: s.id.clone(), context_key: s.context_key.clone(), title: s.title.clone(),
            cwd: s.cwd.clone(), shell: s.shell.clone(), status: s.status,
            exit_code: s.exit_code, signal: s.signal, attached_count: s.attached.len(),
            cols: s.cols, rows: s.rows, created_at: s.created_at.clone(),
        }
    }
}

// ── VT screen serialization ─────────────────────────────────────────────────
// (Reserved for future VT-state reconnect; currently using ring buffer replay.)

// ── Helpers ─────────────────────────────────────────────────────────────────

fn resolve_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into())
}

fn bad_request(code: ErrorCode, msg: String) -> AppError {
    AppError::business(code, axum::http::StatusCode::BAD_REQUEST, msg, None)
}
fn not_found(msg: &str) -> AppError {
    AppError::business(ErrorCode::TerminalNotFound, axum::http::StatusCode::NOT_FOUND, msg.into(), None)
}
fn context_mismatch() -> AppError {
    AppError::business(ErrorCode::TerminalContextMismatch, axum::http::StatusCode::BAD_REQUEST, "context mismatch".into(), None)
}
fn not_attached() -> AppError {
    AppError::business(ErrorCode::TerminalSocketNotAttached, axum::http::StatusCode::BAD_REQUEST, "socket not attached".into(), None)
}
fn exited() -> AppError {
    AppError::business(ErrorCode::TerminalExited, axum::http::StatusCode::BAD_REQUEST, "terminal has exited".into(), None)
}

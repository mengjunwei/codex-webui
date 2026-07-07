//! 终端服务 —— 共享 PTY 会话，支持基于 wezterm VT 状态的重连。
//!
//! 与 `src/terminal/terminal.service.ts` 及 `terminal.gateway.ts` 保持对齐。
//! 使用 wezterm-term 实现完整的 VT100/VT220 模拟（光标位置、备用屏幕、
//! 颜色、回滚缓冲）。重连时会将屏幕状态序列化以供 xterm.js 渲染 ——
//! 不再进行原始字节重放。

use crate::error::{AppError, ErrorCode};
use crate::settings::SettingsReader;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use wezterm_term::color::ColorPalette;
use tokio::sync::broadcast;

// ── 公共类型（与 terminal.types.ts 保持对齐） ─────────────────────────────

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

// ── 最小化的 TerminalConfiguration ───────────────────────────────────────────

#[derive(Debug)]
struct MinimalConfig {
    scrollback: usize,
}

impl TerminalConfiguration for MinimalConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }
}

// config::impl_downcast 是私有的；这里通过 Any 手动做 downcast。
// MinimalConfig 是具体类型，运行时无需 downcast。

// ── 内部会话 ────────────────────────────────────────────────────────

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
    /// wezterm VT 终端模型，用于保存屏幕状态、resize 以及重连序列化。
    vt_terminal: Mutex<Terminal>,
    /// 用于写入 PTY stdin 的内部可变 writer。
    writer: Mutex<Option<Box<dyn std::io::Write + Send>>>,
    /// 用于 resize 的 PTY master（SIGWINCH）。
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Mutex<Option<Box<dyn portable_pty::Child + Send>>>,
    _reader_task: Option<tokio::task::JoinHandle<()>>,
    grace_handle: Option<tokio::task::AbortHandle>,
}

// ── 配置 ──────────────────────────────────────────────────────────────────

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

#[derive(Clone, Serialize)]
pub struct TerminalMetadataEvent {
    pub terminal: TerminalMetadata,
    pub socket_ids: Vec<String>,
}

// ── 终端服务 ────────────────────────────────────────────────────────

pub struct TerminalService {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    output_tx: broadcast::Sender<TerminalOutputEvent>,
    exit_tx: broadcast::Sender<TerminalExitEvent>,
    closed_tx: broadcast::Sender<TerminalClosedEvent>,
    metadata_tx: broadcast::Sender<TerminalMetadataEvent>,
    config: Mutex<TerminalConfig>,
}

impl TerminalService {
    pub fn new(config: TerminalConfig) -> Arc<Self> {
        let (output_tx, _) = broadcast::channel(512);
        let (exit_tx, _) = broadcast::channel(64);
        let (closed_tx, _) = broadcast::channel(64);
        let (metadata_tx, _) = broadcast::channel(64);
        Arc::new(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            output_tx,
            exit_tx,
            closed_tx,
            metadata_tx,
            config: Mutex::new(config),
        })
    }

    pub fn subscribe_output(&self) -> broadcast::Receiver<TerminalOutputEvent> { self.output_tx.subscribe() }
    pub fn subscribe_exit(&self) -> broadcast::Receiver<TerminalExitEvent> { self.exit_tx.subscribe() }
    pub fn subscribe_closed(&self) -> broadcast::Receiver<TerminalClosedEvent> { self.closed_tx.subscribe() }
    pub fn subscribe_metadata(&self) -> broadcast::Receiver<TerminalMetadataEvent> { self.metadata_tx.subscribe() }

    /// M2: 广播元数据变更给所有已附着的客户端。
    fn emit_metadata(&self, s: &Session) {
        let socket_ids: Vec<String> = s.attached.iter().cloned().collect();
        let _ = self.metadata_tx.send(TerminalMetadataEvent {
            terminal: Self::meta(s),
            socket_ids,
        });
    }

    pub fn get_config_json(&self) -> Value {
        let c = self.config.lock().unwrap();
        json!({ "maxSessions": c.max_sessions, "graceMs": c.grace_ms, "scrollback": c.scrollback, "defaultCwd": c.default_cwd })
    }

    /// 列出某个 context 下的终端。
    pub fn list(&self, context_key: &str) -> Vec<TerminalMetadata> {
        self.sessions.lock().unwrap().values()
            .filter(|s| s.context_key == context_key)
            .map(|s| Self::meta(s))
            .collect()
    }

    /// 打开一个新的 PTY 会话并挂载 socket。
    pub fn open(&self, socket_id: &str, context_key: &str,
        cwd: Option<&str>, cols: Option<u16>, rows: Option<u16>, title: Option<&str>,
    ) -> Result<TerminalMetadata, AppError> {
        let cfg = self.config.lock().unwrap();
        let max = cfg.max_sessions;
        let scrollback = cfg.scrollback;
        let default_cwd = cfg.default_cwd.clone();
        drop(cfg);

        {   // max_sessions 上限检查
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

        // 校验 cwd 存在且是一个目录。
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

        // 使用配置中的 scrollback 创建 wezterm Terminal。
        let vt_term = Terminal::new(
            TerminalSize { rows: rows as usize, cols: cols as usize, pixel_width: 0, pixel_height: 0, dpi: 0 },
            Arc::new(MinimalConfig { scrollback }) as Arc<dyn TerminalConfiguration + Send + Sync>,
            "xterm-256color",
            "",
            Box::new(std::io::sink()),
        );

        // PTY 输出 → 喂给 VT 终端 + 环形缓冲 + 广播原始字节。
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
                let mut carry: Vec<u8> = Vec::new(); // H1: 跨块 UTF-8 续传缓冲
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // H1 修复：合并 carry + 新数据，在最后一个完整 UTF-8 边界处切割。
                            carry.extend_from_slice(&buf[..n]);
                            let boundary = find_utf8_boundary(&carry);
                            let (valid, rest) = carry.split_at(boundary);
                            let data = String::from_utf8_lossy(valid).to_string();
                            let raw = valid.to_vec();
                            carry = rest.to_vec();

                            let socket_ids: Vec<String> = {
                                let mut sessions = sessions_c.lock().unwrap();
                                if let Some(s) = sessions.get_mut(&sid) {
                                    // 用原始字节喂给 VT 终端（正确的 UTF-8 处理）。
                                    s.vt_terminal.lock().unwrap().advance_bytes(&raw);
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
                // PTY 已退出。M1 修复：捕获退出码/signal。
                let (meta, socket_ids) = {
                    let mut sessions = sessions_c.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&sid) {
                        s.status = TerminalStatus::Exited;
                        // 尝试获取退出码。
                        if let Ok(mut child_guard) = s.child.lock() {
                            if let Some(ref mut child) = *child_guard {
                                if let Ok(status) = child.wait() {
                                    s.exit_code = Some(status.exit_code() as i32);
                                }
                            }
                        }
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

    /// 挂载 socket 并返回 VT 屏幕状态以供重连。
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
        // 序列化 VT 屏幕：通过 wezterm Terminal 获取回滚 + 可见行。
        let state = serialize_terminal_screen(&s.vt_terminal.lock().unwrap());
        // M2: 通知其他客户端有人重新连上了（attached_count 变化）。
        self.emit_metadata(s);
        Ok((meta, vec![state]))
    }

    /// 从一个或全部终端上解绑 socket。
    pub fn detach(&self, socket_id: &str, terminal_id: Option<&str>) {
        let grace_ms = self.config.lock().unwrap().grace_ms;
        let sessions_arc = self.sessions.clone();
        let closed_tx = self.closed_tx.clone();

        let mut sessions = sessions_arc.lock().unwrap();
        // H4 修复：收集需要立即清理的已退出 session。
        let mut to_remove: Vec<(String, String)> = Vec::new();
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
            // H4 修复：已退出的 session 在所有 socket 断开后立即清理。
            if s.attached.is_empty() && s.status == TerminalStatus::Exited {
                to_remove.push((s.id.clone(), s.context_key.clone()));
            }
        }
        // 执行 H4 清理（在锁外发送事件避免死锁）。
        for (id, ctx) in &to_remove {
            if let Some(s) = sessions.get_mut(id) {
                if let Ok(mut child) = s.child.lock() {
                    if let Some(ref mut c) = *child { let _ = c.kill(); }
                    child.take();
                }
                if let Ok(mut writer) = s.writer.lock() { writer.take(); }
                s.master.take();
            }
            sessions.remove(id);
            let _ = closed_tx.send(TerminalClosedEvent {
                terminal_id: id.clone(), context_key: ctx.clone(), socket_ids: vec![],
            });
        }
    }

    /// 向某个终端的 PTY 写入输入。
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

    /// 调整终端尺寸（更新存储的尺寸 + PTY SIGWINCH + VT 终端）。
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
            // 调整 VT 终端模型尺寸。
            s.vt_terminal.lock().unwrap().resize(TerminalSize {
                rows: next_rows as usize, cols: next_cols as usize,
                pixel_width: 0, pixel_height: 0, dpi: 0,
            });
            // M2: 广播元数据变更给所有已附着客户端。
            self.emit_metadata(s);
        }
        Ok(Self::meta(s))
    }

    /// 重命名终端标签（所有已附着客户端共享）。
    pub fn rename(&self, socket_id: &str, context_key: &str, terminal_id: &str, title: &str)
        -> Result<TerminalMetadata, AppError>
    {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(terminal_id)
            .ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return Err(bad_request(ErrorCode::TerminalInvalidContext, "title must not be empty".to_string()));
        }
        s.title = trimmed.to_string();
        self.emit_metadata(s);
        Ok(Self::meta(s))
    }

    /// 显式关闭某个终端。
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

    /// VT 屏幕的纯文本快照（用于下载 / 全部复制）。
    pub fn download(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(String, String), AppError>
    {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(terminal_id).ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        let content = serialize_terminal_screen(&s.vt_terminal.lock().unwrap());
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

// ── VT 屏幕序列化 ─────────────────────────────────────────────────

/// 将 wezterm 终端的屏幕（回滚 + 可见行）序列化为单个字符串。
/// 使用 scrollback_rows() + lines_in_phys_range()（原始 wezterm-term 中
/// 非测试的公开 API）。
fn serialize_terminal_screen(term: &Terminal) -> String {
    let screen = term.screen();
    let total = screen.scrollback_rows();
    let lines = screen.lines_in_phys_range(0..total);
    let mut out = String::new();
    for line in &lines {
        let text: std::borrow::Cow<str> = line.as_str();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

// ── 辅助函数 ─────────────────────────────────────────────────────────────────

/// H1 修复：在字节流的末尾找到最后一个完整 UTF-8 序列的边界。
/// 返回可安全转为字符串的字节数；尾部不完整的序列留给下一次合并。
fn find_utf8_boundary(bytes: &[u8]) -> usize {
    if bytes.is_empty() { return 0; }
    // 从末尾往回找，最多检查 4 字节（UTF-8 最长 4 字节）。
    let start = bytes.len().saturating_sub(4);
    for i in (start..bytes.len()).rev() {
        let b = bytes[i];
        if b & 0xC0 != 0x80 {
            // 这是一个首字节（非续传字节）。检查后续是否有足够的续传字节。
            let expected = if b < 0x80 { 1 }
                else if b & 0xE0 == 0xC0 { 2 }
                else if b & 0xF0 == 0xE0 { 3 }
                else if b & 0xF8 == 0xF0 { 4 }
                else { 1 }; // 非法首字节，按 1 字节处理
            if i + expected <= bytes.len() {
                return bytes.len(); // 最后一个序列完整，整个缓冲区安全
            }
            return i; // 首字节后缺续传字节，在此处切割
        }
    }
    bytes.len() // 全是续传字节（极端情况），返回全长（lossy 会处理）
}

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

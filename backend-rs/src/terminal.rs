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
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{
    CellAttributes, Intensity, Terminal, TerminalConfiguration, TerminalSize, Underline,
};
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
    /// 用 Arc<Mutex> 以便 reader_task / reconnect 在释放全局 sessions 锁后仍可访问。
    vt_terminal: Arc<Mutex<Terminal>>,
    /// 用于写入 PTY stdin 的内部可变 writer（Arc 以便 write_input 锁外写入）。
    writer: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>,
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
    pub async fn from_settings(reader: &SettingsReader<'_>) -> Self {
        Self {
            max_sessions: reader.get_number("terminal.maxSessions").await.map(|n| n as usize).unwrap_or(10),
            grace_ms: reader.get_number("terminal.graceMs").await.map(|n| n as u64).unwrap_or(45_000),
            scrollback: reader.get_number("terminal.scrollback").await.map(|n| n as usize).unwrap_or(5000),
            default_cwd: reader.get_string("terminal.defaultCwd").await.filter(|s| !s.is_empty()),
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
        let (output_tx, _) = broadcast::channel(4096);
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

    /// 配置的默认终端 cwd（供 realtime 层在做 cwd 沙箱解析时按 TS 优先级使用）。
    pub fn default_cwd(&self) -> Option<String> {
        self.config.lock().unwrap().default_cwd.clone()
    }

    /// 列出某个 context 下的终端。
    pub fn list(&self, context_key: &str) -> Result<Vec<TerminalMetadata>, AppError> {
        // 对齐 TS normalizeContextKey：非法 contextKey 抛 terminal.invalid_context。
        let context_key = normalize_context_key(context_key)?;
        Ok(self.sessions.lock().unwrap().values()
            .filter(|s| s.context_key == context_key)
            .map(|s| Self::meta(s))
            .collect())
    }

    /// 打开一个新的 PTY 会话并挂载 socket。
    pub fn open(&self, socket_id: &str, context_key: &str,
        cwd: Option<&str>, cols: Option<u16>, rows: Option<u16>, title: Option<&str>,
    ) -> Result<TerminalMetadata, AppError> {
        let context_key = normalize_context_key(context_key)?;
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
        let cwd_raw = cwd_canonical.to_string_lossy();
        // 剥离 Windows 长路径前缀 `\\?\`，避免 PowerShell prompt 显示 `Microsoft.PowerShell.Core\FileSystem::\\?\...`。
        let cwd = cwd_raw.strip_prefix("\\\\?\\").unwrap_or(&cwd_raw);

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
        // 设置 TERM（对齐 node-pty 的 name:'xterm-256color'）；服务/Docker 环境下
        // 父进程可能没有 TERM，否则 vim/htop/彩色 ls 会失效。
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
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

                            // H9 修复：锁 sessions 仅 clone vt_terminal Arc + 收集 attached，
                            // 释放 sessions 锁后再做 VT 解析（高频重活），避免串行化所有终端操作。
                            let (vt_clone, socket_ids): (Option<Arc<Mutex<Terminal>>>, Vec<String>) = {
                                let sessions = sessions_c.lock().unwrap();
                                if let Some(s) = sessions.get(&sid) {
                                    (Some(s.vt_terminal.clone()), s.attached.iter().cloned().collect())
                                } else {
                                    (None, vec![])
                                }
                            };
                            if let Some(vt) = vt_clone {
                                // 用原始字节喂给 VT 终端（正确的 UTF-8 处理）。
                                vt.lock().unwrap().advance_bytes(&raw);
                            }
                            let _ = out_tx.send(TerminalOutputEvent {
                                terminal_id: sid.clone(), data, socket_ids,
                            });
                        }
                        Err(_) => break,
                    }
                }
                // PTY 已退出（Ok(0)=EOF 或 Err）。T1+T3+T4：锁内仅置 Exited + take child +
                // 收集 socket_ids，释放 sessions 锁后再 wait（避免持全局锁做阻塞 waitpid
                // 挂起整个终端服务）；Err 分支也走此收尾（T4：不再静默丢状态/事件/僵尸）。
                let (child_opt, socket_ids) = {
                    let mut sessions = sessions_c.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&sid) {
                        s.status = TerminalStatus::Exited;
                        // portable-pty 0.8 的 signal 字段无私有 getter，仅取 exit_code。
                        let child_opt = s.child.lock().ok().and_then(|mut g| g.take());
                        (child_opt, s.attached.iter().cloned().collect())
                    } else {
                        return;
                    }
                };
                // 锁外 wait（T1：不持 sessions 锁；T3：显式 wait 防僵尸 ——
                // portable-pty 0.8 的 Child=std::process::Child，其 drop 既不 kill 也不 wait）。
                let exit_code = child_opt
                    .and_then(|mut c| c.wait().ok())
                    .map(|st| st.exit_code() as i32);
                let meta = {
                    let mut sessions = sessions_c.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&sid) {
                        if let Some(code) = exit_code {
                            s.exit_code = Some(code);
                        }
                        Self::meta(s)
                    } else {
                        return;
                    }
                };
                let _ = ex_tx.send(TerminalExitEvent { terminal: meta, socket_ids });
            })
        };

        let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut attached = HashSet::new();
        attached.insert(socket_id.to_string());

        let mut session = Session {
            id: id.clone(), context_key: context_key.to_string(), attached,
            title: display_title, cwd: cwd.to_string(), shell: std::path::Path::new(&shell)
                .file_name().unwrap_or_default().to_string_lossy().to_string(),
            status: TerminalStatus::Running, exit_code: None, signal: None,
            cols, rows, created_at: created_at.clone(),
            vt_terminal: Arc::new(Mutex::new(vt_term)),
            writer: Arc::new(Mutex::new(Some(writer))),
            master: Some(master),
            child: Mutex::new(Some(child)),
            _reader_task: Some(reader_task),
            grace_handle: None,
        };

        let meta = Self::meta(&session);
        // H10 修复：insert 时原子重新检查 max_sessions，消除入口检查与 insert 之间的 TOCTOU
        // （多个并发 open 可能都通过入口检查；此处只允许 max 个真正落库，多余的清理 PTY 并拒绝）。
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= max {
                drop(sessions);
                // 回收刚创建的 PTY 资源：take child 后 kill+wait（T3：显式 wait 防僵尸，
                // portable-pty Child::drop 既不 kill 也不 wait）+ 释放 writer/master。
                let child_opt = session.child.lock().ok().and_then(|mut g| g.take());
                if let Some(mut c) = child_opt {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                if let Ok(mut w) = session.writer.lock() {
                    w.take();
                }
                session.master.take();
                return Err(bad_request(
                    ErrorCode::TerminalMaxSessionsReached,
                    format!("max sessions reached ({max})"),
                ));
            }
            sessions.insert(id.clone(), session);
        }
        tracing::info!(terminal = %id, cwd = %cwd, "opened terminal");
        Ok(meta)
    }

    /// 挂载 socket 并返回 VT 屏幕状态以供重连。
    pub fn reconnect(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(TerminalMetadata, Vec<String>), AppError>
    {
        let context_key = normalize_context_key(context_key)?;
        // 锁 sessions 做校验 + attached 变更 + clone vt_terminal Arc，随即释放锁。
        let (vt_clone, meta) = {
            let mut sessions = self.sessions.lock().unwrap();
            let s = sessions.get_mut(terminal_id)
                .ok_or_else(|| not_found("terminal not found"))?;
            if s.context_key != context_key { return Err(context_mismatch()); }
            s.attached.insert(socket_id.to_string());
            if let Some(h) = s.grace_handle.take() { h.abort(); }
            (s.vt_terminal.clone(), Self::meta(s))
        };
        // H9 修复：序列化 VT 屏幕（重活）移出 sessions 锁，仅持有 per-session 的 vt_terminal 锁。
        let state = serialize_terminal_screen(&vt_clone.lock().unwrap());
        // M2: 通知其他客户端有人重新连上了（attached_count 变化）。
        {
            let sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.get(terminal_id) {
                self.emit_metadata(s);
            }
        }
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
                    // T2+T3：锁内 take child + clone writer + remove，锁外 kill+wait + take writer + send。
                    let cleanup = {
                        let mut sessions = sessions_c.lock().unwrap();
                        if let Some(session) = sessions.get_mut(&sid) {
                            // 只要无附着 socket 即移除（无论 Running/Exited）；PTY 在 grace 窗口内
                            // 退出时 status 已被 reader_task 置 Exited。
                            if session.attached.is_empty() {
                                if session.status == TerminalStatus::Running {
                                    session.status = TerminalStatus::Exited;
                                }
                                let child_opt = session.child.lock().ok().and_then(|mut g| g.take());
                                let writer_arc = session.writer.clone();
                                session.master.take();
                                sessions.remove(&sid);
                                Some((child_opt, writer_arc))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some((child_opt, writer_arc)) = cleanup {
                        if let Some(mut c) = child_opt {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        if let Ok(mut w) = writer_arc.lock() {
                            w.take();
                        }
                        let _ = tx.send(TerminalClosedEvent {
                            terminal_id: sid, context_key: ctx, socket_ids: vec![],
                        });
                    }
                }).abort_handle();
                s.grace_handle = Some(handle);
            }
            // H4 修复：已退出的 session 在所有 socket 断开后立即清理。
            if s.attached.is_empty() && s.status == TerminalStatus::Exited {
                to_remove.push((s.id.clone(), s.context_key.clone()));
            }
        }
        // T2+T3：锁内 take child + clone writer + remove，收集到 cleaned；释放 sessions 锁后再 kill+wait。
        let mut cleaned: Vec<(
            String,
            String,
            Option<Box<dyn portable_pty::Child + Send>>,
            std::sync::Arc<std::sync::Mutex<Option<Box<dyn std::io::Write + Send>>>>,
        )> = Vec::new();
        for (id, ctx) in &to_remove {
            if let Some(s) = sessions.get_mut(id) {
                let child_opt = s.child.lock().ok().and_then(|mut g| g.take());
                let writer_arc = s.writer.clone();
                s.master.take();
                cleaned.push((id.clone(), ctx.clone(), child_opt, writer_arc));
            }
            sessions.remove(id);
        }
        drop(sessions);
        for (id, ctx, child_opt, writer_arc) in cleaned {
            if let Some(mut c) = child_opt {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Ok(mut w) = writer_arc.lock() {
                w.take();
            }
            let _ = closed_tx.send(TerminalClosedEvent {
                terminal_id: id, context_key: ctx, socket_ids: vec![],
            });
        }
    }

    /// 向某个终端的 PTY 写入输入。
    pub fn write_input(&self, socket_id: &str, context_key: &str, terminal_id: &str, data: &str)
        -> Result<(), AppError>
    {
        let context_key = normalize_context_key(context_key)?;
        let max_input = 1024 * 1024;
        if data.len() > max_input {
            return Err(bad_request(ErrorCode::TerminalInputTooLarge, "Terminal input is too large".to_string()));
        }
        let data_bytes = data.as_bytes().to_vec();

        // S2 修复：锁 sessions 仅做校验 + clone writer Arc，随即释放 sessions 锁，
        // 在锁外做 PTY 写入 —— 避免 write_all 阻塞时持有全局 sessions 锁导致所有终端卡死。
        let writer_clone = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions.get(terminal_id).filter(|s| s.status != TerminalStatus::Exited)
                .ok_or_else(|| not_found("terminal not found"))?;
            if s.context_key != context_key { return Err(context_mismatch()); }
            if !s.attached.contains(socket_id) { return Err(not_attached()); }
            if s.status != TerminalStatus::Running { return Err(exited()); }
            s.writer.clone()
        };
        let mut writer = writer_clone.lock().unwrap();
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
        let context_key = normalize_context_key(context_key)?;
        // T6：锁内仅更新 cols/rows + PTY master.resize + clone vt_terminal Arc + emit_metadata，
        // 释放 sessions 锁后再调 wezterm vt_terminal.resize（避免持全局 sessions 锁调第三方库，
        // 一旦 panic 会中毒 sessions 锁波及整个终端服务 —— 与 download/reconnect 一致）。
        let (vt_resize, meta) = {
            let mut sessions = self.sessions.lock().unwrap();
            let s = sessions.get_mut(terminal_id)
                .ok_or_else(|| not_found("terminal not found"))?;
            if s.context_key != context_key { return Err(context_mismatch()); }
            if !s.attached.contains(socket_id) { return Err(not_attached()); }
            let next_cols = cols.clamp(20, 300);
            let next_rows = rows.clamp(5, 120);
            if next_cols != s.cols || next_rows != s.rows {
                s.cols = next_cols;
                s.rows = next_rows;
                if s.status == TerminalStatus::Running {
                    if let Some(ref mut master) = s.master {
                        let _ = master.resize(PtySize { rows: next_rows, cols: next_cols, pixel_width: 0, pixel_height: 0 });
                    }
                }
                let vt = s.vt_terminal.clone();
                self.emit_metadata(s);
                (Some((vt, next_cols, next_rows)), Self::meta(s))
            } else {
                (None, Self::meta(s))
            }
        };
        if let Some((vt, next_cols, next_rows)) = vt_resize {
            vt.lock().unwrap().resize(TerminalSize {
                rows: next_rows as usize, cols: next_cols as usize,
                pixel_width: 0, pixel_height: 0, dpi: 0,
            });
        }
        Ok(meta)
    }

    /// 重命名终端标签（所有已附着客户端共享）。
    pub fn rename(&self, socket_id: &str, context_key: &str, terminal_id: &str, title: &str)
        -> Result<TerminalMetadata, AppError>
    {
        let context_key = normalize_context_key(context_key)?;
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(terminal_id)
            .ok_or_else(|| not_found("terminal not found"))?;
        if s.context_key != context_key { return Err(context_mismatch()); }
        if !s.attached.contains(socket_id) { return Err(not_attached()); }
        let trimmed = title.trim();
        // 对齐 TS normalizeTitle：空标题回落到 shell 名；非空则截断到 80 字符。
        const MAX_TITLE_LENGTH: usize = 80;
        s.title = if trimmed.is_empty() {
            s.shell.clone()
        } else {
            trimmed.chars().take(MAX_TITLE_LENGTH).collect()
        };
        self.emit_metadata(s);
        Ok(Self::meta(s))
    }

    /// 显式关闭某个终端。
    /// 调用者必须是已附着的 socket（对齐 TS getAttachedSession 校验）。
    pub fn close(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(), AppError>
    {
        let context_key = normalize_context_key(context_key)?;
        // T2+T3：锁内仅 take child + clone writer Arc + take master + remove，释放 sessions 锁后
        // 再 kill+wait child 与 take writer（避免与 write_input 持 writer 锁做 write_all 互锁，
        // 且 kill/wait 不持全局 sessions 锁、显式 wait 防僵尸）。
        let (child_opt, writer_arc, socket_ids) = {
            let mut sessions = self.sessions.lock().unwrap();
            let s = sessions.get_mut(terminal_id).ok_or_else(|| not_found("terminal not found"))?;
            if s.context_key != context_key { return Err(context_mismatch()); }
            if !s.attached.contains(socket_id) { return Err(not_attached()); }
            let socket_ids: Vec<String> = s.attached.iter().cloned().collect();
            s.status = TerminalStatus::Exited;
            let child_opt = s.child.lock().ok().and_then(|mut g| g.take());
            let writer_arc = s.writer.clone();
            s.master.take();
            s.attached.clear();
            sessions.remove(terminal_id);
            (child_opt, writer_arc, socket_ids)
        };
        if let Some(mut c) = child_opt {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(mut w) = writer_arc.lock() {
            w.take();
        }
        let _ = self.closed_tx.send(TerminalClosedEvent {
            terminal_id: terminal_id.into(), context_key: context_key.into(), socket_ids,
        });
        Ok(())
    }

    /// VT 屏幕的纯文本快照（用于下载 / 全部复制）。
    pub fn download(&self, socket_id: &str, context_key: &str, terminal_id: &str)
        -> Result<(String, String), AppError>
    {
        let context_key = normalize_context_key(context_key)?;
        // H3：锁内仅校验 + clone vt_terminal Arc + 取 title，释放 sessions 锁后再序列化
        // （serialize_terminal_plain 是 wezterm 重活；持全局 sessions 锁会阻塞所有终端，
        // 且 wezterm panic 会中毒全局 sessions 锁波及整个终端服务）。
        let (vt_clone, safe_title) = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions.get(terminal_id).ok_or_else(|| not_found("terminal not found"))?;
            if s.context_key != context_key { return Err(context_mismatch()); }
            if !s.attached.contains(socket_id) { return Err(not_attached()); }
            let safe_title = s.title.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect::<String>();
            (s.vt_terminal.clone(), safe_title)
        };
        let content = serialize_terminal_plain(&vt_clone.lock().unwrap());
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

impl Drop for TerminalService {
    fn drop(&mut self) {
        // 防御性回收：service 析构时 kill 所有子进程并清理资源
        // （单例运行时不触发；测试/service 重建场景下避免孤儿 PTY 子进程与悬挂 reader 任务）。
        // T2+T3：锁内遍历 take child + clone writer + take master + abort grace + clear，
        // 释放锁后再 kill+wait（不持全局锁、防僵尸）。
        let mut pending: Vec<(
            Option<Box<dyn portable_pty::Child + Send>>,
            std::sync::Arc<std::sync::Mutex<Option<Box<dyn std::io::Write + Send>>>>,
        )> = Vec::new();
        if let Ok(mut sessions) = self.sessions.lock() {
            for (_, s) in sessions.iter_mut() {
                let child_opt = s.child.lock().ok().and_then(|mut g| g.take());
                let writer_arc = s.writer.clone();
                s.master.take();
                if let Some(h) = s.grace_handle.take() {
                    h.abort();
                }
                pending.push((child_opt, writer_arc));
            }
            sessions.clear();
        }
        for (child_opt, writer_arc) in pending {
            if let Some(mut c) = child_opt {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Ok(mut w) = writer_arc.lock() {
                *w = None;
            }
        }
    }
}

// ── VT 屏幕序列化 ─────────────────────────────────────────────────

/// 规整 contextKey:trim 后必须为 `global` 或 `thread:<id>`(对齐 TS normalizeContextKey)。
fn normalize_context_key(raw: &str) -> Result<String, AppError> {
    let value = raw.trim();
    if value == "global" || (value.starts_with("thread:") && value.len() > "thread:".len()) {
        Ok(value.to_string())
    } else {
        Err(bad_request(
            ErrorCode::TerminalInvalidContext,
            "contextKey must be global or thread:<id>".to_string(),
        ))
    }
}

/// 单个 cell 的渲染属性快照，用于检测相邻 cell 间属性变化并输出增量 SGR。
/// ColorAttribute / Intensity / Underline 均为 Copy，可直接比较与复制。
#[derive(Clone, Copy, PartialEq, Eq)]
struct AttrState {
    foreground: ColorAttribute,
    background: ColorAttribute,
    intensity: Intensity,
    underline: Underline,
    italic: bool,
    reverse: bool,
}

impl AttrState {
    /// 默认属性状态（对应 SGR RESET `\x1b[0m` 之后的状态）。
    fn default_state() -> Self {
        Self {
            foreground: ColorAttribute::Default,
            background: ColorAttribute::Default,
            intensity: Intensity::Normal,
            underline: Underline::None,
            italic: false,
            reverse: false,
        }
    }

    /// 从 CellAttributes 提取当前 cell 的属性快照。
    fn from_attrs(attrs: &CellAttributes) -> Self {
        Self {
            foreground: attrs.foreground(),
            background: attrs.background(),
            intensity: attrs.intensity(),
            underline: attrs.underline(),
            italic: attrs.italic(),
            reverse: attrs.reverse(),
        }
    }

    /// 输出从默认状态到当前状态所需的完整 SGR 序列。
    /// 采用“基于默认的完整 SGR”而非精细 diff：实现简单，且 xterm.js 在每行
    /// RESET 之后能正确叠加这些设置序列。
    fn sgr_from_default(&self) -> String {
        let mut s = String::new();
        // 前景色
        match self.foreground {
            ColorAttribute::Default => s.push_str("\x1b[39m"),
            ColorAttribute::PaletteIndex(n) => {
                s.push_str(&format!("\x1b[38;5;{n}m"));
            }
            ColorAttribute::TrueColorWithDefaultFallback(c)
            | ColorAttribute::TrueColorWithPaletteFallback(c, _) => {
                // TrueColor 取 r/g/b，忽略 fallback 调色板索引。
                let (r, g, b, _) = c.as_rgba_u8();
                s.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
            }
        }
        // 背景色
        match self.background {
            ColorAttribute::Default => s.push_str("\x1b[49m"),
            ColorAttribute::PaletteIndex(n) => {
                s.push_str(&format!("\x1b[48;5;{n}m"));
            }
            ColorAttribute::TrueColorWithDefaultFallback(c)
            | ColorAttribute::TrueColorWithPaletteFallback(c, _) => {
                let (r, g, b, _) = c.as_rgba_u8();
                s.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
            }
        }
        // 粗细 / 亮度
        match self.intensity {
            Intensity::Bold => s.push_str("\x1b[1m"),
            Intensity::Half => s.push_str("\x1b[2m"),
            Intensity::Normal => s.push_str("\x1b[22m"),
        }
        // 斜体
        s.push_str(if self.italic { "\x1b[3m" } else { "\x1b[23m" });
        // 下划线：Double 用 21，其它非 None（Curly/Dotted/Dashed）统一按单下划线 4 输出
        match self.underline {
            Underline::None => s.push_str("\x1b[24m"),
            Underline::Double => s.push_str("\x1b[21m"),
            _ => s.push_str("\x1b[4m"),
        }
        // 反显
        s.push_str(if self.reverse { "\x1b[7m" } else { "\x1b[27m" });
        s
    }
}

/// 将 wezterm 终端的屏幕（回滚 + 可见行）序列化为带 SGR 颜色与样式的 VT 序列字符串。
/// 重连时 xterm.js 通过 `term.write(state)` 即可恢复彩色画面，而非丢失颜色的纯文本。
/// 使用 scrollback_rows() + lines_in_phys_range()（原始 wezterm-term 中非测试的公开 API）
/// 配合 Line::visible_cells() 逐 cell 遍历，按属性变化输出增量 SGR。
fn serialize_terminal_screen(term: &Terminal) -> String {
    let screen = term.screen();
    let total = screen.scrollback_rows();
    let lines = screen.lines_in_phys_range(0..total);
    let mut out = String::new();
    for line in &lines {
        // 每行开头 RESET，确保后续 SGR 基于默认状态叠加。
        out.push_str("\x1b[0m");
        let mut prev = AttrState::default_state();
        for cell in line.visible_cells() {
            let cur = AttrState::from_attrs(cell.attrs());
            // 仅在属性变化时输出该 cell 的完整 SGR，减少冗余序列。
            if cur != prev {
                out.push_str(&cur.sgr_from_default());
                prev = cur;
            }
            out.push_str(cell.str());
        }
        // 行尾 RESET + CRLF：\r\n 确保光标回到行首再换行，避免 xterm.js 画面错位。
        out.push_str("\x1b[0m\r\n");
    }
    out
}

/// VT 屏幕的纯文本快照（不含 SGR 转义），用于下载 / 全部复制为 .txt。
fn serialize_terminal_plain(term: &Terminal) -> String {
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
    // 对齐 TS resolveShell：优先 SHELL 环境变量，否则按平台回落
    // （Windows: powershell.exe；macOS: /bin/zsh；Linux: /bin/bash）。
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() {
            return shell;
        }
    }
    if cfg!(target_os = "windows") {
        "powershell.exe".to_string()
    } else if cfg!(target_os = "macos") {
        "/bin/zsh".to_string()
    } else {
        "/bin/bash".to_string()
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

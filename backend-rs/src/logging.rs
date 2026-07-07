//! tracing 初始化（按大小滚动文件 + 标准输出）以及 URL 脱敏。
//!
//! 两个 layer：
//! - **stdout**：人类可读的格式（适合开发控制台 / `docker logs`）
//! - **file**（`logs/app`，按大小滚动 10MB × 5 个）：**JSON** 格式，以便 `logs`
//!   模块能将条目解析为结构化的 `LogEntry` 记录。
//!
//! 对齐 pino-roll：按大小滚动（10MB × 5 个文件），由自建 `RollingWriter` 实现
//! （tracing-appender 仅支持按时间滚动）。参见 spec §6.7。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::{self, JsonFields};
use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*};

/// 初始化 tracing：stdout（人类可读）+ 滚动文件（JSON）。
/// 返回 `WorkerGuard` —— **必须持有**到进程退出，否则非阻塞写入器的后台线程
/// 会被丢弃，尚未写出的日志行会丢失。
pub fn init(level: &str) -> WorkerGuard {
    let file_appender = RollingWriter::new(PathBuf::from("logs").join("app"), 10 * 1024 * 1024, 5);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        // stdout：人类可读，带 ANSI 颜色
        .with(fmt::layer().with_writer(std::io::stdout))
        // file：JSON 格式，供 logs 模块做结构化解析
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .fmt_fields(JsonFields::default())
                .event_format(format::Format::default().json()),
        )
        .init();

    guard
}

/// 按大小滚动的日志 writer（对齐 spec §6.7：10MB × 5 个文件 / logs/app）。
/// 写入 `base`；累计字节超过 `max_size` 时滚动：base→base.1→…→base.(max_files-1)，
/// 最老的删除。tracing 的 non_blocking 仅由单后台线程写入，故无需额外同步。
struct RollingWriter {
    base: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
    max_size: u64,
    max_files: usize,
}

impl RollingWriter {
    fn new(base: PathBuf, max_size: u64, max_files: usize) -> Self {
        let _ = std::fs::create_dir_all(base.parent().unwrap_or(std::path::Path::new(".")));
        let written = std::fs::metadata(&base).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)
            .ok();
        Self { base, file, written, max_size, max_files }
    }

    /// 滚动一次：删除 base.(n-1)，base.i → base.(i+1)，base → base.1，再新建 base。
    fn rotate(&mut self) {
        if let Some(f) = self.file.take() {
            drop(f);
        }
        if self.max_files >= 2 {
            let _ = std::fs::remove_file(rotated_path(&self.base, self.max_files - 1));
            for i in (1..self.max_files - 1).rev() {
                let _ = std::fs::rename(
                    rotated_path(&self.base, i),
                    rotated_path(&self.base, i + 1),
                );
            }
            let _ = std::fs::rename(&self.base, rotated_path(&self.base, 1));
        } else {
            let _ = std::fs::remove_file(&self.base);
        }
        self.written = 0;
        self.file = OpenOptions::new().create(true).append(true).open(&self.base).ok();
    }
}

fn rotated_path(base: &std::path::Path, n: usize) -> PathBuf {
    let mut s = base.to_string_lossy().into_owned();
    s.push('.');
    s.push_str(&n.to_string());
    PathBuf::from(s)
}

impl Write for RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.max_size {
            self.rotate();
        }
        match self.file.as_mut() {
            Some(f) => {
                let n = f.write(buf)?;
                self.written += n as u64;
                Ok(n)
            }
            None => Err(io::Error::new(io::ErrorKind::Other, "log file not open")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// 将 URL 中的 `access_token=...` 查询参数值替换为 `[Redacted]`。
/// 对齐 `app.module.ts:sanitizeUrl`（保留参数名 + 脱敏标记，与"原本没有该参数"区分）。
pub fn sanitize_url(url: &str) -> String {
    let (base, query) = match url.find('?') {
        Some(i) => (&url[..i], Some(&url[i + 1..])),
        None => (url, None),
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(base);
    if let Some(q) = query {
        out.push('?');
        for (i, part) in q.split('&').enumerate() {
            if i > 0 {
                out.push('&');
            }
            if part.starts_with("access_token=") {
                out.push_str("access_token=[Redacted]");
            } else {
                out.push_str(part);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_access_token_at_start() {
        assert_eq!(
            sanitize_url("/api/files/serve?access_token=abc&x=1"),
            "/api/files/serve?access_token=[Redacted]&x=1",
        );
    }

    #[test]
    fn redacts_access_token_in_middle() {
        assert_eq!(
            sanitize_url("/api/files/serve?x=1&access_token=abc&y=2"),
            "/api/files/serve?x=1&access_token=[Redacted]&y=2",
        );
    }

    #[test]
    fn redacts_access_token_when_only_param() {
        assert_eq!(
            sanitize_url("/api/files/serve?access_token=abc"),
            "/api/files/serve?access_token=[Redacted]",
        );
    }

    #[test]
    fn keeps_url_without_token() {
        assert_eq!(sanitize_url("/api/health"), "/api/health");
    }

    #[test]
    fn keeps_url_with_non_token_params() {
        assert_eq!(
            sanitize_url("/api/files?path=/ws"),
            "/api/files?path=/ws",
        );
    }
}

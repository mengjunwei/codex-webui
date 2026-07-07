//! tracing 初始化（按天滚动文件 + 标准输出）以及 URL 脱敏。
//!
//! 两个 layer：
//! - **stdout**：人类可读的格式（适合开发控制台 / `docker logs`）
//! - **file**（`logs/app.YYYY-MM-DD`）：**JSON** 格式，以便 `logs` 模块
//!   能将条目解析为结构化的 `LogEntry` 记录。
//!
//! 对齐说明：pino-roll 采用按大小滚动（10MB × 5 个文件）；
//! tracing-appender 仅支持按时间滚动（每天）。Phase 0 采用按天滚动；
//! 按大小滚动暂缓（使用 logrotate 或自定义 appender）。参见 spec §6.7。

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::fmt::format::{self, JsonFields};
use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*};

/// 初始化 tracing：stdout（人类可读）+ 滚动文件（JSON）。
/// 返回 `WorkerGuard` —— **必须持有**到进程退出，否则非阻塞写入器的后台线程
/// 会被丢弃，尚未写出的日志行会丢失。
pub fn init(level: &str) -> WorkerGuard {
    let file_appender = rolling::daily("logs", "app");
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

/// 从 URL 中剔除 `access_token=...` 查询参数。
/// 与 `app.module.ts:sanitizeUrl` 对齐（PINO_REDACT 会剔除 `req.query.access_token`）。
pub fn sanitize_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut first_param = true;

    let (base, query) = match url.find('?') {
        Some(i) => (&url[..i], Some(&url[i + 1..])),
        None => (url, None),
    };

    out.push_str(base);

    if let Some(q) = query {
        for part in q.split('&') {
            if part.starts_with("access_token=") {
                continue; // 脱敏剔除
            }
            if first_param {
                out.push('?');
                first_param = false;
            } else {
                out.push('&');
            }
            out.push_str(part);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_access_token_at_start() {
        assert_eq!(
            sanitize_url("/api/files/serve?access_token=abc&x=1"),
            "/api/files/serve?x=1",
        );
    }

    #[test]
    fn strips_access_token_in_middle() {
        assert_eq!(
            sanitize_url("/api/files/serve?x=1&access_token=abc&y=2"),
            "/api/files/serve?x=1&y=2",
        );
    }

    #[test]
    fn strips_access_token_when_only_param() {
        assert_eq!(
            sanitize_url("/api/files/serve?access_token=abc"),
            "/api/files/serve",
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

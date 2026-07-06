//! Tracing initialization (daily rolling file + stdout) and URL redaction.
//!
//! Parity note: pino-roll does size-based rotation (10MB × 5 files);
//! tracing-appender only supports time-based (daily). Phase 0 uses daily;
//! size-based rotation deferred (logrotate or custom appender). See spec §6.7.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{filter::EnvFilter, fmt, prelude::*};

/// Initialize tracing: stdout + rolling file (logs/app, daily).
/// Returns `WorkerGuard` — **must be held** until process exit, or the
/// non-blocking writer's background thread drops and pending log lines are lost.
pub fn init(level: &str) -> WorkerGuard {
    let file_appender = rolling::daily("logs", "app");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stdout))
        .with(fmt::layer().with_writer(non_blocking))
        .init();

    guard
}

/// Strip `access_token=...` query parameters from a URL.
/// Parity with `app.module.ts:sanitizeUrl` (PINO_REDACT strips `req.query.access_token`).
pub fn sanitize_url(url: &str) -> String {
    // Strip token from both ?access_token=X and &access_token=X positions.
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
                continue; // redact
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

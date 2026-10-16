//! Tracing subscriber initialization for the Carrel backend (F-2-3).
//!
//! The library crate (`carrel-core`) only emits events/spans; this module —
//! living in the binary — owns the single global subscriber. The
//! `tracing-log` bridge (enabled via the `tracing-subscriber` feature) routes
//! existing `log::` records into the same subscriber, so no `log::` call site
//! needs rewriting.

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Build the env-filter directive string. Pure and unit-testable.
/// Falls back to `info` when the env value is absent or blank.
pub fn resolve_filter(env: Option<String>) -> String {
    match env {
        Some(s) if !s.trim().is_empty() => s,
        _ => "info".to_string(),
    }
}

/// Daily log files kept on disk; the appender prunes the oldest on startup and
/// at each rollover, touching only files whose name starts with `carrel.log`.
const MAX_LOG_FILES: usize = 14;

fn build_log_appender(
    dir: &std::path::Path,
) -> Result<tracing_appender::rolling::RollingFileAppender, tracing_appender::rolling::InitError> {
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("carrel.log")
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
}

/// Initialize the global tracing subscriber.
///
/// - Dev (`cfg!(debug_assertions)`): human-readable `fmt` layer to stderr;
///   returns `None` (no flush worker needed).
/// - Prod: non-blocking daily-rolling file at `{log_dir}/carrel.log.<date>`;
///   returns the `WorkerGuard`, which the caller MUST keep alive for the
///   lifetime of the app so buffered records flush.
///
/// Uses `try_init()` so a duplicate initialization never panics — the first
/// caller wins the global default and later calls are no-ops.
pub fn init_tracing(log_dir: Option<PathBuf>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_new(resolve_filter(std::env::var("CARREL_LOG").ok()))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // A log dir we cannot open (read-only, bad permissions) falls back to
    // stderr rather than aborting startup, like the ignored create_dir_all.
    let file_appender = match (cfg!(debug_assertions), log_dir) {
        (false, Some(dir)) => {
            let _ = std::fs::create_dir_all(&dir);
            build_log_appender(&dir)
                .map_err(|e| eprintln!("carrel: cannot open log file in {}: {e}", dir.display()))
                .ok()
        }
        _ => None,
    };

    match file_appender {
        Some(appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_ansi(false).with_writer(non_blocking))
                .try_init();
            Some(guard)
        }
        None => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr))
                .try_init();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_filter_defaults_to_info() {
        assert_eq!(resolve_filter(None), "info");
        assert_eq!(resolve_filter(Some(String::new())), "info");
        assert_eq!(resolve_filter(Some("   ".to_string())), "info");
    }

    #[test]
    fn resolve_filter_honors_env() {
        assert_eq!(resolve_filter(Some("debug".to_string())), "debug");
        assert_eq!(
            resolve_filter(Some("carrel_core=debug,info".to_string())),
            "carrel_core=debug,info"
        );
    }

    #[test]
    fn log_appender_keeps_only_the_14_newest_daily_files() {
        let dir = tempfile::tempdir().unwrap();
        // 20 old daily files, created oldest-first (the appender orders by
        // creation time), plus a file that is not ours.
        for day in 1..=20 {
            std::fs::write(dir.path().join(format!("carrel.log.2026-01-{day:02}")), "x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::fs::write(dir.path().join("other.txt"), "keep me").unwrap();

        let _appender = build_log_appender(dir.path()).unwrap();

        let mut logs: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("carrel.log."))
            .collect();
        logs.sort();
        // 13 survivors + today's freshly opened file = 14.
        assert_eq!(logs.len(), 14, "{logs:?}");
        assert!(!logs.contains(&"carrel.log.2026-01-01".to_string()));
        assert!(!logs.contains(&"carrel.log.2026-01-07".to_string()));
        assert!(logs.contains(&"carrel.log.2026-01-08".to_string()));
        assert!(logs.contains(&"carrel.log.2026-01-20".to_string()));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("other.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn init_tracing_does_not_panic_and_is_idempotent() {
        // First call may win the global default; a second call must be a
        // harmless no-op. Neither may panic. (Tests run in debug, so this
        // exercises the stderr branch and returns None.)
        let first = init_tracing(None);
        let second = init_tracing(None);
        assert!(first.is_none());
        assert!(second.is_none());
    }
}

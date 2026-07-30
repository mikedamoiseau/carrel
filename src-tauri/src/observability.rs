//! Tracing subscriber initialization for the Carrel backend (F-2-3).
//!
//! The library crate (`folio-core`) only emits events/spans; this module —
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

/// Initialize the global tracing subscriber.
///
/// - Dev (`cfg!(debug_assertions)`): human-readable `fmt` layer to stderr;
///   returns `None` (no flush worker needed).
/// - Prod: non-blocking daily-rolling file at `{log_dir}/folio.log.<date>`;
///   returns the `WorkerGuard`, which the caller MUST keep alive for the
///   lifetime of the app so buffered records flush.
///
/// Uses `try_init()` so a duplicate initialization never panics — the first
/// caller wins the global default and later calls are no-ops.
pub fn init_tracing(log_dir: Option<PathBuf>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_new(resolve_filter(std::env::var("FOLIO_LOG").ok()))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    match (cfg!(debug_assertions), log_dir) {
        (false, Some(dir)) => {
            let _ = std::fs::create_dir_all(&dir);
            let appender = tracing_appender::rolling::daily(&dir, "folio.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_ansi(false).with_writer(non_blocking))
                .try_init();
            Some(guard)
        }
        _ => {
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
            resolve_filter(Some("folio_core=debug,info".to_string())),
            "folio_core=debug,info"
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

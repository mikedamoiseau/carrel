//! Shared retry/backoff helper for enrichment provider HTTP calls (#F-2-7).
//!
//! All enrichment traffic (the `providers/` modules and the legacy
//! `openlibrary` module) funnels through [`send_with_retry`], which retries
//! transport errors, HTTP 429, and HTTP 5xx with exponential backoff,
//! honoring `Retry-After` (seconds form). Non-retryable statuses (404, 400,
//! 401, …) are returned to the caller untouched so existing status handling
//! keeps working. An optional process-wide observer receives a
//! [`RetryEvent`] before each retry so the UI layer can surface feedback.

use crate::error::{CarrelError, CarrelResult};
use std::sync::OnceLock;
use std::time::Duration;

/// Retry behavior knobs. One default policy is shared by all call sites.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles each retry.
    pub base_delay: Duration,
    /// Upper bound for any computed or server-requested delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// A status worth retrying: rate limit or server-side failure. Everything
/// else (404, 400, 401, …) must reach the caller untouched.
pub fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// `Retry-After` in seconds form. The HTTP-date form is rare on JSON APIs
/// and deliberately treated as absent.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Delay before the next attempt. `attempt` is the 1-based attempt that just
/// failed. A server-provided `Retry-After` wins over the computed backoff;
/// both are clamped to `policy.max_delay`.
pub fn retry_delay(attempt: u32, retry_after: Option<Duration>, policy: &RetryPolicy) -> Duration {
    if let Some(ra) = retry_after {
        return ra.min(policy.max_delay);
    }
    let exp = attempt.saturating_sub(1).min(16);
    policy
        .base_delay
        .saturating_mul(2u32.saturating_pow(exp))
        .min(policy.max_delay)
}

/// Emitted to the registered observer immediately before each retry sleep.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryEvent {
    /// Provider tag passed to [`send_with_retry`] (e.g. `"openlibrary"`).
    pub provider: String,
    /// The attempt about to be made (2..=max_attempts).
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
}

type ObserverFn = Box<dyn Fn(&RetryEvent) + Send + Sync>;

static OBSERVER: OnceLock<ObserverFn> = OnceLock::new();

/// Register a process-wide retry observer. Write-once: later calls are
/// silently ignored. When unset (tests, headless), retries are log-only.
pub fn set_retry_observer(f: Box<dyn Fn(&RetryEvent) + Send + Sync>) {
    let _ = OBSERVER.set(f);
}

fn notify_observer(ev: &RetryEvent) {
    if let Some(f) = OBSERVER.get() {
        f(ev);
    }
}

/// Send `req`, retrying transport errors, 429, and 5xx per `policy`.
/// Non-retryable statuses are returned as `Ok(resp)` untouched. After
/// exhausting attempts: [`CarrelError::RateLimited`] if the last failure was
/// a 429, [`CarrelError::Network`] otherwise.
pub fn send_with_retry(
    req: reqwest::blocking::RequestBuilder,
    provider: &str,
    policy: &RetryPolicy,
) -> CarrelResult<reqwest::blocking::Response> {
    let mut last_was_rate_limit = false;
    let mut last_reason = String::new();

    for attempt in 1..=policy.max_attempts {
        let cloned = req.try_clone().ok_or_else(|| {
            CarrelError::network(format!("{provider}: request cannot be retried"))
        })?;

        let retry_after = match cloned.send() {
            Ok(resp) if !is_retryable(resp.status()) => return Ok(resp),
            Ok(resp) => {
                last_was_rate_limit = resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS;
                last_reason = format!("HTTP {}", resp.status());
                parse_retry_after(resp.headers())
            }
            Err(e) => {
                last_was_rate_limit = false;
                last_reason = e.to_string();
                None
            }
        };

        if attempt == policy.max_attempts {
            break;
        }

        let delay = retry_delay(attempt, retry_after, policy);
        tracing::warn!(
            provider,
            attempt,
            max_attempts = policy.max_attempts,
            delay_ms = delay.as_millis() as u64,
            reason = %last_reason,
            "retrying provider request"
        );
        notify_observer(&RetryEvent {
            provider: provider.to_string(),
            attempt: attempt + 1,
            max_attempts: policy.max_attempts,
            delay_ms: delay.as_millis() as u64,
        });
        std::thread::sleep(delay);
    }

    if last_was_rate_limit {
        Err(CarrelError::RateLimited(format!(
            "{provider}: rate limited after {} attempts",
            policy.max_attempts
        )))
    } else {
        Err(CarrelError::network(format!(
            "{provider}: request failed after {} attempts: {last_reason}",
            policy.max_attempts
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use reqwest::StatusCode;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal scripted HTTP server: serves each response string to one
    /// connection, in order, then exits. `Connection: close` in the
    /// responses forces reqwest to reconnect per attempt.
    fn spawn_stub(responses: Vec<&'static str>) -> (String, std::thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for resp in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                stream.write_all(resp.as_bytes()).unwrap();
                served += 1;
            }
            served
        });
        (format!("http://{}", addr), handle)
    }

    /// Fast policy so retry tests don't sleep for real.
    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    const TOO_MANY: &str =
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const NOT_FOUND_RESP: &str =
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    #[test]
    fn succeeds_after_429_then_200() {
        let (url, server) = spawn_stub(vec![TOO_MANY, OK]);
        let client = reqwest::blocking::Client::new();
        let resp = send_with_retry(client.get(&url), "test", &fast_policy()).unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(server.join().unwrap(), 2);
    }

    #[test]
    fn persistent_429_yields_rate_limited_after_max_attempts() {
        let (url, server) = spawn_stub(vec![TOO_MANY, TOO_MANY, TOO_MANY]);
        let client = reqwest::blocking::Client::new();
        let err = send_with_retry(client.get(&url), "test", &fast_policy()).unwrap_err();
        assert_eq!(err.kind(), "RateLimited");
        assert_eq!(server.join().unwrap(), 3);
    }

    #[test]
    fn non_retryable_status_returns_immediately() {
        let (url, server) = spawn_stub(vec![NOT_FOUND_RESP]);
        let client = reqwest::blocking::Client::new();
        let resp = send_with_retry(client.get(&url), "test", &fast_policy()).unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn transport_error_retried_then_network_error() {
        // Bind then immediately drop the listener: connection refused.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{}", port);
        let client = reqwest::blocking::Client::new();
        let err = send_with_retry(client.get(&url), "test", &fast_policy()).unwrap_err();
        assert_eq!(err.kind(), "Network");
    }

    #[test]
    fn observer_receives_retry_events() {
        // Sole owner of the process-wide observer — no other test may set it.
        // Filter to the "openlibrary" provider so parallel tests using
        // provider "test" don't pollute the event list.
        static EVENTS: std::sync::Mutex<Vec<RetryEvent>> = std::sync::Mutex::new(Vec::new());
        set_retry_observer(Box::new(|ev| {
            if ev.provider == "openlibrary" {
                EVENTS.lock().unwrap().push(ev.clone());
            }
        }));

        let (url, _server) = spawn_stub(vec![TOO_MANY, OK]);
        let client = reqwest::blocking::Client::new();
        send_with_retry(client.get(&url), "openlibrary", &fast_policy()).unwrap();

        let events = EVENTS.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "openlibrary");
        assert_eq!(events[0].attempt, 2);
        assert_eq!(events[0].max_attempts, 3);
    }

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable(StatusCode::OK));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn delay_grows_exponentially_and_caps() {
        let p = policy();
        assert_eq!(retry_delay(1, None, &p), Duration::from_millis(500));
        assert_eq!(retry_delay(2, None, &p), Duration::from_secs(1));
        assert_eq!(retry_delay(3, None, &p), Duration::from_secs(2));
        // Far past the cap
        assert_eq!(retry_delay(10, None, &p), Duration::from_secs(8));
    }

    #[test]
    fn retry_after_overrides_and_clamps() {
        let p = policy();
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(3)), &p),
            Duration::from_secs(3)
        );
        // Server asks for more than the cap — clamp
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(120)), &p),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));
    }

    #[test]
    fn parse_retry_after_absent_or_malformed() {
        let h = HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);

        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&h), None);

        // HTTP-date form is deliberately ignored (treated as absent)
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);
    }
}

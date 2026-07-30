use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, USER_AGENT};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Static full-changelog target (never derived from GitHub data).
pub const CHANGELOG_URL: &str = "https://github.com/mikedamoiseau/carrel/releases";

const RELEASE_TAG_PATH_PREFIX: &str = "/mikedamoiseau/carrel/releases/tag/";

/// Minimal DTO. `body` is `Option` — GitHub release bodies can be null/absent;
/// deserializing into `String` would error a valid release.
#[derive(Debug, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub html_url: String,
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateCheck {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub changelog_url: String,
    pub release_notes: String,
}

/// Pure seam: map a fetched release + installed version to `UpdateCheck`.
/// No network I/O. On failure returns the stable code `malformed_response`;
/// the underlying detail is logged, never returned to the frontend.
pub fn map_release(release: &GitHubRelease, current: &Version) -> Result<UpdateCheck, String> {
    let tag = release.tag_name.trim();
    let latest_str = tag.strip_prefix('v').unwrap_or(tag);
    let latest = Version::parse(latest_str).map_err(|e| {
        tracing::debug!(tag = %release.tag_name, error = %e, "update check: unparseable release tag");
        "malformed_response".to_string()
    })?;

    validate_release_url(&release.html_url)?;

    Ok(UpdateCheck {
        update_available: latest > *current,
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        release_url: release.html_url.clone(),
        changelog_url: CHANGELOG_URL.to_string(),
        release_notes: release.body.clone().unwrap_or_default(),
    })
}

/// Authoritative download-URL validation via a parsed URL — states the security
/// contract directly (scheme + exact host + path prefix) and avoids literal
/// string surprises (userinfo spoofing, case, normalization).
fn validate_release_url(raw: &str) -> Result<(), String> {
    let reject = || {
        tracing::warn!(url = raw, "update check: unexpected release url");
        "malformed_response".to_string()
    };
    let parsed = url::Url::parse(raw).map_err(|_| reject())?;
    if parsed.scheme() == "https"
        && parsed.host_str() == Some("github.com")
        && parsed.path().starts_with(RELEASE_TAG_PATH_PREFIX)
    {
        Ok(())
    } else {
        Err(reject())
    }
}

// The GitHub repo itself was renamed `mikedamoiseau/folio` -> `mikedamoiseau/carrel`
// (2026-07-30), so this slug tracks the real repo name now. It's load-bearing
// in more than this URL: it is also what `isTrustedReleaseUrl` (frontend) and
// `RELEASE_TAG_PATH_PREFIX` below allow-list — keep all three in sync.
const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/mikedamoiseau/carrel/releases/latest";

/// Shared client + endpoint + tunable TTL/timeout + single-flight/cache state.
/// The mutable part lives behind an `Arc` so the detached fetch task can own it.
pub struct UpdateCheckState {
    client: reqwest::Client,
    releases_url: String,
    ttl: Duration,
    timeout: Duration,
    inner: Arc<tokio::sync::Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    cache: Option<(Instant, UpdateCheck)>,
    in_flight: Option<broadcast::Sender<Result<UpdateCheck, String>>>,
}

impl UpdateCheckState {
    pub fn new() -> Self {
        Self::with_config(
            RELEASES_LATEST_URL.to_string(),
            Duration::from_secs(300),
            Duration::from_secs(15),
        )
    }

    fn with_config(releases_url: String, ttl: Duration, timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            releases_url,
            ttl,
            timeout,
            inner: Arc::new(tokio::sync::Mutex::new(Inner::default())),
        }
    }
}

impl Default for UpdateCheckState {
    fn default() -> Self {
        Self::new()
    }
}

/// Concurrency-safe check: serve fresh cache, else join the in-flight request,
/// else start one. A **supervisor task** owns the flight: it runs the fetch in
/// an inner worker task and awaits its `JoinHandle`, so even if the worker
/// PANICS the supervisor converts the `JoinError` to a stable `Err`, ALWAYS
/// clears `in_flight`, caches on success only, and broadcasts. Neither caller
/// cancellation nor worker panic can leave followers waiting forever. The brief
/// lock is never held across the network `await`.
pub async fn check(state: &UpdateCheckState, current: &Version) -> Result<UpdateCheck, String> {
    let mut rx = {
        let mut inner = state.inner.lock().await;
        if let Some((at, cached)) = &inner.cache {
            if at.elapsed() < state.ttl {
                return Ok(cached.clone());
            }
        }
        match &inner.in_flight {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = broadcast::channel(1);
                inner.in_flight = Some(tx.clone());
                drop(inner);

                let inner = state.inner.clone();
                let client = state.client.clone();
                let url = state.releases_url.clone();
                let timeout = state.timeout;
                let cur = current.clone();
                // Supervisor: awaits an isolated worker so a worker panic becomes
                // a JoinError here (never a lost flight), then always cleans up.
                tokio::spawn(async move {
                    let worker =
                        tokio::spawn(async move { do_fetch(&client, &url, timeout, &cur).await });
                    let result = match worker.await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!(error = %e, "update-check worker task failed");
                            Err("network".to_string())
                        }
                    };
                    {
                        let mut g = inner.lock().await;
                        g.in_flight = None;
                        if let Ok(uc) = &result {
                            g.cache = Some((Instant::now(), uc.clone()));
                        }
                    }
                    let _ = tx.send(result); // followers (if any) receive it
                });
                rx // initiator joins as a follower too
            }
        }
    };

    match rx.recv().await {
        Ok(result) => result,
        Err(_) => Err("network".to_string()),
    }
}

async fn do_fetch(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    current: &Version,
) -> Result<UpdateCheck, String> {
    // Test-only seam to exercise the supervisor's panic handling.
    #[cfg(test)]
    if url == "panic://boom" {
        panic!("test-only update-check worker panic");
    }
    let release = fetch_latest(client, url, timeout, current).await?;
    map_release(&release, current)
}

/// Map a reqwest transport/decode error to a stable IPC error code.
fn classify_reqwest_err(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_decode() {
        "malformed_response"
    } else {
        "network"
    }
}

async fn fetch_latest(
    client: &reqwest::Client,
    releases_url: &str,
    timeout: Duration,
    current: &Version,
) -> Result<GitHubRelease, String> {
    let user_agent = format!("Carrel/{current} (+https://github.com/mikedamoiseau/carrel)");
    let resp = client
        .get(releases_url)
        .header(USER_AGENT, user_agent)
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| {
            let code = classify_reqwest_err(&e);
            tracing::warn!(error = %e, code, "update check: request failed");
            code.to_string()
        })?;

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let remaining_zero = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false);
        let has_retry_after = resp.headers().contains_key("retry-after");
        if code == 429 || (code == 403 && (remaining_zero || has_retry_after)) {
            tracing::warn!(code, "update check: rate limited");
            return Err("rate_limited".to_string());
        }
        tracing::warn!(code, "update check: http error");
        return Err("http_error".to_string());
    }

    resp.json::<GitHubRelease>().await.map_err(|e| {
        let code = classify_reqwest_err(&e);
        tracing::warn!(error = %e, code, "update check: failed reading response body");
        code.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;

    fn rel(tag: &str, body: Option<&str>) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_string(),
            html_url: "https://github.com/mikedamoiseau/carrel/releases/tag/v2.8.0".to_string(),
            body: body.map(|b| b.to_string()),
        }
    }

    #[test]
    fn newer_patch_is_update() {
        let out = map_release(
            &rel("v2.8.0", Some("notes")),
            &Version::parse("2.7.0").unwrap(),
        )
        .unwrap();
        assert!(out.update_available);
        assert_eq!(out.latest_version, "2.8.0");
        assert_eq!(out.current_version, "2.7.0");
        assert_eq!(out.release_notes, "notes");
        assert_eq!(out.changelog_url, CHANGELOG_URL);
    }

    #[test]
    fn equal_version_is_not_update() {
        assert!(
            !map_release(&rel("v2.7.0", None), &Version::parse("2.7.0").unwrap())
                .unwrap()
                .update_available
        );
    }

    #[test]
    fn older_latest_is_not_update() {
        assert!(
            !map_release(&rel("2.6.0", None), &Version::parse("2.7.0").unwrap())
                .unwrap()
                .update_available
        );
    }

    #[test]
    fn tag_without_v_prefix_parses() {
        let out = map_release(&rel("2.9.0", None), &Version::parse("2.7.0").unwrap()).unwrap();
        assert_eq!(out.latest_version, "2.9.0");
        assert!(out.update_available);
    }

    #[test]
    fn surrounding_whitespace_trimmed() {
        assert_eq!(
            map_release(&rel("  v2.8.0 ", None), &Version::parse("2.7.0").unwrap())
                .unwrap()
                .latest_version,
            "2.8.0"
        );
    }

    #[test]
    fn uppercase_v_is_malformed() {
        assert_eq!(
            map_release(&rel("V2.8.0", None), &Version::parse("2.7.0").unwrap()).unwrap_err(),
            "malformed_response"
        );
    }

    #[test]
    fn null_body_becomes_empty_notes() {
        assert_eq!(
            map_release(&rel("v2.8.0", None), &Version::parse("2.7.0").unwrap())
                .unwrap()
                .release_notes,
            ""
        );
    }

    #[test]
    fn installed_prerelease_beats_stable_latest() {
        assert!(
            !map_release(
                &rel("v2.7.0", None),
                &Version::parse("2.8.0-beta.1").unwrap()
            )
            .unwrap()
            .update_available
        );
    }

    #[test]
    fn malformed_tag_is_malformed() {
        assert_eq!(
            map_release(
                &rel("not-a-version", None),
                &Version::parse("2.7.0").unwrap()
            )
            .unwrap_err(),
            "malformed_response"
        );
    }

    #[test]
    fn deceptive_host_rejected() {
        let mut r = rel("v2.8.0", None);
        r.html_url =
            "https://github.com.example.org/mikedamoiseau/carrel/releases/tag/v2.8.0".into();
        assert_eq!(
            map_release(&r, &Version::parse("2.7.0").unwrap()).unwrap_err(),
            "malformed_response"
        );
    }

    #[test]
    fn http_scheme_rejected() {
        let mut r = rel("v2.8.0", None);
        r.html_url = "http://github.com/mikedamoiseau/carrel/releases/tag/v2.8.0".into();
        assert_eq!(
            map_release(&r, &Version::parse("2.7.0").unwrap()).unwrap_err(),
            "malformed_response"
        );
    }

    #[test]
    fn wrong_path_prefix_rejected() {
        let mut r = rel("v2.8.0", None);
        r.html_url = "https://github.com/mikedamoiseau/carrel/issues/1".into();
        assert_eq!(
            map_release(&r, &Version::parse("2.7.0").unwrap()).unwrap_err(),
            "malformed_response"
        );
    }

    #[test]
    fn userinfo_spoof_rejected() {
        let mut r = rel("v2.8.0", None);
        r.html_url = "https://github.com@evil.com/mikedamoiseau/carrel/releases/tag/v2.8.0".into();
        assert_eq!(
            map_release(&r, &Version::parse("2.7.0").unwrap()).unwrap_err(),
            "malformed_response"
        );
    }

    use std::time::Duration;
    use wiremock::matchers::{header, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body_json(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "html_url": "https://github.com/mikedamoiseau/carrel/releases/tag/v2.8.0",
            "body": "release notes"
        })
    }

    fn state_for(url: String) -> UpdateCheckState {
        UpdateCheckState::with_config(url, Duration::from_secs(300), Duration::from_secs(15))
    }

    #[tokio::test]
    async fn ok_maps_and_sends_required_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .and(header("accept", "application/vnd.github+json"))
            .and(header("x-github-api-version", "2022-11-28"))
            .and(header_regex("user-agent", "^Carrel/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_json("v2.8.0")))
            .expect(1)
            .mount(&server)
            .await;
        let out = check(
            &state_for(format!("{}/x", server.uri())),
            &Version::parse("2.7.0").unwrap(),
        )
        .await
        .unwrap();
        assert!(out.update_available);
        assert_eq!(out.latest_version, "2.8.0");
    }

    #[tokio::test]
    async fn http_429_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn http_403_with_zero_remaining_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn http_403_without_zero_remaining_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "57"))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "http_error"
        );
    }

    #[tokio::test]
    async fn http_403_with_retry_after_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).insert_header("retry-after", "60"))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "rate_limited"
        );
    }

    #[tokio::test]
    async fn http_500_is_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "http_error"
        );
    }

    #[tokio::test]
    async fn malformed_json_is_malformed_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        assert_eq!(
            check(
                &state_for(format!("{}/x", server.uri())),
                &Version::parse("2.7.0").unwrap()
            )
            .await
            .unwrap_err(),
            "malformed_response"
        );
    }

    #[tokio::test]
    async fn timeout_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(body_json("v2.8.0")),
            )
            .mount(&server)
            .await;
        let state = UpdateCheckState::with_config(
            format!("{}/x", server.uri()),
            Duration::from_secs(300),
            Duration::from_millis(50),
        );
        assert_eq!(
            check(&state, &Version::parse("2.7.0").unwrap())
                .await
                .unwrap_err(),
            "timeout"
        );
    }

    #[tokio::test]
    async fn success_is_cached_within_ttl() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_json("v2.8.0")))
            .expect(1) // second call served from cache, no new request
            .mount(&server)
            .await;
        let state = state_for(format!("{}/x", server.uri()));
        let cur = Version::parse("2.7.0").unwrap();
        let a = check(&state, &cur).await.unwrap();
        let b = check(&state, &cur).await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn failure_is_not_cached_and_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_json("v2.8.0")))
            .mount(&server)
            .await;
        let state = state_for(format!("{}/x", server.uri()));
        let cur = Version::parse("2.7.0").unwrap();
        assert_eq!(check(&state, &cur).await.unwrap_err(), "http_error"); // not cached
        assert!(check(&state, &cur).await.unwrap().update_available); // retried, succeeds
    }

    #[tokio::test]
    async fn cache_expires_after_ttl() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_json("v2.8.0")))
            .expect(2)
            .mount(&server)
            .await;
        let state = UpdateCheckState::with_config(
            format!("{}/x", server.uri()),
            Duration::from_millis(30),
            Duration::from_secs(15),
        );
        let cur = Version::parse("2.7.0").unwrap();
        check(&state, &cur).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        check(&state, &cur).await.unwrap(); // TTL expired → second request
    }

    #[tokio::test]
    async fn concurrent_success_is_single_flight() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(150))
                    .set_body_json(body_json("v2.8.0")),
            )
            .expect(1) // both callers share ONE request
            .mount(&server)
            .await;
        let state = state_for(format!("{}/x", server.uri()));
        let cur = Version::parse("2.7.0").unwrap();
        let (a, b) = tokio::join!(check(&state, &cur), check(&state, &cur));
        assert!(a.unwrap().update_available && b.unwrap().update_available);
    }

    #[tokio::test]
    async fn concurrent_failure_is_single_flight() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_millis(150)))
            .expect(1) // one failed request shared by both callers, not two
            .mount(&server)
            .await;
        let state = state_for(format!("{}/x", server.uri()));
        let cur = Version::parse("2.7.0").unwrap();
        let (a, b) = tokio::join!(check(&state, &cur), check(&state, &cur));
        assert_eq!(a.unwrap_err(), "http_error");
        assert_eq!(b.unwrap_err(), "http_error");
    }

    #[tokio::test]
    async fn leader_cancellation_does_not_poison_flight() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(120))
                    .set_body_json(body_json("v2.8.0")),
            )
            .mount(&server)
            .await;
        let state = std::sync::Arc::new(state_for(format!("{}/x", server.uri())));
        let cur = Version::parse("2.7.0").unwrap();
        // Start a caller, then cancel it mid-flight; the detached fetch task lives on.
        let s1 = state.clone();
        let c1 = cur.clone();
        let h = tokio::spawn(async move { check(&s1, &c1).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.abort();
        // A fresh caller must still complete.
        assert!(check(&state, &cur).await.unwrap().update_available);
    }

    #[tokio::test]
    async fn worker_panic_does_not_poison_flight() {
        // The test-only "panic://boom" URL makes the worker task panic.
        let state = state_for("panic://boom".to_string());
        let cur = Version::parse("2.7.0").unwrap();
        // Supervisor converts the JoinError to Err and clears the flight...
        assert!(check(&state, &cur).await.is_err());
        // ...so a subsequent caller also returns (does not hang forever).
        assert!(check(&state, &cur).await.is_err());
    }
}

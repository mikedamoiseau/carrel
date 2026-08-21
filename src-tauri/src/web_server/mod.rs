pub mod api;
pub mod auth;
pub mod opds_feed;
pub mod web_ui;

use crate::db::DbPool;
use crate::error::{CarrelError, CarrelResult};
use axum::{http::StatusCode, middleware, Router};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// One profile as the web layer sees it (`GET /api/profiles`).
///
/// `switchable` is `profile_lock::access_allowed(locked, unlocked_this_session)`
/// — the same rule the desktop command and `profile_lock_gate` apply, so the
/// UI can grey out a locked profile instead of hiding it, and the switch
/// endpoint's refusal is never a surprise.
#[derive(Clone, serde::Serialize)]
pub struct WebProfile {
    pub name: String,
    pub active: bool,
    pub locked: bool,
    pub switchable: bool,
}

/// Profile listing + switching for the web layer.
///
/// The pool map, the `profile_lifecycle` lock, and the plugin-host rebuild all
/// live on `AppState`/`AppHandle`, which this module deliberately doesn't
/// depend on — so the capability is injected instead. `lib.rs` supplies an
/// `AppHandle`-backed implementation (the one new dependency the web layer
/// gains for remote switching); test harnesses supply a fake, and embeddings
/// with no Tauri app leave it `None`, which makes the endpoints 503.
pub trait ProfileHost: Send + Sync {
    /// All profiles with their active/locked/switchable state.
    fn list(&self) -> CarrelResult<Vec<WebProfile>>;

    /// Switch the active profile. Boxed future rather than `async fn` so the
    /// trait stays object-safe.
    fn switch(
        &self,
        name: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CarrelResult<()>> + Send + '_>>;
}

/// State shared with all axum handlers.
#[derive(Clone)]
pub struct WebState {
    /// The currently-active profile's DB pool (swapped on profile switch).
    pub pool: Arc<Mutex<DbPool>>,
    /// App data directory (covers, EPUB images, etc.).
    pub data_dir: PathBuf,
    /// App cache directory — root of the local source-cache
    /// (`{cache_dir}/source-cache/…`). Mirrors `AppState::cache_dir` so the
    /// web reader resolves network-mounted books to the same staged local
    /// copies the desktop reader stages.
    pub cache_dir: PathBuf,
    /// SHA-256 hash of the PIN (None if no PIN configured).
    pub pin_hash: Arc<Mutex<Option<String>>>,
    /// Active session tokens → creation time.
    pub sessions: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Rate limiter for login attempts (R2-2).
    pub login_limiter: Arc<auth::RateLimiter>,
    /// The currently-active profile's name, shared with `AppState` and
    /// swapped on profile switch (A-M2). Read by `profile_lock_gate` to
    /// know which profile's soft-lock state to check.
    pub active_profile_name: Arc<Mutex<String>>,
    /// Profiles unlocked (soft-lock, A-M2) this desktop session, shared
    /// with `AppState`. The profile password is never accepted over the
    /// web (Decision 5) — this is populated only by desktop-side
    /// `unlock_profile`/`switch_profile`.
    pub unlocked_profiles: Arc<Mutex<HashSet<String>>>,
    /// "Don't track this session" / private mode (B-M1), shared with
    /// `AppState` — the same flag desktop commands read, so a web-driven
    /// passive write (e.g. `PUT /api/books/:id/progress`) is suppressed
    /// exactly like its desktop counterpart. The only runtime mutator is
    /// the desktop-side `set_private_mode` command.
    pub private_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Remote profile switching (`GET /api/profiles`, `POST /api/profile`).
    /// `None` in harnesses with no Tauri app behind them — the endpoints then
    /// report 503 instead of pretending there are no profiles.
    pub profile_host: Option<Arc<dyn ProfileHost>>,
    /// Lazily-opened readonly pool over the installed dictionary artifact
    /// (`{data_dir}/dictionary/dictionary.db`), cached in place after first
    /// open. This is the SAME cache `AppState` holds — cloned in at server
    /// start — so desktop's `download_dictionary`/`delete_dictionary`
    /// invalidation covers web lookups too; a re-download or deletion while
    /// the server runs is visible on the next lookup instead of the pool
    /// silently serving the old artifact's unlinked inode forever. The
    /// dictionary artifact is profile-independent (one artifact serves every
    /// profile), so — unlike `pool` above — this is never touched by a
    /// profile switch.
    pub dictionary_pool: Arc<Mutex<Option<DbPool>>>,
}

impl WebState {
    /// Reads the private-mode flag (B-M1). Mirrors `AppState::is_private`.
    pub fn is_private(&self) -> bool {
        self.private_mode.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Directory holding the offline dictionary artifact. Mirrors
    /// `AppState::dictionary_dir`.
    pub fn dictionary_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("dictionary")
    }

    /// Get a database connection from the active pool.
    pub fn conn(
        &self,
    ) -> CarrelResult<r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>> {
        let pool = self.pool.lock()?;
        Ok(pool.get()?)
    }

    /// Resolve a book's stored `file_path` to an absolute local path,
    /// applying the same #64 M4 semantics the Tauri app uses:
    /// - linked books → return unchanged
    /// - legacy imported rows with an absolute path → return unchanged
    /// - imported rows with a storage key → resolve through the library
    ///   folder setting (falls back to the platform default)
    pub fn resolve_book_path(&self, book: &carrel_core::models::Book) -> CarrelResult<String> {
        // Prefer a locally-staged copy of the source file when one exists —
        // mirrors `AppState::resolve_book_path`, so a linked book on a network
        // share is served from local disk. Existence-only, LOCAL-only, and
        // cheap (never stats the possibly network-mounted source), so it's safe
        // on the per-page hot path. Content-addressed by `file_hash`, so a
        // present copy is always this book's bytes.
        if let Some(hash) = book.file_hash.as_deref() {
            let ext = std::path::Path::new(&book.file_path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if let Some(staged) =
                carrel_core::source_cache::staged_if_present(&self.cache_dir, hash, ext)
            {
                return Ok(staged.to_string_lossy().into_owned());
            }
        }

        if !book.is_imported {
            return Ok(book.file_path.clone());
        }
        let p = std::path::Path::new(&book.file_path);
        if p.is_absolute() {
            return Ok(book.file_path.clone());
        }
        let folder = {
            let conn = self.conn()?;
            match carrel_core::db::get_setting(&conn, "library_folder")? {
                Some(f) => f,
                None => carrel_core::paths::default_library_folder()?,
            }
        };
        let storage = carrel_core::storage::LocalStorage::new(folder)?;
        use carrel_core::storage::Storage;
        Ok(storage
            .local_path(&book.file_path)?
            .to_string_lossy()
            .to_string())
    }

    /// Returns a `Storage` handle for EPUB inline chapter images, rooted at
    /// `{data_dir}/images`. Mirrors `AppState::images_storage` so the Tauri
    /// and web-server flows write to the same physical layout. Introduced
    /// for #64 M6.
    pub fn images_storage(&self) -> CarrelResult<Arc<dyn carrel_core::storage::Storage>> {
        let root = self.data_dir.join("images");
        Ok(Arc::new(carrel_core::storage::LocalStorage::new(root)?))
    }

    /// The app-managed covers root, `{data_dir}/covers` — mirrors
    /// `AppState::covers_storage`'s layout. Used by
    /// `api::cover_write_path_is_safe` to confirm a book's (DB-backed, so
    /// potentially malformed) `cover_path` resolves inside this directory
    /// before the `?size=thumb` cache is allowed to write a sibling
    /// `thumb.jpg` next to it.
    pub fn covers_root(&self) -> PathBuf {
        self.data_dir.join("covers")
    }

    /// Whether a PIN is currently configured (i.e. web auth is enabled).
    /// Mirrors the check `auth_middleware` performs. A poisoned lock is
    /// treated as "PIN configured" so callers fail toward the safer choice
    /// (e.g. a non-cacheable response) rather than toward open access.
    pub fn has_pin(&self) -> bool {
        self.pin_hash
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(true)
    }
}

/// Map any error convertible to [`CarrelError`] into an HTTP `(status, message)`
/// tuple for axum handlers.
///
/// `NotFound` → 404, `PermissionDenied` → 403, `InvalidInput` → 400 and
/// `RateLimited` → 429 keep the error's own message text: clients rely on it,
/// and it is normally this codebase's own validation and lookup wording.
///
/// Everything else gets a fixed, generic body with the real message logged
/// server-side at error level, because the web server is reachable by anything
/// on the user's LAN that gets past the PIN and those messages are not ours:
///
/// - `Database`, `Io`, `Serialization`, `LockRequired`, `Internal` → 500.
///   These carry SQL fragments, filesystem paths and library diagnostics.
/// - `Network` → 502. Kept its text until M1's review: `From<reqwest::Error>`
///   and `From<opendal::Error>` populate it verbatim (see carrel-core's
///   `error.rs`), so it hands out upstream URLs, hostnames, ports and storage
///   endpoint config. No web route makes outbound requests today, which is
///   why nothing was leaking through it yet — it is sanitized so that the
///   first route which does cannot start.
///
/// Known remaining gap, deliberately not addressed here: the 4xx kinds above
/// are also constructed by carrel-core from third-party parser text (e.g.
/// `cbz.rs`'s `not_found(format!("Cannot read page '{name}': {e}"))`, and
/// `From<std::io::Error>` mapping `ErrorKind::NotFound` straight to
/// `NotFound(e.to_string())`), so archive internals can still reach a client
/// through a 404/400 on the page and cover routes. Fixing that means giving
/// those routes their own client-facing wording rather than blanket-sanitizing
/// bodies clients depend on.
///
/// Accepts `CarrelError` directly or any source error with a `From<E> for
/// CarrelError` impl (e.g. `std::io::Error`).
pub fn carrel_status<E: Into<CarrelError>>(e: E) -> (StatusCode, String) {
    let err: CarrelError = e.into();
    match err.kind() {
        "NotFound" => (StatusCode::NOT_FOUND, err.to_string()),
        "PermissionDenied" => (StatusCode::FORBIDDEN, err.to_string()),
        "InvalidInput" => (StatusCode::BAD_REQUEST, err.to_string()),
        "RateLimited" => (StatusCode::TOO_MANY_REQUESTS, err.to_string()),
        // Foreign text: upstream URLs, hosts and endpoint config.
        "Network" => {
            log::error!("web request failed (Network): {err}");
            (
                StatusCode::BAD_GATEWAY,
                "Upstream request failed".to_string(),
            )
        }
        kind => {
            // The kind is in the log line because it is now the only place it
            // survives — the body no longer distinguishes a Database failure
            // from an Io or Serialization one, and that distinction is the
            // first thing worth knowing when triaging a report.
            log::error!("web request failed ({kind}): {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

/// Handle to a running web server instance.
pub struct WebServerHandle {
    pub shutdown_tx: oneshot::Sender<()>,
    pub url: String,
    pub port: u16,
}

/// Which user-facing surfaces the embedded HTTP server exposes.
#[derive(Debug, Clone, Copy)]
pub struct ServerModes {
    pub web_ui: bool,
    pub opds: bool,
}

impl ServerModes {
    /// Whether the server should run at all.
    pub fn any(&self) -> bool {
        self.web_ui || self.opds
    }
}

/// Status returned to the frontend.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebServerStatus {
    pub running: bool,
    pub url: Option<String>,
    pub port: u16,
    pub has_pin: bool,
    pub web_ui_enabled: bool,
    pub opds_enabled: bool,
}

/// Detect the local LAN IP address.
/// Uses a UDP socket connecting to Google DNS (8.8.8.8:53) to determine
/// which local interface would be used for outbound traffic.
pub fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // R2-3: Use port 53 (DNS), not 80 (HTTP). No actual packet is sent —
    // connect() on a UDP socket just sets the default destination and lets
    // us read the local address the OS chose.
    socket.connect("8.8.8.8:53").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    // Don't return loopback — it's useless for LAN access
    if ip.is_loopback() {
        return None;
    }
    Some(ip.to_string())
}

/// Item 6: CSP hash for the tiny inline bootstrap script in index.html's
/// `<head>` that sets `data-theme` before first paint (avoids a flash of the
/// wrong theme). Must be regenerated (sha256, base64) if that script's exact
/// text ever changes — a mismatch here means the browser silently blocks the
/// script instead of erroring, so `test_csp_allows_theme_bootstrap_script_hash`
/// exists to catch drift in CI.
const THEME_BOOTSTRAP_SCRIPT_HASH: &str = "'sha256-49QkYHwfN2ynPleLv4yaOqJf59H4tRu6P3+IGG901M8='";

/// Middleware that adds security headers to all responses (R3-3).
/// Hex-encoded profile name, used as the `x-carrel-profile` response header.
///
/// Clients compare it against the value they booted with to notice that the
/// active profile moved under them (someone switched from another tab, another
/// device, or the desktop app — there is one shared active profile). Encoded
/// rather than sent verbatim because profile names are arbitrary text: a name
/// containing a newline must not be able to inject a header. Callers only ever
/// compare it, so nothing decodes it.
///
/// `static/app.js` and `static/sw.js` also read `x-carrel-profile`, so the
/// header name has to change on both sides at once.
pub(crate) fn profile_tag(name: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Stamps every response with the active profile's tag (see [`profile_tag`]).
///
/// Read *after* the inner handler runs, so the response to `POST /api/profile`
/// already carries the profile it switched to.
async fn profile_tag_middleware(
    axum::extract::State(state): axum::extract::State<WebState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(req).await;
    let name = match state.active_profile_name.lock() {
        Ok(guard) => guard.clone(),
        // A poisoned lock is not worth failing an otherwise-good response over;
        // the client simply doesn't get the hint on this one.
        Err(_) => return response,
    };
    if let Ok(value) = axum::http::HeaderValue::from_str(&profile_tag(&name)) {
        response.headers_mut().insert("x-carrel-profile", value);
    }
    response
}

async fn security_headers_middleware(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert(
        "content-security-policy",
        format!(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             script-src 'self' {THEME_BOOTSTRAP_SCRIPT_HASH}"
        )
        .parse()
        .unwrap(),
    );
    response
}

/// Middleware gating every request behind the soft-lock session state
/// (A-M2, Decision 5 / OQ-1a): the web/OPDS server only serves the
/// currently-active profile once it has been unlocked in the desktop
/// session. A locked-and-not-yet-unlocked profile is dark on the network —
/// the server keeps running but responds `503` to everything except the
/// public shell assets and the health check. This is independent of, and
/// in addition to, `auth::auth_middleware`'s web-PIN gate below; the
/// profile password itself is never accepted over HTTP.
///
/// Per the design spec this checks `unlocked_profiles` membership only —
/// it does not re-derive lock status from the keychain on every request.
/// `AppState`/`WebState` keep that set in sync with reality: a profile
/// with no lock configured is inserted into the set the moment it becomes
/// active (`switch_profile`, startup), so "not in the set" always means
/// "locked and not yet unlocked", never "never checked".
async fn profile_lock_gate(
    axum::extract::State(state): axum::extract::State<WebState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let path = req.uri().path();
    if path == "/api/health" || web_ui::PUBLIC_SHELL_ASSETS.contains(&path) {
        return next.run(req).await;
    }

    let active = match state.active_profile_name.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    };
    let unlocked = match state.unlocked_profiles.lock() {
        Ok(guard) => guard.contains(&active),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    };

    if unlocked {
        next.run(req).await
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "Profile locked").into_response()
    }
}

/// Build the full axum router with all routes and middleware.
/// Routes are conditionally mounted based on `modes`. Calling with
/// `ServerModes { web_ui: false, opds: false }` returns a router that
/// 404s every path — safe to call but the reconciler in commands.rs
/// short-circuits before reaching this state in production.
pub fn build_router(state: WebState, modes: ServerModes) -> Router {
    let mut router = Router::new();

    if modes.web_ui {
        // Web UI consumes /api, so /api lives alongside web_ui mode.
        // Without web_ui there's no consumer for /api.
        let api_routes = api::routes(state.clone());
        router = router.nest("/api", api_routes).merge(web_ui::routes());
    }
    if modes.opds {
        let opds_routes = opds_feed::routes(state.clone());
        router = router.nest("/opds", opds_routes);
    }

    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            profile_lock_gate,
        ))
        .layer(middleware::from_fn(security_headers_middleware))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            profile_tag_middleware,
        ))
        .with_state(state)
}

/// Default port for the web server.
pub const DEFAULT_PORT: u16 = 7788;

/// Start the web server on the given port. Returns a handle for shutdown.
pub async fn start(
    state: WebState,
    port: u16,
    modes: ServerModes,
) -> crate::error::CarrelResult<WebServerHandle> {
    use crate::error::CarrelError;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let router = build_router(state, modes);

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            CarrelError::invalid(format!(
                "Port {port} is already in use. Try a different port (1024\u{2013}65535)."
            ))
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            CarrelError::permission(format!(
                "Permission denied for port {port}. Use a port above 1024."
            ))
        } else {
            CarrelError::network(format!("Failed to start server on port {port}: {e}"))
        }
    })?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .ok();
    });

    let ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let url = format!("http://{}:{}", ip, port);

    Ok(WebServerHandle {
        shutdown_tx,
        url,
        port,
    })
}

/// Stop a running web server by sending on its shutdown channel.
pub fn stop(handle: WebServerHandle) {
    let _ = handle.shutdown_tx.send(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> WebState {
        let pool =
            crate::db::create_pool(&std::path::PathBuf::from(":memory:")).expect("in-memory DB");
        WebState {
            pool: Arc::new(Mutex::new(pool)),
            data_dir: PathBuf::from("/tmp"),
            cache_dir: std::env::temp_dir(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(HashSet::from(["default".to_string()]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: Arc::new(Mutex::new(None)),
        }
    }

    // ── Profile listing / switching over HTTP ────────────────────────────
    //
    // The handlers are exercised against a fake `ProfileHost` that mirrors
    // the real gate's decisions (unknown → InvalidInput, locked-and-not-
    // unlocked → LockRequired). The gate itself is the desktop command's
    // shared core, covered in `commands::tests`; what's under test here is
    // the HTTP surface: JSON shape, status mapping, auth, and the
    // CSRF-shaped request.

    struct FakeProfileHost {
        profiles: Mutex<Vec<WebProfile>>,
    }

    impl FakeProfileHost {
        /// `default` (active, no lock), `magazines` (no lock), `vault`
        /// (locked and not unlocked this session).
        fn new() -> Arc<Self> {
            Arc::new(Self {
                profiles: Mutex::new(vec![
                    WebProfile {
                        name: "default".into(),
                        active: true,
                        locked: false,
                        switchable: true,
                    },
                    WebProfile {
                        name: "magazines".into(),
                        active: false,
                        locked: false,
                        switchable: true,
                    },
                    WebProfile {
                        name: "vault".into(),
                        active: false,
                        locked: true,
                        switchable: false,
                    },
                ]),
            })
        }

        fn active(&self) -> String {
            self.profiles
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.active)
                .map(|p| p.name.clone())
                .unwrap()
        }
    }

    impl ProfileHost for FakeProfileHost {
        fn list(&self) -> CarrelResult<Vec<WebProfile>> {
            Ok(self.profiles.lock().unwrap().clone())
        }

        fn switch(
            &self,
            name: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CarrelResult<()>> + Send + '_>>
        {
            Box::pin(async move {
                let mut profiles = self.profiles.lock().unwrap();
                let target = profiles
                    .iter()
                    .find(|p| p.name == name)
                    .ok_or_else(|| CarrelError::invalid(format!("Profile '{name}' not found")))?;
                if !target.switchable {
                    return Err(CarrelError::lock_required(format!(
                        "Profile '{name}' is locked"
                    )));
                }
                for p in profiles.iter_mut() {
                    p.active = p.name == name;
                }
                Ok(())
            })
        }
    }

    fn state_with_profile_host(host: Arc<dyn ProfileHost>) -> WebState {
        WebState {
            profile_host: Some(host),
            ..test_state()
        }
    }

    /// Serves `state` on an ephemeral port; returns the port and a shutdown
    /// sender. Mirrors `test_start_and_stop_server`'s setup.
    async fn serve(state: WebState) -> (u16, oneshot::Sender<()>) {
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });
        (port, tx)
    }

    #[tokio::test]
    async fn get_profiles_reports_active_locked_and_switchable() {
        let (port, tx) = serve(state_with_profile_host(FakeProfileHost::new())).await;

        let body: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(
            body,
            serde_json::json!([
                {"name": "default", "active": true, "locked": false, "switchable": true},
                {"name": "magazines", "active": false, "locked": false, "switchable": true},
                {"name": "vault", "active": false, "locked": true, "switchable": false},
            ])
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn post_profile_switches_the_shared_active_profile() {
        let host = FakeProfileHost::new();
        let (port, tx) = serve(state_with_profile_host(host.clone())).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/profile"))
            .json(&serde_json::json!({"name": "magazines"}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"active": "magazines"})
        );
        assert_eq!(host.active(), "magazines");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn post_profile_returns_404_for_an_unknown_profile() {
        let (port, tx) = serve(state_with_profile_host(FakeProfileHost::new())).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/profile"))
            .json(&serde_json::json!({"name": "ghost"}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);
        let _ = tx.send(());
    }

    /// Decision 2: the profile password never crosses the network, so a
    /// locked profile that wasn't unlocked on the desktop this session is
    /// refused — 423 Locked, never a password prompt.
    #[tokio::test]
    async fn post_profile_returns_423_for_a_locked_profile() {
        let host = FakeProfileHost::new();
        let (port, tx) = serve(state_with_profile_host(host.clone())).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/profile"))
            .json(&serde_json::json!({"name": "vault"}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 423);
        assert!(resp.text().await.unwrap().contains("desktop"));
        assert_eq!(host.active(), "default", "active profile untouched");
        let _ = tx.send(());
    }

    /// CSRF: the session cookie is `SameSite=Strict` (asserted by
    /// `test_login_sets_session_cookie`), so a cross-site POST carries no
    /// session. Basic auth, however, IS accepted on every `/api` path and
    /// browsers do replay cached Basic credentials cross-site — so the switch
    /// additionally requires a JSON body. A form-encoded POST (the only shape
    /// that dodges a CORS preflight, and thus the only cross-site request that
    /// would actually reach the handler) is rejected.
    #[tokio::test]
    async fn post_profile_rejects_a_form_encoded_body() {
        let host = FakeProfileHost::new();
        let (port, tx) = serve(state_with_profile_host(host.clone())).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/profile"))
            .form(&[("name", "magazines")])
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);
        assert_eq!(host.active(), "default");
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn profile_routes_require_authentication() {
        let state = state_with_profile_host(FakeProfileHost::new());
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));
        let (port, tx) = serve(state).await;

        let list = reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
            .await
            .unwrap();
        assert_eq!(list.status(), 401);

        let switch = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/api/profile"))
            .json(&serde_json::json!({"name": "magazines"}))
            .send()
            .await
            .unwrap();
        assert_eq!(switch.status(), 401);
        let _ = tx.send(());
    }

    /// The OPDS surface gains no switch: `/api` is only mounted in `web_ui`
    /// mode, so an OPDS-only server has no profile endpoints at all.
    #[tokio::test]
    async fn opds_only_server_exposes_no_profile_endpoints() {
        let router = build_router(
            state_with_profile_host(FakeProfileHost::new()),
            ServerModes {
                web_ui: false,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        assert_eq!(
            reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/profile"))
                .json(&serde_json::json!({"name": "magazines"}))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let _ = tx.send(());
    }

    /// Every response carries a tag identifying the active profile so a client
    /// left on a stale profile (another browser tab, or one open while someone
    /// switched from the desktop) can notice on its next request and reload.
    /// Hex-encoded rather than the raw name, because profile names are
    /// arbitrary text and would otherwise need sanitizing to be a header value;
    /// clients only ever compare it, never decode it.
    #[tokio::test]
    async fn responses_carry_the_active_profile_tag() {
        let state = state_with_profile_host(FakeProfileHost::new());
        let unlocked = state.unlocked_profiles.clone();
        let active = state.active_profile_name.clone();
        let (port, tx) = serve(state).await;

        let tag = |resp: &reqwest::Response| {
            resp.headers()
                .get("x-carrel-profile")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };

        let first = reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
            .await
            .unwrap();
        let before = tag(&first).expect("header present");
        assert_eq!(before, profile_tag("default"));

        // A switch from anywhere moves the shared active-profile handle; the
        // header must follow it.
        unlocked.lock().unwrap().insert("magazines".to_string());
        *active.lock().unwrap() = "magazines".to_string();
        let second = reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
            .await
            .unwrap();
        let after = tag(&second).expect("header present");
        assert_eq!(after, profile_tag("magazines"));
        assert_ne!(before, after);
        let _ = tx.send(());
    }

    /// Always a valid header value however the profile was named — a name with
    /// a newline in it must not be able to inject a header — and distinct per
    /// name so a client can tell a switch happened.
    #[test]
    fn profile_tag_is_hex_and_distinguishes_names() {
        let a = profile_tag("default");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, profile_tag("default"));
        assert_ne!(a, profile_tag("Default"));
        assert_ne!(a, profile_tag("magazines"));
        let weird = profile_tag("héllo\r\nX-Injected: 1");
        assert!(weird.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(axum::http::HeaderValue::from_str(&weird).is_ok());
    }

    /// Harnesses (and any future headless embedding) construct `WebState`
    /// without a host; the endpoints must degrade to a clear 503 rather than
    /// panicking or silently reporting an empty profile list.
    #[tokio::test]
    async fn profile_routes_report_503_without_a_profile_host() {
        let (port, tx) = serve(test_state()).await;

        assert_eq!(
            reqwest::get(format!("http://127.0.0.1:{port}/api/profiles"))
                .await
                .unwrap()
                .status(),
            503
        );
        assert_eq!(
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/profile"))
                .json(&serde_json::json!({"name": "magazines"}))
                .send()
                .await
                .unwrap()
                .status(),
            503
        );
        let _ = tx.send(());
    }

    /// Balanced-brace body of the first CSS block whose header text matches
    /// `header` — the substring before its `{`, e.g. a selector list or an
    /// `@media (...)` prelude. Balanced scanning handles the nesting an @media
    /// block introduces as well as a flat rule, so the CSS source-guard tests
    /// below share this one scanner.
    fn css_block<'a>(css: &'a str, header: &str) -> Option<&'a str> {
        let start = css.find(header)?;
        let open = css[start..].find('{')? + start;
        let mut depth = 0usize;
        for (i, ch) in css[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&css[open + 1..open + i]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn test_get_local_ip() {
        // Should return Some on machines with network access
        let ip = get_local_ip();
        if let Some(ref addr) = ip {
            assert!(!addr.is_empty());
            // Should look like an IP address (contains dots)
            assert!(addr.contains('.'));
            // R2-3: Should not be 127.0.0.1 on a machine with a LAN interface
            // (can't strictly assert this in CI, but verify it's a valid IP)
            assert!(addr.parse::<std::net::IpAddr>().is_ok());
        }
    }

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 7788);
    }

    #[test]
    fn test_web_state_conn() {
        let state = test_state();
        // Should be able to get a connection from the pool
        let conn = state.conn();
        assert!(conn.is_ok());
    }

    #[tokio::test]
    async fn test_start_and_stop_server() {
        let state = test_state();
        // Use port 0 to let the OS assign a free port
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let actual_port = listener.local_addr().unwrap().port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_handle = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
        });

        // Server should be responding
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Shutdown
        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_server_auth_blocks_protected_routes() {
        let state = test_state();
        // Set a PIN so auth is required
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let actual_port = listener.local_addr().unwrap().port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        // Protected route without auth should return 401
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/books"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Public routes should work without auth
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_profile_lock_gate_blocks_locked_and_not_unlocked_profile() {
        let state = test_state();
        // `test_state()` defaults `unlocked_profiles` to `{"default"}` (the
        // common case: no lock ever configured). Clearing it simulates the
        // active profile having a stored lock that hasn't been unlocked
        // this session (A-M2) — no PIN is configured, so without the
        // profile-lock gate this request would otherwise sail through.
        state.unlocked_profiles.lock().unwrap().clear();

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state.clone(),
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let actual_port = listener.local_addr().unwrap().port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        // Library-data routes are refused while the active profile is locked.
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/books"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503);

        // The server keeps running: health check and the public shell
        // still respond.
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Unlocking the active profile (as `unlock_profile`/`switch_profile`
        // would) lets requests through again.
        state
            .unlocked_profiles
            .lock()
            .unwrap()
            .insert("default".to_string());
        let resp = client
            .get(format!("http://127.0.0.1:{actual_port}/api/books"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_server_login_and_access() {
        let state = test_state();
        let pin_hash = auth::hash_pin("9876");
        *state.pin_hash.lock().unwrap() = Some(pin_hash);

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let actual_port = listener.local_addr().unwrap().port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        // Login with correct PIN
        let resp = client
            .post(format!("http://127.0.0.1:{actual_port}/api/auth"))
            .json(&serde_json::json!({"pin": "9876"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let token = body["token"].as_str().unwrap();
        assert!(!token.is_empty());

        // Login with wrong PIN
        let resp = client
            .post(format!("http://127.0.0.1:{actual_port}/api/auth"))
            .json(&serde_json::json!({"pin": "0000"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_login_sets_cookie() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/auth"))
            .json(&serde_json::json!({"pin": "1234"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Check Set-Cookie header
        let cookie = resp
            .headers()
            .get("set-cookie")
            .expect("login should set a cookie");
        let cookie_str = cookie.to_str().unwrap();
        assert!(cookie_str.contains("carrel_session="));
        assert!(cookie_str.contains("HttpOnly"));
        assert!(cookie_str.contains("SameSite=Strict"));

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn test_bearer_token_grants_access() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("5555"));

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        // Login to get token
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/auth"))
            .json(&serde_json::json!({"pin": "5555"}))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let token = body["token"].as_str().unwrap();

        // Use bearer token to access protected route
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        // Should not be 401 (might be 404 since /api/books isn't implemented yet, that's ok)
        assert_ne!(resp.status(), 401);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn test_basic_auth_grants_access() {
        use base64::Engine;

        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("7777"));

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:7777");

        // Basic auth with correct PIN should grant access to OPDS
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .header("Authorization", format!("Basic {encoded}"))
            .send()
            .await
            .unwrap();
        assert_ne!(resp.status(), 401);

        // Basic auth with wrong PIN should be rejected
        let wrong = base64::engine::general_purpose::STANDARD.encode("user:0000");
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .header("Authorization", format!("Basic {wrong}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let _ = tx.send(());
    }

    /// The OpenSearch descriptor must be reachable through the real router.
    /// The handler's own tests call it directly, so they would still pass if
    /// the route were never mounted under `/opds` — and every feed advertises
    /// it, so an unmounted route would be a 404 that only third-party clients
    /// ever hit. This also pins that the served template is absolute on the
    /// authority the client actually dialed.
    #[tokio::test]
    async fn opds_opensearch_descriptor_is_mounted_and_absolute() {
        let state = test_state();
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: false,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/opds/opensearch.xml"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "descriptor route must be mounted");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/opensearchdescription+xml"
        );
        let xml = resp.text().await.unwrap();
        assert!(
            xml.contains(&format!(
                r#"template="http://127.0.0.1:{port}/opds/search?q={{searchTerms}}""#
            )),
            "template must be absolute on the dialed authority, got: {xml}"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn test_no_pin_allows_all_access() {
        let state = test_state();
        // pin_hash is None — no PIN set, should allow open access

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        // Protected route should be accessible without auth when no PIN is set
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            401,
            "No PIN = open access, should not get 401"
        );

        let _ = tx.send(());
    }

    // R3-3: CSP headers present on responses
    #[tokio::test]
    async fn test_responses_have_security_headers() {
        let state = test_state();
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/health"))
            .send()
            .await
            .unwrap();

        assert!(resp.headers().contains_key("x-content-type-options"));
        assert!(resp.headers().contains_key("x-frame-options"));
        assert!(resp.headers().contains_key("content-security-policy"));

        let _ = tx.send(());
    }

    // Item 6: the theme bootstrap inline script in index.html (sets
    // data-theme before first paint, avoiding a flash of the wrong theme)
    // must be allowed by CSP via a script-src hash rather than a blanket
    // 'unsafe-inline'. Finding 7: the hash is computed here independently
    // from the actual served index.html (the same file web_ui.rs embeds via
    // include_str!) rather than compared against THEME_BOOTSTRAP_SCRIPT_HASH
    // itself — comparing the constant to the constant it also builds the CSP
    // header from could never catch drift between the script text and the
    // hash, which is exactly the case this test exists to catch.
    #[tokio::test]
    async fn test_csp_allows_theme_bootstrap_script_hash() {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        // Same source of truth web_ui.rs embeds as INDEX_HTML.
        const INDEX_HTML: &str = include_str!("static/index.html");
        let open_tag = "<script>";
        let start = INDEX_HTML
            .find(open_tag)
            .expect("bootstrap <script> tag not found in index.html")
            + open_tag.len();
        let end = INDEX_HTML[start..]
            .find("</script>")
            .expect("closing </script> tag not found after bootstrap script")
            + start;
        let script_body = &INDEX_HTML[start..end];

        let digest = Sha256::digest(script_body.as_bytes());
        let computed_hash = format!(
            "'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
        assert_eq!(
            computed_hash, THEME_BOOTSTRAP_SCRIPT_HASH,
            "THEME_BOOTSTRAP_SCRIPT_HASH is out of date with index.html's actual bootstrap \
             script text — regenerate it if the script was intentionally changed"
        );

        let state = test_state();
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();

        let csp = resp
            .headers()
            .get("content-security-policy")
            .expect("CSP header present")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            csp.contains(&computed_hash),
            "CSP script-src should allow the independently-computed bootstrap script hash: {csp}"
        );
        assert!(
            csp.contains("script-src 'self'"),
            "script-src should still allow the external app.js: {csp}"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn build_router_web_ui_only_serves_root_not_opds() {
        let state = test_state();
        let modes = ServerModes {
            web_ui: true,
            opds: false,
        };
        let router = build_router(state, modes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        // / → 200 (HTML UI)
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // /opds → 404 (not mounted)
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn build_router_opds_only_serves_opds_not_root() {
        let state = test_state();
        let modes = ServerModes {
            web_ui: false,
            opds: true,
        };
        let router = build_router(state, modes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        // /opds → 200
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // / → 404 (web UI not mounted)
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // /api/* → 404 (api lives with web_ui)
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn build_router_both_serves_root_and_opds() {
        let state = test_state();
        let modes = ServerModes {
            web_ui: true,
            opds: true,
        };
        let router = build_router(state, modes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let r1 = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(r1.status(), 200);
        let r2 = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .send()
            .await
            .unwrap();
        assert_eq!(r2.status(), 200);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn data_export_requires_auth() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/data-export"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn data_export_forbidden_without_pin() {
        // No PIN configured: `auth_middleware` lets every route through, but the
        // bulk personal-data export must still refuse to serve on an
        // unauthenticated server.
        let state = test_state();
        assert!(state.pin_hash.lock().unwrap().is_none());

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/data-export"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn data_export_returns_zip_for_authed_request() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        // Authenticate via HTTP Basic Auth (PIN as password).
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/api/data-export"))
            .basic_auth("carrel", Some("1234"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/zip"
        );
        let disp = resp
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disp.contains("carrel-export-"));
        assert!(disp.ends_with(".zip\""));

        let bytes = resp.bytes().await.unwrap();
        let reader = std::io::Cursor::new(bytes.to_vec());
        let mut archive = zip::ZipArchive::new(reader).expect("valid zip");
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        assert!(entry.name().starts_with("carrel-export-"));
        assert!(entry.name().ends_with(".json"));
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).expect("valid json");
        assert!(parsed["books"].is_array());
        assert!(parsed["activity_log"].is_array());
        assert!(parsed["settings"].is_object());

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn build_router_neither_serves_nothing() {
        let state = test_state();
        let modes = ServerModes {
            web_ui: false,
            opds: false,
        };
        // Must not panic. Every request 404s.
        let router = build_router(state, modes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .get(format!("http://127.0.0.1:{port}/opds"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    /// Minimal CBZ fixture with a single (fake) page image, for the
    /// page-image/page-count cache-control tests below.
    fn write_cache_test_cbz(dir: &std::path::Path) -> std::path::PathBuf {
        let cbz_path = dir.join("test.cbz");
        let file = std::fs::File::create(&cbz_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("page01.jpg", options).unwrap();
        std::io::Write::write_all(&mut zip, b"fake jpg bytes").unwrap();
        zip.finish().unwrap();
        cbz_path
    }

    fn cache_test_book(cbz_path: &std::path::Path) -> crate::models::Book {
        crate::models::Book {
            id: "cache-test-book".to_string(),
            title: "Cache Test".to_string(),
            author: "Author".to_string(),
            file_path: cbz_path.to_string_lossy().to_string(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: crate::models::BookFormat::Cbz,
            file_hash: None,
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: None,
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: false,
            want_to_read: false,
        }
    }

    /// The web reader must resolve a network-mounted book to its locally-staged
    /// copy when one is present — the same optimization the desktop reader gets
    /// (`AppState::resolve_book_path`). Content-addressed by `file_hash`, so a
    /// present copy is always this book's bytes; a book with no hash, or no
    /// staged copy, resolves to the original (remote) path unchanged.
    #[test]
    fn resolve_book_path_prefers_staged_copy() {
        let cache = tempfile::tempdir().unwrap();
        let pool = crate::db::create_pool(&PathBuf::from(":memory:")).expect("in-memory DB");
        let state = WebState {
            pool: Arc::new(Mutex::new(pool)),
            data_dir: PathBuf::from("/tmp"),
            cache_dir: cache.path().to_path_buf(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(HashSet::from(["default".to_string()]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: Arc::new(Mutex::new(None)),
        };

        let mut book = cache_test_book(std::path::Path::new("/Volumes/remote/comic.pdf"));
        book.format = crate::models::BookFormat::Pdf;
        book.file_hash = Some("webhash1".to_string());

        // No staged copy yet → resolves to the original (remote) path.
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            "/Volumes/remote/comic.pdf"
        );

        // Stage a local copy under the state's cache_dir (keyed by hash + ext).
        let srcdir = tempfile::tempdir().unwrap();
        let src = srcdir.path().join("comic.pdf");
        std::fs::write(&src, b"staged bytes").unwrap();
        let staged =
            carrel_core::source_cache::stage(cache.path(), &src, "webhash1", "pdf").unwrap();

        // Now resolve prefers the local staged copy.
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            staged.to_string_lossy()
        );

        // A book with no hash never resolves to a staged copy.
        book.file_hash = None;
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            "/Volumes/remote/comic.pdf"
        );
    }

    /// M2 acceptance (local side): a page request for a book whose source is on
    /// a LOCAL filesystem must never create a staged copy — there's nothing to
    /// optimize — and must not break the serving path. This also exercises the
    /// `ensure_web_source_staged` → `spawn_blocking` trigger from the web
    /// (tokio) request context, so a runtime mismatch would surface as a failed
    /// render here rather than only against a real network mount.
    #[tokio::test]
    async fn local_book_page_request_does_not_stage() {
        let cache = tempfile::tempdir().unwrap();
        let pool = crate::db::create_pool(&PathBuf::from(":memory:")).expect("in-memory DB");
        let state = WebState {
            pool: Arc::new(Mutex::new(pool)),
            data_dir: PathBuf::from("/tmp"),
            cache_dir: cache.path().to_path_buf(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(HashSet::from(["default".to_string()]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: Arc::new(Mutex::new(None)),
        };

        let dir = tempfile::tempdir().unwrap();
        let cbz_path = write_cache_test_cbz(dir.path());
        let mut book = cache_test_book(&cbz_path);
        book.file_hash = Some("localcbzhash".to_string());
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &book).unwrap();
        }

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/cache-test-book/pages/0"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "a local CBZ page must still render with the staging trigger wired in"
        );

        assert!(
            carrel_core::source_cache::staged_if_present(cache.path(), "localcbzhash", "cbz")
                .is_none(),
            "a local book must never be staged"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn page_image_and_page_count_cache_control_no_pin() {
        let state = test_state();
        // pin_hash is None — no PIN configured, so responses are safe to
        // cache in the browser for a while.

        let dir = tempfile::tempdir().unwrap();
        let cbz_path = write_cache_test_cbz(dir.path());
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cache_test_book(&cbz_path)).unwrap();
        }

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/cache-test-book/pages/0"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "private, max-age=3600",
            "pages/{{index}} should be cacheable when no PIN is configured"
        );

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/cache-test-book/page-count"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "private, max-age=3600",
            "page-count should be cacheable when no PIN is configured"
        );

        let _ = tx.send(());
    }

    // S2: once a PIN is configured, a cached page image/page-count response
    // would let the same browser keep serving protected pages for up to an
    // hour after the session expires — those requests never reach
    // `auth_middleware` at all. `no-store` closes that gap.
    #[tokio::test]
    async fn page_image_and_page_count_cache_control_with_pin_is_no_store() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));

        let dir = tempfile::tempdir().unwrap();
        let cbz_path = write_cache_test_cbz(dir.path());
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cache_test_book(&cbz_path)).unwrap();
        }

        let router = build_router(
            state,
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });

        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/cache-test-book/pages/0"
            ))
            .basic_auth("carrel", Some("1234"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-store",
            "pages/{{index}} must not be cacheable once a PIN is configured"
        );

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/cache-test-book/page-count"
            ))
            .basic_auth("carrel", Some("1234"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-store",
            "page-count must not be cacheable once a PIN is configured"
        );

        let _ = tx.send(());
    }

    // ── Item 11: cover thumbnails ────────────────────────────────────────────

    /// Encodes a solid-color JPEG of the given dimensions to `path`.
    fn write_test_jpeg(path: &std::path::Path, w: u32, h: u32) {
        let buf: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_fn(w, h, |_, _| image::Rgb([180u8, 90, 60]));
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 90);
        encoder.encode_image(&buf).unwrap();
    }

    fn cover_test_book(id: &str, cover_path: Option<&std::path::Path>) -> crate::models::Book {
        crate::models::Book {
            id: id.to_string(),
            title: "Cover Test".to_string(),
            author: "Author".to_string(),
            file_path: "/nonexistent/cover-test.epub".to_string(),
            cover_path: cover_path.map(|p| p.to_string_lossy().to_string()),
            total_chapters: 1,
            added_at: 0,
            format: crate::models::BookFormat::Epub,
            file_hash: None,
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: None,
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: false,
            want_to_read: false,
        }
    }

    fn dims_of(bytes: &[u8]) -> (u32, u32) {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    }

    #[tokio::test]
    async fn cover_thumb_returns_downscaled_jpeg() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-1", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-1/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "image/jpeg"
        );
        let bytes = resp.bytes().await.unwrap();
        let (w, _h) = dims_of(&bytes);
        assert!(
            w <= crate::commands::THUMB_WIDTH,
            "thumb width {w} must be <= desktop THUMB_WIDTH {}",
            crate::commands::THUMB_WIDTH
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_thumb_persists_and_second_request_serves_cached_file() {
        let mut state = test_state();
        let dir = tempfile::tempdir().unwrap();
        // Finding 1 requires the thumbnail write to land inside
        // `{data_dir}/covers` — lay the fixture out like the real app does
        // (`{data_dir}/covers/{book_id}/cover.jpg`) so the persist isn't
        // skipped by the new safety guard.
        state.data_dir = dir.path().to_path_buf();
        let cover_dir = dir.path().join("covers").join("thumb-2");
        std::fs::create_dir_all(&cover_dir).unwrap();
        let cover_path = cover_dir.join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-2", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-2/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let thumb_path = cover_dir.join("thumb.jpg");
        assert!(thumb_path.exists(), "first request must persist thumb.jpg");

        // Overwrite the persisted thumbnail with a marker so we can prove
        // the second request serves the cached file instead of regenerating.
        let marker = b"MARKER-BYTES-NOT-A-REAL-JPEG-BUT-READ-AS-IS".to_vec();
        std::fs::write(&thumb_path, &marker).unwrap();

        let resp2 = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-2/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp2.status(), 200);
        let bytes2 = resp2.bytes().await.unwrap();
        assert_eq!(
            bytes2.as_ref(),
            marker.as_slice(),
            "second request must serve the persisted thumb.jpg unchanged, not regenerate"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_thumb_returns_original_bytes_for_small_cover() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        // Below THUMB_WIDTH (320) — make_thumbnail returns Ok(None).
        write_test_jpeg(&cover_path, 200, 300);
        let original_bytes = std::fs::read(&cover_path).unwrap();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-3", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-3/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(
            bytes.as_ref(),
            original_bytes.as_slice(),
            "small cover must be served unchanged when thumb is requested"
        );

        let thumb_path = dir.path().join("thumb.jpg");
        assert!(
            !thumb_path.exists(),
            "no thumb.jpg should be persisted for an already-small cover"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_no_size_param_is_byte_identical_to_previous_behavior() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        let original_bytes = std::fs::read(&cover_path).unwrap();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-4", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books/thumb-4/cover"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), original_bytes.as_slice());

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_unknown_size_value_falls_back_to_full() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        let original_bytes = std::fs::read(&cover_path).unwrap();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-5", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-5/cover?size=banana"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), original_bytes.as_slice());

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_no_cover_404s_for_both_sizes() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("no-cover-1", None)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/no-cover-1/cover"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/no-cover-1/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_cache_control_no_pin() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-6", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        for url in [
            format!("http://127.0.0.1:{port}/api/books/thumb-6/cover"),
            format!("http://127.0.0.1:{port}/api/books/thumb-6/cover?size=thumb"),
        ] {
            let resp = client.get(&url).send().await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(
                resp.headers()
                    .get("cache-control")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "private, max-age=86400",
                "{url} should be cacheable when no PIN is configured"
            );
        }

        let _ = tx.send(());
    }

    // Finding 8: covers are decorative artwork, not book content — unlike
    // page images/page-count, they stay cacheable even once a PIN is
    // configured (OPDS e-reader clients re-fetch full covers constantly
    // under per-request Basic Auth, and a blanket `no-store` regressed them
    // for little real security benefit).
    #[tokio::test]
    async fn cover_cache_control_with_pin_is_still_cacheable() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-7", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        for url in [
            format!("http://127.0.0.1:{port}/api/books/thumb-7/cover"),
            format!("http://127.0.0.1:{port}/api/books/thumb-7/cover?size=thumb"),
        ] {
            let resp = client
                .get(&url)
                .basic_auth("carrel", Some("1234"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(
                resp.headers()
                    .get("cache-control")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "private, max-age=86400",
                "{url} should stay cacheable even once a PIN is configured — covers aren't \
                 session-sensitive like page content"
            );
        }

        let _ = tx.send(());
    }

    // Finding 1: a `cover_path` outside the app's covers root (e.g. a
    // malformed/adversarial DB row) must still be servable — reading it is
    // pre-existing behavior — but the thumbnail-cache write this feature
    // introduces must never be steered outside that directory.
    #[tokio::test]
    async fn cover_thumb_write_skipped_outside_covers_root() {
        let state = test_state();
        // `test_state()` fixes `data_dir` to `/tmp`, so a cover living in an
        // unrelated tempdir (same shape every other cover test used before
        // this review pass) resolves outside `{data_dir}/covers`.
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-8", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-8/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        let (w, _h) = dims_of(&bytes);
        assert!(
            w <= crate::commands::THUMB_WIDTH,
            "must still serve an in-memory-generated thumbnail even when the write is skipped"
        );

        let thumb_path = dir.path().join("thumb.jpg");
        assert!(
            !thumb_path.exists(),
            "a cover_path outside the covers root must never get a thumb.jpg written next to it"
        );

        let _ = tx.send(());
    }

    // Finding 2a: a persisted thumbnail must not be served forever once the
    // cover it was made from has been replaced.
    #[tokio::test]
    async fn cover_thumb_regenerates_after_cover_replaced() {
        let mut state = test_state();
        let dir = tempfile::tempdir().unwrap();
        state.data_dir = dir.path().to_path_buf();
        let cover_dir = dir.path().join("covers").join("thumb-9");
        std::fs::create_dir_all(&cover_dir).unwrap();
        let cover_path = cover_dir.join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-9", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-9/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (_w1, h1) = dims_of(&resp.bytes().await.unwrap());

        let thumb_path = cover_dir.join("thumb.jpg");
        assert!(thumb_path.exists(), "first request must persist thumb.jpg");

        // Replace the cover with a differently-shaped image and force its
        // mtime strictly ahead of the persisted thumbnail's — the freshness
        // check (finding 2a) has nothing else to key off of.
        write_test_jpeg(&cover_path, 900, 300);
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&cover_path)
            .unwrap()
            .set_modified(future)
            .unwrap();

        let resp2 = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-9/cover?size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp2.status(), 200);
        let (_w2, h2) = dims_of(&resp2.bytes().await.unwrap());

        assert_ne!(
            h1, h2,
            "thumbnail must reflect the replaced cover's new aspect ratio, not stale cached art"
        );

        let _ = tx.send(());
    }

    // Finding 5: `Query<CoverQuery>` used to hard-400 on request shapes real
    // clients send (duplicate params from a proxy, malformed percent
    // encoding). Both must now serve a normal 200 instead.
    #[tokio::test]
    async fn cover_duplicate_size_param_is_lenient() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-10", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-10/cover?size=thumb&size=thumb"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "duplicate size params must not 400");

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn cover_malformed_percent_encoding_falls_back_to_full() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let cover_path = dir.path().join("cover.jpg");
        write_test_jpeg(&cover_path, 1200, 1800);
        let original_bytes = std::fs::read(&cover_path).unwrap();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cover_test_book("thumb-11", Some(&cover_path))).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // `%zz` isn't valid percent-encoding — it must fall back to serving
        // the full cover rather than 400ing.
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/thumb-11/cover?size=%zz"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), original_bytes.as_slice());

        let _ = tx.send(());
    }

    // ── Item 4: Two-way reading progress sync ───────────────────────────────

    fn progress_test_book(id: &str, total_chapters: u32) -> crate::models::Book {
        crate::models::Book {
            id: id.to_string(),
            title: "Progress Test".to_string(),
            author: "Author".to_string(),
            file_path: "/nonexistent/progress-test.cbz".to_string(),
            cover_path: None,
            total_chapters,
            added_at: 0,
            format: crate::models::BookFormat::Cbz,
            file_hash: None,
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: None,
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: false,
            want_to_read: false,
        }
    }

    /// Spins up a real server on a random port for a progress-sync test.
    /// Returns the (moved-back) state for direct DB assertions, the port,
    /// and the shutdown sender the caller must fire when done.
    async fn spawn_progress_test_server(state: WebState) -> (WebState, u16, oneshot::Sender<()>) {
        let router = build_router(
            state.clone(),
            ServerModes {
                web_ui: true,
                opds: true,
            },
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
        });
        (state, port, tx)
    }

    #[tokio::test]
    async fn progress_put_then_get_roundtrip() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-1", 50)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let put_resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-1/progress"))
            .json(&serde_json::json!({"chapter_index": 5, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), 200);

        let get_resp = client
            .get(format!("http://127.0.0.1:{port}/api/books/prog-1/progress"))
            .send()
            .await
            .unwrap();
        assert_eq!(get_resp.status(), 200);
        let body: serde_json::Value = get_resp.json().await.unwrap();
        assert_eq!(body["chapter_index"], 5);
        assert_eq!(body["book_id"], "prog-1");

        // The same row must be readable through the desktop app's own db
        // function — the web write path must not diverge from the shape
        // the desktop persists.
        let conn = state.conn().unwrap();
        let progress = crate::db::get_reading_progress(&conn, "prog-1")
            .unwrap()
            .expect("progress should exist");
        assert_eq!(progress.chapter_index, 5);
        assert_eq!(progress.scroll_position, 0.0);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn progress_get_with_no_progress_returns_null() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-2", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books/prog-2/progress"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body.is_null(), "expected null progress, got {body:?}");

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn progress_unknown_book_returns_404() {
        let state = test_state();
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/does-not-exist/progress"
            ))
            .json(&serde_json::json!({"chapter_index": 0, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/does-not-exist/progress"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn progress_put_malformed_body_returns_400() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-3", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Negative index doesn't fit the u32 field — rejected at deserialization.
        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-3/progress"))
            .json(&serde_json::json!({"chapter_index": -1, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Garbage body.
        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-3/progress"))
            .header("content-type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let _ = tx.send(());
    }

    // ── "Want to read" flag: PUT endpoint + filter param ─────────────────────

    // `books.file_path` is UNIQUE, so each seeded book needs a distinct path.
    fn want_test_book(id: &str) -> crate::models::Book {
        let mut b = progress_test_book(id, 1);
        b.file_path = format!("/nonexistent/{id}.cbz");
        b
    }

    #[tokio::test]
    async fn put_want_to_read_then_filter() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-1")).unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-2")).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/wt-1/want-to-read"
            ))
            .json(&serde_json::json!({"want_to_read": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?want_to_read=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let ids: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["wt-1"]);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn put_want_to_read_malformed_400_and_missing_404() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-3")).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/wt-3/want-to-read"
            ))
            .header("content-type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/nope/want-to-read"
            ))
            .json(&serde_json::json!({"want_to_read": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn put_want_to_read_requires_auth_when_pin_set() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1234"));
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-4")).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/wt-4/want-to-read"
            ))
            .json(&serde_json::json!({"want_to_read": true}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn want_to_read_filter_composes_with_series_and_q() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            // Two books in series "S"; one also matches a title search.
            let mut s1 = want_test_book("wt-s1");
            s1.series = Some("S".to_string());
            s1.title = "Alpha".to_string();
            let mut s2 = want_test_book("wt-s2");
            s2.series = Some("S".to_string());
            s2.title = "Beta".to_string();
            // A flagged book NOT in series "S" — must be excluded by the series filter.
            let mut other = want_test_book("wt-other");
            other.series = Some("T".to_string());
            crate::db::insert_book(&conn, &s1).unwrap();
            crate::db::insert_book(&conn, &s2).unwrap();
            crate::db::insert_book(&conn, &other).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Flag wt-s1 (series S) and wt-other (series T).
        for id in ["wt-s1", "wt-other"] {
            let resp = client
                .put(format!(
                    "http://127.0.0.1:{port}/api/books/{id}/want-to-read"
                ))
                .json(&serde_json::json!({"want_to_read": true}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }

        // want_to_read ∩ series=S → only wt-s1 (wt-other flagged but wrong series;
        // wt-s2 in series but not flagged).
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?want_to_read=true&series=S"
            ))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let ids: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["wt-s1"]);

        // want_to_read ∩ q=Beta → empty (wt-s2 matches q but isn't flagged).
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?want_to_read=true&q=Beta"
            ))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body.as_array().unwrap().is_empty(),
            "want_to_read ∩ q=Beta must be empty, got {body:?}"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn want_to_read_filter_is_lenient_and_paginates() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-a")).unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-b")).unwrap();
            crate::db::insert_book(&conn, &want_test_book("wt-c")).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Flag two of the three books.
        for id in ["wt-a", "wt-b"] {
            let resp = client
                .put(format!(
                    "http://127.0.0.1:{port}/api/books/{id}/want-to-read"
                ))
                .json(&serde_json::json!({"want_to_read": true}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }

        // Malformed / empty / non-"true" values must NOT 400 the listing — they
        // leave the filter off and return the full library. Regression guard:
        // an `Option<bool>` field made axum's Query extraction 400 the whole
        // catalog on these inputs.
        for q in ["want_to_read=", "want_to_read=1", "want_to_read=false"] {
            let resp = client
                .get(format!("http://127.0.0.1:{port}/api/books?{q}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "query `{q}` must not 400");
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(
                body.as_array().unwrap().len(),
                3,
                "query `{q}` leaves the filter off (full library)"
            );
        }

        // `want_to_read=true` composes with pagination: X-Total-Count reflects
        // the filtered set (2), and `limit` slices it.
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?want_to_read=true&limit=1"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "2",
            "total reflects the flagged set, not the whole library"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body.as_array().unwrap().len(),
            1,
            "limit slices the flagged set"
        );

        let _ = tx.send(());
    }

    // F4: `total_chapters` can be stale relative to the reader's live
    // /page-count (e.g. re-paginated PDF/CBZ). Rejecting indices beyond the
    // stored total made saves beyond that stale bound silently fail. The web
    // PUT now accepts any non-negative index and stores it as-is; the client
    // clamps when reading progress back.
    #[tokio::test]
    async fn progress_put_chapter_index_beyond_total_is_accepted() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-4", 10)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-4/progress"))
            .json(&serde_json::json!({"chapter_index": 10, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let conn = state.conn().unwrap();
        let progress = crate::db::get_reading_progress(&conn, "prog-4")
            .unwrap()
            .expect("progress should be stored even though it exceeds total_chapters");
        assert_eq!(progress.chapter_index, 10);

        let _ = tx.send(());
    }

    // F1: a web-driven completion (PUT landing on the last chapter) must
    // perform the same activity-log side effect the desktop
    // `save_reading_progress` command performs, and must not fire twice.
    #[tokio::test]
    async fn progress_put_last_chapter_logs_completion_activity() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-6", 5)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Land on the last chapter (index 4 of 5).
        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-6/progress"))
            .json(&serde_json::json!({"chapter_index": 4, "scroll_position": 0.5}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        {
            let conn = state.conn().unwrap();
            let activity = crate::db::get_all_activity(&conn).unwrap();
            let completions: Vec<_> = activity
                .iter()
                .filter(|a| {
                    a.action == "book_completed" && a.entity_id.as_deref() == Some("prog-6")
                })
                .collect();
            assert_eq!(
                completions.len(),
                1,
                "expected exactly one completion activity entry, got {activity:?}"
            );
        }

        // A second save that stays on the last chapter (e.g. a scroll-only
        // update) must not log a second completion.
        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-6/progress"))
            .json(&serde_json::json!({"chapter_index": 4, "scroll_position": 0.9}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let conn = state.conn().unwrap();
        let activity = crate::db::get_all_activity(&conn).unwrap();
        let completions: Vec<_> = activity
            .iter()
            .filter(|a| a.action == "book_completed" && a.entity_id.as_deref() == Some("prog-6"))
            .collect();
        assert_eq!(
            completions.len(),
            1,
            "completion must not be logged twice for repeat saves on the last chapter"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn progress_put_requires_auth_when_pin_configured() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("4321"));
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("prog-5", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Unauthenticated PUT is rejected.
        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-5/progress"))
            .json(&serde_json::json!({"chapter_index": 3, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // Log in, then retry authenticated (bearer token — validated by the
        // same `validate_session` path as the cookie the browser sends).
        let login_resp = client
            .post(format!("http://127.0.0.1:{port}/api/auth"))
            .json(&serde_json::json!({"pin": "4321"}))
            .send()
            .await
            .unwrap();
        let login_body: serde_json::Value = login_resp.json().await.unwrap();
        let token = login_body["token"].as_str().unwrap();

        let resp = client
            .put(format!("http://127.0.0.1:{port}/api/books/prog-5/progress"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"chapter_index": 3, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let _ = tx.send(());
    }

    // ── Bookmarks ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn bookmark_crud_round_trip() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-book-1", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}/api/books/bm-book-1/bookmarks");

        // create
        let resp = client
            .post(&base)
            .json(&serde_json::json!({"chapter_index": 1, "scroll_position": 0.5}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let created: serde_json::Value = resp.json().await.unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["chapter_index"], 1);
        assert_eq!(created["name"], serde_json::Value::Null);

        // list
        let resp = client.get(&base).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let list: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);

        // rename
        let resp = client
            .put(format!("{base}/{id}"))
            .json(&serde_json::json!({"name": "My spot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let renamed: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(renamed["name"], "My spot");

        // delete
        let resp = client.delete(format!("{base}/{id}")).send().await.unwrap();
        assert_eq!(resp.status(), 204);
        let resp = client.get(&base).send().await.unwrap();
        let list: serde_json::Value = resp.json().await.unwrap();
        assert!(list.as_array().unwrap().is_empty());
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_malformed_body_is_400() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-book-2", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://127.0.0.1:{port}/api/books/bm-book-2/bookmarks"
            ))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_unknown_book_is_404() {
        let state = test_state();
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books/ghost/bookmarks"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/books/ghost/bookmarks"))
            .json(&serde_json::json!({"chapter_index": 0, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_cross_book_mutation_is_rejected() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-A", 50)).unwrap();
            // Distinct file_path — books.file_path is UNIQUE (see cr_test_book).
            crate::db::insert_book(
                &conn,
                &crate::models::Book {
                    file_path: "/nonexistent/bm-B.cbz".to_string(),
                    ..progress_test_book("bm-B", 50)
                },
            )
            .unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        // Create under A.
        let created: serde_json::Value = client
            .post(format!("http://127.0.0.1:{port}/api/books/bm-A/bookmarks"))
            .json(&serde_json::json!({"chapter_index": 0, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        // Delete via B's URL: idempotent 204, A's row survives.
        let resp = client
            .delete(format!(
                "http://127.0.0.1:{port}/api/books/bm-B/bookmarks/{id}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        {
            let conn = state.conn().unwrap();
            assert_eq!(crate::db::list_bookmarks(&conn, "bm-A").unwrap().len(), 1);
        }
        // Rename via B's URL: 404.
        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/bm-B/bookmarks/{id}"
            ))
            .json(&serde_json::json!({"name": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_rename_empty_clears_and_long_truncates() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-book-3", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!(
                "http://127.0.0.1:{port}/api/books/bm-book-3/bookmarks"
            ))
            .json(&serde_json::json!({"chapter_index": 0, "scroll_position": 0.0}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}/api/books/bm-book-3/bookmarks/{id}");
        // 150-char name → truncated to 100.
        let renamed: serde_json::Value = client
            .put(&base)
            .json(&serde_json::json!({"name": "a".repeat(150)}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(renamed["name"].as_str().unwrap().chars().count(), 100);
        // whitespace-only → cleared.
        let cleared: serde_json::Value = client
            .put(&base)
            .json(&serde_json::json!({"name": "   "}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cleared["name"], serde_json::Value::Null);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_persists_in_private_mode() {
        let state = test_state();
        state
            .private_mode
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-priv", 50)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://127.0.0.1:{port}/api/books/bm-priv/bookmarks"
            ))
            .json(&serde_json::json!({"chapter_index": 2, "scroll_position": 0.1}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        // Persisted despite private mode (explicit action; desktop parity).
        let conn = state.conn().unwrap();
        assert_eq!(
            crate::db::list_bookmarks(&conn, "bm-priv").unwrap().len(),
            1
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn bookmark_routes_require_auth_when_pin_configured() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("4321"));
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("bm-auth", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/bm-auth/bookmarks"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = tx.send(());
    }

    // ── Highlights (spec 2026-07-23-web-highlighting-design.md §1) ──────────

    #[tokio::test]
    async fn highlight_create_and_list_roundtrip() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-1", 50)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/books/hl-1/highlights"))
            .json(&serde_json::json!({
                "chapterIndex": 2, "text": "some quoted words",
                "color": "#7bc47f", "startOffset": 100, "endOffset": 117
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let created: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(created["color"], "#7bc47f");
        assert_eq!(created["chapterIndex"], 2);
        assert_eq!(created["note"], serde_json::Value::Null);
        assert!(created["id"].as_str().unwrap().len() > 10);

        let listed: serde_json::Value = client
            .get(format!("http://127.0.0.1:{port}/api/books/hl-1/highlights"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["startOffset"], 100);
        // DB-side check
        let conn = state.conn().unwrap();
        assert_eq!(crate::db::list_highlights(&conn, "hl-1").unwrap().len(), 1);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn highlight_create_validation_rejects_bad_input() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-2", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/api/books/hl-2/highlights");
        let cases = [
            // non-allowlisted color
            serde_json::json!({"chapterIndex":0,"text":"abc","color":"#123456","startOffset":0,"endOffset":3}),
            // end <= start
            serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":5,"endOffset":5}),
            // empty text after trim
            serde_json::json!({"chapterIndex":0,"text":"   ","color":"#f6c445","startOffset":0,"endOffset":3}),
            // over-long note
            serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":0,"endOffset":3,"note":"x".repeat(2001)}),
        ];
        for body in cases {
            let resp = client.post(&url).json(&body).send().await.unwrap();
            assert_eq!(resp.status(), 400, "body: {body}");
        }
        // unknown book → 404
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/books/ghost/highlights"))
            .json(&serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":0,"endOffset":3}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn highlight_persists_in_private_mode() {
        let state = test_state();
        state
            .private_mode
            .store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-priv", 50)).unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://127.0.0.1:{port}/api/books/hl-priv/highlights"
            ))
            .json(&serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":0,"endOffset":3}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let conn = state.conn().unwrap();
        assert_eq!(
            crate::db::list_highlights(&conn, "hl-priv").unwrap().len(),
            1
        );
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn highlight_routes_require_auth_when_pin_configured() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("4321"));
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-auth", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/hl-auth/highlights"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn highlight_put_presence_aware_note_and_color() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-put", 50)).unwrap();
        }
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("http://127.0.0.1:{port}/api/books/hl-put/highlights"))
            .json(&serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":0,"endOffset":3}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let hid = created["id"].as_str().unwrap().to_string();
        let base = format!("http://127.0.0.1:{port}/api/books/hl-put/highlights/{hid}");

        // set note; color untouched; returns full Highlight
        let up: serde_json::Value = client
            .put(&base)
            .json(&serde_json::json!({"note": "my note"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(up["note"], "my note");
        assert_eq!(up["color"], "#f6c445");

        // note ABSENT (color-only) — note must survive
        let up: serde_json::Value = client
            .put(&base)
            .json(&serde_json::json!({"color": "#e88baf"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(up["note"], "my note");
        assert_eq!(up["color"], "#e88baf");

        // explicit null clears the note
        let up: serde_json::Value = client
            .put(&base)
            .json(&serde_json::json!({"note": serde_json::Value::Null}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(up["note"], serde_json::Value::Null);

        // empty body → 400
        let resp = client
            .put(&base)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // bad color → 400
        let resp = client
            .put(&base)
            .json(&serde_json::json!({"color":"red"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        // wrong type for note → 400
        let resp = client
            .put(&base)
            .json(&serde_json::json!({"note": 5}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn highlight_delete_idempotent_and_scoped() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &progress_test_book("hl-A", 50)).unwrap();
            // Distinct file_path — books.file_path is UNIQUE (see cr_test_book).
            crate::db::insert_book(
                &conn,
                &crate::models::Book {
                    file_path: "/nonexistent/hl-B.cbz".to_string(),
                    ..progress_test_book("hl-B", 50)
                },
            )
            .unwrap();
        }
        let (state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("http://127.0.0.1:{port}/api/books/hl-A/highlights"))
            .json(&serde_json::json!({"chapterIndex":0,"text":"abc","color":"#f6c445","startOffset":0,"endOffset":3}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let hid = created["id"].as_str().unwrap().to_string();

        // delete via the WRONG book: idempotent 204, A's row survives
        let resp = client
            .delete(format!(
                "http://127.0.0.1:{port}/api/books/hl-B/highlights/{hid}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        {
            let conn = state.conn().unwrap();
            assert_eq!(crate::db::list_highlights(&conn, "hl-A").unwrap().len(), 1);
        }
        // PUT via the wrong book → 404
        let resp = client
            .put(format!(
                "http://127.0.0.1:{port}/api/books/hl-B/highlights/{hid}"
            ))
            .json(&serde_json::json!({"note":"x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // right book: 204, then repeat-delete still 204; PUT on deleted → 404
        let base = format!("http://127.0.0.1:{port}/api/books/hl-A/highlights/{hid}");
        assert_eq!(client.delete(&base).send().await.unwrap().status(), 204);
        assert_eq!(client.delete(&base).send().await.unwrap().status(), 204);
        let resp = client
            .put(&base)
            .json(&serde_json::json!({"note":"x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = tx.send(());
    }

    // ── Item 5: Continue Reading shelf ──────────────────────────────────────

    /// `progress_test_book` reuses one fixed `file_path` for every call — fine
    /// when a test inserts a single book, but the `books.file_path` unique
    /// constraint rejects a second one. These tests insert several, so give
    /// each a distinct path.
    fn cr_test_book(id: &str, total_chapters: u32) -> crate::models::Book {
        crate::models::Book {
            file_path: format!("/nonexistent/{id}.cbz"),
            ..progress_test_book(id, total_chapters)
        }
    }

    // Finding J: these HTTP-layer tests cover route concerns only —
    // registration/status, the JSON shape returned over the wire, limit-param
    // parsing/capping, and auth gating. The underlying SQL filter/order/
    // exclusion logic (unread vs. finished vs. in-progress, most-recent-first
    // ordering, total_chapters=0 exclusion) is exercised exhaustively by
    // `db::tests::test_get_continue_reading_books_*` and must not be
    // duplicated here.

    #[tokio::test]
    async fn continue_reading_returns_json_shape_for_one_item() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cr_test_book("cr-shape", 10)).unwrap();
            crate::db::upsert_reading_progress(
                &conn,
                &crate::models::ReadingProgress {
                    book_id: "cr-shape".to_string(),
                    chapter_index: 5,
                    scroll_position: 0.4,
                    last_read_at: 400,
                },
            )
            .unwrap();
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/continue-reading"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "route returns the JSON array shape over HTTP");
        assert_eq!(arr[0]["id"], "cr-shape");
        assert_eq!(arr[0]["chapter_index"], 5);
        assert_eq!(arr[0]["total_chapters"], 10);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn continue_reading_respects_limit_param() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for i in 0..5 {
                let id = format!("cr-limit-{i}");
                crate::db::insert_book(&conn, &cr_test_book(&id, 10)).unwrap();
                crate::db::upsert_reading_progress(
                    &conn,
                    &crate::models::ReadingProgress {
                        book_id: id,
                        chapter_index: 3,
                        scroll_position: 0.2,
                        last_read_at: 1000 + i,
                    },
                )
                .unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/continue-reading?limit=2"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "limit param must cap the result count");

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn continue_reading_limit_param_caps_at_50() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for i in 0..55 {
                let id = format!("cr-cap-{i}");
                crate::db::insert_book(&conn, &cr_test_book(&id, 10)).unwrap();
                crate::db::upsert_reading_progress(
                    &conn,
                    &crate::models::ReadingProgress {
                        book_id: id,
                        chapter_index: 3,
                        scroll_position: 0.1,
                        last_read_at: 1000 + i,
                    },
                )
                .unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/continue-reading?limit=1000"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body.as_array().unwrap().len(),
            50,
            "limit param must be capped at 50 regardless of the requested value"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn continue_reading_requires_auth_when_pin_configured() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("9999"));

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books/continue-reading"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let _ = tx.send(());
    }

    // ── Item 15: bulk reading-progress endpoint (grid progress badges) ──────
    //
    // `GET /api/reading-progress` is a thin wrapper over the existing
    // `db::get_all_reading_progress` (already used internally for the
    // `last_read` sort) — no new query, no `BookGridItem` model change. It's
    // PIN-gated like the other `/api/books*` reads (not a public shell
    // asset), so no auth.rs carve-out entry is needed.

    #[tokio::test]
    async fn reading_progress_returns_empty_for_fresh_db() {
        let state = test_state();
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/reading-progress"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body.as_array().unwrap().len(), 0);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn reading_progress_returns_rows_with_progress() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            crate::db::insert_book(&conn, &cr_test_book("rp-1", 10)).unwrap();
            crate::db::insert_book(&conn, &cr_test_book("rp-2", 10)).unwrap();
            crate::db::upsert_reading_progress(
                &conn,
                &crate::models::ReadingProgress {
                    book_id: "rp-1".to_string(),
                    chapter_index: 3,
                    scroll_position: 0.5,
                    last_read_at: 123,
                },
            )
            .unwrap();
            // rp-2 has no progress row — must not appear in the response.
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/reading-progress"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only books with a progress row are returned");
        assert_eq!(arr[0]["book_id"], "rp-1");
        assert_eq!(arr[0]["chapter_index"], 3);
        assert_eq!(arr[0]["scroll_position"], 0.5);
        assert_eq!(arr[0]["last_read_at"], 123);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn reading_progress_requires_auth_when_pin_configured() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("9999"));

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/reading-progress"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        let _ = tx.send(());
    }

    // ── Item 14: paginate the library grid (infinite scroll) ────────────────
    //
    // `list_books` stays backward-compatible: `limit`/`offset` are optional
    // and only change behavior when `limit` is present. Pagination is applied
    // strictly after the existing in-memory filter+sort pipeline, so a slice
    // is the only difference from the pre-pagination response — total via the
    // `X-Total-Count` header, body stays a bare array (Decisions locked in
    // docs/web-ui-improvements.md Item 14).

    fn pagination_test_book(id: &str, title: &str, added_at: i64) -> crate::models::Book {
        crate::models::Book {
            title: title.to_string(),
            added_at,
            ..cr_test_book(id, 10)
        }
    }

    #[tokio::test]
    async fn list_books_limit_and_offset_returns_slice_and_total_count_header() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for i in 0..5 {
                crate::db::insert_book(
                    &conn,
                    &pagination_test_book(&format!("pg-{i}"), &format!("Book {i}"), 1000 + i),
                )
                .unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Default sort is date_added DESC, so page 0 is the two most-recently
        // added books (pg-4, pg-3), page 1 the next two (pg-2, pg-1) — no
        // overlap and no gap across the boundary.
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?limit=2&offset=0"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "5"
        );
        let page0: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(page0.len(), 2);
        assert_eq!(page0[0]["id"], "pg-4");
        assert_eq!(page0[1]["id"], "pg-3");

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?limit=2&offset=2"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "5"
        );
        let page1: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0]["id"], "pg-2");
        assert_eq!(page1[1]["id"], "pg-1");

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn list_books_offset_past_end_returns_empty_with_correct_total() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for i in 0..3 {
                crate::db::insert_book(
                    &conn,
                    &pagination_test_book(&format!("pgpe-{i}"), &format!("Book {i}"), 1000 + i),
                )
                .unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?limit=10&offset=100"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "offset past the end must not 500");
        assert_eq!(
            resp.headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "3"
        );
        let body: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(body.is_empty());

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn list_books_without_limit_returns_full_list_unchanged() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for i in 0..7 {
                crate::db::insert_book(
                    &conn,
                    &pagination_test_book(&format!("pgfull-{i}"), &format!("Book {i}"), 1000 + i),
                )
                .unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        // Backward-compat guard: omitting `limit` must return every book,
        // exactly as it did before pagination existed — OPDS/desktop and any
        // other caller of this endpoint never send `limit`.
        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(body.len(), 7);

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn list_books_limit_composes_with_filter_and_sort() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            let mut charlie = pagination_test_book("pgfs-1", "Charlie", 1001);
            charlie.series = Some("Wanted".to_string());
            crate::db::insert_book(&conn, &charlie).unwrap();
            let mut alpha = pagination_test_book("pgfs-2", "Alpha", 1002);
            alpha.series = Some("Wanted".to_string());
            crate::db::insert_book(&conn, &alpha).unwrap();
            let mut bravo = pagination_test_book("pgfs-3", "Bravo", 1003);
            bravo.series = Some("Wanted".to_string());
            crate::db::insert_book(&conn, &bravo).unwrap();
            // A fourth book in a different series must be excluded from both
            // the slice and the total.
            let mut other = pagination_test_book("pgfs-4", "Zulu", 1004);
            other.series = Some("Other".to_string());
            crate::db::insert_book(&conn, &other).unwrap();
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/api/books?series=Wanted&sort=title&limit=1&offset=0"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "3",
            "total must reflect the filtered set, not the whole table"
        );
        let page: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(
            page[0]["id"], "pgfs-2",
            "page 0 of sort=title must start at the alphabetically-first title \
             within the filtered series — slice is taken after sort"
        );

        let _ = tx.send(());
    }

    // Fix D: `added_at` only has second-granularity, so concurrent/batch
    // imports can tie — without a unique tiebreaker (`id`) in the SQL
    // ORDER BY, offset pagination isn't guaranteed stable across requests
    // and a tied book could land on two pages or be skipped entirely.
    #[tokio::test]
    async fn list_books_tied_added_at_paginates_without_dup_or_skip() {
        let state = test_state();
        {
            let conn = state.conn().unwrap();
            for id in ["tie-c", "tie-a", "tie-b"] {
                crate::db::insert_book(&conn, &pagination_test_book(id, id, 5000)).unwrap();
            }
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let mut seen = Vec::new();
        for offset in 0..3 {
            let resp = client
                .get(format!(
                    "http://127.0.0.1:{port}/api/books?limit=1&offset={offset}"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let page: Vec<serde_json::Value> = resp.json().await.unwrap();
            assert_eq!(page.len(), 1);
            seen.push(page[0]["id"].as_str().unwrap().to_string());
        }

        seen.sort();
        assert_eq!(
            seen,
            vec!["tie-a", "tie-b", "tie-c"],
            "tied added_at must still paginate deterministically — no book \
             duplicated across pages or skipped entirely"
        );

        let _ = tx.send(());
    }

    // ── Item 8: richer book detail (file_size) ──────────────────────────────

    #[tokio::test]
    async fn get_book_detail_includes_file_size() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("detail-test.cbz");
        std::fs::write(&file_path, b"0123456789").unwrap(); // 10 bytes
        {
            let conn = state.conn().unwrap();
            let mut book = progress_test_book("detail-1", 10);
            book.file_path = file_path.to_string_lossy().to_string();
            crate::db::insert_book(&conn, &book).unwrap();
        }

        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}/api/books/detail-1"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["file_size"], 10);
        assert_eq!(body["id"], "detail-1");
        // The rest of the `Book` shape must still be present alongside it.
        assert_eq!(body["total_chapters"], 10);

        let _ = tx.send(());
    }

    // ── Item 9: PWA shell (manifest/sw/icons) ───────────────────────────────
    // Each new static route must be reachable WITHOUT auth even when a PIN is
    // configured (auth.rs's public carve-out) — otherwise a PIN-protected
    // setup would 401 the install/offline shell before the user ever logs in.

    async fn assert_public_route_ok(path: &str, expected_content_type: &str) {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("2468"));
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "{path} should be public (200) even with a PIN configured"
        );
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with(expected_content_type),
            "{path} content-type was {content_type:?}, expected to start with {expected_content_type:?}"
        );

        let _ = tx.send(());
    }

    #[tokio::test]
    async fn manifest_json_is_public_and_correct_content_type() {
        assert_public_route_ok("/manifest.json", "application/manifest+json").await;
    }

    #[tokio::test]
    async fn sw_js_is_public_and_correct_content_type() {
        assert_public_route_ok("/sw.js", "application/javascript").await;
    }

    #[tokio::test]
    async fn icon_192_is_public_and_correct_content_type() {
        assert_public_route_ok("/icon-192.png", "image/png").await;
    }

    #[tokio::test]
    async fn icon_512_is_public_and_correct_content_type() {
        assert_public_route_ok("/icon-512.png", "image/png").await;
    }

    // Finding 9: a manual CACHE_VERSION bump enforced only by a code-comment
    // reminder means a changed app.js/app.css/index.html/manifest.json can
    // ship without invalidating browsers' already-installed SW caches,
    // serving the stale shell forever. This computes a short content hash
    // over the concatenated shell assets — independently of sw.js — and
    // asserts CACHE_VERSION embeds it, so any future asset edit without
    // regenerating the version fails here with the expected hash to paste
    // in, the same pattern `test_csp_allows_theme_bootstrap_script_hash`
    // (web_server::tests, this file) already uses for the CSP hash.
    /// The offline scope marker is the SW's only channel for the active
    /// profile's namespace on a cold start, so app.js and sw.js must agree on
    /// where it lives — a silent drift would make the SW serve (or refuse to
    /// serve) the wrong profile's saved books. Neither file can import the
    /// other, so the constants are checked here.
    #[test]
    fn offline_scope_constants_agree_between_app_js_and_sw_js() {
        const APP_JS: &str = include_str!("static/app.js");
        const SW_JS: &str = include_str!("static/sw.js");

        for decl in [
            r#"const OFFLINE_SCOPE_CACHE = "carrel-offline-scope";"#,
            r#"const OFFLINE_SCOPE_URL = "/__offline_scope";"#,
            r#"const OFFLINE_CACHE_PREFIX = "carrel-offline-book-";"#,
        ] {
            assert!(APP_JS.contains(decl), "app.js must declare `{decl}`");
            assert!(SW_JS.contains(decl), "sw.js must declare `{decl}`");
        }

        // The activate purge deletes every cache it doesn't recognize; the
        // scope marker must be on its keep-list, or a SW update would silently
        // reset every client to the default profile's namespace.
        assert!(
            SW_JS.contains("key !== OFFLINE_SCOPE_CACHE"),
            "sw.js's activate purge must spare the offline scope marker cache"
        );
    }

    #[tokio::test]
    async fn cache_version_embeds_shell_asset_content_hash() {
        use sha2::{Digest, Sha256};

        const INDEX_HTML: &str = include_str!("static/index.html");
        const APP_JS: &str = include_str!("static/app.js");
        const APP_CSS: &str = include_str!("static/app.css");
        const MANIFEST_JSON: &str = include_str!("static/manifest.json");
        const SW_JS: &str = include_str!("static/sw.js");

        let mut hasher = Sha256::new();
        hasher.update(INDEX_HTML.as_bytes());
        hasher.update(APP_JS.as_bytes());
        hasher.update(APP_CSS.as_bytes());
        hasher.update(MANIFEST_JSON.as_bytes());
        // Fold the embedded reader-font bytes too, so swapping a font forces a
        // CACHE_VERSION bump (the SW precaches these; stale caches would 404).
        for (_p, bytes) in web_ui::FONT_ASSETS {
            hasher.update(bytes);
        }
        let digest = format!("{:x}", hasher.finalize());
        let expected_fragment = &digest[..12];

        let cache_version_line = SW_JS
            .lines()
            .find(|l| l.trim_start().starts_with("const CACHE_VERSION"))
            .expect("sw.js must define a CACHE_VERSION so shell-asset changes can invalidate old caches");

        assert!(
            cache_version_line.contains(expected_fragment),
            "sw.js's CACHE_VERSION is stale relative to the current shell asset content \
             (index.html + app.js + app.css + manifest.json). Update it to embed \
             {expected_fragment:?}, e.g. CACHE_VERSION = \"carrel-shell-{expected_fragment}\"; \
             found: {cache_version_line}"
        );
    }

    // Item C (app-feel Tier 1): the reader's full-viewport surfaces must use
    // dynamic-viewport units (dvh) so mobile browser toolbars expanding/
    // collapsing don't clip the reader (bare 100vh is the pre-Item-C tell).
    // Computed style resolves dvh->px so a headless-browser check can't see
    // the unit; this guards the CSS source directly against a silent revert.
    #[test]
    fn reader_full_viewport_uses_dynamic_viewport_height() {
        const APP_CSS: &str = include_str!("static/app.css");

        // Guard each reader surface that filled the viewport *within its own
        // rule*: a file-wide `contains("100dvh")` would pass on the body/login
        // declarations even if a reader rule regressed, and an exact-substring
        // negative is evaded by any reformat. Requiring `100vh` before `100dvh`
        // in the specific block catches a reordered/relocated revert too.
        for selector in [".reader-page, .reader-chapter", ".reader-skeleton"] {
            let block = css_block(APP_CSS, selector)
                .unwrap_or_else(|| panic!("app.css must contain a `{selector}` rule"));
            let vh = block.find("100vh");
            let dvh = block.find("100dvh");
            assert!(
                matches!((vh, dvh), (Some(v), Some(d)) if v < d),
                "the `{selector}` rule must size to 100dvh with a preceding 100vh \
                 fallback (Item C) — found vh={vh:?} dvh={dvh:?} in block:{block}"
            );
        }
    }

    // Item B (app-feel Tier 1): edge-to-edge standalone chrome must respect the
    // notch / status bar / home indicator. That needs viewport-fit=cover + a
    // black-translucent status bar in index.html and env(safe-area-inset-*)
    // padding in app.css. `env()` resolves to 0 in a non-notched headless
    // browser, so a runtime check can't observe the insets; these guard the
    // source directly (the same approach as the dvh guard above).
    #[test]
    fn standalone_chrome_declares_safe_area_and_edge_to_edge() {
        const INDEX_HTML: &str = include_str!("static/index.html");
        const APP_CSS: &str = include_str!("static/app.css");

        assert!(
            INDEX_HTML.contains("viewport-fit=cover"),
            "index.html's viewport meta must include viewport-fit=cover (Item B) \
             so content can extend into the safe-area insets"
        );
        assert!(
            INDEX_HTML.contains(r#"content="black-translucent""#),
            "index.html's apple-mobile-web-app-status-bar-style must be \
             black-translucent for an edge-to-edge status bar (Item B)"
        );
        for inset in ["env(safe-area-inset-top", "env(safe-area-inset-bottom"] {
            assert!(
                APP_CSS.contains(inset),
                "app.css must pad chrome with {inset}...) so it clears system UI \
                 in standalone mode (Item B)"
            );
        }
    }

    // Item D (app-feel Tier 2): the book-cover lift is hover feedback. On touch
    // :hover latches after a tap and the card stays stuck lifted — the "it's a
    // web page" tell. It must be gated behind @media (hover: hover) and
    // (pointer: fine) so only a hovering fine pointer (desktop mouse/trackpad)
    // sees it; a coarse finger tap never does. Pointer-capability forks don't
    // render in headless Chromium, so guard the CSS source directly (same
    // approach, and shared brace-scanner, as the Item B/C guards above).
    #[test]
    fn card_lift_is_gated_to_hovering_fine_pointers() {
        const APP_CSS: &str = include_str!("static/app.css");

        let gate = css_block(APP_CSS, "@media (hover: hover) and (pointer: fine)").expect(
            "app.css must gate the card lift behind @media (hover: hover) and (pointer: fine) (Item D)",
        );
        // Require every translateY(-2px) lift in the file to sit inside that
        // gate, so a stray ungated lift (which would re-latch on touch) fails
        // loudly rather than slipping through a file-wide `contains`.
        let total = APP_CSS.matches("translateY(-2px)").count();
        let gated = gate.matches("translateY(-2px)").count();
        assert!(
            total > 0 && total == gated,
            "every translateY(-2px) lift must be inside the hover+fine-pointer gate \
             so it never sticks after a touch tap — found {total} total, {gated} gated (Item D)"
        );
    }

    // Item F (app-feel Tier 2): chrome suppresses the iOS long-press callout
    // and stray selection, while reading content keeps selection. The e2e spec
    // asserts `user-select: none` on the chrome (computed style), but
    // `-webkit-touch-callout` is Safari-only and not exposed in headless
    // Chromium, and the seed books have no description / EPUB chapter to select
    // there — so guard both the callout suppression and the content re-enable
    // at the CSS source.
    #[test]
    fn chrome_suppresses_callout_and_content_keeps_selection() {
        const APP_CSS: &str = include_str!("static/app.css");

        // The chrome rule must turn off the long-press callout and selection.
        let chrome = css_block(APP_CSS, ".header, .tab-bar, .card")
            .expect("app.css must have a chrome rule starting `.header, .tab-bar, .card` (Item F)");
        assert!(
            chrome.contains("-webkit-touch-callout: none"),
            "chrome must suppress the iOS long-press callout (Item F)"
        );
        assert!(
            chrome.contains("user-select: none"),
            "chrome must suppress stray text selection (Item F)"
        );

        // Reading content must re-enable selection so quotes/notes still work.
        let content = css_block(APP_CSS, ".reader-chapter .content, .detail-description").expect(
            "app.css must re-enable selection on `.reader-chapter .content, .detail-description` (Item F)",
        );
        assert!(
            content.contains("user-select: text"),
            "chapter text and the book description must keep text selection (Item F)"
        );
    }

    // Finding 11: PUBLIC_SHELL_ASSETS is the single source of truth shared by
    // web_ui::routes() and auth::auth_middleware's carve-out — this walks the
    // list end-to-end against a live, PIN-protected server, so a path added
    // to one but not the other (or simply mistyped) fails loudly here rather
    // than as a silent 401 on someone's PWA install screen.
    #[tokio::test]
    async fn all_public_shell_assets_are_reachable_without_auth() {
        let state = test_state();
        *state.pin_hash.lock().unwrap() = Some(auth::hash_pin("1357"));
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();

        for path in web_ui::PUBLIC_SHELL_ASSETS {
            let resp = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "{path} is listed in PUBLIC_SHELL_ASSETS but is not publicly reachable"
            );
        }

        let _ = tx.send(());
    }

    // ── Web reader typography fonts ─────────────────────────────────────────
    // The four reading faces are embedded, content-addressed woff2 served as
    // public shell assets and precached by sw.js. If a font path is missing
    // from either public list, sw.js's atomic `cache.addAll()` rejects and the
    // service worker never installs — so both lists are asserted, driven off
    // the single FONT_ASSETS table so they cannot silently drift.
    #[test]
    fn font_assets_are_public_and_precached() {
        let sw = include_str!("static/sw.js");
        // Scope the SW check to the SHELL_ASSETS array body, not the whole
        // file, so a path appearing only in a comment/OFFLINE list can't
        // false-pass.
        // Scope to the `const SHELL_ASSETS = [ ... ]` array body specifically
        // (not any comment that merely mentions SHELL_ASSETS/PUBLIC_SHELL_ASSETS)
        // so a path in a comment or the OFFLINE list can't false-pass.
        let shell = sw
            .split("const SHELL_ASSETS")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("sw.js must define `const SHELL_ASSETS = [ ... ]`");
        for (path, _bytes) in web_ui::FONT_ASSETS {
            assert!(
                web_ui::PUBLIC_SHELL_ASSETS.contains(path),
                "{path} missing from PUBLIC_SHELL_ASSETS"
            );
            assert!(
                shell.contains(&format!("\"{path}\"")),
                "{path} missing from sw.js SHELL_ASSETS array"
            );
        }
        assert!(!web_ui::FONT_ASSETS.is_empty(), "no fonts registered");

        // Reverse direction: no STALE font entry may linger in either public
        // list after a font is renamed/removed from FONT_ASSETS. A leftover
        // "/fonts/..." path that no longer resolves would 404 and break the
        // atomic core install (PUBLIC_SHELL_ASSETS) or the precache add.
        let font_paths: HashSet<&str> = web_ui::FONT_ASSETS.iter().map(|(p, _)| *p).collect();
        for path in web_ui::PUBLIC_SHELL_ASSETS {
            if path.starts_with("/fonts/") {
                assert!(
                    font_paths.contains(path),
                    "{path} is a stale font entry in PUBLIC_SHELL_ASSETS (not in FONT_ASSETS)"
                );
            }
        }
        // Split on either quote char so a single-quoted entry can't slip a
        // stale font path past the reverse check.
        for token in shell.split(['"', '\'']) {
            if token.starts_with("/fonts/") {
                assert!(
                    font_paths.contains(token),
                    "{token} is a stale font entry in sw.js SHELL_ASSETS (not in FONT_ASSETS)"
                );
            }
        }
    }

    #[tokio::test]
    async fn font_routes_public_under_pin() {
        for (path, _b) in web_ui::FONT_ASSETS {
            assert_public_route_ok(path, "font/woff2").await;
        }
    }

    #[tokio::test]
    async fn font_routes_public_under_locked_profile() {
        // profile_lock_gate 503s when active_profile_name is NOT in
        // unlocked_profiles, EXCEPT for PUBLIC_SHELL_ASSETS. test_state() seeds
        // unlocked_profiles = {"default"}; set the active profile to one that
        // is NOT unlocked to simulate "locked", then assert fonts still 200.
        let state = test_state();
        *state.active_profile_name.lock().unwrap() = "locked-acct".to_string();
        // (do NOT add "locked-acct" to unlocked_profiles)
        let (_state, port, tx) = spawn_progress_test_server(state).await;
        let client = reqwest::Client::new();
        // sanity: a non-public route IS gated
        let gated = client
            .get(format!("http://127.0.0.1:{port}/api/books"))
            .send()
            .await
            .unwrap();
        assert_eq!(gated.status(), 503, "non-public route must be locked");
        for (path, _b) in web_ui::FONT_ASSETS {
            let resp = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "{path} must be public even under a locked profile"
            );
            assert_eq!(resp.headers()["content-type"], "font/woff2");
        }
        let _ = tx.send(());
    }

    // Content-addressing is the cache-safety contract for the plain-HTTP LAN
    // path (no service worker): filenames carry a 12-char sha256 prefix and are
    // served `immutable`. Assert the fragment is exactly 12 lowercase hex chars
    // AND equals sha256(bytes)[..12], so a swapped font body without a renamed
    // file is caught.
    #[test]
    fn font_filenames_are_content_addressed() {
        use sha2::{Digest, Sha256};
        for (path, bytes) in web_ui::FONT_ASSETS {
            let fname = path.rsplit('/').next().unwrap();
            let frag = fname.trim_end_matches(".woff2").rsplit('-').next().unwrap();
            assert!(
                frag.len() == 12
                    && frag
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{path}: hash fragment must be 12 lowercase hex chars, got {frag:?}"
            );
            let digest = format!("{:x}", Sha256::digest(bytes));
            assert_eq!(
                &digest[..12],
                frag,
                "{path}: filename hash != sha256 prefix"
            );
        }
    }

    #[tokio::test]
    async fn manifest_json_parses_with_required_fields() {
        const MANIFEST_JSON: &str = include_str!("static/manifest.json");
        let value: serde_json::Value =
            serde_json::from_str(MANIFEST_JSON).expect("manifest.json must be valid JSON");
        assert_eq!(value["name"], "Carrel");
        assert_eq!(value["display"], "standalone");
        assert!(value["theme_color"].is_string());
        assert!(value["background_color"].is_string());
        let icons = value["icons"].as_array().expect("icons array");
        assert!(
            icons.len() >= 2,
            "manifest should list at least the 192 and 512 icons"
        );
        let sizes: Vec<&str> = icons.iter().filter_map(|i| i["sizes"].as_str()).collect();
        assert!(sizes.contains(&"192x192"));
        assert!(sizes.contains(&"512x512"));
    }

    // ── carrel_status body sanitization (LAN hardening M1) ──────────────
    //
    // The web server is reachable by anything on the user's LAN behind a
    // PIN session. Kinds that mean "something broke internally" must not
    // hand the client SQL fragments, filesystem paths, or other internals
    // via the response body. Same for Network, whose text comes verbatim
    // from reqwest/opendal. Our own validation/lookup/rate-limit wording
    // for NotFound/PermissionDenied/InvalidInput/RateLimited is relied on
    // by clients, so it must survive unchanged.

    #[test]
    fn carrel_status_sanitizes_internal_failure_bodies() {
        let secret = "/Users/mike/Library/secret-path near column token='abc123xyz'";
        let errors: Vec<CarrelError> = vec![
            CarrelError::database(secret),
            CarrelError::io(secret),
            CarrelError::Serialization(secret.to_string()),
            CarrelError::lock_required(secret),
            CarrelError::internal(secret),
        ];
        for err in errors {
            let kind = err.kind();
            let (status, body) = carrel_status(err);
            assert_eq!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{kind} must map to 500"
            );
            assert!(
                !body.contains(secret),
                "{kind}: response body leaked internal detail: {body:?}"
            );
            assert_eq!(
                body, "Internal server error",
                "{kind}: body must be generic"
            );
        }
    }

    #[test]
    fn carrel_status_keeps_message_text_for_client_facing_kinds() {
        let (status, body) = carrel_status(CarrelError::not_found("Book file not found"));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "Book file not found");

        let (status, body) = carrel_status(CarrelError::permission("Not allowed"));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "Not allowed");

        let (status, body) = carrel_status(CarrelError::invalid("Bad input"));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "Bad input");
    }

    #[test]
    fn carrel_status_sanitizes_network_failure_bodies() {
        // Network is populated verbatim from reqwest/opendal, so its text is
        // foreign: upstream URLs, hostnames, ports, storage endpoint config.
        let err = CarrelError::network(
            "error sending request for url (https://sync.internal.example:8443/bucket/private): \
             connection refused",
        );
        let (status, body) = carrel_status(err);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            !body.contains("sync.internal.example"),
            "Network body leaked the upstream host: {body:?}"
        );
        assert_eq!(body, "Upstream request failed");
    }

    #[test]
    fn carrel_status_maps_rate_limited_to_429_and_keeps_its_message() {
        let (status, body) = carrel_status(CarrelError::RateLimited(
            "some-provider: rate limited after 3 attempts".to_string(),
        ));
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body, "some-provider: rate limited after 3 attempts");
    }
}

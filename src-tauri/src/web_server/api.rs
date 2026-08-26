use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};

use rusqlite::OptionalExtension;

use super::auth::{log_login_attempt, LoginOutcome, WebAuthMethod};
use super::{book_file_status, carrel_status, WebState};
use crate::db;
use crate::models::BookFormat;
use carrel_core::events::{self, CarrelEvent};

/// Settings keys excluded from the GDPR export. Defense-in-depth: the web PIN
/// and OPDS/backup credential secrets live in the OS keyring (not in
/// settings), but three settings DO carry sensitive data and are never
/// exported:
/// - `backup_config`: remote endpoint details / pre-keyring secret values
/// - `enrichment_providers`: per-provider config including plaintext API keys
/// - `opds_auth`: per-catalog credential metadata — no secret (that's
///   keychain-only), but `username` is the account identity used to sign in
///   (for Carrel Server, the user's account email), which is not worth
///   surfacing in a PIN-gated export reachable over the network
const EXPORT_SETTINGS_DENYLIST: &[&str] = &["backup_config", "enrichment_providers", "opds_auth"];

/// Refuse an *administrative* dump route unless a web PIN is actually
/// configured.
///
/// `auth_middleware` lets every request through when there is no PIN — an
/// acceptable posture for reading one's own library on a home LAN, and the
/// documented default. It is not acceptable for the two routes that exist to
/// hand the whole account over in one response: the settings/annotations
/// export and the authentication audit log. Neither backs a screen, so
/// requiring a PIN for them costs a no-PIN install nothing.
///
/// The criterion is deliberately "administrative dump", not "returns many
/// personal rows". Ordinary reading data the web UI renders — the grid's
/// progress badges (`GET /api/reading-progress`) and the vocabulary list
/// (`GET /api/vocabulary`) — is bulk and personal too, and is *not* gated:
/// gating it would break the no-PIN default outright, which is a different
/// (and user-visible) decision from hardening an admin endpoint.
///
/// Poisoned mutex → fail closed (500), never open access, mirroring
/// `auth_middleware` itself.
fn require_configured_pin(state: &WebState, what: &str) -> Result<(), (StatusCode, String)> {
    let has_pin = match state.pin_hash.lock() {
        Ok(guard) => guard.is_some(),
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            ))
        }
    };
    if !has_pin {
        return Err((
            StatusCode::FORBIDDEN,
            format!("{what} requires a configured web PIN."),
        ));
    }
    Ok(())
}

/// Build the full GDPR export document: the shared core metadata plus the
/// activity log and a redacted settings map.
///
/// Unlike `PublicBook`/`PublicBookGridItem`/`PublicContinueReadingItem`
/// elsewhere in this file, the `books` array here (via
/// `db::build_core_export`) keeps `file_path` and `cover_path` — this is the
/// library owner's own data dump, so the stored path is a legitimate part of
/// the record, not a leak. It's also not reachable without a web PIN
/// configured (`data_export`, below), unlike the routes those wrappers
/// guard.
fn build_gdpr_export(
    conn: &rusqlite::Connection,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let mut value = db::build_core_export(conn).map_err(carrel_status)?;

    let activity = db::get_all_activity(conn).map_err(carrel_status)?;
    let activity_val = serde_json::to_value(activity).map_err(carrel_status)?;

    let settings: serde_json::Map<String, serde_json::Value> = db::list_settings(conn)
        .map_err(carrel_status)?
        .into_iter()
        .filter(|(k, _)| !EXPORT_SETTINGS_DENYLIST.contains(&k.as_str()))
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();

    if let Some(obj) = value.as_object_mut() {
        obj.insert("activity_log".to_string(), activity_val);
        obj.insert("settings".to_string(), serde_json::Value::Object(settings));
    }
    Ok(value)
}

/// Current UTC date as `YYYYMMDD`, used for the export filenames.
fn export_datestamp() -> String {
    chrono::Utc::now().format("%Y%m%d").to_string()
}

/// Best-effort: record the export in the activity log. A failure is logged and
/// swallowed so it never fails the download (mirrors the login-audit pattern).
fn log_export_event(conn: &rusqlite::Connection) {
    use carrel_core::activity::ActivityEvent;
    let f = ActivityEvent::LibraryExported {
        detail: "GDPR data export (web)".to_string(),
    }
    .into_fields();
    let entry = crate::models::ActivityEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        action: f.action.to_string(),
        entity_type: f.entity_type.to_string(),
        entity_id: f.entity_id,
        entity_name: f.entity_name,
        detail: f.detail,
    };
    if let Err(e) = db::insert_activity(conn, &entry) {
        tracing::warn!(error = %e, "failed to log GDPR export to activity log");
    }
}

async fn data_export(State(state): State<WebState>) -> Result<Response, (StatusCode, String)> {
    use std::io::Write;

    // Defense-in-depth: `auth_middleware` lets every route through when no PIN
    // is configured. That open-access posture is acceptable for individual
    // reads, but this endpoint bulk-dumps personal data that has no other web
    // route (bookmarks, highlights, reading progress, full activity log,
    // settings). Refuse to serve it on an unauthenticated server — the GDPR
    // export requires that web auth actually be set up. Poisoned mutex → fail
    // closed (500), never open access (mirrors `auth_middleware`).
    require_configured_pin(&state, "Data export")?;

    let conn = state.conn().map_err(carrel_status)?;
    let value = build_gdpr_export(&conn)?;
    let json = serde_json::to_string_pretty(&value).map_err(carrel_status)?;

    let date = export_datestamp();
    let inner_name = format!("carrel-export-{date}.json");
    let zip_name = format!("carrel-export-{date}.zip");

    let buf = {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(&inner_name, options)
            .map_err(carrel_status)?;
        zip.write_all(json.as_bytes()).map_err(carrel_status)?;
        zip.finish().map_err(carrel_status)?.into_inner()
    };

    log_export_event(&conn);

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{zip_name}\""),
            ),
        ],
        buf,
    )
        .into_response())
}

/// Build all `/api/` routes.
pub fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/health", get(health))
        .route("/auth", axum::routing::post(login))
        .route("/books", get(list_books))
        .route("/books/continue-reading", get(continue_reading))
        .route("/books/{id}", get(get_book))
        .route("/books/{id}/cover", get(get_cover))
        .route("/books/{id}/chapters", get(get_chapters))
        .route("/books/{id}/chapters/{index}", get(get_chapter_content))
        .route(
            "/books/{id}/images/{chapter}/{filename}",
            get(get_epub_image),
        )
        .route("/books/{id}/pages/{index}", get(get_page_image))
        .route("/books/{id}/page-count", get(get_page_count))
        .route("/books/{id}/progress", get(get_progress).put(put_progress))
        .route(
            "/books/{id}/bookmarks",
            get(list_book_bookmarks).post(create_bookmark),
        )
        .route(
            "/books/{id}/bookmarks/{bookmark_id}",
            axum::routing::put(rename_bookmark).delete(delete_bookmark),
        )
        .route(
            "/books/{id}/highlights",
            get(list_book_highlights).post(create_highlight),
        )
        .route(
            "/books/{id}/highlights/{highlight_id}",
            axum::routing::put(update_highlight).delete(delete_highlight_route),
        )
        .route(
            "/books/{id}/want-to-read",
            axum::routing::put(put_want_to_read),
        )
        .route("/reading-progress", get(get_all_progress))
        .route("/books/{id}/download", get(download_book))
        // OPDS feeds emit `/download/{book_id}.{ext}` so clients using URL-
        // based extension detection can disambiguate AZW vs AZW3 (both share
        // the `application/vnd.amazon.ebook` MIME). The filename segment is
        // ignored server-side — the same handler serves the stored file.
        .route(
            "/books/{id}/download/{filename}",
            get(download_book_with_filename),
        )
        .route("/stats", get(get_stats))
        .route("/dictionary/status", get(get_dictionary_status))
        .route("/dictionary/lookup", get(lookup_dictionary_word))
        .route(
            "/vocabulary",
            get(list_vocabulary_words).post(create_vocabulary_word),
        )
        .route("/vocabulary/due", get(get_due_vocabulary_words))
        .route(
            "/vocabulary/{id}",
            axum::routing::delete(delete_vocabulary_word_route),
        )
        .route(
            "/vocabulary/{id}/review",
            axum::routing::post(review_vocabulary_word_route),
        )
        .route("/series", get(list_series))
        .route("/collections", get(list_collections))
        .route("/collections/{id}/books", get(get_collection_books))
        .route("/audit/login-history", get(login_history))
        .route("/data-export", get(data_export))
        .route("/profiles", get(list_profiles))
        .route("/profile", axum::routing::post(switch_profile))
        .with_state(state)
}

// ── Profiles ─────────────────────────────────────────────────────────────────

/// The injected profile host, or a 503 explaining that this server has none
/// (a harness / headless embedding with no Tauri app behind it).
fn profile_host(
    state: &WebState,
) -> Result<&std::sync::Arc<dyn super::ProfileHost>, (StatusCode, String)> {
    state.profile_host.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Profile switching is unavailable on this server".to_string(),
    ))
}

/// List profiles with their active / locked / switchable state. Locked
/// profiles are listed (their *names* are already visible to a client holding
/// the PIN) but marked unswitchable; their data stays dark behind
/// `profile_lock_gate`.
async fn list_profiles(
    State(state): State<WebState>,
) -> Result<Json<Vec<super::WebProfile>>, (StatusCode, String)> {
    Ok(Json(profile_host(&state)?.list().map_err(carrel_status)?))
}

#[derive(serde::Deserialize)]
struct ProfileSwitchRequest {
    name: String,
}

#[derive(serde::Serialize)]
struct ProfileSwitchResponse {
    active: String,
}

/// Switch the active profile for the whole server — desktop included: there is
/// one shared active profile, not one per session.
///
/// CSRF: the session cookie is `SameSite=Strict`
/// (`mod::tests::test_login_sets_session_cookie`), so a cross-site POST
/// carries no session. Basic auth is accepted on every `/api` path though, and
/// browsers replay cached Basic credentials cross-site — so the body is parsed
/// strictly as JSON. A form-encoded POST, the only shape that dodges a CORS
/// preflight and could therefore reach this handler cross-site, gets a 400; a
/// real `application/json` POST needs a preflight this server never answers.
async fn switch_profile(
    State(state): State<WebState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Json<ProfileSwitchResponse>, (StatusCode, String)> {
    let host = profile_host(&state)?;

    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().unwrap_or("").trim() == "application/json");
    if !is_json {
        return Err((
            StatusCode::BAD_REQUEST,
            "Expected an application/json body".to_string(),
        ));
    }

    // Parsed manually (like `put_want_to_read`) so a malformed body maps to
    // 400 rather than axum's built-in 422.
    let req: ProfileSwitchRequest = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    host.switch(req.name.clone()).await.map_err(switch_status)?;

    Ok(Json(ProfileSwitchResponse { active: req.name }))
}

/// Status mapping for a refused switch. The shared core's only `InvalidInput`
/// is its unknown-profile check, so that's a 404; a locked profile that hasn't
/// been unlocked on the desktop this session is `423 Locked` — the password is
/// never accepted here, so there is nothing the client can retry with.
fn switch_status(err: crate::error::CarrelError) -> (StatusCode, String) {
    match err {
        crate::error::CarrelError::LockRequired(msg) => (
            StatusCode::LOCKED,
            format!("{msg}. Unlock it on the desktop to use it over the network."),
        ),
        crate::error::CarrelError::InvalidInput(msg) => (StatusCode::NOT_FOUND, msg),
        other => carrel_status(other),
    }
}

// ── Health + Auth ────────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[derive(serde::Deserialize)]
struct LoginRequest {
    pin: String,
}

#[derive(serde::Serialize)]
struct LoginResponse {
    token: String,
}

async fn login(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(state): State<WebState>,
    req: axum::extract::Request,
) -> Result<Response, (StatusCode, String)> {
    // Use the actual peer IP from the TCP connection (not spoofable headers)
    let client_ip = addr.ip().to_string();

    // Capture the user-agent before the request body is consumed below.
    let user_agent = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // R2-2: Atomically check rate limit and record the attempt
    if !state.login_limiter.attempt(&client_ip) {
        if let Ok(conn) = state.conn() {
            log_login_attempt(
                &conn,
                &client_ip,
                user_agent.as_deref(),
                WebAuthMethod::Session,
                LoginOutcome::RateLimited,
            );
        }
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "Too many login attempts. Try again later.".to_string(),
        ));
    }

    let body: LoginRequest = {
        let bytes = axum::body::to_bytes(req.into_body(), 1024)
            .await
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid request body".to_string()))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid JSON".to_string()))?
    };

    let valid = state
        .pin_hash
        .lock()
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .as_ref()
        .map(|hash| super::auth::verify_pin(&body.pin, hash))
        .unwrap_or(false);

    if !valid {
        if let Ok(conn) = state.conn() {
            log_login_attempt(
                &conn,
                &client_ip,
                user_agent.as_deref(),
                WebAuthMethod::Session,
                LoginOutcome::InvalidPin,
            );
        }
        return Err((StatusCode::UNAUTHORIZED, "Invalid PIN".into()));
    }

    // Successful login — clear rate limit entries for this IP
    state.login_limiter.clear(&client_ip);

    let token = super::auth::create_session(&state).map_err(carrel_status)?;

    // Log success only after the session token was actually created.
    if let Ok(conn) = state.conn() {
        log_login_attempt(
            &conn,
            &client_ip,
            user_agent.as_deref(),
            WebAuthMethod::Session,
            LoginOutcome::Success,
        );
    }

    let cookie =
        format!("carrel_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
    let body = Json(LoginResponse {
        token: token.clone(),
    });

    Ok(([(header::SET_COOKIE, cookie)], body).into_response())
}

#[derive(serde::Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}

/// Every login attempt this server has seen: client IP, user agent, outcome.
/// Gated like the data export — it is the same kind of payload, and the rows
/// outlive the PIN that produced them, so an owner who configures a PIN, uses
/// the web UI, then clears the PIN would otherwise leave that log readable by
/// anything on the LAN.
async fn login_history(
    State(state): State<WebState>,
    Query(params): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::models::WebSessionEntry>>, (StatusCode, String)> {
    require_configured_pin(&state, "The login history")?;
    let conn = state.conn().map_err(carrel_status)?;
    let rows = db::get_web_session_log(&conn, params.limit.unwrap_or(100).min(1000))
        .map_err(carrel_status)?;
    Ok(Json(rows))
}

// ── Books ────────────────────────────────────────────────────────────────────

// The three structs below are web-safe views of carrel-core's `Book`,
// `BookGridItem` and `ContinueReadingItem`: every field except `file_path`,
// `cover_path` and (on `Book`) `file_hash`. The first two are absolute
// filesystem paths — `file_path` is the book's own location (under
// `import_mode = link`, the owner's original path outside the library folder
// entirely) and `cover_path` is under the app-data covers directory, which
// embeds the OS username. `file_hash` is not a path but leaks in the same
// direction: it fingerprints the exact file the owner holds (see the note on
// the field itself). The web server is reachable by anything on the LAN that
// gets past the (optional) PIN, so none of the three may reach a response
// body. Clients that need the bytes already use
// `GET /api/books/{id}/download` and `.../cover`, which serve the file
// directly rather than its path.
//
// Deliberately field-listed rather than "serialize to `Value` and strip two
// keys": the concise shape republishes any new field `carrel-core` adds to
// these models by default, which is how this defect exists in the first
// place. Each `From` impl below also destructures its source struct
// exhaustively (no `..`), so a new field on `Book`/`BookGridItem`/
// `ContinueReadingItem` fails this file to compile until someone decides
// whether it belongs on the web-facing side too — the key-set tests in
// `mod::tests` are the second, independent guard against the same field
// reappearing.
//
// Not done in carrel-core itself: it's a git dependency Carrel Server pins
// to a release tag, and the desktop frontend reads `cover_path` over IPC
// from these same structs, so a `skip_serializing` there would break both.

#[derive(serde::Serialize)]
struct PublicBook {
    id: String,
    title: String,
    author: String,
    total_chapters: u32,
    added_at: i64,
    format: BookFormat,
    description: Option<String>,
    genres: Option<String>,
    rating: Option<f64>,
    isbn: Option<String>,
    openlibrary_key: Option<String>,
    enrichment_status: Option<String>,
    series: Option<String>,
    volume: Option<u32>,
    language: Option<String>,
    publisher: Option<String>,
    publish_year: Option<u16>,
    is_imported: bool,
    want_to_read: bool,
}

impl From<crate::models::Book> for PublicBook {
    fn from(book: crate::models::Book) -> Self {
        let crate::models::Book {
            id,
            title,
            author,
            file_path: _,
            cover_path: _,
            total_chapters,
            added_at,
            format,
            // The file's SHA-256. Nothing on the web side reads it, and it is
            // an exact-copy fingerprint of the owner's file — enough to match
            // against a known-file database and identify the precise edition
            // they hold. Dropped for the same reason as the two paths.
            file_hash: _,
            description,
            genres,
            rating,
            isbn,
            openlibrary_key,
            enrichment_status,
            series,
            volume,
            language,
            publisher,
            publish_year,
            is_imported,
            want_to_read,
        } = book;
        Self {
            id,
            title,
            author,
            total_chapters,
            added_at,
            format,
            description,
            genres,
            rating,
            isbn,
            openlibrary_key,
            enrichment_status,
            series,
            volume,
            language,
            publisher,
            publish_year,
            is_imported,
            want_to_read,
        }
    }
}

#[derive(serde::Serialize)]
struct PublicBookGridItem {
    id: String,
    title: String,
    author: String,
    total_chapters: u32,
    added_at: i64,
    format: BookFormat,
    series: Option<String>,
    volume: Option<u32>,
    rating: Option<f64>,
    language: Option<String>,
    publish_year: Option<u16>,
    is_imported: bool,
    want_to_read: bool,
}

impl From<crate::models::BookGridItem> for PublicBookGridItem {
    fn from(item: crate::models::BookGridItem) -> Self {
        let crate::models::BookGridItem {
            id,
            title,
            author,
            cover_path: _,
            total_chapters,
            added_at,
            format,
            series,
            volume,
            rating,
            language,
            publish_year,
            is_imported,
            want_to_read,
        } = item;
        Self {
            id,
            title,
            author,
            total_chapters,
            added_at,
            format,
            series,
            volume,
            rating,
            language,
            publish_year,
            is_imported,
            want_to_read,
        }
    }
}

#[derive(serde::Serialize)]
struct PublicContinueReadingItem {
    id: String,
    title: String,
    author: String,
    format: BookFormat,
    total_chapters: u32,
    chapter_index: u32,
    scroll_position: f64,
    last_read_at: i64,
}

impl From<crate::models::ContinueReadingItem> for PublicContinueReadingItem {
    fn from(item: crate::models::ContinueReadingItem) -> Self {
        let crate::models::ContinueReadingItem {
            id,
            title,
            author,
            cover_path: _,
            format,
            total_chapters,
            chapter_index,
            scroll_position,
            last_read_at,
        } = item;
        Self {
            id,
            title,
            author,
            format,
            total_chapters,
            chapter_index,
            scroll_position,
            last_read_at,
        }
    }
}

#[derive(serde::Deserialize)]
struct BookQuery {
    q: Option<String>,
    series: Option<String>,
    sort: Option<String>, // title, author, last_read, rating (default: date_added)
    // Item 14: both optional and backward-compatible — when `limit` is
    // absent the response is the full filtered+sorted list exactly as
    // before (OPDS/desktop and any other caller never sends it).
    limit: Option<usize>,
    offset: Option<usize>,
    /// Presence-only: `?want_to_read=true` enables the filter; any other value
    /// or absence leaves it off. Typed as `String` (not `bool`) deliberately —
    /// axum's `Query` extraction is all-or-nothing, so a `bool` field would
    /// 400 the whole listing on a malformed/empty value (`want_to_read=`,
    /// `want_to_read=1`) that a proxy or OPDS client might append. Lenient
    /// parsing keeps the catalog serving, matching the Item-14 param convention.
    want_to_read: Option<String>,
}

async fn list_books(
    State(state): State<WebState>,
    Query(params): Query<BookQuery>,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let books = db::list_books_grid(&conn).map_err(carrel_status)?;

    let books = match params.series {
        Some(ref s) if !s.is_empty() => books
            .into_iter()
            .filter(|b| b.series.as_deref() == Some(s.as_str()))
            .collect(),
        _ => books,
    };

    let books = match params.q {
        Some(ref q) if !q.is_empty() => {
            let q_lower = q.to_lowercase();
            books
                .into_iter()
                .filter(|b| {
                    b.title.to_lowercase().contains(&q_lower)
                        || b.author.to_lowercase().contains(&q_lower)
                })
                .collect()
        }
        _ => books,
    };

    let books = match params.want_to_read.as_deref() {
        Some("true") => books.into_iter().filter(|b| b.want_to_read).collect(),
        _ => books,
    };

    // Sort
    // Fix D: every branch falls back to `id` on equality — ties (identical
    // title/author/rating/last-read, or no reading progress at all) would
    // otherwise sort in whatever order the underlying Vec happened to be in,
    // which isn't stable across requests. That breaks offset pagination:
    // the same book could land on two pages or be skipped depending on how
    // ties resolved between two calls. `id` is unique, so this gives every
    // sort a total, deterministic order (mirrors resolveSeriesNav in
    // app.js, which needed the same fix for the same reason).
    let mut books = books;
    match params.sort.as_deref() {
        Some("title") => books.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        }),
        Some("author") => books.sort_by(|a, b| {
            a.author
                .to_lowercase()
                .cmp(&b.author.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        }),
        Some("rating") => books.sort_by(|a, b| {
            b.rating
                .unwrap_or(0.0)
                .partial_cmp(&a.rating.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        }),
        Some("last_read") => {
            // Need reading progress for last_read sort
            let progress_map: std::collections::HashMap<String, i64> =
                db::get_all_reading_progress(&conn)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.book_id, p.last_read_at))
                    .collect();
            books.sort_by(|a, b| {
                let la = progress_map.get(&a.id).copied().unwrap_or(0);
                let lb = progress_map.get(&b.id).copied().unwrap_or(0);
                lb.cmp(&la).then_with(|| a.id.cmp(&b.id))
            });
        }
        _ => {} // default: date_added DESC, id from SQL
    }

    // Item 14: pagination is applied strictly after filter+sort, so it's
    // purely a slice of the same result the pre-pagination endpoint would
    // have returned — no `limit` means no behavior change at all.
    match params.limit {
        Some(limit) => {
            let total = books.len();
            let offset = params.offset.unwrap_or(0).min(total);
            let end = offset.saturating_add(limit).min(total);
            let page: Vec<PublicBookGridItem> =
                books[offset..end].iter().cloned().map(Into::into).collect();
            Ok((
                [(
                    axum::http::HeaderName::from_static("x-total-count"),
                    total.to_string(),
                )],
                Json(page),
            )
                .into_response())
        }
        None => {
            let books: Vec<PublicBookGridItem> = books.into_iter().map(Into::into).collect();
            Ok(Json(books).into_response())
        }
    }
}

// Item 5: "Continue Reading" shelf on the home screen — books with progress
// that is neither zero nor "finished", most recently read first.
#[derive(serde::Deserialize)]
struct ContinueReadingQuery {
    limit: Option<u32>,
}

async fn continue_reading(
    State(state): State<WebState>,
    Query(params): Query<ContinueReadingQuery>,
) -> Result<Json<Vec<PublicContinueReadingItem>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let limit = params.limit.unwrap_or(12).min(50);
    let books = db::get_continue_reading_books(&conn, limit).map_err(carrel_status)?;
    Ok(Json(books.into_iter().map(Into::into).collect()))
}

/// Item 8: the book-detail response is the shared `Book` model (minus
/// `file_path`, `cover_path` and `file_hash` — see [`PublicBook`]) plus
/// `file_size`, which
/// isn't a DB column — it's stat'd from the resolved book file on disk
/// (same path `download_book` reads) so no schema change is needed. `None`
/// when the file can't be stat'd (e.g. missing/unlinked).
#[derive(serde::Serialize)]
struct BookDetail {
    #[serde(flatten)]
    book: PublicBook,
    file_size: Option<u64>,
}

async fn get_book(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<BookDetail>, (StatusCode, String)> {
    // Finding E: fetch the book and drop the connection before resolving its
    // path — `resolve_book_path` acquires its own connection internally for
    // imported books with a relative path, so holding this one across that
    // call meant two connections held from the pool (max 5) at once,
    // stalling concurrent detail requests under load.
    let book = {
        let conn = state.conn().map_err(carrel_status)?;
        db::get_book(&conn, &id)
            .map_err(carrel_status)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?
    };
    // The filesystem stat is best-effort (`file_size` stays `None` on any
    // error) and run on a blocking thread — `std::fs::metadata` on a
    // network-mounted library folder can stall for seconds, which would
    // otherwise block a tokio worker thread directly in this async handler.
    let file_size = match state.resolve_book_path(&book) {
        Ok(path) => {
            tokio::task::spawn_blocking(move || std::fs::metadata(path).ok().map(|m| m.len()))
                .await
                .unwrap_or(None)
        }
        Err(_) => None,
    };
    Ok(Json(BookDetail {
        book: book.into(),
        file_size,
    }))
}

// ── Covers ───────────────────────────────────────────────────────────────────

/// Finding 8: covers are decorative artwork, not book content — far less
/// sensitive than page images/chapter text, and OPDS e-reader clients
/// re-fetch full covers constantly under per-request Basic Auth (no
/// cookie/session reuse to worry about). A blanket `no-store` whenever a PIN
/// is configured regressed those clients for little real security benefit,
/// so covers (both the full image and `?size=thumb`) always get a cacheable
/// response regardless of PIN — unlike `session_cache_control`'s policy for
/// page images/page-count, which must stay PIN-aware since those requests
/// never pass through `auth_middleware` once a cached response exists.
const COVER_CACHE_CONTROL: &str = "private, max-age=86400";

/// Finding 5: `Query<CoverQuery>` (axum's `serde_urlencoded`-backed
/// extractor) hard-rejects request shapes real clients send in practice — a
/// duplicate `size` key, or a `%` sequence that isn't valid percent-encoding
/// — turning what used to serve fine into a 400. Parse the query string
/// ourselves instead: take the *last* `size=` occurrence (mirrors how most
/// frameworks resolve duplicate keys) and never fail the request over
/// anything else — an unparseable or unrecognized value already falls
/// through to the "serve full cover" branch below, same as `size=banana`
/// always has.
fn parse_cover_size(query: Option<&str>) -> Option<String> {
    let mut size = None;
    for pair in query?.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "size" {
            size = Some(urlencoding::decode(value).unwrap_or_default().into_owned());
        }
    }
    size
}

/// Finding 1: true only when `cover_path`'s parent directory canonicalizes
/// to somewhere inside `covers_root`. `cover_path` is a DB-backed value that
/// predates this hardening pass — reading it (here and in the full-cover
/// route) stays unguarded — but the disk write the thumbnail cache
/// introduces below must never be steered outside the app-managed covers
/// directory by a malformed or adversarial row.
fn cover_write_path_is_safe(covers_root: &std::path::Path, cover_path: &std::path::Path) -> bool {
    let (Some(parent), Ok(canon_root)) = (cover_path.parent(), covers_root.canonicalize()) else {
        return false;
    };
    parent
        .canonicalize()
        .map(|canon_parent| canon_parent.starts_with(&canon_root))
        .unwrap_or(false)
}

/// Item 11 cover-thumbnail resolution, hardened per code review (findings
/// 1-4, 7): serves the persisted `thumb.jpg` sibling only when it is at
/// least as fresh as the cover it was made from (finding 2a — otherwise a
/// replaced cover serves stale art forever), regenerates and atomically
/// persists a new one otherwise (finding 3), and only ever writes inside the
/// app's covers root (finding 1). Generation/persist failures are logged and
/// fall back to serving whatever bytes are already in hand (finding 7)
/// rather than 500ing. Synchronous — the caller runs this inside a single
/// `spawn_blocking` (finding 6).
fn resolve_cover_thumb(
    covers_root: &std::path::Path,
    cover_path: &std::path::Path,
) -> std::io::Result<(Vec<u8>, String)> {
    use std::io::Write;

    let thumb_path = cover_path.with_file_name(crate::commands::THUMB_FILENAME);

    let cover_mtime = std::fs::metadata(cover_path)?.modified()?;

    // A stat/read failure here is an ordinary cache miss (no thumb yet, or a
    // race with a concurrent writer) — not something worth logging. Fall
    // through and regenerate.
    let cached = std::fs::metadata(&thumb_path)
        .and_then(|m| m.modified())
        .ok()
        .filter(|&thumb_mtime| thumb_mtime >= cover_mtime)
        .and_then(|_| std::fs::read(&thumb_path).ok());
    if let Some(bytes) = cached {
        return Ok((bytes, "image/jpeg".to_string()));
    }

    let full_bytes = std::fs::read(cover_path)?;

    let generated =
        carrel_core::image_util::make_thumbnail(&full_bytes, crate::commands::THUMB_WIDTH)
            .unwrap_or_else(|e| {
                log::warn!(
                    "cover thumbnail generation failed for '{}': {e}",
                    cover_path.display()
                );
                None
            });

    let Some(thumb_bytes) = generated else {
        let mime = mime_guess::from_path(cover_path)
            .first_or_octet_stream()
            .to_string();
        return Ok((full_bytes, mime));
    };

    if cover_write_path_is_safe(covers_root, cover_path) {
        // Finding 4 (TOCTOU): re-stat the cover right before persisting. If
        // it changed since `cover_mtime` was captured above (the desktop app
        // replaced cover+thumb concurrently), skip the write — persisting
        // now would clobber the fresh thumb with stale art, and the stale
        // write's own mtime would still pass future freshness checks.
        let still_current = std::fs::metadata(cover_path)
            .and_then(|m| m.modified())
            .map(|m| m == cover_mtime)
            .unwrap_or(false);

        if still_current {
            if let Err(e) =
                carrel_core::storage::write_atomic(&thumb_path, |f| f.write_all(&thumb_bytes))
            {
                log::warn!(
                    "cover thumbnail persist failed for '{}': {e}",
                    thumb_path.display()
                );
            }
        } else {
            log::warn!(
                "skipping thumbnail persist for '{}': cover changed during generation",
                cover_path.display()
            );
        }
    } else {
        log::warn!(
            "skipping thumbnail persist for '{}': cover path resolves outside the covers root",
            cover_path.display()
        );
    }

    Ok((thumb_bytes, "image/jpeg".to_string()))
}

/// Async wrapper: runs [`resolve_cover_thumb`] in a single `spawn_blocking`
/// (finding 6 — decode/resize/persist is all CPU- and I/O-bound). A panic
/// inside that closure (finding 7) must not 500 the request when the cover
/// itself is perfectly readable, so it falls back to serving the full cover
/// via `tokio::fs` instead.
async fn get_cover_thumb_bytes(
    covers_root: std::path::PathBuf,
    cover_path: String,
) -> Result<(Vec<u8>, String), (StatusCode, String)> {
    let cover_path_buf = std::path::PathBuf::from(&cover_path);
    match tokio::task::spawn_blocking(move || resolve_cover_thumb(&covers_root, &cover_path_buf))
        .await
    {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(book_file_status(
            "Cover image not found",
            "Cover image could not be read",
            e,
        )),
        Err(join_err) => {
            log::warn!("cover thumbnail worker panicked for '{cover_path}': {join_err}");
            let bytes = tokio::fs::read(&cover_path).await.map_err(|e| {
                book_file_status("Cover image not found", "Cover image could not be read", e)
            })?;
            let mime = mime_guess::from_path(&cover_path)
                .first_or_octet_stream()
                .to_string();
            Ok((bytes, mime))
        }
    }
}

async fn get_cover(
    State(state): State<WebState>,
    Path(id): Path<String>,
    uri: axum::http::Uri,
) -> Result<Response, (StatusCode, String)> {
    let cover_path = {
        let conn = state.conn().map_err(carrel_status)?;
        let book = db::get_book(&conn, &id)
            .map_err(carrel_status)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
        book.cover_path
            .ok_or_else(|| (StatusCode::NOT_FOUND, "No cover available".to_string()))?
    };

    let size = parse_cover_size(uri.query());
    let (bytes, mime) = if size.as_deref() == Some("thumb") {
        get_cover_thumb_bytes(state.covers_root(), cover_path).await?
    } else {
        let bytes = std::fs::read(&cover_path).map_err(|e| {
            book_file_status("Cover image not found", "Cover image could not be read", e)
        })?;
        let mime = mime_guess::from_path(&cover_path)
            .first_or_octet_stream()
            .to_string();
        (bytes, mime)
    };

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, COVER_CACHE_CONTROL.to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ── EPUB Chapters ────────────────────────────────────────────────────────────

async fn get_chapters(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let book = db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    let toc_not_found = "Table of contents not available for this book";
    let toc_invalid =
        "Table of contents could not be read: the book file may be corrupt or unsupported";
    let file_path = state
        .resolve_book_path(&book)
        .map_err(|e| book_file_status(toc_not_found, toc_invalid, e))?;
    let toc = match book.format {
        BookFormat::Epub => crate::epub::get_toc(&file_path)
            .map_err(|e| book_file_status(toc_not_found, toc_invalid, e))?,
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            // MOBI has no real TOC — mirror the desktop `get_toc` behaviour by
            // synthesising a flat list from the chapter list.
            let chapters = carrel_core::mobi::get_chapter_list(&file_path)
                .map_err(|e| book_file_status(toc_not_found, toc_invalid, e))?;
            chapters
                .into_iter()
                .map(|c| crate::models::TocEntry {
                    chapter_index: c.index as u32,
                    label: c.title,
                    play_order: format!("{}", c.index + 1),
                    children: Vec::new(),
                })
                .collect()
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => {
            return Err((
                StatusCode::BAD_REQUEST,
                "MOBI support is not enabled in this build".to_string(),
            ));
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "TOC is only available for EPUB and MOBI books".to_string(),
            ));
        }
    };

    Ok(Json(serde_json::to_value(toc).unwrap_or_default()))
}

async fn get_chapter_content(
    State(state): State<WebState>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let book = db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    let chapter_not_found = "Chapter not found";
    let chapter_invalid = "Chapter could not be read: the book file may be corrupt or unsupported";
    let file_path = state
        .resolve_book_path(&book)
        .map_err(|e| book_file_status(chapter_not_found, chapter_invalid, e))?;
    let images_storage = state
        .images_storage()
        .map_err(|e| book_file_status(chapter_not_found, chapter_invalid, e))?;

    // Reject the unsupported formats here rather than letting the reader
    // module do it, so each rejection keeps its own client-facing wording —
    // `book_file_status` would flatten both into the generic "corrupt or
    // unsupported" body.
    #[cfg(not(feature = "mobi"))]
    if matches!(book.format, BookFormat::Mobi) {
        return Err((
            StatusCode::BAD_REQUEST,
            "MOBI support is not enabled in this build".to_string(),
        ));
    }
    if !matches!(book.format, BookFormat::Epub | BookFormat::Mobi) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Chapter content is only available for EPUB and MOBI books".to_string(),
        ));
    }

    // Reads through the process-wide archive cache (M1): before this, every
    // chapter request reopened and reparsed the whole book file, while the
    // desktop reader had been reading from a warm archive all along.
    let html = carrel_core::reader::chapter_html(
        book.format,
        &file_path,
        index,
        images_storage.as_ref(),
        &id,
        &state.archives,
    )
    .map_err(|e| book_file_status(chapter_not_found, chapter_invalid, e))?;

    // Rewrite asset:// URLs to HTTP URLs for web serving
    let html = rewrite_asset_urls_to_http(&html, &id, index);

    // R3-1: Sanitize HTML to prevent XSS from malicious book content
    let html = sanitize_chapter_html(&html);

    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response())
}

/// Validate that a filename is safe (no path traversal sequences).
fn is_safe_filename(name: &str) -> bool {
    let decoded = urlencoding::decode(name).unwrap_or_default();
    let decoded = decoded.as_ref();
    !decoded.contains("..")
        && !decoded.starts_with('/')
        && !decoded.starts_with('\\')
        && !decoded.contains('\0')
}

/// Sanitize chapter HTML for web serving — strip scripts and event handlers.
fn sanitize_chapter_html(html: &str) -> String {
    ammonia::Builder::new()
        .add_tags([
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "p",
            "div",
            "span",
            "a",
            "em",
            "strong",
            "b",
            "i",
            "u",
            "s",
            "sub",
            "sup",
            "br",
            "hr",
            "img",
            "figure",
            "figcaption",
            "ul",
            "ol",
            "li",
            "dl",
            "dt",
            "dd",
            "table",
            "thead",
            "tbody",
            "tr",
            "th",
            "td",
            "blockquote",
            "pre",
            "code",
            "section",
            "article",
            "nav",
            "header",
            "footer",
            "aside",
            "details",
            "summary",
        ])
        .add_tag_attributes("a", &["href", "title"])
        .add_tag_attributes("img", &["src", "alt", "width", "height"])
        .add_tag_attributes("td", &["colspan", "rowspan"])
        .add_tag_attributes("th", &["colspan", "rowspan"])
        .url_relative(ammonia::UrlRelative::PassThrough)
        .clean(html)
        .to_string()
}

/// Rewrite `asset://localhost/...` image URLs to HTTP `/api/books/{id}/images/{chapter}/{filename}`.
fn rewrite_asset_urls_to_http(html: &str, book_id: &str, chapter_index: usize) -> String {
    // The epub module produces URLs like: asset://localhost/{url_encoded_path}
    // We need to extract the filename and rewrite to our HTTP route.
    let mut result = html.to_string();

    while let Some(start) = result.find("asset://localhost/") {
        let rest = &result[start + 18..]; // skip "asset://localhost/"
        let url_end = rest
            .find('"')
            .or_else(|| rest.find('\''))
            .or_else(|| rest.find(')'))
            .unwrap_or(rest.len());

        let encoded_path = &rest[..url_end];
        let decoded = urlencoding::decode(encoded_path).unwrap_or_default();
        let filename = decoded.rsplit('/').next().unwrap_or("image");

        let new_url = format!("/api/books/{book_id}/images/{chapter_index}/{filename}");
        result = format!(
            "{}{}{}",
            &result[..start],
            new_url,
            &result[start + 18 + url_end..]
        );
    }

    result
}

async fn get_epub_image(
    State(state): State<WebState>,
    Path((id, chapter, filename)): Path<(String, usize, String)>,
) -> Result<Response, (StatusCode, String)> {
    // R2-1: Prevent path traversal
    if !is_safe_filename(&filename) {
        return Err((StatusCode::BAD_REQUEST, "Invalid filename".to_string()));
    }

    let image_path = state
        .data_dir
        .join("images")
        .join(&id)
        .join(chapter.to_string())
        .join(&filename);

    let bytes = std::fs::read(&image_path).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            format!("Image not found: {filename}"),
        )
    })?;

    let mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .to_string();

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ── PDF / Comic Pages ────────────────────────────────────────────────────────

/// Cache-control for content that must respect session expiry: PDF/CBZ/CBR
/// page images and page counts are rasterized from the book file itself, so
/// they're safe to cache in the browser when the server is unauthenticated
/// (no PIN, no session gate). Once a PIN is configured, a cached response
/// would let the same browser keep serving protected pages for up to an hour
/// after the session expires — those requests never reach `auth_middleware`
/// at all. `no-store` closes that gap. Contrast `COVER_CACHE_CONTROL`, which
/// doesn't need this treatment (finding 8).
fn session_cache_control(state: &WebState) -> &'static str {
    if state.has_pin() {
        "no-store"
    } else {
        "private, max-age=3600"
    }
}

/// Tolerant `?width=` parsing (web-reader offline mode downscales page images
/// on download). Any input that isn't exactly one positive integer resolves to
/// `None` — byte-identical current behavior — so a malformed query can never
/// break a page request. Valid values clamp to 64..=2048 — the cap bounds per-request raster cost on this unauthenticated-capable surface (no-PIN mode) close to the old fixed 1200 px render. Zero is rejected (a
/// zero-width render is meaningless); duplicates are rejected (ambiguous
/// intent); unrelated params (the reader's `?__reload=` retry nonce) are
/// ignored.
fn parse_width(query: Option<&str>) -> Option<u32> {
    let query = query?;
    let mut found: Option<u32> = None;
    // form_urlencoded percent-decodes keys and values (RawQuery is raw), so an
    // encoded `width=%31%30%38%30` parses normally and an encoded `w%69dth`
    // still counts toward duplicate detection instead of sneaking past it.
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key != "width" {
            continue;
        }
        let parsed: u32 = value.parse().ok().filter(|w| *w > 0)?;
        if found.is_some() {
            return None; // duplicate width params
        }
        found = Some(parsed.clamp(64, 2048));
    }
    found
}

async fn get_page_image(
    State(state): State<WebState>,
    Path((id, index)): Path<(String, u32)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Result<Response, (StatusCode, String)> {
    let width = parse_width(query.as_deref());
    // Scoped so the `PooledConnection` (not `Send`) is dropped before the
    // `spawn_blocking` hop below — held across that `.await` it would make
    // this handler's future non-`Send` and axum would refuse to compile it.
    // The eviction budget is read here too (same connection, same scope) so
    // there is no second checkout and nothing DB-shaped survives near the
    // hop: same key, same fallback, same parse as `commands::prepare_comic`,
    // which is the desktop's resolution of this exact setting.
    let (book, max_cache_size_mb) = {
        let conn = state.conn().map_err(carrel_status)?;
        let book = db::get_book(&conn, &id)
            .map_err(carrel_status)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
        let max_cache_size_mb = db::get_setting(&conn, "page_cache_max_size_mb")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(crate::page_cache::DEFAULT_MAX_CACHE_SIZE_MB);
        (book, max_cache_size_mb)
    };

    let page_not_found = "Page not found in this book";
    let page_invalid =
        "Page image could not be rendered: the book file may be corrupt or unsupported";
    let file_path = state
        .resolve_book_path(&book)
        .map_err(|e| book_file_status(page_not_found, page_invalid, e))?;
    let page_cache_control = session_cache_control(&state);

    // Stage the source locally when it lives on a network mount (M2). The web
    // server has no desktop-style "open" event, so this runs per page request;
    // it's cheap once staged (local check + LRU touch) and deduped while a copy
    // is in flight. Only the page-image formats below reach a per-page render
    // over the (possibly remote) file, so gate the trigger to them.
    if matches!(
        book.format,
        BookFormat::Pdf | BookFormat::Cbz | BookFormat::Cbr
    ) {
        crate::commands::ensure_web_source_staged(&state.cache_dir, &book, &file_path);
    }

    match book.format {
        // Reads through the disk page cache (M2): before this, every PDF
        // page request re-rendered with pdfium from scratch, while the
        // desktop reader had been reading from a warm cache all along (once
        // `commands::prepare_pdf` had run on book-open). This route has no
        // equivalent open event, so `carrel_core::reader::page_image`'s PDF
        // arm establishes the same zero-prewarm manifest lazily on its own
        // first call for a book — see that function's doc comment. Unlike
        // the comic arm below, a miss renders exactly the one requested
        // page, not the whole document, so this still runs on the blocking
        // pool (a pdfium render is CPU-heavy) but for a bounded, single-page
        // cost rather than a whole-archive one.
        BookFormat::Pdf => {
            let pages_storage = state
                .pages_storage()
                .map_err(|e| book_file_status(page_not_found, page_invalid, e))?;
            let book_id = id.clone();
            let file_hash = book.file_hash.clone();
            let is_private = state.is_private();
            let (bytes, mime) = tokio::task::spawn_blocking(move || {
                let evict_storage = pages_storage.clone();
                carrel_core::reader::page_image(
                    BookFormat::Pdf,
                    &file_path,
                    index,
                    width,
                    &book_id,
                    file_hash.as_deref(),
                    pages_storage.as_ref(),
                    // Fires when the lazy-write counter crosses a multiple
                    // of `page_cache::LAZY_EVICTION_BATCH` — a sparser
                    // cadence than the comic arm's "every priming miss",
                    // since a PDF page cache fills one page at a time. Same
                    // budget resolution as the comic arm below: the user's
                    // `page_cache_max_size_mb` setting, so a lowered budget
                    // is honored regardless of which format primed the
                    // cache first.
                    move || {
                        if let Err(e) = crate::page_cache::run_eviction(
                            evict_storage.as_ref(),
                            max_cache_size_mb,
                        ) {
                            log::warn!("pdf page-cache eviction failed: {e}");
                        }
                    },
                    // Private mode: suppress only the page-bytes disk write,
                    // matching `commands::get_pdf_page_bytes`'s desktop
                    // behaviour, so a "don't track this session" read
                    // doesn't persist rendered pages to the shared
                    // page-cache directory.
                    is_private,
                )
            })
            .await
            .map_err(|e| {
                log::error!("pdf page render task panicked: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            })?
            .map_err(|e| book_file_status(page_not_found, page_invalid, e))?;
            Ok((
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, page_cache_control.to_string()),
                ],
                bytes,
            )
                .into_response())
        }
        // Reads through the disk page cache (M1 follow-on): before this, every
        // page request reopened and reparsed the whole archive, while the
        // desktop reader had been reading from a warm cache all along.
        //
        // A cache miss now extracts the *whole* archive (see
        // `carrel_core::reader::page_image`'s doc comment for why) —
        // proportional to the whole book rather than one page, and on this
        // project's real library that archive can live on a network mount.
        // Run it on the blocking pool (`spawn_blocking`), the same treatment
        // `commands::get_comic_page_bytes` gives this exact work on the
        // desktop side: a handful of concurrent cold opens would otherwise
        // occupy every Tokio worker and stall the health check and every
        // unrelated route behind them (M1 review, finding 1). Everything
        // moved into the closure is owned, not borrowed from `state`/`book`.
        BookFormat::Cbz | BookFormat::Cbr => {
            let pages_storage = state
                .pages_storage()
                .map_err(|e| book_file_status(page_not_found, page_invalid, e))?;
            let format = book.format.clone();
            let book_id = id.clone();
            let file_hash = book.file_hash.clone();
            let (bytes, mime) = tokio::task::spawn_blocking(move || {
                let evict_storage = pages_storage.clone();
                carrel_core::reader::page_image(
                    format,
                    &file_path,
                    index,
                    width,
                    &book_id,
                    file_hash.as_deref(),
                    pages_storage.as_ref(),
                    // Fires only on the miss that just extracted the whole
                    // archive (M1 review, finding 2) — `run_eviction` walks
                    // the entire page cache, so it must never run on the hit
                    // path that serves nearly every request. The budget is
                    // the user's `page_cache_max_size_mb` setting, resolved
                    // above with `DEFAULT_MAX_CACHE_SIZE_MB` only as the
                    // fallback when it's unset — a lowered budget must be
                    // honored here too, or the desktop and web halves of one
                    // install would disagree about the same cache directory
                    // (M1 review, finding 3). A failed sweep is logged and
                    // swallowed rather than failing a page request that
                    // already has its bytes.
                    move || {
                        if let Err(e) = crate::page_cache::run_eviction(
                            evict_storage.as_ref(),
                            max_cache_size_mb,
                        ) {
                            log::warn!("comic page-cache eviction failed: {e}");
                        }
                    },
                    // Comics do not yet consult private mode (tracked in
                    // `docs/backlog/comic-cache-ignores-private-mode.md`) —
                    // hardcoded rather than wired to `state.is_private()`
                    // so fixing that gap later is a deliberate, reviewed
                    // change, not an accidental side effect of this one.
                    false,
                )
            })
            .await
            .map_err(|e| {
                log::error!("comic page render task panicked: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            })?
            .map_err(|e| book_file_status(page_not_found, page_invalid, e))?;
            Ok((
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, page_cache_control.to_string()),
                ],
                bytes,
            )
                .into_response())
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "Page images only available for PDF/CBZ/CBR".to_string(),
        )),
    }
}

async fn get_page_count(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    // Scoped for the same reason as `get_page_image`: the `PooledConnection`
    // must be dropped before the `spawn_blocking` hop below, or this
    // handler's future stops being `Send`.
    let book = {
        let conn = state.conn().map_err(carrel_status)?;
        db::get_book(&conn, &id)
            .map_err(carrel_status)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?
    };

    let count_not_found = "Page count not available for this book";
    let count_invalid =
        "Page count could not be determined: the book file may be corrupt or unsupported";
    let file_path = state
        .resolve_book_path(&book)
        .map_err(|e| book_file_status(count_not_found, count_invalid, e))?;

    let count = match book.format {
        // Answers from the cache when a complete manifest is already on disk
        // (M1 review, finding 5) rather than opening the archive on every
        // call; falls back to the archive otherwise. Blocking pool for the
        // same reason as the page-image route above (M1 review, finding 1).
        //
        // PDF joined this arm in M2 (review finding F3): the page-image
        // route above now serves cached PDF pages without touching the
        // source, and the web reader fetches this count *before* its first
        // page request — so leaving this one always opening the file meant a
        // fully-cached PDF still could not be opened with its source gone,
        // which is the case the caching is for.
        BookFormat::Cbz | BookFormat::Cbr | BookFormat::Pdf => {
            // A cache directory that cannot even be opened must cost the
            // cache, not the book (M2 review round 2, finding 1): before
            // this route consulted the cache at all, it just read the file,
            // and a `LocalStorage::new` failure — an unwritable or missing
            // cache parent — would otherwise 500 a count the file itself
            // can still answer. `page_count` skips the manifest lookup when
            // handed `None`.
            let pages_storage = match state.pages_storage() {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("page cache unavailable for page count, reading the file: {e}");
                    None
                }
            };
            let format = book.format.clone();
            let file_hash = book.file_hash.clone();
            tokio::task::spawn_blocking(move || {
                carrel_core::reader::page_count(
                    format,
                    &file_path,
                    file_hash.as_deref(),
                    pages_storage.as_deref(),
                )
            })
            .await
            .map_err(|e| {
                log::error!("page-count task panicked: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            })?
            .map_err(|e| book_file_status(count_not_found, count_invalid, e))?
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "Page count only available for PDF/CBZ/CBR".to_string(),
            ))
        }
    };

    let page_cache_control = session_cache_control(&state);

    Ok((
        [(header::CACHE_CONTROL, page_cache_control)],
        Json(serde_json::json!({ "count": count })),
    )
        .into_response())
}

// ── Reading progress ─────────────────────────────────────────────────────────

/// PUT body for saving reading progress. Field names mirror
/// `carrel_core::models::ReadingProgress` exactly (and thus the shape the
/// desktop app already persists via `save_reading_progress`): `chapter_index`
/// doubles as the page index for PDF/CBZ/CBR books, `scroll_position` is the
/// 0..1 scroll fraction used by EPUB/MOBI.
#[derive(serde::Deserialize)]
struct ProgressUpdate {
    chapter_index: u32,
    scroll_position: f64,
}

async fn get_progress(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Option<crate::models::ReadingProgress>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    let progress = db::get_reading_progress(&conn, &id).map_err(carrel_status)?;
    Ok(Json(progress))
}

async fn put_progress(
    State(state): State<WebState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<crate::models::ReadingProgress>, (StatusCode, String)> {
    // Parsed manually (rather than via the `Json<T>` extractor) so malformed
    // bodies map to 400 like the rest of this API's validation errors —
    // axum's built-in JSON rejection uses 422.
    let body: ProgressUpdate = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    let conn = state.conn().map_err(carrel_status)?;
    let book = db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    // F4: intentionally NOT bounds-checked against `book.total_chapters`
    // here. The reader paginates against a live `/page-count`, which can
    // exceed a stale `total_chapters` (e.g. after re-pagination) — rejecting
    // those saves made progress beyond the stale bound silently fail. The
    // client clamps the index when it reads progress back.
    let scroll_position =
        crate::commands::validate_scroll_position(body.scroll_position).map_err(carrel_status)?;

    // F1: goes through the same completion-detection path as the desktop
    // `save_reading_progress` command (`apply_reading_progress`) so a
    // web-driven completion logs the same activity entry and bus event.
    // `None` here means no desktop window-toast event is emitted for a
    // web-only completion — see `apply_reading_progress`'s doc comment.
    // Private mode (B-M1): read the shared flag fresh for this request.
    let progress = crate::commands::apply_reading_progress(
        &conn,
        &book,
        &id,
        body.chapter_index,
        scroll_position,
        None,
        state.is_private(),
    )
    .map_err(carrel_status)?;

    Ok(Json(progress))
}

#[derive(serde::Deserialize)]
struct WantToReadUpdate {
    want_to_read: bool,
}

async fn put_want_to_read(
    State(state): State<WebState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    // Parsed manually (like `put_progress`) so malformed bodies map to 400
    // rather than axum's built-in 422.
    let body: WantToReadUpdate = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
    db::set_want_to_read(&conn, &id, body.want_to_read).map_err(carrel_status)?;
    Ok(StatusCode::OK)
}

// ── Bookmarks ────────────────────────────────────────────────────────────────

/// POST body for creating a bookmark. `note` is optional (desktop persists it;
/// the web UI does not edit it yet, but accept it for parity).
#[derive(serde::Deserialize)]
struct BookmarkCreate {
    chapter_index: u32,
    scroll_position: f64,
    #[serde(default)]
    note: Option<String>,
}

/// PUT body for renaming a bookmark.
#[derive(serde::Deserialize)]
struct BookmarkRename {
    name: Option<String>,
}

async fn list_book_bookmarks(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::models::Bookmark>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
    let bookmarks = db::list_bookmarks(&conn, &id).map_err(carrel_status)?;
    Ok(Json(bookmarks))
}

async fn create_bookmark(
    State(state): State<WebState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<crate::models::Bookmark>), (StatusCode, String)> {
    // Manual parse (like `put_progress`) so malformed bodies map to 400.
    let body: BookmarkCreate = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    let scroll_position =
        crate::commands::validate_scroll_position(body.scroll_position).map_err(carrel_status)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // Bookmarks are explicit user actions: persisted regardless of private mode,
    // matching desktop `add_bookmark` (which does not check the flag).
    let bookmark = crate::models::Bookmark {
        id: uuid::Uuid::new_v4().to_string(),
        book_id: id.clone(),
        chapter_index: body.chapter_index,
        scroll_position,
        name: None,
        note: body.note,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    db::insert_bookmark(&conn, &bookmark).map_err(carrel_status)?;

    // Emit the same bus event as desktop `add_bookmark` so hooks/plugins fire
    // for web-created bookmarks too. `events::bus()` is a carrel-core global
    // singleton (not tied to a Tauri handle), already used by the web progress
    // path (`apply_reading_progress` emits `BookFinished` on it).
    events::bus().emit(CarrelEvent::BookmarkCreated {
        book_id: bookmark.book_id.clone(),
        bookmark_id: bookmark.id.clone(),
    });

    Ok((StatusCode::CREATED, Json(bookmark)))
}

async fn rename_bookmark(
    State(state): State<WebState>,
    Path((id, bookmark_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<crate::models::Bookmark>, (StatusCode, String)> {
    let body: BookmarkRename = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    // Desktop parity (commands.rs `update_bookmark`): trim only to detect empty;
    // store the ORIGINAL string truncated to 100 chars (not the trimmed value).
    let normalized: Option<String> = body
        .name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.chars().take(100).collect::<String>());

    let conn = state.conn().map_err(carrel_status)?;
    // Atomic update-and-return (RETURNING): a concurrent delete can't turn a
    // committed rename into a spurious 404, and there's no second query. None =>
    // no live bookmark with this id in this book => 404.
    let updated = db::update_bookmark_name_scoped(&conn, &id, &bookmark_id, normalized.as_deref())
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Bookmark not found".to_string()))?;
    Ok(Json(updated))
}

async fn delete_bookmark(
    State(state): State<WebState>,
    Path((id, bookmark_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    // Idempotent: unknown/foreign/already-deleted ids affect 0 rows and still
    // return 204. The book_id scope prevents touching another book's row.
    db::soft_delete_bookmark_scoped(&conn, &id, &bookmark_id).map_err(carrel_status)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Highlights ────────────────────────────────────────────────────────────────

/// The 5 canonical swatches (desktop `HighlightsPanel.tsx`). Server-validated
/// so free-form colors can't enter the shared DB from the web surface.
const HIGHLIGHT_COLORS: [&str; 5] = ["#f6c445", "#7bc47f", "#6ba3d6", "#e88baf", "#e8a55d"];
const HIGHLIGHT_NOTE_MAX: usize = 2000;

/// POST body. camelCase (matches the Highlight model's serialization; the
/// web client sends camelCase — deliberate divergence from the snake_case
/// bookmark bodies, per spec).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HighlightCreate {
    chapter_index: u32,
    text: String,
    color: String,
    #[serde(default)]
    note: Option<String>,
    start_offset: u32,
    end_offset: u32,
}

fn validate_highlight_note(note: &Option<String>) -> Result<(), (StatusCode, String)> {
    if let Some(n) = note {
        if n.chars().count() > HIGHLIGHT_NOTE_MAX {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Note exceeds {HIGHLIGHT_NOTE_MAX} characters"),
            ));
        }
    }
    Ok(())
}

async fn list_book_highlights(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::models::Highlight>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
    let highlights = db::list_highlights(&conn, &id).map_err(carrel_status)?;
    Ok(Json(highlights))
}

async fn create_highlight(
    State(state): State<WebState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<crate::models::Highlight>), (StatusCode, String)> {
    // Manual parse (like `create_bookmark`) so malformed bodies map to 400.
    let body: HighlightCreate = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;
    if body.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Highlight text is empty".into()));
    }
    if body.end_offset <= body.start_offset {
        return Err((
            StatusCode::BAD_REQUEST,
            "endOffset must be greater than startOffset".into(),
        ));
    }
    if !HIGHLIGHT_COLORS.contains(&body.color.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "Unknown highlight color".into()));
    }
    validate_highlight_note(&body.note)?;

    let conn = state.conn().map_err(carrel_status)?;
    db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // Highlights are explicit user actions: persisted regardless of private
    // mode, matching desktop `add_highlight` (which does not check the flag).
    let highlight = crate::models::Highlight {
        id: uuid::Uuid::new_v4().to_string(),
        book_id: id.clone(),
        chapter_index: body.chapter_index,
        text: body.text,
        color: body.color,
        note: body.note,
        start_offset: body.start_offset,
        end_offset: body.end_offset,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    db::insert_highlight(&conn, &highlight).map_err(carrel_status)?;

    // Same bus event as desktop `add_highlight` so hooks/plugins fire for
    // web-created highlights too (bookmark precedent).
    events::bus().emit(CarrelEvent::HighlightCreated {
        book_id: highlight.book_id.clone(),
        highlight_id: highlight.id.clone(),
    });

    Ok((StatusCode::CREATED, Json(highlight)))
}

async fn update_highlight(
    State(state): State<WebState>,
    Path((id, highlight_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<crate::models::Highlight>, (StatusCode, String)> {
    // Presence-aware manual parse: serde double-Option cannot distinguish an
    // absent key from an explicit null, so inspect the JSON object directly.
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;
    let obj = v.as_object().ok_or((
        StatusCode::BAD_REQUEST,
        "Body must be a JSON object".to_string(),
    ))?;
    if !obj.contains_key("note") && !obj.contains_key("color") {
        return Err((
            StatusCode::BAD_REQUEST,
            "Provide at least one of: note, color".into(),
        ));
    }
    let note_change: Option<Option<String>> = match obj.get("note") {
        None => None,                                // key absent → unchanged
        Some(serde_json::Value::Null) => Some(None), // explicit null → clear
        Some(serde_json::Value::String(s)) => Some(Some(s.clone())),
        Some(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "note must be a string or null".into(),
            ))
        }
    };
    if let Some(Some(n)) = &note_change {
        validate_highlight_note(&Some(n.clone()))?;
    }
    let color: Option<String> = match obj.get("color") {
        None => None,
        Some(serde_json::Value::String(c)) if HIGHLIGHT_COLORS.contains(&c.as_str()) => {
            Some(c.clone())
        }
        Some(_) => return Err((StatusCode::BAD_REQUEST, "Unknown highlight color".into())),
    };

    let conn = state.conn().map_err(carrel_status)?;
    // Atomic update-and-return (RETURNING): None => no live highlight with this
    // id in this book => 404 (bookmark `rename_bookmark` precedent).
    let updated = db::update_highlight_scoped(
        &conn,
        &id,
        &highlight_id,
        note_change.as_ref().map(|o| o.as_deref()),
        color.as_deref(),
    )
    .map_err(carrel_status)?
    .ok_or_else(|| (StatusCode::NOT_FOUND, "Highlight not found".to_string()))?;

    events::bus().emit(CarrelEvent::HighlightUpdated {
        highlight_id: updated.id.clone(),
    });
    Ok(Json(updated))
}

async fn delete_highlight_route(
    State(state): State<WebState>,
    Path((id, highlight_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    // Idempotent: unknown/foreign/already-deleted ids affect 0 rows and still
    // return 204. Emit the bus event only when a live row actually transitioned.
    let n = db::soft_delete_highlight_scoped(&conn, &id, &highlight_id).map_err(carrel_status)?;
    if n > 0 {
        events::bus().emit(CarrelEvent::HighlightDeleted {
            highlight_id: highlight_id.clone(),
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Item 15: bulk progress rows for the library grid's progress badges.
/// Reuses `db::get_all_reading_progress` verbatim (already used internally
/// for the `last_read` sort above) — no new query, no `BookGridItem` model
/// change. Only books with a progress row are included; the frontend treats
/// absence as "no badge".
async fn get_all_progress(
    State(state): State<WebState>,
) -> Result<Json<Vec<crate::models::ReadingProgress>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let progress = db::get_all_reading_progress(&conn).map_err(carrel_status)?;
    Ok(Json(progress))
}

// ── Download ─────────────────────────────────────────────────────────────────

async fn download_book(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let book = db::get_book(&conn, &id)
        .map_err(carrel_status)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;

    // Same treatment as the render routes: every error below is the OS
    // talking about a path the client must never see.
    let dl_not_found = "Book file not found";
    let dl_invalid = "Book file could not be read";
    let file_path = state
        .resolve_book_path(&book)
        .map_err(|e| book_file_status(dl_not_found, dl_invalid, e))?;

    // R3-2: Stream the file instead of reading entirely into memory
    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|e| book_file_status(dl_not_found, dl_invalid, e))?;

    let metadata = file
        .metadata()
        .await
        .map_err(|e| book_file_status(dl_not_found, dl_invalid, e))?;

    let filename = std::path::Path::new(&file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("book");

    let mime = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::CONTENT_LENGTH, metadata.len().to_string()),
        ],
        body,
    )
        .into_response())
}

/// Same as [`download_book`] but with a trailing filename segment that is
/// discarded. The OPDS feed emits URLs of the form `/download/{id}.{ext}`
/// so OPDS clients can key off the URL extension when the MIME is ambiguous
/// (e.g. AZW vs AZW3 both use `application/vnd.amazon.ebook`).
async fn download_book_with_filename(
    state: State<WebState>,
    Path((id, _filename)): Path<(String, String)>,
) -> Result<Response, (StatusCode, String)> {
    download_book(state, Path(id)).await
}

// ── Collections ──────────────────────────────────────────────────────────────

async fn list_series(
    State(state): State<WebState>,
) -> Result<Json<Vec<crate::models::SeriesInfo>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let series = db::list_series(&conn).map_err(carrel_status)?;
    Ok(Json(series))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionWithCount {
    id: String,
    name: String,
    r#type: crate::models::CollectionType,
    icon: Option<String>,
    color: Option<String>,
    created_at: i64,
    updated_at: i64,
    rules: Vec<crate::models::CollectionRule>,
    book_count: usize,
}

async fn list_collections(
    State(state): State<WebState>,
) -> Result<Json<Vec<CollectionWithCount>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let collections = db::list_collections(&conn).map_err(carrel_status)?;

    let result: Vec<CollectionWithCount> = collections
        .into_iter()
        .map(|c| {
            let book_count = db::get_books_in_collection_grid(&conn, &c.id)
                .map(|books| books.len())
                .unwrap_or(0);
            CollectionWithCount {
                id: c.id,
                name: c.name,
                r#type: c.r#type,
                icon: c.icon,
                color: c.color,
                created_at: c.created_at,
                updated_at: c.updated_at,
                rules: c.rules,
                book_count,
            }
        })
        .collect();

    Ok(Json(result))
}

async fn get_collection_books(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<PublicBookGridItem>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let books = db::get_books_in_collection_grid(&conn, &id).map_err(carrel_status)?;
    Ok(Json(books.into_iter().map(Into::into).collect()))
}

// ── Stats ───────────────────────────────────────────────────────────────────

async fn get_stats(
    State(state): State<WebState>,
) -> Result<Json<db::ReadingStats>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let stats = db::get_reading_stats(&conn).map_err(carrel_status)?;
    Ok(Json(stats))
}

// ── Dictionary (F-1-1 web) ───────────────────────────────────────────────────

/// Longest word accepted by `GET /api/dictionary/lookup` — generous for any
/// real dictionary headword, tight enough to reject junk queries early.
const DICTIONARY_WORD_MAX_LEN: usize = 100;

#[derive(serde::Serialize)]
struct DictionaryStatusResponse {
    installed: bool,
    enabled: bool,
    /// Whether `vocabulary_enabled` is on for the active profile (M3). Lets
    /// the web UI decide whether the Define popover offers "Save to
    /// vocabulary" — `POST /api/vocabulary` still succeeds either way (see
    /// its doc comment), this is purely so the button isn't shown for no
    /// reason.
    vocabulary: bool,
}

#[derive(serde::Deserialize)]
struct DictionaryLookupQuery {
    word: Option<String>,
}

/// Whether the per-profile boolean setting named `key` is on. Shared by
/// `dictionary_setting_enabled` and `vocabulary_setting_enabled` below, which
/// were byte-identical apart from the key.
fn bool_setting_enabled(
    conn: &rusqlite::Connection,
    key: &str,
) -> Result<bool, (StatusCode, String)> {
    Ok(db::get_setting(conn, key)
        .map_err(carrel_status)?
        .as_deref()
        == Some("true"))
}

/// Whether the per-profile `dictionary_enabled` setting is on. Re-checked
/// server-side on every request (defense in depth — the frontend already
/// gates the lookup UI), mirroring `log_vocabulary_word_entry`'s handling of
/// `vocabulary_enabled`.
fn dictionary_setting_enabled(conn: &rusqlite::Connection) -> Result<bool, (StatusCode, String)> {
    bool_setting_enabled(conn, "dictionary_enabled")
}

/// Whether the per-profile `vocabulary_enabled` setting is on. Same
/// defense-in-depth re-check as `dictionary_setting_enabled` above; used to
/// populate `GET /api/dictionary/status`'s `vocabulary` field (M3) and to
/// gate `POST /api/vocabulary` itself.
fn vocabulary_setting_enabled(conn: &rusqlite::Connection) -> Result<bool, (StatusCode, String)> {
    bool_setting_enabled(conn, "vocabulary_enabled")
}

/// Get (opening and caching on first use) the readonly pool over the
/// installed dictionary artifact. Mirrors desktop's `lookup_word` command;
/// the cache is shared with `AppState` (cloned in at server start), so a
/// desktop download/delete invalidates it for web lookups too. The
/// per-request `inspect()` calls below are what make a deletion or a
/// disabled setting visible between requests; the shared cache invalidation
/// is what prevents this warmed pool from going on serving a replaced
/// artifact's old inode after a re-download.
fn dictionary_readonly_pool(
    state: &WebState,
) -> crate::error::CarrelResult<carrel_core::db::DbPool> {
    let mut guard = state.dictionary_pool.lock()?;
    if guard.is_none() {
        *guard = Some(carrel_core::dictionary::open_readonly_pool(
            &state.dictionary_dir(),
        )?);
    }
    Ok(guard.as_ref().expect("pool populated above").clone())
}

async fn get_dictionary_status(
    State(state): State<WebState>,
) -> Result<Json<DictionaryStatusResponse>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let enabled = dictionary_setting_enabled(&conn)?;
    let vocabulary = vocabulary_setting_enabled(&conn)?;
    drop(conn);
    let status = carrel_core::dictionary::inspect(&state.dictionary_dir());
    let installed = status.state == carrel_core::dictionary::DictionaryState::Ready;
    Ok(Json(DictionaryStatusResponse {
        installed,
        enabled,
        vocabulary,
    }))
}

async fn lookup_dictionary_word(
    State(state): State<WebState>,
    Query(params): Query<DictionaryLookupQuery>,
) -> Result<Json<carrel_core::dictionary::DictionaryEntry>, (StatusCode, String)> {
    let word = params.word.unwrap_or_default();
    let word = word.trim();
    if word.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "word must not be empty".to_string(),
        ));
    }
    if word.chars().count() > DICTIONARY_WORD_MAX_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("word exceeds {DICTIONARY_WORD_MAX_LEN} characters"),
        ));
    }

    let conn = state.conn().map_err(carrel_status)?;
    let enabled = dictionary_setting_enabled(&conn)?;
    drop(conn);
    if !enabled {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Dictionary is not enabled".to_string(),
        ));
    }
    let status = carrel_core::dictionary::inspect(&state.dictionary_dir());
    if status.state != carrel_core::dictionary::DictionaryState::Ready {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Dictionary is not installed".to_string(),
        ));
    }

    let pool = dictionary_readonly_pool(&state).map_err(carrel_status)?;
    let dict_conn = pool.get().map_err(carrel_status)?;
    match carrel_core::dictionary::lookup(&dict_conn, word).map_err(carrel_status)? {
        Some(entry) => Ok(Json(entry)),
        None => Err((StatusCode::NOT_FOUND, "Word not found".to_string())),
    }
}

// ── Vocabulary builder (F-1-5 web, M3) ──────────────────────────────────────

/// Longest `word`/`definition` accepted by `POST /api/vocabulary` — generous
/// for a real headword/gloss snapshot, tight enough to reject junk bodies.
const VOCABULARY_WORD_MAX_LEN: usize = 200;
const VOCABULARY_DEFINITION_MAX_LEN: usize = 2000;
/// `lemma` is the dedup key (see `log_vocabulary_word_entry`'s `UNIQUE`
/// constraint), so it gets the same cap as `word`.
const VOCABULARY_LEMMA_MAX_LEN: usize = 200;
const VOCABULARY_POS_MAX_LEN: usize = 50;
const VOCABULARY_BOOK_TITLE_MAX_LEN: usize = 500;
/// Matches desktop's `MAX_CONTEXT_CHARS` (`src/lib/vocabulary.ts`) — the web
/// client truncates to this before sending, this is the server-side backstop.
const VOCABULARY_CONTEXT_SENTENCE_MAX_LEN: usize = 300;

/// POST body. camelCase, like `HighlightCreate` above (the web client sends
/// camelCase for both). Mirrors the fields of `commands::log_vocabulary_word`
/// one-for-one; this route shares that command's logic (see
/// `commands::log_vocabulary_word_entry`) rather than reimplementing it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VocabularyCreate {
    word: String,
    lemma: String,
    #[serde(default)]
    pos: Option<String>,
    definition: String,
    #[serde(default)]
    book_id: Option<String>,
    #[serde(default)]
    book_title: Option<String>,
    #[serde(default)]
    chapter_index: Option<i64>,
    #[serde(default)]
    context_sentence: Option<String>,
    #[serde(default)]
    start_offset: Option<i64>,
    #[serde(default)]
    end_offset: Option<i64>,
}

/// Save a looked-up word to the vocabulary builder from the web Define
/// popover. Delegates to `commands::log_vocabulary_word_entry` — the exact
/// logic desktop's `log_vocabulary_word` IPC command runs — so the two
/// surfaces can never drift apart on dedup-by-lemma, seen-count bumping, or
/// the `vocabulary_enabled` re-check.
///
/// `vocabulary_enabled` off is a 403 here, checked before the delegation.
/// The entry fn's own silent no-op stays as the backstop, but a silent
/// success is the wrong answer over HTTP: the web UI flips its Save button to
/// "Saved ✓" on 2xx, so a no-op would claim a save that never happened and
/// lose the word. An honest failure is something a stale tab (one that
/// fetched `GET /api/dictionary/status` before the setting flipped off) can
/// react to — it drops its cached status and stops offering Save.
async fn create_vocabulary_word(
    State(state): State<WebState>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    // Manual parse (like `create_highlight`/`put_progress`) so malformed
    // bodies map to 400 rather than axum's built-in 422.
    let body: VocabularyCreate = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    let word = body.word.trim();
    if word.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "word is empty".to_string()));
    }
    if word.chars().count() > VOCABULARY_WORD_MAX_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("word exceeds {VOCABULARY_WORD_MAX_LEN} characters"),
        ));
    }
    let definition = body.definition.trim();
    if definition.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "definition is empty".to_string()));
    }
    if definition.chars().count() > VOCABULARY_DEFINITION_MAX_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("definition exceeds {VOCABULARY_DEFINITION_MAX_LEN} characters"),
        ));
    }
    // `lemma` is the dedup key, so it is validated as strictly as `word` —
    // and trimmed before it reaches the UNIQUE column, since " cat" and
    // "cat" would otherwise be two rows for one word.
    let lemma = body.lemma.trim();
    if lemma.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "lemma is empty".to_string()));
    }
    if lemma.chars().count() > VOCABULARY_LEMMA_MAX_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("lemma exceeds {VOCABULARY_LEMMA_MAX_LEN} characters"),
        ));
    }
    // The remaining free-text fields are capped rather than required: they
    // are display context, but they are still LAN-reachable writes into a
    // table the desktop UI renders, so none of them may be unbounded.
    let pos = match body.pos.as_deref().map(str::trim) {
        Some(p) if p.chars().count() > VOCABULARY_POS_MAX_LEN => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("pos exceeds {VOCABULARY_POS_MAX_LEN} characters"),
            ));
        }
        Some("") | None => None,
        Some(p) => Some(p.to_string()),
    };
    if let Some(title) = &body.book_title {
        if title.chars().count() > VOCABULARY_BOOK_TITLE_MAX_LEN {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("bookTitle exceeds {VOCABULARY_BOOK_TITLE_MAX_LEN} characters"),
            ));
        }
    }
    if let Some(sentence) = &body.context_sentence {
        if sentence.chars().count() > VOCABULARY_CONTEXT_SENTENCE_MAX_LEN {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("contextSentence exceeds {VOCABULARY_CONTEXT_SENTENCE_MAX_LEN} characters"),
            ));
        }
    }
    // Offsets and chapter index feed desktop's jump-to-context navigation
    // (`VocabularyPanel`), so nonsense here lands the reader on the wrong
    // chapter later with no visible cause. Same rigor as `create_highlight`.
    for (name, value) in [
        ("chapterIndex", body.chapter_index),
        ("startOffset", body.start_offset),
        ("endOffset", body.end_offset),
    ] {
        if value.is_some_and(|v| v < 0) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{name} must not be negative"),
            ));
        }
    }
    if let (Some(start), Some(end)) = (body.start_offset, body.end_offset) {
        if end <= start {
            return Err((
                StatusCode::BAD_REQUEST,
                "endOffset must be greater than startOffset".to_string(),
            ));
        }
    }

    let conn = state.conn().map_err(carrel_status)?;
    if !vocabulary_setting_enabled(&conn)? {
        return Err((StatusCode::FORBIDDEN, "Vocabulary is disabled".to_string()));
    }
    // Same rigor as `create_highlight`'s book check: a bookId that doesn't
    // name a real book 404s explicitly here. It normally avoids a raw
    // FK-constraint error from the `vocabulary.book_id` foreign key, though
    // no transaction spans this check and the insert below — a book deleted
    // in between still surfaces as a 500.
    if let Some(book_id) = &body.book_id {
        db::get_book(&conn, book_id)
            .map_err(carrel_status)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Book not found".to_string()))?;
    }

    crate::commands::log_vocabulary_word_entry(
        &conn,
        word.to_string(),
        lemma.to_string(),
        pos,
        definition.to_string(),
        body.book_id,
        body.book_title,
        body.chapter_index,
        body.context_sentence,
        body.start_offset,
        body.end_offset,
    )
    .map_err(carrel_status)?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/vocabulary` query params (M4). `bookId` filters to one book's
/// words for the reader-drawer view; omitted, every row is returned (M5's
/// full-list screen).
#[derive(serde::Deserialize)]
struct VocabularyListQuery {
    #[serde(rename = "bookId")]
    book_id: Option<String>,
}

/// List saved vocabulary words, optionally scoped to one book. Filtered in
/// the handler rather than in `carrel-core` (see the epic plan) — that crate
/// is a git dependency for Carrel Server and gets no new query variants for a
/// web-only view. A `bookId` matching no real book returns an empty list, not
/// 404: `vocabulary.book_id` is nullable, so a word survives its source
/// book's deletion, and "no rows" is the honest answer either way.
async fn list_vocabulary_words(
    State(state): State<WebState>,
    Query(params): Query<VocabularyListQuery>,
) -> Result<Json<Vec<crate::models::VocabularyWord>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    if !vocabulary_setting_enabled(&conn)? {
        return Err((StatusCode::FORBIDDEN, "Vocabulary is disabled".to_string()));
    }
    let words = db::list_vocabulary(&conn).map_err(carrel_status)?;
    let words = match params.book_id {
        Some(book_id) => words
            .into_iter()
            .filter(|w| w.book_id.as_deref() == Some(book_id.as_str()))
            .collect(),
        None => words,
    };
    Ok(Json(words))
}

/// Delete a saved vocabulary word. Idempotent (same as `delete_highlight_route`):
/// an unknown id affects 0 rows and still returns 204.
async fn delete_vocabulary_word_route(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    if !vocabulary_setting_enabled(&conn)? {
        return Err((StatusCode::FORBIDDEN, "Vocabulary is disabled".to_string()));
    }
    db::delete_vocabulary_word(&conn, &id).map_err(carrel_status)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/vocabulary/due` query params (M6). `limit` is caller-suggested
/// only — never trusted verbatim, see [`clamp_due_limit`].
#[derive(serde::Deserialize)]
struct VocabularyDueQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// Default and hard-cap for `GET /api/vocabulary/due`'s `limit`. The cap is
/// the same 200 as desktop's `VocabularyPanel.tsx` `REVIEW_LIMIT` — a generous
/// safety bound on a personal vocabulary list, not real pagination — but it is
/// enforced here rather than trusted from the client, because this route is
/// LAN-reachable. Keeping the two equal matters: the web UI labels the returned
/// length as a due *count*, so a lower cap here would understate it and then
/// let the UI claim the queue was finished with cards still due.
const VOCABULARY_DUE_DEFAULT_LIMIT: i64 = 20;
const VOCABULARY_DUE_MAX_LIMIT: i64 = 200;

/// Clamp a caller-supplied `limit` into `1..=VOCABULARY_DUE_MAX_LIMIT`,
/// defaulting to `VOCABULARY_DUE_DEFAULT_LIMIT` when absent. A non-positive
/// value is treated the same as "too high" (clamped up to 1) rather than
/// rejected — this is a read endpoint, so there is no invalid input to 400
/// on, only a nonsensical request to make harmless.
fn clamp_due_limit(limit: Option<i64>) -> i64 {
    limit
        .unwrap_or(VOCABULARY_DUE_DEFAULT_LIMIT)
        .clamp(1, VOCABULARY_DUE_MAX_LIMIT)
}

/// List vocabulary rows due for review (M6 flashcard queue). `now` is always
/// the server clock — a client-supplied time would let a review session be
/// replayed or postponed by lying about it.
async fn get_due_vocabulary_words(
    State(state): State<WebState>,
    Query(params): Query<VocabularyDueQuery>,
) -> Result<Json<Vec<crate::models::VocabularyWord>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    if !vocabulary_setting_enabled(&conn)? {
        return Err((StatusCode::FORBIDDEN, "Vocabulary is disabled".to_string()));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let limit = clamp_due_limit(params.limit);
    let words = db::due_vocabulary(&conn, now, limit).map_err(carrel_status)?;
    Ok(Json(words))
}

/// `POST /api/vocabulary/{id}/review` body.
#[derive(serde::Deserialize)]
struct VocabularyReviewBody {
    correct: bool,
}

/// Score a flashcard review and persist the result. Delegates to
/// `commands::record_vocabulary_review_now` — the exact logic desktop's
/// `record_vocabulary_review` IPC command runs — so the two surfaces cannot
/// drift on Leitner-box scheduling.
///
/// An unknown id is a 404: `db::record_vocabulary_review` reads the row's
/// current box before scoring it, which would otherwise surface as an opaque
/// 500 (a bare SQLite "no rows returned" maps to `CarrelError::Database`, not
/// `NotFound`). Checked explicitly here rather than teaching that mapping to
/// carrel-core, which is a git dependency for Carrel Server. No transaction
/// spans the check and the update below, so a delete landing in that window
/// (another tab/device) still surfaces as a 500 — same accepted race as
/// `create_vocabulary_word`'s book-id check above.
async fn review_vocabulary_word_route(
    State(state): State<WebState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    // Manual parse (like `create_vocabulary_word`) so a malformed body maps to
    // 400 rather than axum's built-in 422.
    let body: VocabularyReviewBody = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;

    let conn = state.conn().map_err(carrel_status)?;
    if !vocabulary_setting_enabled(&conn)? {
        return Err((StatusCode::FORBIDDEN, "Vocabulary is disabled".to_string()));
    }
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM vocabulary WHERE id = ?1",
            rusqlite::params![id],
            |_| Ok(()),
        )
        .optional()
        .map_err(carrel_status)?
        .is_some();
    if !exists {
        return Err((StatusCode::NOT_FOUND, "Word not found".to_string()));
    }

    crate::commands::record_vocabulary_review_now(&conn, &id, body.correct)
        .map_err(carrel_status)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `WebState` for dictionary handler tests, rooted at `data_dir`
    /// (a fresh tempdir per test, so `write_test_artifact` and the settings
    /// pool never leak between tests). Mirrors `mod::tests::test_state`.
    fn dictionary_test_state(data_dir: std::path::PathBuf) -> WebState {
        let pool =
            crate::db::create_pool(&std::path::PathBuf::from(":memory:")).expect("in-memory DB");
        WebState {
            archives: carrel_core::reader::ArchiveCaches::with_capacity(2),
            pool: std::sync::Arc::new(std::sync::Mutex::new(pool)),
            data_dir,
            cache_dir: std::env::temp_dir(),
            pin_hash: std::sync::Arc::new(std::sync::Mutex::new(None)),
            sessions: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            login_limiter: std::sync::Arc::new(super::super::auth::RateLimiter::new(5, 300)),
            active_profile_name: std::sync::Arc::new(std::sync::Mutex::new("default".to_string())),
            unlocked_profiles: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::from(["default".to_string()]),
            )),
            private_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// A linked EPUB book row pointing at `path`, seeded into `state`'s DB.
    fn seed_epub_book(state: &WebState, id: &str, path: &std::path::Path) {
        let book = crate::models::Book {
            id: id.to_string(),
            title: "Chapter Read Test Book".to_string(),
            author: "Test Author".to_string(),
            file_path: path.to_string_lossy().into_owned(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: BookFormat::Epub,
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
        };
        let conn = state.conn().unwrap();
        crate::db::insert_book(&conn, &book).unwrap();
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn body_bytes(resp: Response) -> Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    /// A one-page CBZ containing a real (decodable) JPEG, so the page-cache
    /// round trip below is byte-meaningful rather than just "some bytes
    /// passed through".
    fn write_cbz(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("page00.jpg", opts).unwrap();
        let img: image::ImageBuffer<image::Rgb<u8>, _> =
            image::ImageBuffer::from_pixel(20, 20, image::Rgb([200u8, 30, 30]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(&img)
            .unwrap();
        std::io::Write::write_all(&mut zip, &jpeg).unwrap();
        zip.finish().unwrap();
        path
    }

    /// A one-page CBZ whose page is exactly `page_bytes` long (arbitrary
    /// content — nothing here decodes it, so it need not be a real image).
    /// Used to make the page cache's disk footprint large enough to trigger
    /// `run_eviction`'s size cap deterministically in a test.
    fn write_large_cbz(dir: &std::path::Path, name: &str, page_bytes: usize) -> std::path::PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("page00.jpg", opts).unwrap();
        std::io::Write::write_all(&mut zip, &vec![0xABu8; page_bytes]).unwrap();
        zip.finish().unwrap();
        path
    }

    /// A linked CBZ book row pointing at `path`, seeded into `state`'s DB.
    fn seed_comic_book(
        state: &WebState,
        id: &str,
        path: &std::path::Path,
        file_hash: Option<&str>,
    ) {
        let book = crate::models::Book {
            id: id.to_string(),
            title: "Comic Page Read Test Book".to_string(),
            author: "Test Author".to_string(),
            file_path: path.to_string_lossy().into_owned(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: BookFormat::Cbz,
            file_hash: file_hash.map(|s| s.to_string()),
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
        };
        let conn = state.conn().unwrap();
        crate::db::insert_book(&conn, &book).unwrap();
    }

    /// Comic-page tests each need an isolated `cache_dir` — unlike
    /// `dictionary_test_state`'s shared `std::env::temp_dir()`, the page
    /// cache is written to, so sharing it across tests risks a book-hash
    /// collision.
    fn comic_test_state(dir: &std::path::Path) -> WebState {
        let mut state = dictionary_test_state(dir.to_path_buf());
        state.cache_dir = dir.join("cache");
        state
    }

    /// M1's acceptance criterion: a second chapter request is served from the
    /// cached archive rather than by reopening the book file, which is what
    /// this route did on *every* request before.
    ///
    /// Deleting the file between the two requests is what makes that
    /// observable — asserting two 200s would pass with the caching removed.
    #[tokio::test]
    async fn web_chapter_reads_reuse_the_cached_archive() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        let epub = crate::test_epub::write_epub_with_image_chapter(dir.path(), "web-chapter");
        seed_epub_book(&state, "bk-web", &epub);

        let first = get_chapter_content(State(state.clone()), Path(("bk-web".to_string(), 0usize)))
            .await
            .unwrap();
        let first = body_string(first).await;
        assert!(first.contains("Hello"), "unexpected first body: {first}");
        assert_eq!(state.archives.epub.lock().unwrap().len(), 1);

        std::fs::remove_file(&epub).unwrap();

        let second =
            get_chapter_content(State(state.clone()), Path(("bk-web".to_string(), 0usize)))
                .await
                .expect("second read must be served from the cached archive");
        assert_eq!(body_string(second).await, first);
        assert_eq!(state.archives.epub.lock().unwrap().len(), 1);
    }

    /// M1 follow-on's acceptance criterion: a second comic page request is
    /// served from the disk page cache rather than by reopening the archive,
    /// which is what this route did on *every* request before.
    ///
    /// Deleting the file between the two requests is what makes that
    /// observable — asserting two 200s would pass with the caching removed.
    #[tokio::test]
    async fn web_comic_page_reads_reuse_the_page_cache() {
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let cbz = write_cbz(dir.path(), "comic.cbz");
        seed_comic_book(&state, "bk-comic", &cbz, Some("comic-hash-1"));

        let first = get_page_image(
            State(state.clone()),
            Path(("bk-comic".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .unwrap();
        let first_bytes = body_bytes(first).await;
        assert!(!first_bytes.is_empty());

        std::fs::remove_file(&cbz).unwrap();

        let second = get_page_image(
            State(state.clone()),
            Path(("bk-comic".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .expect("second read must be served from the cached page");
        assert_eq!(body_bytes(second).await, first_bytes);
    }

    /// A book with no `file_hash` has nothing to key the page cache on — the
    /// route must still serve it (uncached) rather than erroring.
    #[tokio::test]
    async fn web_comic_page_without_hash_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let cbz = write_cbz(dir.path(), "comic.cbz");
        seed_comic_book(&state, "bk-comic-nohash", &cbz, None);

        let resp = get_page_image(
            State(state.clone()),
            Path(("bk-comic-nohash".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .expect("a book with no file_hash must still render");
        assert!(!body_bytes(resp).await.is_empty());
    }

    /// Trap from the M1 brief: a missing comic file must stay a 404, not
    /// collapse into the generic 500 `book_file_status` maps unrecognized
    /// error kinds to (see `carrel_core::reader::page_image`'s
    /// `ensure_file_exists` guard, added for exactly this).
    #[tokio::test]
    async fn web_comic_page_missing_file_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let missing = dir.path().join("nope.cbz");
        seed_comic_book(&state, "bk-comic-missing", &missing, Some("comic-hash-3"));

        let err = get_page_image(
            State(state.clone()),
            Path(("bk-comic-missing".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND, "unexpected status: {err:?}");
    }

    #[tokio::test]
    async fn web_comic_page_count_returns_number_of_pages() {
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let cbz = write_cbz(dir.path(), "comic.cbz");
        seed_comic_book(&state, "bk-comic-count", &cbz, Some("comic-hash-2"));

        let resp = get_page_count(State(state.clone()), Path("bk-comic-count".to_string()))
            .await
            .unwrap();
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["count"], 1);
    }

    /// M1 review finding 3: the eviction budget passed to `run_eviction`
    /// must be the user's `page_cache_max_size_mb` setting, not a hardcoded
    /// default — otherwise a lowered budget is silently ignored whenever
    /// the web route is what primes the cache, and the desktop and web
    /// halves of one install disagree about the same directory on disk.
    ///
    /// Sets the setting to 1 MB, then primes two ~700 KB comics — together
    /// over budget, neither alone. If eviction saw the configured 1 MB cap,
    /// priming the second book must reclaim the first; if it silently used
    /// `DEFAULT_MAX_CACHE_SIZE_MB` (500 MB) instead, both would still fit
    /// and neither would ever be evicted.
    #[tokio::test]
    async fn web_comic_page_eviction_honors_the_configured_cache_size_setting() {
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "page_cache_max_size_mb", "1").unwrap();
        }

        let cbz_a = write_large_cbz(dir.path(), "comic-a.cbz", 700 * 1024);
        seed_comic_book(&state, "bk-comic-a", &cbz_a, Some("comic-hash-a"));
        let cbz_b = write_large_cbz(dir.path(), "comic-b.cbz", 700 * 1024);
        seed_comic_book(&state, "bk-comic-b", &cbz_b, Some("comic-hash-b"));

        get_page_image(
            State(state.clone()),
            Path(("bk-comic-a".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .unwrap();
        get_page_image(
            State(state.clone()),
            Path(("bk-comic-b".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .unwrap();

        let pages_storage = state.pages_storage().unwrap();
        let a_present =
            carrel_core::page_cache::read_manifest(pages_storage.as_ref(), "comic-hash-a")
                .is_some();
        let b_present =
            carrel_core::page_cache::read_manifest(pages_storage.as_ref(), "comic-hash-b")
                .is_some();
        assert!(
            !(a_present && b_present),
            "at least one book must be evicted once the combined cache exceeds the \
             configured 1 MB budget; both present means eviction used a larger budget \
             than what was set"
        );
    }

    // ── PDF page reads (M2) ─────────────────────────────────────────────

    /// Point pdfium at the bundled library and return whether it is usable.
    ///
    /// Mirrors `carrel_core::pdf::tests::pdfium_available`: the binary is
    /// downloaded by `scripts/download-pdfium.sh` (and by CI on
    /// Linux/macOS) into `src-tauri/resources/`, which is gitignored — a
    /// fresh clone that skipped the script, and the Windows CI job (which
    /// builds this test binary but has no library at all), skip PDF tests
    /// rather than fail them.
    fn pdfium_available() -> bool {
        let lib_name = if cfg!(target_os = "windows") {
            "pdfium.dll"
        } else if cfg!(target_os = "macos") {
            "libpdfium.dylib"
        } else {
            "libpdfium.so"
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(lib_name);
        if !path.exists() {
            return false;
        }
        carrel_core::pdf::set_pdfium_library_path(Some(path));
        true
    }

    /// A minimal one-page, hand-crafted PDF — no PDF-writing crate needed.
    /// Mirrors `carrel_core::reader::tests::write_pdf`.
    fn write_pdf(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let content = b"0 0 1 rg 0 0 10 10 re f\n";
        let bodies: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Contents 4 0 R/Resources<<>>>>"
                .to_vec(),
            format!(
                "<</Length {}>>stream\n{}\nendstream",
                content.len(),
                std::str::from_utf8(content).unwrap()
            )
            .into_bytes(),
        ];

        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"endobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", bodies.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{}\n%%EOF\n",
                bodies.len() + 1,
                xref
            )
            .as_bytes(),
        );

        let path = dir.join(name);
        std::fs::write(&path, out).unwrap();
        path
    }

    /// A linked PDF book row pointing at `path`, seeded into `state`'s DB.
    fn seed_pdf_book(state: &WebState, id: &str, path: &std::path::Path, file_hash: Option<&str>) {
        let book = crate::models::Book {
            id: id.to_string(),
            title: "PDF Page Read Test Book".to_string(),
            author: "Test Author".to_string(),
            file_path: path.to_string_lossy().into_owned(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: BookFormat::Pdf,
            file_hash: file_hash.map(|s| s.to_string()),
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
        };
        let conn = state.conn().unwrap();
        crate::db::insert_book(&conn, &book).unwrap();
    }

    /// M2's acceptance criterion: a second PDF page request is served from
    /// the disk page cache rather than by re-rendering with pdfium, which is
    /// what this route did on *every* request before.
    ///
    /// Deleting the file between the two requests is what makes that
    /// observable — asserting two 200s would pass with the caching removed.
    #[tokio::test]
    async fn web_pdf_page_reads_reuse_the_page_cache() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let pdf = write_pdf(dir.path(), "book.pdf");
        seed_pdf_book(&state, "bk-pdf", &pdf, Some("pdf-hash-1"));

        let first = get_page_image(
            State(state.clone()),
            Path(("bk-pdf".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .unwrap();
        let first_bytes = body_bytes(first).await;
        assert!(!first_bytes.is_empty());

        std::fs::remove_file(&pdf).unwrap();

        let second = get_page_image(
            State(state.clone()),
            Path(("bk-pdf".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .expect("second read must be served from the cached page");
        assert_eq!(body_bytes(second).await, first_bytes);
    }

    /// M2's private-mode requirement, proven at the web-route level (not
    /// just core): with `WebState::private_mode` on, the page still renders,
    /// but the page BYTES must never reach disk — only `is_private` reaching
    /// `carrel_core::reader::page_image` unchanged makes that true; a wiring
    /// bug that dropped or hardcoded the flag would still pass a test that
    /// only checked the response was 200.
    #[tokio::test]
    async fn web_pdf_page_private_mode_does_not_write_page_bytes() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        state
            .private_mode
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pdf = write_pdf(dir.path(), "book.pdf");
        seed_pdf_book(&state, "bk-pdf-private", &pdf, Some("pdf-hash-private"));

        let resp = get_page_image(
            State(state.clone()),
            Path(("bk-pdf-private".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .expect("private mode must still render the page");
        assert!(!body_bytes(resp).await.is_empty());

        let pages_storage = state.pages_storage().unwrap();
        let entries = pages_storage.list("page-cache/pdf-hash-private/").unwrap();
        assert!(
            !entries.iter().any(|e| e.ends_with(".jpg")),
            "private mode must not write page bytes to disk: {entries:?}"
        );
    }

    /// A PDF with no `file_hash` has nothing to key the page cache on — the
    /// route must still serve it (uncached) rather than erroring.
    #[tokio::test]
    async fn web_pdf_page_without_hash_still_renders() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = comic_test_state(dir.path());
        let pdf = write_pdf(dir.path(), "book.pdf");
        seed_pdf_book(&state, "bk-pdf-nohash", &pdf, None);

        let resp = get_page_image(
            State(state.clone()),
            Path(("bk-pdf-nohash".to_string(), 0u32)),
            axum::extract::RawQuery(None),
        )
        .await
        .expect("a book with no file_hash must still render");
        assert!(!body_bytes(resp).await.is_empty());
    }

    #[tokio::test]
    async fn dictionary_status_reports_not_installed_on_empty_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());

        let Json(status) = get_dictionary_status(State(state)).await.unwrap();

        assert!(!status.installed);
        assert!(!status.enabled);
        assert!(!status.vocabulary);
    }

    #[tokio::test]
    async fn dictionary_status_reports_installed_and_enabled() {
        let dir = tempfile::tempdir().unwrap();
        carrel_core::dictionary::write_test_artifact(&dir.path().join("dictionary")).unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "dictionary_enabled", "true").unwrap();
        }

        let Json(status) = get_dictionary_status(State(state)).await.unwrap();

        assert!(status.installed);
        assert!(status.enabled);
        // vocabulary_enabled was never set — false by default (M3).
        assert!(!status.vocabulary);
    }

    #[tokio::test]
    async fn dictionary_status_reports_vocabulary_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let Json(status) = get_dictionary_status(State(state)).await.unwrap();

        assert!(status.vocabulary);
    }

    #[tokio::test]
    async fn dictionary_lookup_returns_seeded_word() {
        let dir = tempfile::tempdir().unwrap();
        carrel_core::dictionary::write_test_artifact(&dir.path().join("dictionary")).unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "dictionary_enabled", "true").unwrap();
        }

        let Json(entry) = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("cat".to_string()),
            }),
        )
        .await
        .unwrap();

        assert_eq!(entry.matched_word, "cat");
        assert_eq!(entry.senses[0].gloss, "feline mammal");
    }

    /// Simulates desktop's `download_dictionary`/`delete_dictionary`
    /// invalidating the shared cache mid-run: after a lookup has warmed the
    /// pool, clearing `dictionary_pool` (as those commands do) and swapping
    /// in a fresh artifact must be picked up by the next lookup, proving the
    /// lazy re-open works through the shared handle rather than going stale.
    #[tokio::test]
    async fn dictionary_lookup_reopens_pool_after_shared_cache_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let dict_dir = dir.path().join("dictionary");
        carrel_core::dictionary::write_test_artifact(&dict_dir).unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "dictionary_enabled", "true").unwrap();
        }

        // Warm the pool.
        let Json(entry) = lookup_dictionary_word(
            State(state.clone()),
            Query(DictionaryLookupQuery {
                word: Some("cat".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(entry.matched_word, "cat");

        // Simulate desktop-side invalidation: clear the shared cache and
        // replace the artifact with a fresh copy.
        *state.dictionary_pool.lock().unwrap() = None;
        std::fs::remove_dir_all(&dict_dir).unwrap();
        carrel_core::dictionary::write_test_artifact(&dict_dir).unwrap();

        let Json(entry) = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("cat".to_string()),
            }),
        )
        .await
        .unwrap();

        assert_eq!(entry.matched_word, "cat");
    }

    #[tokio::test]
    async fn dictionary_lookup_unknown_word_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        carrel_core::dictionary::write_test_artifact(&dir.path().join("dictionary")).unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "dictionary_enabled", "true").unwrap();
        }

        let err = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("zzznotaword".to_string()),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dictionary_lookup_disabled_setting_is_service_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        // Artifact is installed, but `dictionary_enabled` is left unset.
        carrel_core::dictionary::write_test_artifact(&dir.path().join("dictionary")).unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("cat".to_string()),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn dictionary_lookup_artifact_absent_is_service_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        // `dictionary_enabled` is on, but no artifact was ever installed.
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "dictionary_enabled", "true").unwrap();
        }

        let err = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("cat".to_string()),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn dictionary_lookup_empty_word_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("   ".to_string()),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dictionary_lookup_overlong_word_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = lookup_dictionary_word(
            State(state),
            Query(DictionaryLookupQuery {
                word: Some("a".repeat(DICTIONARY_WORD_MAX_LEN + 1)),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    // ── POST /api/vocabulary (M3) ───────────────────────────────────────────

    fn vocabulary_test_book(id: &str) -> crate::models::Book {
        crate::models::Book {
            id: id.to_string(),
            title: "Vocabulary Test Book".to_string(),
            author: "Author".to_string(),
            file_path: format!("/nonexistent/vocab-test-{id}.epub"),
            cover_path: None,
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

    fn vocabulary_body(extra: &str) -> Bytes {
        Bytes::from(format!(
            r#"{{"word":"cat","lemma":"cat","pos":"noun","definition":"feline mammal"{extra}}}"#
        ))
    }

    #[tokio::test]
    async fn create_vocabulary_word_saves_row_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let status = create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        let words = db::list_vocabulary(&conn).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].lemma, "cat");
        assert_eq!(words[0].definition, "feline mammal");
    }

    #[tokio::test]
    async fn create_vocabulary_word_disabled_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        // vocabulary_enabled is left unset (defaults to off).
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap_err();
        // An honest failure, not a silent no-op: the web UI turns 2xx into
        // "Saved ✓", so success here would claim a save that never landed.
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        let conn = state.conn().unwrap();
        let words = db::list_vocabulary(&conn).unwrap();
        assert!(words.is_empty());
    }

    #[tokio::test]
    async fn create_vocabulary_word_empty_lemma_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let body = Bytes::from(r#"{"word":"cat","lemma":"  ","definition":"feline mammal"}"#);
        let err = create_vocabulary_word(State(state), body)
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    /// The lemma is the UNIQUE dedup key, so a padded one must not become a
    /// second row for a word already saved.
    #[tokio::test]
    async fn create_vocabulary_word_trims_lemma_before_storing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let body = Bytes::from(r#"{"word":"cat","lemma":"  cat  ","definition":"feline mammal"}"#);
        let status = create_vocabulary_word(State(state.clone()), body)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        let words = db::list_vocabulary(&conn).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].lemma, "cat");
    }

    #[tokio::test]
    async fn create_vocabulary_word_overlong_context_sentence_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let long = "a".repeat(VOCABULARY_CONTEXT_SENTENCE_MAX_LEN + 1);
        let err = create_vocabulary_word(
            State(state),
            vocabulary_body(&format!(r#","contextSentence":"{long}""#)),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_negative_offset_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let err = create_vocabulary_word(State(state), vocabulary_body(r#","startOffset":-5"#))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_inverted_offsets_are_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let err = create_vocabulary_word(
            State(state),
            vocabulary_body(r#","startOffset":40,"endOffset":10"#),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_empty_word_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let body = Bytes::from(r#"{"word":"   ","lemma":"cat","definition":"feline mammal"}"#);
        let err = create_vocabulary_word(State(state), body)
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_overlong_word_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let overlong = "a".repeat(VOCABULARY_WORD_MAX_LEN + 1);
        let body = Bytes::from(
            serde_json::json!({
                "word": overlong,
                "lemma": "cat",
                "definition": "feline mammal",
            })
            .to_string(),
        );
        let err = create_vocabulary_word(State(state), body)
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_overlong_definition_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let overlong = "a".repeat(VOCABULARY_DEFINITION_MAX_LEN + 1);
        let body = Bytes::from(
            serde_json::json!({
                "word": "cat",
                "lemma": "cat",
                "definition": overlong,
            })
            .to_string(),
        );
        let err = create_vocabulary_word(State(state), body)
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_vocabulary_word_unknown_book_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let body = vocabulary_body(r#","bookId":"does-not-exist""#);
        let err = create_vocabulary_word(State(state), body)
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_vocabulary_word_with_real_book_id_saves_row() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
            db::insert_book(&conn, &vocabulary_test_book("book-1")).unwrap();
        }

        let body = vocabulary_body(
            r#","bookId":"book-1","bookTitle":"Vocabulary Test Book","chapterIndex":2"#,
        );
        let status = create_vocabulary_word(State(state.clone()), body)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        let words = db::list_vocabulary(&conn).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].book_id.as_deref(), Some("book-1"));
        assert_eq!(words[0].chapter_index, Some(2));
    }

    // ── GET/DELETE /api/vocabulary (M4) ─────────────────────────────────────

    #[tokio::test]
    async fn list_vocabulary_words_returns_all_rows_without_filter() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
            db::insert_book(&conn, &vocabulary_test_book("book-1")).unwrap();
            db::insert_book(&conn, &vocabulary_test_book("book-2")).unwrap();
        }
        create_vocabulary_word(
            State(state.clone()),
            vocabulary_body(r#","bookId":"book-1""#),
        )
        .await
        .unwrap();
        create_vocabulary_word(
            State(state.clone()),
            Bytes::from(
                r#"{"word":"dog","lemma":"dog","definition":"canine mammal","bookId":"book-2"}"#,
            ),
        )
        .await
        .unwrap();

        let Json(words) =
            list_vocabulary_words(State(state), Query(VocabularyListQuery { book_id: None }))
                .await
                .unwrap();

        assert_eq!(words.len(), 2);
    }

    #[tokio::test]
    async fn list_vocabulary_words_filters_by_book_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
            db::insert_book(&conn, &vocabulary_test_book("book-1")).unwrap();
            db::insert_book(&conn, &vocabulary_test_book("book-2")).unwrap();
        }
        create_vocabulary_word(
            State(state.clone()),
            vocabulary_body(r#","bookId":"book-1""#),
        )
        .await
        .unwrap();
        create_vocabulary_word(
            State(state.clone()),
            Bytes::from(
                r#"{"word":"dog","lemma":"dog","definition":"canine mammal","bookId":"book-2"}"#,
            ),
        )
        .await
        .unwrap();

        let Json(words) = list_vocabulary_words(
            State(state),
            Query(VocabularyListQuery {
                book_id: Some("book-1".to_string()),
            }),
        )
        .await
        .unwrap();

        assert_eq!(words.len(), 1);
        assert_eq!(words[0].lemma, "cat");
    }

    /// A word's source book can be deleted while the word survives
    /// (`vocabulary.book_id` is nullable) — a `bookId` naming no real book is
    /// therefore an honest "no rows", not a 404.
    #[tokio::test]
    async fn list_vocabulary_words_unknown_book_id_returns_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();

        let Json(words) = list_vocabulary_words(
            State(state),
            Query(VocabularyListQuery {
                book_id: Some("does-not-exist".to_string()),
            }),
        )
        .await
        .unwrap();

        assert!(words.is_empty());
    }

    #[tokio::test]
    async fn list_vocabulary_words_disabled_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        // vocabulary_enabled is left unset (defaults to off).
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = list_vocabulary_words(State(state), Query(VocabularyListQuery { book_id: None }))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_vocabulary_word_route_removes_row() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();
        let id = {
            let conn = state.conn().unwrap();
            db::list_vocabulary(&conn).unwrap()[0].id.clone()
        };

        let status = delete_vocabulary_word_route(State(state.clone()), Path(id))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        assert!(db::list_vocabulary(&conn).unwrap().is_empty());
    }

    /// Idempotent, same as `delete_highlight_route`: deleting an id that was
    /// never there (or already deleted) is still a success.
    #[tokio::test]
    async fn delete_vocabulary_word_route_unknown_id_is_still_no_content() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let status = delete_vocabulary_word_route(State(state), Path("does-not-exist".to_string()))
            .await
            .unwrap();

        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn delete_vocabulary_word_route_disabled_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        // vocabulary_enabled is left unset (defaults to off).
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = delete_vocabulary_word_route(State(state), Path("some-id".to_string()))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    // ── GET /api/vocabulary/due, POST /api/vocabulary/{id}/review (M6) ──────

    /// The discriminating half of "due" — a row already scheduled for the
    /// future must NOT come back, not merely that a due row does. A handler
    /// that ignored `next_due_at` entirely (returned every row) would still
    /// pass a test that only checked inclusion.
    #[tokio::test]
    async fn due_vocabulary_words_excludes_a_row_not_yet_due() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        // Never-reviewed row: due immediately (next_due_at IS NULL).
        create_vocabulary_word(
            State(state.clone()),
            Bytes::from(r#"{"word":"cat","lemma":"cat-due","definition":"feline mammal"}"#),
        )
        .await
        .unwrap();
        // A row already scheduled far in the future must not be due yet.
        create_vocabulary_word(
            State(state.clone()),
            Bytes::from(r#"{"word":"dog","lemma":"dog-not-due","definition":"canine mammal"}"#),
        )
        .await
        .unwrap();
        {
            let conn = state.conn().unwrap();
            conn.execute(
                "UPDATE vocabulary SET next_due_at = ?1 WHERE lemma = 'dog-not-due'",
                rusqlite::params![i64::MAX],
            )
            .unwrap();
        }

        let Json(due) =
            get_due_vocabulary_words(State(state), Query(VocabularyDueQuery { limit: None }))
                .await
                .unwrap();

        assert_eq!(due.len(), 1);
        assert_eq!(due[0].lemma, "cat-due");
    }

    /// A caller-supplied limit above the server cap is silently clamped, not
    /// trusted or rejected — this asserts the clamp actually bites by
    /// counting what comes back, not just that the request succeeds.
    #[tokio::test]
    async fn due_vocabulary_words_limit_clamps_to_server_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        for i in 0..(VOCABULARY_DUE_MAX_LIMIT + 5) {
            create_vocabulary_word(
                State(state.clone()),
                Bytes::from(format!(
                    r#"{{"word":"w{i}","lemma":"clamp-{i}","definition":"def"}}"#
                )),
            )
            .await
            .unwrap();
        }

        let Json(due) = get_due_vocabulary_words(
            State(state),
            Query(VocabularyDueQuery {
                limit: Some(10_000),
            }),
        )
        .await
        .unwrap();

        assert_eq!(due.len(), VOCABULARY_DUE_MAX_LIMIT as usize);
    }

    #[tokio::test]
    async fn due_vocabulary_words_disabled_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        // vocabulary_enabled is left unset (defaults to off).
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = get_due_vocabulary_words(State(state), Query(VocabularyDueQuery { limit: None }))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    fn review_body(correct: bool) -> Bytes {
        Bytes::from(format!(r#"{{"correct":{correct}}}"#))
    }

    /// Persistence, not just the status code: a 204 that left the row
    /// untouched would pass a weaker test. Reads the row back and checks both
    /// the box and the due timestamp actually moved.
    #[tokio::test]
    async fn review_vocabulary_word_route_correct_advances_box_and_sets_next_due() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();
        let id = {
            let conn = state.conn().unwrap();
            db::list_vocabulary(&conn).unwrap()[0].id.clone()
        };

        let status =
            review_vocabulary_word_route(State(state.clone()), Path(id.clone()), review_body(true))
                .await
                .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        let word = db::list_vocabulary(&conn)
            .unwrap()
            .into_iter()
            .find(|w| w.id == id)
            .unwrap();
        assert_eq!(word.box_num, 2, "a correct review should advance the box");
        assert!(word.next_due_at.is_some(), "next_due_at must be set");
        assert!(word.last_reviewed_at.is_some());
    }

    /// A wrong answer resets the box to 1 regardless of where it started —
    /// verified by first advancing the box with a correct review, then
    /// checking a wrong one resets it rather than merely leaving it alone.
    #[tokio::test]
    async fn review_vocabulary_word_route_wrong_resets_box_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();
        let id = {
            let conn = state.conn().unwrap();
            db::list_vocabulary(&conn).unwrap()[0].id.clone()
        };
        review_vocabulary_word_route(State(state.clone()), Path(id.clone()), review_body(true))
            .await
            .unwrap();

        let status = review_vocabulary_word_route(
            State(state.clone()),
            Path(id.clone()),
            review_body(false),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let conn = state.conn().unwrap();
        let word = db::list_vocabulary(&conn)
            .unwrap()
            .into_iter()
            .find(|w| w.id == id)
            .unwrap();
        assert_eq!(word.box_num, 1, "a wrong review must reset the box to 1");
    }

    #[tokio::test]
    async fn review_vocabulary_word_route_unknown_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }

        let err = review_vocabulary_word_route(
            State(state),
            Path("does-not-exist".to_string()),
            review_body(true),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn review_vocabulary_word_route_disabled_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        // vocabulary_enabled is left unset (defaults to off).
        let state = dictionary_test_state(dir.path().to_path_buf());

        let err = review_vocabulary_word_route(
            State(state),
            Path("some-id".to_string()),
            review_body(true),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn review_vocabulary_word_route_malformed_body_is_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = dictionary_test_state(dir.path().to_path_buf());
        {
            let conn = state.conn().unwrap();
            db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        }
        create_vocabulary_word(State(state.clone()), vocabulary_body(""))
            .await
            .unwrap();
        let id = {
            let conn = state.conn().unwrap();
            db::list_vocabulary(&conn).unwrap()[0].id.clone()
        };

        let err = review_vocabulary_word_route(State(state), Path(id), Bytes::from("not json"))
            .await
            .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn width_param_absent_and_invalid_resolve_to_none() {
        assert_eq!(parse_width(None), None);
        assert_eq!(parse_width(Some("")), None);
        assert_eq!(parse_width(Some("width=")), None);
        assert_eq!(parse_width(Some("width=abc")), None);
        assert_eq!(parse_width(Some("width=-5")), None);
        assert_eq!(parse_width(Some("width=0")), None);
        // u32 overflow
        assert_eq!(parse_width(Some("width=99999999999999999999")), None);
        // duplicate width params are ambiguous
        assert_eq!(parse_width(Some("width=800&width=900")), None);
        assert_eq!(parse_width(Some("other=1")), None);
    }

    #[test]
    fn width_param_valid_values_clamp_to_range() {
        assert_eq!(parse_width(Some("width=1080")), Some(1080));
        assert_eq!(parse_width(Some("width=64")), Some(64));
        assert_eq!(parse_width(Some("width=2048")), Some(2048));
        assert_eq!(parse_width(Some("width=1")), Some(64)); // clamp low
        assert_eq!(parse_width(Some("width=8000")), Some(2048)); // clamp high
                                                                 // other params (e.g. the reader's ?__reload= retry nonce) are ignored
        assert_eq!(parse_width(Some("width=1080&__reload=123")), Some(1080));
        assert_eq!(parse_width(Some("__reload=123&width=1080")), Some(1080));
    }

    #[test]
    fn width_param_is_percent_decoded() {
        // RawQuery hands us the undecoded query string — values…
        assert_eq!(parse_width(Some("width=%31%30%38%30")), Some(1080));
        // …and keys decode, so an encoded duplicate is still ambiguous.
        assert_eq!(parse_width(Some("width=800&w%69dth=900")), None);
        // form-encoding: '+' is a space, so "+800" (" 800") is not a number.
        assert_eq!(parse_width(Some("width=+800")), None);
    }

    #[test]
    fn test_rewrite_asset_urls() {
        let html = r#"<img src="asset://localhost/%2Ftmp%2Fimages%2Fbook1%2F0%2Fchapter1.jpg" />"#;
        let result = rewrite_asset_urls_to_http(html, "book1", 0);
        assert!(result.contains("/api/books/book1/images/0/chapter1.jpg"));
        assert!(!result.contains("asset://"));
    }

    #[test]
    fn test_rewrite_asset_urls_no_assets() {
        let html = "<p>Hello world</p>";
        let result = rewrite_asset_urls_to_http(html, "book1", 0);
        assert_eq!(result, html);
    }

    // R2-1: Path traversal prevention
    #[test]
    fn test_validate_image_filename_rejects_traversal() {
        assert!(!is_safe_filename("../../../etc/passwd"));
        assert!(!is_safe_filename("..%2F..%2Fetc/passwd"));
        assert!(!is_safe_filename("foo/../bar"));
        assert!(!is_safe_filename(".."));
        assert!(!is_safe_filename("/absolute/path"));
    }

    #[test]
    fn test_validate_image_filename_accepts_valid() {
        assert!(is_safe_filename("image.jpg"));
        assert!(is_safe_filename("chapter1-cover.png"));
        assert!(is_safe_filename("my image (1).webp"));
    }

    // R3-1: XSS sanitization
    #[test]
    fn test_sanitize_chapter_html_strips_scripts() {
        let html = r#"<p>Hello</p><script>alert('xss')</script><p>World</p>"#;
        let sanitized = sanitize_chapter_html(html);
        assert!(!sanitized.contains("<script>"));
        assert!(!sanitized.contains("alert("));
        assert!(sanitized.contains("Hello"));
        assert!(sanitized.contains("World"));
    }

    #[test]
    fn test_sanitize_chapter_html_strips_event_handlers() {
        let html = r#"<img src="x" onerror="alert('xss')">"#;
        let sanitized = sanitize_chapter_html(html);
        assert!(!sanitized.contains("onerror"));
        assert!(!sanitized.contains("alert"));
    }

    #[test]
    fn test_sanitize_chapter_html_preserves_safe_content() {
        let html = r#"<h1>Title</h1><p>Text with <em>emphasis</em> and <a href="/link">a link</a>.</p><img src="/api/books/1/images/0/fig.jpg">"#;
        let sanitized = sanitize_chapter_html(html);
        assert!(sanitized.contains("<h1>"));
        assert!(sanitized.contains("<em>"));
        assert!(sanitized.contains("<img"));
    }

    // R2-4: URL rewriting with regex handles multiple URLs
    #[test]
    fn test_rewrite_asset_urls_multiple_images() {
        let html = r#"<img src="asset://localhost/a/b/c/img1.jpg"><img src="asset://localhost/x/y/z/img2.png">"#;
        let result = rewrite_asset_urls_to_http(html, "book1", 3);
        assert!(result.contains("/api/books/book1/images/3/img1.jpg"));
        assert!(result.contains("/api/books/book1/images/3/img2.png"));
        assert!(!result.contains("asset://"));
    }

    // R2-4: URL rewriting handles UTF-8 filenames
    #[test]
    fn test_rewrite_asset_urls_utf8_filename() {
        let html = r#"<img src="asset://localhost/path/%E5%9B%BE%E7%89%87.jpg">"#;
        let result = rewrite_asset_urls_to_http(html, "book1", 0);
        assert!(!result.contains("asset://"));
    }

    #[test]
    fn test_book_query_accepts_series_param() {
        let query: BookQuery =
            serde_json::from_str(r#"{"series":"My Series"}"#).expect("should parse series param");
        assert_eq!(query.series, Some("My Series".to_string()));
        assert_eq!(query.q, None);
    }

    #[test]
    fn test_book_query_accepts_both_params() {
        let query: BookQuery =
            serde_json::from_str(r#"{"q":"test","series":"Sci-Fi"}"#).expect("should parse both");
        assert_eq!(query.q, Some("test".to_string()));
        assert_eq!(query.series, Some("Sci-Fi".to_string()));
    }

    #[test]
    fn test_book_query_empty() {
        let query: BookQuery = serde_json::from_str("{}").expect("should parse empty");
        assert_eq!(query.q, None);
        assert_eq!(query.series, None);
    }

    #[test]
    fn test_collection_with_count_serializes() {
        let coll = CollectionWithCount {
            id: "c1".into(),
            name: "Test".into(),
            r#type: crate::models::CollectionType::Manual,
            icon: Some("\u{1F4DA}".into()),
            color: None,
            created_at: 0,
            updated_at: 0,
            rules: vec![],
            book_count: 5,
        };
        let json = serde_json::to_value(&coll).unwrap();
        assert_eq!(json["bookCount"], 5);
        assert_eq!(json["name"], "Test");
        assert_eq!(json["icon"], "\u{1F4DA}");
    }

    #[test]
    fn gdpr_export_redacts_denylisted_settings() {
        // `run_schema` is private to carrel-core; build a schema-migrated
        // in-memory connection through the pool helper (same as `test_state`).
        let pool = crate::db::create_pool(&std::path::PathBuf::from(":memory:")).unwrap();
        let conn = pool.get().unwrap();
        db::set_setting(&conn, "backup_config", "{\"secret\":\"x\"}").unwrap();
        db::set_setting(
            &conn,
            "enrichment_providers",
            "{\"google\":{\"enabled\":true,\"apiKey\":\"SECRET\"}}",
        )
        .unwrap();
        db::set_setting(
            &conn,
            "opds_auth",
            "[{\"catalogUrl\":\"http://localhost/opds\",\"origin\":\"http://localhost\",\"kind\":\"basic\",\"username\":\"mike@buzzwoo.de\"}]",
        )
        .unwrap();
        db::set_setting(&conn, "import_mode", "copy").unwrap();

        let value = build_gdpr_export(&conn).expect("build_gdpr_export");
        let settings = value["settings"].as_object().expect("settings object");
        assert!(
            !settings.contains_key("backup_config"),
            "backup_config must be redacted"
        );
        assert!(
            !settings.contains_key("enrichment_providers"),
            "enrichment_providers (carries API keys) must be redacted"
        );
        assert!(
            !settings.contains_key("opds_auth"),
            "opds_auth (carries catalog account usernames) must be redacted"
        );
        assert_eq!(settings["import_mode"], "copy");
        assert!(value["activity_log"].is_array());

        let serialized = serde_json::to_string(&value).unwrap();
        assert!(!serialized.contains("SECRET"), "API key leaked into export");
        assert!(
            !serialized.contains("mike@buzzwoo.de"),
            "OPDS catalog username leaked into export"
        );
    }

    #[test]
    fn test_stats_endpoint_exists() {
        let stats = db::ReadingStats {
            total_reading_time_secs: 3600,
            total_sessions: 10,
            total_pages_read: 200,
            total_books: 5,
            books_finished: 2,
            books_finished_this_year: 1,
            current_streak_days: 3,
            longest_streak_days: 7,
            daily_reading: vec![("2026-05-01".to_string(), 1800)],
            daily_reading_year: vec![("2026-05-01".to_string(), 1800)],
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["totalReadingTimeSecs"], 3600);
        assert_eq!(json["totalSessions"], 10);
        assert_eq!(json["totalPagesRead"], 200);
        assert_eq!(json["booksFinished"], 2);
        assert_eq!(json["booksFinishedThisYear"], 1);
        assert_eq!(json["currentStreakDays"], 3);
        assert_eq!(json["longestStreakDays"], 7);
        assert!(json["dailyReading"].is_array());
    }
}

//! Kids mode on the web: which sessions are in Kids mode, and the
//! default-deny gate that confines them.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{MatchedPath, Path, Query, RawPathParams, State},
    handler::Handler,
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json, RequestExt,
};
use carrel_core::kids::KidsCodeStore;

use super::auth::{RateLimiter, SessionToken};
use super::{api, carrel_status, web_ui, WebState};
use crate::db;

/// Kids-mode state for one running web server.
pub struct KidsWebState {
    /// Session tokens currently in Kids mode. A token that's in here and in
    /// `WebState::sessions` is a Kids session.
    pub sessions: Mutex<HashSet<String>>,
    /// Parents'-code attempts to leave Kids mode, keyed by session token.
    /// Its own instance, never `login_limiter`: behind `tailscale serve`
    /// every device is 127.0.0.1, and a successful login clears the login
    /// limiter for that IP.
    pub exit_limiter: RateLimiter,
    /// Where the parents' code hash lives (the OS keychain in production).
    pub code_store: Arc<dyn KidsCodeStore>,
}

impl KidsWebState {
    pub fn new(code_store: Arc<dyn KidsCodeStore>) -> Self {
        Self {
            sessions: Mutex::new(HashSet::new()),
            exit_limiter: RateLimiter::new(5, 300),
            code_store,
        }
    }

    /// Whether `token` is a Kids session. A poisoned lock counts as yes:
    /// failing closed costs an adult some access, failing open shows a child
    /// the whole library.
    pub fn is_kids(&self, token: &str) -> bool {
        self.sessions
            .lock()
            .map(|sessions| sessions.contains(token))
            .unwrap_or(true)
    }
}

/// Routes about one book, gated on that book being visible. Every method on
/// these templates passes for a visible book, so a method added to one of
/// them later is reachable from a Kids session too.
const BOOK_ROUTES: &[&str] = &[
    "/api/books/{id}",
    "/api/books/{id}/cover",
    "/api/books/{id}/chapters",
    "/api/books/{id}/chapters/{index}",
    "/api/books/{id}/images/{chapter}/{filename}",
    "/api/books/{id}/pages/{index}",
    "/api/books/{id}/page-count",
    "/api/books/{id}/progress",
    "/api/books/{id}/bookmarks",
    "/api/books/{id}/bookmarks/{bookmark_id}",
    "/api/books/{id}/highlights",
    "/api/books/{id}/highlights/{highlight_id}",
    "/api/books/{id}/want-to-read",
    "/api/books/{id}/download",
    "/api/books/{id}/download/{filename}",
];

const REFUSED: &str = "Not available in Kids mode";

/// Default-deny gate for Kids sessions. Layered inside `auth_middleware`, so
/// it runs after authentication and after routing.
///
/// A request without a Kids `SessionToken` passes untouched. A Kids request
/// is classified by its `MatchedPath`, the route template the router picked,
/// rather than by re-parsing the raw path, so the gate and the router can't
/// disagree about which handler a URL reaches. The policy:
/// - **Pass:** health, login, the public shell assets, `GET` dictionary
///   routes, and `GET /api/kids/status`, `POST /api/kids/exit`,
///   `GET /api/kids/visible-ids`.
/// - **Pass if the book is visible:** [`BOOK_ROUTES`]. A hidden or missing
///   book is `404 "Book not found"`, so its existence isn't revealed.
/// - **Served here, filtered:** the six book lists, by the same code as the
///   full-library handlers over the `*_kids` queries.
/// - **Everything else** (the fallback included): `403`, never 401, which
///   the web UI would read as logged out.
pub async fn kids_gate(
    State(state): State<WebState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let is_kids = req
        .extensions()
        .get::<SessionToken>()
        .is_some_and(|token| state.kids.is_kids(&token.0));
    if !is_kids {
        return next.run(req).await;
    }

    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let method = req.method().clone();

    if passes(&method, &route) {
        return next.run(req).await;
    }
    if BOOK_ROUTES.contains(&route.as_str()) {
        let id = req
            .extract_parts::<RawPathParams>()
            .await
            .ok()
            .and_then(|params| {
                params
                    .iter()
                    .find(|(key, _)| *key == "id")
                    .map(|(_, id)| id.to_owned())
            });
        return match id.map(|id| book_visible(&state, &id)) {
            Some(Ok(true)) => next.run(req).await,
            Some(Err(e)) => e.into_response(),
            Some(Ok(false)) | None => (StatusCode::NOT_FOUND, "Book not found").into_response(),
        };
    }
    if method == Method::GET {
        match route.as_str() {
            "/api/books" => return books.call(req, state).await,
            "/api/books/continue-reading" => return continue_reading.call(req, state).await,
            "/api/series" => return series.call(req, state).await,
            "/api/collections" => return collections.call(req, state).await,
            "/api/collections/{id}/books" => return collection_books.call(req, state).await,
            "/api/reading-progress" => return reading_progress.call(req, state).await,
            _ => {}
        }
    }
    (StatusCode::FORBIDDEN, REFUSED).into_response()
}

/// The routes a Kids session reaches unchanged.
fn passes(method: &Method, route: &str) -> bool {
    route == "/api/health"
        || route == "/api/auth"
        || web_ui::PUBLIC_SHELL_ASSETS.contains(&route)
        || (method == Method::GET
            && (route.starts_with("/api/dictionary/")
                || route == "/api/kids/status"
                || route == "/api/kids/visible-ids"))
        || (method == Method::POST && route == "/api/kids/exit")
}

/// The connection is dropped before the caller runs the handler, which takes
/// its own.
fn book_visible(state: &WebState, id: &str) -> Result<bool, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    carrel_core::kids::is_book_visible(&conn, id).map_err(carrel_status)
}

// ── Kids-filtered lists ──────────────────────────────────────────────────────
//
// Same extractors and return types as their counterparts in `api.rs`, so
// query strings parse and bodies serialize identically.

async fn books(
    State(state): State<WebState>,
    Query(params): Query<api::BookQuery>,
) -> Result<Response, (StatusCode, String)> {
    api::list_books_with(&state, params, db::query_books_grid_kids)
}

async fn continue_reading(
    State(state): State<WebState>,
    Query(params): Query<api::ContinueReadingQuery>,
) -> Result<Json<Vec<api::PublicContinueReadingItem>>, (StatusCode, String)> {
    api::continue_reading_with(&state, params, db::get_continue_reading_books_kids)
}

async fn series(
    State(state): State<WebState>,
) -> Result<Json<Vec<crate::models::SeriesInfo>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    Ok(Json(db::list_series_kids(&conn).map_err(carrel_status)?))
}

async fn collections(
    State(state): State<WebState>,
) -> Result<Json<Vec<api::CollectionWithCount>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let collections = db::list_collections_kids(&conn).map_err(carrel_status)?;
    Ok(Json(collections.into_iter().map(Into::into).collect()))
}

async fn collection_books(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Query(params): Query<api::CollectionBooksQuery>,
) -> Result<Json<Vec<api::PublicBookGridItem>>, (StatusCode, String)> {
    api::collection_books_with(&state, &id, params, db::query_books_in_collection_grid_kids)
}

async fn reading_progress(
    State(state): State<WebState>,
) -> Result<Json<Vec<crate::models::ReadingProgress>>, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    Ok(Json(
        db::get_all_reading_progress_kids(&conn).map_err(carrel_status)?,
    ))
}

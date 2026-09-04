use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use super::{carrel_status, WebState};
use crate::db;
use crate::models::Book;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const ATOM_CONTENT_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";
const ATOM_ACQ_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";
const OPENSEARCH_DESC_TYPE: &str = "application/opensearchdescription+xml";

/// Build all `/opds/` routes.
pub fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/", get(root_catalog))
        .route("/all", get(all_books))
        .route("/new", get(new_books))
        .route("/collections/{id}", get(collection_feed))
        .route("/search", get(search_books))
        .route("/opensearch.xml", get(opensearch_descriptor))
        .with_state(state)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Weak ETag for one feed response: SHA-256 of the feed id, this request's
/// own feed URL (`self_href`), and the sorted `(id, updated_at)` pairs of
/// every book the feed *covers*. Weak (`W/"..."`) because equal-state bodies
/// are not byte-identical. Hashing pairs rather than putting raw timestamps in
/// the tag avoids leaking library activity times to clients.
///
/// "Covers", not "renders": for the two paginated feeds `rendered_ids` is the
/// whole matching set, not the slice this page shows. That is deliberate — it
/// is what makes one change to any covered book invalidate every page of that
/// feed at once, so a deletion on a later page cannot leave an earlier page
/// pointing at a stale `next`.
///
/// `self_href` is folded in so two page URLs of the same feed can never
/// produce the same tag despite hashing that identical whole-set input.
/// Ambiguity between the three fields is prevented by a `\0` separator, which
/// is sound only because none of them can contain a NUL byte: feed ids are
/// literals plus a DB-generated id, and `self_href` is percent-encoded.
fn feed_etag(
    feed_id: &str,
    self_href: &str,
    rendered_ids: &[&str],
    pairs: &HashMap<String, i64>,
) -> String {
    let mut ids: Vec<&str> = rendered_ids.to_vec();
    ids.sort_unstable();
    let mut h = Sha256::new();
    h.update(feed_id.as_bytes());
    h.update([0u8]); // separator, as below — see the note on NUL in the doc comment
    h.update(self_href.as_bytes());
    for id in ids {
        h.update([0u8]); // separator so ("ab","c") != ("a","bc")
        h.update(id.as_bytes());
        h.update(pairs.get(id).copied().unwrap_or(0).to_le_bytes());
    }
    let hex = format!("{:x}", h.finalize());
    format!("W/\"{}\"", &hex[..16])
}

/// RFC 9110 §13.1.2 If-None-Match: comma-separated entity tags or `*`,
/// compared weakly (the `W/` prefix is ignored on both sides).
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    fn opaque(tag: &str) -> &str {
        tag.trim().trim_start_matches("W/").trim_matches('"')
    }
    let ours = opaque(etag);
    value
        .split(',')
        .any(|candidate| candidate.trim() == "*" || opaque(candidate) == ours)
}

/// Max `updated_at` among the rendered books — the feed-level `<updated>`
/// value. `None` for an empty feed (caller falls back to now).
fn max_updated(rendered_ids: &[&str], pairs: &HashMap<String, i64>) -> Option<i64> {
    rendered_ids
        .iter()
        .filter_map(|id| pairs.get(*id).copied())
        .max()
}

/// Derive an OPDS acquisition extension + MIME from a MOBI-family book's
/// stored file path. Import preserves the original extension when copying
/// into the library, so the filename is authoritative. Falls back to plain
/// `.mobi` when the extension is missing or unrecognized.
fn mobi_ext_and_mime(file_path: &str) -> (&'static str, &'static str) {
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("azw3") => ("azw3", "application/vnd.amazon.ebook"),
        Some("azw") => ("azw", "application/vnd.amazon.ebook"),
        _ => ("mobi", "application/x-mobipocket-ebook"),
    }
}

fn cover_mime(cover_path: Option<&str>) -> &'static str {
    // Stays in lockstep with the actual cover endpoint at
    // `web_server/api.rs::get_cover`, which derives the response
    // `Content-Type` from the path extension via `mime_guess`. If the
    // feed advertised a different MIME than the endpoint serves, strict
    // OPDS clients can mis-cache or reject the response — that is the
    // exact bug this function exists to prevent, so the explicit
    // `webp` arm is required (mime_guess returns `image/webp` for it).
    match cover_path
        .and_then(|path| std::path::Path::new(path).extension())
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

fn book_to_entry(book: &Book) -> String {
    let title = xml_escape(&book.title);
    let author = xml_escape(&book.author);
    let id = &book.id;
    let updated = chrono::DateTime::from_timestamp(book.added_at, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "2024-01-01T00:00:00Z".to_string());

    let description = book
        .description
        .as_ref()
        .map(|d| format!("<summary>{}</summary>", xml_escape(d)))
        .unwrap_or_default();

    let cover_link = format!(
        r#"<link rel="http://opds-spec.org/image" href="/api/books/{id}/cover" type="{}"/>"#,
        cover_mime(book.cover_path.as_deref())
    );

    // `BookFormat::Mobi` is a single enum variant covering `.mobi`, `.azw`, and
    // `.azw3` — we collapsed them on import. For OPDS we need the actual
    // container type so clients pick the right parser/MIME; derive it from the
    // stored file path (import preserves the original extension).
    let (ext, mime) = match book.format {
        crate::models::BookFormat::Epub => ("epub", "application/epub+zip"),
        crate::models::BookFormat::Pdf => ("pdf", "application/pdf"),
        crate::models::BookFormat::Cbz => ("cbz", "application/x-cbz"),
        crate::models::BookFormat::Cbr => ("cbr", "application/x-cbr"),
        crate::models::BookFormat::Mobi => mobi_ext_and_mime(&book.file_path),
    };
    // The extension is included in the URL path so `opds_extension_from_url`
    // can disambiguate on import — this matters for the MOBI family, where
    // `application/vnd.amazon.ebook` covers both `.azw` and `.azw3` and the
    // MIME alone can't tell them apart. The filename is derived from the
    // book id (stable, no escaping hazard) rather than the title.
    let download_link = format!(
        r#"<link rel="http://opds-spec.org/acquisition" href="/api/books/{id}/download/{id}.{ext}" type="{mime}" title="{title}.{ext}"/>"#
    );

    // OPDS clients cache and dedupe on feed and entry ids, so changing the
    // `urn:carrel:*` scheme used throughout this module makes every entry look
    // new to an already-subscribed client.
    format!(
        r#"<entry>
  <title>{title}</title>
  <id>urn:carrel:{id}</id>
  <updated>{updated}</updated>
  <author><name>{author}</name></author>
  {description}
  {cover_link}
  {download_link}
</entry>"#
    )
}

const OPDS_PAGE_SIZE: usize = 50;

fn wrap_feed(
    title: &str,
    feed_id: &str,
    entries: &str,
    self_href: &str,
    kind: &str,
    next_href: Option<&str>,
    updated_ts: Option<i64>,
) -> String {
    // Feed-level <updated>: the library-state change time for ETag-scoped
    // feeds (max updated_at of rendered books), request time otherwise.
    let updated = updated_ts
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    let next_link = next_href
        .map(|h| format!(r#"  <link rel="next" href="{h}" type="{kind}"/>"#))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:opds="http://opds-spec.org/2010/catalog">
  <id>{feed_id}</id>
  <title>{title}</title>
  <updated>{updated}</updated>
  <link rel="self" href="{self_href}" type="{kind}"/>
  <link rel="start" href="/opds" type="{ATOM_CONTENT_TYPE}"/>
  <link rel="search" href="/opds/opensearch.xml" type="{OPENSEARCH_DESC_TYPE}"/>
  <link rel="search" href="/opds/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>
{next_link}
{entries}
</feed>"#
    )
}

/// The authority (`host[:port]`) this request was addressed to, if it is safe
/// to interpolate into a URL inside an XML attribute.
///
/// Only characters that can legally appear in an authority are accepted —
/// letters, digits, `.`, `-`, `_`, `:`, and `[`/`]` for IPv6 literals. That
/// rules out quotes, `<`, `>`, whitespace, and `/`, so the value cannot break
/// out of the attribute or graft a different path onto the template.
///
/// HTTP/1.1 always carries `Host`; the `:authority` fallback is there for
/// completeness rather than because this server negotiates HTTP/2.
fn request_authority(headers: &HeaderMap, uri: &axum::http::Uri) -> Option<String> {
    let raw = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| uri.authority().map(|a| a.to_string()))?;
    let safe = !raw.is_empty()
        && raw.len() <= 255
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
    safe.then_some(raw)
}

/// OpenSearch Description Document for the catalog's search facility.
///
/// OPDS 1.2 advertises search as a `rel="search"` link of type
/// `application/opensearchdescription+xml` pointing at a document like this
/// one. Carrel's feeds also carry the equivalent inline `{searchTerms}`
/// template, which Carrel's own client uses directly (one fewer round trip);
/// this document exists for third-party readers that only recognise the
/// spec's form and would otherwise show no search at all.
///
/// The `template` is ABSOLUTE, built from the authority the client dialed.
/// It has to be: `carrel_core::opds::resolve_search_url_with_context` returns
/// the template verbatim without resolving it against the descriptor's URL,
/// and — for a catalog with stored credentials — requires it to be on the
/// descriptor's own origin. A relative template would therefore be discarded
/// and search would silently disappear for authenticated catalogs.
async fn opensearch_descriptor(headers: HeaderMap, uri: axum::http::Uri) -> Response {
    let Some(authority) = request_authority(&headers, &uri) else {
        return (StatusCode::BAD_REQUEST, "missing or invalid Host").into_response();
    };
    // Plain HTTP unless something in front terminated TLS and said so.
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| *s == "https")
        .unwrap_or("http");
    let template = xml_escape(&format!(
        "{scheme}://{authority}/opds/search?q={{searchTerms}}"
    ));

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>Carrel</ShortName>
  <Description>Search the Carrel library</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <Url type="{ATOM_ACQ_TYPE}" template="{template}"/>
</OpenSearchDescription>"#
    );

    ([(header::CONTENT_TYPE, OPENSEARCH_DESC_TYPE)], xml).into_response()
}

async fn root_catalog() -> Response {
    let entries = format!(
        r#"<entry>
  <title>All Books</title>
  <id>urn:carrel:all</id>
  <updated>{now}</updated>
  <content type="text">Browse the entire library</content>
  <link rel="subsection" href="/opds/all" type="{ATOM_ACQ_TYPE}"/>
</entry>
<entry>
  <title>Recently Added</title>
  <id>urn:carrel:new</id>
  <updated>{now}</updated>
  <content type="text">Books added recently</content>
  <link rel="subsection" href="/opds/new" type="{ATOM_ACQ_TYPE}"/>
</entry>"#,
        now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
    );

    let xml = wrap_feed(
        "Carrel Library",
        "urn:carrel:root",
        &entries,
        "/opds",
        ATOM_CONTENT_TYPE,
        None,
        None,
    );

    ([(header::CONTENT_TYPE, ATOM_CONTENT_TYPE)], xml).into_response()
}

#[derive(serde::Deserialize)]
struct PaginationQuery {
    /// Note the divergence from `SearchQuery::page`, which is a lenient
    /// `String`: a malformed `?page=abc` still rejects this feed outright.
    /// Left as it is deliberately — changing it is a behaviour change to a
    /// handler the milestone that made search lenient did not touch — but it
    /// is the weaker of the two conventions, and the note in `docs/backlog/`
    /// on these handlers records it alongside the other reasons to revisit
    /// them together.
    page: Option<usize>,
}

async fn all_books(
    State(state): State<WebState>,
    Query(params): Query<PaginationQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let books = db::list_books(&conn).map_err(carrel_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(carrel_status)?;

    // The digest covers the whole matching set *and* this request's own URL
    // (self_href): a library change still invalidates every page, but two
    // page URLs never share a validator.
    let page = params.page.unwrap_or(0);
    let self_href = if page > 0 {
        format!("/opds/all?page={page}")
    } else {
        "/opds/all".to_string()
    };
    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    let etag = feed_etag("urn:carrel:all", &self_href, &rendered_ids, &pairs);
    if if_none_match_matches(&headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let start = page * OPDS_PAGE_SIZE;
    let page_books: Vec<&Book> = books.iter().skip(start).take(OPDS_PAGE_SIZE).collect();

    let entries: String = page_books
        .iter()
        .map(|b| book_to_entry(b))
        .collect::<Vec<_>>()
        .join("\n");

    let has_next = start + OPDS_PAGE_SIZE < books.len();
    let next_href = if has_next {
        Some(format!("/opds/all?page={}", page + 1))
    } else {
        None
    };

    let xml = wrap_feed(
        "All Books",
        "urn:carrel:all",
        &entries,
        &self_href,
        ATOM_ACQ_TYPE,
        next_href.as_deref(),
        max_updated(&rendered_ids, &pairs),
    );

    Ok((
        [
            (header::CONTENT_TYPE, ATOM_ACQ_TYPE.to_string()),
            (header::ETAG, etag),
        ],
        xml,
    )
        .into_response())
}

async fn new_books(
    State(state): State<WebState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let mut books = db::list_books(&conn).map_err(carrel_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(carrel_status)?;

    // Sort by added_at descending, take 25 most recent
    books.sort_by_key(|b| std::cmp::Reverse(b.added_at));
    books.truncate(25);

    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    let etag = feed_etag("urn:carrel:new", "/opds/new", &rendered_ids, &pairs);
    if if_none_match_matches(&headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let entries: String = books
        .iter()
        .map(book_to_entry)
        .collect::<Vec<_>>()
        .join("\n");

    let xml = wrap_feed(
        "Recently Added",
        "urn:carrel:new",
        &entries,
        "/opds/new",
        ATOM_ACQ_TYPE,
        None,
        max_updated(&rendered_ids, &pairs),
    );

    Ok((
        [
            (header::CONTENT_TYPE, ATOM_ACQ_TYPE.to_string()),
            (header::ETAG, etag),
        ],
        xml,
    )
        .into_response())
}

async fn collection_feed(
    State(state): State<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let books = db::get_books_in_collection(&conn, &id).map_err(carrel_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(carrel_status)?;

    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    // Hash the RESOLVED membership — works for manual and rule-based collections alike.
    let feed_id = format!("urn:carrel:collection:{id}");
    let self_href = format!("/opds/collections/{id}");
    let etag = feed_etag(&feed_id, &self_href, &rendered_ids, &pairs);
    if if_none_match_matches(&headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let entries: String = books
        .iter()
        .map(book_to_entry)
        .collect::<Vec<_>>()
        .join("\n");

    let xml = wrap_feed(
        &format!("Collection {id}"),
        &feed_id,
        &entries,
        &self_href,
        ATOM_ACQ_TYPE,
        None,
        max_updated(&rendered_ids, &pairs),
    );

    Ok((
        [
            (header::CONTENT_TYPE, ATOM_ACQ_TYPE.to_string()),
            (header::ETAG, etag),
        ],
        xml,
    )
        .into_response())
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
    /// Lenient by design, and typed `String` rather than `usize` for the
    /// reason `api.rs`'s `BookQuery` documents on its own presence-only
    /// field: axum's `Query` extraction is all-or-nothing, so a `usize` here
    /// would reject the entire request — 400, no feed — on a malformed or
    /// empty value (`page=`, `page=abc`) that a proxy or an OPDS client might
    /// append. Before this parameter existed such a value was simply ignored
    /// and the feed served, and it still is. Read it through
    /// [`SearchQuery::page`], never directly.
    page: Option<String>,
}

impl SearchQuery {
    /// The requested page, or 0 for anything that is not a page number.
    fn page(&self) -> usize {
        self.page
            .as_deref()
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0)
    }
}

async fn search_books(
    State(state): State<WebState>,
    Query(params): Query<SearchQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(carrel_status)?;
    let page = params.page();
    let search_term = params.q.unwrap_or_default();
    let encoded_term = urlencoding::encode(&search_term);
    let self_href = if page > 0 {
        format!("/opds/search?q={}&amp;page={page}", encoded_term)
    } else {
        format!("/opds/search?q={}", encoded_term)
    };

    // Filter and sort in SQL; page in Rust. `limit`/`offset` deliberately go
    // unused here, because the ETag below covers the whole *filtered* set and
    // therefore needs the complete matching list before it can be sliced.
    //
    // The digest also folds in this request's own URL (self_href), so two
    // different pages of one search never share a validator even though both
    // hash the same whole-set `pairs` input. See the note in `docs/backlog/`
    // on these handlers for the SQL-paging alternative.
    let query = db::BookQuery {
        q: Some(search_term.clone()),
        series: None,
        want_to_read: false,
        sort: db::BookSort::default(),
        limit: None,
        offset: 0,
    };
    let books = db::query_books(&conn, &query).map_err(carrel_status)?.items;
    let pairs = db::book_etag_pairs(&conn).map_err(carrel_status)?;

    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    let etag = feed_etag("urn:carrel:search", &self_href, &rendered_ids, &pairs);
    if if_none_match_matches(&headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    // Saturating, not `*`: `page` comes off the wire, so a large-but-parseable
    // value reaches here. `page * OPDS_PAGE_SIZE` panics in debug and wraps in
    // release, and a wrapped `start + OPDS_PAGE_SIZE` would then read as
    // "there is a next page" and emit a `next` link back to page 0.
    let start = page.saturating_mul(OPDS_PAGE_SIZE);
    let page_books: Vec<&Book> = books.iter().skip(start).take(OPDS_PAGE_SIZE).collect();

    let entries: String = page_books
        .iter()
        .map(|b| book_to_entry(b))
        .collect::<Vec<_>>()
        .join("\n");

    let has_next = start.saturating_add(OPDS_PAGE_SIZE) < books.len();
    // `&amp;` (not a raw `&`): these hrefs sit inside a double-quoted XML
    // attribute, and an unescaped `&` starts an entity reference — the same
    // reason `opensearch_descriptor`'s template runs through `xml_escape`.
    let next_href = if has_next {
        Some(format!(
            "/opds/search?q={}&amp;page={}",
            encoded_term,
            page + 1
        ))
    } else {
        None
    };

    let xml = wrap_feed(
        &format!("Search: {}", xml_escape(&search_term)),
        "urn:carrel:search",
        &entries,
        &self_href,
        ATOM_ACQ_TYPE,
        next_href.as_deref(),
        max_updated(&rendered_ids, &pairs),
    );

    Ok((
        [
            (header::CONTENT_TYPE, ATOM_ACQ_TYPE.to_string()),
            (header::ETAG, etag),
        ],
        xml,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_escape() {
        assert_eq!(xml_escape("foo & bar"), "foo &amp; bar");
        assert_eq!(xml_escape("<script>"), "&lt;script&gt;");
        assert_eq!(xml_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn test_book_to_entry_contains_required_elements() {
        let book = Book {
            id: "test-1".to_string(),
            title: "Test & Book".to_string(),
            author: "Author <Name>".to_string(),
            file_path: "/tmp/test.epub".to_string(),
            cover_path: None,
            total_chapters: 5,
            added_at: 1700000000,
            format: crate::models::BookFormat::Epub,
            file_hash: None,
            description: Some("A <great> book".to_string()),
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
            is_imported: true,
            want_to_read: false,
        };

        let entry = book_to_entry(&book);
        assert!(entry.contains("<title>Test &amp; Book</title>"));
        assert!(entry.contains("Author &lt;Name&gt;"));
        assert!(entry.contains("urn:carrel:test-1"));
        assert!(entry.contains("application/epub+zip"));
        assert!(entry.contains("/api/books/test-1/download"));
        assert!(entry.contains("A &lt;great&gt; book"));
    }

    #[test]
    fn mobi_ext_and_mime_preserves_original_extension() {
        assert_eq!(
            mobi_ext_and_mime("/lib/book.mobi"),
            ("mobi", "application/x-mobipocket-ebook")
        );
        assert_eq!(
            mobi_ext_and_mime("/lib/book.azw"),
            ("azw", "application/vnd.amazon.ebook")
        );
        assert_eq!(
            mobi_ext_and_mime("/lib/book.azw3"),
            ("azw3", "application/vnd.amazon.ebook")
        );
        // Case-insensitive.
        assert_eq!(
            mobi_ext_and_mime("/lib/BOOK.AZW3"),
            ("azw3", "application/vnd.amazon.ebook")
        );
    }

    #[test]
    fn mobi_ext_and_mime_falls_back_to_mobi() {
        // Missing / unknown extension falls back to plain mobi.
        assert_eq!(
            mobi_ext_and_mime("/lib/book"),
            ("mobi", "application/x-mobipocket-ebook")
        );
        assert_eq!(
            mobi_ext_and_mime("/lib/book.xyz"),
            ("mobi", "application/x-mobipocket-ebook")
        );
    }

    #[test]
    fn cover_mime_matches_cover_extension() {
        assert_eq!(cover_mime(Some("/tmp/cover.jpg")), "image/jpeg");
        assert_eq!(cover_mime(Some("/tmp/cover.png")), "image/png");
        assert_eq!(cover_mime(Some("/tmp/cover.gif")), "image/gif");
        assert_eq!(cover_mime(Some("/tmp/cover.bmp")), "image/bmp");
        assert_eq!(cover_mime(Some("/tmp/cover.webp")), "image/webp");
        assert_eq!(cover_mime(Some("/tmp/cover.jpeg")), "image/jpeg");
        // Unknown / missing extensions default to JPEG so the link tag
        // still validates; the cover endpoint's mime_guess fallback is
        // also octet-stream → image/jpeg here is the safer OPDS-side
        // default since clients will at least try to render it.
        assert_eq!(cover_mime(Some("/tmp/cover.xyz")), "image/jpeg");
        assert_eq!(cover_mime(None), "image/jpeg");
    }

    fn make_book(file_path: &str, format: crate::models::BookFormat) -> Book {
        Book {
            id: "book-1".to_string(),
            title: "Title".to_string(),
            author: "A".to_string(),
            file_path: file_path.to_string(),
            cover_path: None,
            total_chapters: 1,
            added_at: 1700000000,
            format,
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
            is_imported: true,
            want_to_read: false,
        }
    }

    #[test]
    fn download_url_carries_extension_for_azw3() {
        // Round-tripping AZW3 through OPDS must preserve the extension in
        // the acquisition URL so opds_extension_from_url disambiguates the
        // ambiguous `application/vnd.amazon.ebook` MIME.
        let book = make_book("/lib/story.azw3", crate::models::BookFormat::Mobi);
        let entry = book_to_entry(&book);
        assert!(
            entry.contains("/api/books/book-1/download/book-1.azw3"),
            "acquisition href missing .azw3 suffix: {entry}"
        );
        assert!(entry.contains("application/vnd.amazon.ebook"));
    }

    #[test]
    fn download_url_carries_extension_for_azw() {
        let book = make_book("/lib/story.azw", crate::models::BookFormat::Mobi);
        let entry = book_to_entry(&book);
        assert!(
            entry.contains("/api/books/book-1/download/book-1.azw"),
            "acquisition href missing .azw suffix: {entry}"
        );
        // Plain .azw and .azw3 share a MIME but the URL extension now
        // disambiguates.
        assert!(!entry.contains("/api/books/book-1/download/book-1.azw3"));
    }

    #[test]
    fn download_url_carries_extension_for_core_formats() {
        for (path, fmt, ext) in [
            ("/lib/a.epub", crate::models::BookFormat::Epub, "epub"),
            ("/lib/a.pdf", crate::models::BookFormat::Pdf, "pdf"),
            ("/lib/a.cbz", crate::models::BookFormat::Cbz, "cbz"),
            ("/lib/a.cbr", crate::models::BookFormat::Cbr, "cbr"),
            ("/lib/a.mobi", crate::models::BookFormat::Mobi, "mobi"),
        ] {
            let book = make_book(path, fmt);
            let entry = book_to_entry(&book);
            let expected = format!("/api/books/book-1/download/book-1.{ext}");
            assert!(
                entry.contains(&expected),
                "{ext} entry missing {expected}:\n{entry}"
            );
        }
    }

    #[test]
    fn opds_cover_link_uses_real_cover_mime() {
        let mut book = make_book("/lib/story.mobi", crate::models::BookFormat::Mobi);
        book.cover_path = Some("/tmp/covers/book-1/cover.png".to_string());

        let entry = book_to_entry(&book);

        assert!(
            entry.contains(r#"href="/api/books/book-1/cover" type="image/png""#),
            "cover link should advertise png mime:\n{entry}"
        );
    }

    use axum::http::HeaderMap;
    use std::collections::HashMap;

    fn pairs(entries: &[(&str, i64)]) -> HashMap<String, i64> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn feed_etag_is_order_independent_and_weak() {
        let p = pairs(&[("a", 1), ("b", 2)]);
        let t1 = feed_etag("urn:carrel:all", "/opds/all", &["a", "b"], &p);
        let t2 = feed_etag("urn:carrel:all", "/opds/all", &["b", "a"], &p);
        assert_eq!(t1, t2);
        assert!(t1.starts_with("W/\""), "weak ETag required, got {t1}");
        assert!(t1.ends_with('"'));
    }

    #[test]
    fn feed_etag_changes_on_updated_at_bump_and_set_change() {
        let p1 = pairs(&[("a", 1), ("b", 2)]);
        let base = feed_etag("urn:carrel:all", "/opds/all", &["a", "b"], &p1);

        // updated_at bump
        let p2 = pairs(&[("a", 1), ("b", 3)]);
        assert_ne!(
            base,
            feed_etag("urn:carrel:all", "/opds/all", &["a", "b"], &p2)
        );

        // id removed from rendered set
        assert_ne!(base, feed_etag("urn:carrel:all", "/opds/all", &["a"], &p1));

        // id added to rendered set
        let p3 = pairs(&[("a", 1), ("b", 2), ("c", 9)]);
        assert_ne!(
            base,
            feed_etag("urn:carrel:all", "/opds/all", &["a", "b", "c"], &p3)
        );
    }

    #[test]
    fn feed_etag_differs_across_feed_ids() {
        let p = pairs(&[("a", 1)]);
        assert_ne!(
            feed_etag("urn:carrel:all", "/opds/all", &["a"], &p),
            feed_etag("urn:carrel:new", "/opds/all", &["a"], &p)
        );
    }

    #[test]
    fn feed_etag_differs_across_self_href() {
        // Two page URLs of the same feed, hashing the identical whole-set
        // `pairs` input, must still produce different tags — this is the
        // mechanism the handler-level page-collision tests below rely on.
        let p = pairs(&[("a", 1)]);
        assert_ne!(
            feed_etag("urn:carrel:all", "/opds/all", &["a"], &p),
            feed_etag("urn:carrel:all", "/opds/all?page=1", &["a"], &p)
        );
    }

    #[test]
    fn if_none_match_absent_header_no_match() {
        let headers = HeaderMap::new();
        assert!(!if_none_match_matches(&headers, "W/\"abc\""));
    }

    #[test]
    fn if_none_match_exact_weak_and_star() {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "W/\"abc\"".parse().unwrap());
        assert!(if_none_match_matches(&headers, "W/\"abc\""));

        // Strong-form client tag still matches our weak tag (weak comparison)
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "\"abc\"".parse().unwrap());
        assert!(if_none_match_matches(&headers, "W/\"abc\""));

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "*".parse().unwrap());
        assert!(if_none_match_matches(&headers, "W/\"anything\""));
    }

    #[test]
    fn if_none_match_comma_list_and_mismatch() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            "\"zzz\", W/\"abc\", \"q\"".parse().unwrap(),
        );
        assert!(if_none_match_matches(&headers, "W/\"abc\""));

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "W/\"other\"".parse().unwrap());
        assert!(!if_none_match_matches(&headers, "W/\"abc\""));
    }

    #[test]
    fn max_updated_picks_max_of_rendered_only() {
        let p = pairs(&[("a", 10), ("b", 50), ("c", 99)]);
        assert_eq!(max_updated(&["a", "b"], &p), Some(50));
        assert_eq!(max_updated(&[], &p), None);
    }

    use super::super::{auth, WebState};
    use axum::extract::{Path as AxumPath, Query as AxumQuery, State as AxumState};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn etag_test_book(id: &str, added_at: i64) -> Book {
        Book {
            id: id.to_string(),
            title: format!("Book {id}"),
            author: "Author".to_string(),
            file_path: format!("/tmp/{id}.epub"),
            cover_path: None,
            total_chapters: 1,
            added_at,
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
            is_imported: true,
            want_to_read: false,
        }
    }

    fn seeded_state(books: &[(&str, i64)]) -> WebState {
        let pool = crate::db::create_pool(&PathBuf::from(":memory:")).expect("in-memory DB");
        {
            let conn = pool.get().unwrap();
            for (id, ts) in books {
                crate::db::insert_book(&conn, &etag_test_book(id, *ts)).unwrap();
            }
        }
        WebState {
            archives: carrel_core::reader::ArchiveCaches::with_capacity(2),
            pool: Arc::new(Mutex::new(pool)),
            data_dir: PathBuf::from("/tmp"),
            cache_dir: std::env::temp_dir(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(std::collections::HashSet::from([
                "default".to_string()
            ]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: Arc::new(Mutex::new(None)),
        }
    }

    fn response_etag(resp: &axum::response::Response) -> Option<String> {
        resp.headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[tokio::test]
    async fn all_books_sets_etag_and_returns_304_on_match() {
        let state = seeded_state(&[("b1", 100), ("b2", 200)]);

        let resp = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = response_etag(&resp).expect("200 must carry ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response_etag(&resp).as_deref(), Some(etag.as_str()));
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "304 must have empty body");
    }

    #[tokio::test]
    async fn all_books_etag_changes_after_book_mutation() {
        let state = seeded_state(&[("b1", 100)]);

        let resp = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let etag = response_etag(&resp).unwrap();

        state
            .conn()
            .unwrap()
            .execute("UPDATE books SET updated_at = 999 WHERE id = 'b1'", [])
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "stale tag must re-send");
        assert_ne!(response_etag(&resp).unwrap(), etag);
    }

    #[tokio::test]
    async fn new_books_ignores_changes_outside_top_25() {
        // 26 books: ids b00..b25, added_at ascending — b00 is outside top-25.
        let books: Vec<(String, i64)> = (0..26)
            .map(|i| (format!("b{i:02}"), 1000 + i as i64))
            .collect();
        let refs: Vec<(&str, i64)> = books.iter().map(|(s, t)| (s.as_str(), *t)).collect();
        let state = seeded_state(&refs);

        let resp = new_books(AxumState(state.clone()), HeaderMap::new())
            .await
            .unwrap();
        let etag = response_etag(&resp).unwrap();

        // Bump the one book NOT rendered (lowest added_at) — tag must not change.
        state
            .conn()
            .unwrap()
            .execute("UPDATE books SET updated_at = 9999 WHERE id = 'b00'", [])
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = new_books(AxumState(state.clone()), headers).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_MODIFIED,
            "change outside rendered top-25 must not invalidate /opds/new"
        );
    }

    #[tokio::test]
    async fn collection_feed_etag_changes_on_membership_change() {
        let state = seeded_state(&[("b1", 100), ("b2", 200)]);
        {
            let conn = state.conn().unwrap();
            let coll = crate::models::Collection {
                id: "c1".to_string(),
                name: "Test".to_string(),
                r#type: crate::models::CollectionType::Manual,
                icon: None,
                color: None,
                created_at: 1,
                updated_at: 1,
                rules: Vec::new(),
            };
            crate::db::insert_collection(&conn, &coll).unwrap();
            crate::db::add_book_to_collection(&conn, "b1", "c1").unwrap();
        }

        let resp = collection_feed(
            AxumState(state.clone()),
            AxumPath("c1".to_string()),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = response_etag(&resp).expect("collection 200 must carry ETag");

        // Membership change → new tag
        crate::db::add_book_to_collection(&state.conn().unwrap(), "b2", "c1").unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = collection_feed(
            AxumState(state.clone()),
            AxumPath("c1".to_string()),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_ne!(response_etag(&resp).unwrap(), etag);
    }

    async fn descriptor_body(host: Option<&str>, forwarded_proto: Option<&str>) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(h) = host {
            headers.insert(header::HOST, h.parse().unwrap());
        }
        if let Some(p) = forwarded_proto {
            headers.insert("x-forwarded-proto", p.parse().unwrap());
        }
        opensearch_descriptor(
            headers,
            axum::http::Uri::from_static("/opds/opensearch.xml"),
        )
        .await
    }

    async fn body_string(resp: Response) -> String {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    /// Every feed must advertise search BOTH ways: the spec's descriptor link,
    /// for third-party readers that only recognise that form, and the inline
    /// template Carrel's own client uses directly.
    ///
    /// Order is deliberate. `carrel_core::opds::parse_feed` assigns
    /// `search_url` on every matching link without checking whether one is
    /// already set, so the LAST `rel="search"` link wins — keeping the inline
    /// template last leaves Carrel's own client on the cheaper direct-template
    /// path. The descriptor is written to be correct either way, so a future
    /// parser that took the first link instead would also work.
    #[test]
    fn feed_advertises_search_as_descriptor_and_inline_template() {
        let xml = wrap_feed(
            "T",
            "urn:carrel:test",
            "",
            "/opds/all",
            ATOM_ACQ_TYPE,
            None,
            None,
        );
        let descriptor = format!(
            r#"<link rel="search" href="/opds/opensearch.xml" type="{OPENSEARCH_DESC_TYPE}"/>"#
        );
        let inline = format!(
            r#"<link rel="search" href="/opds/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>"#
        );
        let d = xml.find(&descriptor).expect("descriptor search link");
        let i = xml.find(&inline).expect("inline template search link");
        assert!(d < i, "inline template must come last so it wins");
    }

    /// The descriptor's `template` must be ABSOLUTE.
    /// `resolve_search_url_with_context` returns it verbatim — it never
    /// resolves it against the descriptor's own URL — and for a catalog with
    /// stored credentials it requires the template to be on the descriptor's
    /// origin. A relative template would be discarded and search would
    /// silently vanish for exactly the authenticated catalogs this is for.
    #[tokio::test]
    async fn opensearch_descriptor_template_is_absolute_and_same_origin() {
        let resp = descriptor_body(Some("192.168.0.50:7788"), None).await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            OPENSEARCH_DESC_TYPE
        );
        let xml = body_string(resp).await;
        assert!(
            xml.contains(r#"template="http://192.168.0.50:7788/opds/search?q={searchTerms}""#),
            "template must be absolute on the dialed authority, got: {xml}"
        );
        assert!(xml.contains(&format!(r#"type="{ATOM_ACQ_TYPE}""#)));
    }

    /// A TLS-terminating proxy in front (Tailscale) makes the catalog https;
    /// the template has to match, or it lands on the wrong origin.
    #[tokio::test]
    async fn opensearch_descriptor_honors_forwarded_proto() {
        let xml =
            body_string(descriptor_body(Some("carrel.example.ts.net"), Some("https")).await).await;
        assert!(
            xml.contains(r#"template="https://carrel.example.ts.net/opds/search?q={searchTerms}""#),
            "got: {xml}"
        );
    }

    /// A Host that could break out of the XML attribute — or graft a different
    /// path onto the template a client will later call with the catalog's
    /// credential — is refused rather than escaped and echoed.
    #[tokio::test]
    async fn opensearch_descriptor_rejects_unsafe_authority() {
        let hosts = ["evil\" foo=\"bar", "host/other", "host with space"];
        let mut checked = 0;
        for host in hosts {
            let mut headers = HeaderMap::new();
            // A header value that cannot even be constructed is already
            // rejected by hyper; only test ones that can.
            let Ok(value) = host.parse() else { continue };
            checked += 1;
            headers.insert(header::HOST, value);
            let resp = opensearch_descriptor(
                headers,
                axum::http::Uri::from_static("/opds/opensearch.xml"),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "host {host:?} must be refused"
            );
        }
        assert_eq!(
            checked,
            hosts.len(),
            "every unsafe host must actually reach the handler, not be skipped"
        );
    }

    #[tokio::test]
    async fn root_has_no_etag() {
        let resp = root_catalog().await;
        assert!(
            response_etag(&resp).is_none(),
            "root catalog is out of ETag scope"
        );
    }

    fn search_query(q: Option<&str>, page: Option<usize>) -> SearchQuery {
        SearchQuery {
            q: q.map(str::to_string),
            page: page.map(|p| p.to_string()),
        }
    }

    async fn call_search(state: &WebState, q: Option<&str>, page: Option<usize>) -> Response {
        search_books(
            AxumState(state.clone()),
            AxumQuery(search_query(q, page)),
            HeaderMap::new(),
        )
        .await
        .unwrap()
    }

    fn seeded_state_with_books(books: Vec<Book>) -> WebState {
        let pool = crate::db::create_pool(&PathBuf::from(":memory:")).expect("in-memory DB");
        {
            let conn = pool.get().unwrap();
            for book in &books {
                crate::db::insert_book(&conn, book).unwrap();
            }
        }
        WebState {
            archives: carrel_core::reader::ArchiveCaches::with_capacity(2),
            pool: Arc::new(Mutex::new(pool)),
            data_dir: PathBuf::from("/tmp"),
            cache_dir: std::env::temp_dir(),
            pin_hash: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
            active_profile_name: Arc::new(Mutex::new("default".to_string())),
            unlocked_profiles: Arc::new(Mutex::new(std::collections::HashSet::from([
                "default".to_string()
            ]))),
            private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            profile_host: None,
            dictionary_pool: Arc::new(Mutex::new(None)),
        }
    }

    fn search_test_book(id: &str, title: &str, author: &str, added_at: i64) -> Book {
        let mut book = etag_test_book(id, added_at);
        book.title = title.to_string();
        book.author = author.to_string();
        book
    }

    /// A filter that would pass with the feature deleted proves nothing, so
    /// every fixture here mixes matching and non-matching books.
    fn search_fixture() -> Vec<Book> {
        vec![
            search_test_book("t1", "The Great Gatsby", "F. Scott Fitzgerald", 100),
            search_test_book("a1", "Moby Dick", "Herman Melville", 200),
            search_test_book("none1", "Unrelated Tome", "Someone Else", 300),
        ]
    }

    #[tokio::test]
    async fn search_matches_title_and_excludes_non_matches() {
        let state = seeded_state_with_books(search_fixture());
        let xml = body_string(call_search(&state, Some("Gatsby"), None).await).await;
        assert!(xml.contains("urn:carrel:t1"));
        assert!(!xml.contains("urn:carrel:a1"));
        assert!(!xml.contains("urn:carrel:none1"));
    }

    #[tokio::test]
    async fn search_matches_author_and_excludes_non_matches() {
        let state = seeded_state_with_books(search_fixture());
        let xml = body_string(call_search(&state, Some("Melville"), None).await).await;
        assert!(xml.contains("urn:carrel:a1"));
        assert!(!xml.contains("urn:carrel:t1"));
        assert!(!xml.contains("urn:carrel:none1"));
    }

    #[tokio::test]
    async fn search_folds_unicode_case_via_carrel_lower() {
        // SQLite's built-in LOWER() is ASCII-only, so a query that only
        // worked via `carrel_lower` (Unicode-aware) pins the module's whole
        // reason for existing — `LOWER('É')` stays `'É'` and would not match
        // a lowercase query.
        let books = vec![
            search_test_book("u1", "Éducation sentimentale", "Gustave Flaubert", 100),
            search_test_book("u2", "Der Steppenwolf", "Hermann Süskind", 200),
        ];
        let state = seeded_state_with_books(books);
        let xml = body_string(call_search(&state, Some("éducation"), None).await).await;
        assert!(xml.contains("urn:carrel:u1"));
        assert!(!xml.contains("urn:carrel:u2"));
    }

    #[tokio::test]
    async fn search_absent_or_empty_q_returns_whole_library() {
        let state = seeded_state_with_books(search_fixture());

        let xml_absent = body_string(call_search(&state, None, None).await).await;
        for id in ["t1", "a1", "none1"] {
            assert!(
                xml_absent.contains(&format!("urn:carrel:{id}")),
                "absent q must return the whole library, missing {id}"
            );
        }

        let xml_empty = body_string(call_search(&state, Some(""), None).await).await;
        for id in ["t1", "a1", "none1"] {
            assert!(
                xml_empty.contains(&format!("urn:carrel:{id}")),
                "empty q must return the whole library, missing {id}"
            );
        }
    }

    #[tokio::test]
    async fn search_sets_etag_and_returns_304_on_match() {
        let state = seeded_state_with_books(search_fixture());

        let resp = call_search(&state, Some("Gatsby"), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = response_etag(&resp).expect("200 must carry ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = search_books(
            AxumState(state.clone()),
            AxumQuery(search_query(Some("Gatsby"), None)),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response_etag(&resp).as_deref(), Some(etag.as_str()));
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "304 must have empty body");
    }

    #[tokio::test]
    async fn search_etag_changes_when_matching_set_changes() {
        let state = seeded_state_with_books(search_fixture());

        let resp = call_search(&state, Some("Gatsby"), None).await;
        let etag = response_etag(&resp).unwrap();

        // Add a second book that also matches the same search term — the
        // filtered set this feed's ETag covers has changed even though
        // neither existing row's own timestamp did.
        crate::db::insert_book(
            &state.conn().unwrap(),
            &search_test_book("t2", "Gatsby Revisited", "Someone New", 400),
        )
        .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
        let resp = search_books(
            AxumState(state.clone()),
            AxumQuery(search_query(Some("Gatsby"), None)),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "stale tag must re-send");
        assert_ne!(response_etag(&resp).unwrap(), etag);
    }

    /// How `page` deserializes is only reachable through the extractor, and
    /// every other test here hand-builds `SearchQuery`, so this is the one
    /// place the wire behaviour of the new parameter is pinned.
    ///
    /// Before this milestone `SearchQuery` had no `page` field at all, so
    /// axum ignored a stray `page=` entirely and the feed served 200. A bare
    /// `Option<usize>` would instead reject the whole request, which is the
    /// trap `api.rs`'s `BookQuery` documents on its `want_to_read` field:
    /// axum's `Query` extraction is all-or-nothing, so one malformed value a
    /// proxy or client appended takes the catalog down with it.
    ///
    /// One case this does *not* rescue, pinned below so nobody assumes
    /// otherwise: a **repeated** `?page=1&page=2` is still rejected, because
    /// serde's derive reports a duplicate field before any of this runs. That
    /// matches how a repeated `?q=` has always behaved here, so the endpoint
    /// has never tolerated duplicated parameters — widening `page` alone
    /// would only make the two inconsistent.
    #[test]
    fn search_query_tolerates_a_malformed_page() {
        use axum::extract::Query as AxumQ;

        for (uri, expected) in [
            ("/opds/search?q=x", None),
            ("/opds/search?q=x&page=2", Some(2)),
            // Each of these would 400 the whole feed with a bare Option<usize>.
            ("/opds/search?q=x&page=", None),
            ("/opds/search?q=x&page=abc", None),
            ("/opds/search?q=x&page=-1", None),
        ] {
            let parsed: AxumQ<SearchQuery> = AxumQ::try_from_uri(&uri.parse().unwrap())
                .unwrap_or_else(|e| panic!("{uri} must not be rejected: {e:?}"));
            assert_eq!(parsed.0.q.as_deref(), Some("x"), "{uri}");
            assert_eq!(parsed.0.page(), expected.unwrap_or(0), "{uri}");
        }

        // A repeated parameter is still rejected — serde's derive reports the
        // duplicate field before the lenient parse gets a look in. Pinned
        // because the field's doc comment claims leniency, and this is the
        // edge that claim does not cover. A repeated `q` behaves the same way
        // and always has, so the endpoint is at least consistent.
        for uri in ["/opds/search?q=x&page=1&page=2", "/opds/search?q=x&q=y"] {
            let parsed: Result<AxumQ<SearchQuery>, _> = AxumQ::try_from_uri(&uri.parse().unwrap());
            assert!(parsed.is_err(), "{uri} is expected to be rejected");
        }
    }

    /// The design decision behind this feed's ETag is that the digest covers
    /// the whole *filtered* set (not the whole library, not just the page)
    /// *and* the request's own URL. Two pages of one search therefore no
    /// longer share a literal tag (see `search_page_urls_get_different_etags`
    /// above) — self_href differs — but they must still move *together* on a
    /// change to the filtered set. These assertions are what separate that
    /// from wrong implementations:
    ///   * a non-matching book changing must NOT move either page's tag
    ///     (rules out hashing every pair in the library), and
    ///   * editing a book that only renders on page 0 must still move page
    ///     1's tag too (rules out a tag that hashes only the ids actually
    ///     rendered on that page, which would serve a stale page 1 after a
    ///     deletion on page 0).
    #[tokio::test]
    async fn search_etag_covers_the_filtered_set_not_the_library_or_the_page() {
        let mut books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        books.push(search_test_book("other", "Unrelated", "Nobody", 900));
        let state = seeded_state_with_books(books);

        let page0 = call_search(&state, Some("Matched"), None).await;
        let tag0 = response_etag(&page0).expect("search must set an ETag");
        let page1 = call_search(&state, Some("Matched"), Some(1)).await;
        let tag1 = response_etag(&page1).expect("search must set an ETag");

        // Touch only the book that does not match the term.
        {
            let conn = state.conn().unwrap();
            carrel_core::db::set_want_to_read(&conn, "other", true).unwrap();
        }
        let after0 = call_search(&state, Some("Matched"), None).await;
        let after1 = call_search(&state, Some("Matched"), Some(1)).await;
        assert_eq!(
            response_etag(&after0).as_deref(),
            Some(tag0.as_str()),
            "a change to a book outside the filtered set must not move page 0's tag"
        );
        assert_eq!(
            response_etag(&after1).as_deref(),
            Some(tag1.as_str()),
            "a change to a book outside the filtered set must not move page 1's tag"
        );

        // ...but editing a book that IS in the set must move BOTH pages' tags.
        //
        // Which assertion does the work here is not obvious, so state it: the
        // fixture is m00..m50 with `added_at = i`, sorted `added_at DESC, id`,
        // so the order is m50..m00 and m00 lands at index 50 — alone on page 1.
        // Editing m00 therefore makes the page-1 assertion a tautology: a
        // digest scoped to a single page's rendered ids would move page 1's tag
        // too, because m00 is exactly what page 1 renders.
        //
        // The **page-0** assertion is the discriminating one. m00 is not among
        // page 0's rendered ids, so a per-page digest would leave page 0's tag
        // unmoved and that assertion would fail. Delete it and criterion 4
        // stops being pinned while the test still passes.
        {
            let conn = state.conn().unwrap();
            carrel_core::db::set_want_to_read(&conn, "m00", true).unwrap();
        }
        let edited0 = call_search(&state, Some("Matched"), None).await;
        let edited1 = call_search(&state, Some("Matched"), Some(1)).await;
        assert_ne!(
            response_etag(&edited0).as_deref(),
            Some(tag0.as_str()),
            "editing m00 — which page 0 does NOT render — must still move page \
             0's tag, because the digest covers the whole filtered set. This is \
             the assertion that rules out a per-page digest."
        );
        assert_ne!(
            response_etag(&edited1).as_deref(),
            Some(tag1.as_str()),
            "editing m00 must move page 1's tag, which renders it"
        );
    }

    /// `page` is caller-supplied and parsed leniently, so a value large
    /// enough to overflow `page * OPDS_PAGE_SIZE` is reachable from the wire.
    /// Unsaturated that multiply panics in debug — a 500 with no
    /// `CatchPanicLayer` in front of it — and wraps in release, where
    /// `start + OPDS_PAGE_SIZE` then wraps back near zero and advertises a
    /// `next` link to page 0. Neither shows up in the other paging tests.
    #[tokio::test]
    async fn search_page_at_usize_max_is_empty_and_does_not_link_onward() {
        let books: Vec<Book> = (0..3)
            .map(|i| search_test_book(&format!("m{i}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let xml = body_string(call_search(&state, Some("Matched"), Some(usize::MAX)).await).await;
        assert!(!xml.contains("<entry>"), "must be empty: {xml}");
        assert!(
            !xml.contains(r#"rel="next""#),
            "must not link onward: {xml}"
        );
    }

    #[tokio::test]
    async fn search_page_beyond_the_last_is_an_empty_feed_with_no_next() {
        let books: Vec<Book> = (0..3)
            .map(|i| search_test_book(&format!("m{i}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let xml = body_string(call_search(&state, Some("Matched"), Some(5)).await).await;
        assert!(
            !xml.contains("<entry>"),
            "page past the end must be empty: {xml}"
        );
        assert!(
            !xml.contains(r#"rel="next""#),
            "no next past the end: {xml}"
        );
        assert!(
            xml.contains(r#"<link rel="self" href="/opds/search?q=Matched&amp;page=5""#),
            "self href must still name the requested page: {xml}"
        );
    }

    /// The boundary `has_next` gets wrong if it is written `<=` rather than
    /// `<`: exactly one full page and nothing after it must not advertise a
    /// next link to an empty page. The 51-book test cannot catch that.
    #[tokio::test]
    async fn search_exactly_one_full_page_has_no_next_link() {
        let books: Vec<Book> = (0..OPDS_PAGE_SIZE)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let xml = body_string(call_search(&state, Some("Matched"), None).await).await;
        assert_eq!(
            xml.matches("<entry>").count(),
            OPDS_PAGE_SIZE,
            "the page should be full"
        );
        assert!(
            !xml.contains(r#"rel="next""#),
            "exactly one full page must not link to an empty next page: {xml}"
        );
    }

    #[tokio::test]
    async fn search_self_href_omits_page_at_page_zero() {
        let state = seeded_state_with_books(search_fixture());
        let xml = body_string(call_search(&state, Some("Gatsby"), None).await).await;
        assert!(
            xml.contains(r#"<link rel="self" href="/opds/search?q=Gatsby" type=""#),
            "self href must omit page at page 0, got: {xml}"
        );
        assert!(!xml.contains("q=Gatsby&amp;page=0"));
    }

    /// The paged hrefs carry two query params, so they contain a literal `&`
    /// inside a double-quoted XML attribute — and `wrap_feed` interpolates
    /// `self_href`/`next_href` raw, with no escaping of its own. A raw `&`
    /// there opens an entity reference that never terminates, which every
    /// `contains(...)` assertion in this module would happily pass.
    ///
    /// Note that merely reading the events is *not* enough: quick-xml's
    /// reader tolerates an unescaped `&` in an attribute value and only
    /// reports it when the value is normalized. Verified by dropping the
    /// escape and watching an event-only version of this test still pass. So
    /// normalize every attribute — that is what makes this discriminating.
    #[tokio::test]
    async fn search_paged_feed_attributes_normalize_cleanly() {
        use quick_xml::events::Event;

        let books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        for page in [None, Some(1)] {
            let xml = body_string(call_search(&state, Some("Matched"), page).await).await;
            let mut reader = quick_xml::Reader::from_str(&xml);
            let mut checked = 0usize;
            loop {
                match reader.read_event() {
                    Ok(Event::Eof) => break,
                    Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                        for attr in e.attributes() {
                            let attr = attr.unwrap_or_else(|err| {
                                panic!("page {page:?}: bad attribute: {err}")
                            });
                            attr.normalized_value(quick_xml::XmlVersion::Explicit1_0)
                                .unwrap_or_else(|err| {
                                    panic!(
                                        "page {page:?}: attribute {:?} does not normalize: {err}\n{xml}",
                                        String::from_utf8_lossy(attr.key.as_ref())
                                    )
                                });
                            checked += 1;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => panic!("page {page:?} produced ill-formed XML: {e}\n{xml}"),
                }
            }
            assert!(checked > 0, "page {page:?}: parsed no attributes at all");
        }
    }

    #[tokio::test]
    async fn search_next_link_present_on_full_page_absent_on_last() {
        // 51 matching books: page 0 is full (50) and has a next link; page 1
        // (the last, 1 book) has none.
        let books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let xml_first = body_string(call_search(&state, Some("Matched"), None).await).await;
        assert!(
            xml_first.contains(r#"<link rel="next" href="/opds/search?q=Matched&amp;page=1""#),
            "full page must carry a next link, got: {xml_first}"
        );

        let xml_last = body_string(call_search(&state, Some("Matched"), Some(1)).await).await;
        assert!(
            !xml_last.contains(r#"rel="next""#),
            "last page must have no next link, got: {xml_last}"
        );
    }

    /// Behaviour change 1: `db::list_books`'s old `added_at DESC` order had
    /// no tie-break; `BookSort::DateAdded` appends `id`. Several books
    /// sharing one `added_at` must therefore now get a stable order that
    /// neither repeats nor skips a book across pages.
    #[tokio::test]
    async fn search_paging_over_tied_added_at_is_stable_no_repeat_no_skip() {
        // 51 matching books, ALL sharing the same added_at — without the
        // `id` tie-break this order (and therefore the paging split) would
        // be undefined between calls. Inserted in DESCENDING id order
        // (deliberately the opposite of the expected ascending-id result) so
        // this discriminates against a query that just falls back to
        // insertion/rowid order rather than actually sorting by id: with no
        // tie-break, page 0 would come back highest-id-first (m50..m01) and
        // every assertion below would fail.
        let books: Vec<Book> = (0..51)
            .rev()
            .map(|i| search_test_book(&format!("m{i:02}"), "Tied Title", "Author", 1000))
            .collect();
        let expected_order: Vec<String> = {
            let mut ids: Vec<String> = (0..51).map(|i| format!("m{i:02}")).collect();
            ids.sort();
            ids
        };
        let state = seeded_state_with_books(books);

        let xml_p0 = body_string(call_search(&state, Some("Tied"), None).await).await;
        let xml_p1 = body_string(call_search(&state, Some("Tied"), Some(1)).await).await;

        let page0_ids = &expected_order[0..50];
        let page1_ids = &expected_order[50..51];
        for id in page0_ids {
            assert!(
                xml_p0.contains(&format!("urn:carrel:{id}")),
                "page 0 missing {id}"
            );
            assert!(
                !xml_p1.contains(&format!("urn:carrel:{id}")),
                "page 1 must not repeat {id}"
            );
        }
        for id in page1_ids {
            assert!(
                xml_p1.contains(&format!("urn:carrel:{id}")),
                "page 1 missing {id}"
            );
            assert!(
                !xml_p0.contains(&format!("urn:carrel:{id}")),
                "page 0 must not have {id} — no book may be skipped or duplicated"
            );
        }
    }

    // --- Per-URL ETag (self_href in the digest) -----------------------
    //
    // Every page of a feed used to get the same ETag, so a client that cached
    // page 0's validator got an empty 304 body back for page 1. These pin the
    // fix at the handler level: two page URLs of the same feed/search must
    // never share a tag, a stale (other page's) tag must not short-circuit a
    // 304, and a page's own tag must still 304 — the feature must stop
    // misfiring, not stop working.

    #[tokio::test]
    async fn all_books_page_urls_get_different_etags() {
        let books: Vec<(String, i64)> = (0..60).map(|i| (format!("b{i:02}"), i as i64)).collect();
        let refs: Vec<(&str, i64)> = books.iter().map(|(s, t)| (s.as_str(), *t)).collect();
        let state = seeded_state(&refs);

        let page0 = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let tag0 = response_etag(&page0).expect("page 0 must carry an ETag");

        let page1 = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: Some(1) }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let tag1 = response_etag(&page1).expect("page 1 must carry an ETag");

        assert_ne!(
            tag0, tag1,
            "page 0 and page 1 of /opds/all must not share an ETag"
        );
    }

    #[tokio::test]
    async fn all_books_page1_with_page0_etag_is_200_not_304() {
        let books: Vec<(String, i64)> = (0..60).map(|i| (format!("b{i:02}"), i as i64)).collect();
        let refs: Vec<(&str, i64)> = books.iter().map(|(s, t)| (s.as_str(), *t)).collect();
        let state = seeded_state(&refs);

        let page0 = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let tag0 = response_etag(&page0).expect("page 0 must carry an ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag0.parse().unwrap());
        let page1 = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: Some(1) }),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(
            page1.status(),
            StatusCode::OK,
            "page 0's ETag must not satisfy page 1's If-None-Match"
        );
    }

    #[tokio::test]
    async fn all_books_page1_with_own_etag_is_still_304() {
        let books: Vec<(String, i64)> = (0..60).map(|i| (format!("b{i:02}"), i as i64)).collect();
        let refs: Vec<(&str, i64)> = books.iter().map(|(s, t)| (s.as_str(), *t)).collect();
        let state = seeded_state(&refs);

        let page1 = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: Some(1) }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let tag1 = response_etag(&page1).expect("page 1 must carry an ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag1.parse().unwrap());
        let page1_again = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: Some(1) }),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(
            page1_again.status(),
            StatusCode::NOT_MODIFIED,
            "a page's own ETag must still 304 it"
        );
    }

    #[tokio::test]
    async fn search_page_urls_get_different_etags() {
        let books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let page0 = call_search(&state, Some("Matched"), None).await;
        let tag0 = response_etag(&page0).expect("page 0 must carry an ETag");
        let page1 = call_search(&state, Some("Matched"), Some(1)).await;
        let tag1 = response_etag(&page1).expect("page 1 must carry an ETag");

        assert_ne!(
            tag0, tag1,
            "page 0 and page 1 of the same search must not share an ETag"
        );
    }

    #[tokio::test]
    async fn search_page1_with_page0_etag_is_200_not_304() {
        let books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let page0 = call_search(&state, Some("Matched"), None).await;
        let tag0 = response_etag(&page0).expect("page 0 must carry an ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag0.parse().unwrap());
        let page1 = search_books(
            AxumState(state.clone()),
            AxumQuery(search_query(Some("Matched"), Some(1))),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(
            page1.status(),
            StatusCode::OK,
            "page 0's ETag must not satisfy page 1's If-None-Match"
        );
    }

    #[tokio::test]
    async fn search_page1_with_own_etag_is_still_304() {
        let books: Vec<Book> = (0..51)
            .map(|i| search_test_book(&format!("m{i:02}"), "Matched Title", "Author", i as i64))
            .collect();
        let state = seeded_state_with_books(books);

        let page1 = call_search(&state, Some("Matched"), Some(1)).await;
        let tag1 = response_etag(&page1).expect("page 1 must carry an ETag");

        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag1.parse().unwrap());
        let page1_again = search_books(
            AxumState(state.clone()),
            AxumQuery(search_query(Some("Matched"), Some(1))),
            headers,
        )
        .await
        .unwrap();
        assert_eq!(
            page1_again.status(),
            StatusCode::NOT_MODIFIED,
            "a page's own ETag must still 304 it"
        );
    }

    #[tokio::test]
    async fn feed_updated_reflects_max_book_updated_at() {
        let state = seeded_state(&[("b1", 100), ("b2", 1700000000)]);
        let resp = all_books(
            AxumState(state.clone()),
            AxumQuery(PaginationQuery { page: None }),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let xml = String::from_utf8(body.to_vec()).unwrap();
        // 1700000000 = 2023-11-14T22:13:20Z — feed-level <updated> is the max
        // updated_at of rendered books, not request time.
        assert!(
            xml.contains("<updated>2023-11-14T22:13:20Z</updated>"),
            "feed <updated> must be max book updated_at; got: {}",
            &xml[..xml.len().min(600)]
        );
    }
}

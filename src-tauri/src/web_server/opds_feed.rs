use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use super::{folio_status, WebState};
use crate::db;
use crate::models::Book;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const ATOM_CONTENT_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";
const ATOM_ACQ_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";

/// Build all `/opds/` routes.
pub fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/", get(root_catalog))
        .route("/all", get(all_books))
        .route("/new", get(new_books))
        .route("/collections/{id}", get(collection_feed))
        .route("/search", get(search_books))
        .with_state(state)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Weak ETag over the rendered book subset: SHA-256 of the feed id plus the
/// sorted `(id, updated_at)` pairs of exactly the books this feed renders.
/// Weak (`W/"..."`) because equal-state bodies are not byte-identical.
/// Hashing pairs (not raw timestamps in the tag) avoids leaking library
/// activity times to clients.
fn feed_etag(feed_id: &str, rendered_ids: &[&str], pairs: &HashMap<String, i64>) -> String {
    let mut ids: Vec<&str> = rendered_ids.to_vec();
    ids.sort_unstable();
    let mut h = Sha256::new();
    h.update(feed_id.as_bytes());
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

    format!(
        r#"<entry>
  <title>{title}</title>
  <id>urn:folio:{id}</id>
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
  <link rel="search" href="/opds/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>
{next_link}
{entries}
</feed>"#
    )
}

async fn root_catalog() -> Response {
    let entries = format!(
        r#"<entry>
  <title>All Books</title>
  <id>urn:folio:all</id>
  <updated>{now}</updated>
  <content type="text">Browse the entire library</content>
  <link rel="subsection" href="/opds/all" type="{ATOM_ACQ_TYPE}"/>
</entry>
<entry>
  <title>Recently Added</title>
  <id>urn:folio:new</id>
  <updated>{now}</updated>
  <content type="text">Books added recently</content>
  <link rel="subsection" href="/opds/new" type="{ATOM_ACQ_TYPE}"/>
</entry>"#,
        now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
    );

    let xml = wrap_feed(
        "Folio Library",
        "urn:folio:root",
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
    page: Option<usize>,
}

async fn all_books(
    State(state): State<WebState>,
    Query(params): Query<PaginationQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(folio_status)?;
    let books = db::list_books(&conn).map_err(folio_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(folio_status)?;

    // Whole-set tag shared by every page: clients cache per-URL, so a
    // shared tag across page URLs is correct and any library change
    // invalidates all pages at once.
    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    let etag = feed_etag("urn:folio:all", &rendered_ids, &pairs);
    if if_none_match_matches(&headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let page = params.page.unwrap_or(0);
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
    let self_href = if page > 0 {
        format!("/opds/all?page={page}")
    } else {
        "/opds/all".to_string()
    };

    let xml = wrap_feed(
        "All Books",
        "urn:folio:all",
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
    let conn = state.conn().map_err(folio_status)?;
    let mut books = db::list_books(&conn).map_err(folio_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(folio_status)?;

    // Sort by added_at descending, take 25 most recent
    books.sort_by_key(|b| std::cmp::Reverse(b.added_at));
    books.truncate(25);

    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    let etag = feed_etag("urn:folio:new", &rendered_ids, &pairs);
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
        "urn:folio:new",
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
    let conn = state.conn().map_err(folio_status)?;
    let books = db::get_books_in_collection(&conn, &id).map_err(folio_status)?;
    let pairs = db::book_etag_pairs(&conn).map_err(folio_status)?;

    let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
    // Hash the RESOLVED membership — works for manual and rule-based collections alike.
    let feed_id = format!("urn:folio:collection:{id}");
    let etag = feed_etag(&feed_id, &rendered_ids, &pairs);
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
        &format!("/opds/collections/{id}"),
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
}

async fn search_books(
    State(state): State<WebState>,
    Query(params): Query<SearchQuery>,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.conn().map_err(folio_status)?;
    let books = db::list_books(&conn).map_err(folio_status)?;

    let filtered: Vec<Book> = match params.q {
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

    let entries: String = filtered
        .iter()
        .map(book_to_entry)
        .collect::<Vec<_>>()
        .join("\n");

    let search_term = params.q.as_deref().unwrap_or("");
    let xml = wrap_feed(
        &format!("Search: {}", xml_escape(search_term)),
        "urn:folio:search",
        &entries,
        &format!("/opds/search?q={}", urlencoding::encode(search_term)),
        ATOM_ACQ_TYPE,
        None,
        None,
    );

    Ok(([(header::CONTENT_TYPE, ATOM_ACQ_TYPE)], xml).into_response())
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
        assert!(entry.contains("urn:folio:test-1"));
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
        let t1 = feed_etag("urn:folio:all", &["a", "b"], &p);
        let t2 = feed_etag("urn:folio:all", &["b", "a"], &p);
        assert_eq!(t1, t2);
        assert!(t1.starts_with("W/\""), "weak ETag required, got {t1}");
        assert!(t1.ends_with('"'));
    }

    #[test]
    fn feed_etag_changes_on_updated_at_bump_and_set_change() {
        let p1 = pairs(&[("a", 1), ("b", 2)]);
        let base = feed_etag("urn:folio:all", &["a", "b"], &p1);

        // updated_at bump
        let p2 = pairs(&[("a", 1), ("b", 3)]);
        assert_ne!(base, feed_etag("urn:folio:all", &["a", "b"], &p2));

        // id removed from rendered set
        assert_ne!(base, feed_etag("urn:folio:all", &["a"], &p1));

        // id added to rendered set
        let p3 = pairs(&[("a", 1), ("b", 2), ("c", 9)]);
        assert_ne!(base, feed_etag("urn:folio:all", &["a", "b", "c"], &p3));
    }

    #[test]
    fn feed_etag_differs_across_feed_ids() {
        let p = pairs(&[("a", 1)]);
        assert_ne!(
            feed_etag("urn:folio:all", &["a"], &p),
            feed_etag("urn:folio:new", &["a"], &p)
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

    #[tokio::test]
    async fn search_and_root_have_no_etag() {
        let state = seeded_state(&[("b1", 100)]);

        let resp = search_books(
            AxumState(state.clone()),
            AxumQuery(SearchQuery {
                q: Some("Book".to_string()),
            }),
        )
        .await
        .unwrap();
        assert!(
            response_etag(&resp).is_none(),
            "/search is out of ETag scope"
        );

        let resp = root_catalog().await;
        assert!(
            response_etag(&resp).is_none(),
            "root catalog is out of ETag scope"
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

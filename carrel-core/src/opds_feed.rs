//! Pure builder primitives for rendering OPDS Atom feeds.
//!
//! `carrel_core::opds` (the sibling module) is a *client* — it ingests
//! external OPDS catalogs. This module covers the inverse: rendering
//! OPDS Atom XML from `Book` rows. The two responsibilities are kept
//! in separate files so neither one becomes a junk drawer.
//!
//! The public surface is intentionally narrow: pure string-in /
//! string-out functions plus a handful of small types. No HTTP layer,
//! no router, no filesystem access. Caller-side concerns — pagination
//! state, route construction, MIME negotiation against an actual cover
//! file — live in the consuming app.
//!
//! Per-entry hrefs are injected via [`EntryUrls`] so this module never
//! assumes a particular URL scheme. The wider [`wrap_feed`] still emits
//! a hardcoded `/opds` start link and `/opds/search?q=...` search link
//! to preserve the existing OPDS catalog shape exactly — consumers
//! that need a different prefix should wrap the output.

use crate::models::Book;

/// OPDS Atom navigation feed content type.
pub const ATOM_CONTENT_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";

/// OPDS Atom acquisition feed content type.
pub const ATOM_ACQ_TYPE: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";

/// Per-book link block for [`book_to_entry`]. Caller supplies the
/// cover and download URLs because the route shape is consuming-app
/// specific. The builder inlines the URLs as-is (no escaping inside
/// the function — caller passes pre-validated values).
pub struct EntryUrls {
    /// Absolute or app-relative URL for the cover image.
    pub cover_href: String,
    /// Absolute or app-relative URL for the book file download.
    pub download_href: String,
}

/// Feed kind for [`wrap_feed`]. Selects the `type=` attribute on the
/// `<link rel="self">` and `<link rel="next">` elements.
pub enum FeedKind {
    /// `application/atom+xml;profile=opds-catalog;kind=navigation`.
    Navigation,
    /// `application/atom+xml;profile=opds-catalog;kind=acquisition`.
    Acquisition,
}

impl FeedKind {
    fn as_content_type(&self) -> &'static str {
        match self {
            FeedKind::Navigation => ATOM_CONTENT_TYPE,
            FeedKind::Acquisition => ATOM_ACQ_TYPE,
        }
    }
}

/// Escape XML 1.0 entities (`& < > "`).
///
/// Single-quote (`'`) is intentionally not escaped because the
/// rendered XML uses only double-quoted attribute values. Mirrors the
/// existing behaviour of every consumer that has shipped against this
/// helper.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Derive an OPDS acquisition extension + MIME from a MOBI-family
/// book's stored file path.
///
/// `Book::format == BookFormat::Mobi` collapses `.mobi`, `.azw`, and
/// `.azw3` into a single variant at import time; on download we need
/// the original extension back so OPDS clients pick the right parser
/// (the `.azw` vs `.azw3` distinction matters and MIME alone cannot
/// disambiguate them). Falls back to plain `.mobi` when the extension
/// is missing or unrecognised.
pub fn mobi_ext_and_mime(file_path: &str) -> (&'static str, &'static str) {
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

/// Map a cover image path's extension to a MIME type.
///
/// Recognised: `.jpg`/`.jpeg` → `image/jpeg`, `.png` → `image/png`,
/// `.gif` → `image/gif`, `.bmp` → `image/bmp`,
/// `.webp` → `image/webp`. Fallback: `image/jpeg`.
/// `cover_path = None` returns the fallback directly.
///
/// Stays in lockstep with the cover-serving endpoint that any consumer
/// pairs this feed with: if the feed advertises a different MIME than
/// the endpoint actually serves, strict OPDS clients can mis-cache or
/// reject the response.
pub fn cover_mime(cover_path: Option<&str>) -> &'static str {
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

/// Render a single Atom `<entry>` element for `book`.
///
/// The returned string is the entry XML alone (no `<feed>` wrapper).
/// Caller-supplied `urls` MUST already be valid URLs; the builder
/// XML-escapes them before interpolation so query strings containing
/// `&` (e.g. `?a=1&b=2`) produce well-formed Atom. All metadata
/// fields (title, author, description) are likewise XML-escaped
/// internally.
///
/// For MOBI-family books the acquisition link's MIME type is derived
/// from the stored file path via [`mobi_ext_and_mime`] so clients see
/// the correct `.azw` vs `.azw3` distinction. The cover link's MIME
/// is derived via [`cover_mime`] from `book.cover_path` (or omitted
/// entirely is not the right call — the previous shipping behaviour
/// always emits the cover link with the URL the caller supplied, even
/// when `cover_path` is `None`; preserved here for byte-for-byte
/// parity with the desktop renderer).
pub fn book_to_entry(book: &Book, urls: &EntryUrls) -> String {
    let title = xml_escape(&book.title);
    let author = xml_escape(&book.author);
    let id = xml_escape(&book.id);
    let updated = chrono::DateTime::from_timestamp(book.added_at, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "2024-01-01T00:00:00Z".to_string());

    let description = book
        .description
        .as_ref()
        .map(|d| format!("<summary>{}</summary>", xml_escape(d)))
        .unwrap_or_default();

    let cover_href = xml_escape(&urls.cover_href);
    let download_href = xml_escape(&urls.download_href);

    let cover_link = format!(
        r#"<link rel="http://opds-spec.org/image" href="{cover_href}" type="{}"/>"#,
        cover_mime(book.cover_path.as_deref())
    );

    let (ext, mime) = match book.format {
        crate::models::BookFormat::Epub => ("epub", "application/epub+zip"),
        crate::models::BookFormat::Pdf => ("pdf", "application/pdf"),
        crate::models::BookFormat::Cbz => ("cbz", "application/x-cbz"),
        crate::models::BookFormat::Cbr => ("cbr", "application/x-cbr"),
        crate::models::BookFormat::Mobi => mobi_ext_and_mime(&book.file_path),
    };
    let download_link = format!(
        r#"<link rel="http://opds-spec.org/acquisition" href="{download_href}" type="{mime}" title="{title}.{ext}"/>"#,
    );

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

/// Wrap a sequence of pre-built entry XML strings into a complete
/// Atom feed.
///
/// `entries` content is inlined as-is (callers pass strings from
/// [`book_to_entry`]). `title`, `feed_id`, `self_href`, and `next_href`
/// are XML-escaped inside the function. `next_href = Some(...)` adds
/// a `rel="next"` pagination link.
///
/// The emitted feed includes hardcoded `<link rel="start" href="/opds">`
/// and `<link rel="search" href="/opds/search?q={searchTerms}">`
/// elements that match the OPDS catalog shape shipped today. Consumers
/// that mount their catalog under a different prefix should post-process
/// the output.
pub fn wrap_feed(
    title: &str,
    feed_id: &str,
    entries: &[String],
    self_href: &str,
    kind: FeedKind,
    next_href: Option<&str>,
) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let kind_type = kind.as_content_type();
    let title_esc = xml_escape(title);
    let feed_id_esc = xml_escape(feed_id);
    let self_href_esc = xml_escape(self_href);
    let next_link = next_href
        .map(|h| {
            format!(
                r#"  <link rel="next" href="{}" type="{kind_type}"/>"#,
                xml_escape(h)
            )
        })
        .unwrap_or_default();
    let entries_joined = entries.join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:opds="http://opds-spec.org/2010/catalog">
  <id>{feed_id_esc}</id>
  <title>{title_esc}</title>
  <updated>{now}</updated>
  <link rel="self" href="{self_href_esc}" type="{kind_type}"/>
  <link rel="start" href="/opds" type="{ATOM_CONTENT_TYPE}"/>
  <link rel="search" href="/opds/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>
{next_link}
{entries_joined}
</feed>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::BookFormat;

    fn make_book(file_path: &str, format: BookFormat) -> Book {
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

    fn fixed_urls() -> EntryUrls {
        EntryUrls {
            cover_href: "https://example.test/cover/abc".to_string(),
            download_href: "https://example.test/file/abc".to_string(),
        }
    }

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
            format: BookFormat::Epub,
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

        let entry = book_to_entry(&book, &fixed_urls());
        assert!(entry.contains("<title>Test &amp; Book</title>"));
        assert!(entry.contains("Author &lt;Name&gt;"));
        assert!(entry.contains("urn:carrel:test-1"));
        assert!(entry.contains("application/epub+zip"));
        assert!(entry.contains("https://example.test/file/abc"));
        assert!(entry.contains("https://example.test/cover/abc"));
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
        assert_eq!(cover_mime(Some("/tmp/cover.xyz")), "image/jpeg");
        assert_eq!(cover_mime(None), "image/jpeg");
    }

    #[test]
    fn download_link_mime_for_azw3() {
        // AZW3 books must surface `application/vnd.amazon.ebook` with
        // the `.azw3` extension visible in the entry's `title=` so
        // OPDS clients can disambiguate against `.azw`.
        let book = make_book("/lib/story.azw3", BookFormat::Mobi);
        let entry = book_to_entry(&book, &fixed_urls());
        assert!(
            entry.contains("application/vnd.amazon.ebook"),
            "expected azw3 MIME: {entry}"
        );
        assert!(
            entry.contains("title=\"Title.azw3\""),
            "expected .azw3 in entry title attribute: {entry}"
        );
    }

    #[test]
    fn download_link_mime_for_azw() {
        let book = make_book("/lib/story.azw", BookFormat::Mobi);
        let entry = book_to_entry(&book, &fixed_urls());
        assert!(entry.contains("application/vnd.amazon.ebook"));
        assert!(
            entry.contains("title=\"Title.azw\""),
            "expected .azw in entry title attribute: {entry}"
        );
        // The .azw3 extension MUST NOT appear when the underlying
        // file is plain .azw — the title attribute is the OPDS-side
        // disambiguator that consumers rely on.
        assert!(!entry.contains("Title.azw3"));
    }

    #[test]
    fn download_link_mime_for_core_formats() {
        let cases = [
            ("/lib/a.epub", BookFormat::Epub, "application/epub+zip"),
            ("/lib/a.pdf", BookFormat::Pdf, "application/pdf"),
            ("/lib/a.cbz", BookFormat::Cbz, "application/x-cbz"),
            ("/lib/a.cbr", BookFormat::Cbr, "application/x-cbr"),
            (
                "/lib/a.mobi",
                BookFormat::Mobi,
                "application/x-mobipocket-ebook",
            ),
        ];
        for (path, fmt, expected_mime) in cases {
            let book = make_book(path, fmt);
            let entry = book_to_entry(&book, &fixed_urls());
            assert!(
                entry.contains(expected_mime),
                "{expected_mime} missing in entry for {path}:\n{entry}"
            );
        }
    }

    #[test]
    fn opds_cover_link_uses_real_cover_mime() {
        let mut book = make_book("/lib/story.mobi", BookFormat::Mobi);
        book.cover_path = Some("/tmp/covers/book-1/cover.png".to_string());

        let entry = book_to_entry(&book, &fixed_urls());

        assert!(
            entry.contains(r#"href="https://example.test/cover/abc" type="image/png""#),
            "cover link should advertise png mime with caller-supplied href:\n{entry}"
        );
    }

    #[test]
    fn entry_id_is_xml_escaped() {
        // Book IDs are stored as unconstrained TEXT; if a row carries
        // XML-significant characters they must be escaped or the
        // emitted `<id>` element becomes malformed / injectable.
        let mut book = make_book("/lib/story.epub", BookFormat::Epub);
        book.id = "x&y</id><entry>".to_string();
        let entry = book_to_entry(&book, &fixed_urls());
        assert!(
            entry.contains("urn:carrel:x&amp;y&lt;/id&gt;&lt;entry&gt;"),
            "book id must be XML-escaped:\n{entry}"
        );
        assert!(!entry.contains("urn:carrel:x&y"));
        assert!(!entry.contains("</id><entry>"));
    }

    #[test]
    fn entry_hrefs_are_xml_escaped() {
        // Query-string URLs containing `&` must be escaped in the
        // emitted attributes or the feed becomes ill-formed XML.
        let book = make_book("/lib/story.epub", BookFormat::Epub);
        let urls = EntryUrls {
            cover_href: "/books/1/cover?a=1&b=2".to_string(),
            download_href: "/books/1/download?token=abc&format=epub".to_string(),
        };
        let entry = book_to_entry(&book, &urls);
        assert!(
            entry.contains(r#"href="/books/1/cover?a=1&amp;b=2""#),
            "cover href must be XML-escaped:\n{entry}"
        );
        assert!(
            entry.contains(r#"href="/books/1/download?token=abc&amp;format=epub""#),
            "download href must be XML-escaped:\n{entry}"
        );
        assert!(!entry.contains("?a=1&b=2"));
        assert!(!entry.contains("token=abc&format=epub"));
    }

    #[test]
    fn wrap_feed_includes_entries_and_self_link() {
        let entries = vec![
            "<entry><id>a</id></entry>".to_string(),
            "<entry><id>b</id></entry>".to_string(),
        ];
        let feed = wrap_feed(
            "Library",
            "urn:test:lib",
            &entries,
            "/opds/all",
            FeedKind::Acquisition,
            None,
        );

        assert!(feed.contains("<title>Library</title>"));
        assert!(feed.contains("<id>urn:test:lib</id>"));
        assert!(feed.contains(r#"href="/opds/all""#));
        assert!(feed.contains("<entry><id>a</id></entry>"));
        assert!(feed.contains("<entry><id>b</id></entry>"));
        assert!(!feed.contains(r#"rel="next""#));
        // Acquisition kind reflected in `type=` on the self link.
        assert!(feed.contains(ATOM_ACQ_TYPE));
    }

    #[test]
    fn wrap_feed_includes_next_link_when_provided() {
        let feed = wrap_feed(
            "Page 1",
            "urn:test:lib:p1",
            &[],
            "/opds/all?page=1",
            FeedKind::Acquisition,
            Some("/opds/all?page=2&from=somewhere"),
        );

        assert!(feed.contains(r#"rel="next""#));
        // The `&` in the next href must be XML-escaped.
        assert!(
            feed.contains("/opds/all?page=2&amp;from=somewhere"),
            "next href must be XML-escaped:\n{feed}"
        );
        assert!(!feed.contains("/opds/all?page=2&from"));
    }

    #[test]
    fn wrap_feed_navigation_kind_sets_navigation_content_type() {
        let feed = wrap_feed(
            "Root",
            "urn:test:root",
            &[],
            "/opds",
            FeedKind::Navigation,
            None,
        );
        // The self link's `type=` attribute uses the navigation MIME.
        assert!(
            feed.contains(&format!(
                r#"rel="self" href="/opds" type="{ATOM_CONTENT_TYPE}""#
            )),
            "navigation self link should advertise navigation MIME:\n{feed}"
        );
    }
}

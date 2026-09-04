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
//! assumes a particular URL scheme. [`wrap_feed`] still emits a hardcoded
//! `/opds` start link and `/opds/search?q=...` search link, to preserve the
//! existing OPDS catalog shape exactly. Consumers that need a different
//! mount prefix, a discoverable OpenSearch descriptor, or a
//! caching-friendly ETag should call [`render_feed`] with a [`FeedOptions`]
//! directly instead — `wrap_feed` is now a thin `render_feed` call with
//! today's defaults, kept only so existing callers are unaffected.

use crate::models::Book;
use sha2::{Digest, Sha256};

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

    // OPDS clients cache and dedupe on feed and entry ids, so changing the
    // `urn:carrel:*` scheme makes every entry look new to an already-subscribed
    // client. This is the only place the entry-id shape exists — the desktop
    // web server used to carry an identical copy, with this same note, until it
    // adopted this module and the copy was deleted. See the
    // persistence-boundary table in CLAUDE.md.
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

/// Options for [`render_feed`].
///
/// Construct with `..Default::default()` for any fields not being set
/// explicitly, so fields added later don't break existing call sites.
///
/// `Default` is hand-written rather than derived (see the `Default` impl
/// below): a derived
/// `Default` would give `prefix` (and every other `&str` field) `""`,
/// which for `prefix` specifically means a caller who forgets to set it
/// silently gets `rel="start" href=""` — a well-formed feed pointing
/// nowhere, with nothing to error on it.
pub struct FeedOptions<'a> {
    /// Feed-level `<title>`.
    pub title: &'a str,
    /// Feed-level `<id>`.
    pub feed_id: &'a str,
    /// Pre-built entry XML strings, inlined as-is (see [`book_to_entry`]).
    pub entries: &'a [String],
    /// This feed page's own URL, used for `rel="self"`.
    pub self_href: &'a str,
    /// Selects the navigation vs. acquisition `type=` attribute.
    pub kind: FeedKind,
    /// `Some(...)` adds a `rel="next"` pagination link.
    pub next_href: Option<&'a str>,
    /// Mount prefix (e.g. `"/opds"`), used to build `rel="start"` and the
    /// inline-template `rel="search"` link.
    pub prefix: &'a str,
    /// `Some(...)` adds a `rel="search"` link of type
    /// `application/opensearchdescription+xml` pointing at the URL an
    /// [`opensearch_descriptor`] document is served from. `None` omits it.
    pub opensearch_href: Option<&'a str>,
    /// Feed-level `<updated>` timestamp (Unix seconds). `None` falls back
    /// to the render-time wall clock.
    pub updated: Option<i64>,
}

impl Default for FeedOptions<'_> {
    fn default() -> Self {
        Self {
            title: "",
            feed_id: "",
            entries: &[],
            self_href: "",
            // Not derived: a `FeedKind::Navigation` default would let a
            // forgotten `kind` field silently serve an acquisition feed
            // with navigation `type=` attributes — nothing errors on
            // that, clients just read it wrong.
            kind: FeedKind::Acquisition,
            next_href: None,
            prefix: "/opds",
            opensearch_href: None,
            updated: None,
        }
    }
}

/// A rendered feed page and the validator for the *inputs* that produced it.
///
/// The etag identifies the [`FeedOptions`], not the exact octets: with
/// `updated: None` the body's `<updated>` carries the render time, so two
/// renders a second apart share an etag and differ in bytes. That is the
/// intended trade — the private `feed_etag` explains why the struct and not
/// the body is hashed.
///
/// What that means for an HTTP caller depends on `updated`:
///
/// - `updated: None` — `now()` enters the body, so equal etags do not imply
///   equal bytes. The validator **must** be marked weak (`W/"…"`).
/// - `updated: Some(t)` that is representable — every byte of the body is
///   determined by `opts`, so equal etags do imply equal bytes and a strong
///   validator is correct.
///
/// Marking it weak is always safe; marking it strong is only honest in the
/// second case.
pub struct RenderedFeed {
    /// The complete Atom XML document.
    pub body: String,
    /// Bare lowercase hex digest — no `W/`, no quotes. Callers wrap it
    /// themselves (e.g. a tenant-scoped ETag helper that interpolates the
    /// validator raw into the header must never receive anything but the
    /// digest).
    pub etag: String,
}

/// The `<updated>` shape every feed and entry in this module emits.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Deterministic stand-in for a timestamp `chrono` cannot represent, so that
/// only `updated: None` can ever reach the wall clock. Matches the fallback
/// `book_to_entry` already uses.
const EPOCH_FALLBACK: &str = "2024-01-01T00:00:00Z";

macro_rules! next_link_template {
    () => {
        r#"  <link rel="next" href="{href}" type="{kind_type}"/>"#
    };
}

macro_rules! opensearch_link_template {
    () => {
        "  <link rel=\"search\" href=\"{href}\" type=\"application/opensearchdescription+xml\"/>\n"
    };
}

macro_rules! feed_envelope_template {
    () => {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:opds="http://opds-spec.org/2010/catalog">
  <id>{feed_id_esc}</id>
  <title>{title_esc}</title>
  <updated>{updated}</updated>
  <link rel="self" href="{self_href_esc}" type="{kind_type}"/>
  <link rel="start" href="{prefix_esc}" type="{ATOM_CONTENT_TYPE}"/>
{opensearch_link}  <link rel="search" href="{prefix_esc}/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>
{next_link}
{entries_joined}
</feed>"#
    };
}

/// This module's own source text, folded into every feed's digest so that a
/// change to the emitted shape invalidates cached feeds automatically.
///
/// The obvious implementation is a registry of the templates that can reach a
/// feed, and that is what this was. It kept being incomplete. Three separate
/// reviews found three separate omissions — first the `next` and OpenSearch
/// link templates and the timestamp format, then `ATOM_CONTENT_TYPE` and
/// `ATOM_ACQ_TYPE` (which arrive as `format!` *arguments*, so only their
/// placeholder names were in the envelope), then the `entries` join separator
/// and `EPOCH_FALLBACK`. Each fix was correct and the next omission was the
/// same bug. A registry only works while someone keeps it complete, and the
/// evidence is that nobody does.
///
/// Hashing the file removes the obligation instead of restating it: a new
/// inline `format!`, an edited content-type constant, a change to
/// `xml_escape`'s entity list — all of it moves the validator, with nothing to
/// remember.
///
/// The cost, stated plainly: **any** edit to this file invalidates every
/// cached feed once, including a comment-only or test-only edit. That is one
/// refetch per client per release that touches this module. Feeds are small
/// and releases are rare, so the trade is worth it — but it is a real cost,
/// not a free win.
///
/// Its limit, equally plainly: it covers this module and nothing else.
/// `chrono`'s formatting, or `models::Book` changing shape, remains invisible
/// to it.
const SELF_SRC: &str = include_str!("opds_feed.rs");

/// Length-delimited hash of `s` into `hasher`.
///
/// Length-delimited (rather than just hashing the bytes) so that hashing
/// two adjacent fields back-to-back can't be confused with hashing one
/// field that happens to contain the same concatenated bytes.
fn hash_len_prefixed(hasher: &mut Sha256, s: &str) {
    hasher.update((s.len() as u64).to_le_bytes());
    hasher.update(s.as_bytes());
}

/// Hashes `Option<&str>` with an explicit discriminant byte, so `None`
/// and `Some("")` — and `Some("a")` followed by `Some("b")` vs.
/// `Some("ab")` alone — cannot collide.
fn hash_opt_str(hasher: &mut Sha256, s: Option<&str>) {
    match s {
        None => hasher.update([0u8]),
        Some(v) => {
            hasher.update([1u8]);
            hash_len_prefixed(hasher, v);
        }
    }
}

/// Digest identifying a [`FeedOptions`] value, used as [`RenderedFeed`]'s
/// `etag`.
///
/// Hashes every field of `opts` — not a hand-picked subset, which would
/// drift from the real inputs as fields are added — plus this module's own
/// source text ([`SELF_SRC`]), so a change to any emitted shape invalidates
/// cached feeds with nothing to remember to bump. Hashing only the envelope
/// was not enough: the `next` link, the OpenSearch link and the timestamp
/// format are substituted *into* it, so editing one of those changed the
/// body while leaving the digest alone. A registry of template constants was
/// not enough either — three attempts at one each left something out (the
/// timestamp format, the `ATOM_*` constants, the entries join separator),
/// which is why the digest reads the file rather than a list.
///
/// The cost is that any edit to this file moves every feed's tag, including
/// a comment-only one. That is the direction to fail in, but it means a
/// caller must not expect a stable tag across a crate version bump, and no
/// test may assert a literal digest value.
///
/// Deliberately hashes the *struct*, never the rendered body: by the time
/// a body exists, `render_feed` has already substituted `now()` for
/// `updated: None`, so hashing the body would make the digest change on
/// every single request in exactly the `None` case that looks like it
/// works today and is silently broken. `updated` below is therefore
/// hashed as the `Option` itself, never the resolved timestamp.
fn feed_etag(opts: &FeedOptions<'_>) -> String {
    feed_etag_over(opts, SELF_SRC)
}

/// `feed_etag`'s body, with the rendering source as a parameter.
///
/// The parameter exists so a test can vary that input without copying the
/// field-hashing sequence — a copy would drift the moment a field is added,
/// and would not notice the regression it was written to catch. Production has
/// exactly one caller, passing [`SELF_SRC`].
fn feed_etag_over(opts: &FeedOptions<'_>, render_src: &str) -> String {
    let mut hasher = Sha256::new();

    hash_len_prefixed(&mut hasher, render_src);

    hash_len_prefixed(&mut hasher, opts.title);
    hash_len_prefixed(&mut hasher, opts.feed_id);
    hasher.update((opts.entries.len() as u64).to_le_bytes());
    for entry in opts.entries {
        hash_len_prefixed(&mut hasher, entry);
    }
    hash_len_prefixed(&mut hasher, opts.self_href);
    hasher.update([match opts.kind {
        FeedKind::Navigation => 0u8,
        FeedKind::Acquisition => 1u8,
    }]);
    hash_opt_str(&mut hasher, opts.next_href);
    hash_len_prefixed(&mut hasher, opts.prefix);
    hash_opt_str(&mut hasher, opts.opensearch_href);
    // CRITICAL: the Option, not the now()-resolved value — see doc comment.
    match opts.updated {
        None => hasher.update([0u8]),
        Some(t) => {
            hasher.update([1u8]);
            hasher.update(t.to_le_bytes());
        }
    }

    format!("{:x}", hasher.finalize())
}

/// Render a complete Atom feed page from `opts`, plus its ETag.
///
/// One call, deliberately: hashing and rendering share `opts`, so a
/// caller can never hash one `FeedOptions` value and render a different
/// one — the only failure mode here that yields a *wrong* answer (a
/// validator for content the client didn't get) rather than merely a
/// slow one.
pub fn render_feed(opts: &FeedOptions<'_>) -> RenderedFeed {
    let etag = feed_etag(opts);

    let kind_type = opts.kind.as_content_type();
    let title_esc = xml_escape(opts.title);
    let feed_id_esc = xml_escape(opts.feed_id);
    let self_href_esc = xml_escape(opts.self_href);
    let prefix_esc = xml_escape(opts.prefix);
    // Only `None` may reach the wall clock. An out-of-range `Some(t)` falls
    // back to a fixed instant instead, the way `book_to_entry` already does:
    // the digest hashes `Some(t)` and would stay stable while a `now()` body
    // changed every request, which is exactly the etag-stops-describing-the-
    // bytes failure this design exists to prevent.
    let updated = match opts.updated {
        Some(t) => chrono::DateTime::from_timestamp(t, 0)
            .map(|dt| dt.format(TIMESTAMP_FORMAT).to_string())
            .unwrap_or_else(|| EPOCH_FALLBACK.to_string()),
        None => chrono::Utc::now().format(TIMESTAMP_FORMAT).to_string(),
    };
    let next_link = opts
        .next_href
        .map(|h| {
            format!(
                next_link_template!(),
                href = xml_escape(h),
                kind_type = kind_type
            )
        })
        .unwrap_or_default();
    let opensearch_link = opts
        .opensearch_href
        .map(|h| format!(opensearch_link_template!(), href = xml_escape(h)))
        .unwrap_or_default();
    let entries_joined = opts.entries.join("\n");

    let body = format!(
        feed_envelope_template!(),
        feed_id_esc = feed_id_esc,
        title_esc = title_esc,
        updated = updated,
        self_href_esc = self_href_esc,
        kind_type = kind_type,
        prefix_esc = prefix_esc,
        ATOM_CONTENT_TYPE = ATOM_CONTENT_TYPE,
        opensearch_link = opensearch_link,
        ATOM_ACQ_TYPE = ATOM_ACQ_TYPE,
        next_link = next_link,
        entries_joined = entries_joined,
    );

    RenderedFeed { body, etag }
}

/// OpenSearch Description Document for a catalog's search facility.
///
/// OPDS 1.2 advertises search as a `rel="search"` link of type
/// `application/opensearchdescription+xml` pointing at a document like
/// this one, for third-party readers that only recognise the spec's form
/// (Carrel's own client uses the feed's inline `{searchTerms}` template
/// directly instead, for one fewer round trip). Modeled on the descriptor
/// the desktop app already serves from
/// `src-tauri/src/web_server/opds_feed.rs`, but parameterised by the
/// caller-supplied search href rather than assuming a fixed origin —
/// this module has no access to the request's authority.
///
/// **Caller obligation:** pass a href that is already correct for whoever will
/// resolve it, and prefer an absolute URL. Third-party readers resolve the
/// `template` attribute out of band, where a relative href may not resolve
/// against the feed at all; the desktop app builds this from the request's
/// authority for that reason, and search would silently disappear rather than
/// error if it did not. This function escapes the href but cannot validate
/// it — the same convention [`EntryUrls`] documents.
pub fn opensearch_descriptor(search_href: &str) -> String {
    let template = xml_escape(search_href);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>Carrel</ShortName>
  <Description>Search the Carrel library</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <Url type="{ATOM_ACQ_TYPE}" template="{template}"/>
</OpenSearchDescription>"#
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
///
/// Delegates to [`render_feed`] with today's defaults (`prefix: "/opds"`,
/// `opensearch_href: None`, `updated: None`); output is byte-for-byte
/// unchanged from before `render_feed` existed.
/// Note the cost this delegation carries: `render_feed` always computes the
/// digest, and `wrap_feed` throws it away. The digest is SHA-256 over this
/// file (~50 KB of source text) plus every entry, which measured 158.8 µs
/// of `render_feed`'s 160.6 µs on an M-series Mac in release — building the
/// body itself is 1.8 µs. So a body-only caller pays roughly 90x the body's
/// cost for a value it discards, on every call.
///
/// Left as-is deliberately. Splitting a body-only entry point back out would
/// restore exactly the hazard `render_feed` exists to remove — a caller
/// hashing one `FeedOptions` and rendering another — and making the field
/// lazy would change `RenderedFeed`'s shape, which the additive-only
/// constraint forbids. 160 µs against a feed request's DB work is a real but
/// small cost, and the safe shape is worth it. See the backlog item on these
/// handlers.
pub fn wrap_feed(
    title: &str,
    feed_id: &str,
    entries: &[String],
    self_href: &str,
    kind: FeedKind,
    next_href: Option<&str>,
) -> String {
    render_feed(&FeedOptions {
        title,
        feed_id,
        entries,
        self_href,
        kind,
        next_href,
        ..Default::default()
    })
    .body
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

    /// Extracts the text between `<updated>` and `</updated>` so
    /// timestamp-bearing output can be compared without racing the clock.
    fn extract_updated(feed: &str) -> &str {
        let start = feed.find("<updated>").expect("missing <updated>") + "<updated>".len();
        let end = feed[start..]
            .find("</updated>")
            .expect("missing </updated>")
            + start;
        &feed[start..end]
    }

    /// Blanks out the `<updated>` element's content so two feeds that were
    /// rendered from independent `now()` calls can still be compared for
    /// byte-identical *structure*.
    fn normalize_updated(feed: &str) -> String {
        let start = feed.find("<updated>").expect("missing <updated>");
        let end = feed.find("</updated>").expect("missing </updated>") + "</updated>".len();
        format!("{}<updated>X</updated>{}", &feed[..start], &feed[end..])
    }

    /// Reconstructs the pre-refactor `wrap_feed` template by hand, so the
    /// delegating implementation can be checked against it byte-for-byte
    /// rather than merely "looks right".
    fn expected_wrap_feed_body(
        title_esc: &str,
        feed_id_esc: &str,
        updated: &str,
        self_href_esc: &str,
        kind_type: &str,
        next_link: &str,
        entries_joined: &str,
    ) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:opds="http://opds-spec.org/2010/catalog">
  <id>{feed_id_esc}</id>
  <title>{title_esc}</title>
  <updated>{updated}</updated>
  <link rel="self" href="{self_href_esc}" type="{kind_type}"/>
  <link rel="start" href="/opds" type="{ATOM_CONTENT_TYPE}"/>
  <link rel="search" href="/opds/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}"/>
{next_link}
{entries_joined}
</feed>"#
        )
    }

    #[test]
    fn wrap_feed_navigation_feed_is_byte_identical_to_pre_refactor_shape() {
        let feed = wrap_feed(
            "Root",
            "urn:test:root",
            &[],
            "/opds",
            FeedKind::Navigation,
            None,
        );
        let updated = extract_updated(&feed);
        let expected = expected_wrap_feed_body(
            "Root",
            "urn:test:root",
            updated,
            "/opds",
            ATOM_CONTENT_TYPE,
            "",
            "",
        );
        assert_eq!(feed, expected);
    }

    #[test]
    fn wrap_feed_acquisition_feed_is_byte_identical_to_pre_refactor_shape() {
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
        let updated = extract_updated(&feed);
        let expected = expected_wrap_feed_body(
            "Library",
            "urn:test:lib",
            updated,
            "/opds/all",
            ATOM_ACQ_TYPE,
            "",
            &entries.join("\n"),
        );
        assert_eq!(feed, expected);
    }

    #[test]
    fn wrap_feed_with_next_href_is_byte_identical_to_pre_refactor_shape() {
        let feed = wrap_feed(
            "Page 1",
            "urn:test:lib:p1",
            &[],
            "/opds/all?page=1",
            FeedKind::Acquisition,
            Some("/opds/all?page=2&from=somewhere"),
        );
        let updated = extract_updated(&feed);
        let next_link = format!(
            r#"  <link rel="next" href="/opds/all?page=2&amp;from=somewhere" type="{ATOM_ACQ_TYPE}"/>"#
        );
        let expected = expected_wrap_feed_body(
            "Page 1",
            "urn:test:lib:p1",
            updated,
            "/opds/all?page=1",
            ATOM_ACQ_TYPE,
            &next_link,
            "",
        );
        assert_eq!(feed, expected);
    }

    #[test]
    fn wrap_feed_without_next_href_is_byte_identical_to_pre_refactor_shape() {
        let feed = wrap_feed(
            "All",
            "urn:test:all",
            &[],
            "/opds/all",
            FeedKind::Acquisition,
            None,
        );
        let updated = extract_updated(&feed);
        let expected = expected_wrap_feed_body(
            "All",
            "urn:test:all",
            updated,
            "/opds/all",
            ATOM_ACQ_TYPE,
            "",
            "",
        );
        assert_eq!(feed, expected);
    }

    #[test]
    fn render_feed_matches_wrap_feed_with_default_options() {
        let entries = vec!["<entry><id>a</id></entry>".to_string()];
        let wrapped = wrap_feed(
            "Library",
            "urn:test:lib",
            &entries,
            "/opds/all",
            FeedKind::Acquisition,
            Some("/opds/all?page=1"),
        );

        let rendered = render_feed(&FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &entries,
            self_href: "/opds/all",
            kind: FeedKind::Acquisition,
            next_href: Some("/opds/all?page=1"),
            ..Default::default()
        });

        // Both calls independently resolve `updated: None` via `now()`;
        // normalize it out rather than racing the clock between the two
        // calls.
        assert_eq!(
            normalize_updated(&rendered.body),
            normalize_updated(&wrapped)
        );
    }

    #[test]
    fn feed_etag_is_stable_across_a_real_second_boundary_when_updated_is_none() {
        // Back-to-back calls are NOT a sufficient test here: chrono formats
        // `now()` at second granularity, so two calls made within the same
        // wall-clock second would pass even if the bug (hashing the
        // *resolved* substitution timestamp instead of the `Option` itself)
        // were present. This test forces a real second to elapse between
        // the two calls so a granularity-masked bug cannot hide.
        let opts = FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &[],
            self_href: "/opds/all",
            updated: None,
            ..Default::default()
        };

        let first = render_feed(&opts).etag;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = render_feed(&opts).etag;

        assert_eq!(
            first, second,
            "etag for updated: None must not depend on now()"
        );
    }

    #[test]
    fn feed_etag_differs_by_updated_value() {
        fn opts_with_updated(updated: Option<i64>) -> FeedOptions<'static> {
            FeedOptions {
                title: "Library",
                feed_id: "urn:test:lib",
                entries: &[],
                self_href: "/opds/all",
                updated,
                ..Default::default()
            }
        }

        let none_tag = render_feed(&opts_with_updated(None)).etag;
        let some_a = render_feed(&opts_with_updated(Some(1_700_000_000))).etag;
        let some_b = render_feed(&opts_with_updated(Some(1_800_000_000))).etag;

        assert_ne!(none_tag, some_a);
        assert_ne!(none_tag, some_b);
        assert_ne!(some_a, some_b);
    }

    /// One `assert_ne!` per digest field. `next_href` had one of these from the
    /// start; the rest did not, which meant `self_href` — the field whose
    /// absence let page 1 answer 304 to page 0's validator, the defect this
    /// whole redesign exists to fix — could be deleted from `feed_etag` with
    /// the entire suite still green. So could `entries`, which is the whole
    /// point of hashing rendered bytes rather than `(id, updated_at)` pairs.
    ///
    /// Each case changes exactly one field from the same base, so a passing
    /// row means that field reached the hasher.
    #[test]
    fn feed_etag_reflects_every_field() {
        let base_entries = vec!["<entry><id>a</id></entry>".to_string()];
        let other_entries = vec!["<entry><id>b</id></entry>".to_string()];

        // A fresh base per case, mutated in one field. `FeedKind` is not
        // `Copy`, so `..base` cannot be used, and mutating a field is clearer
        // than nine full literals.
        let base = || FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &base_entries,
            self_href: "/opds/all",
            kind: FeedKind::Acquisition,
            next_href: None,
            prefix: "/opds",
            opensearch_href: None,
            updated: None,
        };

        let base_tag = feed_etag(&base());

        let mut cases: Vec<(&str, String)> = Vec::new();
        let mut o = base();
        o.title = "Other";
        cases.push(("title", feed_etag(&o)));

        let mut o = base();
        o.feed_id = "urn:test:other";
        cases.push(("feed_id", feed_etag(&o)));

        let mut o = base();
        o.entries = &other_entries;
        cases.push(("entries", feed_etag(&o)));

        let mut o = base();
        o.self_href = "/opds/all?page=1";
        cases.push(("self_href", feed_etag(&o)));

        let mut o = base();
        o.kind = FeedKind::Navigation;
        cases.push(("kind", feed_etag(&o)));

        let mut o = base();
        o.next_href = Some("/opds/all?page=1");
        cases.push(("next_href", feed_etag(&o)));

        let mut o = base();
        o.prefix = "/catalog";
        cases.push(("prefix", feed_etag(&o)));

        let mut o = base();
        o.opensearch_href = Some("/opds/opensearch.xml");
        cases.push(("opensearch_href", feed_etag(&o)));

        let mut o = base();
        o.updated = Some(1_700_000_000);
        cases.push(("updated", feed_etag(&o)));

        for (field, tag) in cases {
            assert_ne!(
                base_tag, tag,
                "changing `{field}` must change the digest — if this fails, \
                 that field is not reaching the hasher"
            );
        }
    }

    /// An `updated: Some(t)` that chrono cannot represent must still render a
    /// deterministic `<updated>`. Falling back to `now()` there would leave the
    /// digest stable (it hashes `Some(t)`) while the body changed every
    /// request — the etag would stop describing the bytes, which is the exact
    /// failure the `Option`-not-value rule exists to prevent.
    #[test]
    fn out_of_range_updated_renders_deterministically() {
        let opts = FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &[],
            self_href: "/opds/all",
            updated: Some(i64::MAX),
            ..Default::default()
        };

        let first = render_feed(&opts).body;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = render_feed(&opts).body;

        assert_eq!(
            first, second,
            "an unrepresentable Some(t) must not fall through to now()"
        );
        assert!(first.contains(&format!("<updated>{EPOCH_FALLBACK}</updated>")));
    }

    /// A representable `Some(t)` reaches the body in the same shape `now()`
    /// produces — the digest distinguishing `Some` values would be worth
    /// little if the body ignored them.
    #[test]
    fn some_updated_reaches_the_body() {
        let opts = FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &[],
            self_href: "/opds/all",
            updated: Some(1_700_000_000),
            ..Default::default()
        };
        assert!(render_feed(&opts)
            .body
            .contains("<updated>2023-11-14T22:13:20Z</updated>"));
    }

    #[test]
    fn feed_etag_reflects_next_href() {
        fn opts_with_next(next_href: Option<&str>) -> FeedOptions<'_> {
            FeedOptions {
                title: "Library",
                feed_id: "urn:test:lib",
                entries: &[],
                self_href: "/opds/all",
                next_href,
                ..Default::default()
            }
        }

        let without = render_feed(&opts_with_next(None)).etag;
        let with = render_feed(&opts_with_next(Some("/opds/all?page=1"))).etag;

        assert_ne!(
            without, with,
            "next_href must be part of the etag digest — this is the assertion \
             that fails if next_href is later dropped from the hash"
        );
    }

    /// The two Atom content types as they appear on the wire.
    ///
    /// Every other assertion in this module interpolates these constants, so
    /// expectation and actual move together and an edit to either would change
    /// what OPDS clients receive with nothing failing. This is the one place
    /// the literal is written out. OPDS clients dispatch on these strings, so
    /// changing one is a wire-format change and should be a deliberate,
    /// visible act — not a silent diff.
    #[test]
    fn atom_content_types_are_pinned_to_their_wire_values() {
        assert_eq!(
            ATOM_CONTENT_TYPE,
            "application/atom+xml;profile=opds-catalog;kind=navigation"
        );
        assert_eq!(
            ATOM_ACQ_TYPE,
            "application/atom+xml;profile=opds-catalog;kind=acquisition"
        );
    }

    /// The module's own source text reaches the digest, so any change to how a
    /// feed is rendered moves the validator without anyone maintaining a list.
    ///
    /// This replaced a registry of templates that three reviews found
    /// incomplete three separate times — see [`SELF_SRC`]. The property being
    /// pinned is one-directional and worth stating exactly: a different render
    /// source yields a different digest. That is what makes an edit to this
    /// file invalidate cached feeds.
    #[test]
    fn render_source_reaches_the_digest() {
        let opts = FeedOptions {
            title: "Library",
            feed_id: "urn:test:lib",
            entries: &[],
            self_href: "/opds/all",
            ..Default::default()
        };

        assert_eq!(
            feed_etag(&opts),
            feed_etag_over(&opts, SELF_SRC),
            "feed_etag must hash this module's source"
        );
        assert_ne!(
            feed_etag(&opts),
            feed_etag_over(&opts, "a different rendering"),
            "a change to the rendering source must move the digest"
        );

        // The content-type constants are the omission that motivated hashing
        // the file rather than a template list: they reach the body as `type=`
        // attribute values but arrive as `format!` arguments, so only their
        // placeholder names ever appeared in the envelope template. They are
        // in the source, so they are now in the digest.
        assert!(SELF_SRC.contains(ATOM_ACQ_TYPE));
        assert!(SELF_SRC.contains(ATOM_CONTENT_TYPE));
        assert!(SELF_SRC.contains(TIMESTAMP_FORMAT));
        assert!(SELF_SRC.contains(EPOCH_FALLBACK));
    }

    #[test]
    fn prefix_reaches_start_and_search_links() {
        let rendered = render_feed(&FeedOptions {
            title: "Root",
            feed_id: "urn:test:root",
            entries: &[],
            self_href: "/catalog",
            kind: FeedKind::Navigation,
            prefix: "/catalog",
            ..Default::default()
        });

        assert!(rendered
            .body
            .contains(&format!(r#"href="/catalog" type="{ATOM_CONTENT_TYPE}""#)));
        assert!(rendered.body.contains(&format!(
            r#"rel="search" href="/catalog/search?q={{searchTerms}}" type="{ATOM_ACQ_TYPE}""#
        )));
    }

    #[test]
    fn opensearch_href_some_adds_descriptor_link_and_none_omits_it() {
        let with = render_feed(&FeedOptions {
            title: "Root",
            feed_id: "urn:test:root",
            entries: &[],
            self_href: "/opds",
            opensearch_href: Some("/opds/opensearch.xml"),
            ..Default::default()
        });
        assert!(with.body.contains(
            r#"<link rel="search" href="/opds/opensearch.xml" type="application/opensearchdescription+xml"/>"#
        ));

        let without = render_feed(&FeedOptions {
            title: "Root",
            feed_id: "urn:test:root",
            entries: &[],
            self_href: "/opds",
            opensearch_href: None,
            ..Default::default()
        });
        assert!(!without.body.contains("opensearchdescription+xml"));
    }

    #[test]
    fn feed_options_default_uses_opds_prefix_and_acquisition_kind() {
        let opts = FeedOptions::default();
        assert_eq!(opts.prefix, "/opds");
        assert!(matches!(opts.kind, FeedKind::Acquisition));
    }

    #[test]
    fn opensearch_descriptor_uses_caller_supplied_href() {
        let doc = opensearch_descriptor("https://example.test/opds/search?q={searchTerms}");
        assert!(doc.contains(r#"template="https://example.test/opds/search?q={searchTerms}""#));
        assert!(doc.contains("<ShortName>Carrel</ShortName>"));
    }

    #[test]
    fn opensearch_descriptor_escapes_href() {
        let doc = opensearch_descriptor("/opds/search?q={searchTerms}&x=1");
        assert!(doc.contains("&amp;x=1"));
        assert!(!doc.contains("&x=1"));
    }
}

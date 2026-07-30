use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::{CarrelError, CarrelResult};

/// Maximum time to wait for an OPDS HTTP response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// User-Agent for OPDS catalog fetches. Wrapped in `Mozilla/5.0
/// (compatible; …)` because several legitimate public catalogs
/// (OpenEdition, Atramenta, others) reject any UA that doesn't start
/// with `Mozilla/`. The "compatible" pattern is the long-standing way
/// for non-browser clients to identify themselves while still passing
/// these filters — feedreaders like NewsBlur and Feedbin use the same
/// shape. Server logs still see "Carrel" so honest identification is
/// preserved.
const OPDS_USER_AGENT: &str = "Mozilla/5.0 (compatible; Carrel/1.4; OPDS reader)";

/// Maximum response body size (5 MB) to prevent DoS via large feeds.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

/// Validate that a URL is safe to fetch (no file://, no private IPs).
/// Test-only convenience for the strict variant; production callers go
/// through [`is_safe_url_with_trusted`] (with an empty list when no
/// trusted catalogs are configured).
#[cfg(test)]
fn is_safe_url(url: &str) -> bool {
    is_safe_url_with_trusted(url, &[])
}

/// Like [`is_safe_url`], but bypasses the private-IP / loopback block when the
/// URL's `host:port` matches an entry in `trusted`. The HTTP(S) scheme check
/// is still enforced — a trusted host cannot smuggle in `file://` or
/// `javascript:` URLs.
///
/// Used to allow user-added catalogs on private/LAN addresses (the user typed
/// the URL themselves, so SSRF protection isn't applicable there) while
/// keeping the strict check for arbitrary URLs encountered in untrusted feed
/// content.
pub fn is_safe_url_with_trusted(url: &str, trusted: &[String]) -> bool {
    // Only allow http:// and https:// schemes — the trusted list does not
    // relax this; `file://`, `javascript:`, etc. are always rejected.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return false;
    }

    // If the URL's host:port matches a trusted entry, allow it without
    // applying the private-range block.
    if !trusted.is_empty() {
        if let Some(hp) = host_port_from_url(url) {
            if trusted.iter().any(|t| t.eq_ignore_ascii_case(&hp)) {
                return true;
            }
        }
    }

    // Extract the host with the `url` crate — the SAME parser the allowlist
    // and trusted-host checks use (`host_port_from_url`). A manual string
    // split disagrees with it on userinfo tricks (`http://a@b/`), letting a
    // URL pass one check while the other sees a different host.
    let host = match url::Url::parse(url).ok().and_then(|u| {
        u.host_str().map(|h| {
            // url crate keeps IPv6 in brackets; strip them for IpAddr parsing.
            h.trim_start_matches('[').trim_end_matches(']').to_string()
        })
    }) {
        Some(h) => h,
        None => return false, // unparseable / hostless → unsafe
    };
    let host = host.as_str();

    // Block loopback and private network ranges
    if host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                if v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
                    || v4.octets()[0] == 169 && v4.octets()[1] == 254
                // link-local
                {
                    return false;
                }
            }
            std::net::IpAddr::V6(v6) => {
                if v6.is_loopback() || v6.is_unspecified() {
                    return false;
                }
            }
        }
    }
    true
}

/// Extract a normalized `host:port` representation from a URL. Used by
/// callers to build the trusted-host list for [`is_safe_url_with_trusted`].
/// Falls back to the scheme's default port (80/443) when none is specified
/// so that `http://example.com/x` and `http://example.com:80/x` match.
pub fn host_port_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let port = parsed.port_or_known_default()?;
    Some(format!("{host}:{port}"))
}

/// Upgrade a cover URL from `http://` to `https://` so it satisfies the
/// renderer's CSP — unless the host is in `trusted`, in which case the
/// upgrade is skipped (LAN/loopback servers typically don't speak TLS, so
/// upgrading would break the image). Non-`http://` URLs are returned
/// unchanged.
fn maybe_upgrade_http(url: &str, trusted: &[String]) -> String {
    if !url.starts_with("http://") {
        return url.to_string();
    }
    if let Some(hp) = host_port_from_url(url) {
        if trusted.iter().any(|t| t.eq_ignore_ascii_case(&hp)) {
            return url.to_string();
        }
    }
    url.replacen("http://", "https://", 1)
}

/// Validate a URL the user typed in "Add custom OPDS catalog". Permissive
/// about destination (private/loopback hosts are allowed because the user
/// explicitly entered them) but strict about scheme — only `http://` or
/// `https://` URLs are accepted, and the URL must parse with a host.
pub fn is_user_addable_url(url: &str) -> bool {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return false;
    }
    match url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        Some(host) => !host.is_empty(),
        None => false,
    }
}

/// A single entry from an OPDS feed (book or navigation link).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpdsEntry {
    pub id: String,
    pub title: String,
    pub author: String,
    pub summary: String,
    pub cover_url: Option<String>,
    /// Download links: Vec<(href, type, rel)>
    pub links: Vec<OpdsLink>,
    /// Navigation links (for sub-catalogs)
    pub nav_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpdsLink {
    pub href: String,
    pub mime_type: String,
    pub rel: String,
    /// Download size in bytes, parsed from the OPDS `length` attribute.
    /// `None` when the feed omits it (many feeds do).
    pub size_bytes: Option<u64>,
}

/// Parsed OPDS feed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpdsFeed {
    pub title: String,
    pub entries: Vec<OpdsEntry>,
    /// Next page link if paginated
    pub next_url: Option<String>,
    /// Search URL template (OpenSearch)
    pub search_url: Option<String>,
}

/// Build a reqwest client with timeout.
fn http_client() -> CarrelResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))
}

/// Fetch and parse an OPDS feed from a URL.
pub fn fetch_feed(url: &str) -> CarrelResult<OpdsFeed> {
    fetch_feed_with_trusted(url, &[])
}

/// Like [`fetch_feed`], but allows URLs whose `host:port` matches a trusted
/// entry — lets user-added LAN catalogs work without disabling the SSRF
/// guard for arbitrary feed-derived URLs.
pub fn fetch_feed_with_trusted(url: &str, trusted: &[String]) -> CarrelResult<OpdsFeed> {
    if !is_safe_url_with_trusted(url, trusted) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    let client = http_client()?;
    let response = client
        .get(url)
        .header("User-Agent", OPDS_USER_AGENT)
        .send()
        .map_err(|e| CarrelError::network(format!("HTTP error: {e}")))?;
    if !response.status().is_success() {
        return Err(CarrelError::network(format!("HTTP {}", response.status())));
    }
    let bytes = response
        .bytes()
        .map_err(|e| CarrelError::network(format!("Read error: {e}")))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(CarrelError::invalid("Response too large (limit: 5 MB)."));
    }
    let xml = String::from_utf8_lossy(&bytes).to_string();
    parse_feed_with_trusted(&xml, url, trusted)
}

/// Parse OPDS/Atom XML into structured data.
/// Test-only convenience wrapper; production callers route through
/// [`parse_feed_with_trusted`] via [`fetch_feed_with_trusted`].
#[cfg(test)]
fn parse_feed(xml: &str, base_url: &str) -> CarrelResult<OpdsFeed> {
    parse_feed_with_trusted(xml, base_url, &[])
}

/// Parse OPDS/Atom XML; skip the `http://` → `https://` cover upgrade when
/// the cover URL targets a trusted host (LAN servers don't speak TLS).
fn parse_feed_with_trusted(
    xml: &str,
    base_url: &str,
    trusted: &[String],
) -> CarrelResult<OpdsFeed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut feed_title = String::new();
    let mut entries: Vec<OpdsEntry> = Vec::new();
    let mut next_url: Option<String> = None;
    let mut search_url: Option<String> = None;

    // Current entry being parsed
    let mut in_entry = false;
    let mut entry_id = String::new();
    let mut entry_title = String::new();
    let mut entry_author = String::new();
    let mut entry_summary = String::new();
    let mut entry_cover: Option<String> = None;
    let mut entry_links: Vec<OpdsLink> = Vec::new();
    let mut entry_nav: Option<String> = None;

    // Track which element we're inside
    let mut current_tag = String::new();
    let mut in_author = false;
    let mut in_feed_title = false;

    let parsed_base = url::Url::parse(base_url).ok();
    let resolve = |href: &str| -> String {
        // Reject non-HTTP schemes outright (file://, javascript:, data:, etc.)
        if !href.is_empty()
            && !href.starts_with("http://")
            && !href.starts_with("https://")
            && !href.starts_with('/')
            && href.contains(':')
        {
            return String::new(); // blocked
        }
        if href.starts_with("http://") || href.starts_with("https://") {
            return href.to_string();
        }
        // RFC-compliant URL resolution via the url crate
        if let Some(ref base) = parsed_base {
            if let Ok(resolved) = base.join(href) {
                return resolved.to_string();
            }
        }
        String::new()
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let ln = e.local_name();
                let local = std::str::from_utf8(ln.as_ref()).unwrap_or("");
                match local {
                    "entry" => {
                        in_entry = true;
                        entry_id.clear();
                        entry_title.clear();
                        entry_author.clear();
                        entry_summary.clear();
                        entry_cover = None;
                        entry_links.clear();
                        entry_nav = None;
                    }
                    "title" => {
                        if !in_entry && feed_title.is_empty() {
                            in_feed_title = true;
                        }
                        current_tag = "title".to_string();
                    }
                    "id" => {
                        current_tag = "id".to_string();
                    }
                    "name" if in_author => {
                        current_tag = "author_name".to_string();
                    }
                    "author" => {
                        in_author = true;
                    }
                    "summary" | "content" => {
                        current_tag = "summary".to_string();
                    }
                    // media:thumbnail (used by Standard Ebooks Atom feeds)
                    "thumbnail" if in_entry && entry_cover.is_none() => {
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            if key == "url" {
                                let url = attr.unescape_value().unwrap_or_default().to_string();
                                let url = resolve(&url);
                                entry_cover = Some(maybe_upgrade_http(&url, trusted));
                            }
                        }
                    }
                    "link" => {
                        let mut href = String::new();
                        let mut rel = String::new();
                        let mut mime = String::new();
                        let mut size_bytes: Option<u64> = None;
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            match key {
                                "href" => href = resolve(&val),
                                "rel" => rel = val,
                                "type" => mime = val,
                                "length" => size_bytes = val.parse::<u64>().ok(),
                                _ => {}
                            }
                        }
                        if !href.is_empty() {
                            // Feed-level links
                            if !in_entry {
                                if rel == "next" {
                                    next_url = Some(href.clone());
                                } else if rel.contains("search") || mime.contains("opensearch") {
                                    search_url = Some(href.clone());
                                }
                            }
                            // Entry-level links
                            if in_entry {
                                // Cover/thumbnail
                                if rel.contains("thumbnail")
                                    || rel.contains("image")
                                    || (mime.starts_with("image/") && rel != "alternate")
                                {
                                    entry_cover = Some(maybe_upgrade_http(&href, trusted));
                                }
                                // Navigation (sub-catalog)
                                if mime.contains("atom+xml")
                                    || mime.contains("opds-catalog")
                                    || rel.contains("subsection")
                                    || rel.contains("alternate") && mime.contains("atom")
                                {
                                    entry_nav = Some(href.clone());
                                }
                                // Acquisition (download)
                                if rel.contains("acquisition")
                                    || rel == "enclosure"
                                    || rel.is_empty()
                                        && (mime.contains("epub")
                                            || mime.contains("pdf")
                                            || mime.contains("zip")
                                            || mime.contains("octet"))
                                {
                                    entry_links.push(OpdsLink {
                                        href,
                                        mime_type: mime,
                                        rel,
                                        size_bytes,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if in_feed_title && !in_entry {
                    feed_title = text.clone();
                    in_feed_title = false;
                }
                if in_entry {
                    match current_tag.as_str() {
                        "title" => entry_title.push_str(&text),
                        "id" => entry_id.push_str(&text),
                        "author_name" => entry_author.push_str(&text),
                        "summary" => entry_summary.push_str(&text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let ln = e.local_name();
                let local = std::str::from_utf8(ln.as_ref()).unwrap_or("");
                match local {
                    "entry" => {
                        in_entry = false;
                        entries.push(OpdsEntry {
                            id: entry_id.clone(),
                            title: entry_title.clone(),
                            author: entry_author.clone(),
                            summary: entry_summary.clone(),
                            cover_url: entry_cover.clone(),
                            links: entry_links.clone(),
                            nav_url: entry_nav.clone(),
                        });
                    }
                    "author" => {
                        in_author = false;
                    }
                    "title" | "id" | "name" | "summary" | "content" => {
                        current_tag.clear();
                        in_feed_title = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(CarrelError::invalid(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    // Resolve OpenSearch description URLs to direct search templates
    let resolved_search = search_url.and_then(|u| resolve_search_url(&u));

    Ok(OpdsFeed {
        title: feed_title,
        entries,
        next_url,
        search_url: resolved_search,
    })
}

/// Resolve a search URL — if it's an OpenSearch description XML, fetch it and
/// extract the Atom/OPDS template URL. Otherwise return it as-is.
pub fn resolve_search_url(url: &str) -> Option<String> {
    resolve_search_url_with_trusted(url, &[])
}

/// Like [`resolve_search_url`], but allows URLs whose `host:port` matches a
/// trusted entry.
pub fn resolve_search_url_with_trusted(url: &str, trusted: &[String]) -> Option<String> {
    // If it already contains {searchTerms}, it's a direct template
    if url.contains("{searchTerms}") {
        return Some(url.to_string());
    }
    // Try fetching as OpenSearch description
    if !is_safe_url_with_trusted(url, trusted) {
        return None;
    }
    let client = http_client().ok()?;
    let response = client
        .get(url)
        .header("User-Agent", OPDS_USER_AGENT)
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let xml = response.text().ok()?;

    // Parse and find the Atom/OPDS Url template
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let ln = e.local_name();
                let local = std::str::from_utf8(ln.as_ref()).unwrap_or("");
                if local.eq_ignore_ascii_case("url") {
                    let mut template = String::new();
                    let mut url_type = String::new();
                    for attr in e.attributes().flatten() {
                        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                        let val = attr.unescape_value().unwrap_or_default().to_string();
                        match key {
                            "template" => template = val,
                            "type" => url_type = val,
                            _ => {}
                        }
                    }
                    // Prefer atom+xml / opds-catalog type
                    if !template.is_empty()
                        && (url_type.contains("atom") || url_type.contains("opds"))
                    {
                        return Some(template);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Percent-encode a string for use in URLs.
pub fn url_encode(s: &str) -> String {
    s.replace('%', "%25")
        .replace(' ', "%20")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('#', "%23")
        .replace('+', "%2B")
}

/// Download a file from a URL to a local path.
pub fn download_file(url: &str, dest: &str) -> CarrelResult<()> {
    download_file_with_trusted(url, dest, &[])
}

/// Like [`download_file`], but allows URLs whose `host:port` matches a
/// trusted entry — required for downloading from user-added LAN catalogs.
pub fn download_file_with_trusted(url: &str, dest: &str, trusted: &[String]) -> CarrelResult<()> {
    if !is_safe_url_with_trusted(url, trusted) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120)) // longer timeout for file downloads
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))?;
    let response = client
        .get(url)
        .send()
        .map_err(|e| CarrelError::network(format!("Download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CarrelError::network(format!("HTTP {}", response.status())));
    }
    let bytes = response
        .bytes()
        .map_err(|e| CarrelError::network(format!("Read error: {e}")))?;
    std::fs::write(dest, &bytes).map_err(|e| CarrelError::io(format!("Write error: {e}")))?;
    Ok(())
}

/// Redirect policy that re-applies the SSRF guard on EVERY hop (no trusted-host
/// relaxation) and caps the hop count at 5. Shared by the plugin `import:books`
/// download and the dictionary artifact download so a public URL can't 302 to a
/// private/loopback target.
pub fn ssrf_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        if is_safe_url_with_trusted(attempt.url().as_str(), &[]) {
            attempt.follow()
        } else {
            attempt.error("redirect to a blocked (private/non-HTTP) URL")
        }
    })
}

/// Download a file with the SSRF guard re-applied on EVERY redirect hop, not
/// just the initial URL. Used by the plugin `import:books` path so a public
/// URL can't 302 to a private/loopback target (no trusted-host relaxation).
pub fn download_file_ssrf_guarded(url: &str, dest: &str) -> CarrelResult<()> {
    if !is_safe_url_with_trusted(url, &[]) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(ssrf_redirect_policy())
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))?;
    let response = client
        .get(url)
        .send()
        .map_err(|e| CarrelError::network(format!("Download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CarrelError::network(format!("HTTP {}", response.status())));
    }
    let bytes = response
        .bytes()
        .map_err(|e| CarrelError::network(format!("Read error: {e}")))?;
    std::fs::write(dest, &bytes).map_err(|e| CarrelError::io(format!("Write error: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_feed_basic_entry() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test Catalog</title>
          <entry>
            <id>urn:uuid:123</id>
            <title>My Book</title>
            <author><name>Jane Doe</name></author>
            <summary>A great book</summary>
            <link href="/download/book.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert_eq!(feed.title, "Test Catalog");
        assert_eq!(feed.entries.len(), 1);

        let entry = &feed.entries[0];
        assert_eq!(entry.id, "urn:uuid:123");
        assert_eq!(entry.title, "My Book");
        assert_eq!(entry.author, "Jane Doe");
        assert_eq!(entry.summary, "A great book");
        assert_eq!(entry.links.len(), 1);
        assert_eq!(
            entry.links[0].href,
            "https://example.com/download/book.epub"
        );
        assert_eq!(entry.links[0].mime_type, "application/epub+zip");
        // No `length` attribute → size_bytes is None.
        assert_eq!(entry.links[0].size_bytes, None);
    }

    #[test]
    fn parse_feed_acquisition_length_populates_size_bytes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test Catalog</title>
          <entry>
            <id>urn:uuid:size</id>
            <title>Sized Book</title>
            <link href="/download/book.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition" length="12345"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert_eq!(feed.entries.len(), 1);
        assert_eq!(feed.entries[0].links.len(), 1);
        assert_eq!(feed.entries[0].links[0].size_bytes, Some(12345));
    }

    #[test]
    fn parse_feed_relative_url_resolution() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test</title>
          <entry>
            <id>1</id>
            <title>Book</title>
            <link href="book.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/catalog/root.xml").unwrap();
        // Relative path should resolve against base directory
        assert_eq!(
            feed.entries[0].links[0].href,
            "https://example.com/catalog/book.epub"
        );
    }

    #[test]
    fn parse_feed_absolute_path_resolution() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test</title>
          <entry>
            <id>1</id>
            <title>Book</title>
            <link href="/files/book.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/catalog/root.xml").unwrap();
        // Absolute path should use scheme+host only
        assert_eq!(
            feed.entries[0].links[0].href,
            "https://example.com/files/book.epub"
        );
    }

    #[test]
    fn parse_feed_full_url_unchanged() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test</title>
          <entry>
            <id>1</id>
            <title>Book</title>
            <link href="https://cdn.example.com/book.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert_eq!(
            feed.entries[0].links[0].href,
            "https://cdn.example.com/book.epub"
        );
    }

    #[test]
    fn parse_feed_cover_http_upgraded_to_https() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Test</title>
          <entry>
            <id>1</id>
            <title>Book</title>
            <link href="http://covers.example.com/cover.jpg" type="image/jpeg" rel="http://opds-spec.org/image/thumbnail"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        // Cover URLs should be upgraded from http to https
        assert_eq!(
            feed.entries[0].cover_url.as_deref(),
            Some("https://covers.example.com/cover.jpg")
        );
    }

    #[test]
    fn parse_feed_navigation_links() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Root</title>
          <entry>
            <id>1</id>
            <title>Science Fiction</title>
            <link href="/catalog/scifi" type="application/atom+xml" rel="subsection"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert!(feed.entries[0].nav_url.is_some());
        assert_eq!(
            feed.entries[0].nav_url.as_deref(),
            Some("https://example.com/catalog/scifi")
        );
    }

    #[test]
    fn parse_feed_next_page_and_search() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Catalog</title>
          <link href="/opds?page=2" rel="next" type="application/atom+xml"/>
          <link href="/search?q={searchTerms}" rel="search" type="application/opensearchdescription+xml"/>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert_eq!(
            feed.next_url.as_deref(),
            Some("https://example.com/opds?page=2")
        );
        assert_eq!(
            feed.search_url.as_deref(),
            Some("https://example.com/search?q={searchTerms}")
        );
    }

    #[test]
    fn parse_feed_empty_feed() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Empty</title>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com").unwrap();
        assert_eq!(feed.title, "Empty");
        assert!(feed.entries.is_empty());
        assert!(feed.next_url.is_none());
        assert!(feed.search_url.is_none());
    }

    #[test]
    fn parse_feed_invalid_xml() {
        let xml = "not xml at all <<<<";
        assert!(parse_feed(xml, "https://example.com").is_err());
    }

    #[test]
    fn parse_feed_multiple_entries() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Books</title>
          <entry>
            <id>1</id><title>Book One</title>
          </entry>
          <entry>
            <id>2</id><title>Book Two</title>
          </entry>
          <entry>
            <id>3</id><title>Book Three</title>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com").unwrap();
        assert_eq!(feed.entries.len(), 3);
        assert_eq!(feed.entries[0].title, "Book One");
        assert_eq!(feed.entries[2].title, "Book Three");
    }

    #[test]
    fn is_safe_url_blocks_private_ips_and_loopback() {
        assert!(!is_safe_url("http://192.168.0.12:7788/opds"));
        assert!(!is_safe_url("http://10.0.0.1/"));
        assert!(!is_safe_url("http://172.16.5.5/"));
        assert!(!is_safe_url("http://127.0.0.1/"));
        assert!(!is_safe_url("http://localhost/"));
        assert!(!is_safe_url("http://169.254.169.254/"));
    }

    #[test]
    fn is_safe_url_allows_public_hosts() {
        assert!(is_safe_url("https://example.com/opds"));
        assert!(is_safe_url("http://standardebooks.org/feeds"));
        assert!(is_safe_url("https://m.gutenberg.org/ebooks.opds/"));
    }

    #[test]
    fn is_safe_url_blocks_non_http_schemes() {
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("ftp://example.com/"));
        assert!(!is_safe_url("data:text/html,<h1>hi"));
    }

    #[test]
    fn trusted_host_bypasses_private_ip_block() {
        let trusted = vec!["192.168.0.12:7788".to_string()];
        assert!(is_safe_url_with_trusted(
            "http://192.168.0.12:7788/opds",
            &trusted
        ));
        assert!(is_safe_url_with_trusted(
            "http://192.168.0.12:7788/opds/all",
            &trusted
        ));
        assert!(is_safe_url_with_trusted(
            "http://192.168.0.12:7788/api/cover/abc.jpg",
            &trusted
        ));
    }

    #[test]
    fn trusted_host_with_different_port_still_blocked() {
        let trusted = vec!["192.168.0.12:7788".to_string()];
        // Different port on same IP — could be a different service
        assert!(!is_safe_url_with_trusted(
            "http://192.168.0.12:8080/opds",
            &trusted
        ));
        // Different IP on the LAN
        assert!(!is_safe_url_with_trusted(
            "http://192.168.0.13:7788/opds",
            &trusted
        ));
        // No port on the URL means default 80, which doesn't match :7788
        assert!(!is_safe_url_with_trusted(
            "http://192.168.0.12/opds",
            &trusted
        ));
    }

    #[test]
    fn trusted_host_does_not_relax_scheme_check() {
        let trusted = vec!["192.168.0.12:7788".to_string()];
        assert!(!is_safe_url_with_trusted("file:///etc/passwd", &trusted));
        assert!(!is_safe_url_with_trusted("javascript:alert(1)", &trusted));
        assert!(!is_safe_url_with_trusted(
            "ftp://192.168.0.12:7788/x",
            &trusted
        ));
    }

    #[test]
    fn trusted_list_match_is_case_insensitive_for_host() {
        let trusted = vec!["MyServer.Local:7788".to_string()];
        // Host comparison should be case-insensitive (DNS is case-insensitive).
        assert!(is_safe_url_with_trusted(
            "http://myserver.local:7788/opds",
            &trusted
        ));
    }

    #[test]
    fn host_port_from_url_uses_default_ports() {
        assert_eq!(
            host_port_from_url("http://example.com/opds"),
            Some("example.com:80".to_string())
        );
        assert_eq!(
            host_port_from_url("https://example.com/opds"),
            Some("example.com:443".to_string())
        );
        assert_eq!(
            host_port_from_url("http://192.168.0.12:7788/opds"),
            Some("192.168.0.12:7788".to_string())
        );
        assert_eq!(host_port_from_url("not a url"), None);
        assert_eq!(host_port_from_url(""), None);
    }

    #[test]
    fn is_user_addable_url_accepts_lan_hosts() {
        // The whole point: users typing a LAN URL must be accepted.
        assert!(is_user_addable_url("http://192.168.0.12:7788/opds"));
        assert!(is_user_addable_url("http://10.0.0.1/opds"));
        assert!(is_user_addable_url("http://localhost:7788/opds"));
        assert!(is_user_addable_url("https://example.com/opds"));
    }

    #[test]
    fn is_user_addable_url_rejects_non_http_and_malformed() {
        assert!(!is_user_addable_url("file:///etc/passwd"));
        assert!(!is_user_addable_url("javascript:alert(1)"));
        assert!(!is_user_addable_url("ftp://example.com/"));
        assert!(!is_user_addable_url("not a url"));
        assert!(!is_user_addable_url(""));
        // Empty authority is rejected by the url crate's parser
        assert!(!is_user_addable_url("http://"));
    }

    #[test]
    fn maybe_upgrade_http_keeps_trusted_lan_url_as_http() {
        let trusted = vec!["192.168.0.12:7788".to_string()];
        // Trusted LAN host: keep http so the LAN server (no TLS) can serve covers.
        assert_eq!(
            maybe_upgrade_http("http://192.168.0.12:7788/api/cover/abc.jpg", &trusted),
            "http://192.168.0.12:7788/api/cover/abc.jpg"
        );
    }

    #[test]
    fn maybe_upgrade_http_upgrades_untrusted_public_url() {
        let trusted = vec!["192.168.0.12:7788".to_string()];
        assert_eq!(
            maybe_upgrade_http("http://covers.example.com/x.jpg", &trusted),
            "https://covers.example.com/x.jpg"
        );
    }

    #[test]
    fn maybe_upgrade_http_passthrough_for_https_and_others() {
        let trusted: Vec<String> = vec![];
        assert_eq!(
            maybe_upgrade_http("https://x.com/y.jpg", &trusted),
            "https://x.com/y.jpg"
        );
        assert_eq!(maybe_upgrade_http("", &trusted), "");
    }

    #[test]
    fn parse_feed_keeps_http_cover_for_trusted_host() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>LAN</title>
          <entry>
            <id>1</id>
            <title>Book</title>
            <link href="/api/books/123/cover" type="image/jpeg" rel="http://opds-spec.org/image"/>
          </entry>
        </feed>"#;
        let trusted = vec!["192.168.0.12:7788".to_string()];
        let feed =
            parse_feed_with_trusted(xml, "http://192.168.0.12:7788/opds/all", &trusted).unwrap();
        // Trusted LAN host — http preserved.
        assert_eq!(
            feed.entries[0].cover_url.as_deref(),
            Some("http://192.168.0.12:7788/api/books/123/cover")
        );
    }

    #[test]
    fn fetch_feed_returns_blocked_error_for_private_url() {
        let err = fetch_feed("http://192.168.0.12:7788/opds").unwrap_err();
        assert!(
            err.to_string().contains("URL blocked"),
            "expected blocked error, got: {err}"
        );
    }

    #[test]
    fn parse_feed_enclosure_links_treated_as_acquisition() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Standard Ebooks</title>
          <entry>
            <id>1</id>
            <title>Jenny</title>
            <link href="https://example.com/jenny.epub" rel="enclosure" type="application/epub+zip"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com").unwrap();
        assert_eq!(feed.entries[0].links.len(), 1);
        assert_eq!(
            feed.entries[0].links[0].href,
            "https://example.com/jenny.epub"
        );
        assert_eq!(feed.entries[0].links[0].mime_type, "application/epub+zip");
    }
}

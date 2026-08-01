use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::{CarrelError, CarrelResult};

/// Maximum time to wait for an OPDS HTTP response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Longer timeout for file downloads than for feed fetches.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

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

/// Canonical origin for credential identity: `scheme://host[:port]`, scheme and
/// host lowercased, port omitted when it is the scheme default. `None` for
/// non-http(s) or hostless URLs.
///
/// Deliberately distinct from [`host_port_from_url`], which ignores the scheme
/// and keys the SSRF trusted list: a credential stored for `https://h:443` must
/// NOT be sent to `http://h:443`, so credential identity has to carry the
/// scheme. reqwest does not help here — it scrubs `Authorization` across a
/// host/port change on redirect but not across a scheme downgrade.
pub fn origin_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    match parsed.port() {
        Some(p) if p != default_port => Some(format!("{scheme}://{host}:{p}")),
        _ => Some(format!("{scheme}://{host}")),
    }
}

/// True for `localhost`, `*.localhost`, and loopback IPs.
///
/// Used only to decide whether a cleartext credential needs an explicit
/// acknowledgement from the user — NOT a general "is this address private"
/// test. The SSRF guard's IPv6 branch below blocks only loopback and
/// unspecified addresses (neither unique-local nor link-local), so a
/// `is_private_host` helper claiming those ranges would not match the code it
/// was extracted from. Restricting the silent exception to loopback avoids the
/// mismatch: every other cleartext credential goes through the acknowledgement.
pub fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") || bare.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
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

/// Which catalog an OPDS operation is following, and that catalog's origin.
///
/// Deliberately independent of whether a secret exists or could be read: a
/// public catalog and a catalog whose keychain entry is unreadable both still
/// have valid provenance, and the origin is needed for the cross-origin checks
/// either way. Folding the origin into the credential would silently disable
/// those checks whenever no secret happened to be loaded.
pub struct OpdsProvenance {
    /// Catalog URL exactly as configured, used to stamp returned data.
    pub catalog_url: String,
    /// `origin_from_url(catalog_url)`, resolved once by the caller.
    pub origin: String,
}

/// A credential for exactly one OPDS catalog.
///
/// Derives nothing on purpose — no `Debug`, `Display`, `Serialize` or `Clone`.
/// A derived `Debug` would print the password into any log line or panic
/// message that formatted the enclosing context.
pub enum OpdsCredential {
    Basic { username: String, password: String },
    Bearer(String),
}

/// Per-request context for OPDS network calls.
///
/// Replaces the bare `trusted: &[String]` parameter the `*_with_trusted`
/// functions used to take, so the credential rules travel with the request
/// rather than being re-derived at each site.
#[derive(Default)]
pub struct OpdsContext {
    /// `host:port` entries that bypass the private-IP/loopback SSRF guard.
    pub trusted: Vec<String>,
    /// The catalog this operation follows. `None` when provenance was never
    /// admitted (no catalog named, or one that is not configured) or was
    /// dropped because the request left the catalog's origin.
    pub provenance: Option<OpdsProvenance>,
    /// Credential for `provenance`'s catalog, when one is stored and readable.
    pub cred: Option<OpdsCredential>,
}

/// Build a reqwest client with timeout.
fn http_client() -> CarrelResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))
}

/// Build a client with an explicit timeout and redirect policy. Used for the
/// authenticated variants; [`http_client`] remains the unauthenticated feed
/// default so public catalogs behave exactly as before.
fn build_client(
    timeout: Duration,
    policy: reqwest::redirect::Policy,
) -> CarrelResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(policy)
        .build()
        .map_err(|e| CarrelError::network(format!("HTTP client error: {e}")))
}

/// The credential to use for `url`: `Some` only when the context has both
/// provenance and a credential, and `url`'s origin equals the provenance
/// origin.
///
/// This is the whole attachment rule. A feed can name any URL it likes; unless
/// that URL is on the origin of the catalog the operation started from, it gets
/// no credential.
fn credential_for_url<'a>(url: &str, ctx: &'a OpdsContext) -> Option<&'a OpdsCredential> {
    let prov = ctx.provenance.as_ref()?;
    let cred = ctx.cred.as_ref()?;
    (origin_from_url(url)? == prov.origin).then_some(cred)
}

/// Attach the credential registered for `url`, if any.
fn apply_auth(
    rb: reqwest::blocking::RequestBuilder,
    url: &str,
    ctx: &OpdsContext,
) -> reqwest::blocking::RequestBuilder {
    match credential_for_url(url, ctx) {
        Some(OpdsCredential::Basic { username, password }) => {
            rb.basic_auth(username, Some(password))
        }
        Some(OpdsCredential::Bearer(token)) => rb.bearer_auth(token),
        None => rb,
    }
}

/// Follow up to 5 redirects, but only within `origin`.
///
/// Required whenever an `Authorization` header is attached: reqwest scrubs that
/// header when a redirect changes host or port, but **not** when it changes
/// scheme (`remove_sensitive_headers`, reqwest-0.12 `redirect.rs:239-241`), so
/// an HTTPS→HTTP hop on one host would carry the credential out in cleartext.
/// An authenticated request should not silently continue to another origin in
/// any case.
pub fn same_origin_redirect_policy(origin: String) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        match origin_from_url(attempt.url().as_str()) {
            Some(next) if next == origin => attempt.follow(),
            _ => attempt.error("authenticated request redirected to another origin"),
        }
    })
}

/// Map 401/403 to a distinct auth-required error; any other status yields
/// `None` and falls through to the caller's generic handling.
///
/// The message carries the exact `OPDS auth required` substring that
/// `src/lib/errors.ts`'s `MESSAGE_KEYS` matches on, so the frontend can tell an
/// auth failure from an unreachable host and offer a sign-in prompt. Without
/// this the status became `CarrelError::network("HTTP 401 …")` and reached the
/// user as "Could not connect to the server."
///
/// 403 is treated like 401 because OPDS servers return it for a missing or
/// wrong credential at least as often, and a wrong guess costs one dismissible
/// prompt.
fn auth_error_for(status: reqwest::StatusCode) -> Option<CarrelError> {
    matches!(status.as_u16(), 401 | 403)
        .then(|| CarrelError::permission(format!("OPDS auth required: HTTP {status}")))
}

/// Pick the client for a request that may carry a credential: the strict
/// same-origin policy when one is attached, otherwise the caller's default.
fn client_for(
    url: &str,
    ctx: &OpdsContext,
    timeout: Duration,
    unauthenticated: impl FnOnce() -> CarrelResult<reqwest::blocking::Client>,
) -> CarrelResult<reqwest::blocking::Client> {
    match (credential_for_url(url, ctx), origin_from_url(url)) {
        (Some(_), Some(origin)) => build_client(timeout, same_origin_redirect_policy(origin)),
        _ => unauthenticated(),
    }
}

/// Fetch and parse an OPDS feed from a URL.
pub fn fetch_feed(url: &str) -> CarrelResult<OpdsFeed> {
    fetch_feed_with_context(url, &OpdsContext::default())
}

/// Like [`fetch_feed`], but carries an [`OpdsContext`]: URLs whose `host:port`
/// matches a trusted entry are allowed (so user-added LAN catalogs work without
/// disabling the SSRF guard for arbitrary feed-derived URLs), and the context's
/// credential is attached when the request stays on its catalog's origin.
pub fn fetch_feed_with_context(url: &str, ctx: &OpdsContext) -> CarrelResult<OpdsFeed> {
    if !is_safe_url_with_trusted(url, &ctx.trusted) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    // Select the credential first: the redirect policy depends on whether one
    // is attached, and a client's policy is fixed at build time.
    let client = client_for(url, ctx, HTTP_TIMEOUT, http_client)?;
    let response = apply_auth(
        client.get(url).header("User-Agent", OPDS_USER_AGENT),
        url,
        ctx,
    )
    .send()
    .map_err(|e| CarrelError::network(format!("HTTP error: {e}")))?;
    if let Some(err) = auth_error_for(response.status()) {
        return Err(err);
    }
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
    parse_feed_with_context(&xml, url, ctx)
}

/// Parse OPDS/Atom XML into structured data.
/// Test-only convenience wrapper; production callers route through
/// [`parse_feed_with_context`] via [`fetch_feed_with_context`].
#[cfg(test)]
fn parse_feed(xml: &str, base_url: &str) -> CarrelResult<OpdsFeed> {
    parse_feed_with_context(xml, base_url, &OpdsContext::default())
}

/// Parse OPDS/Atom XML; skip the `http://` → `https://` cover upgrade when
/// the cover URL targets a trusted host (LAN servers don't speak TLS).
fn parse_feed_with_context(xml: &str, base_url: &str, ctx: &OpdsContext) -> CarrelResult<OpdsFeed> {
    let trusted = &ctx.trusted;
    // Deliberately *not* `trim_text(true)`: that trims every text event
    // individually, and quick-xml emits an entity reference as its own event,
    // so `Fish &amp; Chips` would arrive as three events and come back out as
    // `Fish&Chips`. Whitespace is trimmed per completed character-data run
    // below instead, which is what the reader-level option was standing in for.
    let mut reader = Reader::from_str(xml);

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
    // Adjacent `Text` and `GeneralRef` events form one character-data run;
    // collect them untrimmed and route the trimmed result when the run ends.
    let mut run = String::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        if !matches!(event, Ok(Event::Text(_)) | Ok(Event::GeneralRef(_))) {
            let text = run.trim();
            if !text.is_empty() {
                if in_feed_title && !in_entry {
                    feed_title = text.to_string();
                    in_feed_title = false;
                }
                if in_entry {
                    match current_tag.as_str() {
                        "title" => entry_title.push_str(text),
                        "id" => entry_id.push_str(text),
                        "author_name" => entry_author.push_str(text),
                        "summary" => entry_summary.push_str(text),
                        _ => {}
                    }
                }
            }
            run.clear();
        }
        match event {
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
                                let url = attr
                                    .normalized_value(XmlVersion::Implicit1_0)
                                    .unwrap_or_default()
                                    .to_string();
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
                            let val = attr
                                .normalized_value(XmlVersion::Implicit1_0)
                                .unwrap_or_default()
                                .to_string();
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
                run.push_str(&e.decode().unwrap_or_default());
            }
            Ok(Event::GeneralRef(ref e)) => {
                run.push_str(&crate::epub::decode_general_ref(e).unwrap_or_default());
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
    resolve_search_url_with_context(url, &OpdsContext::default())
}

/// Like [`resolve_search_url`], but carries an [`OpdsContext`] so trusted
/// `host:port` entries are allowed and credentials can be attached.
pub fn resolve_search_url_with_context(url: &str, ctx: &OpdsContext) -> Option<String> {
    // If it already contains {searchTerms}, it's a direct template
    if url.contains("{searchTerms}") {
        return Some(url.to_string());
    }
    // Try fetching as OpenSearch description
    if !is_safe_url_with_trusted(url, &ctx.trusted) {
        return None;
    }
    // This function returns Option, so a Result cannot be propagated with `?`.
    let client = client_for(url, ctx, HTTP_TIMEOUT, http_client).ok()?;
    let response = apply_auth(
        client.get(url).header("User-Agent", OPDS_USER_AGENT),
        url,
        ctx,
    )
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
                        let val = attr
                            .normalized_value(XmlVersion::Implicit1_0)
                            .unwrap_or_default()
                            .to_string();
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
    download_file_with_context(url, dest, &OpdsContext::default())
}

/// Like [`download_file`], but carries an [`OpdsContext`]: trusted `host:port`
/// entries are allowed (required for user-added LAN catalogs) and the context's
/// credential is attached when the request stays on its catalog's origin.
pub fn download_file_with_context(url: &str, dest: &str, ctx: &OpdsContext) -> CarrelResult<()> {
    if !is_safe_url_with_trusted(url, &ctx.trusted) {
        return Err(CarrelError::invalid(
            "URL blocked: only public HTTP/HTTPS URLs are allowed.",
        ));
    }
    // Downloads keep their longer timeout and reqwest's default redirect policy
    // when unauthenticated; an authenticated download gets the same timeout with
    // the strict same-origin policy.
    let client = client_for(url, ctx, DOWNLOAD_TIMEOUT, || {
        build_client(DOWNLOAD_TIMEOUT, reqwest::redirect::Policy::default())
    })?;
    let response = apply_auth(client.get(url), url, ctx)
        .send()
        .map_err(|e| CarrelError::network(format!("Download failed: {e}")))?;
    if let Some(err) = auth_error_for(response.status()) {
        return Err(err);
    }
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
    // Deliberately no `auth_error_for` here: this is the plugin / dictionary
    // download path, which never carries a catalog credential, so a 401 from it
    // is not something a sign-in prompt could fix. Its error mapping is
    // unchanged.
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

    /// Character and general entity references have to survive parsing into the
    /// decoded text, in both element content and attribute values. quick-xml
    /// reports them as a separate event from the surrounding text, so a reader
    /// loop that only accumulates text events silently drops them.
    #[test]
    fn parse_feed_decodes_entities_in_text_and_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>Fish &amp; Chips</title>
          <entry>
            <id>urn:uuid:1</id>
            <title>Alice &amp; Bob &#38; Carol</title>
            <author><name>R&#xE9;my</name></author>
            <summary>Costs &lt; 5 &amp; &gt; 1</summary>
            <link href="/d/a&amp;b.epub" type="application/epub+zip" rel="http://opds-spec.org/acquisition"/>
          </entry>
        </feed>"#;

        let feed = parse_feed(xml, "https://example.com/opds").unwrap();
        assert_eq!(feed.title, "Fish & Chips");

        let entry = &feed.entries[0];
        assert_eq!(entry.title, "Alice & Bob & Carol");
        assert_eq!(entry.author, "Rémy");
        assert_eq!(entry.summary, "Costs < 5 & > 1");
        assert_eq!(entry.links[0].href, "https://example.com/d/a&b.epub");
    }

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
        let ctx = OpdsContext {
            trusted: vec!["192.168.0.12:7788".to_string()],
            ..Default::default()
        };
        let feed = parse_feed_with_context(xml, "http://192.168.0.12:7788/opds/all", &ctx).unwrap();
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

    #[test]
    fn origin_from_url_omits_default_ports_and_lowercases() {
        assert_eq!(
            origin_from_url("https://Books.Example.org:443/opds").as_deref(),
            Some("https://books.example.org")
        );
        assert_eq!(
            origin_from_url("http://Example.com:80/x").as_deref(),
            Some("http://example.com")
        );
        assert_eq!(
            origin_from_url("http://localhost:8080/opds").as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn origin_from_url_distinguishes_scheme() {
        assert_ne!(
            origin_from_url("http://h:443/x"),
            origin_from_url("https://h:443/x")
        );
    }

    #[test]
    fn origin_from_url_rejects_hostless_and_non_http() {
        assert!(origin_from_url("file:///etc/passwd").is_none());
        assert!(origin_from_url("javascript:alert(1)").is_none());
        assert!(origin_from_url("not a url").is_none());
    }

    #[test]
    fn is_loopback_host_matches_localhost_and_loopback_ips() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("app.localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.0.50"));
        assert!(!is_loopback_host("books.example.org"));
    }

    fn basic_ctx(origin: &str) -> OpdsContext {
        OpdsContext {
            trusted: vec![],
            provenance: Some(OpdsProvenance {
                catalog_url: format!("{origin}/opds"),
                origin: origin.to_string(),
            }),
            cred: Some(OpdsCredential::Basic {
                username: "u".into(),
                password: "p".into(),
            }),
        }
    }

    #[test]
    fn credential_applies_only_to_the_provenance_origin() {
        let ctx = basic_ctx("https://books.example.org");
        assert!(credential_for_url("https://books.example.org/opds/all", &ctx).is_some());
        assert!(
            credential_for_url("http://books.example.org/opds", &ctx).is_none(),
            "scheme downgrade"
        );
        assert!(
            credential_for_url("https://books.example.org:8443/opds", &ctx).is_none(),
            "port change"
        );
        assert!(
            credential_for_url("https://evil.example.net/opds", &ctx).is_none(),
            "host change"
        );
    }

    #[test]
    fn auth_status_maps_to_permission_error_with_the_frontend_substring() {
        let err = auth_error_for(reqwest::StatusCode::UNAUTHORIZED).expect("401 must map");
        assert!(err.to_string().contains("OPDS auth required"));
        assert!(auth_error_for(reqwest::StatusCode::FORBIDDEN).is_some());
        assert!(auth_error_for(reqwest::StatusCode::NOT_FOUND).is_none());
        assert!(auth_error_for(reqwest::StatusCode::OK).is_none());
    }

    #[test]
    fn credential_requires_both_provenance_and_secret() {
        let mut ctx = basic_ctx("https://h");
        ctx.cred = None;
        assert!(credential_for_url("https://h/x", &ctx).is_none());
        let mut ctx = basic_ctx("https://h");
        ctx.provenance = None;
        assert!(credential_for_url("https://h/x", &ctx).is_none());
    }
}

/// Tests that need a real HTTP server.
///
/// Credential handling is only observable on the wire: which `Authorization`
/// header actually goes out, and which redirects are followed. Neither can be
/// asserted by inspecting a `RequestBuilder`, so these use `wiremock`.
///
/// Two constraints apply to every test here. wiremock binds `127.0.0.1`, which
/// `is_safe_url_with_trusted` blocks by design — so the server's `host:port`
/// must go in `ctx.trusted`. And `OpdsContext` is deliberately not `Clone`, so
/// each call gets a freshly built context.
#[cfg(test)]
mod http_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(super) const MINIMAL_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>T</title>
          <entry><id>1</id><title>Book</title></entry>
        </feed>"#;

    /// Run a blocking core call from an async test without stalling the runtime.
    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(f).await.unwrap()
    }

    /// Context that trusts `url`'s host and carries nothing else.
    fn trusting_ctx(url: &str) -> OpdsContext {
        OpdsContext {
            trusted: vec![host_port_from_url(url).unwrap()],
            ..Default::default()
        }
    }

    /// Context that trusts `url`'s host and carries `cred` for `url`'s origin.
    fn authed_ctx(url: &str, cred: OpdsCredential) -> OpdsContext {
        OpdsContext {
            trusted: vec![host_port_from_url(url).unwrap()],
            provenance: Some(OpdsProvenance {
                catalog_url: url.to_string(),
                origin: origin_from_url(url).unwrap(),
            }),
            cred: Some(cred),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn basic_credentials_reach_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            // base64("u:p") == "dTpw" — an unmatched header 404s, so a parsed
            // feed proves this exact header was sent.
            .and(wiremock::matchers::header("authorization", "Basic dTpw"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());
        let ctx = authed_ctx(
            &url,
            OpdsCredential::Basic {
                username: "u".into(),
                password: "p".into(),
            },
        );
        let feed = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap();
        assert_eq!(feed.title, "T");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bearer_token_reaches_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer ck_live_abc",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());
        let ctx = authed_ctx(&url, OpdsCredential::Bearer("ck_live_abc".into()));
        let feed = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap();
        assert_eq!(feed.title, "T");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_credential_is_sent_to_another_origin_on_the_same_host() {
        // The scheme-downgrade guard, end to end: a credential registered for
        // the https origin must not go out over http, even to the same host and
        // port. Here provenance names an https origin while the request is
        // http, so the header must be absent.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .and(wiremock::matchers::header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());
        let mut ctx = authed_ctx(&url, OpdsCredential::Bearer("t".into()));
        // Same host:port, https instead of http.
        ctx.provenance = Some(OpdsProvenance {
            catalog_url: url.replace("http://", "https://"),
            origin: origin_from_url(&url)
                .unwrap()
                .replace("http://", "https://"),
        });
        let feed = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap();
        assert_eq!(
            feed.title, "T",
            "an https credential must not go out over http"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn authenticated_request_refuses_a_cross_origin_redirect() {
        // A 302s to B. With a credential for A the fetch must ERROR rather than
        // follow, and B must never be hit.
        //
        // Deliberately NOT using `.expect(0)` on B: during the red phase the
        // assertion panics, MockServer verifies expectations in Drop, and a
        // second panic while unwinding aborts the process instead of producing
        // a clean red test. `received_requests()` avoids that.
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feed"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&b)
            .await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{}/feed", b.uri())),
            )
            .mount(&a)
            .await;
        let url = format!("{}/opds", a.uri());
        let mut ctx = authed_ctx(&url, OpdsCredential::Bearer("t".into()));
        ctx.trusted.push(host_port_from_url(&b.uri()).unwrap());
        let res = blocking(move || fetch_feed_with_context(&url, &ctx)).await;
        let b_hits = b.received_requests().await.unwrap_or_default().len();
        assert!(
            res.is_err() && b_hits == 0,
            "authenticated cross-origin redirect must not be followed (is_err={}, B hits={b_hits})",
            res.is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unauthenticated_request_still_follows_a_cross_origin_redirect() {
        // Public catalogs (Gutenberg mirrors) redirect across hosts and must
        // keep working unchanged when no credential is involved.
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feed"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&b)
            .await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{}/feed", b.uri())),
            )
            .mount(&a)
            .await;
        let url = format!("{}/opds", a.uri());
        let mut ctx = trusting_ctx(&url);
        ctx.trusted.push(host_port_from_url(&b.uri()).unwrap());
        let feed = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap();
        assert_eq!(feed.title, "T");
        assert_eq!(
            b.received_requests().await.unwrap_or_default().len(),
            1,
            "B must have served the feed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_401_feed_surfaces_the_auth_error_not_a_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());
        let ctx = trusting_ctx(&url);
        let err = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("OPDS auth required"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_feed_reads_a_served_feed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/opds"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());
        let ctx = trusting_ctx(&url);
        let feed = blocking(move || fetch_feed_with_context(&url, &ctx))
            .await
            .unwrap();
        assert_eq!(feed.title, "T");
        assert_eq!(feed.entries.len(), 1);
    }
}

use ammonia::clean;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::io::Read;
use zip::ZipArchive;

use crate::models::{ChapterMeta, SearchResult, TocEntry};

// ---- Error type ----

/// `#[non_exhaustive]`: adding `LimitExceeded` broke every downstream exhaustive
/// match, which is why this release is a minor bump. Marking the enum here means
/// the next variant is additive instead — downstream matches must already carry a
/// wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum EpubError {
    InvalidFormat(String),
    MissingFile(String),
    ParseError(String),
    Io(std::io::Error),
    /// An archive-bounds limit was hit: too many entries, or an entry whose
    /// declared or actual decompressed size exceeds the cap. Distinct from
    /// `InvalidFormat` so callers (and tests) can tell a resource-exhaustion
    /// rejection apart from a malformed-zip rejection.
    LimitExceeded(String),
}

impl std::fmt::Display for EpubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpubError::InvalidFormat(msg) => write!(f, "Invalid EPUB format: {msg}"),
            EpubError::MissingFile(path) => write!(f, "Missing file in EPUB: {path}"),
            EpubError::ParseError(msg) => write!(f, "Parse error: {msg}"),
            EpubError::Io(e) => write!(f, "IO error: {e}"),
            EpubError::LimitExceeded(msg) => write!(f, "Archive limit exceeded: {msg}"),
        }
    }
}

impl From<std::io::Error> for EpubError {
    fn from(e: std::io::Error) -> Self {
        EpubError::Io(e)
    }
}

impl From<zip::result::ZipError> for EpubError {
    fn from(e: zip::result::ZipError) -> Self {
        EpubError::InvalidFormat(e.to_string())
    }
}

/// Bridge `EpubError` into the crate-wide [`crate::error::CarrelError`]. Both
/// types live in `carrel-core` (M3), so the impl is colocated with `EpubError`.
impl From<EpubError> for crate::error::CarrelError {
    fn from(e: EpubError) -> Self {
        use crate::error::CarrelError;
        match e {
            EpubError::MissingFile(s) => {
                CarrelError::not_found(format!("Missing file in EPUB: {s}"))
            }
            EpubError::Io(err) => CarrelError::from(err),
            EpubError::InvalidFormat(s) => {
                CarrelError::invalid(format!("Invalid EPUB format: {s}"))
            }
            EpubError::ParseError(s) => CarrelError::invalid(format!("Parse error: {s}")),
            EpubError::LimitExceeded(s) => {
                CarrelError::invalid(format!("Archive limit exceeded: {s}"))
            }
        }
    }
}

// ---- Data structures ----

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BookMetadata {
    pub title: String,
    pub author: String,
    pub language: String,
    pub description: Option<String>,
    pub isbn: Option<String>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChapterInfo {
    pub index: usize,
    pub title: String,
    pub href: String,
}

// ---- Cached archive ----

/// Holds an opened EPUB zip archive together with pre-parsed OPF metadata
/// so that consecutive calls (e.g. page turns) do not re-open the file.
pub struct CachedEpubArchive {
    archive: ZipArchive<std::fs::File>,
    #[allow(dead_code)]
    opf: String,
    base_dir: String,
    manifest: HashMap<String, ManifestItem>,
    spine: Vec<String>,
}

// ZipArchive<File> is not Send by default, but we protect access with a std::sync::Mutex
// in AppState so this is safe.
unsafe impl Send for CachedEpubArchive {}

/// Maximum number of entries allowed in an EPUB/CBZ archive.
pub const MAX_ARCHIVE_ENTRIES: usize = 10_000;
/// Maximum decompressed size per archive entry (100 MB).
pub const MAX_ENTRY_SIZE: u64 = 100 * 1024 * 1024;
/// Maximum decompressed size for a *text* archive entry — OPF, container.xml,
/// XHTML chapters, NCX/nav documents, CSS (16 MB).
///
/// Real-world text entries are kilobytes to low single-digit megabytes: the
/// largest single-file XHTML in a Gutenberg-scale EPUB is a few MB, and OPF /
/// NCX documents are far smaller. 16 MB leaves roughly an order of magnitude of
/// headroom over anything legitimate while capping the peak allocation a hostile
/// entry can force to 16 MB instead of the 100 MB the binary path allows.
///
/// **This number is a concurrency budget, not a parser limit.** What it bounds is
/// per-entry allocation for server-side readers handling many cold requests at
/// once: the worst case is roughly this cap × concurrent chapter reads, so 16 MB
/// keeps 50 concurrent readers' text path under 1 GB resident, where 32 MB would
/// put it at 1.6 GB. Raising it multiplies that ceiling — do not raise it to
/// accommodate one book.
///
/// **Known cost:** a fixed-layout EPUB that inlines its images as base64 data
/// URIs inside a single XHTML file can legitimately exceed 16 MB, and is
/// rejected with [`EpubError::LimitExceeded`]. That trade is deliberate. The
/// boundary is pinned by tests on both sides (15 MB parses, 17 MB is refused).
pub const MAX_TEXT_ENTRY_SIZE: u64 = 16 * 1024 * 1024;

/// Validate archive bounds: entry count and per-entry decompressed size.
///
/// Defense-in-depth strategy:
///
/// 1. **Pre-filter (this function):** Reject archives whose central directory claims any
///    single entry exceeds `MAX_ENTRY_SIZE` (100 MB). This is a fast O(n) scan over
///    metadata and catches honest or accidental oversized files without decompressing.
///
/// 2. **Runtime protection (`read_entry_capped`):** The pre-filter only sees what the
///    headers *claim*. The zip crate bounds a read by the entry's **compressed** size and
///    only notices a size/CRC mismatch once the entry has been fully decompressed — too
///    late to bound the allocation. Entry reads therefore go through `Read::take` with a
///    hard cap (`MAX_TEXT_ENTRY_SIZE` for text, `MAX_ENTRY_SIZE` for binary), so an entry
///    that understates its decompressed size is rejected at the cap instead of expanding
///    at deflate's full ratio.
///
/// We use `by_index()` rather than `by_index_raw()` so that sizes are read from the
/// central directory (which the zip crate cross-checks against local headers) instead
/// of only from local file headers, which are easier to forge.
///
/// Full decompression validation at import time is intentionally avoided — it would be
/// prohibitively slow for large archives with many entries.
pub fn validate_archive(archive: &mut ZipArchive<std::fs::File>) -> Result<(), EpubError> {
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(EpubError::LimitExceeded(format!(
            "Archive has {} entries (maximum {})",
            archive.len(),
            MAX_ARCHIVE_ENTRIES
        )));
    }
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            if entry.size() > MAX_ENTRY_SIZE {
                return Err(EpubError::LimitExceeded(format!(
                    "Archive entry '{}' decompressed size ({} MB) exceeds limit ({} MB)",
                    entry.name(),
                    entry.size() / (1024 * 1024),
                    MAX_ENTRY_SIZE / (1024 * 1024)
                )));
            }
        }
    }
    Ok(())
}

/// Open an EPUB from disk with archive bounds enforced before any parsing.
///
/// Every path-based entry point in this module funnels through here, so bounds
/// validation is a property of the API boundary rather than of each caller's
/// diligence. The `*_from_archive` / `*_from_cache` variants deliberately do
/// *not* validate: their caller already owns an archive it validated once, and
/// must not pay for the O(n) scan again per chapter read.
fn open_validated(file_path: &str) -> Result<ZipArchive<std::fs::File>, EpubError> {
    let file = std::fs::File::open(file_path).map_err(EpubError::Io)?;
    let mut archive = ZipArchive::new(file)?;
    validate_archive(&mut archive)?;
    Ok(archive)
}

impl CachedEpubArchive {
    pub fn open(file_path: &str) -> Result<Self, EpubError> {
        let mut archive = open_validated(file_path)?;
        let opf_path = find_opf_path(&mut archive)?;
        let opf = read_zip_entry(&mut archive, &opf_path)?;
        let base_dir = opf_base_dir(&opf_path).to_string();
        let manifest = parse_manifest(&opf);
        let spine = parse_spine_idrefs(&opf);
        Ok(Self {
            archive,
            opf,
            base_dir,
            manifest,
            spine,
        })
    }
}

// ---- Internal manifest item ----

#[derive(Debug, Clone)]
struct ManifestItem {
    href: String,
    properties: Option<String>,
}

// ---- Internal helpers ----

/// Read an entry through a hard byte cap.
///
/// [`validate_archive`]'s pre-scan only sees the size the central directory
/// *declares*, and the zip crate bounds a read by the entry's **compressed**
/// size (`ZipFile::get_reader` wraps the source in `take(compressed_size)`), not
/// by its decompressed size. An entry that understates its size therefore
/// decompresses to whatever the deflate ratio allows (~1032:1) before the CRC
/// check at EOF notices. Reading through `take(cap + 1)` bounds the buffer to
/// `cap` bytes regardless of what the headers claim; one byte over the cap is
/// enough to know the entry is too big.
pub(crate) fn read_entry_capped(
    entry: impl Read,
    cap: u64,
    name: &str,
) -> Result<Vec<u8>, EpubError> {
    let mut buf = Vec::new();
    entry
        .take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(EpubError::Io)?;
    if buf.len() as u64 > cap {
        return Err(EpubError::LimitExceeded(format!(
            "Archive entry '{name}' decompresses past the {cap}-byte limit"
        )));
    }
    Ok(buf)
}

/// Capped read of a text entry, decoded as UTF-8. Invalid UTF-8 yields the same
/// `Io(InvalidData)` error the previous `read_to_string` produced.
fn read_entry_capped_string(entry: impl Read, cap: u64, name: &str) -> Result<String, EpubError> {
    let buf = read_entry_capped(entry, cap, name)?;
    String::from_utf8(buf)
        .map_err(|e| EpubError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

/// Read a text file from a zip archive by name (case-insensitive path matching),
/// bounded by [`MAX_TEXT_ENTRY_SIZE`].
fn read_zip_entry(
    archive: &mut ZipArchive<std::fs::File>,
    name: &str,
) -> Result<String, EpubError> {
    // Try exact match first
    if let Ok(entry) = archive.by_name(name) {
        return read_entry_capped_string(entry, MAX_TEXT_ENTRY_SIZE, name);
    }
    // Try case-insensitive match
    let lower = name.to_lowercase();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.name().to_lowercase() == lower {
            let entry_name = entry.name().to_string();
            return read_entry_capped_string(entry, MAX_TEXT_ENTRY_SIZE, &entry_name);
        }
    }
    Err(EpubError::MissingFile(name.to_string()))
}

/// Read a file from a zip archive by name as raw bytes, bounded by
/// [`MAX_ENTRY_SIZE`] — binary entries (cover art, inline images) legitimately
/// dwarf text entries, so they keep the wider per-entry allowance.
fn read_zip_entry_bytes(
    archive: &mut ZipArchive<std::fs::File>,
    name: &str,
) -> Result<Vec<u8>, EpubError> {
    if let Ok(entry) = archive.by_name(name) {
        return read_entry_capped(entry, MAX_ENTRY_SIZE, name);
    }
    let lower = name.to_lowercase();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.name().to_lowercase() == lower {
            let entry_name = entry.name().to_string();
            return read_entry_capped(entry, MAX_ENTRY_SIZE, &entry_name);
        }
    }
    Err(EpubError::MissingFile(name.to_string()))
}

/// Parse META-INF/container.xml to find the OPF path.
/// Uses quick-xml so multi-line attribute formatting is handled correctly.
fn find_opf_path(archive: &mut ZipArchive<std::fs::File>) -> Result<String, EpubError> {
    let container = read_zip_entry(archive, "META-INF/container.xml")?;
    let mut reader = Reader::from_str(&container);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == b"rootfile" =>
            {
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"full-path" {
                        return Ok(String::from_utf8_lossy(&attr.value).into_owned());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(EpubError::ParseError(e.to_string())),
            _ => {}
        }
        buf.clear();
    }
    Err(EpubError::InvalidFormat(
        "Cannot find OPF path in container.xml".to_string(),
    ))
}

/// Given OPF path, return the base directory (for resolving relative hrefs).
fn opf_base_dir(opf_path: &str) -> &str {
    if let Some(pos) = opf_path.rfind('/') {
        &opf_path[..pos + 1]
    } else {
        ""
    }
}

/// Extract text content between XML tags (minimal parser for metadata elements).
pub fn extract_tag_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start_tag = xml.find(&open)?;
    let after_open = &xml[start_tag..];
    let content_start = after_open.find('>')? + 1;
    let content = &after_open[content_start..];
    let end = content.find(&close)?;
    Some(content[..end].trim())
}

/// Decode XML/HTML character entities (`&lt;`, `&gt;`, `&amp;`, `&quot;`,
/// numeric `&#..;`, …) in extracted metadata text. ComicInfo `<Summary>` and
/// OPF `<dc:*>` values are frequently entity-encoded at the source; without
/// this the raw `&gt;`/`&amp;` survive into the DB and render literally in
/// both the desktop and web UIs. Falls back to the original string if the
/// input isn't well-formed (e.g. a stray unescaped `&`), so decoding can
/// never drop or mangle a value — worst case it is left exactly as today.
fn decode_entities(s: &str) -> String {
    quick_xml::escape::unescape(s)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

/// Like [`extract_tag_text`] but decodes character entities in the result.
/// Use for human-readable free-text fields (title, description, summary,
/// series, publisher, …); numeric/identifier fields (Year, Volume,
/// LanguageISO) should keep using the raw [`extract_tag_text`].
pub fn extract_tag_text_decoded(xml: &str, tag: &str) -> Option<String> {
    extract_tag_text(xml, tag).map(decode_entities)
}

/// Extract all occurrences of a tag's text content.
fn extract_all_tag_texts(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut results = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start..];
        if let Some(content_start) = rest.find('>') {
            let after = &rest[content_start + 1..];
            if let Some(end) = after.find(&close) {
                results.push(after[..end].trim().to_string());
                rest = &after[end + close.len()..];
            } else {
                break;
            }
        } else {
            break;
        }
    }
    results
}

/// Parse manifest items (id → ManifestItem) from OPF XML using quick-xml.
/// Captures href and properties attributes to enable nav/cover detection.
fn parse_manifest(opf: &str) -> HashMap<String, ManifestItem> {
    let mut manifest = HashMap::new();
    let mut reader = Reader::from_str(opf);
    let mut buf = Vec::new();
    let mut in_manifest = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"manifest" {
                    in_manifest = true;
                } else if in_manifest && local.as_ref() == b"item" {
                    let mut id: Option<String> = None;
                    let mut href: Option<String> = None;
                    let mut properties: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => id = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                            b"href" => {
                                href = Some(String::from_utf8_lossy(&attr.value).into_owned())
                            }
                            b"properties" => {
                                properties = Some(String::from_utf8_lossy(&attr.value).into_owned())
                            }
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(href)) = (id, href) {
                        manifest.insert(id, ManifestItem { href, properties });
                    }
                }
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == b"manifest" => {
                in_manifest = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    manifest
}

/// Parse spine items (idref list) from OPF XML using quick-xml.
fn parse_spine_idrefs(opf: &str) -> Vec<String> {
    let mut idrefs = Vec::new();
    let mut reader = Reader::from_str(opf);
    let mut buf = Vec::new();
    let mut in_spine = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"spine" {
                    in_spine = true;
                } else if in_spine && local.as_ref() == b"itemref" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"idref" {
                            idrefs.push(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == b"spine" => {
                in_spine = false;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    idrefs
}

/// Find the cover-meta id from EPUB 2 OPF (<meta name="cover" content="id"/>).
fn find_cover_meta_id(opf: &str) -> Option<String> {
    let mut reader = Reader::from_str(opf);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e))
                if e.local_name().as_ref() == b"meta" =>
            {
                let mut name: Option<String> = None;
                let mut content: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"name" => name = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        b"content" => {
                            content = Some(String::from_utf8_lossy(&attr.value).into_owned())
                        }
                        _ => {}
                    }
                }
                if name.as_deref() == Some("cover") {
                    return content;
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

/// Find the cover image href from OPF (EPUB 2 and 3).
fn find_cover_href(opf: &str) -> Option<String> {
    let manifest = parse_manifest(opf);

    // EPUB 3: item with properties="cover-image"
    for item in manifest.values() {
        if let Some(ref props) = item.properties {
            if props.split_whitespace().any(|p| p == "cover-image") {
                return Some(item.href.clone());
            }
        }
    }

    // EPUB 2: <meta name="cover" content="cover-id"/>
    let cover_id = find_cover_meta_id(opf)?;
    manifest.get(&cover_id).map(|item| item.href.clone())
}

/// Find all zip entry names that match a given href prefix (handles path normalization).
fn find_zip_entry_name(
    archive: &mut ZipArchive<std::fs::File>,
    base_dir: &str,
    href: &str,
) -> Option<String> {
    let candidate = format!("{base_dir}{href}");
    // Strip query/fragment from href
    let clean_href = href.split('#').next().unwrap_or(href);
    let clean_candidate = format!("{base_dir}{clean_href}");
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index_raw(i) {
            let name = entry.name().to_string();
            if name == candidate
                || name == clean_candidate
                || name.to_lowercase() == clean_candidate.to_lowercase()
            {
                return Some(name);
            }
        }
    }
    None
}

// ---- Public API ----

/// Parse metadata from an already-opened EPUB zip archive.
pub fn parse_epub_metadata_from_archive(
    archive: &mut ZipArchive<std::fs::File>,
) -> Result<BookMetadata, EpubError> {
    let opf_path = find_opf_path(archive)?;
    let opf = read_zip_entry(archive, &opf_path)?;

    let title = extract_tag_text_decoded(&opf, "dc:title")
        .or_else(|| extract_tag_text_decoded(&opf, "title"))
        .unwrap_or_else(|| "Unknown Title".to_string());

    let author = extract_all_tag_texts(&opf, "dc:creator")
        .into_iter()
        .next()
        .map(|s| decode_entities(&s))
        .or_else(|| extract_tag_text_decoded(&opf, "creator"))
        .unwrap_or_else(|| "Unknown Author".to_string());

    let language = extract_tag_text(&opf, "dc:language")
        .or_else(|| extract_tag_text(&opf, "language"))
        .unwrap_or("en")
        .to_string();

    let description = extract_tag_text_decoded(&opf, "dc:description")
        .or_else(|| extract_tag_text_decoded(&opf, "description"));

    let isbn = extract_all_tag_texts(&opf, "dc:identifier")
        .iter()
        .chain(extract_all_tag_texts(&opf, "identifier").iter())
        .find_map(|id| crate::isbn::extract_isbn(id));

    let genres = {
        let mut subjects = extract_all_tag_texts(&opf, "dc:subject");
        subjects.extend(extract_all_tag_texts(&opf, "subject"));
        subjects
    };

    Ok(BookMetadata {
        title,
        author,
        language,
        description,
        isbn,
        genres,
    })
}

/// Parse metadata from the OPF file inside the EPUB.
pub fn parse_epub_metadata(file_path: &str) -> Result<BookMetadata, EpubError> {
    let mut archive = open_validated(file_path)?;
    parse_epub_metadata_from_archive(&mut archive)
}

/// Sanitize a cover href from OPF metadata to prevent path traversal attacks.
/// Returns `None` if the href is malicious or would resolve to an empty path.
fn sanitize_cover_href(href: &str) -> Option<String> {
    // Reject null bytes
    if href.contains('\0') {
        return None;
    }

    // Reject absolute paths (Unix and Windows)
    if href.starts_with('/') || href.starts_with('\\') {
        return None;
    }

    // Reject Windows drive letters (e.g., "C:", "D:\")
    if href.len() >= 2 && href.as_bytes()[0].is_ascii_alphabetic() && href.as_bytes()[1] == b':' {
        return None;
    }

    // Resolve the path and reject if it escapes the base directory.
    // We simulate resolving from an empty base — any ".." that would go above
    // the root means the path is trying to escape.
    let mut parts: Vec<&str> = Vec::new();
    for segment in href.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                // Tried to go above root — path traversal
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(href.to_string())
}

/// Validate a cover image file extension against an allowlist.
/// Returns the extension (lowercase) if it is a recognized image type,
/// or `"jpg"` as a safe default otherwise.
fn sanitize_cover_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" => "jpg",
        "jpeg" => "jpeg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "svg" => "svg",
        _ => "jpg",
    }
}

/// Raw cover image data extracted from an EPUB: the image bytes plus a
/// sanitized file extension (`png`, `jpg`, `webp`, `gif`, `svg`). Callers
/// decide where to persist the bytes — typically via
/// `carrel_core::storage::Storage::put`. Introduced for #64 M3 so the
/// parser no longer assumes a local filesystem destination.
pub struct ExtractedCover {
    pub bytes: Vec<u8>,
    pub ext: String,
}

/// Extract the cover image bytes from an already-opened EPUB zip archive.
///
/// Returns `Ok(None)` when the EPUB has no cover or the referenced entry
/// is missing/unsafe. The caller is responsible for writing the bytes to
/// whichever storage layer it owns (previous versions of this function
/// wrote directly to disk).
pub fn extract_cover_from_archive(
    archive: &mut ZipArchive<std::fs::File>,
) -> Result<Option<ExtractedCover>, EpubError> {
    let opf_path = find_opf_path(archive)?;
    let opf = read_zip_entry(archive, &opf_path)?;
    let base_dir = opf_base_dir(&opf_path).to_string();

    let cover_href = match find_cover_href(&opf) {
        Some(h) => h,
        None => return Ok(None),
    };

    // Sanitize cover href to prevent path traversal
    let cover_href = match sanitize_cover_href(&cover_href) {
        Some(h) => h,
        None => return Ok(None),
    };

    // Determine the entry name in the zip
    let entry_name = match find_zip_entry_name(archive, &base_dir, &cover_href) {
        Some(n) => n,
        None => return Ok(None),
    };

    let bytes = read_zip_entry_bytes(archive, &entry_name)?;

    // Derive extension from href, restricted to known image types
    let raw_ext = cover_href.rsplit('.').next().unwrap_or("jpg");
    let ext = sanitize_cover_ext(raw_ext).to_string();

    Ok(Some(ExtractedCover { bytes, ext }))
}

/// Extract cover image bytes from an EPUB file on disk.
pub fn extract_cover(file_path: &str) -> Result<Option<ExtractedCover>, EpubError> {
    let mut archive = open_validated(file_path)?;
    extract_cover_from_archive(&mut archive)
}

/// Get ordered list of chapters (spine order) from an already-opened archive.
pub fn get_chapter_list_from_archive(
    archive: &mut ZipArchive<std::fs::File>,
) -> Result<Vec<ChapterInfo>, EpubError> {
    let opf_path = find_opf_path(archive)?;
    let opf = read_zip_entry(archive, &opf_path)?;

    let manifest = parse_manifest(&opf);
    let spine = parse_spine_idrefs(&opf);

    let chapters: Vec<ChapterInfo> = spine
        .into_iter()
        .enumerate()
        .filter_map(|(index, idref)| {
            manifest.get(&idref).map(|item| ChapterInfo {
                index,
                title: format!("Chapter {}", index + 1),
                href: item.href.clone(),
            })
        })
        .collect();

    Ok(chapters)
}

/// Get ordered list of chapters (spine order).
pub fn get_chapter_list(file_path: &str) -> Result<Vec<ChapterInfo>, EpubError> {
    let mut archive = open_validated(file_path)?;
    get_chapter_list_from_archive(&mut archive)
}

/// Get HTML content of a specific chapter by index.
/// Relative `<img src>` attributes are rewritten to `asset://` URLs pointing to
/// images extracted from the EPUB into `storage`, avoiding large base64
/// strings in memory. Keys land under `{book_id}/{chapter_index}/{basename}`.
pub fn get_chapter_content(
    file_path: &str,
    chapter_index: usize,
    storage: &dyn crate::storage::Storage,
    book_id: &str,
) -> Result<String, EpubError> {
    let mut archive = open_validated(file_path)?;

    let opf_path = find_opf_path(&mut archive)?;
    let opf = read_zip_entry(&mut archive, &opf_path)?;
    let base_dir = opf_base_dir(&opf_path).to_string();

    let manifest = parse_manifest(&opf);
    let spine = parse_spine_idrefs(&opf);

    let idref = spine.get(chapter_index).ok_or_else(|| {
        EpubError::InvalidFormat(format!("Chapter index {chapter_index} out of range"))
    })?;

    let href = manifest
        .get(idref)
        .map(|item| item.href.clone())
        .ok_or_else(|| EpubError::MissingFile(format!("Manifest item '{idref}' not found")))?;

    let entry_name = find_zip_entry_name(&mut archive, &base_dir, &href)
        .ok_or_else(|| EpubError::MissingFile(format!("{base_dir}{href}")))?;

    let raw_html = read_zip_entry(&mut archive, &entry_name)?;
    // Sanitize first so ammonia never sees the asset URLs we are about to inject.
    let cleaned = clean(&raw_html);

    // Compute the directory of the chapter file within the zip so relative
    // image paths (e.g. "../images/foo.png") can be resolved.
    let chapter_dir = {
        let full_path = format!("{base_dir}{href}");
        match full_path.rfind('/') {
            Some(pos) => full_path[..pos + 1].to_string(),
            None => String::new(),
        }
    };

    let key_prefix = format!("{book_id}/{chapter_index}");

    Ok(rewrite_img_srcs_to_asset_urls(
        &cleaned,
        &mut archive,
        &chapter_dir,
        storage,
        &key_prefix,
    ))
}

/// Like [`get_chapter_content`] but operates on a [`CachedEpubArchive`],
/// avoiding the cost of re-opening the zip and re-parsing OPF metadata.
pub fn get_chapter_content_from_cache(
    cached: &mut CachedEpubArchive,
    chapter_index: usize,
    storage: &dyn crate::storage::Storage,
    book_id: &str,
) -> Result<String, EpubError> {
    let idref = cached.spine.get(chapter_index).ok_or_else(|| {
        EpubError::InvalidFormat(format!("Chapter index {chapter_index} out of range"))
    })?;

    let href = cached
        .manifest
        .get(idref)
        .map(|item| item.href.clone())
        .ok_or_else(|| EpubError::MissingFile(format!("Manifest item '{idref}' not found")))?;

    let base_dir = &cached.base_dir;
    let entry_name = find_zip_entry_name(&mut cached.archive, base_dir, &href)
        .ok_or_else(|| EpubError::MissingFile(format!("{base_dir}{href}")))?;

    let raw_html = read_zip_entry(&mut cached.archive, &entry_name)?;
    let cleaned = clean(&raw_html);

    let chapter_dir = {
        let full_path = format!("{base_dir}{href}");
        match full_path.rfind('/') {
            Some(pos) => full_path[..pos + 1].to_string(),
            None => String::new(),
        }
    };

    let key_prefix = format!("{book_id}/{chapter_index}");

    Ok(rewrite_img_srcs_to_asset_urls(
        &cleaned,
        &mut cached.archive,
        &chapter_dir,
        storage,
        &key_prefix,
    ))
}

// ---- Full-text search ----

/// Clamp a byte index to the nearest valid UTF-8 char boundary (floor).
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Clamp a byte index to the nearest valid UTF-8 char boundary (ceil).
fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Extract a context snippet around a match position in plain text.
/// All byte offsets are clamped to valid UTF-8 char boundaries.
pub fn extract_snippet(
    text: &str,
    match_start: usize,
    match_len: usize,
    context_chars: usize,
) -> String {
    let start = floor_char_boundary(text, match_start.saturating_sub(context_chars));
    let end = ceil_char_boundary(
        text,
        (match_start + match_len + context_chars).min(text.len()),
    );

    // Align to word boundaries
    let snippet_start = if start > 0 {
        text[start..]
            .find(' ')
            .map(|p| start + p + 1)
            .unwrap_or(start)
    } else {
        0
    };
    let snippet_end = if end < text.len() {
        text[..end].rfind(' ').unwrap_or(end)
    } else {
        text.len()
    };

    let snippet_start = floor_char_boundary(text, snippet_start);
    let snippet_end = ceil_char_boundary(text, snippet_end);

    let mut snippet = String::new();
    if snippet_start > 0 {
        snippet.push_str("...");
    }
    snippet.push_str(text[snippet_start..snippet_end].trim());
    if snippet_end < text.len() {
        snippet.push_str("...");
    }
    snippet
}

const MAX_SEARCH_RESULTS: usize = 200;

/// Search all chapters of an EPUB for a query string (case-insensitive).
pub fn search_book(
    cached: &mut CachedEpubArchive,
    query: &str,
) -> Result<Vec<SearchResult>, EpubError> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for i in 0..cached.spine.len() {
        let idref = &cached.spine[i];
        let href = cached
            .manifest
            .get(idref)
            .map(|item| item.href.clone())
            .ok_or_else(|| EpubError::MissingFile(format!("Manifest item '{idref}' not found")))?;

        let base_dir = &cached.base_dir;
        let entry_name = find_zip_entry_name(&mut cached.archive, base_dir, &href)
            .ok_or_else(|| EpubError::MissingFile(format!("{base_dir}{href}")))?;

        let raw_html = read_zip_entry(&mut cached.archive, &entry_name)?;
        let body_only = clean(&raw_html);
        let text = strip_html_tags(&body_only);
        let text_lower = text.to_lowercase();

        let mut search_from = 0;
        while let Some(pos) = text_lower[search_from..].find(&query_lower) {
            let match_start = search_from + pos;
            results.push(SearchResult {
                chapter_index: i as u32,
                snippet: extract_snippet(&text, match_start, query_lower.len(), 40),
                match_offset: match_start,
            });
            if results.len() >= MAX_SEARCH_RESULTS {
                return Ok(results);
            }
            search_from = match_start + query_lower.len();
        }
    }

    Ok(results)
}

// ---- Word counting ----

/// Strip HTML tags from a string and return plain text.
///
/// This is a simple char-level scanner that replaces `<…>` runs with a space.
/// It does NOT handle HTML entities (`&nbsp;`, `&amp;`), comments (`<!-- -->`),
/// or CDATA sections. This is acceptable because all input has already been
/// sanitized by ammonia's `clean()`, which strips comments and normalizes
/// entities, so the remaining markup is well-formed simple HTML tags.
pub fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                result.push(' '); // space between tags
            }
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

/// Count words in a string (split by whitespace).
pub fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Get word counts for all chapters from a cached EPUB archive.
pub fn get_chapter_word_counts(cached: &mut CachedEpubArchive) -> Result<Vec<usize>, EpubError> {
    let mut counts = Vec::with_capacity(cached.spine.len());
    for i in 0..cached.spine.len() {
        let idref = &cached.spine[i];
        let href = cached
            .manifest
            .get(idref)
            .map(|item| item.href.clone())
            .ok_or_else(|| EpubError::MissingFile(format!("Manifest item '{idref}' not found")))?;

        let base_dir = &cached.base_dir;
        let entry_name = find_zip_entry_name(&mut cached.archive, base_dir, &href)
            .ok_or_else(|| EpubError::MissingFile(format!("{base_dir}{href}")))?;

        let raw_html = read_zip_entry(&mut cached.archive, &entry_name)?;
        let body_only = clean(&raw_html); // strip <head>, <style>, <script> etc.
        let text = strip_html_tags(&body_only);
        counts.push(count_words(&text));
    }
    Ok(counts)
}

/// Combined chapter index, title, and word count in a single pass.
pub fn get_chapter_metadata_batch(
    cached: &mut CachedEpubArchive,
) -> Result<Vec<ChapterMeta>, EpubError> {
    let mut result = Vec::with_capacity(cached.spine.len());
    for i in 0..cached.spine.len() {
        let idref = &cached.spine[i];
        let href = cached
            .manifest
            .get(idref)
            .map(|item| item.href.clone())
            .ok_or_else(|| EpubError::MissingFile(format!("Manifest item '{idref}' not found")))?;

        let base_dir = &cached.base_dir;
        let entry_name = find_zip_entry_name(&mut cached.archive, base_dir, &href)
            .ok_or_else(|| EpubError::MissingFile(format!("{base_dir}{href}")))?;

        let raw_html = read_zip_entry(&mut cached.archive, &entry_name)?;
        let body_only = clean(&raw_html);
        let text = strip_html_tags(&body_only);

        result.push(ChapterMeta {
            index: i,
            title: format!("Chapter {}", i + 1),
            word_count: count_words(&text),
        });
    }
    Ok(result)
}

// ---- Inline-image helpers ----

/// Resolve a relative path against a zip-internal base directory.
/// e.g. base="OEBPS/Text/", relative="../images/foo.png" → "OEBPS/images/foo.png"
fn resolve_zip_path(base_dir: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = base_dir
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Extract the value of a quoted HTML attribute from a tag string.
/// Handles both double- and single-quoted values.
fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let dq = format!("{attr}=\"");
    let sq = format!("{attr}='");
    if let Some(pos) = tag.find(&dq) {
        let after = &tag[pos + dq.len()..];
        let end = after.find('"')?;
        Some(after[..end].to_string())
    } else if let Some(pos) = tag.find(&sq) {
        let after = &tag[pos + sq.len()..];
        let end = after.find('\'')?;
        Some(after[..end].to_string())
    } else {
        None
    }
}

/// Replace a specific attribute value within a tag string.
fn replace_attr_value(tag: &str, attr: &str, old_val: &str, new_val: &str) -> String {
    let dq_old = format!("{attr}=\"{old_val}\"");
    let dq_new = format!("{attr}=\"{new_val}\"");
    if tag.contains(&dq_old) {
        return tag.replacen(&dq_old, &dq_new, 1);
    }
    let sq_old = format!("{attr}='{old_val}'");
    let sq_new = format!("{attr}='{new_val}'");
    tag.replacen(&sq_old, &sq_new, 1)
}

/// Byte offset just past the `>` that closes the tag beginning at `from_tag[0]`,
/// skipping any `>` that sits inside a single- or double-quoted attribute value.
/// ammonia does not escape `>` in attribute values (e.g. `alt="a > b"`), so a
/// naive `find('>')` would truncate the tag mid-attribute and miss its `src`.
/// Falls back to the full length when no closing `>` is found.
fn find_tag_end(from_tag: &str) -> usize {
    let mut quote: Option<char> = None;
    for (i, c) in from_tag.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return i + 1,
                _ => {}
            },
        }
    }
    from_tag.len()
}

/// 16 hex chars (64 bits) of SHA-256 over a resolved zip path — enough to
/// disambiguate every distinct image within a single EPUB chapter and
/// deterministic so repeated reads resolve to the same storage key.
fn short_zip_path_hash(resolved_zip_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(resolved_zip_path.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Walk the sanitized chapter HTML and replace relative `<img src>` values
/// with `asset://localhost/` URLs pointing to images extracted from the EPUB
/// zip through `storage`. This avoids base64-encoding large images into the
/// HTML string, which can cause memory issues with illustrated books.
///
/// Images are keyed as `{key_prefix}/{hash}-{basename}` where `hash` is a
/// short SHA-256 of the resolved zip path (see [`short_zip_path_hash`]).
/// The hash prefix disambiguates entries that share a basename across zip
/// subdirectories, so two images like `dir_a/cover.png` and `dir_b/cover.png`
/// produce distinct keys and never overwrite each other. Identical resolved
/// paths still hash to the same key, so repeated extraction of the same
/// image within a chapter stays idempotent.
///
/// If `storage.exists(key)` is already true the zip entry is not
/// re-extracted. External URLs and SVG images are left untouched. Missing
/// images and storage failures fall back to leaving the `<img>` tag
/// unchanged so a single broken asset doesn't abort the whole chapter.
///
/// The `asset://` URL is derived from `storage.local_path(key)` — remote
/// backends that need a command-fetch fallback should implement a
/// caching-aware `local_path` or return an error to trigger the fallback.
fn rewrite_img_srcs_to_asset_urls(
    html: &str,
    archive: &mut ZipArchive<std::fs::File>,
    chapter_dir: &str,
    storage: &dyn crate::storage::Storage,
    key_prefix: &str,
) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(tag_start) = rest.find("<img") {
        // Confirm the match is really an <img tag (not e.g. <imgfoo).
        let after_tag = &rest[tag_start + 4..];
        match after_tag.bytes().next() {
            Some(b)
                if b == b' '
                    || b == b'\t'
                    || b == b'\n'
                    || b == b'\r'
                    || b == b'>'
                    || b == b'/' => {}
            _ => {
                // Not an img tag — advance past the false match.
                result.push_str(&rest[..tag_start + 1]);
                rest = &rest[tag_start + 1..];
                continue;
            }
        }

        result.push_str(&rest[..tag_start]);
        let from_tag = &rest[tag_start..];

        let tag_end = find_tag_end(from_tag);
        let tag = &from_tag[..tag_end];

        let rewritten = match extract_attr_value(tag, "src") {
            None => tag.to_string(),
            Some(src) => {
                // Leave external or already-resolved URLs alone.
                let is_external = src.starts_with("http://")
                    || src.starts_with("https://")
                    || src.starts_with("data:")
                    || src.starts_with("//")
                    || src.starts_with("asset://")
                    || src.starts_with('/');
                if is_external {
                    tag.to_string()
                } else {
                    // Strip fragment / query before resolving.
                    let clean_src = src
                        .split('#')
                        .next()
                        .unwrap_or(&src)
                        .split('?')
                        .next()
                        .unwrap_or(&src);
                    let ext = clean_src.rsplit('.').next().unwrap_or("").to_lowercase();

                    // DOMPurify strips SVG data URIs by default — skip them.
                    if ext == "svg" {
                        tag.to_string()
                    } else {
                        let resolved = resolve_zip_path(chapter_dir, clean_src);
                        // Derive a safe filename from the image path basename.
                        let basename = clean_src.rsplit('/').next().unwrap_or(clean_src);
                        // Prefix the basename with a short hash of the
                        // resolved zip path so two distinct entries that
                        // share a basename (e.g. `dir_a/cover.png` and
                        // `dir_b/cover.png`) cannot collide on the same
                        // storage key. Keeping the basename in the key
                        // preserves debuggability.
                        let key =
                            format!("{key_prefix}/{}-{basename}", short_zip_path_hash(&resolved));

                        // Extract through storage if not already cached.
                        let written = if storage.exists(&key).unwrap_or(false) {
                            true
                        } else if let Ok(bytes) = read_zip_entry_bytes(archive, &resolved) {
                            storage.put(&key, &bytes).is_ok()
                        } else {
                            false
                        };

                        if written {
                            match storage.local_path(&key) {
                                Ok(p) => {
                                    let abs_path = p.to_string_lossy();
                                    let encoded = urlencoding::encode(&abs_path);
                                    let asset_url = format!("asset://localhost/{}", encoded);
                                    replace_attr_value(tag, "src", &src, &asset_url)
                                }
                                Err(_) => tag.to_string(),
                            }
                        } else {
                            tag.to_string()
                        }
                    }
                }
            }
        };

        result.push_str(&rewritten);
        rest = &from_tag[tag_end..];
    }

    result.push_str(rest);
    result
}

/// Get table of contents from NCX (EPUB 2) or nav document (EPUB 3).
pub fn get_toc(file_path: &str) -> Result<Vec<TocEntry>, EpubError> {
    let mut archive = open_validated(file_path)?;

    let opf_path = find_opf_path(&mut archive)?;
    let opf = read_zip_entry(&mut archive, &opf_path)?;
    let base_dir = opf_base_dir(&opf_path).to_string();
    let manifest = parse_manifest(&opf);
    let spine = parse_spine_idrefs(&opf);

    // Build href → chapter_index map
    let href_to_index: HashMap<String, usize> = spine
        .iter()
        .enumerate()
        .filter_map(|(i, idref)| manifest.get(idref).map(|item| (item.href.clone(), i)))
        .collect();

    // Try EPUB 3 nav document first (detected via properties="nav" attribute per spec)
    let nav_href = manifest.iter().find_map(|(_, item)| {
        item.properties
            .as_deref()
            .filter(|p| p.split_whitespace().any(|prop| prop == "nav"))
            .map(|_| item.href.clone())
    });

    if let Some(nav_href) = nav_href {
        let entry_name = find_zip_entry_name(&mut archive, &base_dir, &nav_href);
        if let Some(name) = entry_name {
            if let Ok(nav_content) = read_zip_entry(&mut archive, &name) {
                let entries = parse_nav_toc(&nav_content, &href_to_index);
                if !entries.is_empty() {
                    return Ok(entries);
                }
            }
        }
    }

    // Fall back to EPUB 2 NCX
    let ncx_href = manifest.iter().find_map(|(_, item)| {
        if item.href.ends_with(".ncx") {
            Some(item.href.clone())
        } else {
            None
        }
    });

    if let Some(ncx_href) = ncx_href {
        let entry_name = find_zip_entry_name(&mut archive, &base_dir, &ncx_href);
        if let Some(name) = entry_name {
            if let Ok(ncx_content) = read_zip_entry(&mut archive, &name) {
                return Ok(parse_ncx_toc(&ncx_content, &href_to_index));
            }
        }
    }

    // Fallback: generate TOC from spine
    let toc = spine
        .iter()
        .enumerate()
        .filter_map(|(i, idref)| {
            manifest.get(idref).map(|_| TocEntry {
                label: format!("Chapter {}", i + 1),
                chapter_index: i as u32,
                play_order: format!("{}", i + 1),
                children: vec![],
            })
        })
        .collect();
    Ok(toc)
}

/// Like [`get_toc`] but operates on a [`CachedEpubArchive`].
pub fn get_toc_from_cache(cached: &mut CachedEpubArchive) -> Result<Vec<TocEntry>, EpubError> {
    let href_to_index: HashMap<String, usize> = cached
        .spine
        .iter()
        .enumerate()
        .filter_map(|(i, idref)| {
            cached
                .manifest
                .get(idref)
                .map(|item| (item.href.clone(), i))
        })
        .collect();

    // Try EPUB 3 nav document first
    let nav_href = cached.manifest.iter().find_map(|(_, item)| {
        item.properties
            .as_deref()
            .filter(|p| p.split_whitespace().any(|prop| prop == "nav"))
            .map(|_| item.href.clone())
    });

    if let Some(nav_href) = nav_href {
        let entry_name = find_zip_entry_name(&mut cached.archive, &cached.base_dir, &nav_href);
        if let Some(name) = entry_name {
            if let Ok(nav_content) = read_zip_entry(&mut cached.archive, &name) {
                let entries = parse_nav_toc(&nav_content, &href_to_index);
                if !entries.is_empty() {
                    return Ok(entries);
                }
            }
        }
    }

    // Fall back to EPUB 2 NCX
    let ncx_href = cached.manifest.iter().find_map(|(_, item)| {
        if item.href.ends_with(".ncx") {
            Some(item.href.clone())
        } else {
            None
        }
    });

    if let Some(ncx_href) = ncx_href {
        let entry_name = find_zip_entry_name(&mut cached.archive, &cached.base_dir, &ncx_href);
        if let Some(name) = entry_name {
            if let Ok(ncx_content) = read_zip_entry(&mut cached.archive, &name) {
                return Ok(parse_ncx_toc(&ncx_content, &href_to_index));
            }
        }
    }

    // Fallback: generate TOC from spine
    let toc = cached
        .spine
        .iter()
        .enumerate()
        .filter_map(|(i, idref)| {
            cached.manifest.get(idref).map(|_| TocEntry {
                label: format!("Chapter {}", i + 1),
                chapter_index: i as u32,
                play_order: format!("{}", i + 1),
                children: vec![],
            })
        })
        .collect();
    Ok(toc)
}

/// Parse EPUB 3 nav TOC using quick-xml.
/// Finds the nav element with epub:type="toc" and extracts anchor links.
fn parse_nav_toc(nav: &str, href_to_index: &HashMap<String, usize>) -> Vec<TocEntry> {
    let mut entries = Vec::new();
    let mut reader = Reader::from_str(nav);
    let mut buf = Vec::new();
    let mut in_toc_nav = false;
    let mut nav_depth = 0u32;
    let mut in_anchor = false;
    let mut anchor_depth = 0u32;
    let mut current_href: Option<String> = None;
    let mut current_label = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                if !in_toc_nav {
                    if local.as_ref() == b"nav" {
                        // Check for epub:type="toc" — strip namespace prefix from attribute key
                        let is_toc = e.attributes().flatten().any(|attr| {
                            let key = attr.key.as_ref();
                            let local_key = key.splitn(2, |&b| b == b':').last().unwrap_or(key);
                            local_key == b"type" && attr.value.as_ref() == b"toc"
                        });
                        if is_toc {
                            in_toc_nav = true;
                            nav_depth = 1;
                        }
                    }
                } else if in_anchor {
                    anchor_depth += 1;
                } else if local.as_ref() == b"nav" {
                    nav_depth += 1;
                } else if local.as_ref() == b"a" {
                    in_anchor = true;
                    anchor_depth = 1;
                    current_label.clear();
                    current_href = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"href" {
                            current_href = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) if in_anchor => {
                if let Ok(text) = e.unescape() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if !current_label.is_empty() {
                            current_label.push(' ');
                        }
                        current_label.push_str(trimmed);
                    }
                }
            }
            Ok(Event::End(ref e)) if in_toc_nav => {
                if in_anchor {
                    anchor_depth = anchor_depth.saturating_sub(1);
                    if anchor_depth == 0 {
                        if let Some(href) = current_href.take() {
                            let clean_href = href.split('#').next().unwrap_or(&href).to_string();
                            let chapter_index =
                                href_to_index.get(&clean_href).copied().unwrap_or(0);
                            let label = if current_label.is_empty() {
                                format!("Chapter {}", chapter_index + 1)
                            } else {
                                current_label.clone()
                            };
                            entries.push(TocEntry {
                                label,
                                chapter_index: chapter_index as u32,
                                play_order: format!("{}", chapter_index + 1), // Default simple play order
                                children: vec![],
                            });
                        }
                        in_anchor = false;
                    }
                } else if e.local_name().as_ref() == b"nav" {
                    nav_depth = nav_depth.saturating_sub(1);
                    if nav_depth == 0 {
                        in_toc_nav = false;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    entries
}

/// Parse EPUB 2 NCX TOC using quick-xml.
fn parse_ncx_toc(ncx: &str, href_to_index: &HashMap<String, usize>) -> Vec<TocEntry> {
    let mut entries = Vec::new();
    let mut reader = Reader::from_str(ncx);
    let mut buf = Vec::new();

    #[derive(Default)]
    struct NavState {
        label: String,
        src: String,
        in_text: bool,
    }

    let mut stack: Vec<NavState> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"navPoint" => stack.push(NavState::default()),
                    b"text" => {
                        if let Some(state) = stack.last_mut() {
                            state.in_text = true;
                        }
                    }
                    b"content" => {
                        // Handle non-self-closing <content src="..."></content>
                        if let Some(state) = stack.last_mut() {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"src" {
                                    state.src = String::from_utf8_lossy(&attr.value).into_owned();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => match e.local_name().as_ref() {
                b"content" => {
                    if let Some(state) = stack.last_mut() {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"src" {
                                state.src = String::from_utf8_lossy(&attr.value).into_owned();
                            }
                        }
                    }
                }
                b"navPoint" => stack.push(NavState::default()),
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if let Some(state) = stack.last_mut() {
                    if state.in_text {
                        if let Ok(text) = e.unescape() {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                state.label.push_str(trimmed);
                            }
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"text" => {
                    if let Some(state) = stack.last_mut() {
                        state.in_text = false;
                    }
                }
                b"navPoint" => {
                    if let Some(state) = stack.pop() {
                        if !state.label.is_empty() {
                            let clean_src = state
                                .src
                                .split('#')
                                .next()
                                .unwrap_or(&state.src)
                                .to_string();
                            let chapter_index = href_to_index.get(&clean_src).copied().unwrap_or(0);
                            entries.push(TocEntry {
                                label: state.label,
                                chapter_index: chapter_index as u32,
                                play_order: format!("{}", chapter_index + 1), // Default simple play order
                                children: vec![],
                            });
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    entries
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_extract_tag_text() {
        let xml = "<dc:title>My Book Title</dc:title>";
        assert_eq!(extract_tag_text(xml, "dc:title"), Some("My Book Title"));
    }

    #[test]
    fn test_extract_tag_text_decoded_decodes_entities() {
        // The reported bug: an entity-encoded ComicInfo Summary rendered its
        // `&gt;`/`&lt;`/`&amp;` literally. Extraction must decode them.
        let xml = "<Summary>&gt;La Compagnie&lt; A &amp; B &#39;q&#39;</Summary>";
        assert_eq!(
            extract_tag_text_decoded(xml, "Summary").as_deref(),
            Some(">La Compagnie< A & B 'q'")
        );
    }

    #[test]
    fn test_extract_tag_text_decoded_plain_text_unchanged() {
        // Safety: a description with no entities must be byte-for-byte unchanged.
        let xml = "<Summary>Une dark fantasy implacable, sans entités.</Summary>";
        assert_eq!(
            extract_tag_text_decoded(xml, "Summary").as_deref(),
            Some("Une dark fantasy implacable, sans entités.")
        );
    }

    #[test]
    fn test_extract_tag_text_decoded_malformed_falls_back_to_raw() {
        // Safety: a stray unescaped `&` isn't well-formed; decoding must fall
        // back to the original text rather than drop or mangle the value.
        let xml = "<Summary>Rock & Roll</Summary>";
        assert_eq!(
            extract_tag_text_decoded(xml, "Summary").as_deref(),
            Some("Rock & Roll")
        );
    }

    #[test]
    fn test_extract_all_tag_texts() {
        let xml = "<dc:creator>Alice</dc:creator><dc:creator>Bob</dc:creator>";
        let results = extract_all_tag_texts(xml, "dc:creator");
        assert_eq!(results, vec!["Alice", "Bob"]);
    }

    #[test]
    fn test_opf_base_dir() {
        assert_eq!(opf_base_dir("OEBPS/content.opf"), "OEBPS/");
        assert_eq!(opf_base_dir("content.opf"), "");
    }

    #[test]
    fn test_parse_spine() {
        let opf = r#"
        <spine toc="ncx">
            <itemref idref="chapter1"/>
            <itemref idref="chapter2"/>
        </spine>"#;
        let spine = parse_spine_idrefs(opf);
        assert_eq!(spine, vec!["chapter1", "chapter2"]);
    }

    #[test]
    fn test_parse_spine_multiline_attributes() {
        // Validates that multi-line attribute formatting is handled correctly
        let opf = r#"
        <spine toc="ncx">
            <itemref
                idref="chapter1"/>
            <itemref
                idref="chapter2"/>
        </spine>"#;
        let spine = parse_spine_idrefs(opf);
        assert_eq!(spine, vec!["chapter1", "chapter2"]);
    }

    #[test]
    fn test_parse_manifest() {
        let opf = r#"
        <manifest>
            <item id="chapter1" href="ch01.xhtml" media-type="application/xhtml+xml"/>
            <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
        </manifest>"#;
        let manifest = parse_manifest(opf);
        assert_eq!(
            manifest.get("chapter1").map(|i| i.href.as_str()),
            Some("ch01.xhtml")
        );
        assert_eq!(
            manifest.get("ncx").map(|i| i.href.as_str()),
            Some("toc.ncx")
        );
    }

    #[test]
    fn test_parse_manifest_multiline_attributes() {
        // Validates that multi-line attribute formatting is handled correctly
        let opf = r#"
        <manifest>
            <item
                id="chapter1"
                href="ch01.xhtml"
                media-type="application/xhtml+xml"/>
        </manifest>"#;
        let manifest = parse_manifest(opf);
        assert_eq!(
            manifest.get("chapter1").map(|i| i.href.as_str()),
            Some("ch01.xhtml")
        );
    }

    #[test]
    fn test_manifest_nav_properties() {
        let opf = r#"
        <manifest>
            <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
            <item id="ch1" href="ch01.xhtml" media-type="application/xhtml+xml"/>
        </manifest>"#;
        let manifest = parse_manifest(opf);
        let nav_item = manifest.values().find(|item| {
            item.properties
                .as_deref()
                .map(|p| p.split_whitespace().any(|prop| prop == "nav"))
                .unwrap_or(false)
        });
        assert!(nav_item.is_some());
        assert_eq!(nav_item.unwrap().href, "nav.xhtml");
    }

    #[test]
    fn test_find_cover_href_epub3() {
        let opf = r#"
        <manifest>
            <item id="cover-img" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
        </manifest>"#;
        assert_eq!(find_cover_href(opf), Some("images/cover.jpg".to_string()));
    }

    #[test]
    fn test_sanitize_strips_script_tags() {
        let dangerous = r#"<p>Hello world</p><script>alert(1)</script><p>More text</p>"#;
        let sanitized = clean(dangerous);
        assert!(
            !sanitized.contains("<script>"),
            "script tag should be stripped"
        );
        assert!(
            !sanitized.contains("alert(1)"),
            "script content should be stripped"
        );
        assert!(
            sanitized.contains("Hello world"),
            "normal content should be preserved"
        );
        assert!(
            sanitized.contains("More text"),
            "normal content should be preserved"
        );
    }

    #[test]
    fn test_sanitize_strips_inline_event_handlers() {
        let dangerous = r#"<p onmouseover="alert(1)">Text</p><img src="x" onerror="alert(2)"/>"#;
        let sanitized = clean(dangerous);
        assert!(
            !sanitized.contains("onmouseover"),
            "event handler should be stripped"
        );
        assert!(
            !sanitized.contains("onerror"),
            "event handler should be stripped"
        );
        assert!(
            sanitized.contains("Text"),
            "normal content should be preserved"
        );
    }

    #[test]
    fn test_find_cover_href_epub2() {
        let opf = r#"
        <metadata>
            <meta name="cover" content="cover-image"/>
        </metadata>
        <manifest>
            <item id="cover-image" href="cover.jpeg" media-type="image/jpeg"/>
        </manifest>"#;
        assert_eq!(find_cover_href(opf), Some("cover.jpeg".to_string()));
    }

    #[test]
    fn test_sanitize_cover_href_valid_paths() {
        assert_eq!(
            sanitize_cover_href("images/cover.jpg"),
            Some("images/cover.jpg".to_string())
        );
        assert_eq!(
            sanitize_cover_href("cover.png"),
            Some("cover.png".to_string())
        );
        assert_eq!(
            sanitize_cover_href("OEBPS/images/cover.jpeg"),
            Some("OEBPS/images/cover.jpeg".to_string())
        );
        // Relative path that stays within bounds
        assert_eq!(
            sanitize_cover_href("a/b/../cover.jpg"),
            Some("a/b/../cover.jpg".to_string())
        );
    }

    #[test]
    fn test_sanitize_cover_href_rejects_null_bytes() {
        assert_eq!(sanitize_cover_href("cover\0.jpg"), None);
        assert_eq!(sanitize_cover_href("\0"), None);
    }

    #[test]
    fn test_sanitize_cover_href_rejects_absolute_paths() {
        assert_eq!(sanitize_cover_href("/etc/passwd"), None);
        assert_eq!(sanitize_cover_href("\\Windows\\system32"), None);
    }

    #[test]
    fn test_sanitize_cover_href_rejects_windows_drive() {
        assert_eq!(sanitize_cover_href("C:\\cover.jpg"), None);
        assert_eq!(sanitize_cover_href("D:cover.jpg"), None);
    }

    #[test]
    fn test_sanitize_cover_href_rejects_traversal() {
        assert_eq!(sanitize_cover_href("../../etc/passwd"), None);
        assert_eq!(sanitize_cover_href("../secret"), None);
        assert_eq!(sanitize_cover_href(".."), None);
        assert_eq!(sanitize_cover_href("a/../../b"), None);
    }

    #[test]
    fn test_sanitize_cover_href_rejects_empty_resolution() {
        assert_eq!(sanitize_cover_href(""), None);
        assert_eq!(sanitize_cover_href("."), None);
        assert_eq!(sanitize_cover_href("a/.."), None);
    }

    #[test]
    fn test_sanitize_cover_ext_valid_extensions() {
        assert_eq!(sanitize_cover_ext("jpg"), "jpg");
        assert_eq!(sanitize_cover_ext("jpeg"), "jpeg");
        assert_eq!(sanitize_cover_ext("png"), "png");
        assert_eq!(sanitize_cover_ext("gif"), "gif");
        assert_eq!(sanitize_cover_ext("webp"), "webp");
        assert_eq!(sanitize_cover_ext("svg"), "svg");
    }

    #[test]
    fn test_sanitize_cover_ext_case_insensitive() {
        assert_eq!(sanitize_cover_ext("JPG"), "jpg");
        assert_eq!(sanitize_cover_ext("Png"), "png");
        assert_eq!(sanitize_cover_ext("JPEG"), "jpeg");
        assert_eq!(sanitize_cover_ext("WebP"), "webp");
    }

    #[test]
    fn test_sanitize_cover_ext_rejects_dangerous_extensions() {
        assert_eq!(sanitize_cover_ext("exe"), "jpg");
        assert_eq!(sanitize_cover_ext("sh"), "jpg");
        assert_eq!(sanitize_cover_ext("bat"), "jpg");
        assert_eq!(sanitize_cover_ext("js"), "jpg");
        assert_eq!(sanitize_cover_ext("html"), "jpg");
        assert_eq!(sanitize_cover_ext("php"), "jpg");
    }

    #[test]
    fn test_sanitize_cover_ext_rejects_empty_and_unusual() {
        assert_eq!(sanitize_cover_ext(""), "jpg");
        assert_eq!(sanitize_cover_ext(".."), "jpg");
        assert_eq!(sanitize_cover_ext("jpg.exe"), "jpg");
    }

    /// Helper to create a zip archive with an image entry for testing.
    fn create_test_zip_with_image(
        dir: &std::path::Path,
        zip_name: &str,
        image_path: &str,
        image_bytes: &[u8],
    ) -> String {
        create_test_zip_with_images(dir, zip_name, &[(image_path, image_bytes)])
    }

    /// Helper to create a zip archive with multiple image entries.
    fn create_test_zip_with_images(
        dir: &std::path::Path,
        zip_name: &str,
        entries: &[(&str, &[u8])],
    ) -> String {
        let zip_path = dir.join(zip_name);
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (path, bytes) in entries {
            writer.start_file(*path, options).unwrap();
            std::io::Write::write_all(&mut writer, bytes).unwrap();
        }
        writer.finish().unwrap();
        zip_path.to_string_lossy().to_string()
    }

    #[test]
    fn test_rewrite_img_srcs_produces_asset_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "OEBPS/images/photo.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0], // JPEG magic bytes
        );

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = r#"<p>Text</p><img src="../images/photo.jpg"/><p>More</p>"#;
        let result =
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "OEBPS/Text/", &storage, "book1/0");

        assert!(
            result.contains("asset://localhost/"),
            "should contain asset URL, got: {result}"
        );
        assert!(
            !result.contains("data:"),
            "should not contain data URI, got: {result}"
        );
        assert!(
            result.contains("photo.jpg"),
            "should reference the image filename"
        );
        // Verify image was written through storage — list by prefix instead
        // of asserting an exact key shape, so future changes to the key
        // derivation (e.g. adding a hash to disambiguate basenames) don't
        // churn this test.
        use crate::storage::Storage;
        let keys = storage.list("book1/0/").unwrap();
        assert_eq!(keys.len(), 1, "expected one cached image, got: {keys:?}");
        assert!(
            keys[0].ends_with("photo.jpg"),
            "cached key should end with source basename, got: {}",
            keys[0]
        );
    }

    /// A `>` inside an earlier quoted attribute (ammonia leaves it literal, it
    /// does not escape `>` in attribute values) must not fool the tag-boundary
    /// scan: the `src` still has to be rewritten to an asset URL.
    #[test]
    fn test_rewrite_img_srcs_handles_gt_in_earlier_attribute() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "OEBPS/images/photo.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = r#"<img alt="a > b" src="../images/photo.jpg">"#;
        let result =
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "OEBPS/Text/", &storage, "book1/0");

        assert!(
            result.contains("asset://localhost/"),
            "src should be rewritten despite the `>` in alt, got: {result}"
        );
        assert!(
            !result.contains(r#"src="../images/photo.jpg""#),
            "relative src should be gone, got: {result}"
        );
        assert!(
            result.contains(r#"alt="a > b""#),
            "the alt attribute (with its literal >) must be preserved, got: {result}"
        );
    }

    /// Same fragility via a single-quoted attribute value.
    #[test]
    fn test_rewrite_img_srcs_handles_gt_in_single_quoted_attribute() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "OEBPS/images/photo.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = r#"<img alt='a > b' src='../images/photo.jpg'>"#;
        let result =
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "OEBPS/Text/", &storage, "book1/0");

        assert!(
            result.contains("asset://localhost/"),
            "src should be rewritten despite the `>` in single-quoted alt, got: {result}"
        );
    }

    /// Duplicate `src` (ammonia collapses these to one in production, so this
    /// exercises the isolated rewriter as defense-in-depth): the first `src`
    /// is rewritten to an asset URL and the tag stays well-formed.
    #[test]
    fn test_rewrite_img_srcs_handles_duplicate_src() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "OEBPS/images/photo.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = r#"<img src="../images/photo.jpg" src="other.jpg">"#;
        let result =
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "OEBPS/Text/", &storage, "book1/0");

        assert!(
            result.contains("asset://localhost/"),
            "the first src should be rewritten, got: {result}"
        );
        assert!(
            !result.contains(r#"src="../images/photo.jpg""#),
            "the first (relative) src should be gone, got: {result}"
        );
        assert!(
            result.contains(r#"src="other.jpg""#),
            "the second src should be left untouched, got: {result}"
        );
    }

    #[test]
    fn test_find_tag_end_respects_quotes_and_fallbacks() {
        // Plain tag: index just past the first `>`.
        assert_eq!(find_tag_end("<img src=\"x\">rest"), 13);
        // `>` inside a double-quoted value is skipped.
        assert_eq!(find_tag_end("<img alt=\"a > b\">"), 17);
        // `>` inside a single-quoted value is skipped.
        assert_eq!(find_tag_end("<img alt='a > b'>"), 17);
        // No closing `>` at all: fall back to the full length.
        let no_gt = "<img alt=\"unterminated";
        assert_eq!(find_tag_end(no_gt), no_gt.len());
        // A `>` before any quote closes the tag immediately.
        assert_eq!(find_tag_end("<br>"), 4);
        // Multibyte content before the closing `>` keeps byte offsets valid.
        let mb = "<img alt=\"é 文\">x";
        let end = find_tag_end(mb);
        assert_eq!(&mb[..end], "<img alt=\"é 文\">");
    }

    #[test]
    fn test_rewrite_img_srcs_caches_images_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "images/icon.png",
            &[0x89, 0x50, 0x4E, 0x47], // PNG magic bytes
        );

        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();
        let html = r#"<img src="images/icon.png"/>"#;

        // First call: extracts image
        {
            let file = std::fs::File::open(&zip_path).unwrap();
            let mut archive = ZipArchive::new(file).unwrap();
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "book1/0");
        }
        // Look up the cached key via list() instead of hard-coding the
        // shape — the exact key now includes a content-independent hash of
        // the resolved zip path, and hard-coded keys would fight the fix.
        use crate::storage::Storage;
        let keys_after_first = storage.list("book1/0/").unwrap();
        assert_eq!(keys_after_first.len(), 1);
        let cached_key = keys_after_first[0].clone();
        let dest = storage.local_path(&cached_key).unwrap();
        assert!(dest.exists());
        let mtime_first = std::fs::metadata(&dest).unwrap().modified().unwrap();

        // Small delay to ensure filesystem time resolution
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Second call: should skip extraction (file already cached)
        {
            let file = std::fs::File::open(&zip_path).unwrap();
            let mut archive = ZipArchive::new(file).unwrap();
            rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "book1/0");
        }
        let mtime_second = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert_eq!(
            mtime_first, mtime_second,
            "cached file should not be rewritten"
        );
    }

    #[test]
    fn test_rewrite_img_srcs_leaves_external_urls_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(tmp.path(), "test.zip", "img.jpg", &[0xFF]);

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("out")).unwrap();

        let html =
            r#"<img src="https://example.com/photo.jpg"/><img src="data:image/png;base64,abc"/>"#;
        let result = rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "b/0");

        assert_eq!(result, html, "external URLs should be unchanged");
    }

    #[test]
    fn test_rewrite_img_srcs_skips_svg() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(tmp.path(), "test.zip", "icon.svg", b"<svg/>");

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("out")).unwrap();

        let html = r#"<img src="icon.svg"/>"#;
        let result = rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "b/0");

        assert_eq!(result, html, "SVG images should be left as-is");
    }

    #[test]
    fn test_rewrite_img_srcs_leaves_tag_unchanged_when_local_path_errors() {
        // Mock Storage whose put() succeeds but local_path() always errors
        // — simulates a remote backend that accepted a write but could not
        // materialize a local filesystem path. The rewriter must fall back
        // to leaving the <img> tag unchanged rather than emitting a broken
        // asset:// URL.
        use crate::storage::Storage;

        struct NoLocalPathStorage;
        impl Storage for NoLocalPathStorage {
            fn put(&self, _key: &str, _bytes: &[u8]) -> crate::error::CarrelResult<()> {
                Ok(())
            }
            fn get(&self, _key: &str) -> crate::error::CarrelResult<Vec<u8>> {
                Ok(Vec::new())
            }
            fn exists(&self, _key: &str) -> crate::error::CarrelResult<bool> {
                Ok(false)
            }
            fn delete(&self, _key: &str) -> crate::error::CarrelResult<()> {
                Ok(())
            }
            fn list(&self, _prefix: &str) -> crate::error::CarrelResult<Vec<String>> {
                Ok(Vec::new())
            }
            fn size(&self, _key: &str) -> crate::error::CarrelResult<u64> {
                Ok(0)
            }
            fn local_path(&self, key: &str) -> crate::error::CarrelResult<std::path::PathBuf> {
                Err(crate::error::CarrelError::internal(format!(
                    "remote backend cannot resolve {key}"
                )))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_image(
            tmp.path(),
            "test.zip",
            "photo.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = NoLocalPathStorage;

        let html = r#"<img src="photo.jpg"/>"#;
        let result = rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "b/0");

        assert_eq!(
            result, html,
            "tag should be unchanged when local_path cannot resolve"
        );
    }

    #[test]
    fn test_rewrite_img_srcs_missing_image_leaves_tag_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        // Create zip without the referenced image
        let zip_path = create_test_zip_with_image(tmp.path(), "test.zip", "other.jpg", &[0xFF]);

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("out")).unwrap();

        let html = r#"<img src="missing.jpg"/>"#;
        let result = rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "b/0");

        assert_eq!(result, html, "missing images should leave tag unchanged");
    }

    #[test]
    fn test_rewrite_img_srcs_same_basename_different_dirs_coexist() {
        // Two distinct zip entries that share a basename (`cover.png`) but
        // live in different directories must each get a unique storage key
        // and resolve to a different `asset://` URL — otherwise the second
        // write overwrites the first and both `<img>` tags render the wrong
        // image. Regression for the collision flagged during #64 M6 review.
        use crate::storage::Storage;

        let tmp = tempfile::tempdir().unwrap();
        let zip_path = create_test_zip_with_images(
            tmp.path(),
            "test.zip",
            &[
                ("dir_a/cover.png", b"AAAA-distinct-bytes-a"),
                ("dir_b/cover.png", b"BBBB-distinct-bytes-b"),
            ],
        );
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = r#"<img src="dir_a/cover.png"/><img src="dir_b/cover.png"/>"#;
        let result = rewrite_img_srcs_to_asset_urls(html, &mut archive, "", &storage, "book1/0");

        // Two distinct keys land in storage under this chapter's prefix.
        let mut keys = storage.list("book1/0/").unwrap();
        keys.sort();
        assert_eq!(
            keys.len(),
            2,
            "both images must coexist under distinct keys, got: {keys:?}"
        );

        // The bytes at each key match the source — first write wasn't overwritten.
        let bytes_a = storage.get(&keys[0]).unwrap();
        let bytes_b = storage.get(&keys[1]).unwrap();
        assert_ne!(
            bytes_a, bytes_b,
            "distinct source images must produce distinct stored bytes"
        );

        // Both <img> tags get rewritten to asset:// URLs, and the two URLs
        // are distinct (otherwise both would still resolve to the same file).
        let asset_urls: Vec<&str> = result
            .match_indices("asset://localhost/")
            .map(|(i, _)| {
                let rest = &result[i..];
                let end = rest.find('"').unwrap_or(rest.len());
                &rest[..end]
            })
            .collect();
        assert_eq!(
            asset_urls.len(),
            2,
            "both <img> tags should be rewritten, got: {result}"
        );
        assert_ne!(
            asset_urls[0], asset_urls[1],
            "the two <img> tags must resolve to different asset URLs"
        );
    }

    // ---- Word counting tests ----

    #[test]
    fn test_strip_html_tags_basic() {
        assert_eq!(strip_html_tags("<p>Hello world</p>"), " Hello world ");
    }

    #[test]
    fn test_strip_html_tags_nested() {
        assert_eq!(
            strip_html_tags("<div><p>One <strong>two</strong> three</p></div>"),
            "  One  two  three  "
        );
    }

    #[test]
    fn test_strip_html_tags_empty() {
        assert_eq!(strip_html_tags(""), "");
        assert_eq!(strip_html_tags("<br/><hr/>"), "  ");
    }

    #[test]
    fn test_count_words_basic() {
        assert_eq!(count_words("hello world"), 2);
        assert_eq!(count_words("  one  two  three  "), 3);
        assert_eq!(count_words(""), 0);
        assert_eq!(count_words("   "), 0);
    }

    #[test]
    fn test_count_words_from_html() {
        let html = "<p>The quick brown fox jumps over the lazy dog.</p>";
        let text = strip_html_tags(html);
        assert_eq!(count_words(&text), 9);
    }

    // ---- Search tests ----

    #[test]
    fn test_extract_snippet_middle() {
        let text = "The quick brown fox jumps over the lazy dog and runs away fast.";
        let snippet = extract_snippet(text, 16, 3, 15);
        assert!(snippet.contains("fox"));
        assert!(snippet.starts_with("..."));
    }

    #[test]
    fn test_extract_snippet_at_start() {
        let text = "Fox jumps over the lazy dog.";
        let snippet = extract_snippet(text, 0, 3, 15);
        assert!(snippet.contains("Fox"));
        assert!(!snippet.starts_with("..."));
    }

    #[test]
    fn test_extract_snippet_at_end() {
        let text = "The quick brown fox.";
        let snippet = extract_snippet(text, 16, 3, 15);
        assert!(snippet.contains("fox"));
        assert!(!snippet.ends_with("..."));
    }

    #[test]
    fn test_extract_snippet_short_text() {
        let text = "Hello world";
        let snippet = extract_snippet(text, 0, 5, 100);
        assert_eq!(snippet, "Hello world");
    }

    #[test]
    fn test_extract_snippet_multibyte_chars() {
        let text = "Le café est très bon et délicieux.";
        // "café" starts at byte 3 and is 5 bytes (é = 2 bytes)
        let pos = text.find("café").unwrap();
        let snippet = extract_snippet(text, pos, "café".len(), 10);
        assert!(
            snippet.contains("café"),
            "snippet should contain the match: {snippet}"
        );
    }

    #[test]
    fn test_extract_snippet_cjk() {
        let text = "这是一个中文测试句子。";
        let pos = text.find("中文").unwrap();
        let snippet = extract_snippet(text, pos, "中文".len(), 10);
        assert!(
            snippet.contains("中文"),
            "snippet should contain CJK match: {snippet}"
        );
    }

    // ---- Archive bounds at the path-based API boundary ----
    //
    // The `*_from_archive` / `*_from_cache` variants trust their caller to have
    // validated already; the path-based wrappers own the file handle, so bounds
    // enforcement is their job. These tests drive the wrappers only.

    const BOUNDS_CONTAINER_XML: &[u8] = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

    const BOUNDS_CONTENT_OPF: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Bounds Test</dc:title>
    <dc:creator>Bounds Author</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch0" href="ch0.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch0"/>
  </spine>
</package>"#;

    fn bounds_chapter(body: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Ch0</title></head><body>{body}</body></html>"#
        )
        .into_bytes()
    }

    /// Build a structurally valid single-chapter EPUB on disk, with `ch0` as the
    /// chapter body bytes and `extra` appended as additional archive entries.
    fn build_bounds_epub(
        dir: &std::path::Path,
        name: &str,
        ch0: Vec<u8>,
        extra: Vec<(String, Vec<u8>)>,
    ) -> String {
        let zip_path = dir.join(name);
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let deflated = zip::write::SimpleFileOptions::default();
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        writer.start_file("mimetype", stored).unwrap();
        std::io::Write::write_all(&mut writer, b"application/epub+zip").unwrap();

        for (entry_name, bytes) in [
            ("META-INF/container.xml", BOUNDS_CONTAINER_XML.to_vec()),
            ("OEBPS/content.opf", BOUNDS_CONTENT_OPF.to_vec()),
            ("OEBPS/ch0.xhtml", ch0),
        ] {
            writer.start_file(entry_name, deflated).unwrap();
            std::io::Write::write_all(&mut writer, &bytes).unwrap();
        }
        for (entry_name, bytes) in extra {
            writer.start_file(entry_name, deflated).unwrap();
            std::io::Write::write_all(&mut writer, &bytes).unwrap();
        }
        writer.finish().unwrap();
        zip_path.to_string_lossy().into_owned()
    }

    /// Overwrite the declared uncompressed size of `entry` in *both* the local
    /// file header and the central directory, producing an archive whose
    /// metadata lies about how much data an entry decompresses to. Patching both
    /// keeps the two consistent, so the zip crate's header cross-check passes and
    /// the lie actually reaches our code.
    pub(crate) fn forge_declared_size(zip_path: &str, entry: &str, declared: u32) {
        let mut bytes = std::fs::read(zip_path).unwrap();
        let name = entry.as_bytes();
        let mut patched = 0;
        let mut i = 0;
        while i + 46 <= bytes.len() {
            if &bytes[i..i + 4] == b"PK\x03\x04" {
                let nlen = u16::from_le_bytes([bytes[i + 26], bytes[i + 27]]) as usize;
                if bytes.get(i + 30..i + 30 + nlen) == Some(name) {
                    bytes[i + 22..i + 26].copy_from_slice(&declared.to_le_bytes());
                    patched += 1;
                }
            } else if &bytes[i..i + 4] == b"PK\x01\x02" {
                let nlen = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
                if bytes.get(i + 46..i + 46 + nlen) == Some(name) {
                    bytes[i + 24..i + 28].copy_from_slice(&declared.to_le_bytes());
                    patched += 1;
                }
            }
            i += 1;
        }
        assert_eq!(
            patched, 2,
            "expected to patch the local and central header for {entry}"
        );
        std::fs::write(zip_path, bytes).unwrap();
    }

    /// Highly compressible filler — a deflate bomb's payload. `len` bytes on
    /// read, a few KB on disk.
    fn filler(len: usize) -> Vec<u8> {
        vec![b'A'; len]
    }

    #[test]
    fn test_parse_epub_metadata_rejects_entry_over_max_entry_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = build_bounds_epub(
            tmp.path(),
            "oversized.epub",
            bounds_chapter("<p>hello</p>"),
            vec![("OEBPS/big.bin".to_string(), b"tiny".to_vec())],
        );
        forge_declared_size(&path, "OEBPS/big.bin", MAX_ENTRY_SIZE as u32 + 1);

        let err = parse_epub_metadata(&path).unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_get_chapter_content_rejects_entry_over_max_entry_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = build_bounds_epub(
            tmp.path(),
            "oversized.epub",
            bounds_chapter("<p>hello</p>"),
            vec![("OEBPS/big.bin".to_string(), b"tiny".to_vec())],
        );
        forge_declared_size(&path, "OEBPS/big.bin", MAX_ENTRY_SIZE as u32 + 1);
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let err = get_chapter_content(&path, 0, &storage, "book1").unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_parse_epub_metadata_rejects_too_many_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let extra: Vec<(String, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|i| (format!("OEBPS/pad{i:05}.txt"), b"x".to_vec()))
            .collect();
        let path = build_bounds_epub(
            tmp.path(),
            "many.epub",
            bounds_chapter("<p>hello</p>"),
            extra,
        );

        let err = parse_epub_metadata(&path).unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_get_chapter_content_rejects_too_many_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let extra: Vec<(String, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|i| (format!("OEBPS/pad{i:05}.txt"), b"x".to_vec()))
            .collect();
        let path = build_bounds_epub(
            tmp.path(),
            "many.epub",
            bounds_chapter("<p>hello</p>"),
            extra,
        );
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let err = get_chapter_content(&path, 0, &storage, "book1").unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_get_chapter_content_rejects_text_entry_over_text_cap() {
        // Honest declaration: the chapter really is this big, and it stays under
        // MAX_ENTRY_SIZE, so only the tighter text cap can reject it. Peak
        // allocation stays at the cap — the test OOMing is the failure mode.
        let tmp = tempfile::tempdir().unwrap();
        let oversize = (MAX_TEXT_ENTRY_SIZE as usize) + 1024;
        let path = build_bounds_epub(tmp.path(), "bomb.epub", filler(oversize), Vec::new());
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let err = get_chapter_content(&path, 0, &storage, "book1").unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_get_chapter_content_rejects_entry_lying_about_decompressed_size() {
        // Declares 1 KB, decompresses to well over the text cap. The central
        // directory pre-scan cannot catch this — only the bounded read can.
        let tmp = tempfile::tempdir().unwrap();
        let oversize = (MAX_TEXT_ENTRY_SIZE as usize) + 1024;
        let path = build_bounds_epub(tmp.path(), "liar.epub", filler(oversize), Vec::new());
        forge_declared_size(&path, "OEBPS/ch0.xhtml", 1024);
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let err = get_chapter_content(&path, 0, &storage, "book1").unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    /// Body text of roughly `target` bytes, shaped like a real chapter rather
    /// than a run of one byte, so `ammonia::clean` does representative work.
    fn chapter_body_of_size(target: usize) -> String {
        let para = "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do \
                    eiusmod tempor incididunt ut labore et dolore magna aliqua.</p>";
        let mut body = String::with_capacity(target + para.len());
        while body.len() < target {
            body.push_str(para);
        }
        body
    }

    #[test]
    fn test_single_xhtml_book_just_under_text_cap_parses() {
        // Documents the MAX_TEXT_ENTRY_SIZE boundary from below: a 15 MB
        // single-chapter book — the shape a fixed-layout EPUB with base64-inlined
        // images takes — still reads.
        let tmp = tempfile::tempdir().unwrap();
        let body = chapter_body_of_size(15 * 1024 * 1024);
        let path = build_bounds_epub(tmp.path(), "big15.epub", bounds_chapter(&body), Vec::new());
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let html = get_chapter_content(&path, 0, &storage, "book1").unwrap();
        assert!(html.contains("Lorem ipsum"), "chapter should have parsed");
    }

    #[test]
    fn test_single_xhtml_book_over_text_cap_is_rejected() {
        // The same boundary from above: 17 MB is refused. Deliberate cost of the
        // cap — see the note on `MAX_TEXT_ENTRY_SIZE`.
        let tmp = tempfile::tempdir().unwrap();
        let body = chapter_body_of_size(17 * 1024 * 1024);
        let path = build_bounds_epub(tmp.path(), "big17.epub", bounds_chapter(&body), Vec::new());
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let err = get_chapter_content(&path, 0, &storage, "book1").unwrap_err();
        assert!(
            matches!(err, EpubError::LimitExceeded(_)),
            "expected LimitExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_valid_epub_still_parses_after_bounds_enforcement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = build_bounds_epub(
            tmp.path(),
            "good.epub",
            bounds_chapter("<p>hello bounds</p>"),
            Vec::new(),
        );
        let storage = crate::storage::LocalStorage::new(tmp.path().join("images")).unwrap();

        let meta = parse_epub_metadata(&path).unwrap();
        assert_eq!(meta.title, "Bounds Test");
        assert_eq!(meta.author, "Bounds Author");

        let html = get_chapter_content(&path, 0, &storage, "book1").unwrap();
        assert!(
            html.contains("hello bounds"),
            "chapter content should be unchanged: {html}"
        );

        assert_eq!(get_chapter_list(&path).unwrap().len(), 1);
        assert!(extract_cover(&path).unwrap().is_none());
    }

    #[test]
    fn test_remaining_path_entry_points_enforce_archive_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let extra: Vec<(String, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|i| (format!("OEBPS/pad{i:05}.txt"), b"x".to_vec()))
            .collect();
        let path = build_bounds_epub(
            tmp.path(),
            "many.epub",
            bounds_chapter("<p>hello</p>"),
            extra,
        );

        // `ExtractedCover` is not `Debug`, so unwrap_err() is unavailable for it.
        let cover_err = match extract_cover(&path) {
            Err(e) => e,
            Ok(_) => panic!("extract_cover accepted an over-limit archive"),
        };
        for err in [
            get_chapter_list(&path).unwrap_err(),
            cover_err,
            get_toc(&path).unwrap_err(),
        ] {
            assert!(
                matches!(err, EpubError::LimitExceeded(_)),
                "expected LimitExceeded, got {err:?}"
            );
        }
    }
}

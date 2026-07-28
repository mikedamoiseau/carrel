use base64::Engine;
use pdfium_render::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};

use crate::epub;
use crate::error::{FolioError, FolioResult};
use crate::page_cache;
use crate::storage::Storage;

/// Canonical render width used when populating the on-disk page cache.
/// Wider than typical reading viewports so zoomed-in views can downscale
/// rather than re-render, but small enough that 200-page books stay
/// comfortably inside the shared `page-cache/` budget (≈ 200–500 KB JPEG
/// per page at this width).
pub const CACHE_CANONICAL_WIDTH: u32 = 2400;

// ---- PDF text cache ----

/// Maximum number of books whose extracted text we keep in memory.
const TEXT_CACHE_MAX_BOOKS: usize = 5;

static PDF_TEXT_CACHE: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---- Data structures ----

pub struct PdfMeta {
    pub title: String,
    pub author: String,
    pub page_count: u32,
}

// ---- Library path ----

static PDFIUM_LIBRARY_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Called once during app setup to point pdfium at the bundled library.
/// Pass `None` to fall back to the system library search path.
pub fn set_pdfium_library_path(path: Option<PathBuf>) {
    let _ = PDFIUM_LIBRARY_PATH.set(path);
}

/// Check whether pdfium can be loaded. Used at startup so the frontend
/// can disable PDF-related UI when the library is unavailable.
pub fn is_available() -> bool {
    bind_pdfium().is_ok()
}

// ---- Internal helpers ----

/// Global lock serializing every pdfium operation. pdfium (the C library) is
/// not thread-safe, and folio links the crate's default single-threaded
/// bindings, so two renders touching pdfium's global state concurrently is
/// undefined behavior. Renders run on the blocking pool (foreground page loads
/// and the background prerender pass), which can overlap — so each pdfium user
/// holds this for the full bind → use → drop lifetime. The guard protects only
/// `()`, so a panicking holder leaves nothing inconsistent: poison is recovered
/// rather than propagated.
static PDFIUM_RENDER_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// A bound `Pdfium` together with the render lock it holds. Deref lets callers
/// use it exactly like a `Pdfium`.
///
/// Field order is load-bearing: Rust drops struct fields in DECLARATION order,
/// so `pdfium` (and its `FPDF_DestroyLibrary` in `Drop`) runs BEFORE `_guard`
/// releases the lock. Reversing them would unlock before destruction, letting
/// another thread's `FPDF_InitLibrary` overlap this one's teardown — exactly
/// the race the lock exists to prevent.
struct PdfiumSession {
    pdfium: Pdfium,
    _guard: MutexGuard<'static, ()>,
}

impl std::ops::Deref for PdfiumSession {
    type Target = Pdfium;
    fn deref(&self) -> &Pdfium {
        &self.pdfium
    }
}

/// Bind pdfium AND acquire the global render lock, returned together as a
/// [`PdfiumSession`] so the lock is held for the full lifetime of the `Pdfium`
/// (and any documents/pages derived from it), including its destructor. This is
/// the single choke point that serializes all pdfium access.
fn bind_pdfium() -> FolioResult<PdfiumSession> {
    let guard = PDFIUM_RENDER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let bindings = match PDFIUM_LIBRARY_PATH.get().and_then(|p| p.as_deref()) {
        Some(path) => {
            let path_str = path
                .to_str()
                .ok_or_else(|| FolioError::internal("pdfium path is not valid UTF-8"))?;
            Pdfium::bind_to_library(path_str).map_err(|e| {
                FolioError::internal(format!(
                    "failed to load bundled pdfium from {path_str}: {e}"
                ))
            })?
        }
        None => Pdfium::bind_to_system_library().map_err(|e| {
            FolioError::internal(format!(
                "pdfium library not found: {e}. Install the pdfium shared library and ensure it \
                 is on your library path (e.g. DYLD_LIBRARY_PATH on macOS)."
            ))
        })?,
    };
    Ok(PdfiumSession {
        pdfium: Pdfium::new(bindings),
        _guard: guard,
    })
}

fn filename_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string()
}

/// True when `s` is exactly a canonical UUID (`8-4-4-4-12` hex groups).
/// Tools like ImageMagick stamp a bare UUID into the PDF Title; that is
/// never a real book title, so callers fall back to the filename.
fn is_uuid_like(s: &str) -> bool {
    let parts: Vec<&str> = s.trim().split('-').collect();
    let groups = [8usize, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(groups)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// True when `s` looks like a URL. PDF tooling (e.g. ImageMagick) leaks its
/// homepage into the Author field; an author name is never a URL.
fn looks_like_url(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    s.starts_with("http://") || s.starts_with("https://")
}

fn read_metadata_tag(document: &PdfDocument, tag: PdfDocumentMetadataTagType) -> Option<String> {
    let entry: PdfDocumentMetadataTag = document.metadata().get(tag)?;
    let s = entry.value().to_string();
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---- Public API ----

/// Parse a PDF file and return its title, author, and page count.
pub fn import_pdf(path: &str) -> FolioResult<PdfMeta> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;

    let page_count = document.pages().len() as u32;

    // A bare-UUID Title (common in tool-generated PDFs) is junk — fall back
    // to the filename so the library shows something meaningful.
    let title = read_metadata_tag(&document, PdfDocumentMetadataTagType::Title)
        .filter(|t| !is_uuid_like(t))
        .unwrap_or_else(|| filename_stem(path));

    // A URL is never an author name (e.g. ImageMagick leaks its homepage).
    let author = read_metadata_tag(&document, PdfDocumentMetadataTagType::Author)
        .filter(|a| !looks_like_url(a))
        .unwrap_or_default();

    Ok(PdfMeta {
        title,
        author,
        page_count,
    })
}

/// Return the number of pages in a PDF.
pub fn get_page_count(path: &str) -> FolioResult<u32> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;
    Ok(document.pages().len() as u32)
}

/// Render one PDF page to a base64-encoded JPEG data URI.
///
/// `width` is the target pixel width; height is calculated to preserve aspect ratio.
/// Uses JPEG encoding for fast encode times and small transfer sizes.
pub fn get_page_image(path: &str, page_index: u32, width: u32) -> FolioResult<String> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;

    let pages = document.pages();
    if page_index > u16::MAX as u32 {
        return Err(FolioError::invalid(format!(
            "page index {page_index} exceeds maximum supported ({})",
            u16::MAX
        )));
    }
    let page = pages
        .get(page_index as u16)
        .map_err(|e| FolioError::not_found(format!("page {page_index} not found: {e}")))?;

    let config = PdfRenderConfig::new().set_target_width(width as i32);

    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| FolioError::internal(format!("render failed: {e}")))?;

    let img = bitmap.as_image();
    let mut jpeg_bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 90);
    img.write_with_encoder(encoder)
        .map_err(|e| FolioError::internal(format!("JPEG encode failed: {e}")))?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg_bytes);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

/// Render one PDF page to raw JPEG bytes + mime type.
/// Avoids the base64 encode/decode round-trip for web serving.
///
/// `target_width` controls the render resolution. When `None`, falls
/// back to [`DEFAULT_RENDER_WIDTH`] (preserves the legacy 1200 px web
/// default). The caller is responsible for clamping to a sensible
/// upper bound (a 10 000 px request will be honored).
pub fn get_page_image_bytes(
    path: &str,
    page_index: u32,
    target_width: Option<u32>,
) -> FolioResult<(Vec<u8>, &'static str)> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;

    let pages = document.pages();
    if page_index > u16::MAX as u32 {
        return Err(FolioError::invalid(format!(
            "page index {page_index} exceeds maximum supported ({})",
            u16::MAX
        )));
    }
    let page = pages
        .get(page_index as u16)
        .map_err(|e| FolioError::not_found(format!("page {page_index} not found: {e}")))?;

    let width = match target_width {
        Some(0) | None => DEFAULT_RENDER_WIDTH,
        Some(w) => w,
    };
    let config = PdfRenderConfig::new().set_target_width(width as i32);

    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| FolioError::internal(format!("render failed: {e}")))?;

    let img = bitmap.as_image();
    let mut jpeg_bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 90);
    img.write_with_encoder(encoder)
        .map_err(|e| FolioError::internal(format!("JPEG encode failed: {e}")))?;

    Ok((jpeg_bytes, "image/jpeg"))
}

/// Default render width when no `target_width` is supplied. Picked to
/// match the historical web-server fallback resolution.
pub const DEFAULT_RENDER_WIDTH: u32 = 1200;

/// Search result from PDF text search — mirrors epub::SearchResult so the
/// frontend can use the same type for both formats.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfSearchResult {
    pub chapter_index: usize, // page index (reuses "chapter_index" for frontend compat)
    pub snippet: String,
    pub match_offset: usize,
}

const MAX_SEARCH_RESULTS: usize = 200;

/// Version tag for [`PdfTextIndex`]'s on-disk format
/// (`page_cache::read_text_index`/`write_text_index`). Bump this when the
/// schema changes incompatibly — a read whose `version` doesn't match is
/// treated as a miss, forcing a clean re-extract, rather than needing a
/// migration.
pub const TEXT_INDEX_VERSION: u32 = 1;

/// Persisted per-book PDF text index (F-4-6): one full-page text string per
/// page, written to `page-cache/{book_hash}/text-index.json` via the
/// `Storage` trait (see `page_cache::write_text_index`/`read_text_index`).
///
/// `pages[i]` is built by concatenating `page.text()?.chars()` in iteration
/// order (see [`extract_all_page_texts`]) — the same offset space used by
/// search's `match_offset` and, in a later milestone, glyph bounds, so
/// downstream consumers can share one char-offset space per page.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfTextIndex {
    pub version: u32,
    pub page_count: u32,
    pub pages: Vec<String>,
}

/// Extract text from every page of a PDF and return as a Vec (one entry per page).
///
/// Built by iterating `page.text()?.chars()` and concatenating each
/// character's `unicode_char()`, NOT `PdfPageText::all()`. `all()` calls
/// pdfium's bulk `FPDFText_GetBoundedText`, which can inject characters
/// (e.g. synthetic line breaks) that don't correspond 1:1 to the per-char
/// codepoints `chars()` yields — pdfium's own docs note that `len()` (char
/// count) "may differ slightly" from `all().len()`. Search (`match_offset`)
/// and, in a later milestone, glyph bounds both index into this
/// `chars()`-built string, so it must be the single source of truth for
/// page text (see the PDF text epic design doc's "Global Constraints").
///
/// KNOWN LIMITATION: this holds the global pdfium render lock for the whole
/// book (the document borrows the session, so the lock can't be released
/// between pages without reopening the file per page — prohibitively slow over
/// a network mount). A foreground page render can therefore wait for a full
/// text-index build. It's bounded (text extraction is far cheaper than
/// rasterization, runs once per book, and only after the prerender pass), so
/// it's accepted for now; a proper fix would decouple the document lifetime
/// from the lock or throttle the build against foreground renders.
fn extract_all_page_texts(path: &str) -> FolioResult<Vec<String>> {
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;

    let pages = document.pages();
    let page_count = pages.len();
    let mut texts = Vec::with_capacity(page_count as usize);

    for page_idx in 0..page_count {
        let page = pages
            .get(page_idx)
            .map_err(|e| FolioError::not_found(format!("page {page_idx} not found: {e}")))?;
        let text = page.text().map_err(|e| {
            FolioError::internal(format!("failed to extract text from page {page_idx}: {e}"))
        })?;

        let mut page_text = String::new();
        for ch in text.chars().iter() {
            if let Some(c) = ch.unicode_char() {
                page_text.push(c);
            }
        }
        texts.push(page_text);
    }

    Ok(texts)
}

/// Read-only peek at the in-memory `PDF_TEXT_CACHE`, without extracting on miss.
fn peek_cached_page_texts(path: &str) -> FolioResult<Option<Vec<String>>> {
    let cache = PDF_TEXT_CACHE.lock()?;
    Ok(cache.get(path).cloned())
}

/// Populate the in-memory `PDF_TEXT_CACHE` for `path`, evicting the whole
/// cache first if it's at capacity (matches the pre-existing eviction
/// policy in `get_cached_page_texts`).
fn store_cached_page_texts(path: &str, texts: &[String]) -> FolioResult<()> {
    let mut cache = PDF_TEXT_CACHE.lock()?;
    if cache.len() >= TEXT_CACHE_MAX_BOOKS && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(path.to_string(), texts.to_vec());
    Ok(())
}

/// Return cached page texts for a PDF, extracting and caching if needed.
/// Memory-only (no disk index) — kept as the simple two-tier path backing
/// [`search_pdf`]. Session-crossing durability goes through
/// [`resolve_page_texts`]/[`search_pdf_with_storage`] instead.
fn get_cached_page_texts(path: &str) -> FolioResult<Vec<String>> {
    if let Some(texts) = peek_cached_page_texts(path)? {
        return Ok(texts);
    }
    let texts = extract_all_page_texts(path)?;
    store_cached_page_texts(path, &texts)?;
    Ok(texts)
}

/// Text resolution order backing [`search_pdf_with_storage`] (F-4-6):
/// in-memory `PDF_TEXT_CACHE` → persisted `text-index.json` (populating the
/// in-memory layer on hit) → `extract` (memory-only). The extractor is
/// injected so tests can stub pdfium out and assert it's never called on a
/// disk-index hit.
///
/// Read-only with respect to disk: this never writes `text-index.json`.
/// Persisting the index is the background build's job alone (`prepare_pdf`
/// in the command layer), which is the single guarded writer — see that
/// function's doc comment for why the search/resolve path must not also
/// write.
fn resolve_page_texts_with_extractor<F>(
    path: &str,
    storage: &dyn Storage,
    book_hash: &str,
    extract: F,
) -> FolioResult<Vec<String>>
where
    F: Fn(&str) -> FolioResult<Vec<String>>,
{
    if let Some(texts) = peek_cached_page_texts(path)? {
        return Ok(texts);
    }

    if let Some(index) = page_cache::read_text_index(storage, book_hash) {
        store_cached_page_texts(path, &index.pages)?;
        return Ok(index.pages);
    }

    let texts = extract(path)?;
    store_cached_page_texts(path, &texts)?;

    Ok(texts)
}

/// Production entry point for [`resolve_page_texts_with_extractor`], wiring
/// [`extract_all_page_texts`] as the extractor.
pub fn resolve_page_texts(
    path: &str,
    storage: &dyn Storage,
    book_hash: &str,
) -> FolioResult<Vec<String>> {
    resolve_page_texts_with_extractor(path, storage, book_hash, extract_all_page_texts)
}

/// Case-insensitive substring search across already-resolved page texts.
/// Pure and pdfium-free — shared by [`search_pdf`] and
/// [`search_pdf_with_storage`], and directly testable with stubbed page text.
fn search_in_texts(page_texts: &[String], query: &str) -> Vec<PdfSearchResult> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    if query_lower.is_empty() {
        return results;
    }

    for (page_idx, text) in page_texts.iter().enumerate() {
        // Case-fold the page text char-by-char while recording, for every
        // char of the LOWERED string, which original char ordinal / byte
        // offset it came from. This is necessary because `to_lowercase()`
        // is not always 1:1: some codepoints expand to multiple chars
        // (İ -> "i̇"), which would otherwise disconnect a match position in
        // the lowered string from the original text's char/byte offsets.
        let mut lowered = String::new();
        let mut lc_to_orig_char: Vec<usize> = Vec::new();
        let mut lc_to_orig_byte: Vec<usize> = Vec::new();
        for (orig_char_idx, (orig_byte, ch)) in text.char_indices().enumerate() {
            for lc in ch.to_lowercase() {
                lowered.push(lc);
                lc_to_orig_char.push(orig_char_idx);
                lc_to_orig_byte.push(orig_byte);
            }
        }

        let mut from = 0usize;
        while let Some(rel) = lowered[from..].find(&query_lower) {
            let mb = from + rel; // byte offset of the match within `lowered`
            let lc_idx = lowered[..mb].chars().count(); // lowered char index
                                                        // The CHAR ordinal into the ORIGINAL text — the shared offset
                                                        // space search, glyph bounds, and highlight anchors all index
                                                        // into.
            let match_offset = lc_to_orig_char[lc_idx];
            let orig_byte = lc_to_orig_byte[lc_idx];
            let end_lc = lc_idx + query_lower.chars().count();
            let end_byte = if end_lc < lc_to_orig_byte.len() {
                lc_to_orig_byte[end_lc]
            } else {
                text.len()
            };
            let snippet =
                epub::extract_snippet(text, orig_byte, end_byte.saturating_sub(orig_byte), 40);
            results.push(PdfSearchResult {
                chapter_index: page_idx,
                snippet,
                match_offset,
            });
            if results.len() >= MAX_SEARCH_RESULTS {
                return results;
            }
            from = mb + query_lower.len();
        }
    }

    results
}

/// Search all pages of a PDF for a query string (case-insensitive).
/// Returns up to MAX_SEARCH_RESULTS matches with surrounding context snippets.
/// Memory-cache only; see [`search_pdf_with_storage`] for the disk-backed,
/// cross-session variant (F-4-6).
pub fn search_pdf(path: &str, query: &str) -> FolioResult<Vec<PdfSearchResult>> {
    let page_texts = get_cached_page_texts(path)?;
    Ok(search_in_texts(&page_texts, query))
}

/// Full per-page text via the memory-only path (no disk index) — the text
/// equivalent of [`search_pdf`], for callers that don't have a storage
/// handle / book hash. Resolution: in-memory `PDF_TEXT_CACHE` → extract +
/// populate the cache on miss. Index into the returned `Vec<String>` by page,
/// and index a page string by CHAR (Unicode scalar) offset — the same offset
/// space as search's `match_offset` and glyph `off` (the epic offset invariant).
pub fn page_texts_memory(path: &str) -> FolioResult<Vec<String>> {
    get_cached_page_texts(path)
}

/// One page's text from the in-memory `PDF_TEXT_CACHE`, cloning just that page
/// (not the whole book). `None` if the book isn't cached or the index is out
/// of range. Lets the per-page reader avoid cloning every page's string on the
/// hot path — the whole-book resolve is only needed once, to warm the cache
/// (which `prepare_pdf`'s background pass already does per book on open).
pub fn cached_page_text(path: &str, page_index: usize) -> FolioResult<Option<String>> {
    let cache = PDF_TEXT_CACHE.lock()?;
    Ok(cache
        .get(path)
        .and_then(|pages| pages.get(page_index).cloned()))
}

/// Search all pages of a PDF for a query string (case-insensitive), resolving
/// page text via [`resolve_page_texts`] (memory → disk index → extract).
/// A cold session with a persisted `text-index.json` hits the disk layer
/// instead of re-extracting. Never persists — search is read-only w.r.t.
/// disk; see [`resolve_page_texts_with_extractor`].
pub fn search_pdf_with_storage(
    path: &str,
    query: &str,
    storage: &dyn Storage,
    book_hash: &str,
) -> FolioResult<Vec<PdfSearchResult>> {
    let page_texts = resolve_page_texts(path, storage, book_hash)?;
    Ok(search_in_texts(&page_texts, query))
}

/// Drop the in-memory `PDF_TEXT_CACHE` entry and any `GLYPH_CACHE` entries
/// for `path`, if present. Used when a book is removed so neither stale
/// in-memory cache can resurrect a deleted/replaced book's page text or
/// glyph rects (the disk-side `page-cache/{hash}/text-index.json` is
/// cleared separately via [`crate::page_cache::evict_book`]).
pub fn evict_memory_cache(path: &str) {
    if let Ok(mut cache) = PDF_TEXT_CACHE.lock() {
        cache.remove(path);
    }
    if let Ok(mut cache) = GLYPH_CACHE.lock() {
        cache.entries.retain(|((p, _), _)| p != path);
        // Bump the generation so any in-flight `get_page_glyphs` extraction
        // that missed the cache before this eviction refuses to insert its
        // (now possibly stale, e.g. deleted-file) glyphs afterward.
        cache.generation = cache.generation.wrapping_add(1);
    }
}

// ---- Glyph bounds (F-1-4, M2) ----

/// One character's normalized bounding rectangle on a single PDF page,
/// returned on demand by [`get_page_glyphs`]. Never persisted to disk —
/// only page TEXT is durable (see [`PdfTextIndex`]); bounds are cheap to
/// recompute and kept in a small in-memory LRU instead.
///
/// `off` is the character's ordinal into the SAME `chars()`-built page
/// text string produced by [`extract_all_page_texts`]/[`PdfTextIndex`] and
/// consumed by search's `match_offset` — the shared offset space required
/// by the epic's "Global Constraints", so a highlight's `start_offset`/
/// `end_offset` line up with the glyph rects that render it.
///
/// `x`/`y`/`w`/`h` are fractions (0..1) of the page's width/height, with
/// `y` measured top-down (screen orientation) even though pdfium's native
/// origin is bottom-left.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Glyph {
    pub off: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Maximum number of `(path, page_index)` pages whose glyph rects are
/// kept in memory at once. Small — the reader only ever needs the
/// currently visible page(s) and maybe an adjacent prefetch.
const GLYPH_CACHE_MAX_PAGES: usize = 4;

/// Key identifying one page's glyph rects in [`GLYPH_CACHE`].
type GlyphCacheKey = (String, usize);

/// One [`GLYPH_CACHE`] entry: the page it belongs to, plus its glyphs.
type GlyphCacheEntry = (GlyphCacheKey, Vec<Glyph>);

/// In-memory-only LRU of recent pages' glyph rects (most-recently-used at
/// the front) plus a `generation` counter bumped on every eviction. The
/// generation lets a `get_page_glyphs` call that missed the cache and then
/// extracted detect an eviction (book removal) that raced its extraction,
/// and refuse to insert glyphs that may belong to a now-deleted file under
/// a path that could be reused. Never persisted — see [`Glyph`]'s doc.
#[derive(Default)]
struct GlyphCache {
    entries: VecDeque<GlyphCacheEntry>,
    generation: u64,
}

static GLYPH_CACHE: LazyLock<Mutex<GlyphCache>> =
    LazyLock::new(|| Mutex::new(GlyphCache::default()));

/// Look up a page's glyphs, moving it to the front (LRU touch) on a hit.
/// Returns the cached glyphs (or `None`) AND the cache generation observed
/// under the lock, so a caller that misses can pass it to
/// [`glyph_cache_put`] to detect an eviction that raced its extraction.
fn glyph_cache_get(path: &str, page_index: usize) -> FolioResult<(Option<Vec<Glyph>>, u64)> {
    let mut cache = GLYPH_CACHE.lock()?;
    let generation = cache.generation;
    let Some(pos) = cache
        .entries
        .iter()
        .position(|((p, idx), _)| p == path && *idx == page_index)
    else {
        return Ok((None, generation));
    };
    // Safe: `pos` was just found by `position()` above.
    let entry = cache
        .entries
        .remove(pos)
        .expect("position() found this index");
    let value = entry.1.clone();
    cache.entries.push_front(entry);
    Ok((Some(value), generation))
}

/// Insert (or refresh) a page's glyphs, evicting the LRU tail past
/// [`GLYPH_CACHE_MAX_PAGES`]. Rejected (returns `false`, inserts nothing) if
/// the cache generation changed since `expected_generation` was captured on
/// the miss — i.e. an eviction (book removal) raced this extraction, so
/// these glyphs may be from a now-deleted file and must not be cached under
/// a path that could be reused.
fn glyph_cache_put(
    path: &str,
    page_index: usize,
    glyphs: Vec<Glyph>,
    expected_generation: u64,
) -> FolioResult<bool> {
    let mut cache = GLYPH_CACHE.lock()?;
    if cache.generation != expected_generation {
        return Ok(false);
    }
    cache
        .entries
        .retain(|((p, idx), _)| !(p == path && *idx == page_index));
    cache
        .entries
        .push_front(((path.to_string(), page_index), glyphs));
    while cache.entries.len() > GLYPH_CACHE_MAX_PAGES {
        cache.entries.pop_back();
    }
    Ok(true)
}

/// Reference render width used to build the [`PdfRenderConfig`] passed to
/// `PdfPage::points_to_pixels` in [`get_page_glyphs`]. The config is shaped
/// the same way (`set_target_width`, no explicit rotation) as
/// [`get_page_image_bytes`]'s, so `points_to_pixels` — which pdfium-render
/// derives from the identical `PdfRenderConfig::apply_to_page` settings used
/// by `render_with_config` — maps through the exact clipping/scale/rotate
/// transform the rendered page image uses (origin, CropBox, and intrinsic
/// rotation all resolved inside pdfium).
///
/// `points_to_pixels` returns INTEGER device pixels, so the normalized 0..1
/// fractions carry a quantization error of at most one reference pixel
/// (1 / this width). This width is therefore chosen to be at least as large
/// as the largest width the reader ever renders a page at
/// (`CACHE_CANONICAL_WIDTH` = 2400, and higher on deep zoom), so the glyph
/// grid is never coarser than the displayed image's own pixels: any glyph
/// that occupies ≥1 pixel in the rendered page also spans ≥1 reference pixel
/// and cannot collapse to zero area. Sub-reference-pixel glyphs (sub-point
/// text, invisible at any real display size) may still collapse — acceptable,
/// as they can't be seen or selected regardless.
const GLYPH_RENDER_REFERENCE_WIDTH: i32 = 12000;

/// Output bitmap pixel dimensions pdfium's `apply_to_page` produces for a
/// target-width-only render config: width is the target, height is
/// aspect-locked. Both are ROUNDED to whole pixels — `points_to_pixels`
/// maps glyph corners into these same rounded dimensions, so normalization
/// must divide by them (dividing by the unrounded `page_h * scale` would
/// vertically displace glyphs when the scaled height is fractional).
/// Returns `None` for a degenerate page (non-finite or non-positive
/// dimension), so the caller can reject it rather than divide by zero.
/// Pure — directly unit-testable.
fn render_output_dims(page_w: f32, page_h: f32, target_width: i32) -> Option<(f32, f32)> {
    if !page_w.is_finite() || !page_h.is_finite() || page_w <= 0.0 || page_h <= 0.0 {
        return None;
    }
    let scale = target_width as f32 / page_w;
    let out_w = (page_w * scale).round();
    let out_h = (page_h * scale).round();
    if out_w <= 0.0 || out_h <= 0.0 {
        return None;
    }
    Some((out_w, out_h))
}

/// Normalize a device-PIXEL bounding box (as returned by
/// `PdfPage::points_to_pixels`) into a [`Glyph`]'s 0..1 page-fraction rect.
/// Pure — no pdfium calls — so it's directly unit-testable without a real
/// PDF.
///
/// `points_to_pixels`'s output is already top-down/screen-oriented (device
/// pixel (0,0) is the rendered bitmap's top-left corner), unlike raw PDF
/// user-space (bottom-left origin) — so, unlike the old page-points-based
/// normalization this replaces, no manual y-flip is needed here, and no
/// assumption about the page's origin or rotation is baked in: both are
/// already resolved by whatever pixel values the caller passed in.
///
/// Clamped on BOTH sides to `[0, 1]` — a glyph whose mapped pixels fall
/// even partially outside the rendered bitmap (float noise, antialiasing
/// overshoot) must not produce a rect that hangs off the page in either
/// direction.
fn normalize_glyph_pixels(
    off: u32,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
    out_w: f32,
    out_h: f32,
) -> Glyph {
    let x0 = (min_x / out_w).clamp(0.0, 1.0);
    let y0 = (min_y / out_h).clamp(0.0, 1.0);
    let x1 = (max_x / out_w).clamp(0.0, 1.0);
    let y1 = (max_y / out_h).clamp(0.0, 1.0);
    Glyph {
        off,
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

/// Map one char's `loose_bounds()` (PDF user-space points) through
/// `PdfPage::points_to_pixels` and normalize the resulting device-pixel
/// bounding box into a [`Glyph`]. Not pure — makes pdfium calls — see
/// [`normalize_glyph_pixels`] for the pure, tested math.
///
/// A conversion failure on any corner (like a `loose_bounds()` failure)
/// falls back to a zero-area glyph rather than propagating an error, so a
/// single bad char can't fail the whole page's glyph list.
fn glyph_from_bounds(
    page: &PdfPage,
    config: &PdfRenderConfig,
    off: u32,
    rect: PdfRect,
    out_w: f32,
    out_h: f32,
) -> Glyph {
    let corners = [
        (rect.left(), rect.bottom()),
        (rect.left(), rect.top()),
        (rect.right(), rect.bottom()),
        (rect.right(), rect.top()),
    ];

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in corners {
        let (px, py) = match page.points_to_pixels(x, y, config) {
            Ok(p) => p,
            Err(_) => {
                return Glyph {
                    off,
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                }
            }
        };
        min_x = min_x.min(px as f32);
        max_x = max_x.max(px as f32);
        min_y = min_y.min(py as f32);
        max_y = max_y.max(py as f32);
    }

    normalize_glyph_pixels(off, min_x, min_y, max_x, max_y, out_w, out_h)
}

/// Return normalized, top-down glyph rectangles for one page of a PDF,
/// keyed by `off` — the char ordinal into the SAME `chars()`-built text
/// string as [`extract_all_page_texts`]/[`PdfTextIndex`]/search's
/// `match_offset` (see the epic's offset invariant). Computed on demand;
/// never persisted (see [`Glyph`]'s doc comment). Recent pages are kept
/// in a small in-memory LRU ([`GLYPH_CACHE`]).
///
/// A char whose `unicode_char()` is `None` is skipped entirely — no
/// glyph, no `off` slot — mirroring [`extract_all_page_texts`], which
/// doesn't push it to the page text either. A char whose bounds lookup
/// fails still consumes an `off` slot (pushed as a zero-area glyph) so
/// `off` never desyncs from the text.
pub fn get_page_glyphs(path: &str, page_index: usize) -> FolioResult<Vec<Glyph>> {
    let (cached, generation) = glyph_cache_get(path, page_index)?;
    if let Some(glyphs) = cached {
        return Ok(glyphs);
    }

    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| FolioError::invalid(format!("failed to open PDF: {e}")))?;

    if page_index > u16::MAX as usize {
        return Err(FolioError::invalid(format!(
            "page index {page_index} exceeds maximum supported ({})",
            u16::MAX
        )));
    }
    let pages = document.pages();
    let page = pages
        .get(page_index as u16)
        .map_err(|e| FolioError::not_found(format!("page {page_index} not found: {e}")))?;

    let text = page.text().map_err(|e| {
        FolioError::internal(format!(
            "failed to extract text from page {page_index}: {e}"
        ))
    })?;

    // Same config shape as `get_page_image_bytes` (see
    // `GLYPH_RENDER_REFERENCE_WIDTH`'s doc comment) so `points_to_pixels`
    // maps through the identical transform the rendered page image uses.
    let config = PdfRenderConfig::new().set_target_width(GLYPH_RENDER_REFERENCE_WIDTH);
    // `PdfRenderConfig::apply_to_page` (which computes the output bitmap's
    // pixel dimensions) is private to pdfium-render, so replicate its
    // aspect-locked, target-width-only scaling formula here to get the same
    // output width/height that `points_to_pixels` maps into — including its
    // rounding to whole pixels (see `render_output_dims`).
    let (out_w, out_h) = render_output_dims(
        page.width().value,
        page.height().value,
        GLYPH_RENDER_REFERENCE_WIDTH,
    )
    .ok_or_else(|| FolioError::invalid(format!("page {page_index} has invalid dimensions")))?;

    let mut glyphs = Vec::new();
    let mut counter: u32 = 0;
    for ch in text.chars().iter() {
        if ch.unicode_char().is_none() {
            continue;
        }
        let glyph = match ch.loose_bounds() {
            Ok(rect) => glyph_from_bounds(&page, &config, counter, rect, out_w, out_h),
            Err(_) => Glyph {
                off: counter,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        };
        glyphs.push(glyph);
        counter += 1;
    }

    // Insert only if no eviction (book removal) raced this extraction; the
    // freshly computed glyphs are still returned to this caller either way.
    let _ = glyph_cache_put(path, page_index, glyphs.clone(), generation)?;
    Ok(glyphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn temp_storage() -> (TempDir, LocalStorage) {
        let dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();
        (dir, storage)
    }

    /// Pins the offset invariant (epic "Global Constraints"): `match_offset`
    /// must equal the index of the query substring inside the page's
    /// `chars()`-built text string — the same offset space search, and
    /// later glyph bounds, both index into. Uses stubbed page text (no real
    /// pdfium) since `search_in_texts` is pure.
    #[test]
    fn test_page_text_from_chars_matches_search_offset() {
        let pages = vec!["hello world".to_string(), "goodbye cruel world".to_string()];

        let results = search_in_texts(&pages, "world");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].chapter_index, 0);
        assert_eq!(results[0].match_offset, pages[0].find("world").unwrap());
        assert_eq!(results[1].chapter_index, 1);
        assert_eq!(results[1].match_offset, pages[1].find("world").unwrap());
    }

    /// Multibyte text before the match: `match_offset` must be the CHAR
    /// ordinal (é=0, x=1, space=2, n=3), not the byte offset (which would be
    /// 4, since é is 2 bytes in UTF-8).
    #[test]
    fn test_search_match_offset_is_char_ordinal_with_multibyte_prefix() {
        let pages = vec!["éx needle".to_string()];

        let results = search_in_texts(&pages, "needle");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_offset, 3);
    }

    /// Case-folding a Turkish capital dotted I (İ) expands to two chars
    /// ("i̇") under `to_lowercase()`. The lowered-string match position must
    /// still be mapped back to the correct ORIGINAL char ordinal (İ=0, n=1),
    /// not the (larger) lowered-string position.
    #[test]
    fn test_search_match_offset_survives_case_fold_expansion() {
        let pages = vec!["İneedle".to_string()];

        let results = search_in_texts(&pages, "needle");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_offset, 1);
    }

    #[test]
    fn test_search_uses_disk_index_without_extracting() {
        let (_d, storage) = temp_storage();
        let book_hash = "hash-cold-session";
        let index = PdfTextIndex {
            version: TEXT_INDEX_VERSION,
            page_count: 1,
            pages: vec!["needle in a haystack".to_string()],
        };
        page_cache::write_text_index(&storage, book_hash, &index).unwrap();

        let extractor_calls = AtomicUsize::new(0);
        let extractor = |_path: &str| -> FolioResult<Vec<String>> {
            extractor_calls.fetch_add(1, Ordering::SeqCst);
            Err(FolioError::internal("extractor should not be called"))
        };

        let texts = resolve_page_texts_with_extractor(
            "test_search_uses_disk_index_without_extracting.pdf",
            &storage,
            book_hash,
            extractor,
        )
        .expect("should resolve from the disk index");

        let results = search_in_texts(&texts, "needle");

        assert_eq!(
            extractor_calls.load(Ordering::SeqCst),
            0,
            "extractor must not run on a disk-index hit"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_offset, texts[0].find("needle").unwrap());
    }

    #[test]
    fn filename_stem_normal_path() {
        assert_eq!(filename_stem("/home/user/docs/book.pdf"), "book");
    }

    #[test]
    fn filename_stem_no_extension() {
        assert_eq!(filename_stem("/home/user/docs/readme"), "readme");
    }

    #[test]
    fn filename_stem_multiple_dots() {
        assert_eq!(filename_stem("/path/to/my.great.book.pdf"), "my.great.book");
    }

    #[test]
    fn filename_stem_just_filename() {
        assert_eq!(filename_stem("document.pdf"), "document");
    }

    #[test]
    fn filename_stem_empty_string() {
        // Empty path has no stem — should fall back to "Unknown"
        assert_eq!(filename_stem(""), "Unknown");
    }

    #[test]
    fn is_uuid_like_matches_canonical_uuid() {
        assert!(is_uuid_like("e86d890e-1288-4044-98fd-d0e50be373f9"));
        assert!(is_uuid_like("  742ae232-d411-4c1f-aace-84dedc9a4cb8  "));
    }

    #[test]
    fn is_uuid_like_rejects_real_titles() {
        assert!(!is_uuid_like("Wunderwaffen - T21 - Starjet"));
        assert!(!is_uuid_like("Dune"));
        assert!(!is_uuid_like("")); // empty
        assert!(!is_uuid_like("e86d890e-1288-4044-98fd-d0e50be373f9-extra")); // 6 groups
        assert!(!is_uuid_like("g86d890e-1288-4044-98fd-d0e50be373f9")); // non-hex
    }

    #[test]
    fn looks_like_url_matches_urls() {
        assert!(looks_like_url("https://imagemagick.org"));
        assert!(looks_like_url("HTTP://Example.com"));
        assert!(looks_like_url("  https://x.io "));
    }

    #[test]
    fn looks_like_url_rejects_names() {
        assert!(!looks_like_url("Frank Herbert"));
        assert!(!looks_like_url("O'Reilly"));
        assert!(!looks_like_url(""));
    }

    /// `points_to_pixels`'s output is already top-down (device pixel (0,0)
    /// is the bitmap's top-left corner) — a glyph mapped to a SMALLER
    /// device `y` must yield a smaller normalized `y` than one mapped to a
    /// LARGER device `y`. Guards against reintroducing a manual bottom-left
    /// -> top-down flip, which would invert this and, unlike the old
    /// page-points-based version, would be wrong here since the input is
    /// already screen-oriented.
    #[test]
    fn test_normalize_glyph_pixels_top_down() {
        let out_w = 600.0;
        let out_h = 800.0;

        let top_char = normalize_glyph_pixels(0, 50.0, 20.0, 60.0, 40.0, out_w, out_h);
        let bottom_char = normalize_glyph_pixels(1, 50.0, 770.0, 60.0, 790.0, out_w, out_h);

        assert!(
            top_char.y < bottom_char.y,
            "top_char.y={} should be < bottom_char.y={}",
            top_char.y,
            bottom_char.y
        );
    }

    #[test]
    fn test_normalize_glyph_pixels_within_unit_range() {
        let out_w = 600.0;
        let out_h = 800.0;
        let eps = 1e-4;

        let glyph = normalize_glyph_pixels(0, 50.0, 700.0, 80.0, 720.0, out_w, out_h);

        assert!((0.0..=1.0).contains(&glyph.x), "x={}", glyph.x);
        assert!((0.0..=1.0).contains(&glyph.y), "y={}", glyph.y);
        assert!(glyph.x + glyph.w <= 1.0 + eps, "x+w={}", glyph.x + glyph.w);
        assert!(glyph.y + glyph.h <= 1.0 + eps, "y+h={}", glyph.y + glyph.h);
    }

    /// Regression for BLOCKING finding 1 (non-zero page origin): the old
    /// code divided the raw PDF-point `left` by `page.width()`, which is
    /// wrong whenever the visible box's origin isn't (0,0) — e.g. a
    /// MediaBox/CropBox of `[100,100,500,700]` would place a box-left glyph
    /// (PDF point x=100) at fraction 100/500=0.2 instead of 0. Here the
    /// input is already a device-PIXEL bounding box (as `points_to_pixels`
    /// would produce for such a page, since pdfium's page-to-device
    /// transform maps the visible box's left edge to pixel 0 regardless of
    /// its PDF-point origin), so a box-left glyph must map to x≈0.
    #[test]
    fn test_normalize_glyph_pixels_box_left_maps_to_zero() {
        let out_w = 400.0;
        let out_h = 600.0;

        let glyph = normalize_glyph_pixels(0, 0.0, 100.0, 10.0, 120.0, out_w, out_h);

        assert!(glyph.x.abs() < 1e-4, "x={} should be ≈0", glyph.x);
    }

    /// A glyph whose mapped pixels span the ENTIRE rendered box (or extend
    /// past it, e.g. antialiasing/float noise) must clamp on BOTH sides —
    /// the old `.max(0.0)`-only clamp left the upper side unbounded.
    #[test]
    fn test_normalize_glyph_pixels_clamps_both_sides() {
        let out_w = 400.0;
        let out_h = 600.0;

        let glyph = normalize_glyph_pixels(0, -5.0, -3.0, 410.0, 605.0, out_w, out_h);

        assert!((0.0..=1.0).contains(&glyph.x), "x={}", glyph.x);
        assert!((0.0..=1.0).contains(&glyph.y), "y={}", glyph.y);
        assert!((0.0..=1.0).contains(&glyph.w), "w={}", glyph.w);
        assert!((0.0..=1.0).contains(&glyph.h), "h={}", glyph.h);
        assert!(glyph.x + glyph.w <= 1.0 + 1e-4, "x+w={}", glyph.x + glyph.w);
        assert!(glyph.y + glyph.h <= 1.0 + 1e-4, "y+h={}", glyph.y + glyph.h);
    }

    /// Rotation-adjacent case: the pure normalization step must not assume
    /// a portrait aspect ratio — a rotated page renders into a bitmap whose
    /// width/height are swapped relative to the page's own (unrotated)
    /// `page.width()`/`page.height()`. Feeding swapped `out_w`/`out_h` must
    /// still produce a correctly-proportioned, in-range rect: the function
    /// has no orientation baked in, only whatever pixel dims it's given
    /// (which, for a real rotated PDF, `points_to_pixels` — going through
    /// pdfium's own page-to-device transform — would supply correctly; that
    /// end-to-end behavior isn't testable here without a real pdfium
    /// binary, see the M2 report).
    #[test]
    fn test_normalize_glyph_pixels_orientation_independent() {
        // A landscape-rendered bitmap (e.g. a portrait page rotated 90°).
        let out_w = 800.0;
        let out_h = 600.0;

        let glyph = normalize_glyph_pixels(0, 100.0, 50.0, 150.0, 80.0, out_w, out_h);

        assert!((0.0..=1.0).contains(&glyph.x), "x={}", glyph.x);
        assert!((0.0..=1.0).contains(&glyph.y), "y={}", glyph.y);
        assert!((glyph.x - 100.0 / out_w).abs() < 1e-4);
        assert!((glyph.y - 50.0 / out_h).abs() < 1e-4);
        assert!((glyph.w - 50.0 / out_w).abs() < 1e-4);
        assert!((glyph.h - 30.0 / out_h).abs() < 1e-4);
    }

    /// A bogus path can never resolve to a real PDF, so `get_page_glyphs`
    /// must error rather than panic — covers the error-return path without
    /// requiring a real pdfium binary (unavailable in this test
    /// environment; see the M2 report for what this does/doesn't exercise).
    #[test]
    fn test_get_page_glyphs_nonexistent_file_errors() {
        let result = get_page_glyphs("test_get_page_glyphs_nonexistent_file_errors.pdf", 0);
        assert!(result.is_err());
    }

    /// Regression for BLOCKING finding 2: `evict_memory_cache` used to clear
    /// only `PDF_TEXT_CACHE`, leaving `GLYPH_CACHE` entries for the evicted
    /// path stale — a book re-imported/replaced at the same path could
    /// serve glyph rects computed from the OLD file. `evict_memory_cache`
    /// must also purge every `GLYPH_CACHE` entry for that path.
    #[test]
    fn test_evict_memory_cache_purges_glyph_cache() {
        let path = "test_evict_memory_cache_purges_glyph_cache.pdf";
        let glyphs = vec![Glyph {
            off: 0,
            x: 0.1,
            y: 0.1,
            w: 0.05,
            h: 0.05,
        }];
        let (_, gen) = glyph_cache_get(path, 0).unwrap();
        assert!(glyph_cache_put(path, 0, glyphs, gen).unwrap());
        assert!(glyph_cache_get(path, 0).unwrap().0.is_some());

        evict_memory_cache(path);

        assert!(glyph_cache_get(path, 0).unwrap().0.is_none());
    }

    /// `render_output_dims` must round the aspect-locked height to whole
    /// pixels, matching pdfium's apply_to_page (which points_to_pixels maps
    /// into). 850x1099 at target 1000 → height 1099*1000/850 = 1292.94 → 1293.
    #[test]
    fn test_render_output_dims_rounds_height() {
        let (w, h) = render_output_dims(850.0, 1099.0, 1000).unwrap();
        assert_eq!(w, 1000.0);
        assert_eq!(h, 1293.0);
    }

    #[test]
    fn test_render_output_dims_rejects_degenerate() {
        assert!(render_output_dims(0.0, 100.0, 1000).is_none());
        assert!(render_output_dims(100.0, 0.0, 1000).is_none());
        assert!(render_output_dims(f32::NAN, 100.0, 1000).is_none());
        assert!(render_output_dims(100.0, f32::INFINITY, 1000).is_none());
    }

    /// Race guard: if an eviction (book removal) lands between a
    /// `get_page_glyphs` cache miss and its insert, the insert must be
    /// rejected so glyphs from a now-deleted file can't be cached under a
    /// path that may be reused. Simulated with the captured generation.
    #[test]
    fn test_glyph_cache_put_rejected_after_eviction_bumps_generation() {
        let path = "test_glyph_cache_put_rejected_after_eviction.pdf";
        let glyphs = vec![Glyph {
            off: 0,
            x: 0.1,
            y: 0.1,
            w: 0.05,
            h: 0.05,
        }];

        // Miss: capture the generation, as get_page_glyphs does before it
        // starts extracting.
        let (miss, gen) = glyph_cache_get(path, 0).unwrap();
        assert!(miss.is_none());

        // An eviction races the (simulated) extraction, bumping generation.
        evict_memory_cache(path);

        // The stale insert is rejected; nothing is cached.
        assert!(!glyph_cache_put(path, 0, glyphs, gen).unwrap());
        assert!(glyph_cache_get(path, 0).unwrap().0.is_none());
    }
}

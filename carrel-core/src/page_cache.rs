use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::sync::OnceLock;
use zip::ZipArchive;

use crate::error::{CarrelError, CarrelResult};
use crate::models::BookFormat;
use crate::storage::Storage;

/// Enable with: CARREL_DEBUG_PAGES=1
/// Prints to stderr so it shows in the terminal running `npm run tauri dev`.
pub fn page_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CARREL_DEBUG_PAGES").unwrap_or_default() == "1")
}

#[macro_export]
macro_rules! page_dbg {
    ($($arg:tt)*) => {
        if $crate::page_cache::page_debug_enabled() {
            eprintln!("[page-load] {}", format!($($arg)*));
        }
    };
}

pub use page_dbg;

const MAX_CACHED_BOOKS: usize = 20;
pub const DEFAULT_MAX_CACHE_SIZE_MB: u64 = 500;
const MAX_AGE_DAYS: u64 = 30;

/// Top-level key prefix under which every page-cache artifact lives.
/// `{CACHE_PREFIX}{book_hash}/manifest.json` + `{CACHE_PREFIX}{book_hash}/{NNN}.{ext}`.
const CACHE_PREFIX: &str = "page-cache/";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

fn default_cache_format() -> BookFormat {
    // Legacy comic manifests (pre-PDF-cache) lacked this field.
    // CBZ is a safe default: CBR manifests already carry
    // CBR-specific filenames inside `pages`, so the same comic
    // read path serves both at runtime.
    BookFormat::Cbz
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheManifest {
    pub book_id: String,
    pub book_hash: String,
    pub page_count: u32,
    pub total_size_bytes: u64,
    pub extracted_at: String,
    pub last_accessed: String,
    pub pages: Vec<String>,
    /// Distinguishes comic manifests (dense `pages` populated with
    /// archive entry names) from PDF manifests (empty `pages`,
    /// filenames derived from page index). Defaulted via a named
    /// function so we do not need to declare a global
    /// `Default for BookFormat` impl.
    #[serde(default = "default_cache_format")]
    pub format: BookFormat,
    /// `Some(2400)` for PDF, `None` for comic (which caches archive
    /// bytes as-is and lets the resize helper clamp on read).
    #[serde(default)]
    pub canonical_width: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_size_bytes: u64,
    pub book_count: usize,
    pub books: Vec<CacheBookInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheBookInfo {
    pub book_id: String,
    pub book_hash: String,
    pub size_bytes: u64,
    pub page_count: u32,
    pub last_accessed: String,
}

// ---------------------------------------------------------------------------
// Key helpers
// ---------------------------------------------------------------------------

fn manifest_key(book_hash: &str) -> String {
    format!("{CACHE_PREFIX}{book_hash}/manifest.json")
}

fn text_index_key(book_hash: &str) -> String {
    format!("{CACHE_PREFIX}{book_hash}/text-index.json")
}

fn page_key(book_hash: &str, page_name: &str) -> String {
    format!("{CACHE_PREFIX}{book_hash}/{page_name}")
}

fn book_prefix(book_hash: &str) -> String {
    format!("{CACHE_PREFIX}{book_hash}/")
}

// ---------------------------------------------------------------------------
// Manifest I/O
// ---------------------------------------------------------------------------

pub fn read_manifest(storage: &dyn Storage, book_hash: &str) -> Option<CacheManifest> {
    let bytes = storage.get(&manifest_key(book_hash)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_manifest(
    storage: &dyn Storage,
    book_hash: &str,
    manifest: &CacheManifest,
) -> CarrelResult<()> {
    let json = serde_json::to_string_pretty(manifest)?;
    storage.put(&manifest_key(book_hash), json.as_bytes())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// PDF text index I/O (F-4-6)
// ---------------------------------------------------------------------------

/// Read the persisted PDF text index for `book_hash`. Returns `None` when
/// absent, unparseable, or written by a different
/// [`crate::pdf::TEXT_INDEX_VERSION`] — any of those is treated as a miss so
/// the caller re-extracts cleanly rather than needing a migration.
pub fn read_text_index(storage: &dyn Storage, book_hash: &str) -> Option<crate::pdf::PdfTextIndex> {
    let bytes = storage.get(&text_index_key(book_hash)).ok()?;
    let index: crate::pdf::PdfTextIndex = serde_json::from_slice(&bytes).ok()?;
    if index.version != crate::pdf::TEXT_INDEX_VERSION {
        return None;
    }
    Some(index)
}

/// Persist the PDF text index for `book_hash`, atomically (temp-file +
/// rename via `Storage::put`) — same discipline as [`write_manifest`].
pub fn write_text_index(
    storage: &dyn Storage,
    book_hash: &str,
    index: &crate::pdf::PdfTextIndex,
) -> CarrelResult<()> {
    let json = serde_json::to_string_pretty(index)?;
    storage.put(&text_index_key(book_hash), json.as_bytes())
}

// ---------------------------------------------------------------------------
// Image helpers
// ---------------------------------------------------------------------------

fn file_extension(name: &str) -> &str {
    if let Some(pos) = name.rfind('.') {
        &name[pos..]
    } else {
        ".jpg"
    }
}

fn mime_for_page_name(name: &str) -> &'static str {
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

// ---------------------------------------------------------------------------
// Comic extraction (CBZ / CBR)
//
// The cache stores each page under an index-derived key (`{idx:03}{ext}`).
// The `idx` is a position into the *canonical* page ordering — the exact
// same filter and sort the on-demand readers (`cbz`/`cbr`) use. This is
// load-bearing: `get_comic_page_bytes` serves a cached page when present
// and otherwise falls back to a direct archive read by the same index, so
// the two must agree or a fallback read would return the wrong page.
// ---------------------------------------------------------------------------

/// Canonical sorted page-entry names for a comic archive, delegating to the
/// on-demand readers so cache indices and fallback indices never diverge.
fn comic_page_names(format: &BookFormat, file_path: &str) -> CarrelResult<Vec<String>> {
    match format {
        BookFormat::Cbz => crate::cbz::collect_page_names(file_path),
        BookFormat::Cbr => crate::cbr::collect_page_names(file_path),
        other => Err(CarrelError::invalid(format!(
            "comic_page_names not supported for {other:?}"
        ))),
    }
}

/// Cache filename for the page at `idx`, deriving the (lowercased)
/// extension from the source archive entry name.
fn comic_page_filename(idx: usize, src_name: &str) -> String {
    format!("{:03}{}", idx, file_extension(src_name).to_lowercase())
}

/// Extract the page indices in `want` from a comic archive into the cache,
/// storing each at `{idx:03}{ext}`. `names` must be the canonical list from
/// [`comic_page_names`]. `on_page(idx)` is invoked after each successful
/// write. Returns the total bytes written across the extracted subset.
///
/// CBZ uses random access by entry name; CBR streams a single processing
/// pass and stops early once every wanted page has been read.
fn extract_comic_subset<F: FnMut(usize)>(
    storage: &dyn Storage,
    book_hash: &str,
    format: &BookFormat,
    file_path: &str,
    names: &[String],
    want: &BTreeSet<usize>,
    mut on_page: F,
) -> CarrelResult<u64> {
    if want.is_empty() {
        return Ok(0);
    }
    match format {
        BookFormat::Cbz => {
            let file = std::fs::File::open(file_path)
                .map_err(|e| CarrelError::io(format!("Failed to open CBZ: {e}")))?;
            let mut archive = ZipArchive::new(file)
                .map_err(|e| CarrelError::invalid(format!("Invalid CBZ archive: {e}")))?;

            let mut total_size: u64 = 0;
            for &idx in want {
                let name = &names[idx];
                let mut entry = archive.by_name(name).map_err(|e| {
                    CarrelError::invalid(format!("Failed to read entry {name}: {e}"))
                })?;
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| CarrelError::io(format!("Failed to extract {name}: {e}")))?;

                let page_filename = comic_page_filename(idx, name);
                storage
                    .put(&page_key(book_hash, &page_filename), &data)
                    .map_err(|e| CarrelError::io(format!("Failed to write page {idx}: {e}")))?;
                total_size += data.len() as u64;
                on_page(idx);
            }
            Ok(total_size)
        }
        BookFormat::Cbr => {
            // Map the wanted archive-entry names to their canonical index so
            // a single streaming pass can pick them out in archive order.
            let mut remaining: HashMap<String, usize> =
                want.iter().map(|&i| (names[i].clone(), i)).collect();

            let mut total_size: u64 = 0;
            let archive = unrar::Archive::new(file_path)
                .open_for_processing()
                .map_err(|e| {
                    CarrelError::invalid(format!("Failed to open CBR for processing: {e}"))
                })?;

            let mut cursor = archive;
            loop {
                if remaining.is_empty() {
                    break;
                }
                let header = cursor
                    .read_header()
                    .map_err(|e| CarrelError::invalid(format!("Error reading CBR header: {e}")))?;
                match header {
                    None => break,
                    Some(entry) => {
                        let entry_name = entry.entry().filename.to_string_lossy().to_string();
                        if let Some(idx) = remaining.remove(&entry_name) {
                            let (data, next) = entry.read().map_err(|e| {
                                CarrelError::invalid(format!("Failed to extract CBR entry: {e}"))
                            })?;
                            let page_filename = comic_page_filename(idx, &entry_name);
                            storage
                                .put(&page_key(book_hash, &page_filename), &data)
                                .map_err(|e| {
                                    CarrelError::io(format!("Failed to write page {idx}: {e}"))
                                })?;
                            total_size += data.len() as u64;
                            on_page(idx);
                            cursor = next;
                        } else {
                            cursor = entry.skip().map_err(|e| {
                                CarrelError::invalid(format!("Failed to skip CBR entry: {e}"))
                            })?;
                        }
                    }
                }
            }
            Ok(total_size)
        }
        other => Err(CarrelError::invalid(format!(
            "extract_comic_subset not supported for {other:?}"
        ))),
    }
}

/// Build a comic manifest whose `pages` lists ALL page filenames (so
/// `get_cached_page` can map any index to a filename), even when only a
/// subset of the corresponding files exist on disk. Missing pages simply
/// fail the disk read and are served on-demand.
fn build_comic_manifest(
    book_id: &str,
    book_hash: &str,
    format: &BookFormat,
    names: &[String],
    total_size_bytes: u64,
) -> CacheManifest {
    let pages: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, n)| comic_page_filename(i, n))
        .collect();
    let now = now_iso();
    CacheManifest {
        book_id: book_id.to_string(),
        book_hash: book_hash.to_string(),
        page_count: pages.len() as u32,
        total_size_bytes,
        extracted_at: now.clone(),
        last_accessed: now,
        pages,
        format: format.clone(),
        canonical_width: None,
    }
}

/// Full comic extraction: every page into the cache plus a complete
/// manifest. Backs [`extract_cbz`]/[`extract_cbr`] and the `ensure_cached`
/// comic path.
/// **All or nothing**: a failure part-way through removes whatever was
/// already written, so this either leaves a complete cache entry or leaves
/// the cache exactly as it found it. Page blobs are written first and the
/// manifest last, so without the cleanup a failure on entry 900 of 1000 —
/// or a full disk, or a failed manifest write after every page landed —
/// leaves manifest-less bytes behind with nothing accounting for them.
///
/// That invariant is what lets [`crate::reader::page_image`] decide whether
/// to run its caller's eviction hook by looking at this function's result
/// alone. Without it the caller has to infer "were bytes written?" from
/// "was the manifest complete beforehand?", and those two come apart on
/// exactly the failure paths that matter: an archive that never opened
/// wrote nothing, while one that failed at entry 900 wrote a great deal.
/// Guessing wrong in one direction leaves the cache over budget; in the
/// other it hands an unauthenticated LAN client a full-cache eviction walk
/// per request. Several rounds of the M3 review went back and forth on that
/// inference before the invariant replaced it.
fn extract_comic_full(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
    format: &BookFormat,
) -> CarrelResult<CacheManifest> {
    // Before any write: nothing to undo if this is where it fails, which is
    // the corrupt/unopenable-archive case.
    let names = comic_page_names(format, file_path)?;
    let want: BTreeSet<usize> = (0..names.len()).collect();

    // Counted through `extract_comic_subset`'s existing per-page callback,
    // which fires immediately after each successful `put`. A failure with
    // this still at zero wrote nothing, and must not pay for a cleanup:
    // `evict_book` lists the book's prefix, and `LocalStorage::list` walks
    // the *whole* cache root before filtering. Without this guard a corrupt
    // comic — or a permanently unwritable cache, where the very first `put`
    // fails — would walk the entire cache on every request, and the web
    // route's PIN is optional (M3 review round 8).
    let mut written = 0usize;
    let extracted = (|| {
        let total_size =
            extract_comic_subset(storage, book_hash, format, file_path, &names, &want, |_| {
                written += 1
            })?;
        let manifest = build_comic_manifest(book_id, book_hash, format, &names, total_size);
        write_manifest(storage, book_hash, &manifest)?;
        Ok(manifest)
    })();

    match extracted {
        Ok(manifest) => Ok(manifest),
        // Nothing landed, so there is nothing of *this* call's to undo. The
        // deliberate trade (M3 review round 9): this also skips reclaiming
        // any pre-existing manifest-less residue for the same hash, which a
        // cleanup here would have swept. Recovering someone else's residue
        // is not worth one whole-cache walk per failed request on a route an
        // unauthenticated LAN client can loop — and that residue is already
        // outside the size budget either way.
        Err(e) if written == 0 => Err(e),
        Err(e) => {
            // Best effort: a cleanup that itself fails (the same full disk,
            // the same unwritable storage) must not replace the real error
            // with a second one.
            //
            // There is no backstop if it does. `run_eviction`'s
            // `evict_orphan_prefixes` pass deliberately spares manifest-less
            // page files — it removes only an orphaned `text-index.json`,
            // because a prefix without a manifest may be an extraction still
            // in flight — and `collect_cached_books` skips hashes with no
            // manifest, so those bytes never count toward the size budget
            // either. They survive until `clear_cache`. That is precisely
            // the "extraction path's own orphan handling" that pass's
            // comment defers to, and this is it, so it is worth getting
            // right rather than relying on a later sweep (M3 review round 8;
            // earlier rounds asserted a backstop that does not exist).
            if let Err(cleanup) = evict_book(storage, book_hash) {
                log::warn!(
                    "page cache: cleanup after a failed extraction of {book_hash} also failed \
                     ({cleanup}); manifest-less pages may remain until the cache is cleared"
                );
            }
            Err(e)
        }
    }
}

/// Extract a CBZ into the page cache. See [`extract_comic_full`] for the
/// all-or-nothing contract: **a failure that had already written at least
/// one page removes the book's cached pages**, including any that were
/// there before the call, so it is destructive toward `book_hash`'s prefix
/// rather than merely unsuccessful. A failure before the first write — a
/// corrupt or unopenable archive — leaves the prefix untouched (M3 review
/// rounds 7 and 8). Callers that already hold a complete cache entry should
/// not re-run it speculatively — [`ensure_cached`] short-circuits on a
/// complete manifest for exactly that reason.
pub fn extract_cbz(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
) -> CarrelResult<CacheManifest> {
    extract_comic_full(storage, book_id, book_hash, file_path, &BookFormat::Cbz)
}

/// Extract a CBR into the page cache. Same all-or-nothing, destructive-on-
/// failure contract as [`extract_cbz`].
pub fn extract_cbr(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
) -> CarrelResult<CacheManifest> {
    extract_comic_full(storage, book_id, book_hash, file_path, &BookFormat::Cbr)
}

/// Fast-path comic open (F-4-1). Extracts only the first page — plus any
/// `priority_pages` (e.g. a mid-book resume index) — writes a *full*
/// manifest, and returns immediately so the reader can paint. The rest of
/// the archive is left for [`extract_comic_remaining`], spawned by the
/// caller on a background task; any page requested before then is served
/// on-demand by `get_comic_page_bytes` (cache miss → direct archive read).
///
/// If a complete, valid cache already exists (first and last page present)
/// this short-circuits exactly like `ensure_cached`, bumping
/// `last_accessed` without reopening the archive. A partial/corrupt cache
/// is evicted and re-primed.
pub fn ensure_comic_fast(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
    format: &BookFormat,
    priority_pages: &[u32],
) -> CarrelResult<CacheManifest> {
    if let Some(mut manifest) = read_manifest(storage, book_hash) {
        let first_ok = manifest
            .pages
            .first()
            .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
            .unwrap_or(false);
        let last_ok = manifest
            .pages
            .last()
            .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
            .unwrap_or(false);
        if first_ok && last_ok {
            page_dbg!(
                "ensure_comic_fast: complete cache hit for {} ({} pages)",
                book_hash,
                manifest.page_count
            );
            manifest.last_accessed = now_iso();
            let _ = write_manifest(storage, book_hash, &manifest);
            return Ok(manifest);
        }
        page_dbg!(
            "ensure_comic_fast: partial/corrupt cache for {}, re-priming",
            book_hash
        );
        let _ = evict_book(storage, book_hash);
    }

    let names = comic_page_names(format, file_path)?;
    if names.is_empty() {
        return Err(CarrelError::invalid("Comic archive contains no pages"));
    }

    let mut want: BTreeSet<usize> = BTreeSet::new();
    want.insert(0);
    for &p in priority_pages {
        let idx = p as usize;
        if idx < names.len() {
            want.insert(idx);
        }
    }

    let total_size =
        extract_comic_subset(storage, book_hash, format, file_path, &names, &want, |_| {})?;
    let manifest = build_comic_manifest(book_id, book_hash, format, &names, total_size);
    write_manifest(storage, book_hash, &manifest)?;
    Ok(manifest)
}

/// Background pass (F-4-1): extract every page not already cached, calling
/// `on_progress(available, total)` once up front and again after each page
/// lands. Idempotent — pages already on disk are skipped — so it is safe to
/// run after [`ensure_comic_fast`] and even to re-run. On completion the
/// manifest's `total_size_bytes` is refreshed to disk-truth (best-effort).
///
/// This never deletes or mutates existing page files, and the only writer
/// of page bytes besides the fast path. Concurrent on-demand cache reads
/// are safe: `Storage::put` is atomic (temp-file + rename), so a reader
/// sees either the old (absent) or the fully-written page, never a partial.
pub fn extract_comic_remaining<F: FnMut(u32, u32)>(
    storage: &dyn Storage,
    book_hash: &str,
    file_path: &str,
    format: &BookFormat,
    mut on_progress: F,
) -> CarrelResult<()> {
    let names = comic_page_names(format, file_path)?;
    let total = names.len() as u32;
    if total == 0 {
        return Ok(());
    }

    let mut missing: BTreeSet<usize> = BTreeSet::new();
    for (i, src) in names.iter().enumerate() {
        let key = page_key(book_hash, &comic_page_filename(i, src));
        if !storage.exists(&key).unwrap_or(false) {
            missing.insert(i);
        }
    }

    let available = std::cell::Cell::new(total - missing.len() as u32);
    on_progress(available.get(), total);
    if missing.is_empty() {
        return Ok(());
    }

    let cb = |_idx: usize| {
        available.set(available.get() + 1);
        on_progress(available.get(), total);
    };
    extract_comic_subset(storage, book_hash, format, file_path, &names, &missing, cb)?;

    if let Some(mut manifest) = read_manifest(storage, book_hash) {
        // Refresh the informational size snapshot to disk-truth. Eviction and
        // stats read disk directly, so a failure here is harmless.
        manifest.total_size_bytes = book_disk_size_bytes(storage, book_hash);
        let _ = write_manifest(storage, book_hash, &manifest);
    } else {
        // Our manifest vanished mid-run — a concurrent open's `run_eviction`
        // reclaimed this book under the size cap while we were extracting.
        // Any pages we wrote after that deletion are orphans:
        // `collect_cached_books` skips manifest-less hashes, so eviction can
        // never reclaim them and they defeat the size cap. Remove them.
        page_dbg!(
            "extract_comic_remaining: manifest for {} gone mid-run — cleaning orphans",
            book_hash
        );
        let _ = evict_book(storage, book_hash);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cache read & ensure_cached
// ---------------------------------------------------------------------------

pub fn get_cached_page(
    storage: &dyn Storage,
    book_hash: &str,
    page_index: u32,
) -> CarrelResult<(Vec<u8>, String)> {
    let manifest = read_manifest(storage, book_hash)
        .ok_or_else(|| CarrelError::not_found("Cache manifest not found"))?;

    let page_name = match manifest.format {
        BookFormat::Pdf => {
            if page_index >= manifest.page_count {
                return Err(CarrelError::not_found(format!(
                    "Page index {page_index} out of range (total: {})",
                    manifest.page_count
                )));
            }
            format!("{page_index:03}.jpg")
        }
        _ => manifest
            .pages
            .get(page_index as usize)
            .cloned()
            .ok_or_else(|| {
                CarrelError::not_found(format!(
                    "Page index {page_index} out of range (total: {})",
                    manifest.page_count
                ))
            })?,
    };

    let data = storage
        .get(&page_key(book_hash, &page_name))
        .map_err(|e| CarrelError::io(format!("Failed to read cached page {page_index}: {e}")))?;

    Ok((data, mime_for_page_name(&page_name).to_string()))
}

/// Bump `book_hash`'s `last_accessed` to now, without touching any cached
/// page content.
///
/// Every `ensure_*` function in this module already does this on its own
/// cache-hit path. This is for a caller whose cache-hit path reads pages
/// directly via [`get_cached_page`] instead of going through one of those —
/// without it, a book read only that way looks, to [`run_eviction`], like
/// the coldest entry in the whole cache and is first in line for both its
/// size cap and its 20-book cap, even while it's the one actively being
/// read. A no-op when there is no manifest to touch; like every other
/// manifest write in this module, a failure is swallowed rather than
/// surfaced — the worst case is this read being treated as slightly colder
/// than it really was, not a failed page request.
pub fn touch_last_accessed(storage: &dyn Storage, book_hash: &str) {
    if let Some(mut manifest) = read_manifest(storage, book_hash) {
        manifest.last_accessed = now_iso();
        let _ = write_manifest(storage, book_hash, &manifest);
    }
}

/// Whether `book_hash`'s cached comic is complete enough to trust without
/// reopening the source archive: the manifest exists, and its first and
/// last listed page are both actually on disk. Returns the manifest when
/// so, `None` otherwise.
///
/// This is the exact completeness test [`ensure_cached`] and
/// [`ensure_comic_fast`] already use to decide "cache hit, skip extraction"
/// vs. "partial, evict and re-extract" — a sample of two pages, not a full
/// per-page existence scan (a caller that needs a scan on every book open
/// would put a whole-cache stat storm on that path; this reuses the same
/// evidence those two functions already consider sufficient for their own
/// decisions). `ensure_comic_fast`'s own manifest is the case that makes
/// the sample matter to a caller outside this module: it writes a
/// *complete-looking* manifest immediately but extracts only the first page
/// (plus any priority pages), filling the rest in later via
/// [`extract_comic_remaining`]. A caller that only checked "does a manifest
/// exist" would trust that manifest before the background pass finishes,
/// and report a page count — or serve a read — for pages that are not
/// actually there yet.
pub fn complete_manifest(storage: &dyn Storage, book_hash: &str) -> Option<CacheManifest> {
    let manifest = read_manifest(storage, book_hash)?;
    let first_ok = manifest
        .pages
        .first()
        .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
        .unwrap_or(false);
    let last_ok = manifest
        .pages
        .last()
        .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
        .unwrap_or(false);
    if first_ok && last_ok {
        Some(manifest)
    } else {
        None
    }
}

/// The page count from `book_hash`'s PDF manifest, when that manifest can
/// actually back a read — the counterpart of [`complete_manifest`],
/// deliberately not the same test.
///
/// [`complete_manifest`] samples two of a comic's *listed* pages because a
/// comic manifest makes a claim about what is on disk: `ensure_comic_fast`
/// writes a complete-looking `pages` list while the extraction is still
/// running, so trusting it would report a count for pages that are not
/// there. A PDF manifest makes no such claim — `pages` is empty, filenames
/// derive from the index, and pages are cached one at a time on demand —
/// and its `page_count` comes straight from pdfium at
/// [`ensure_pdf_prewarmed`] time, keyed by content hash, with a failed
/// prewarm leaving no manifest at all. So the *count* is correct whether or
/// not a single page is cached.
///
/// What this gates on instead is whether answering leads anywhere: at least
/// one page must be on disk. Both halves of that matter.
///
/// Requiring the comic-style first-*and-last* sample would refuse a book
/// that is merely part-read — PDF pages fill in as they are visited, so a
/// 300-page book read to page 20 has page 0 and not page 299 — and a book
/// with 21 perfectly readable cached pages would not open at all once its
/// source went away, the exact case the cache is for. Requiring *nothing*
/// is equally wrong: a manifest with zero cached pages is a routine state,
/// not an edge case, since `commands::prepare_pdf` prewarms zero pages and
/// a private-mode read caches none at all. Trusting that count would open
/// a reader in which every single page then fails.
///
/// "At least one page" is what separates them.
///
/// Answering that cheaply matters, because this runs on book open. Page 0
/// is the common case — it is where reading a book starts, and where the
/// desktop's background prerender pass fills in first — so an `exists`
/// probe answers most calls in one stat. It is not the rule, though: a book
/// resumed at page 50 is cached from there up, and `prepare_pdf`'s reserved
/// render targets that resume page rather than page 0, so on such a book
/// nothing is at index 0 at all. That is what the listing fallback is for,
/// and why it must not be deleted as redundant. The fallback is worth
/// confining rather than defaulting to: `Storage::list` filters by prefix
/// but the local implementation walks the whole storage root to build the
/// list first, and that root is the entire app cache — exactly the
/// "whole-cache stat storm on the book-open path" that
/// [`complete_manifest`]'s doc comment above avoids for comics.
pub fn pdf_manifest_page_count(storage: &dyn Storage, book_hash: &str) -> Option<u32> {
    let manifest = read_manifest(storage, book_hash)?;
    if manifest.format != BookFormat::Pdf || manifest.page_count == 0 {
        return None;
    }
    let any_page_cached = storage
        .exists(&page_key(book_hash, "000.jpg"))
        .unwrap_or(false)
        || storage
            .list(&book_prefix(book_hash))
            .map(|keys| keys.iter().any(|k| k.ends_with(".jpg")))
            .unwrap_or(false);
    if any_page_cached {
        Some(manifest.page_count)
    } else {
        None
    }
}

pub fn ensure_cached(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
    format: &BookFormat,
) -> CarrelResult<CacheManifest> {
    // PDF manifests intentionally keep `pages` empty (filenames are
    // derived from the page index), so the comic-style first/last
    // validation below would always report "corrupt" and evict a
    // healthy PDF cache. Delegate to `ensure_pdf_prewarmed`, which
    // has its own format-aware idempotency check.
    if matches!(format, BookFormat::Pdf) {
        return ensure_pdf_prewarmed(storage, book_id, book_hash, file_path, 10);
    }

    if let Some(mut manifest) = read_manifest(storage, book_hash) {
        let first_ok = manifest
            .pages
            .first()
            .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
            .unwrap_or(false);
        let last_ok = manifest
            .pages
            .last()
            .and_then(|p| storage.exists(&page_key(book_hash, p)).ok())
            .unwrap_or(false);

        if first_ok && last_ok {
            page_dbg!(
                "ensure_cached: cache hit for {} ({} pages)",
                book_hash,
                manifest.page_count
            );
            manifest.last_accessed = now_iso();
            let _ = write_manifest(storage, book_hash, &manifest);
            return Ok(manifest);
        }
        page_dbg!(
            "ensure_cached: cache corrupted for {}, re-extracting",
            book_hash
        );
        let _ = evict_book(storage, book_hash);
    }

    page_dbg!("ensure_cached: extracting {:?} {}", format, book_hash);
    let start = std::time::Instant::now();
    let result = match format {
        BookFormat::Cbz => extract_cbz(storage, book_id, book_hash, file_path),
        BookFormat::Cbr => extract_cbr(storage, book_id, book_hash, file_path),
        _ => Err(CarrelError::invalid(format!(
            "Page cache not supported for format: {:?}",
            format
        ))),
    };
    page_dbg!("ensure_cached: extraction took {:?}", start.elapsed());
    result
}

// ---------------------------------------------------------------------------
// PDF cache (lazy on-disk render cache)
// ---------------------------------------------------------------------------

/// Lazy cache writes are coalesced into background eviction passes:
/// every `LAZY_EVICTION_BATCH` successful writes fire the caller's
/// `on_batch` hook. The command layer uses this to spawn a background
/// `run_eviction`.
pub const LAZY_EVICTION_BATCH: u32 = 25;

// Global counter rather than per-book — eviction is whole-cache
// anyway, so the trigger cadence does not need to be per-book.
static LAZY_WRITE_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
pub fn reset_lazy_eviction_counter_for_tests() {
    LAZY_WRITE_COUNTER.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Pre-render the first `prewarm.min(page_count)` pages of a PDF and
/// persist a manifest describing the document. The renderer closure is
/// injected so unit tests can stub pdfium out; production callers go
/// through [`ensure_pdf_prewarmed`] (below) which wires
/// [`crate::pdf::get_page_image_bytes`] for them.
///
/// On any disk write failure (or final manifest write failure) the
/// function rolls back partial state via [`evict_book`] and returns
/// the underlying [`CarrelError`]; no manifest is persisted in that
/// case. This keeps stray page files out of `collect_cached_books`,
/// which only counts books with a manifest.
pub fn ensure_pdf_prewarmed_with_renderer<F>(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    page_count: u32,
    prewarm: u32,
    render: F,
) -> CarrelResult<CacheManifest>
where
    F: Fn(u32) -> CarrelResult<(Vec<u8>, String)>,
{
    let prewarm = prewarm.min(page_count);

    if let Some(mut manifest) = read_manifest(storage, book_hash) {
        if manifest.format == BookFormat::Pdf
            && manifest.page_count == page_count
            && (0..prewarm).all(|i| {
                storage
                    .exists(&page_key(book_hash, &format!("{i:03}.jpg")))
                    .unwrap_or(false)
            })
        {
            page_dbg!(
                "ensure_pdf_prewarmed: cache hit for {} ({}/{} pre-warmed)",
                book_hash,
                prewarm,
                page_count
            );
            manifest.last_accessed = now_iso();
            let _ = write_manifest(storage, book_hash, &manifest);
            return Ok(manifest);
        }
    }

    page_dbg!(
        "ensure_pdf_prewarmed: rendering first {} of {} for {}",
        prewarm,
        page_count,
        book_hash
    );
    let start = std::time::Instant::now();

    // Helper: any failure in the loop or the final manifest write
    // leaves the partial output (page files, possibly an old manifest
    // pointing at stale paths) unreferenced from the manifest layer.
    // `collect_cached_books` skips books without a manifest, so those
    // orphans would never be evicted. Roll back explicitly.
    let try_warm = || -> CarrelResult<u64> {
        let mut total_size: u64 = 0;
        for i in 0..prewarm {
            let (bytes, _mime) = render(i)?;
            let name = format!("{i:03}.jpg");
            storage.put(&page_key(book_hash, &name), &bytes)?;
            total_size += bytes.len() as u64;
        }
        Ok(total_size)
    };

    let total_size = match try_warm() {
        Ok(s) => s,
        Err(e) => {
            page_dbg!(
                "ensure_pdf_prewarmed: warm failed for {} — rolling back partial cache",
                book_hash
            );
            let _ = evict_book(storage, book_hash);
            return Err(e);
        }
    };

    page_dbg!(
        "ensure_pdf_prewarmed: warmed {} pages ({} KB) in {:?}",
        prewarm,
        total_size / 1024,
        start.elapsed()
    );

    let now = now_iso();
    let manifest = CacheManifest {
        book_id: book_id.to_string(),
        book_hash: book_hash.to_string(),
        page_count,
        total_size_bytes: total_size,
        extracted_at: now.clone(),
        last_accessed: now,
        pages: Vec::new(),
        format: BookFormat::Pdf,
        canonical_width: Some(crate::pdf::CACHE_CANONICAL_WIDTH),
    };
    if let Err(e) = write_manifest(storage, book_hash, &manifest) {
        let _ = evict_book(storage, book_hash);
        return Err(e);
    }
    Ok(manifest)
}

/// Production entry point: wires [`crate::pdf::get_page_count`] +
/// [`crate::pdf::get_page_image_bytes`] into the generic prewarm above.
pub fn ensure_pdf_prewarmed(
    storage: &dyn Storage,
    book_id: &str,
    book_hash: &str,
    file_path: &str,
    prewarm: u32,
) -> CarrelResult<CacheManifest> {
    let page_count = crate::pdf::get_page_count(file_path)?;
    let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
        let (bytes, mime) = crate::pdf::get_page_image_bytes(
            file_path,
            idx,
            Some(crate::pdf::CACHE_CANONICAL_WIDTH),
        )?;
        Ok((bytes, mime.to_string()))
    };
    ensure_pdf_prewarmed_with_renderer(storage, book_id, book_hash, page_count, prewarm, render)
}

/// Disk-first PDF page lookup. On cache miss, renders via the injected
/// closure, attempts to persist (best-effort), and returns the bytes
/// either way. Manifest must already exist (created by
/// [`ensure_pdf_prewarmed`]); without one, falls back to render-only.
///
/// `on_batch` fires when the lazy-write counter crosses a multiple of
/// [`LAZY_EVICTION_BATCH`] — the command layer wires this to spawn a
/// background eviction.
///
/// `suppress_write` (private mode, B-M1, OQ-3/SB-9): skips only the page
/// *content* write (`storage.put` + the `LAZY_WRITE_COUNTER`/`on_batch`
/// path) — the real forensic trace of which page was viewed. The read/
/// pre-warm fast path above is untouched, and `last_accessed` is still
/// bumped and persisted to the manifest so a privately-read book isn't
/// preferentially evicted by the LRU/age policy.
pub fn get_or_render_pdf_page_with_renderer<F, B>(
    storage: &dyn Storage,
    book_hash: &str,
    page_index: u32,
    render: F,
    on_batch: B,
    suppress_write: bool,
) -> CarrelResult<(Vec<u8>, String)>
where
    F: Fn(u32) -> CarrelResult<(Vec<u8>, String)>,
    B: Fn(),
{
    let manifest_opt = read_manifest(storage, book_hash);

    if let Some(ref manifest) = manifest_opt {
        if manifest.format == BookFormat::Pdf {
            // Guard against out-of-range indices before either the
            // disk lookup or the renderer runs. `get_cached_page`
            // returns `NotFound` both for "file missing" and "index
            // >= page_count"; we want the latter to surface to the
            // caller rather than silently fall through to the
            // expensive render + cache path.
            if page_index >= manifest.page_count {
                return Err(CarrelError::not_found(format!(
                    "Page index {page_index} out of range (total: {})",
                    manifest.page_count
                )));
            }
            if let Ok((data, mime)) = get_cached_page(storage, book_hash, page_index) {
                return Ok((data, mime));
            }
        }
    }

    let (bytes, mime) = render(page_index)?;

    // Only attempt to cache when a PDF manifest exists; otherwise
    // just return the rendered bytes. Cache writes are best-effort.
    if let Some(mut manifest) = manifest_opt {
        if manifest.format == BookFormat::Pdf {
            if suppress_write {
                // Private mode: never persist the rendered page bytes.
                // Still bump + persist last_accessed (memory AND
                // manifest) so eviction doesn't treat this book as cold.
                manifest.last_accessed = now_iso();
                let _ = write_manifest(storage, book_hash, &manifest);
            } else {
                let name = format!("{page_index:03}.jpg");
                match storage.put(&page_key(book_hash, &name), &bytes) {
                    Ok(()) => {
                        // Only touch last_accessed — `total_size_bytes`
                        // intentionally stays as the warm-time snapshot.
                        // Eviction reads the disk directly via
                        // `book_disk_size_bytes`, so a concurrent lazy
                        // read-modify-write on the field would only ever
                        // cause stats drift, not eviction misbehavior.
                        // Dropping the update kills the lost-increment
                        // race outright.
                        manifest.last_accessed = now_iso();
                        let _ = write_manifest(storage, book_hash, &manifest);

                        let prev =
                            LAZY_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if (prev + 1).is_multiple_of(LAZY_EVICTION_BATCH) {
                            on_batch();
                        }
                    }
                    Err(e) => {
                        page_dbg!(
                            "lazy cache write failed for {}/{}: {} — serving from memory",
                            book_hash,
                            name,
                            e
                        );
                    }
                }
            }
        }
    }

    Ok((bytes, mime))
}

/// Production wrapper: wires [`crate::pdf::get_page_image_bytes`] at
/// the canonical width and forwards the `on_batch` callback and
/// `suppress_write` flag unchanged.
pub fn get_or_render_pdf_page_with_eviction<B>(
    storage: &dyn Storage,
    book_hash: &str,
    file_path: &str,
    page_index: u32,
    on_batch: B,
    suppress_write: bool,
) -> CarrelResult<(Vec<u8>, String)>
where
    B: Fn(),
{
    let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
        let (bytes, mime) = crate::pdf::get_page_image_bytes(
            file_path,
            idx,
            Some(crate::pdf::CACHE_CANONICAL_WIDTH),
        )?;
        Ok((bytes, mime.to_string()))
    };
    get_or_render_pdf_page_with_renderer(
        storage,
        book_hash,
        page_index,
        render,
        on_batch,
        suppress_write,
    )
}

/// No-op-eviction variant for callers that do not have a runtime
/// to dispatch the background pass (tests, ad-hoc tooling).
pub fn get_or_render_pdf_page(
    storage: &dyn Storage,
    book_hash: &str,
    file_path: &str,
    page_index: u32,
) -> CarrelResult<(Vec<u8>, String)> {
    get_or_render_pdf_page_with_eviction(storage, book_hash, file_path, page_index, || {}, false)
}

// ---------------------------------------------------------------------------
// PDF background prerender (F-4-5)
// ---------------------------------------------------------------------------

/// Outcome of a background PDF prerender pass, returned so the caller can
/// emit a definitive terminal progress event and log partial coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfPrerenderOutcome {
    /// Pages rendered and written to disk during THIS pass.
    pub rendered: u32,
    /// Pages present on disk after the pass (already-cached + newly rendered).
    pub cached_total: u32,
    /// Full page count of the document, from the manifest.
    pub page_count: u32,
    /// True if the size bound stopped the pass before covering every page.
    pub stopped_early: bool,
}

/// Background pass (F-4-5): render every PDF page not already on disk into
/// the shared page cache, bounded by `max_size_bytes` so a single oversized
/// book cannot blow past the whole-cache size cap. The renderer is injected
/// so unit tests can stub pdfium out; production callers go through
/// [`prerender_pdf_remaining`].
///
/// Requires a PDF manifest to already exist (written by
/// [`ensure_pdf_prewarmed`]) — without one there is nothing to key pages
/// against, so the pass is a no-op. Idempotent: pages already on disk are
/// skipped, so it is safe to run right after the fast prewarm and even to
/// re-run.
///
/// `on_progress(cached, total)` fires once up front and again after each
/// page lands, where `total` is the document page count and `cached` the
/// pages on disk so far. When the size bound stops the pass early, `cached`
/// settles below `total` — an honest reflection of partial coverage; the
/// remaining pages are still served on-demand by
/// [`get_or_render_pdf_page_with_renderer`].
///
/// A single page's render or write failure is logged and skipped rather than
/// aborting the whole pass (the page falls back to on-demand rendering).
///
/// `should_abort` is polled before each page is rendered; when it returns
/// `true` the pass stops cleanly (like the size bound). Production wires this
/// to the live private-mode flag so that turning "don't track this session"
/// on mid-pass halts further page-cache writes — matching the on-demand read
/// path's write suppression (B-M1/OQ-3/SB-9). Pages already written stay;
/// remaining pages are served (and, while private, not written) on-demand.
///
/// Mirrors [`extract_comic_remaining`]'s orphan-cleanup contract: if our
/// manifest is evicted mid-run by a concurrent open's `run_eviction`, any
/// pages written afterward are orphans (invisible to `collect_cached_books`,
/// so never reclaimable by eviction) and are removed via [`evict_book`].
/// Concurrent on-demand reads are safe — `Storage::put` is atomic
/// (temp-file + rename), so a reader sees either the absent or the fully
/// written page, never a partial.
pub fn prerender_pdf_remaining_with_renderer<F, P, A, T>(
    storage: &dyn Storage,
    book_hash: &str,
    max_size_bytes: u64,
    render: F,
    mut on_progress: P,
    should_abort: A,
    // Called once before each page render. Wired by the command layer to block
    // while a foreground page render is in flight, so this background pass
    // yields the (network-mounted) source and CPU to the page the user is
    // actually waiting on. Must return promptly once `should_abort` is true so
    // the pass can still stop; a no-op disables throttling.
    throttle: T,
) -> CarrelResult<PdfPrerenderOutcome>
where
    F: Fn(u32) -> CarrelResult<(Vec<u8>, String)>,
    P: FnMut(u32, u32),
    A: Fn() -> bool,
    T: Fn(),
{
    let page_count = match read_manifest(storage, book_hash) {
        Some(m) if m.format == BookFormat::Pdf => m.page_count,
        // No PDF manifest — the prewarm must create it first. No-op.
        _ => {
            return Ok(PdfPrerenderOutcome {
                rendered: 0,
                cached_total: 0,
                page_count: 0,
                stopped_early: false,
            });
        }
    };

    let missing: Vec<u32> = (0..page_count)
        .filter(|i| {
            !storage
                .exists(&page_key(book_hash, &format!("{i:03}.jpg")))
                .unwrap_or(false)
        })
        .collect();

    let mut cached = page_count - missing.len() as u32;
    on_progress(cached, page_count);
    if missing.is_empty() {
        return Ok(PdfPrerenderOutcome {
            rendered: 0,
            cached_total: cached,
            page_count,
            stopped_early: false,
        });
    }

    // Bound against the WHOLE cache, not just this book: seed the running
    // total with every cached book's disk usage so the pass cannot push the
    // aggregate past `max_size_bytes` even when other books already occupy
    // most of it (otherwise the cache could transiently reach ~2× the cap
    // before the caller's post-pass `run_eviction` reconciles it). The
    // just-opened book is the most-recently-used, so the post-pass eviction
    // trims colder books first, freeing room for it over subsequent opens.
    let mut current_size: u64 = collect_cached_books(storage)
        .iter()
        .map(|b| book_disk_size_bytes(storage, &b.book_hash))
        .sum();
    let mut rendered: u32 = 0;
    let mut stopped_early = false;

    for idx in missing {
        // Abort: private mode turned on mid-pass (B-M1). Stop before writing
        // any more pages to disk so a "don't track this session" toggle is
        // honored by the background writer just as it is by the on-demand
        // read path. Pages already on disk stay; the rest render on-demand.
        if should_abort() {
            page_dbg!(
                "prerender_pdf_remaining: abort requested for {} — stopping at {}/{} pages",
                book_hash,
                cached,
                page_count
            );
            break;
        }

        // Yield to any in-flight foreground render before starting this page.
        // `throttle` blocks while the reader is actively rendering a page, so
        // the background pass doesn't contend for the (network-mounted) source
        // and CPU. It returns promptly once `should_abort` trips, so re-check
        // afterward in case private mode toggled on during the wait.
        throttle();
        if should_abort() {
            break;
        }

        // Bound: stop before we cross the whole-cache size cap. A single page
        // may overshoot by at most its own size; `run_eviction` (fired by the
        // caller after this pass) reconciles the multi-book budget.
        if current_size >= max_size_bytes {
            stopped_early = true;
            page_dbg!(
                "prerender_pdf_remaining: size bound {} B reached for {} — stopping at {}/{} pages",
                max_size_bytes,
                book_hash,
                cached,
                page_count
            );
            break;
        }

        let bytes = match render(idx) {
            Ok((bytes, _mime)) => bytes,
            Err(e) => {
                // Best-effort: a page that fails to prerender is served
                // on-demand later. Do not abort the rest of the pass.
                page_dbg!(
                    "prerender_pdf_remaining: render of page {} failed for {}: {} — skipping",
                    idx,
                    book_hash,
                    e
                );
                continue;
            }
        };

        let name = format!("{idx:03}.jpg");
        match storage.put(&page_key(book_hash, &name), &bytes) {
            Ok(()) => {
                current_size += bytes.len() as u64;
                cached += 1;
                rendered += 1;
                on_progress(cached, page_count);
            }
            Err(e) => {
                page_dbg!(
                    "prerender_pdf_remaining: write of page {} failed for {}: {} — skipping",
                    idx,
                    book_hash,
                    e
                );
            }
        }
    }

    // Refresh the informational size snapshot to disk-truth (best-effort),
    // mirroring the comic pass. A vanished manifest means a concurrent open's
    // `run_eviction` reclaimed this book mid-run; pages written afterward are
    // orphans, so clean them up.
    if let Some(mut m) = read_manifest(storage, book_hash) {
        m.total_size_bytes = book_disk_size_bytes(storage, book_hash);
        let _ = write_manifest(storage, book_hash, &m);
    } else {
        page_dbg!(
            "prerender_pdf_remaining: manifest for {} gone mid-run — cleaning orphans",
            book_hash
        );
        let _ = evict_book(storage, book_hash);
        // The book (every page) was just scrubbed from disk, so `cached`
        // would overstate coverage. Report disk-truth (0) so the caller's
        // terminal progress event does not leave the bar stuck mid-way.
        cached = 0;
    }

    Ok(PdfPrerenderOutcome {
        rendered,
        cached_total: cached,
        page_count,
        stopped_early,
    })
}

/// Production entry point: wires [`crate::pdf::get_page_image_bytes`] at the
/// canonical cache width into [`prerender_pdf_remaining_with_renderer`].
pub fn prerender_pdf_remaining<P, A, T>(
    storage: &dyn Storage,
    book_hash: &str,
    file_path: &str,
    max_size_bytes: u64,
    on_progress: P,
    should_abort: A,
    throttle: T,
) -> CarrelResult<PdfPrerenderOutcome>
where
    P: FnMut(u32, u32),
    A: Fn() -> bool,
    T: Fn(),
{
    let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
        let (bytes, mime) = crate::pdf::get_page_image_bytes(
            file_path,
            idx,
            Some(crate::pdf::CACHE_CANONICAL_WIDTH),
        )?;
        Ok((bytes, mime.to_string()))
    };
    prerender_pdf_remaining_with_renderer(
        storage,
        book_hash,
        max_size_bytes,
        render,
        on_progress,
        should_abort,
        throttle,
    )
}

// ---------------------------------------------------------------------------
// Eviction
// ---------------------------------------------------------------------------

/// Every distinct `{hash}` directly under `CACHE_PREFIX`, manifested or not.
/// Shared by [`collect_cached_books`] (manifested only) and
/// [`evict_orphan_prefixes`] (the complement: no valid manifest).
fn all_cache_hashes(storage: &dyn Storage) -> HashSet<String> {
    let keys = match storage.list(CACHE_PREFIX) {
        Ok(k) => k,
        Err(_) => return HashSet::new(),
    };
    let mut hashes: HashSet<String> = HashSet::new();
    for key in keys {
        if let Some(rest) = key.strip_prefix(CACHE_PREFIX) {
            if let Some(hash) = rest.split('/').next() {
                if !hash.is_empty() {
                    hashes.insert(hash.to_string());
                }
            }
        }
    }
    hashes
}

/// Read the manifest for every hash from [`all_cache_hashes`]. Books whose
/// manifest is missing or corrupt are skipped — they remain as dead keys
/// until `clear_cache` or [`evict_orphan_prefixes`] wipes them, but their
/// absence from this result set means they won't be counted toward LRU /
/// size / age budgets.
fn collect_cached_books(storage: &dyn Storage) -> Vec<CacheManifest> {
    all_cache_hashes(storage)
        .into_iter()
        .filter_map(|h| read_manifest(storage, &h))
        .collect()
}

/// Delete only the `text-index.json` for `book_hash`, leaving any page
/// images and manifest in place. Used to drop an orphaned or
/// private-mode-forbidden text index without disturbing the rest of the
/// book's cache. A no-op if the file isn't there.
pub fn evict_text_index(storage: &dyn Storage, book_hash: &str) -> CarrelResult<()> {
    let key = text_index_key(book_hash);
    if storage.exists(&key).unwrap_or(false) {
        storage.delete(&key)?;
    }
    Ok(())
}

/// Sweep what an orphaned `page-cache/{hash}/` prefix — one with files on
/// disk but no valid manifest — leaves behind, which is **only** an
/// orphaned `text-index.json`. Manifest-less *page files* are deliberately
/// left alone; see the body for why, and
/// `orphan_sweep_spares_manifestless_page_files` for the test that pins it.
///
/// This is the backstop for Finding F2b: the PDF text-index background
/// build (command layer) re-checks the manifest right before writing
/// `text-index.json` and self-cleans if the book was deleted in that tiny
/// window, and this sweep covers whatever slips through that race.
///
/// It is **not** a backstop for orphaned page files, and reading it as one
/// is a mistake this codebase has made repeatedly: `collect_cached_books`
/// skips manifest-less hashes, so such pages are never counted toward the
/// size budget and never reclaimed here — they survive until
/// `clear_cache`. Whoever writes them owns cleaning them up, which is what
/// [`extract_comic_full`]'s failure path does for comics (M3 review rounds
/// 7 and 8).
fn evict_orphan_prefixes(storage: &dyn Storage) -> CarrelResult<()> {
    // A prefix without a valid manifest may be an ACTIVE build in progress:
    // both PDF prewarm and comic extraction write page files (and the
    // manifest's atomic temp file) BEFORE committing the manifest last. So
    // we must NOT delete the whole prefix here — that could wipe a
    // concurrent book's in-flight pages or its manifest temp file.
    //
    // Only drop an orphaned `text-index.json` — the single artifact the PDF
    // text-index writer adds, and only ever after confirming a manifest
    // existed. If the manifest is now gone but a text-index.json remains,
    // it's a leftover from a delete/eviction that raced the write; removing
    // just that file keeps it from escaping the cache budget while leaving
    // any legitimate in-construction page files untouched. Page files
    // without a manifest are left to the prewarm/extraction paths' own
    // orphan handling.
    for hash in all_cache_hashes(storage) {
        if read_manifest(storage, &hash).is_none() {
            evict_text_index(storage, &hash)?;
        }
    }
    Ok(())
}

/// Remove every cache artifact for a book — the whole `page-cache/{hash}/`
/// prefix, including cached page images and, if present, the persisted PDF
/// `text-index.json` (both live under the same book-hash prefix). Public so
/// callers that delete a book for good (`remove_book`, bulk delete, the
/// missing-file cleanup pass) can evict its cache alongside the DB row.
pub fn evict_book(storage: &dyn Storage, book_hash: &str) -> CarrelResult<()> {
    let prefix = book_prefix(book_hash);
    let keys = storage
        .list(&prefix)
        .map_err(|e| CarrelError::io(format!("Failed to list cache for {book_hash}: {e}")))?;
    for key in keys {
        storage
            .delete(&key)
            .map_err(|e| CarrelError::io(format!("Failed to evict cache key '{key}': {e}")))?;
    }
    Ok(())
}

/// Disk-authoritative size for a single cached book. Sums `storage.size`
/// across every page key under the book's prefix; the manifest JSON
/// is excluded so the number reflects cached payload only (and stays
/// stable across manifest rewrites). Used by `run_eviction` and
/// `get_cache_stats` so a stale `manifest.total_size_bytes` snapshot
/// (which we deliberately do not update on lazy PDF writes to avoid a
/// concurrent read-modify-write race) cannot drift the eviction budget
/// or the Settings stats panel.
fn book_disk_size_bytes(storage: &dyn Storage, book_hash: &str) -> u64 {
    let prefix = book_prefix(book_hash);
    let keys = match storage.list(&prefix) {
        Ok(k) => k,
        Err(_) => return 0,
    };
    keys.into_iter()
        .filter(|k| !k.ends_with("manifest.json"))
        .filter_map(|k| storage.size(&k).ok())
        .sum()
}

pub fn run_eviction(storage: &dyn Storage, max_size_mb: u64) -> CarrelResult<()> {
    // Sweep orphaned text indexes first — unconditionally, since an orphan is
    // invisible to `collect_cached_books` below and so is never counted toward
    // the size/LRU/age budgets in the first place. Note this reclaims only
    // `text-index.json`, NOT manifest-less page files; see the function's doc
    // comment.
    evict_orphan_prefixes(storage)?;

    let mut books = collect_cached_books(storage);
    books.sort_by(|a, b| a.last_accessed.cmp(&b.last_accessed));

    // Layer 1: LRU by book count
    while books.len() > MAX_CACHED_BOOKS {
        let oldest = &books[0];
        evict_book(storage, &oldest.book_hash)?;
        books.remove(0);
    }

    // Layer 2: Total size cap (disk-authoritative — the manifest
    // snapshot is not maintained on lazy PDF writes).
    let max_size_bytes = max_size_mb * 1024 * 1024;
    let mut sizes: Vec<u64> = books
        .iter()
        .map(|b| book_disk_size_bytes(storage, &b.book_hash))
        .collect();
    let mut total_size: u64 = sizes.iter().sum();
    while total_size > max_size_bytes && !books.is_empty() {
        let oldest_size = sizes[0];
        total_size = total_size.saturating_sub(oldest_size);
        evict_book(storage, &books[0].book_hash)?;
        books.remove(0);
        sizes.remove(0);
    }

    // Layer 3: Age expiry
    let cutoff = chrono::Utc::now() - chrono::Duration::days(MAX_AGE_DAYS as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let expired: Vec<String> = books
        .iter()
        .filter(|b| b.last_accessed < cutoff_str)
        .map(|b| b.book_hash.clone())
        .collect();
    for hash in expired {
        evict_book(storage, &hash)?;
    }

    Ok(())
}

pub fn get_cache_stats(storage: &dyn Storage) -> CacheStats {
    let books = collect_cached_books(storage);
    let book_count = books.len();
    let book_infos: Vec<CacheBookInfo> = books
        .into_iter()
        .map(|b| {
            let size_bytes = book_disk_size_bytes(storage, &b.book_hash);
            CacheBookInfo {
                book_id: b.book_id,
                book_hash: b.book_hash,
                size_bytes,
                page_count: b.page_count,
                last_accessed: b.last_accessed,
            }
        })
        .collect();
    let total_size_bytes: u64 = book_infos.iter().map(|b| b.size_bytes).sum();
    CacheStats {
        total_size_bytes,
        book_count,
        books: book_infos,
    }
}

pub fn clear_cache(storage: &dyn Storage) -> CarrelResult<()> {
    let keys = storage
        .list(CACHE_PREFIX)
        .map_err(|e| CarrelError::io(format!("Failed to list page cache: {e}")))?;
    for key in keys {
        storage
            .delete(&key)
            .map_err(|e| CarrelError::io(format!("Failed to delete cache key '{key}': {e}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;
    use tempfile::TempDir;

    fn temp_storage() -> (TempDir, LocalStorage) {
        let dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();
        (dir, storage)
    }

    /// Helper: create a fake cached book with a manifest and dummy page files.
    /// `total_size_bytes` controls both the manifest field AND the actual
    /// payload written to disk so eviction logic that reads disk-truth
    /// sees the same number as the manifest snapshot. Page payload is
    /// `total_size_bytes / page_count` zero bytes.
    fn create_fake_cache(
        storage: &dyn Storage,
        book_id: &str,
        book_hash: &str,
        page_count: u32,
        total_size_bytes: u64,
        last_accessed: &str,
    ) -> CacheManifest {
        let mut pages = Vec::new();
        let per_page = if page_count == 0 {
            0
        } else {
            total_size_bytes / page_count as u64
        };
        for i in 0..page_count {
            let name = format!("{:03}.jpg", i);
            let payload = vec![0u8; per_page as usize];
            storage.put(&page_key(book_hash, &name), &payload).unwrap();
            pages.push(name);
        }

        let manifest = CacheManifest {
            book_id: book_id.to_string(),
            book_hash: book_hash.to_string(),
            page_count,
            total_size_bytes,
            extracted_at: last_accessed.to_string(),
            last_accessed: last_accessed.to_string(),
            pages,
            format: BookFormat::Cbz,
            canonical_width: None,
        };
        write_manifest(storage, book_hash, &manifest).unwrap();
        manifest
    }

    #[test]
    fn manifest_roundtrip() {
        let (_d, storage) = temp_storage();
        let hash = "abc123";

        let manifest = CacheManifest {
            book_id: "book1".to_string(),
            book_hash: hash.to_string(),
            page_count: 3,
            total_size_bytes: 1024,
            extracted_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed: "2026-01-01T00:00:00Z".to_string(),
            pages: vec![
                "000.jpg".to_string(),
                "001.jpg".to_string(),
                "002.jpg".to_string(),
            ],
            format: BookFormat::Cbz,
            canonical_width: None,
        };

        write_manifest(&storage, hash, &manifest).unwrap();
        let loaded = read_manifest(&storage, hash).expect("manifest should be readable");

        assert_eq!(loaded.book_id, "book1");
        assert_eq!(loaded.book_hash, hash);
        assert_eq!(loaded.page_count, 3);
        assert_eq!(loaded.total_size_bytes, 1024);
        assert_eq!(loaded.pages.len(), 3);
    }

    #[test]
    fn read_manifest_missing_returns_none() {
        let (_d, storage) = temp_storage();
        assert!(read_manifest(&storage, "nonexistent").is_none());
    }

    #[test]
    fn test_text_index_roundtrip() {
        let (_d, storage) = temp_storage();
        let index = crate::pdf::PdfTextIndex {
            version: crate::pdf::TEXT_INDEX_VERSION,
            page_count: 3,
            pages: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        };

        write_text_index(&storage, "hash1", &index).unwrap();
        let loaded = read_text_index(&storage, "hash1").expect("index should be readable");

        assert_eq!(loaded.version, index.version);
        assert_eq!(loaded.page_count, 3);
        assert_eq!(loaded.pages.len(), loaded.page_count as usize);
        assert_eq!(loaded.pages, index.pages);
    }

    #[test]
    fn test_text_index_version_mismatch_is_miss() {
        let (_d, storage) = temp_storage();
        let stale_json = r#"{"version":99,"pageCount":1,"pages":["stale"]}"#;
        storage
            .put(&text_index_key("hash2"), stale_json.as_bytes())
            .unwrap();

        assert!(read_text_index(&storage, "hash2").is_none());
    }

    #[test]
    fn test_read_text_index_missing_is_none() {
        let (_d, storage) = temp_storage();
        assert!(read_text_index(&storage, "nonexistent").is_none());
    }

    /// F2b backstop: a `page-cache/{hash}/` prefix containing only a stray
    /// `text-index.json` (no manifest) — e.g. a raced background build that
    /// slipped past its own manifest re-check — is invisible to
    /// `collect_cached_books` and so would otherwise never be reclaimed.
    /// `run_eviction` must sweep it unconditionally, while leaving a normal
    /// manifested book (recent, well under the size cap) untouched.
    #[test]
    fn run_eviction_removes_orphan_text_index_prefix() {
        let (_d, storage) = temp_storage();

        let orphan_hash = "orphan-hash";
        let index = crate::pdf::PdfTextIndex {
            version: crate::pdf::TEXT_INDEX_VERSION,
            page_count: 1,
            pages: vec!["stray".to_string()],
        };
        write_text_index(&storage, orphan_hash, &index).unwrap();
        assert!(read_manifest(&storage, orphan_hash).is_none());

        create_fake_cache(&storage, "book1", "hash1", 2, 100, &now_iso());

        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        let leftovers: Vec<String> = storage.list(&book_prefix(orphan_hash)).unwrap();
        assert!(
            leftovers.is_empty(),
            "orphan text-index-only prefix must be evicted; leftovers: {leftovers:?}"
        );
        assert!(
            read_manifest(&storage, "hash1").is_some(),
            "normal manifested book must be untouched"
        );
    }

    /// Regression: the orphan sweep must NOT wipe a manifest-less prefix that
    /// holds page files — that's an ACTIVE build (prewarm/extraction writes
    /// pages, then the manifest last). Only a lone orphaned `text-index.json`
    /// is swept; in-construction page files survive until their manifest
    /// commits.
    #[test]
    fn orphan_sweep_spares_manifestless_page_files() {
        let (_d, storage) = temp_storage();

        let building_hash = "building-hash";
        storage
            .put(&page_key(building_hash, "000.jpg"), b"pagedata")
            .unwrap();
        storage
            .put(&page_key(building_hash, "001.jpg"), b"pagedata")
            .unwrap();
        assert!(read_manifest(&storage, building_hash).is_none());

        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        let remaining: Vec<String> = storage.list(&book_prefix(building_hash)).unwrap();
        assert_eq!(
            remaining.len(),
            2,
            "active prewarm page files must not be swept as orphans: {remaining:?}"
        );
    }

    #[test]
    fn lru_count_eviction() {
        let (_d, storage) = temp_storage();

        // Create MAX_CACHED_BOOKS + 2 books with increasing last_accessed
        // timestamps (all recent, within MAX_AGE_DAYS). Derived from the
        // constant so bumping MAX_CACHED_BOOKS doesn't require touching the test.
        let n = MAX_CACHED_BOOKS + 2;
        let base = chrono::Utc::now();
        for i in 0..n {
            let ts = base + chrono::Duration::seconds(i as i64);
            create_fake_cache(
                &storage,
                &format!("book{i}"),
                &format!("hash{i}"),
                2,
                100,
                &ts.to_rfc3339(),
            );
        }

        let before = collect_cached_books(&storage);
        assert_eq!(before.len(), n);

        // Run eviction — should trim to MAX_CACHED_BOOKS.
        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        let after = collect_cached_books(&storage);
        assert_eq!(after.len(), MAX_CACHED_BOOKS);

        // The two oldest (hash0, hash1) should have been evicted.
        assert!(read_manifest(&storage, "hash0").is_none());
        assert!(read_manifest(&storage, "hash1").is_none());
        // The newest should remain.
        assert!(read_manifest(&storage, &format!("hash{}", n - 1)).is_some());
    }

    #[test]
    fn age_expiry_retains_within_window() {
        let (_d, storage) = temp_storage();

        // A book last accessed 20 days ago must survive: inside the 30-day
        // retention window, outside the old 7-day one. Guards the
        // MAX_AGE_DAYS bump — fails if the window regresses below 20 days.
        let twenty_days_ago = (chrono::Utc::now() - chrono::Duration::days(20)).to_rfc3339();
        create_fake_cache(&storage, "recent", "hash_recent", 1, 100, &twenty_days_ago);

        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        assert!(
            read_manifest(&storage, "hash_recent").is_some(),
            "a 20-day-old book must survive the {MAX_AGE_DAYS}-day retention window"
        );
    }

    #[test]
    fn size_cap_eviction() {
        let (_d, storage) = temp_storage();

        // Create 3 books, each ~2 MB on disk (real payload — eviction
        // now reads filesystem size directly rather than trusting the
        // manifest field).
        let base = chrono::Utc::now();
        for i in 0..3 {
            let ts = base + chrono::Duration::seconds(i as i64);
            create_fake_cache(
                &storage,
                &format!("book{i}"),
                &format!("hash{i}"),
                1,
                2 * 1024 * 1024, // 2 MB
                &ts.to_rfc3339(),
            );
        }

        // Total ≈ 6 MB; cap at 4 MB → should evict the oldest.
        run_eviction(&storage, 4).unwrap();

        let after = collect_cached_books(&storage);
        assert!(after.len() <= 2);
        assert!(read_manifest(&storage, "hash0").is_none());
    }

    #[test]
    fn age_expiry() {
        let (_d, storage) = temp_storage();

        // One old book (well outside MAX_AGE_DAYS)
        create_fake_cache(
            &storage,
            "old",
            "hash_old",
            1,
            100,
            "2020-01-01T00:00:00+00:00",
        );

        // One recent book
        let now = now_iso();
        create_fake_cache(&storage, "new", "hash_new", 1, 100, &now);

        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        assert!(read_manifest(&storage, "hash_old").is_none());
        assert!(read_manifest(&storage, "hash_new").is_some());
    }

    #[test]
    fn cache_stats_counts_and_sizes() {
        let (_d, storage) = temp_storage();

        // create_fake_cache writes payload sized to match the claim
        // (rounded down to per_page = total / page_count). 3 pages
        // for "a" → 333 bytes each = 999 bytes total; 5 pages for "b"
        // → 400 bytes each = 2000 bytes. Stats now read disk-truth.
        create_fake_cache(
            &storage,
            "a",
            "hash_a",
            3,
            1000,
            "2026-01-01T00:00:00+00:00",
        );
        create_fake_cache(
            &storage,
            "b",
            "hash_b",
            5,
            2000,
            "2026-01-02T00:00:00+00:00",
        );

        let stats = get_cache_stats(&storage);
        assert_eq!(stats.book_count, 2);
        assert_eq!(stats.books.len(), 2);
        // Disk-truth excludes the manifest. 333*3 + 400*5 = 2999.
        assert_eq!(stats.total_size_bytes, 2999);
    }

    #[test]
    fn clear_cache_removes_all_entries() {
        let (_d, storage) = temp_storage();

        create_fake_cache(&storage, "a", "hash_a", 2, 100, "2026-01-01T00:00:00+00:00");
        assert!(!storage.list(CACHE_PREFIX).unwrap().is_empty());

        clear_cache(&storage).unwrap();
        assert!(storage.list(CACHE_PREFIX).unwrap().is_empty());
    }

    #[test]
    fn clear_nonexistent_cache_no_error() {
        let (_d, storage) = temp_storage();
        // No cache created — clearing should be fine
        assert!(clear_cache(&storage).is_ok());
    }

    // --- New behavior (#64 M5) ---

    #[test]
    fn collect_cached_books_groups_keys_by_hash() {
        // Exercises storage.list()-based enumeration: multiple books, each
        // with a manifest + several pages. collect_cached_books must group
        // all keys under a single `{hash}` into one manifest entry.
        let (_d, storage) = temp_storage();
        let now = now_iso();
        create_fake_cache(&storage, "a", "hash_a", 3, 300, &now);
        create_fake_cache(&storage, "b", "hash_b", 4, 400, &now);
        create_fake_cache(&storage, "c", "hash_c", 1, 100, &now);

        let mut books = collect_cached_books(&storage);
        books.sort_by(|a, b| a.book_hash.cmp(&b.book_hash));

        assert_eq!(books.len(), 3);
        assert_eq!(books[0].book_hash, "hash_a");
        assert_eq!(books[0].page_count, 3);
        assert_eq!(books[1].book_hash, "hash_b");
        assert_eq!(books[1].page_count, 4);
        assert_eq!(books[2].book_hash, "hash_c");
        assert_eq!(books[2].page_count, 1);
    }

    /// A `Storage` that fails writes after the first `put_limit` succeed and
    /// counts `list` calls. Both knobs are needed to pin `extract_comic_full`'s
    /// cleanup: a partial write (some pages down, then a failure) and a
    /// zero-write failure look identical from the outside — the prefix is
    /// empty either way — so the difference has to be observed as whether the
    /// cleanup ran at all, and `list` is what it costs.
    struct FlakyStorage {
        inner: LocalStorage,
        put_limit: usize,
        writes: std::sync::atomic::AtomicUsize,
        lists: std::sync::atomic::AtomicUsize,
    }

    impl FlakyStorage {
        fn new(root: std::path::PathBuf, put_limit: usize) -> Self {
            Self {
                inner: LocalStorage::new(root).unwrap(),
                put_limit,
                writes: std::sync::atomic::AtomicUsize::new(0),
                lists: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn lists(&self) -> usize {
            self.lists.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Storage for FlakyStorage {
        fn put(&self, key: &str, bytes: &[u8]) -> CarrelResult<()> {
            let n = self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.put_limit {
                return Err(CarrelError::io("disk full (test)"));
            }
            self.inner.put(key, bytes)
        }
        fn get(&self, key: &str) -> CarrelResult<Vec<u8>> {
            self.inner.get(key)
        }
        fn exists(&self, key: &str) -> CarrelResult<bool> {
            self.inner.exists(key)
        }
        fn delete(&self, key: &str) -> CarrelResult<()> {
            self.inner.delete(key)
        }
        fn list(&self, prefix: &str) -> CarrelResult<Vec<String>> {
            self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.list(prefix)
        }
        fn size(&self, key: &str) -> CarrelResult<u64> {
            self.inner.size(key)
        }
        fn local_path(&self, key: &str) -> CarrelResult<std::path::PathBuf> {
            self.inner.local_path(key)
        }
    }

    /// A small, valid multi-page CBZ for the cleanup tests.
    fn write_cleanup_fixture_cbz(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("book.cbz");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for i in 0..5 {
            zip.start_file(format!("{i:03}.jpg"), options).unwrap();
            std::io::Write::write_all(&mut zip, b"\xff\xd8\xff\xe0jpeg-ish").unwrap();
        }
        zip.finish().unwrap();
        path
    }

    /// M3 review round 8: a failure that wrote nothing must not pay for a
    /// cleanup. `evict_book` lists the book's prefix, and
    /// `LocalStorage::list` walks the whole cache root before filtering, so
    /// cleaning up after a write that never landed would walk the entire
    /// cache on every request — on a route an unauthenticated LAN client can
    /// loop.
    ///
    /// Observed as the `list` count, because the visible outcome (an empty
    /// prefix) is the same whether or not the cleanup ran.
    #[test]
    fn a_failure_that_wrote_nothing_does_not_walk_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // put_limit 0: `comic_page_names` still succeeds, so extraction
        // starts, but the very first page write fails.
        let storage = FlakyStorage::new(dir.path().join("cache"), 0);
        let path = write_cleanup_fixture_cbz(dir.path());

        let result = ensure_cached(
            &storage,
            "bk",
            "hash-nowrite",
            path.to_str().unwrap(),
            &BookFormat::Cbz,
        );

        assert!(result.is_err(), "the failing write must fail the extract");
        assert_eq!(
            storage.lists(),
            0,
            "an extraction that wrote nothing must not list (and so walk) the cache"
        );
    }

    /// M3 review round 7: `extract_comic_full` is all-or-nothing. A failure
    /// after it has already written page blobs must leave the cache exactly
    /// as it found it, not a pile of manifest-less bytes that nothing
    /// accounts for.
    ///
    /// This invariant is load-bearing for `reader::page_image`, which
    /// decides whether to run its caller's eviction hook from this
    /// function's result alone. Without it the caller has to guess whether
    /// bytes were written, and several review rounds showed both guesses are
    /// wrong: skip the sweep after a real extraction and the cache sits over
    /// budget; run it after an archive that never opened and a LAN client
    /// can loop one broken comic into a full-cache walk per request.
    #[test]
    fn a_failed_extraction_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        // Two pages land, the third write fails: a real partial write.
        let storage = FlakyStorage::new(dir.path().join("cache"), 2);
        let path = write_cleanup_fixture_cbz(dir.path());

        let result = ensure_cached(
            &storage,
            "bk",
            "hash-partial",
            path.to_str().unwrap(),
            &BookFormat::Cbz,
        );
        assert!(result.is_err(), "the failing write must fail the extract");

        let leftovers = storage.list(&book_prefix("hash-partial")).unwrap();
        assert!(
            leftovers.is_empty(),
            "a failed extraction must leave nothing behind: {leftovers:?}"
        );
    }

    #[test]
    fn evict_book_removes_only_that_books_keys() {
        let (_d, storage) = temp_storage();
        let now = now_iso();
        create_fake_cache(&storage, "a", "hash_a", 2, 100, &now);
        create_fake_cache(&storage, "b", "hash_b", 2, 100, &now);

        evict_book(&storage, "hash_a").unwrap();

        // hash_a gone; hash_b untouched
        assert!(read_manifest(&storage, "hash_a").is_none());
        assert!(read_manifest(&storage, "hash_b").is_some());

        // No stray hash_a keys remain
        let remaining_a = storage.list(&book_prefix("hash_a")).unwrap();
        assert!(remaining_a.is_empty());

        // hash_b still has its manifest + 2 pages
        let remaining_b = storage.list(&book_prefix("hash_b")).unwrap();
        assert_eq!(remaining_b.len(), 3);
    }

    #[test]
    fn get_cached_page_reads_bytes_through_storage() {
        let (_d, storage) = temp_storage();
        let now = now_iso();
        // 2 pages × 50 bytes each = 100 bytes total (matches the
        // total_size_bytes claim so disk-truth eviction lines up).
        create_fake_cache(&storage, "a", "hash_a", 2, 100, &now);

        let (data, mime) = get_cached_page(&storage, "hash_a", 0).unwrap();
        assert_eq!(data.len(), 50);
        assert!(data.iter().all(|&b| b == 0));
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn get_cached_page_out_of_range_errors() {
        let (_d, storage) = temp_storage();
        let now = now_iso();
        create_fake_cache(&storage, "a", "hash_a", 2, 100, &now);

        assert!(get_cached_page(&storage, "hash_a", 99).is_err());
    }

    #[test]
    fn ensure_cached_hit_updates_last_accessed() {
        let (_d, storage) = temp_storage();
        create_fake_cache(&storage, "a", "hash_a", 1, 100, "2020-01-01T00:00:00+00:00");

        // Use Cbz format — with an existing valid manifest, the archive is
        // never touched, so the file_path can be bogus.
        let result = ensure_cached(&storage, "a", "hash_a", "/nope.cbz", &BookFormat::Cbz).unwrap();

        // `last_accessed` got bumped to ~now
        assert_ne!(result.last_accessed, "2020-01-01T00:00:00+00:00");
        // Persisted change is readable
        let reloaded = read_manifest(&storage, "hash_a").unwrap();
        assert_ne!(reloaded.last_accessed, "2020-01-01T00:00:00+00:00");
    }

    // ---------------------------------------------------------------------
    // PDF cache tests
    // ---------------------------------------------------------------------

    #[test]
    fn manifest_legacy_comic_loads_without_format_or_canonical_width() {
        let (_d, storage) = temp_storage();
        let hash = "legacy-hash";
        // Hand-craft a manifest JSON missing the new fields, exactly as
        // pre-spec comic manifests on disk.
        let legacy = serde_json::json!({
            "book_id": "legacy",
            "book_hash": hash,
            "page_count": 2,
            "total_size_bytes": 42,
            "extracted_at": "2026-01-01T00:00:00Z",
            "last_accessed": "2026-01-01T00:00:00Z",
            "pages": ["000.jpg", "001.jpg"],
        });
        storage
            .put(&manifest_key(hash), legacy.to_string().as_bytes())
            .unwrap();

        let loaded = read_manifest(&storage, hash).expect("legacy manifest must load");
        assert_eq!(loaded.format, BookFormat::Cbz);
        assert_eq!(loaded.canonical_width, None);
    }

    #[test]
    fn get_cached_page_pdf_derives_filename_from_index() {
        let (_d, storage) = temp_storage();
        let hash = "pdf-hash";

        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 50,
            total_size_bytes: 0,
            extracted_at: now_iso(),
            last_accessed: now_iso(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();
        storage
            .put(&page_key(hash, "042.jpg"), b"pdf-page-bytes")
            .unwrap();

        let (bytes, mime) = get_cached_page(&storage, hash, 42).unwrap();
        assert_eq!(bytes, b"pdf-page-bytes");
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn get_cached_page_pdf_out_of_range() {
        let (_d, storage) = temp_storage();
        let hash = "pdf-hash";
        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 10,
            total_size_bytes: 0,
            extracted_at: now_iso(),
            last_accessed: now_iso(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();

        let err = get_cached_page(&storage, hash, 999).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("out of range"), "got: {msg}");
    }

    #[test]
    fn ensure_pdf_prewarmed_writes_first_n_pages() {
        let (_d, storage) = temp_storage();
        let hash = "warm-hash";

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("page-{idx}").into_bytes(), "image/jpeg".into()))
        };

        let manifest = ensure_pdf_prewarmed_with_renderer(
            &storage, "book", hash, /*page_count=*/ 25, /*prewarm=*/ 10, render,
        )
        .unwrap();

        assert_eq!(manifest.format, BookFormat::Pdf);
        assert_eq!(manifest.canonical_width, Some(2400));
        assert!(manifest.pages.is_empty(), "PDF manifests keep pages empty");
        assert_eq!(manifest.page_count, 25);

        for i in 0..10 {
            let key = page_key(hash, &format!("{i:03}.jpg"));
            assert!(storage.exists(&key).unwrap(), "page {i} should be on disk");
        }
        // Pages beyond the prewarm window are not pre-rendered.
        assert!(!storage.exists(&page_key(hash, "010.jpg")).unwrap());
    }

    #[test]
    fn ensure_pdf_prewarmed_with_zero_prewarm_establishes_manifest_without_rendering() {
        // prepare_pdf relies on this: it establishes the cache manifest (so the
        // cache-first read path and the background prerender pass both engage)
        // with a zero prewarm — no page-range render — then reserves only the
        // single page the reader opens on. A non-zero prewarm here would
        // reintroduce the multi-page synchronous open blocking this fixes.
        let (_d, storage) = temp_storage();
        let hash = "warm-hash";

        let calls = std::cell::Cell::new(0u32);
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            calls.set(calls.get() + 1);
            Ok((format!("page-{idx}").into_bytes(), "image/jpeg".into()))
        };

        let manifest = ensure_pdf_prewarmed_with_renderer(
            &storage, "book", hash, /*page_count=*/ 25, /*prewarm=*/ 0, render,
        )
        .unwrap();

        assert_eq!(calls.get(), 0, "zero prewarm must not render any page");
        assert_eq!(manifest.page_count, 25, "page_count is still established");
        assert_eq!(manifest.format, BookFormat::Pdf);
        assert!(
            !storage.exists(&page_key(hash, "000.jpg")).unwrap(),
            "no page is written synchronously"
        );
        assert!(
            read_manifest(&storage, hash).is_some(),
            "manifest is persisted so the cache paths engage"
        );
    }

    #[test]
    fn ensure_pdf_prewarmed_is_idempotent() {
        let (_d, storage) = temp_storage();
        let hash = "warm-hash";

        let calls = std::cell::Cell::new(0u32);
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            calls.set(calls.get() + 1);
            Ok((format!("page-{idx}").into_bytes(), "image/jpeg".into()))
        };

        ensure_pdf_prewarmed_with_renderer(&storage, "book", hash, 25, 5, render).unwrap();
        let first_calls = calls.get();

        ensure_pdf_prewarmed_with_renderer(&storage, "book", hash, 25, 5, render).unwrap();
        assert_eq!(
            calls.get(),
            first_calls,
            "second prewarm with cache intact must not re-render"
        );
    }

    #[test]
    fn ensure_pdf_prewarmed_disk_write_failure_aborts_and_rolls_back() {
        use crate::storage::Storage;

        // Custom storage stub that fails the third put(). Uses
        // AtomicU32 instead of Cell so the type is Sync (the Storage
        // trait requires Send + Sync).
        struct FailingStorage {
            inner: LocalStorage,
            fail_after: std::sync::atomic::AtomicU32,
        }
        impl Storage for FailingStorage {
            fn get(&self, k: &str) -> CarrelResult<Vec<u8>> {
                self.inner.get(k)
            }
            fn put(&self, k: &str, v: &[u8]) -> CarrelResult<()> {
                let n = self.fail_after.load(std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    return Err(CarrelError::io("simulated disk failure"));
                }
                self.fail_after
                    .store(n - 1, std::sync::atomic::Ordering::SeqCst);
                self.inner.put(k, v)
            }
            fn delete(&self, k: &str) -> CarrelResult<()> {
                self.inner.delete(k)
            }
            fn list(&self, prefix: &str) -> CarrelResult<Vec<String>> {
                self.inner.list(prefix)
            }
            fn exists(&self, k: &str) -> CarrelResult<bool> {
                self.inner.exists(k)
            }
            fn size(&self, k: &str) -> CarrelResult<u64> {
                self.inner.size(k)
            }
            fn local_path(&self, k: &str) -> CarrelResult<std::path::PathBuf> {
                self.inner.local_path(k)
            }
        }

        let dir = TempDir::new().unwrap();
        let storage = FailingStorage {
            inner: LocalStorage::new(dir.path()).unwrap(),
            fail_after: std::sync::atomic::AtomicU32::new(3),
        };
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("page-{idx}").into_bytes(), "image/jpeg".into()))
        };

        let result = ensure_pdf_prewarmed_with_renderer(&storage, "book", "h", 25, 10, render);
        assert!(result.is_err(), "must surface disk failure");
        assert!(
            read_manifest(&storage, "h").is_none(),
            "manifest must not be persisted on partial failure"
        );
        // Rollback must wipe the partial page files as well — otherwise
        // collect_cached_books cannot count them and eviction can never
        // reclaim the space.
        let remaining_pages: Vec<String> = storage
            .list(&book_prefix("h"))
            .unwrap_or_default()
            .into_iter()
            .filter(|k| !k.ends_with("manifest.json"))
            .collect();
        assert!(
            remaining_pages.is_empty(),
            "partial cache must be rolled back; orphans: {remaining_pages:?}"
        );
    }

    // ---------------------------------------------------------------------
    // PDF background prerender (F-4-5)
    // ---------------------------------------------------------------------

    /// Write a bare PDF manifest with `page_count` pages and the first
    /// `prewarmed` page files present on disk (each `bytes_per_page` bytes).
    fn seed_pdf_manifest(
        storage: &dyn Storage,
        hash: &str,
        page_count: u32,
        prewarmed: u32,
        bytes_per_page: usize,
    ) {
        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count,
            total_size_bytes: (prewarmed as u64) * bytes_per_page as u64,
            extracted_at: now_iso(),
            last_accessed: now_iso(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(storage, hash, &manifest).unwrap();
        for i in 0..prewarmed {
            storage
                .put(
                    &page_key(hash, &format!("{i:03}.jpg")),
                    &vec![0u8; bytes_per_page],
                )
                .unwrap();
        }
    }

    #[test]
    fn prerender_pdf_remaining_renders_only_missing_pages() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        // 10 pages, first 3 prewarmed.
        seed_pdf_manifest(&storage, hash, 10, 3, 8);

        let rendered_idxs = std::cell::RefCell::new(Vec::<u32>::new());
        let progress = std::cell::RefCell::new(Vec::<(u32, u32)>::new());
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            rendered_idxs.borrow_mut().push(idx);
            Ok((vec![7u8; 8], "image/jpeg".into()))
        };

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |c, t| progress.borrow_mut().push((c, t)),
            || false,
            || {},
        )
        .unwrap();

        // Only pages 3..10 rendered, in order.
        assert_eq!(rendered_idxs.into_inner(), (3..10).collect::<Vec<_>>());
        assert_eq!(outcome.rendered, 7);
        assert_eq!(outcome.cached_total, 10);
        assert_eq!(outcome.page_count, 10);
        assert!(!outcome.stopped_early);

        // Every page now on disk.
        for i in 0..10 {
            assert!(storage
                .exists(&page_key(hash, &format!("{i:03}.jpg")))
                .unwrap());
        }

        // Progress starts at the prewarmed count and ends at full coverage.
        let p = progress.into_inner();
        assert_eq!(p.first(), Some(&(3, 10)));
        assert_eq!(p.last(), Some(&(10, 10)));
    }

    #[test]
    fn prerender_pdf_remaining_throttles_before_each_render() {
        // The background pass must call `throttle` before every page render so
        // the command layer can pause it while a foreground render is in flight.
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 4, 0, 0);

        let log = std::cell::RefCell::new(Vec::<&'static str>::new());
        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            log.borrow_mut().push("render");
            Ok((vec![1u8; 10], "image/jpeg".into()))
        };
        let throttle = || log.borrow_mut().push("throttle");

        prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            || false,
            throttle,
        )
        .unwrap();

        // Exactly one throttle immediately before each of the 4 renders.
        assert_eq!(
            log.into_inner(),
            vec![
                "throttle", "render", "throttle", "render", "throttle", "render", "throttle",
                "render",
            ]
        );
    }

    #[test]
    fn prerender_pdf_remaining_noop_when_all_cached() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 5, 5, 8);

        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            panic!("renderer must not be called when everything is cached");
        };

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();
        assert_eq!(outcome.rendered, 0);
        assert_eq!(outcome.cached_total, 5);
        assert!(!outcome.stopped_early);
    }

    #[test]
    fn prerender_pdf_remaining_noop_without_manifest() {
        let (_d, storage) = temp_storage();
        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            panic!("renderer must not run without a manifest");
        };
        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            "nope",
            u64::MAX,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();
        assert_eq!(outcome.page_count, 0);
        assert_eq!(outcome.rendered, 0);
        assert!(!outcome.stopped_early);
    }

    #[test]
    fn prerender_pdf_remaining_stops_at_size_bound() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        // 20 pages, none prewarmed, each rendered page is 100 bytes.
        seed_pdf_manifest(&storage, hash, 20, 0, 0);

        // Bound at 250 bytes: pages 0 (0B pre), then render adds 100B each.
        // Rendering stops once on-disk size reaches the bound, so we get
        // exactly pages 0,1,2 (size 0 -> 100 -> 200, then 300 >= 250 stops).
        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((vec![9u8; 100], "image/jpeg".into()))
        };

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            250,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();

        assert!(outcome.stopped_early, "should stop at the size bound");
        assert_eq!(outcome.rendered, 3, "3 pages fit before crossing 250 B");
        assert_eq!(outcome.cached_total, 3);
        assert_eq!(outcome.page_count, 20);
        // Later pages were NOT rendered — served on-demand instead.
        assert!(!storage.exists(&page_key(hash, "003.jpg")).unwrap());
    }

    #[test]
    fn prerender_pdf_remaining_stops_when_abort_trips() {
        // Private mode toggled on mid-pass (B-M1): `should_abort` flips to
        // true after two pages land, and the pass must stop writing further
        // pages to disk — mirroring the on-demand read path's write
        // suppression. Pages already written stay; the rest are left for
        // on-demand rendering.
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 10, 0, 0);

        let rendered_count = std::cell::Cell::new(0u32);
        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            rendered_count.set(rendered_count.get() + 1);
            Ok((vec![1u8; 10], "image/jpeg".into()))
        };
        // Abort once two pages have been written.
        let should_abort = || rendered_count.get() >= 2;

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            should_abort,
            || {},
        )
        .unwrap();

        assert_eq!(outcome.rendered, 2, "must stop after the abort flag trips");
        assert_eq!(outcome.cached_total, 2);
        assert_eq!(outcome.page_count, 10);
        // The size bound was never hit — the stop was the abort, so
        // `stopped_early` (a size-bound signal) stays false.
        assert!(!outcome.stopped_early);
        // Pages 0,1 written; page 2 onward left for on-demand.
        assert!(storage.exists(&page_key(hash, "000.jpg")).unwrap());
        assert!(storage.exists(&page_key(hash, "001.jpg")).unwrap());
        assert!(!storage.exists(&page_key(hash, "002.jpg")).unwrap());
    }

    #[test]
    fn prerender_pdf_remaining_continues_past_single_render_failure() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 4, 0, 0);

        // Page 1 fails; the rest still render.
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            if idx == 1 {
                Err(CarrelError::internal("boom"))
            } else {
                Ok((vec![1u8; 10], "image/jpeg".into()))
            }
        };

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();

        assert_eq!(outcome.rendered, 3);
        assert!(!storage.exists(&page_key(hash, "001.jpg")).unwrap());
        assert!(storage.exists(&page_key(hash, "000.jpg")).unwrap());
        assert!(storage.exists(&page_key(hash, "003.jpg")).unwrap());
        // Not "stopped early" — the bound was never hit.
        assert!(!outcome.stopped_early);
    }

    #[test]
    fn prerender_pdf_remaining_refreshes_manifest_size() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 3, 0, 0);
        // Manifest claims 0 bytes initially.
        assert_eq!(read_manifest(&storage, hash).unwrap().total_size_bytes, 0);

        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((vec![5u8; 40], "image/jpeg".into()))
        };
        prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();

        // Snapshot refreshed to disk-truth: 3 pages * 40 bytes.
        assert_eq!(read_manifest(&storage, hash).unwrap().total_size_bytes, 120);
    }

    #[test]
    fn prerender_pdf_remaining_cleans_orphans_if_manifest_evicted_midrun() {
        // Simulate a concurrent `run_eviction` deleting our manifest partway
        // through: the render closure removes the manifest after the first
        // page. Pages written afterward would be orphans; the pass must
        // evict the whole book so they cannot defeat the size cap.
        let (_d, storage) = temp_storage();
        let hash = "h";
        seed_pdf_manifest(&storage, hash, 4, 0, 0);

        let calls = std::cell::Cell::new(0u32);
        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            let n = calls.get();
            calls.set(n + 1);
            if n == 1 {
                // After the first successful page, wipe the manifest to
                // mimic eviction reclaiming this book mid-run.
                storage.delete(&manifest_key(hash)).unwrap();
            }
            Ok((vec![3u8; 10], "image/jpeg".into()))
        };

        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            hash,
            u64::MAX,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();

        // The pass rendered pages, but with the manifest gone at the end the
        // book (including every orphan page) is scrubbed.
        assert!(outcome.rendered >= 1);
        // Coverage is reported as disk-truth (0) after the scrub, so the
        // caller's terminal event does not overstate what is cached.
        assert_eq!(outcome.cached_total, 0);
        let leftovers: Vec<String> = storage.list(&book_prefix(hash)).unwrap();
        assert!(
            leftovers.is_empty(),
            "orphan pages must be cleaned up; leftovers: {leftovers:?}"
        );
    }

    #[test]
    fn prerender_pdf_remaining_bound_counts_whole_cache() {
        // The size bound must account for OTHER cached books, not just this
        // one — otherwise the aggregate cache can grow to ~2x the cap before
        // eviction runs. A pre-existing book already fills most of the cap,
        // leaving room for only a couple of this book's pages.
        let (_d, storage) = temp_storage();
        // Existing book "other": 5 pages * 40 B = 200 B on disk.
        create_fake_cache(&storage, "other", "other", 5, 200, &now_iso());

        // Target PDF "h": 20 pages, none prewarmed, 40 B per rendered page.
        seed_pdf_manifest(&storage, "h", 20, 0, 0);

        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((vec![9u8; 40], "image/jpeg".into()))
        };

        // Cap 300 B. Start total = 200 B (the other book). Render page 0
        // (240) and page 1 (280); page 2 would be checked at 280 < 300 so it
        // also renders (320), then 320 >= 300 stops. So 3 pages fit.
        let outcome = prerender_pdf_remaining_with_renderer(
            &storage,
            "h",
            300,
            render,
            |_, _| {},
            || false,
            || {},
        )
        .unwrap();

        assert!(
            outcome.stopped_early,
            "must stop once the whole cache hits the cap"
        );
        assert_eq!(outcome.rendered, 3);
        // The pre-existing book is untouched by the prerender pass.
        assert!(read_manifest(&storage, "other").is_some());
    }

    #[test]
    fn get_or_render_pdf_page_disk_hit_skips_renderer() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 10,
            total_size_bytes: 0,
            extracted_at: now_iso(),
            last_accessed: now_iso(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();
        storage
            .put(&page_key(hash, "003.jpg"), b"cached-bytes")
            .unwrap();

        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            panic!("renderer must not be called on cache hit");
        };

        let (bytes, mime) =
            get_or_render_pdf_page_with_renderer(&storage, hash, 3, render, || {}, false).unwrap();
        assert_eq!(bytes, b"cached-bytes");
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn get_or_render_pdf_page_miss_writes_disk_and_updates_manifest() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        let baseline_extracted = "2026-01-01T00:00:00+00:00";
        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 50,
            total_size_bytes: 100,
            extracted_at: baseline_extracted.into(),
            last_accessed: baseline_extracted.into(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };

        let (bytes, _) =
            get_or_render_pdf_page_with_renderer(&storage, hash, 42, render, || {}, false).unwrap();
        assert_eq!(bytes, b"p42");

        assert!(storage.exists(&page_key(hash, "042.jpg")).unwrap());
        let updated = read_manifest(&storage, hash).unwrap();
        // total_size_bytes is the warm-time snapshot; lazy writes do
        // NOT update it (would race). Eviction uses disk-truth via
        // `book_disk_size_bytes`. last_accessed is updated so LRU
        // sees the touch.
        assert_eq!(updated.total_size_bytes, 100);
        assert_ne!(updated.last_accessed, baseline_extracted);
        assert!(updated.pages.is_empty(), "PDF manifests keep pages empty");
        // Disk-truth: the new page contributes its bytes.
        assert_eq!(book_disk_size_bytes(&storage, hash), b"p42".len() as u64);
    }

    #[test]
    fn get_or_render_pdf_page_out_of_range_errors_without_rendering() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        write_manifest(
            &storage,
            hash,
            &CacheManifest {
                book_id: "b".into(),
                book_hash: hash.into(),
                page_count: 10,
                total_size_bytes: 0,
                extracted_at: now_iso(),
                last_accessed: now_iso(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();

        let render = |_idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            panic!("renderer must not run for out-of-range index");
        };

        let err = get_or_render_pdf_page_with_renderer(&storage, hash, 999, render, || {}, false)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("out of range"), "got: {msg}");
    }

    #[test]
    fn get_or_render_pdf_page_missing_manifest_falls_back_to_render_only() {
        let (_d, storage) = temp_storage();
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };

        let (bytes, _) =
            get_or_render_pdf_page_with_renderer(&storage, "nope", 0, render, || {}, false)
                .unwrap();
        assert_eq!(bytes, b"p0");
        // No manifest → no cache writes.
        assert!(!storage.exists(&page_key("nope", "000.jpg")).unwrap());
    }

    #[test]
    fn get_or_render_pdf_page_swallows_write_failure() {
        use crate::storage::Storage;

        // Storage that fails all puts EXCEPT manifest writes.
        struct PageWriteFails {
            inner: LocalStorage,
        }
        impl Storage for PageWriteFails {
            fn get(&self, k: &str) -> CarrelResult<Vec<u8>> {
                self.inner.get(k)
            }
            fn put(&self, k: &str, v: &[u8]) -> CarrelResult<()> {
                if k.ends_with("manifest.json") {
                    self.inner.put(k, v)
                } else {
                    Err(CarrelError::io("simulated"))
                }
            }
            fn delete(&self, k: &str) -> CarrelResult<()> {
                self.inner.delete(k)
            }
            fn list(&self, p: &str) -> CarrelResult<Vec<String>> {
                self.inner.list(p)
            }
            fn exists(&self, k: &str) -> CarrelResult<bool> {
                self.inner.exists(k)
            }
            fn size(&self, k: &str) -> CarrelResult<u64> {
                self.inner.size(k)
            }
            fn local_path(&self, k: &str) -> CarrelResult<std::path::PathBuf> {
                self.inner.local_path(k)
            }
        }
        let dir = TempDir::new().unwrap();
        let storage = PageWriteFails {
            inner: LocalStorage::new(dir.path()).unwrap(),
        };
        let hash = "h";
        write_manifest(
            &storage,
            hash,
            &CacheManifest {
                book_id: "b".into(),
                book_hash: hash.into(),
                page_count: 10,
                total_size_bytes: 0,
                extracted_at: now_iso(),
                last_accessed: now_iso(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };
        let (bytes, _) =
            get_or_render_pdf_page_with_renderer(&storage, hash, 5, render, || {}, false).unwrap();
        assert_eq!(bytes, b"p5");
        // Cache write failed silently; manifest size unchanged.
        let m = read_manifest(&storage, hash).unwrap();
        assert_eq!(m.total_size_bytes, 0);
    }

    #[test]
    fn lazy_eviction_callback_fires_every_batch() {
        let (_d, storage) = temp_storage();
        let hash = "h";
        write_manifest(
            &storage,
            hash,
            &CacheManifest {
                book_id: "b".into(),
                book_hash: hash.into(),
                page_count: 200,
                total_size_bytes: 0,
                extracted_at: now_iso(),
                last_accessed: now_iso(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };
        let calls = std::cell::Cell::new(0u32);
        let on_batch = || calls.set(calls.get() + 1);

        // Reset the global counter so the test is deterministic.
        reset_lazy_eviction_counter_for_tests();

        for i in 0..LAZY_EVICTION_BATCH * 2 {
            get_or_render_pdf_page_with_renderer(&storage, hash, i, render, on_batch, false)
                .unwrap();
        }

        assert_eq!(calls.get(), 2, "callback fires exactly once per batch");
    }

    #[test]
    fn ensure_cached_pdf_does_not_evict_via_comic_validation() {
        // Regression test for the PR review finding: PDF manifests
        // have empty `pages`, so the comic-style first/last validation
        // in `ensure_cached` previously reported every warm PDF as
        // corrupt and called `evict_book` *before* any pdfium I/O.
        // After the fix `ensure_cached` short-circuits to
        // `ensure_pdf_prewarmed`, which only mutates cache state on
        // an actual render/write failure (and rolls back partials).
        //
        // We exercise this without a real pdfium dylib: pass a
        // non-existent path so `ensure_pdf_prewarmed` fails at
        // `get_page_count`. The OLD code would have evicted before
        // that failure surfaced; the NEW code propagates the error
        // with the cache intact.
        let (_d, storage) = temp_storage();
        let hash = "warm-pdf";

        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 25,
            total_size_bytes: 0,
            extracted_at: now_iso(),
            last_accessed: now_iso(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();
        // Seed a lazily cached page beyond the prewarm window — the
        // bug would have wiped this via evict_book.
        storage.put(&page_key(hash, "020.jpg"), b"lazy").unwrap();

        let _ = ensure_cached(&storage, "b", hash, "/nonexistent.pdf", &BookFormat::Pdf);

        // Lazy page must survive regardless of whether the PDF
        // open succeeded (it won't in tests without pdfium).
        assert!(
            storage.exists(&page_key(hash, "020.jpg")).unwrap(),
            "ensure_cached must not evict warm PDF state via comic-style validation"
        );
        assert!(
            read_manifest(&storage, hash).is_some(),
            "ensure_cached must not delete the PDF manifest via comic-style validation"
        );
    }

    // --- Private mode (B-M1): suppress_write boundary ---

    #[test]
    fn get_or_render_pdf_page_suppressed_skips_disk_write_but_bumps_last_accessed() {
        let (_d, storage) = temp_storage();
        let hash = "priv-hash";
        let baseline = "2020-01-01T00:00:00+00:00";
        let manifest = CacheManifest {
            book_id: "b".into(),
            book_hash: hash.into(),
            page_count: 10,
            total_size_bytes: 0,
            extracted_at: baseline.into(),
            last_accessed: baseline.into(),
            pages: Vec::new(),
            format: BookFormat::Pdf,
            canonical_width: Some(2400),
        };
        write_manifest(&storage, hash, &manifest).unwrap();

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((
                format!("secret-page-{idx}").into_bytes(),
                "image/jpeg".into(),
            ))
        };

        let (bytes, _) =
            get_or_render_pdf_page_with_renderer(&storage, hash, 3, render, || {}, true).unwrap();
        assert_eq!(
            bytes, b"secret-page-3",
            "bytes must still be returned from the render fast path"
        );

        assert!(
            !storage.exists(&page_key(hash, "003.jpg")).unwrap(),
            "private render must never write page bytes to disk"
        );
        let updated = read_manifest(&storage, hash).unwrap();
        assert_ne!(
            updated.last_accessed, baseline,
            "last_accessed must still be bumped so the book isn't evicted as stale"
        );
    }

    #[test]
    fn get_or_render_pdf_page_suppressed_never_triggers_lazy_eviction_batch() {
        // Suppressed writes must not advance the shared lazy-write
        // counter or fire `on_batch` — there is no disk write to coalesce.
        let (_d, storage) = temp_storage();
        let hash = "priv-hash-2";
        write_manifest(
            &storage,
            hash,
            &CacheManifest {
                book_id: "b".into(),
                book_hash: hash.into(),
                page_count: 200,
                total_size_bytes: 0,
                extracted_at: now_iso(),
                last_accessed: now_iso(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();

        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };
        let calls = std::cell::Cell::new(0u32);
        let on_batch = || calls.set(calls.get() + 1);

        reset_lazy_eviction_counter_for_tests();
        for i in 0..LAZY_EVICTION_BATCH * 2 {
            get_or_render_pdf_page_with_renderer(&storage, hash, i, render, on_batch, true)
                .unwrap();
        }

        assert_eq!(
            calls.get(),
            0,
            "suppressed writes must never trigger an eviction batch"
        );
    }

    #[test]
    fn run_eviction_does_not_preferentially_evict_a_privately_read_book() {
        let (_d, storage) = temp_storage();
        let old_ts = "2020-01-01T00:00:00+00:00";

        // "stale": warmed long ago and never touched since — must age-expire.
        let stale_hash = "stale-hash";
        write_manifest(
            &storage,
            stale_hash,
            &CacheManifest {
                book_id: "stale".into(),
                book_hash: stale_hash.into(),
                page_count: 10,
                total_size_bytes: 0,
                extracted_at: old_ts.into(),
                last_accessed: old_ts.into(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();

        // "priv": same old warm-time snapshot, but read privately just now.
        // last_accessed must have been bumped even though no page bytes
        // ever hit disk for that read.
        let priv_hash = "priv-hash-evict";
        write_manifest(
            &storage,
            priv_hash,
            &CacheManifest {
                book_id: "priv".into(),
                book_hash: priv_hash.into(),
                page_count: 10,
                total_size_bytes: 0,
                extracted_at: old_ts.into(),
                last_accessed: old_ts.into(),
                pages: Vec::new(),
                format: BookFormat::Pdf,
                canonical_width: Some(2400),
            },
        )
        .unwrap();
        let render = |idx: u32| -> CarrelResult<(Vec<u8>, String)> {
            Ok((format!("p{idx}").into_bytes(), "image/jpeg".into()))
        };
        get_or_render_pdf_page_with_renderer(&storage, priv_hash, 0, render, || {}, true).unwrap();

        run_eviction(&storage, DEFAULT_MAX_CACHE_SIZE_MB).unwrap();

        assert!(
            read_manifest(&storage, stale_hash).is_none(),
            "the untouched stale book should be age-expired"
        );
        assert!(
            read_manifest(&storage, priv_hash).is_some(),
            "the privately-read book must survive eviction thanks to its bumped last_accessed"
        );
        // And its page content was still never written to disk.
        assert!(!storage.exists(&page_key(priv_hash, "000.jpg")).unwrap());
    }

    // ---------------------------------------------------------------------
    // F-4-1: progressive comic open (fast path + background extraction)
    // ---------------------------------------------------------------------

    /// Build a synthetic CBZ (a zip of tiny "image" blobs) on disk. Entries
    /// are written in the given order; the readers sort them canonically.
    fn build_cbz(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        use std::io::Write as _;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    fn four_page_cbz(dir: &TempDir) -> String {
        let cbz = dir.path().join("comic.cbz");
        build_cbz(
            &cbz,
            &[
                ("p1.jpg", b"AAAA"),
                ("p2.jpg", b"BBBB"),
                ("p3.jpg", b"CCCC"),
                ("p4.jpg", b"DDDD"),
                // macOS resource fork with an image extension — the on-demand
                // readers exclude it, so extraction must too or indices drift.
                ("__MACOSX/._p1.jpg", b"junkjunk"),
            ],
        );
        cbz.to_str().unwrap().to_string()
    }

    #[test]
    fn fast_path_extracts_only_first_page() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        let manifest =
            ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();

        // __MACOSX image excluded → 4 real pages, full manifest listing.
        assert_eq!(manifest.page_count, 4);
        assert_eq!(manifest.pages.len(), 4);

        // Only page 0 is on disk immediately; the rest wait for background.
        assert!(storage.exists(&page_key("h", &manifest.pages[0])).unwrap());
        assert!(!storage.exists(&page_key("h", &manifest.pages[1])).unwrap());
        assert!(!storage.exists(&page_key("h", &manifest.pages[2])).unwrap());
        assert!(!storage.exists(&page_key("h", &manifest.pages[3])).unwrap());

        // Page 0 serves the right bytes right away.
        let (bytes, _) = get_cached_page(&storage, "h", 0).unwrap();
        assert_eq!(bytes, b"AAAA");
    }

    #[test]
    fn fast_path_extracts_priority_page_for_midbook_resume() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        // Opening mid-book at page 2: page 0 + page 2 must be immediate.
        let manifest =
            ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[2]).unwrap();

        assert!(storage.exists(&page_key("h", &manifest.pages[0])).unwrap());
        assert!(storage.exists(&page_key("h", &manifest.pages[2])).unwrap());
        assert!(!storage.exists(&page_key("h", &manifest.pages[1])).unwrap());
        assert!(!storage.exists(&page_key("h", &manifest.pages[3])).unwrap());

        let (b0, _) = get_cached_page(&storage, "h", 0).unwrap();
        assert_eq!(b0, b"AAAA");
        let (b2, _) = get_cached_page(&storage, "h", 2).unwrap();
        assert_eq!(b2, b"CCCC");
    }

    #[test]
    fn fast_path_out_of_range_priority_is_ignored() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        // A resume index past the end must not panic or error; page 0 still lands.
        let manifest =
            ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[99]).unwrap();
        assert_eq!(manifest.page_count, 4);
        assert!(storage.exists(&page_key("h", &manifest.pages[0])).unwrap());
    }

    #[test]
    fn unextracted_page_is_not_served_from_cache_before_background() {
        // get_cached_page must fail for a not-yet-extracted page so the
        // command layer falls through to its on-demand archive read.
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();
        assert!(get_cached_page(&storage, "h", 2).is_err());
    }

    #[test]
    fn background_extracts_all_remaining_pages_and_reports_progress() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        let manifest =
            ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();

        let mut samples: Vec<(u32, u32)> = Vec::new();
        extract_comic_remaining(&storage, "h", &path, &BookFormat::Cbz, |loaded, total| {
            samples.push((loaded, total));
        })
        .unwrap();

        // Every page now on disk.
        for i in 0..4 {
            assert!(
                storage.exists(&page_key("h", &manifest.pages[i])).unwrap(),
                "page {i} should be extracted by the background pass"
            );
        }

        // Progress is monotonic non-decreasing and finishes at total.
        assert!(!samples.is_empty());
        for w in samples.windows(2) {
            assert!(w[1].0 >= w[0].0, "progress must not go backwards");
        }
        let (last_loaded, last_total) = *samples.last().unwrap();
        assert_eq!(last_total, 4);
        assert_eq!(last_loaded, 4);

        // Ordering lock: each cached page equals a direct on-demand read of
        // the same index (proves cache index == fallback index).
        let expected = [&b"AAAA"[..], b"BBBB", b"CCCC", b"DDDD"];
        for i in 0..4u32 {
            let (cached, _) = get_cached_page(&storage, "h", i).unwrap();
            assert_eq!(
                cached, expected[i as usize],
                "cache bytes wrong for page {i}"
            );
            let (arch, _) = crate::cbz::get_page_image_bytes(&path, i, None).unwrap();
            assert_eq!(cached, arch, "cache/fallback mismatch for page {i}");
        }
    }

    #[test]
    fn background_is_idempotent() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();
        extract_comic_remaining(&storage, "h", &path, &BookFormat::Cbz, |_, _| {}).unwrap();
        // Re-running must be a no-op, not an error, and must not corrupt pages.
        extract_comic_remaining(&storage, "h", &path, &BookFormat::Cbz, |_, _| {}).unwrap();

        for i in 0..4u32 {
            assert!(get_cached_page(&storage, "h", i).is_ok());
        }
    }

    #[test]
    fn background_cleans_up_orphans_if_manifest_evicted_midrun() {
        // Simulates a concurrent `run_eviction` (from another book's open)
        // reclaiming this book's manifest while the background pass is still
        // extracting. The pass must not leave orphan page files behind:
        // `collect_cached_books` can't see a manifest-less hash, so eviction
        // could never reclaim them and they would defeat the size cap.
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        ensure_comic_fast(&storage, "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();
        // Delete just the manifest, mimicking the tail of an `evict_book` that
        // raced our extraction (pages then get written afterwards).
        storage.delete(&manifest_key("h")).unwrap();

        extract_comic_remaining(&storage, "h", &path, &BookFormat::Cbz, |_, _| {}).unwrap();

        assert!(read_manifest(&storage, "h").is_none());
        let leftover = storage.list(&book_prefix("h")).unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "orphan cache files left behind: {leftover:?}"
        );
    }

    #[test]
    fn ensure_comic_fast_returns_complete_cache_without_touching_archive() {
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        // Fully populate the cache first.
        extract_cbz(&storage, "book", "h", &path).unwrap();

        // With first+last present the fast path must short-circuit — a bogus
        // archive path proves it never reopens the file.
        let manifest = ensure_comic_fast(
            &storage,
            "book",
            "h",
            "/does/not/exist.cbz",
            &BookFormat::Cbz,
            &[],
        )
        .unwrap();
        assert_eq!(manifest.page_count, 4);
    }

    #[test]
    fn extract_cbz_excludes_macosx_and_matches_fallback_ordering() {
        // Full extraction (used by ensure_cached) must also use the canonical
        // ordering — otherwise a warm cache disagrees with on-demand reads.
        let (_sd, storage) = temp_storage();
        let book_dir = TempDir::new().unwrap();
        let path = four_page_cbz(&book_dir);

        let manifest = extract_cbz(&storage, "book", "h", &path).unwrap();
        assert_eq!(manifest.page_count, 4, "__MACOSX image must be excluded");
        for i in 0..4u32 {
            let (cached, _) = get_cached_page(&storage, "h", i).unwrap();
            let (arch, _) = crate::cbz::get_page_image_bytes(&path, i, None).unwrap();
            assert_eq!(cached, arch, "index {i} mismatch after full extraction");
        }
    }

    #[test]
    fn on_demand_reads_race_background_without_corruption() {
        use std::sync::Arc;
        use std::thread;

        let store_dir = TempDir::new().unwrap();
        let storage = Arc::new(LocalStorage::new(store_dir.path()).unwrap());
        let book_dir = TempDir::new().unwrap();
        let cbz = book_dir.path().join("big.cbz");

        // 30 uniquely-identifiable pages to widen the race window.
        let contents: Vec<(String, Vec<u8>)> = (0..30)
            .map(|i| (format!("p{i:02}.jpg"), format!("PAGE-{i:02}").into_bytes()))
            .collect();
        let entry_refs: Vec<(&str, &[u8])> = contents
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        build_cbz(&cbz, &entry_refs);
        let path = cbz.to_str().unwrap().to_string();

        // Fast path first (page 0 only).
        ensure_comic_fast(storage.as_ref(), "book", "h", &path, &BookFormat::Cbz, &[]).unwrap();

        // Background extraction on a second storage handle over the same root.
        let bg_storage = Arc::clone(&storage);
        let bg_path = path.clone();
        let handle = thread::spawn(move || {
            extract_comic_remaining(
                bg_storage.as_ref(),
                "h",
                &bg_path,
                &BookFormat::Cbz,
                |_, _| {},
            )
            .unwrap();
        });

        // Hammer cache reads concurrently. A miss (NotFound) is legitimate —
        // it is exactly when the command layer would serve on-demand from the
        // archive. Any HIT must return the correct, untruncated bytes.
        for _ in 0..400 {
            for i in 0..30u32 {
                if let Ok((bytes, _)) = get_cached_page(storage.as_ref(), "h", i) {
                    assert_eq!(
                        bytes,
                        format!("PAGE-{i:02}").into_bytes(),
                        "torn or mismatched cache read for page {i}"
                    );
                }
            }
        }

        handle.join().unwrap();

        // After the background pass every page is present and correct.
        for i in 0..30u32 {
            let (bytes, _) = get_cached_page(storage.as_ref(), "h", i).unwrap();
            assert_eq!(bytes, format!("PAGE-{i:02}").into_bytes());
        }
    }
}

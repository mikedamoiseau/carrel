//! Chapter and comic-page reads, behind one interface.
//!
//! # Why this module exists
//!
//! Before it, every adapter re-derived the same sequence for reading a
//! chapter — resolve the path, `match book.format`, pick between the
//! path-based and the cache-based parser entry point, map the error — and the
//! copies drifted. The desktop IPC adapter read through an archive LRU; the
//! LAN web adapter called the path-based functions, which reopen and reparse
//! the file on *every* chapter request. Same operation, two behaviours, and no
//! single place to fix either.
//!
//! Callers hand in the caches rather than letting this module own them, for
//! three reasons that are all load-bearing:
//!
//! 1. Byte budgets are configured once at app startup (`set_max_bytes` in the
//!    desktop shell's `lib.rs`); core must not re-decide them.
//! 2. The desktop `get_unified_cache_stats` / `clear_all_caches` commands
//!    report on and clear these exact `Arc`s through
//!    [`crate::cache::MemoryCacheAdapter`]. Injecting the same handle means
//!    those commands keep working untouched.
//! 3. An adapter with no cache-management surface (the web server) can hold
//!    its own caches with its own capacity without inheriting a registry.
//!
//! # Locking
//!
//! [`crate::epub::CachedEpubArchive`] carries an `unsafe impl Send` whose
//! soundness argument is that every access happens under the `Mutex` guarding
//! its cache — which now lives in [`ArchiveCaches`], shared by every adapter.
//! Take the guard, read, drop it. An async caller must never hold it across an
//! `.await`.
//!
//! # Comic page reads (CBZ/CBR)
//!
//! [`page_image`]/[`page_count`] (M1 follow-on) use a different cache from
//! [`ArchiveCaches`]: [`crate::page_cache`], the on-disk store the desktop
//! reader already primes on book open. It is keyed by `book_hash`, not file
//! path, and holds decoded page *bytes* rather than a parsed archive — a
//! comic archive is a flat list of image entries with no chapter structure
//! to reparse, so there is nothing analogous to [`ArchiveCaches`]'s
//! parsed-archive reuse to offer here. `caches` is therefore not a parameter
//! of either function; the disk-based `pages` [`crate::storage::Storage`] is
//! injected instead, same principle as [`ArchiveCaches`] — core does not
//! construct it or pick its root or its byte budget.
//!
//! A cache hit reads `pages` only — it never touches `file_path`, which is
//! what lets a caller prove the cache is doing its job by deleting the
//! source file between two reads of the same page. A miss primes the
//! *entire* book into the cache (via
//! [`crate::page_cache::ensure_cached`]) before serving the requested page,
//! so every later read — including a different page — is a pure cache read.
//! See [`page_image`]'s doc comment for why the whole-book prime, not the
//! desktop's page-at-a-time `ensure_comic_fast`, is what belongs here. A
//! book with no `book_hash` (not yet hashed) skips the cache entirely and
//! renders straight from the archive on every call — never an error just
//! because there is nothing to key a cache entry on.
//!
//! Two more caller responsibilities, both because [`page_image`] is
//! synchronous and CPU/I/O-bound: run it off the async executor
//! (`tokio::task::spawn_blocking` on the web adapter), and supply an
//! eviction policy via its `on_extracted` callback — core writes to `pages`
//! but never bounds it, matching every other cache in this module.
//!
//! ## Known limitation: cross-surface eviction race with the desktop
//!
//! Desktop `prepare_comic` primes via [`crate::page_cache::ensure_comic_fast`]
//! — it writes a *complete* manifest immediately but extracts only two
//! pages, then fills the rest in the background via
//! `extract_comic_remaining`. [`page_image`]'s `ensure_cached` treats a
//! manifest as complete only when its first *and* last page are both
//! actually on disk. A web request landing in that window sees a
//! technically-partial cache, evicts it, and re-extracts — and if the
//! desktop's own background pass is still running against the prefix it
//! just lost, *it* then finds its manifest gone mid-write and evicts in
//! turn. The two surfaces can trade evictions instead of converging, and a
//! manifest can briefly list pages that no longer exist on disk, which
//! would 500 a read of one of those pages were it not for the archive
//! fallback documented on [`page_image`] — that turns the user-visible
//! symptom into a slower direct decode rather than an error. `EXTRACTION_LOCKS`
//! (below) closes the equivalent race *between concurrent web requests*, but
//! not this one: `prepare_comic` doesn't take that lock, since wiring the
//! desktop path through this module is milestone M3, not this one.
//! Reworking `page_cache`'s manifest protocol itself is also out of scope
//! here. What would actually resolve this is a shared in-flight/extraction
//! protocol both surfaces participate in — the desktop command taking
//! (or being routed through) the same per-`book_hash` lock this module uses,
//! so "someone is already priming this book" is visible process-wide, not
//! just within this module's own callers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::cache::LruCache;
use crate::epub;
use crate::error::{CarrelError, CarrelResult};
use crate::models::BookFormat;
use crate::page_cache;
use crate::storage::Storage;

/// Parsed-archive caches, keyed by resolved file path, owned by the caller.
#[derive(Clone)]
pub struct ArchiveCaches {
    pub epub: Arc<Mutex<LruCache<epub::CachedEpubArchive>>>,
    /// Holds the post-parse MOBI view (HTML parts + image resources), which
    /// can run to hundreds of MB on an illustrated AZW3 — hence the byte-sized
    /// inserts in [`ensure_mobi_cached`] rather than entry counting.
    #[cfg(feature = "mobi")]
    pub mobi: Arc<Mutex<LruCache<crate::mobi::CachedMobiBook>>>,
}

impl ArchiveCaches {
    /// Empty caches holding at most `entries` parsed archives each.
    ///
    /// For callers that own no caches of their own (a standalone server, a
    /// test harness). No byte budget is applied — set one on the field if the
    /// caller cares, the way the desktop shell does for MOBI.
    pub fn with_capacity(entries: usize) -> Self {
        Self {
            epub: Arc::new(Mutex::new(LruCache::new(entries))),
            #[cfg(feature = "mobi")]
            mobi: Arc::new(Mutex::new(LruCache::new(entries))),
        }
    }
}

/// Read one chapter's sanitized HTML.
///
/// Dispatches on `format`, keeps the parsed archive in `caches` so a repeat
/// read of the same book does not reopen the file, and extracts inline images
/// into `images` under `{book_id}/{chapter_index}/…`.
///
/// Errors with [`CarrelError::invalid`] for the image-only formats (PDF, CBZ,
/// CBR), which have no chapters to read.
pub fn chapter_html(
    format: BookFormat,
    file_path: &str,
    chapter_index: usize,
    images: &dyn Storage,
    book_id: &str,
    caches: &ArchiveCaches,
) -> CarrelResult<String> {
    match format {
        BookFormat::Epub => {
            let mut cache = caches.epub.lock()?;
            ensure_epub_cached(&mut cache, file_path)?;
            let cached = cache
                .get_mut(file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open EPUB archive"))?;
            Ok(epub::get_chapter_content_from_cache(
                cached,
                chapter_index,
                images,
                book_id,
            )?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = caches.mobi.lock()?;
            ensure_mobi_cached(&mut cache, file_path)?;
            let cached = cache
                .get(file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            Ok(crate::mobi::get_chapter_content_from_cache(
                cached,
                chapter_index,
                images,
                book_id,
            )?)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "chapter reads are not supported for format {other}"
        ))),
    }
}

/// Ensure `file_path`'s archive is in `cache`, touching it if already present.
///
/// The open error is propagated rather than swallowed. It carries the kind the
/// callers' error mapping depends on — a missing book file must stay a
/// `NotFound` (404 on the web adapter), not collapse into a generic internal
/// failure, which is what discarding it here produced.
fn ensure_epub_cached(
    cache: &mut LruCache<epub::CachedEpubArchive>,
    file_path: &str,
) -> CarrelResult<()> {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return Ok(());
    }
    let archive = epub::CachedEpubArchive::open(file_path)?;
    cache.insert(file_path.to_string(), archive);
    Ok(())
}

/// MOBI counterpart of [`ensure_epub_cached`]. Returns the open error rather
/// than swallowing it, because `cache.get()` only signals presence and a
/// libmobi parse failure is worth surfacing to the caller as itself.
///
/// Inserts via `insert_with_size` so the byte budget the caller configured on
/// the cache actually drives eviction — entry counting alone would let a
/// handful of illustrated books pin multi-GB of RAM.
#[cfg(feature = "mobi")]
fn ensure_mobi_cached(
    cache: &mut LruCache<crate::mobi::CachedMobiBook>,
    file_path: &str,
) -> CarrelResult<()> {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return Ok(());
    }
    let cached = crate::mobi::CachedMobiBook::open(file_path)?;
    let size = cached.byte_size();
    cache.insert_with_size(file_path.to_string(), cached, size);
    Ok(())
}

/// In-flight comic-cache extractions, one lock per `book_hash` (M1 review,
/// finding F3). The web reader's own preloader fires the current page plus
/// both neighbors as soon as a comic is opened, so a cold open routinely
/// issues three concurrent [`page_image`] calls for the same book. Without
/// this, each would independently see a cache miss and extract the whole
/// archive — three full reads of a possibly network-mounted file, three
/// sets of cache writes, three eviction sweeps. [`page_image`] acquires the
/// lock for `book_hash` before extracting and re-checks the cache once it
/// has it: the first caller through does the real work, everyone else
/// blocks on the same `Mutex` and then finds the cache already warm.
///
/// Follows `commands.rs`'s `STAGING_INFLIGHT` pattern (a global registry
/// keyed by hash) rather than inventing a new one, adapted from "skip if
/// already running" — staging is fire-and-forget, nothing needs its result —
/// to "wait for the one in flight", since every caller here needs bytes
/// back. Entries are never removed: each is a single `Arc<Mutex<()>>`
/// (~tens of bytes) keyed by content hash, so the registry's size is bounded
/// by how many distinct comics this process has ever been asked to open,
/// not by request volume — negligible for the personal libraries this app
/// targets, and it sidesteps the classic "remove on last reference" race a
/// cleanup pass would otherwise need to get right.
static EXTRACTION_LOCKS: std::sync::LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The extraction lock for `hash`, creating it if this is the first request
/// ever seen for this book_hash in this process.
fn extraction_lock(hash: &str) -> Arc<Mutex<()>> {
    let mut locks = EXTRACTION_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    locks
        .entry(hash.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Read one comic page's image bytes, downscaled to `target_width` when
/// given and the source is wider (see [`crate::image_util::maybe_resize_to_jpeg`]).
///
/// CBZ/CBR only in this milestone; every other format errors with
/// [`CarrelError::invalid`], mirroring [`chapter_html`]'s rejection of the
/// image-only formats.
///
/// `book_id`/`book_hash` identify the book for [`page_cache`] — `book_hash`
/// is the cache key, `book_id` is carried into the manifest for
/// informational purposes only. See the module docs for the cache-hit /
/// cache-miss / no-hash behaviour.
///
/// This function is synchronous and, on a cache miss, does the CPU/I/O-bound
/// work of extracting a whole comic archive — callers on an async runtime
/// (the web adapter) must run it on a blocking thread pool
/// (`tokio::task::spawn_blocking`), not inline on an async worker.
///
/// # Degrading instead of failing (M1 review, finding F1)
///
/// A cache that has just been primed can still fail to serve the page it
/// was primed for: `page_cache_max_size_mb` can be smaller than this one
/// comic's extracted size, in which case `on_extracted`'s eviction (see
/// below) reclaims the book we *just* wrote before anyone reads it back; or
/// a concurrent extraction elsewhere in the cache can do the same (see the
/// module docs' cross-surface-race note). Either way, a page cache that
/// cannot serve degrades to a direct archive decode — the same fallback the
/// desktop's `get_comic_page_bytes` has always had — rather than an error.
/// Without it, a comic whose extracted size exceeds the configured budget
/// would 404 on *every* request, forever, for a book that rendered fine
/// before this milestone.
///
/// # `on_extracted` (M1 review, findings F1 and F7)
///
/// Fires **at most once**, and only when this call both (a) performed a
/// real extraction via `ensure_cached` — not when `ensure_cached`
/// short-circuited on an already-complete manifest — and (b) then
/// successfully read the requested page back out of the cache it just
/// wrote. Condition (b) is what makes (a) safe to check so cheaply: any
/// `ensure_cached` short-circuit that still can't serve the requested page
/// (most easily an out-of-range page index on an already-cached book) fails
/// the same read the *first* extraction would have failed too, so gating on
/// a successful post-extraction read is equivalent to gating on "a real
/// extraction happened AND it actually produced this page" — without
/// needing `ensure_cached` to separately report which case it hit. This
/// matters beyond correctness: an unconditional fire lets a LAN client (the
/// PIN is optional) trigger a full-cache eviction walk in a tight loop by
/// repeatedly requesting an invalid page index on a warm book, without ever
/// causing a real extraction.
///
/// Never fires on a cache hit (the hot path — the callback is where a
/// caller is expected to run eviction, which walks the whole cache and does
/// not belong there) and never fires when `book_hash` is `None`, since
/// nothing is written to the cache in that case either. Core does not
/// decide the eviction budget or even whether to evict at all — same
/// "caller supplies the policy" principle as [`ArchiveCaches`] and the
/// `pages` [`Storage`] itself; see
/// [`crate::page_cache::get_or_render_pdf_page_with_eviction`]'s `on_batch`
/// for the existing convention this mirrors. A caller that fails inside the
/// callback must swallow that error itself — a failed eviction must not
/// fail the page request that already has its bytes.
#[allow(clippy::too_many_arguments)]
pub fn page_image<F>(
    format: BookFormat,
    file_path: &str,
    page_index: u32,
    target_width: Option<u32>,
    book_id: &str,
    book_hash: Option<&str>,
    pages: &dyn Storage,
    on_extracted: F,
) -> CarrelResult<(Vec<u8>, String)>
where
    F: FnOnce(),
{
    if !matches!(format, BookFormat::Cbz | BookFormat::Cbr) {
        return Err(CarrelError::invalid(format!(
            "page reads are not supported for format {format}"
        )));
    }

    if let Some(hash) = book_hash {
        if let Ok((data, mime)) = page_cache::get_cached_page(pages, hash, page_index) {
            // A hit — refresh recency (M1 review, finding F4). Without this,
            // a book read only through this cache-hit path never touches
            // `last_accessed` (only `ensure_cached`'s own hit path does),
            // so `run_eviction` sees it as the coldest entry in the whole
            // cache — first in line for both its size and 20-book caps —
            // even while it's the one actively being read. Page turns
            // happen at human speed, so a manifest write per hit is cheap
            // enough not to need throttling.
            page_cache::touch_last_accessed(pages, hash);
            return crate::image_util::maybe_resize_to_jpeg(data, mime, target_width);
        }

        // Cache miss: prime the *whole* archive into the cache before
        // serving, via `ensure_cached` rather than the desktop's
        // page-at-a-time `ensure_comic_fast`. `ensure_comic_fast` only
        // treats a manifest as complete when its first AND last page are
        // both on disk; on the desktop it is always followed by a background
        // `extract_comic_remaining` pass that fills every page in shortly
        // after. This route has no equivalent background pass, so calling it
        // per distinct page would find the manifest "partial" every time,
        // evict what a previous request had just cached, and reopen the
        // archive again — the opposite of this milestone's goal. A full
        // extraction is heavier on the very first page of a book, but every
        // page after that (including different ones) is then a pure cache
        // read for as long as the entry survives eviction.
        //
        // The extraction lock (finding F3) is acquired before any of that:
        // block here, not spin or skip, so every concurrent caller for this
        // `book_hash` gets real bytes back rather than a silent miss.
        let lock = extraction_lock(hash);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        // Re-check now that we hold the lock: whoever got here first has,
        // by the time we acquire it, likely already finished priming the
        // cache for us — the common case under real concurrent load, and
        // exactly what turns "three extractions" into "one".
        if let Ok((data, mime)) = page_cache::get_cached_page(pages, hash, page_index) {
            page_cache::touch_last_accessed(pages, hash);
            return crate::image_util::maybe_resize_to_jpeg(data, mime, target_width);
        }

        // `ensure_file_exists` runs first because neither `ensure_cached`
        // nor the archive readers below preserve
        // `std::io::ErrorKind::NotFound` for a missing book file the way
        // `epub::open_validated` does — without this check a missing file
        // surfaces as a generic `InvalidInput`/`Io` error and the web
        // adapter answers 400/500 instead of 404, the exact class of bug
        // `ensure_epub_cached` guards against above.
        ensure_file_exists(file_path)?;
        page_cache::ensure_cached(pages, book_id, hash, file_path, &format)?;

        // Read back before firing `on_extracted` (finding F1): the callback
        // is the caller's eviction hook, and an eviction pass can reclaim
        // the book we just wrote (its own size can exceed the configured
        // budget, or a concurrent extraction's eviction can land in this
        // exact window) before we ever see its bytes. Reading first means
        // *this* request always gets what it just paid to extract,
        // regardless of what eviction does immediately afterward.
        if let Ok((data, mime)) = page_cache::get_cached_page(pages, hash, page_index) {
            on_extracted();
            return crate::image_util::maybe_resize_to_jpeg(data, mime, target_width);
        }

        // The cache still can't serve the page we just extracted — degrade
        // to a direct decode (see this function's doc comment) rather than
        // propagating `get_cached_page`'s error. `on_extracted` does not
        // fire: there is nothing usable in the cache to run eviction around.
        return if format == BookFormat::Cbz {
            crate::cbz::get_page_image_bytes(file_path, page_index, target_width)
        } else {
            crate::cbr::get_page_image_bytes(file_path, page_index, target_width)
        };
    }

    // No hash to key the cache on — render straight from the archive.
    ensure_file_exists(file_path)?;
    if format == BookFormat::Cbz {
        crate::cbz::get_page_image_bytes(file_path, page_index, target_width)
    } else {
        crate::cbr::get_page_image_bytes(file_path, page_index, target_width)
    }
}

/// Page count for CBZ/CBR. Other formats error with [`CarrelError::invalid`],
/// mirroring [`page_image`].
///
/// Consults the page cache first (M1 review, finding F5) — same
/// `book_hash`/`pages` as [`page_image`] — so a book cached by a previous
/// [`page_image`] call still reports its page count without opening the
/// archive at all, including when the source file is no longer reachable.
/// Without this, a fully-cached comic could not even be *opened* once its
/// source went away, despite every one of its pages already being served
/// from the cache — the web reader calls this before the first page
/// request, so a `page_count` that still always opens the archive defeats
/// the point for that first call.
///
/// Trusting a manifest just because it exists is not enough
/// ([`page_cache::complete_manifest`]'s doc comment has the reason: the
/// desktop's own priming path writes a complete-looking manifest well
/// before every page is actually on disk) — [`page_cache::complete_manifest`]
/// is what this calls instead, so a book only reports a cached count when
/// its cache can actually back it up. Falls back to opening the archive
/// when there is no manifest yet, the manifest is incomplete, or there is
/// no `book_hash`; that fallback does need the file and, on an async
/// runtime, a blocking thread pool — same reason as [`page_image`].
pub fn page_count(
    format: BookFormat,
    file_path: &str,
    book_hash: Option<&str>,
    pages: &dyn Storage,
) -> CarrelResult<u32> {
    if !matches!(format, BookFormat::Cbz | BookFormat::Cbr) {
        return Err(CarrelError::invalid(format!(
            "page count is not supported for format {format}"
        )));
    }

    if let Some(hash) = book_hash {
        if let Some(manifest) = page_cache::complete_manifest(pages, hash) {
            if manifest.format == format {
                return Ok(manifest.page_count);
            }
        }
    }

    ensure_file_exists(file_path)?;
    if format == BookFormat::Cbz {
        crate::cbz::get_page_count(file_path)
    } else {
        crate::cbr::get_page_count(file_path)
    }
}

/// A missing book file must be reported as [`CarrelError::NotFound`] so
/// adapters map it to their "file not found" response (404 on the web
/// route). See the comment at its call site in [`page_image`] for why this
/// check exists rather than relying on the archive-open error.
fn ensure_file_exists(file_path: &str) -> CarrelResult<()> {
    if std::path::Path::new(file_path).exists() {
        Ok(())
    } else {
        Err(CarrelError::not_found(format!(
            "Book file not found at '{file_path}'"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caches() -> ArchiveCaches {
        ArchiveCaches {
            epub: Arc::new(Mutex::new(LruCache::new(4))),
            #[cfg(feature = "mobi")]
            mobi: Arc::new(Mutex::new(LruCache::new(4))),
        }
    }

    /// One-chapter EPUB with a relative `<img src>`, so a read exercises the
    /// spine lookup and the inline-image rewrite.
    fn write_epub(dir: &std::path::Path) -> String {
        let path = dir.join("book.epub");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();

        zip.start_file("mimetype", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Reader Test</dc:title>
  </metadata>
  <manifest>
    <item id="ch0" href="ch0.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="img.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch0"/></spine>
</package>"#,
        )
        .unwrap();
        zip.start_file("ch0.xhtml", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<html><body><p>Hello</p><img src="img.png"/></body></html>"#,
        )
        .unwrap();
        zip.start_file("img.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"\x89PNG\r\n\x1a\n").unwrap();
        zip.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    fn images(dir: &std::path::Path) -> crate::storage::LocalStorage {
        crate::storage::LocalStorage::new(dir.join("images")).unwrap()
    }

    /// The point of the module: a second read of the same book is served from
    /// the cached archive, not by reopening the file. Deleting the file between
    /// the two reads is what makes that observable — a reopen would fail, and a
    /// test that merely asserted "both calls returned Ok" would pass even with
    /// the caching removed.
    #[test]
    fn second_chapter_read_does_not_reopen_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_epub(dir.path());
        let store = images(dir.path());
        let caches = caches();

        let first = chapter_html(BookFormat::Epub, &path, 0, &store, "bk", &caches).unwrap();
        assert_eq!(caches.epub.lock().unwrap().len(), 1);

        std::fs::remove_file(&path).unwrap();

        let second = chapter_html(BookFormat::Epub, &path, 0, &store, "bk", &caches).unwrap();
        assert_eq!(second, first);
        assert_eq!(caches.epub.lock().unwrap().len(), 1);
    }

    /// A missing book file must stay a `NotFound`, because that is the kind
    /// the adapters map to 404. Swallowing the open error and reporting a
    /// generic internal failure instead turned that into a 500 and tripped the
    /// web adapter's LAN-hardening test.
    #[test]
    fn a_missing_file_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = images(dir.path());
        let err = chapter_html(
            BookFormat::Epub,
            &dir.path().join("nope.epub").to_string_lossy(),
            0,
            &store,
            "bk",
            &caches(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    #[test]
    fn image_only_formats_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = images(dir.path());
        for format in [BookFormat::Pdf, BookFormat::Cbz, BookFormat::Cbr] {
            let name = format.to_string();
            let err = chapter_html(format, "/nope", 0, &store, "bk", &caches()).unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    // ── Comic page reads (CBZ/CBR) ──────────────────────────────────────

    /// A CBZ with `pages` as sequentially-named entries (`page00.jpg`,
    /// `page01.jpg`, …), so page index order is predictable.
    fn write_cbz(dir: &std::path::Path, pages: &[Vec<u8>]) -> String {
        let path = dir.join("comic.cbz");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (i, data) in pages.iter().enumerate() {
            zip.start_file(format!("page{i:02}.jpg"), opts).unwrap();
            std::io::Write::write_all(&mut zip, data).unwrap();
        }
        zip.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    /// A real, decodable JPEG — `maybe_resize_to_jpeg` passes undecodable
    /// bytes through unchanged, which would make a resize assertion
    /// vacuously true against a fake fixture.
    fn encode_jpeg(w: u32, h: u32) -> Vec<u8> {
        let buf: image::ImageBuffer<image::Rgb<u8>, _> =
            image::ImageBuffer::from_fn(w, h, |x, y| image::Rgb([((x + y) % 256) as u8, 0, 0]));
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        encoder.encode_image(&buf).unwrap();
        out
    }

    fn pages_storage(dir: &std::path::Path) -> crate::storage::LocalStorage {
        crate::storage::LocalStorage::new(dir.join("cache")).unwrap()
    }

    /// M1's acceptance criterion for comics: a second read of the same page
    /// is served from the disk page cache, not by reopening the archive.
    /// Deleting the source file between the two reads is what makes that
    /// observable — a reopen would fail, and a test that only asserted "both
    /// calls returned Ok" would pass even with the caching removed.
    #[test]
    fn second_page_read_does_not_reopen_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(200, 300)]);
        let pages = pages_storage(dir.path());

        let first = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash1"),
            &pages,
            || {},
        )
        .unwrap();
        assert!(page_cache::read_manifest(&pages, "hash1").is_some());

        std::fs::remove_file(&path).unwrap();

        let second = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash1"),
            &pages,
            || {},
        )
        .unwrap();
        assert_eq!(second, first);
    }

    /// `on_extracted` is the caller's hook for eviction (finding 2 in the M1
    /// review): it must fire on the miss that actually primes the cache, and
    /// must NOT fire on a hit — `run_eviction` walks the whole cache, and
    /// that cost has no business on the hot path. A counter pins both halves:
    /// if the callback moved onto the hit path, or stopped firing at all,
    /// the count after two reads of the same page would differ from 1.
    #[test]
    fn on_extracted_fires_once_on_the_priming_miss_and_never_on_a_hit() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(50, 50)]);
        let pages = pages_storage(dir.path());
        let calls = std::cell::Cell::new(0u32);

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-evict"),
            &pages,
            || {
                calls.set(calls.get() + 1);
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1, "must fire on the priming miss");

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-evict"),
            &pages,
            || {
                calls.set(calls.get() + 1);
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1, "must not fire again on a cache hit");
    }

    /// F7 (M1 review): a page index past the end of an already-cached book
    /// must not fire `on_extracted`. It enters the miss branch (the first
    /// `get_cached_page` fails, since the index itself is invalid), but
    /// `ensure_cached` finds the manifest already complete and does no real
    /// extraction. Firing the callback anyway would let a LAN client (the
    /// PIN protecting this route is optional) trigger a full-cache eviction
    /// walk in a tight loop by repeatedly requesting an out-of-range page on
    /// a warm book, without ever causing a real extraction.
    #[test]
    fn out_of_range_page_on_an_already_cached_book_does_not_fire_on_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(50, 50)]); // only page 0 exists
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-oor"),
            &pages,
            || {},
        )
        .unwrap();

        let calls = std::cell::Cell::new(0u32);
        let err = page_image(
            BookFormat::Cbz,
            &path,
            99,
            None,
            "bk",
            Some("hash-oor"),
            &pages,
            || calls.set(calls.get() + 1),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
        assert_eq!(calls.get(), 0, "must not fire when nothing was extracted");
    }

    /// F4 (M1 review): a cache hit must refresh `last_accessed`, or a comic
    /// read only through this cache-hit path looks, to `run_eviction`, like
    /// the coldest entry in the whole cache — first in line for eviction on
    /// the very next page turn, while it's the one actively being read.
    #[test]
    fn a_cache_hit_refreshes_last_accessed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(50, 50)]);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-touch"),
            &pages,
            || {},
        )
        .unwrap();
        let before = page_cache::read_manifest(&pages, "hash-touch")
            .unwrap()
            .last_accessed;

        // A brief real sleep, so two `Utc::now()` calls are guaranteed to
        // differ rather than hoping scheduler jitter alone produces distinct
        // RFC3339 timestamps.
        std::thread::sleep(std::time::Duration::from_millis(5));

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-touch"),
            &pages,
            || {},
        )
        .unwrap();
        let after = page_cache::read_manifest(&pages, "hash-touch")
            .unwrap()
            .last_accessed;

        assert!(
            after > before,
            "a cache hit must move last_accessed forward: before={before} after={after}"
        );
    }

    /// F1 (M1 review): a comic whose extracted size exceeds the caller's
    /// eviction budget must still serve its pages — every time, not just
    /// once — instead of 404ing forever once the cache write this very
    /// request just made gets evicted out from under it. The `on_extracted`
    /// callback runs real eviction with a budget of 0 MB, which guarantees
    /// `run_eviction` reclaims this book (or any book) regardless of size.
    /// Without reading the page back before firing that callback, and
    /// without the archive-decode fallback for when the read still fails,
    /// the very request that just primed the cache would 404.
    #[test]
    fn a_book_larger_than_the_eviction_budget_still_serves_its_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(200, 300), encode_jpeg(200, 300)]);
        let pages = pages_storage(dir.path());
        let evict_to_zero = || {
            let _ = page_cache::run_eviction(&pages, 0);
        };

        let first = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-oversized"),
            &pages,
            evict_to_zero,
        )
        .unwrap();
        assert!(!first.0.is_empty());
        assert!(
            page_cache::read_manifest(&pages, "hash-oversized").is_none(),
            "the 0 MB budget must actually have reclaimed the book, or this test proves nothing"
        );

        // A second read, now against an empty cache, must ALSO succeed
        // rather than surface `get_cached_page`'s `NotFound` — the
        // regression this finding is about.
        let second = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-oversized"),
            &pages,
            evict_to_zero,
        )
        .unwrap();
        assert_eq!(second, first);
    }

    /// F3 (M1 review): the web reader's own preloader fires the current
    /// page plus both neighbors as soon as a comic is opened, so a cold
    /// open routinely issues three concurrent requests for the same book.
    /// Each must not independently extract the whole archive — a `Barrier`
    /// forces every thread to start at the same instant, maximizing the
    /// race window a missing dedup lock would fall into; the `on_extracted`
    /// counter (already proven single-fire-per-real-extraction by the tests
    /// above) pins the outcome — without the lock, at least one of the
    /// losing threads would also observe a cache miss and extract again.
    #[test]
    fn concurrent_cold_reads_of_the_same_book_extract_only_once() {
        let dir = tempfile::tempdir().unwrap();
        // Enough pages/bytes that extraction takes measurable wall-clock
        // time, widening the window concurrent threads can land in.
        let page_data: Vec<Vec<u8>> = (0..40).map(|_| vec![0xCDu8; 40 * 1024]).collect();
        let path = write_cbz(dir.path(), &page_data);
        let storage = pages_storage(dir.path());
        let page_len = page_data.len();
        let extractions = std::sync::atomic::AtomicU32::new(0);
        let thread_count = 4;
        let barrier = std::sync::Barrier::new(thread_count);

        std::thread::scope(|scope| {
            for i in 0..thread_count {
                let path = &path;
                let storage = &storage;
                let extractions = &extractions;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    page_image(
                        BookFormat::Cbz,
                        path,
                        (i % page_len) as u32,
                        None,
                        "bk",
                        Some("hash-concurrent"),
                        storage,
                        || {
                            extractions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        },
                    )
                    .unwrap();
                });
            }
        });

        assert_eq!(
            extractions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only one of the concurrent cold requests should have extracted the archive"
        );
    }

    /// Trap #4 in the M1 brief: verify the resize on a *cache hit*, not just
    /// the direct-archive path. The bytes stored in the page cache are the
    /// original archive bytes, so resize-on-hit is a separate code path from
    /// `cbz::get_page_image_bytes`'s built-in resize on the cold path —
    /// silently dropping it would mean a cached page ignores `target_width`.
    #[test]
    fn cached_page_is_resized_to_target_width() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(800, 1200)]);
        let pages = pages_storage(dir.path());

        // Prime the cache at full size, then remove the source so the
        // assertion below can only be satisfied by the cache-hit branch —
        // if resize-on-hit were dropped, this would either error (no
        // archive to fall back to) or return the un-resized full-width
        // bytes, not a genuine 200px-wide image.
        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash2"),
            &pages,
            || {},
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        let (bytes, mime) = page_image(
            BookFormat::Cbz,
            &path,
            0,
            Some(200),
            "bk",
            Some("hash2"),
            &pages,
            || {},
        )
        .unwrap();
        assert_eq!(mime, "image/jpeg");
        let (w, _h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(w, 200, "cached page must still honor target_width");
    }

    /// A missing book file must stay `NotFound` on the cache-miss path too —
    /// `ensure_cached`'s underlying archive-open error does not preserve
    /// that kind (see `ensure_file_exists`'s doc comment), so without the
    /// explicit check this would surface as a different kind and the web
    /// adapter would answer something other than 404.
    #[test]
    fn a_missing_file_with_hash_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        let missing = dir.path().join("nope.cbz");
        let err = page_image(
            BookFormat::Cbz,
            &missing.to_string_lossy(),
            0,
            None,
            "bk",
            Some("hash3"),
            &pages,
            || {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// Same as above, but for the no-hash direct-render path (CBR, whose
    /// own archive-open error is always `InvalidInput`, never `NotFound`).
    #[test]
    fn a_missing_file_without_hash_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        let missing = dir.path().join("nope.cbr");
        let err = page_image(
            BookFormat::Cbr,
            &missing.to_string_lossy(),
            0,
            None,
            "bk",
            None,
            &pages,
            || {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// A book with no `file_hash` has nothing to key the page cache on — it
    /// must still render (uncached) rather than erroring, and must not write
    /// anything to the cache.
    #[test]
    fn a_book_with_no_hash_renders_uncached() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(100, 100)]);
        let pages = pages_storage(dir.path());

        let (bytes, mime) =
            page_image(BookFormat::Cbz, &path, 0, None, "bk", None, &pages, || {}).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
        assert!(
            pages.list("page-cache/").unwrap().is_empty(),
            "a hash-less book must not write to the page cache"
        );
    }

    #[test]
    fn page_image_rejects_non_comic_formats() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        for format in [BookFormat::Epub, BookFormat::Mobi, BookFormat::Pdf] {
            let name = format.to_string();
            let err = page_image(format, "/nope", 0, None, "bk", None, &pages, || {}).unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    #[test]
    fn page_count_returns_number_of_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(10, 10), encode_jpeg(10, 10)]);
        let pages = pages_storage(dir.path());
        assert_eq!(page_count(BookFormat::Cbz, &path, None, &pages).unwrap(), 2);
    }

    #[test]
    fn page_count_missing_file_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        let missing = dir.path().join("nope.cbr");
        let err =
            page_count(BookFormat::Cbr, &missing.to_string_lossy(), None, &pages).unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    #[test]
    fn page_count_rejects_non_comic_formats() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        for format in [BookFormat::Epub, BookFormat::Mobi, BookFormat::Pdf] {
            let name = format.to_string();
            let err = page_count(format, "/nope", None, &pages).unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    /// F5 (M1 review): a fully cached comic must still report its page
    /// count with the source file gone — `page_count` must consult the
    /// page-cache manifest before ever touching `file_path`. Priming via
    /// `page_image` first (as the web route always does — it calls
    /// `page_count` before the first page request) and then deleting the
    /// source is what makes this observable: a `page_count` that still
    /// opens the archive would fail here, not merely be slower.
    #[test]
    fn page_count_answers_from_the_cache_with_the_source_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(10, 10), encode_jpeg(10, 10)]);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-count"),
            &pages,
            || {},
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        let count = page_count(BookFormat::Cbz, &path, Some("hash-count"), &pages).unwrap();
        assert_eq!(count, 2);
    }

    /// Follow-up to F5: a manifest existing is not proof a comic is fully
    /// cached. `ensure_comic_fast` — the desktop's own priming path — writes
    /// a *complete-looking* manifest immediately but extracts only the
    /// first page, filling the rest in later. `page_count` must not trust
    /// that manifest just because it's there: with the source still
    /// reachable it must fall through to the (accurate) archive read, and
    /// with the source gone it must fail honestly rather than report a
    /// count nothing on disk backs up — a book that presents as readable
    /// and then mostly isn't would be worse than an honest refusal to open.
    #[test]
    fn page_count_does_not_trust_a_partial_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(
            dir.path(),
            &[
                encode_jpeg(10, 10),
                encode_jpeg(10, 10),
                encode_jpeg(10, 10),
            ],
        );
        let pages = pages_storage(dir.path());

        // Mirrors the desktop's own priming: a full 3-page manifest, but
        // only page 0 actually extracted (no priority pages requested).
        page_cache::ensure_comic_fast(&pages, "bk", "hash-partial", &path, &BookFormat::Cbz, &[])
            .unwrap();

        // Source still reachable: the manifest is incomplete (page 2 is
        // missing), so this falls through to the archive, which knows the
        // real count.
        assert_eq!(
            page_count(BookFormat::Cbz, &path, Some("hash-partial"), &pages).unwrap(),
            3
        );

        // Source gone: must fail rather than report the manifest's
        // unverified `page_count` field.
        std::fs::remove_file(&path).unwrap();
        let err = page_count(BookFormat::Cbz, &path, Some("hash-partial"), &pages).unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }
}

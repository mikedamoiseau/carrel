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
//! not this one: `prepare_comic` doesn't take that lock. M3 routed the
//! desktop's page-*read* commands through this module (via
//! [`OnMiss::ReadSource`], which never primes and so never contends for
//! this lock either), but `prepare_comic`/`prepare_pdf` themselves and their
//! background prerender passes stay outside it — reworking `page_cache`'s
//! manifest protocol, or giving the desktop's priming path a stake in
//! `EXTRACTION_LOCKS`, is still out of scope. What would actually resolve
//! this is a shared in-flight/extraction protocol both surfaces participate
//! in — the desktop command taking (or being routed through) the same
//! per-`book_hash` lock this module uses, so "someone is already priming
//! this book" is visible process-wide, not just within this module's own
//! callers.
//!
//! # PDF page reads (M2)
//!
//! [`page_image`]'s PDF arm shares the `pages` [`crate::storage::Storage`]
//! with comics but not their cache shape: PDF rendering is per-page, and
//! [`crate::page_cache::get_or_render_pdf_page_with_eviction`] already
//! implements the disk-first / render-on-miss / lazy-eviction-batch protocol
//! that needs, one page at a time — there is no `ensure_cached` equivalent
//! to prime a whole PDF, and inventing one would repeat the very
//! whole-archive-on-one-request cost the comic arm's design note above
//! warns about, just for a format where it isn't necessary. See
//! [`pdf_page_image`]'s doc comment for the manifest-establishment and
//! private-mode details.

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

/// What a cache miss in [`page_image`] should do about it (M3).
///
/// Both desktop and web read through the same page cache, but only the
/// desktop has a book-open event (`commands::prepare_comic` /
/// `commands::prepare_pdf`, both outside this crate) plus a background
/// prerender pass that primes the cache ahead of the first page request.
/// The web reader has neither — its page read is the only place that can
/// ever prime its cache, so a miss there must do that priming work itself,
/// or a book never gets cached at all. The desktop's page read runs *after*
/// its own open event already had the chance to prime: a miss there is
/// either "the background pass hasn't gotten to this page yet, and will"
/// (in which case blocking this foreground request on a full prime would
/// fight that background pass for the same network-mounted file and CPU —
/// precisely what the background pass exists to avoid) or "nothing is
/// coming for this book" (nothing to wait for either way). So the desktop's
/// miss must read just the page it was asked for and leave priming to the
/// open event and its background pass.
///
/// A future reader will be tempted to collapse these into one path — don't.
/// The asymmetry is not an accident of how the two surfaces happened to be
/// written; it is a direct consequence of only one of them having a
/// book-open event at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnMiss {
    /// A miss primes the cache for the whole book — the entire archive for
    /// a comic, or an established manifest for a PDF — then serves the
    /// requested page from it. What the web adapter passes: it has no other
    /// event that could ever prime the cache.
    Prime,
    /// A miss reads just the requested page from the source and leaves
    /// priming to whoever else owns it. What the desktop passes: its own
    /// book-open command and background prerender pass already own priming,
    /// so the page read must not duplicate — or race — that work.
    ReadSource,
}

/// Read one comic or PDF page's image bytes, downscaled to `target_width`
/// when given and the source is wider (see
/// [`crate::image_util::maybe_resize_to_jpeg`]).
///
/// CBZ/CBR/PDF only; every other format errors with [`CarrelError::invalid`],
/// mirroring [`chapter_html`]'s rejection of the image-only formats. The PDF
/// arm is [`pdf_page_image`] (below), which this delegates to — its own doc
/// comment has the PDF-specific cache shape, the manifest-establishment
/// step, and `is_private`.
///
/// `book_id`/`book_hash` identify the book for [`page_cache`] — `book_hash`
/// is the cache key, `book_id` is carried into the manifest for
/// informational purposes only. See the module docs for the cache-hit /
/// cache-miss / no-hash behaviour.
///
/// This function is synchronous and, on a cache miss, does the CPU/I/O-bound
/// work of extracting a whole comic archive or rendering one PDF page —
/// callers on an async runtime (the web adapter) must run it on a blocking
/// thread pool (`tokio::task::spawn_blocking`), not inline on an async
/// worker.
///
/// # Degrading instead of failing — comics (M1 review, finding F1)
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
/// PDF (M2) needs no fallback for that exact race — a miss's returned bytes
/// come directly from the pdfium render, never from a disk read-back — but
/// it degrades the same way when the cache is unusable at all (a failed
/// manifest write) or when private mode declines to start one. See
/// [`pdf_page_image`]'s doc comment.
///
/// # `on_extracted` (M1 review, findings F1 and F7; widened to `Fn` for M2)
///
/// The bound widened from `FnOnce` to `Fn` in M2 so the PDF arm can forward
/// this straight into [`page_cache::get_or_render_pdf_page_with_eviction`]'s
/// `on_batch`, which requires `Fn`; every pre-existing caller's closure
/// already satisfied it (none consumed anything they captured), so this is
/// not a behavior change for comics.
///
/// For comics, fires **at most once**, and exactly when this call left new
/// bytes in the cache: `ensure_cached` returned `Ok` *and* did real work,
/// rather than short-circuiting on an already-complete manifest.
///
/// Whether the page then read back successfully is deliberately not part of
/// the condition (M3 review round 4). `ensure_cached` has no size cap of its
/// own, so this callback is the only thing bounding what it wrote against
/// the caller's budget, and a request that goes on to fail — an
/// out-of-range index, a failed read-back, a resize error — has still put a
/// whole archive on disk. It fires on those paths too, including ones that
/// return `Err`.
///
/// A *failed* `ensure_cached` does not fire it, because that function
/// cleans up after itself: a failure part-way through removes what it had
/// written, so there is nothing to sweep (M3 review round 7). That cleanup
/// is best effort — if it fails too, for the same full disk that failed the
/// write, manifest-less pages can survive, and nothing else reclaims them
/// (M3 review round 8; see `extract_comic_full`). Firing the hook would not
/// help in that case either: `collect_cached_books` skips hashes with no
/// manifest, so an eviction pass would not count those bytes, let alone
/// free them. Do not replace this with an inference here. Earlier rounds tried to derive "were bytes
/// written?" from "was the manifest complete beforehand?" and got it wrong
/// in both directions — skipping the sweep after a real extraction left the
/// cache over budget, and running it after an archive that never opened
/// handed an unauthenticated LAN client (the PIN is optional) a full-cache
/// eviction walk per request against one broken comic.
///
/// Gating on real work at all — rather than firing whenever this branch is
/// reached — is what closes that same amplification for a warm book: an
/// invalid page index on an already-cached comic writes nothing, so it must
/// sweep nothing.
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
///
/// For PDF, this same parameter is forwarded straight through as that
/// function's own `on_batch`: it fires only once every
/// [`page_cache::LAZY_EVICTION_BATCH`] disk writes cross a multiple of that
/// count, a much sparser cadence than "every priming miss" — see
/// [`pdf_page_image`].
///
/// `pages` is optional (M2 review round 3, finding 3): a caller whose cache
/// directory cannot even be opened passes `None` and gets the same uncached
/// read as a book with no `book_hash` — an unusable cache must cost the
/// cache, not the book. `on_extracted` never fires in that case, there
/// being nothing to evict around.
///
/// `is_private`, when true, keeps a read out of the page cache (M2): for a
/// PDF whose cache entry already exists it forwards to
/// [`page_cache::get_or_render_pdf_page_with_eviction`]'s `suppress_write`,
/// matching `commands::get_pdf_page_bytes`'s desktop behaviour, and for one
/// with no entry yet it declines to create it at all.
/// Comics ignore it: there is no equivalent write-suppression path for
/// CBZ/CBR yet (tracked in
/// `docs/backlog/comic-cache-ignores-private-mode.md`).
///
/// `on_miss` (M3) is what makes this one function usable by both surfaces
/// despite their different book-open behaviour — see [`OnMiss`]'s doc
/// comment for the full argument. It changes nothing about the cache-*hit*
/// path above; it only changes what happens when there is nothing to serve
/// yet.
#[allow(clippy::too_many_arguments)]
pub fn page_image<F>(
    format: BookFormat,
    file_path: &str,
    page_index: u32,
    target_width: Option<u32>,
    book_id: &str,
    book_hash: Option<&str>,
    pages: Option<&dyn Storage>,
    on_extracted: F,
    is_private: bool,
    on_miss: OnMiss,
) -> CarrelResult<(Vec<u8>, String)>
where
    F: Fn(),
{
    if format == BookFormat::Pdf {
        return pdf_page_image(
            file_path,
            page_index,
            target_width,
            book_id,
            book_hash,
            pages,
            on_extracted,
            is_private,
            on_miss,
        );
    }

    if !matches!(format, BookFormat::Cbz | BookFormat::Cbr) {
        return Err(CarrelError::invalid(format!(
            "page reads are not supported for format {format}"
        )));
    }

    // Restores the two `[page-load]` timing lines the desktop's inline
    // implementation carried before M3 folded it in here (`CARREL_DEBUG_PAGES`
    // gates them). They report the *end-to-end* cost of serving one page,
    // resize included, which is the number this repo's page-turn and
    // PDF-render perf epics were measured against — `page_cache`'s own
    // `page_dbg!` lines time its internals, not this. Both surfaces get
    // them now; the web adapter never had them.
    let started = std::time::Instant::now();

    // Both a hash to key on and a usable cache, or there is no cache path
    // to take: a caller whose cache directory cannot even be opened passes
    // `None` and gets the same uncached read as a book with no hash (M2
    // review round 3, finding 3), rather than an error for a page the
    // archive can still serve.
    if let (Some(hash), Some(pages)) = (book_hash, pages) {
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
            let (bytes, out_mime) =
                crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
            page_cache::page_dbg!(
                "bytes cache HIT: page={} size={}KB total={:?}",
                page_index,
                bytes.len() / 1024,
                started.elapsed()
            );
            return Ok((bytes, out_mime));
        }

        if on_miss == OnMiss::ReadSource {
            // Desktop: priming is `prepare_comic`'s and its background
            // `extract_comic_remaining` pass's job, not this call's — see
            // [`OnMiss`]. Read just the requested page straight from the
            // archive and leave the cache exactly as it was: no lock, no
            // `ensure_cached`, no `on_extracted`. This is the same fallback
            // the pre-M3 desktop implementation always used on a miss.
            ensure_file_exists(file_path)?;
            return direct_comic_page(
                format,
                file_path,
                page_index,
                target_width,
                started,
                std::time::Duration::ZERO,
            );
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
            let (bytes, out_mime) =
                crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
            page_cache::page_dbg!(
                "bytes cache HIT: page={} size={}KB total={:?}",
                page_index,
                bytes.len() / 1024,
                started.elapsed()
            );
            return Ok((bytes, out_mime));
        }

        // `ensure_file_exists` runs first because neither `ensure_cached`
        // nor the archive readers below preserve
        // `std::io::ErrorKind::NotFound` for a missing book file the way
        // `epub::open_validated` does — without this check a missing file
        // surfaces as a generic `InvalidInput`/`Io` error and the web
        // adapter answers 400/500 instead of 404, the exact class of bug
        // `ensure_epub_cached` guards against above.
        ensure_file_exists(file_path)?;
        // Whether `ensure_cached` is about to do real work, decided before
        // it runs: it short-circuits on an already-complete manifest, and
        // `complete_manifest` is the very test it uses to decide that (see
        // its doc comment). This is what keeps F7 true — an out-of-range
        // page of an *already cached* book must not fire the eviction hook,
        // because nothing was written — while still letting the degrade path
        // below fire it when an extraction really did happen.
        let extracted = page_cache::complete_manifest(pages, hash).is_none();
        // A failing `ensure_cached` degrades to a direct decode rather than
        // failing the request (M3 review round 6). Its errors are not all
        // about the archive: a full disk, an unwritable `Storage`, a failed
        // manifest write are all about the *cache*, and this module's
        // standing rule is that an unusable cache costs the cache, not the
        // book (M2 review round 3). `ensure_file_exists` has already run, so
        // a genuinely broken archive still errors — from the direct decode
        // below, which fails too.
        //
        // No eviction hook here: `ensure_cached` cleans up its own partial
        // writes, so a failure leaves the cache as it found it and there is
        // nothing to sweep (M3 review round 7). That cleanup is best effort
        // — if it fails for the same reason the write did, manifest-less
        // pages can survive, and nothing else reclaims them (round 8) — but
        // firing the hook would not help: `collect_cached_books` skips
        // hashes with no manifest, so an eviction pass would not count those
        // bytes, let alone free them. Earlier rounds tried to infer all this
        // from whether the manifest had been complete beforehand and got it
        // wrong in both directions — see `extract_comic_full`'s doc.
        //
        // Logged at `warn`, matching the PDF arm's identical degrade
        // (`pdf page cache unavailable…`): a permanently unwritable or full
        // cache otherwise degrades every comic page request forever with
        // nothing in the app log, and the user just sees slow pages.
        if let Err(e) = page_cache::ensure_cached(pages, book_id, hash, file_path, &format) {
            log::warn!("comic page cache unavailable for {hash}, decoding directly: {e}");
            return direct_comic_page(
                format,
                file_path,
                page_index,
                target_width,
                started,
                std::time::Duration::ZERO,
            );
        }

        // Read back before firing `on_extracted` (finding F1): the callback
        // is the caller's eviction hook, and an eviction pass can reclaim
        // the book we just wrote (its own size can exceed the configured
        // budget, or a concurrent extraction's eviction can land in this
        // exact window) before we ever see its bytes. Reading first means
        // *this* request always gets what it just paid to extract,
        // regardless of what eviction does immediately afterward.
        if let Ok((data, mime)) = page_cache::get_cached_page(pages, hash, page_index) {
            // Fire the eviction hook here — after the read-back, so F1's
            // guarantee holds (this request has its bytes in hand before any
            // sweep can reclaim what the extraction just wrote), but before
            // anything that can fail (M3 review round 3). `ensure_cached`
            // has by now written the *whole archive* to disk, and this hook
            // is what bounds that write against `page_cache_max_size_mb`;
            // skipping it on a later error would leave the cache over budget
            // until some unrelated request happened to sweep.
            //
            // Its cost is measured and subtracted rather than simply timed
            // around, because the web adapter runs `run_eviction` inline in
            // this callback — a full walk of the page cache. The desktop
            // `[page-load]` lines this number is modelled on never included a
            // sweep, and this is the path where that comparison matters most
            // (M3 review round 2, finding 3).
            let sweep_started = std::time::Instant::now();
            if extracted {
                on_extracted();
            }
            let sweep_cost = sweep_started.elapsed();

            let (bytes, out_mime) =
                crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
            // The priming miss is the expensive path — a whole-archive
            // extraction — so it is the one an end-to-end number is most
            // wanted for (M3 review, finding 2).
            page_cache::page_dbg!(
                "bytes primed then read: page={} size={}KB total={:?}",
                page_index,
                bytes.len() / 1024,
                started.elapsed().saturating_sub(sweep_cost)
            );
            return Ok((bytes, out_mime));
        }

        // The cache still can't serve the page we just extracted — degrade
        // to a direct decode (see this function's doc comment) rather than
        // propagating `get_cached_page`'s error.
        //
        // The eviction hook still fires when an extraction actually happened
        // (M3 review round 4). The earlier comment here reasoned that there
        // was "nothing usable in the cache to run eviction around", which had
        // it backwards: `ensure_cached` has no size cap of its own, so if it
        // just wrote a whole archive to disk, this callback is the only thing
        // bounding that write against `page_cache_max_size_mb` — and the
        // read-back failing is no reason to leave it unbounded. The reachable
        // case is an out-of-range page of a cold book: extract 800 MB, fail
        // the read-back, degrade, error — with the archive on disk and, until
        // this fix, nothing sweeping it. Firing before the direct decode
        // rather than after, so a decode that also fails still sweeps; the
        // decode reads the archive, not the cache, so eviction cannot take
        // its bytes away.
        // Timed and subtracted for the same reason the success path above
        // does it: the web adapter runs `run_eviction` inline in this
        // callback, and the desktop `[page-load]` lines this number is
        // modelled on never included a sweep (M3 review round 5).
        let sweep_started = std::time::Instant::now();
        if extracted {
            on_extracted();
        }
        let sweep_cost = sweep_started.elapsed();
        return direct_comic_page(
            format,
            file_path,
            page_index,
            target_width,
            started,
            sweep_cost,
        );
    }

    // No hash to key the cache on — render straight from the archive.
    ensure_file_exists(file_path)?;
    direct_comic_page(
        format,
        file_path,
        page_index,
        target_width,
        started,
        std::time::Duration::ZERO,
    )
}

/// Decode one comic page straight from the archive, bypassing the page
/// cache entirely. Shared by every path in [`page_image`]'s comic arm that
/// has nothing (or nothing usable) cached to serve from: no hash, no usable
/// `pages` storage, [`OnMiss::ReadSource`]'s miss, and the degrade-on-failed
/// read-back fallback.
fn direct_comic_page(
    format: BookFormat,
    file_path: &str,
    page_index: u32,
    target_width: Option<u32>,
    started: std::time::Instant,
    // Time already spent inside the caller's eviction hook, subtracted from
    // the reported total so this number stays comparable with the desktop
    // `[page-load]` lines it is modelled on, which never included a sweep.
    // Zero on every path that reaches here without firing the hook.
    sweep_cost: std::time::Duration,
) -> CarrelResult<(Vec<u8>, String)> {
    let (bytes, mime) = direct_comic_decode(format, file_path, page_index, target_width)?;
    page_cache::page_dbg!(
        "bytes archive read: page={} size={}KB total={:?}",
        page_index,
        bytes.len() / 1024,
        started.elapsed().saturating_sub(sweep_cost)
    );
    Ok((bytes, mime))
}

fn direct_comic_decode(
    format: BookFormat,
    file_path: &str,
    page_index: u32,
    target_width: Option<u32>,
) -> CarrelResult<(Vec<u8>, String)> {
    if format == BookFormat::Cbz {
        crate::cbz::get_page_image_bytes(file_path, page_index, target_width)
    } else {
        crate::cbr::get_page_image_bytes(file_path, page_index, target_width)
    }
}

/// PDF arm of [`page_image`] (M2).
///
/// Unlike comics, there is no `ensure_cached`-style whole-book prime to
/// reuse here: [`page_cache::get_or_render_pdf_page_with_eviction`] already
/// implements the disk-first / render-on-miss / lazy-eviction-batch protocol
/// this needs, one page at a time — which is the right granularity for PDF,
/// where a single page render is already comparable in cost to decoding a
/// whole comic archive, so priming every page on the book's first request
/// would repeat the very "block the request on the whole book" problem the
/// comic arm's whole-book prime exists to accept only once.
///
/// That function only *writes* a page once a cache manifest already exists
/// for `book_hash` — on desktop, `commands::prepare_pdf` establishes one on
/// book-open via `page_cache::ensure_pdf_prewarmed(..., 0)` (zero pages
/// rendered up front, just the page count recorded so later lazy writes
/// have a manifest to attach to). The web reader has no equivalent "open"
/// event, so with `on_miss` set to [`OnMiss::Prime`] this lazily runs that
/// exact same zero-prewarm call — not a new whole-book prime, just the page
/// count — the first time it sees a book with no manifest yet. With
/// [`OnMiss::ReadSource`] (desktop) a missing manifest is never established
/// here at all — only `prepare_pdf` does that — and a book with none yet
/// simply renders at the viewport width on every call, exactly as it did
/// before this module existed. Once a manifest exists (created by either
/// surface, or by `prepare_pdf` out-of-band), every later call for that
/// `book_hash` goes straight to `get_or_render_pdf_page_with_eviction`
/// regardless of `on_miss`: a hit reads the disk cache only (never touches
/// `file_path`, which is what lets a caller prove the cache is doing its
/// job by deleting the source file between two reads of the same page), a
/// miss renders one page via pdfium and writes it.
///
/// When `on_miss` is [`OnMiss::Prime`] and `is_private` is true and no
/// manifest exists yet, that establishing step is skipped entirely and the
/// page renders directly: the manifest holds no page content, but it does
/// record `book_id` and a `last_accessed` that later reads keep refreshing,
/// which a private web read left nowhere before this milestone. (Desktop's
/// `prepare_pdf` writes it unconditionally, but it runs on an explicit
/// book-open, not on every page request — and `on_miss` being
/// [`OnMiss::ReadSource`] there makes this specific check moot, since
/// desktop never reaches it.) For a book whose manifest already exists,
/// `is_private` forwards to `get_or_render_pdf_page_with_eviction`'s
/// `suppress_write`, which skips only the page-bytes disk write — this part
/// applies on both surfaces alike.
///
/// # Degrading instead of failing
///
/// A miss's returned bytes always come from the render itself, not a disk
/// read-back, so unlike the comic arm there is no window where an eviction
/// pass reclaiming what this very call just wrote could turn a successful
/// render into an error (see [`page_image`]'s "degrading instead of
/// failing" doc section) — `on_extracted` (playing the role
/// `get_or_render_pdf_page_with_eviction` calls `on_batch`) only ever runs
/// after the bytes to return are already in hand.
///
/// That covers the eviction race but not an unusable cache: a failing
/// `ensure_pdf_prewarmed` (a full disk, a read-only cache dir) would
/// otherwise fail every request for a page pdfium renders fine, and keep
/// failing, since the manifest never gets written. Every such path falls
/// back to the plain render this arm replaced, so cache health can slow
/// this route down but never break it.
#[allow(clippy::too_many_arguments)]
fn pdf_page_image<F>(
    file_path: &str,
    page_index: u32,
    target_width: Option<u32>,
    book_id: &str,
    book_hash: Option<&str>,
    pages: Option<&dyn Storage>,
    on_extracted: F,
    is_private: bool,
    on_miss: OnMiss,
) -> CarrelResult<(Vec<u8>, String)>
where
    F: Fn(),
{
    // The pre-M2 behaviour of this arm, kept as the thing every path that
    // cannot use the cache degrades to — a plain pdfium render, uninvolved
    // with the cache's health. `get_page_image_bytes` applies the same
    // `None` -> `DEFAULT_RENDER_WIDTH` default as the resize below.
    let direct = || -> CarrelResult<(Vec<u8>, String)> {
        let (data, mime) = crate::pdf::get_page_image_bytes(file_path, page_index, target_width)?;
        Ok((data, mime.to_string()))
    };

    let (Some(hash), Some(pages)) = (book_hash, pages) else {
        // No hash to key the cache on, or no usable cache to key it in —
        // render straight from the file. The guard mirrors the comic arm's:
        // neither pdfium nor `ensure_pdf_prewarmed` preserves `NotFound`
        // for a missing book file, so without it the web adapter answers
        // 400 for a file that is simply gone.
        ensure_file_exists(file_path)?;
        return direct();
    };

    let has_pdf_manifest = page_cache::read_manifest(pages, hash)
        .map(|m| m.format == BookFormat::Pdf)
        .unwrap_or(false);
    if !has_pdf_manifest {
        // The comic arm's guard, and it must sit *inside* this branch:
        // once a manifest exists a page can be served with the file gone,
        // which is this milestone's whole point, so a blanket check at the
        // top would refuse exactly the reads the cache makes possible.
        // Here it does two jobs — neither pdfium nor `ensure_pdf_prewarmed`
        // preserves `NotFound`, so without it the web adapter answers 400
        // for a file that is simply gone; and it keeps an unreachable file
        // out of the cache-error swallow below, which would otherwise log
        // it as a cache failure and then re-open it in `direct()` just to
        // produce the real error (M2 review round 2, finding 3).
        ensure_file_exists(file_path)?;

        if on_miss == OnMiss::ReadSource {
            // Desktop: only `commands::prepare_pdf` establishes a manifest
            // (see [`OnMiss`]) — a page read must never create one itself.
            // With none in place yet this renders at the viewport width
            // directly and touches the cache not at all, the same fallback
            // the pre-M3 desktop implementation always used here.
            return direct();
        }

        // Private mode never *creates* a cache entry (M2 review, finding
        // F2). The manifest holds no page content, but it does record
        // `book_id` and a `last_accessed` this call would then keep
        // refreshing — a durable "book X was being read at time T" that a
        // private web read left nowhere before this milestone, visible in
        // the Settings cache-stats panel, and occupying one of
        // `page_cache::MAX_CACHED_BOOKS` LRU slots against books that do
        // hold bytes. A book already cached from a non-private read still
        // serves from that cache; private mode just never starts one.
        if is_private {
            return direct();
        }
        // A failed manifest write must not take the page down with it (M2
        // review, finding F1). `ensure_pdf_prewarmed` returns `Err` when
        // the final `write_manifest` fails — a full disk, a read-only or
        // quota'd cache dir — and propagating that would turn every request
        // for a page pdfium renders fine into a 500, on every retry,
        // forever, for a book that worked before this milestone. Both
        // comparable paths already degrade instead: the comic arm has its
        // explicit fallback, and `commands::get_pdf_page_bytes` falls
        // through to a viewport render when it cannot even open the cache.
        if let Err(e) = page_cache::ensure_pdf_prewarmed(pages, book_id, hash, file_path, 0) {
            log::warn!("pdf page cache unavailable for {hash}, rendering directly: {e}");
            return direct();
        }
    }

    let (data, mime) = match page_cache::get_or_render_pdf_page_with_eviction(
        pages,
        hash,
        file_path,
        page_index,
        on_extracted,
        is_private,
    ) {
        Ok(v) => v,
        // Already `NotFound` — an out-of-range page index. Return it as it
        // is: the check below would replace a precise "page N out of range"
        // with a vaguer "book file not found", and would stat the source
        // for no reason (M2 review round 4, finding 3).
        Err(e) if e.kind() == "NotFound" => return Err(e),
        Err(e) => {
            // A cache *hit* needs no file; a miss renders, and pdfium maps
            // every open failure to `InvalidInput`, which the web adapter
            // answers as 400 "the book file may be corrupt". Since a
            // manifest lets a part-cached book open with its source
            // unreachable, an uncached page of that book is a routine
            // request — and "this page is not here" is `NotFound`, not a
            // corrupt file. Only on the error path, so a hit pays nothing
            // (M2 review round 3, finding 1).
            //
            // This does stat a possibly network-mounted source, but only
            // where the render just tried to open that same source and
            // failed, so the mount's timeout has already been paid once by
            // the time we get here.
            ensure_file_exists(file_path)?;
            return Err(e);
        }
    };
    // Both a hit and a miss yield `pdf::CACHE_CANONICAL_WIDTH` (2400 px)
    // bytes, whereas the direct render this arm replaced turned a `None`
    // width into `pdf::DEFAULT_RENDER_WIDTH` (1200 px) inside
    // `pdf::get_page_image_bytes` itself. The web reader sends no `?width=`
    // on an ordinary page turn, so passing `None` straight through would
    // silently double every page's resolution on the wire — the opposite of
    // what caching this route is for. Default it here the same way, so a
    // widthless request gets the bytes it always got.
    let target_width = target_width.or(Some(crate::pdf::DEFAULT_RENDER_WIDTH));
    crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)
}

/// Probe `pages` for `book_hash`'s copy of `page_index`, downscaled to
/// `target_width` when given — never touching `file_path` or writing
/// anything. `Ok(None)` on a cache miss, so a caller (the desktop reader's
/// background preloader) can serve "not cached yet" without ever falling
/// through to a source read: that is the whole point of a probe like this
/// one, since decoding a comic archive or rendering a PDF page to warm a
/// neighbor page the reader has not turned to yet would contend with the
/// foreground page turn the user actually is waiting on for the same
/// network-mounted file or CPU (see [`OnMiss::ReadSource`]'s doc comment for
/// why the desktop's *foreground* page read already avoids that; this
/// function is the same discipline applied to the *background* one).
///
/// Shares [`page_cache::get_cached_page`] with [`page_image`]'s own
/// cache-hit path, but — unlike that path — does not call
/// [`page_cache::touch_last_accessed`]: this is a speculative probe fired
/// for pages the reader has not necessarily turned to yet, and treating it
/// as a real read would let `run_eviction` protect pages nobody has looked
/// at over ones that were.
pub fn cached_page(
    pages: &dyn Storage,
    book_hash: &str,
    page_index: u32,
    target_width: Option<u32>,
) -> CarrelResult<Option<(Vec<u8>, String)>> {
    let Ok((data, mime)) = page_cache::get_cached_page(pages, book_hash, page_index) else {
        return Ok(None);
    };
    let (bytes, out_mime) = crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
    Ok(Some((bytes, out_mime)))
}

/// Page count for CBZ/CBR/PDF. Other formats error with
/// [`CarrelError::invalid`], mirroring [`page_image`].
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
///
/// PDF (M2 review, finding F3) gets the same treatment for the same
/// reason: once [`page_image`]'s PDF arm serves pages with no source
/// access, a `page_count` that still always opens the file is what stops a
/// cached PDF from opening at all when its source is gone — the exact
/// scenario the caching exists for. Its test is a different one, not the
/// comic test: [`page_cache::pdf_manifest_page_count`] asks only that
/// *some* page be cached, where the comic test wants a manifest's first and
/// last listed pages both present. A PDF manifest makes no claim about what
/// is on disk, so its count is always right — but answering with nothing
/// cached would open a reader in which every page fails. That function's
/// doc comment has the full argument.
///
/// `pages` is optional (M2 review round 2, finding 1) so that a caller
/// whose cache directory cannot even be opened degrades to reading the
/// file instead of failing — the same principle as [`page_image`]'s
/// fallbacks. `None` simply skips the manifest lookup.
pub fn page_count(
    format: BookFormat,
    file_path: &str,
    book_hash: Option<&str>,
    pages: Option<&dyn Storage>,
) -> CarrelResult<u32> {
    if !matches!(format, BookFormat::Cbz | BookFormat::Cbr | BookFormat::Pdf) {
        return Err(CarrelError::invalid(format!(
            "page count is not supported for format {format}"
        )));
    }

    if let (Some(hash), Some(pages)) = (book_hash, pages) {
        // The two formats get different tests on purpose — see
        // `page_cache::pdf_manifest_page_count` for why a PDF manifest
        // needs only one cached page where a comic manifest needs its
        // first and last.
        if format == BookFormat::Pdf {
            if let Some(count) = page_cache::pdf_manifest_page_count(pages, hash) {
                return Ok(count);
            }
        } else if let Some(manifest) = page_cache::complete_manifest(pages, hash) {
            if manifest.format == format {
                return Ok(manifest.page_count);
            }
        }
    }

    ensure_file_exists(file_path)?;
    match format {
        BookFormat::Cbz => crate::cbz::get_page_count(file_path),
        BookFormat::Cbr => crate::cbr::get_page_count(file_path),
        _ => crate::pdf::get_page_count(file_path),
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {
                calls.set(calls.get() + 1);
            },
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {
                calls.set(calls.get() + 1);
            },
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert_eq!(calls.get(), 1, "must not fire again on a cache hit");
    }

    /// M3 review round 6: a comic whose archive will not open must not fire
    /// the eviction hook. `extracted` means "the manifest was not complete",
    /// not "bytes were written", and `extract_comic_full` reads the entry
    /// list before writing anything — so a corrupt archive fails having
    /// written nothing.
    ///
    /// Firing anyway is not a wasted call, it is an abuse vector: the web
    /// route's PIN is optional, the callback runs a full-cache eviction walk
    /// inline, and a client can loop this request on one broken comic. Round
    /// 5 introduced exactly that while trying to sweep partial extractions;
    /// this pins it shut.
    #[test]
    fn a_comic_that_will_not_open_does_not_fire_on_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.cbz");
        std::fs::write(&path, b"not a zip file!!").unwrap();
        let pages = pages_storage(dir.path());
        let calls = std::cell::Cell::new(0);

        for _ in 0..3 {
            page_image(
                BookFormat::Cbz,
                path.to_str().unwrap(),
                0,
                None,
                "bk",
                Some("hash-broken"),
                Some(&pages),
                || calls.set(calls.get() + 1),
                false,
                OnMiss::Prime,
            )
            .unwrap_err();
        }

        assert_eq!(
            calls.get(),
            0,
            "an archive that never opened wrote nothing — firing the hook \
             would let a LAN client loop one broken comic into a full-cache \
             eviction walk per request"
        );
    }

    /// M3 review round 6: a cache that cannot be written must cost the
    /// cache, not the book — the same rule M2 round 3 established for the
    /// PDF arm. A read-only cache directory fails `ensure_cached`; the page
    /// still has to come back, decoded straight from the archive.
    ///
    /// Unix only: Windows ignores the mode bits.
    #[cfg(unix)]
    #[test]
    fn a_comic_page_still_serves_when_the_cache_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(100, 100)]);
        let pages = pages_storage(dir.path());
        let cache_root = dir.path().join("cache");
        let calls = std::cell::Cell::new(0);

        std::fs::set_permissions(&cache_root, std::fs::Permissions::from_mode(0o555)).unwrap();

        let served = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-ro-cache"),
            Some(&pages),
            || calls.set(calls.get() + 1),
            false,
            OnMiss::Prime,
        );

        // Restore before asserting, so a failure does not also break the
        // temp dir's cleanup.
        std::fs::set_permissions(&cache_root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (bytes, mime) = served.expect("an unwritable cache must not fail the page");
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
        assert_eq!(
            calls.get(),
            0,
            "nothing reached the cache, so there is nothing to evict around"
        );
    }

    /// M3 review round 4: the mirror of F7 below. An out-of-range page of a
    /// *cold* book DOES fire the eviction hook, because `ensure_cached` just
    /// wrote the whole archive to disk before the read-back failed, and this
    /// callback is the only thing that bounds that write against
    /// `page_cache_max_size_mb` — `ensure_cached` has no size cap of its own.
    ///
    /// Without it, `GET /books/{id}/pages/999` on a cold 800 MB comic
    /// extracts the archive, errors, and leaves it on disk unswept; repeated
    /// across books that grows the cache past its budget with nothing
    /// reclaiming it. Together with F7 this pins the actual rule: fire iff an
    /// extraction really happened, regardless of whether the read-back after
    /// it succeeded.
    #[test]
    fn out_of_range_page_on_a_cold_book_still_fires_on_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(10, 10), encode_jpeg(10, 10)]);
        let pages = pages_storage(dir.path());
        let calls = std::cell::Cell::new(0);

        // Cold: nothing cached yet, so the miss primes the whole archive and
        // only then discovers the page index is out of range.
        let err = page_image(
            BookFormat::Cbz,
            &path,
            99,
            None,
            "bk",
            Some("hash-cold-oor"),
            Some(&pages),
            || calls.set(calls.get() + 1),
            false,
            OnMiss::Prime,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");

        assert!(
            page_cache::complete_manifest(&pages, "hash-cold-oor").is_some(),
            "the archive really was extracted, or this test proves nothing"
        );
        assert_eq!(
            calls.get(),
            1,
            "a real extraction must fire the eviction hook even though the \
             read-back failed — nothing else bounds what it just wrote"
        );
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || calls.set(calls.get() + 1),
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            evict_to_zero,
            false,
            OnMiss::Prime,
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
            Some(&pages),
            evict_to_zero,
            false,
            OnMiss::Prime,
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
                        Some(storage),
                        || {
                            extractions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        },
                        false,
                        OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
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

        let (bytes, mime) = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            None,
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
        assert!(
            pages.list("page-cache/").unwrap().is_empty(),
            "a hash-less book must not write to the page cache"
        );
    }

    #[test]
    fn page_image_rejects_non_page_formats() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        for format in [BookFormat::Epub, BookFormat::Mobi] {
            let name = format.to_string();
            let err = page_image(
                format,
                "/nope",
                0,
                None,
                "bk",
                None,
                Some(&pages),
                || {},
                false,
                OnMiss::Prime,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    // ── OnMiss::ReadSource — desktop page reads (M3) ────────────────────

    /// M3's acceptance criterion: the desktop's `on_miss` (`ReadSource`)
    /// must NEVER prime the cache on a miss — it may only read the one page
    /// it was asked for. Deleting the source file straight after the read
    /// is what makes "nothing was primed" observable as a *behaviour*
    /// rather than an implementation detail: if this call had primed the
    /// whole archive (the `OnMiss::Prime` behaviour every other comic test
    /// in this module pins), a manifest would exist and a second read of a
    /// *different* page would still succeed with the file gone. Asserting
    /// only "no manifest" would be enough on its own, but this goes one step
    /// further and proves the negative behaviourally too.
    #[test]
    fn read_source_miss_reads_just_the_page_without_priming_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(
            dir.path(),
            &[
                encode_jpeg(50, 50),
                encode_jpeg(50, 50),
                encode_jpeg(50, 50),
            ],
        );
        let pages = pages_storage(dir.path());
        let calls = std::cell::Cell::new(0u32);

        let got = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-readsource"),
            Some(&pages),
            || calls.set(calls.get() + 1),
            false,
            OnMiss::ReadSource,
        )
        .unwrap();

        let direct = crate::cbz::get_page_image_bytes(&path, 0, None).unwrap();
        assert_eq!(got, direct);
        assert_eq!(
            calls.get(),
            0,
            "on_extracted must not fire — nothing was primed"
        );
        assert!(
            page_cache::read_manifest(&pages, "hash-readsource").is_none(),
            "a ReadSource miss must never write a cache manifest — a foreground \
             desktop page turn priming a whole network-mounted archive is the \
             exact regression this milestone exists to prevent"
        );

        // Confirms the negative behaviourally: with nothing primed, a
        // *different* page can no longer be read once the source is gone —
        // if the miss above had gone through `OnMiss::Prime` instead, this
        // would succeed from the cache it would have written.
        std::fs::remove_file(&path).unwrap();
        let err = page_image(
            BookFormat::Cbz,
            &path,
            1,
            None,
            "bk",
            Some("hash-readsource"),
            Some(&pages),
            || {},
            false,
            OnMiss::ReadSource,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// A `ReadSource` miss must not disable the cache entirely — it only
    /// refuses to *prime* it on a miss. A book already fully cached by
    /// someone else (the desktop's own `prepare_comic`, out of scope for
    /// this crate, so simulated here via `ensure_cached` directly) must
    /// still serve every page from that cache, source deleted, exactly like
    /// `OnMiss::Prime`'s cache-hit path — the two variants only disagree
    /// about what to do when there is nothing to serve yet.
    #[test]
    fn read_source_still_serves_a_cache_primed_by_someone_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(50, 50), encode_jpeg(50, 50)]);
        let pages = pages_storage(dir.path());

        // Stands in for the desktop's `prepare_comic` command, which this
        // crate does not own.
        page_cache::ensure_cached(&pages, "bk", "hash-preprimed", &path, &BookFormat::Cbz).unwrap();
        std::fs::remove_file(&path).unwrap();

        let (bytes, mime) = page_image(
            BookFormat::Cbz,
            &path,
            1,
            None,
            "bk",
            Some("hash-preprimed"),
            Some(&pages),
            || {},
            false,
            OnMiss::ReadSource,
        )
        .unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
    }

    // ── PDF page reads (M2) ─────────────────────────────────────────────

    /// Point pdfium at the bundled library and return whether it is usable.
    ///
    /// Mirrors `pdf::tests::pdfium_available`: the binary is downloaded by
    /// `scripts/download-pdfium.sh` (and by CI on Linux/macOS) into
    /// `src-tauri/resources/`, which is gitignored — a fresh clone that
    /// skipped the script, and the Windows CI job (which builds this test
    /// binary but has no library at all), skip PDF tests rather than fail
    /// them. The path is a process-global `OnceLock`, so setting it here is
    /// idempotent and harmless alongside `pdf.rs`'s own copy of this helper.
    fn pdfium_available() -> bool {
        let lib_name = if cfg!(target_os = "windows") {
            "pdfium.dll"
        } else if cfg!(target_os = "macos") {
            "libpdfium.dylib"
        } else {
            "libpdfium.so"
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("src-tauri")
            .join("resources")
            .join(lib_name);
        if !path.exists() {
            return false;
        }
        crate::pdf::set_pdfium_library_path(Some(path));
        true
    }

    /// A minimal one-page, hand-crafted PDF — no PDF-writing crate needed.
    /// Simplified from `pdf::tests::crafted_pdf` (fixed ordinary page size;
    /// nothing here exercises that helper's aspect-ratio clamp).
    fn write_pdf(dir: &std::path::Path) -> String {
        write_pdf_with_pages(dir, 1)
    }

    /// `write_pdf` for more than one page, so a test can distinguish "some
    /// pages cached" from "every page cached" — the two states
    /// [`page_cache::pdf_manifest_page_count`] deliberately treats alike,
    /// and which a one-page fixture cannot tell apart. Every page shares
    /// one content stream; only the page count matters here.
    fn write_pdf_with_pages(dir: &std::path::Path, page_count: usize) -> String {
        assert!(page_count > 0);
        let content = b"0 0 1 rg 0 0 10 10 re f\n";
        // Objects: 1 catalog, 2 page tree, 3..=2+n the pages, then the
        // shared content stream.
        let content_obj = 3 + page_count;
        let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut bodies: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            format!(
                "<</Type/Pages/Kids[{}]/Count {}>>",
                kids.join(" "),
                page_count
            )
            .into_bytes(),
        ];
        bodies.extend((0..page_count).map(|_| {
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Contents {content_obj} 0 R/Resources<<>>>>"
            )
            .into_bytes()
        }));
        bodies.push(
            format!(
                "<</Length {}>>stream\n{}\nendstream",
                content.len(),
                std::str::from_utf8(content).unwrap()
            )
            .into_bytes(),
        );

        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"endobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", bodies.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{}\n%%EOF\n",
                bodies.len() + 1,
                xref
            )
            .as_bytes(),
        );

        let path = dir.join("book.pdf");
        std::fs::write(&path, out).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// M2's acceptance criterion: a second read of the same PDF page is
    /// served from the disk page cache, not by re-rendering with pdfium.
    /// Deleting the source file between the two reads is what makes that
    /// observable — a re-render would fail, and a test that only asserted
    /// "both calls returned Ok" would pass even with the caching removed.
    #[test]
    fn second_pdf_page_read_does_not_reopen_the_file() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        let first = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-hash1"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert_eq!(first.1, "image/jpeg");
        assert!(!first.0.is_empty());
        assert!(page_cache::read_manifest(&pages, "pdf-hash1").is_some());

        std::fs::remove_file(&path).unwrap();

        let second = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-hash1"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert_eq!(second, first);
    }

    /// A widthless request must still get `pdf::DEFAULT_RENDER_WIDTH`
    /// (1200 px), not the cache's `CACHE_CANONICAL_WIDTH` (2400 px).
    ///
    /// Routing this route through the page cache changed what `None` means:
    /// `pdf::get_page_image_bytes` — the direct render this arm replaced —
    /// turns `None` into 1200 itself, while the cache always stores 2400 and
    /// `maybe_resize_to_jpeg(_, _, None)` is a no-op. The web reader's
    /// ordinary page turn sends no `?width=`, so handing the cached bytes
    /// back unresized would have quadrupled every LAN page turn's payload —
    /// a caching change that made page turns *heavier*. Both the miss and
    /// the hit are checked, since only the hit reads from disk.
    #[test]
    fn a_widthless_pdf_page_keeps_the_legacy_render_width() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        let width_of = |bytes: &[u8]| -> u32 {
            image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap()
                .0
        };

        for pass in ["miss", "hit"] {
            let (bytes, _) = page_image(
                BookFormat::Pdf,
                &path,
                0,
                None,
                "bk",
                Some("pdf-width"),
                Some(&pages),
                || {},
                false,
                OnMiss::Prime,
            )
            .unwrap();
            assert_eq!(
                width_of(&bytes),
                crate::pdf::DEFAULT_RENDER_WIDTH,
                "widthless request on the {pass} must keep the legacy render width"
            );
        }
    }

    /// A PDF with no `file_hash` has nothing to key the page cache on — it
    /// must still render (uncached) rather than erroring, and must not write
    /// anything to the cache.
    #[test]
    fn a_pdf_with_no_hash_renders_uncached() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        let (bytes, mime) = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            None,
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
        assert!(
            pages.list("page-cache/").unwrap().is_empty(),
            "a hash-less book must not write to the page cache"
        );
    }

    /// M2's private-mode requirement: the page still renders, but private
    /// mode must suppress the page-content write specifically — a `.jpg`
    /// must never land under the book's cache prefix, even though the
    /// (metadata-only: page count and timestamps, never page bytes)
    /// manifest still does, matching `commands::prepare_pdf`'s own
    /// unconditional manifest write.
    #[test]
    fn pdf_private_mode_renders_but_does_not_write_page_bytes() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        let (bytes, _) = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-private"),
            Some(&pages),
            || {},
            true,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(!bytes.is_empty(), "private mode must still render the page");

        let entries = pages.list("page-cache/pdf-private/").unwrap();
        assert!(
            !entries.iter().any(|e| e.ends_with(".jpg")),
            "private mode must not write page bytes to disk: {entries:?}"
        );
        // M2 review, finding F2: nor the manifest, which carries `book_id`
        // and a `last_accessed` — a durable record of the read surviving a
        // session that asked not to be recorded.
        assert!(
            page_cache::read_manifest(&pages, "pdf-private").is_none(),
            "private mode must not create a cache entry: {entries:?}"
        );
    }

    /// M2 review, finding F1: an unusable page cache must slow this route
    /// down, never break it. `ensure_pdf_prewarmed` returns `Err` when its
    /// manifest write fails (a full disk, a read-only cache dir) — and
    /// since that write is what would have made the next call skip it,
    /// propagating the error would fail every request for a page pdfium
    /// renders fine, forever, for a book that worked before this milestone.
    ///
    /// A read-only cache directory is the reachable version of that. Unix
    /// only: Windows ignores the mode bits, and pdfium is absent there
    /// anyway.
    #[cfg(unix)]
    #[test]
    fn a_pdf_page_still_renders_when_the_cache_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt;

        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());
        let cache_root = dir.path().join("cache");

        // Read+execute but not write: `read_manifest` still works (and
        // finds nothing), every `put` fails.
        std::fs::set_permissions(&cache_root, std::fs::Permissions::from_mode(0o555)).unwrap();

        let rendered = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-ro"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        );

        // Restore before the assert, so a failure does not also leak the
        // temp dir by breaking its cleanup.
        std::fs::set_permissions(&cache_root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (bytes, mime) = rendered.expect("an unwritable cache must not fail the page");
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
    }

    /// M2 trap #4 (inherited from the M1 review's finding F1): a page whose
    /// cache entry gets reclaimed by eviction must never come back as an
    /// error on a later request — it must degrade to a fresh direct render,
    /// the way `commands::get_pdf_page_bytes` always has. Unlike the comic
    /// arm, a single `page_image` call for PDF can't fail this way on its
    /// own (`get_or_render_pdf_page_with_eviction` returns the bytes it just
    /// rendered directly, never reading them back from disk — see
    /// `pdf_page_image`'s doc comment) — the reachable version of this
    /// failure is a *later* call finding the manifest an earlier eviction
    /// pass reclaimed. Eviction is run directly here (budget 0, guaranteed
    /// to reclaim every book) rather than through `on_extracted`, since that
    /// callback only fires once every `page_cache::LAZY_EVICTION_BATCH`
    /// writes cross a multiple of that count — a global counter shared with
    /// every other test in this binary, so forcing it deterministically here
    /// would be flaky by construction.
    #[test]
    fn pdf_page_survives_eviction_between_two_reads() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        let first = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-evict"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(page_cache::read_manifest(&pages, "pdf-evict").is_some());

        page_cache::run_eviction(&pages, 0).unwrap();
        assert!(
            page_cache::read_manifest(&pages, "pdf-evict").is_none(),
            "the 0 MB budget must actually have reclaimed the book, or this test proves nothing"
        );

        // A second read, now against an empty cache, must ALSO succeed —
        // `pdf_page_image` re-establishes the manifest and renders fresh,
        // exactly as it would for a book it had never seen before.
        let second = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-evict"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert_eq!(second, first);
    }

    // ── OnMiss::ReadSource — desktop PDF reads (M3) ─────────────────────

    /// M3's other acceptance criterion: with no manifest yet, a `ReadSource`
    /// miss must render at the viewport width directly and never establish
    /// one — only `commands::prepare_pdf` (out of scope for this crate) may
    /// do that. Deleting the source right after the render, then asking for
    /// the SAME page again, is what makes "no manifest was written" a
    /// behaviour rather than an implementation detail: had a manifest been
    /// established (the `OnMiss::Prime` behaviour every other PDF test in
    /// this module pins), the second read would succeed from the disk cache
    /// with the file gone.
    #[test]
    fn read_source_pdf_miss_renders_without_establishing_a_manifest() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());
        let calls = std::cell::Cell::new(0u32);

        let (bytes, mime) = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-readsource"),
            Some(&pages),
            || calls.set(calls.get() + 1),
            false,
            OnMiss::ReadSource,
        )
        .unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
        assert_eq!(calls.get(), 0, "no cache write happened, nothing to evict");
        assert!(
            page_cache::read_manifest(&pages, "pdf-readsource").is_none(),
            "a ReadSource miss must never establish a PDF manifest — only \
             `prepare_pdf` may do that"
        );

        // Confirms the negative behaviourally: with no manifest, the same
        // page cannot be recovered once the source is gone.
        std::fs::remove_file(&path).unwrap();
        let err = page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-readsource"),
            Some(&pages),
            || {},
            false,
            OnMiss::ReadSource,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// A `ReadSource` miss must not disable the cache entirely: once a
    /// manifest already exists — established by `prepare_pdf`, out of scope
    /// for this crate, so simulated here via `ensure_pdf_prewarmed` directly
    /// — a page read still goes through
    /// `get_or_render_pdf_page_with_eviction` exactly as `OnMiss::Prime`
    /// does, writing the page to disk. This is the desktop's actual steady
    /// state after a book has been opened once: `ReadSource` only refuses to
    /// establish a *new* manifest, it does not stop using one that is
    /// already there.
    #[test]
    fn read_source_writes_a_page_once_a_manifest_already_exists() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        // Stands in for `commands::prepare_pdf`'s zero-prewarm call.
        page_cache::ensure_pdf_prewarmed(&pages, "bk", "pdf-preprimed", &path, 0).unwrap();

        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-preprimed"),
            Some(&pages),
            || {},
            false,
            OnMiss::ReadSource,
        )
        .unwrap();

        assert!(
            pages
                .exists("page-cache/pdf-preprimed/000.jpg")
                .unwrap_or(false),
            "a page read against an already-established manifest must still \
             write through to disk, exactly like OnMiss::Prime"
        );
    }

    #[test]
    fn page_count_returns_number_of_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(10, 10), encode_jpeg(10, 10)]);
        let pages = pages_storage(dir.path());
        assert_eq!(
            page_count(BookFormat::Cbz, &path, None, Some(&pages)).unwrap(),
            2
        );
    }

    #[test]
    fn page_count_missing_file_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        let missing = dir.path().join("nope.cbr");
        let err = page_count(
            BookFormat::Cbr,
            &missing.to_string_lossy(),
            None,
            Some(&pages),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    #[test]
    fn page_count_rejects_non_page_formats() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        for format in [BookFormat::Epub, BookFormat::Mobi] {
            let name = format.to_string();
            let err = page_count(format, "/nope", None, Some(&pages)).unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    /// F3 (M2 review): a fully cached PDF must report its page count with
    /// the source file gone, the way a fully cached comic already does.
    /// The web reader fetches the count before its first page request, so
    /// without this a PDF whose every page is cached still cannot be
    /// opened once its source goes away — the exact case the caching is
    /// for.
    ///
    /// Deleting the file is what makes it a test: asserting the count
    /// alone would pass with the manifest lookup removed.
    #[test]
    fn a_fully_cached_pdf_reports_its_page_count_without_the_source_file() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf(dir.path());
        let pages = pages_storage(dir.path());

        // Reading page 0 puts a page on disk, which is what
        // `page_cache::pdf_manifest_page_count` requires before it will
        // answer from the manifest.
        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-count"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();

        std::fs::remove_file(&path).unwrap();

        let count = page_count(BookFormat::Pdf, &path, Some("pdf-count"), Some(&pages)).unwrap();
        assert_eq!(count, 1);
    }

    /// A part-cached PDF must still report its count with the source gone:
    /// the comic standard (first *and* last page on disk) is the wrong test
    /// here, because PDF pages fill in as they are visited, so a book read
    /// part-way has its early pages and not its last. See
    /// [`page_cache::pdf_manifest_page_count`] for the full argument.
    ///
    /// Three pages, only the first read, then the file deleted — a
    /// first-and-last test would refuse this book, leaving it unopenable
    /// despite having a readable cached page.
    #[test]
    fn a_partly_cached_pdf_still_reports_its_page_count() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf_with_pages(dir.path(), 3);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-partial"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            page_count(BookFormat::Pdf, &path, Some("pdf-partial"), Some(&pages)).unwrap(),
            3,
            "the manifest's count comes from pdfium and is keyed by content \
             hash — one cached page is enough to trust it"
        );
    }

    /// The other side of that line (M2 review round 3, finding 2): a
    /// manifest with *nothing* cached must not be trusted, because it is a
    /// routine state rather than an edge case — `commands::prepare_pdf`
    /// prewarms zero pages, and a private-mode read caches none — so
    /// answering from it would open a reader in which every page then
    /// fails. With the source gone there is nothing to serve and nothing to
    /// count.
    #[test]
    fn a_pdf_manifest_with_nothing_cached_is_not_trusted() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf_with_pages(dir.path(), 3);
        let pages = pages_storage(dir.path());

        // Exactly what `prepare_pdf` leaves behind: the real page count,
        // zero pages rendered.
        page_cache::ensure_pdf_prewarmed(&pages, "bk", "pdf-bare", &path, 0).unwrap();
        assert!(page_cache::read_manifest(&pages, "pdf-bare").is_some());

        // The file is still here, so the fallback answers.
        assert_eq!(
            page_count(BookFormat::Pdf, &path, Some("pdf-bare"), Some(&pages)).unwrap(),
            3
        );

        std::fs::remove_file(&path).unwrap();
        let err = page_count(BookFormat::Pdf, &path, Some("pdf-bare"), Some(&pages)).unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// M2 review round 3, finding 1: with a manifest present a part-cached
    /// book opens with its source gone, so a request for one of its
    /// *uncached* pages is routine — and "that page is not here" is
    /// `NotFound` (404), not pdfium's `InvalidInput` (which the web adapter
    /// answers as 400 "the book file may be corrupt").
    #[test]
    fn an_uncached_page_of_a_cached_pdf_with_no_file_is_not_found() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf_with_pages(dir.path(), 3);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-uncached"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        // Page 0 still serves from the cache.
        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-uncached"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();

        // Page 2 never was cached, and the file is gone.
        let err = page_image(
            BookFormat::Pdf,
            &path,
            2,
            None,
            "bk",
            Some("pdf-uncached"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
        // The message, not just the kind: an out-of-range page is also
        // `NotFound`, so kind alone would keep passing if the remap were
        // widened to swallow every error — and the web adapter would go
        // back to answering 400 for a file that is simply gone.
        assert!(
            err.to_string().contains("Book file not found"),
            "unexpected error: {err}"
        );
    }

    /// The page-0 probe in `page_cache::pdf_manifest_page_count` is a fast
    /// path, not the rule: a book resumed part-way — cached from a later
    /// page, page 0 never visited — must still report its count. Without
    /// the listing fallback this book would not open with its source gone.
    #[test]
    fn a_pdf_cached_from_a_later_page_still_reports_its_page_count() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf_with_pages(dir.path(), 3);
        let pages = pages_storage(dir.path());

        // Page 1 only — the state of a book resumed from the middle.
        page_image(
            BookFormat::Pdf,
            &path,
            1,
            None,
            "bk",
            Some("pdf-resumed"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(
            !pages
                .exists("page-cache/pdf-resumed/000.jpg")
                .unwrap_or(false),
            "page 0 must be absent, or this exercises the fast path instead"
        );

        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            page_count(BookFormat::Pdf, &path, Some("pdf-resumed"), Some(&pages)).unwrap(),
            3
        );
    }

    /// M2 review round 4, finding 3: an out-of-range page is already
    /// `NotFound`, and must keep saying so precisely. The round-3 remap
    /// would otherwise overwrite "page N out of range" with "book file not
    /// found" — the same status, a worse answer — and stat a possibly
    /// dead mount to do it.
    #[test]
    fn an_out_of_range_pdf_page_says_out_of_range_not_missing_file() {
        if !pdfium_available() {
            eprintln!("skipping: no bundled pdfium library (see scripts/download-pdfium.sh)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = write_pdf_with_pages(dir.path(), 3);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Pdf,
            &path,
            0,
            None,
            "bk",
            Some("pdf-oor"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        let err = page_image(
            BookFormat::Pdf,
            &path,
            99,
            None,
            "bk",
            Some("pdf-oor"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {err}"
        );
    }

    /// M2 review round 3, finding 3: a caller whose cache directory cannot
    /// be opened passes `None` and must still get its page, the same way a
    /// book with no hash does. An unusable cache costs the cache, not the
    /// book.
    #[test]
    fn page_image_without_a_cache_reads_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(100, 100)]);

        let (bytes, mime) = page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-nocache"),
            None,
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
    }

    /// A PDF with no manifest and no reachable file has nothing to answer
    /// from, and must say `NotFound` rather than inventing a count.
    #[test]
    fn a_pdf_with_no_manifest_and_no_file_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        let missing = dir.path().join("gone.pdf");

        let err = page_count(
            BookFormat::Pdf,
            &missing.to_string_lossy(),
            Some("pdf-none"),
            Some(&pages),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    /// M2 review round 2, finding 1: a caller whose cache directory cannot
    /// be opened at all passes `None`, and must still get a count from the
    /// file rather than an error. Before this route consulted the cache it
    /// simply read the file, so a broken cache dir must cost the cache, not
    /// the book.
    #[test]
    fn page_count_without_a_cache_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(10, 10), encode_jpeg(10, 10)]);

        assert_eq!(
            page_count(BookFormat::Cbz, &path, Some("hash-nocache"), None).unwrap(),
            2
        );
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
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        let count = page_count(BookFormat::Cbz, &path, Some("hash-count"), Some(&pages)).unwrap();
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
            page_count(BookFormat::Cbz, &path, Some("hash-partial"), Some(&pages)).unwrap(),
            3
        );

        // Source gone: must fail rather than report the manifest's
        // unverified `page_count` field.
        std::fs::remove_file(&path).unwrap();
        let err =
            page_count(BookFormat::Cbz, &path, Some("hash-partial"), Some(&pages)).unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    // ── cached_page (M3) ─────────────────────────────────────────────────

    /// The desktop preloader probe's whole point: a hit reads `pages`
    /// only. Deleting the source file between priming the cache and probing
    /// it is what makes that observable — a probe that fell through to a
    /// source read would fail here, and a test that only asserted "returns
    /// Some" would pass even with that fallback still in place.
    #[test]
    fn cached_page_hit_never_touches_the_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(200, 300)]);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-probe"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();
        std::fs::remove_file(&path).unwrap();

        let (bytes, mime) = cached_page(&pages, "hash-probe", 0, None)
            .unwrap()
            .expect("a page primed before the source was deleted must still be found in the cache");
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/jpeg");
    }

    /// The other half: a page never written to the cache is `Ok(None)`, not
    /// an error and not a fallback render — the preloader's contract is
    /// "tell me if it's already there", never "get it for me".
    #[test]
    fn cached_page_miss_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let pages = pages_storage(dir.path());
        assert_eq!(
            cached_page(&pages, "hash-nothing-cached", 0, None).unwrap(),
            None
        );
    }

    /// The probe must honor `target_width` on a hit, the same way
    /// [`page_image`]'s own cache-hit path does — the desktop preloader
    /// requests neighbor pages at the same viewport width as the page the
    /// reader is actually looking at.
    #[test]
    fn cached_page_probe_is_resized_to_target_width() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cbz(dir.path(), &[encode_jpeg(800, 1200)]);
        let pages = pages_storage(dir.path());

        page_image(
            BookFormat::Cbz,
            &path,
            0,
            None,
            "bk",
            Some("hash-probe-width"),
            Some(&pages),
            || {},
            false,
            OnMiss::Prime,
        )
        .unwrap();

        let (bytes, _) = cached_page(&pages, "hash-probe-width", 0, Some(200))
            .unwrap()
            .unwrap();
        let (w, _h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!(w, 200, "the probe must still honor target_width on a hit");
    }
}

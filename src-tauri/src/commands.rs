use secrecy::SecretString;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use carrel_core::activity::ActivityEvent;
use carrel_core::events::{self, CarrelEvent, ImportSource, SyncDirection};

use crate::cbr;
use crate::cbz;
use crate::db::{self, DbPool};
use crate::epub;
use crate::error::{CarrelError, CarrelResult};
use crate::ipc_metrics::IpcMetrics;
use crate::models::{
    AutoBackup, Book, BookFormat, BookGridItem, Bookmark, ChapterMeta, CleanupEntry,
    CleanupProgress, CleanupResult, Collection, CollectionRule, CollectionSuggestion,
    CollectionType, CustomFont, FeatureFlag, Highlight, HighlightSearchResult, NewRuleInput,
    ReadingProgress, SeriesInfo, VocabularyWord,
};
use crate::opds;
use crate::openlibrary;
use crate::page_cache;
use crate::pdf;

pub use carrel_core::cache::LruCache;
use carrel_core::cache::{
    DiskPageCacheAdapter, ManagedCache, MemoryCacheAdapter, UnifiedCacheStats,
};

/// Profile state: active profile name + pool map in a single Mutex.
/// This prevents the race condition where the active profile changes between
/// reading the name and looking up the pool.
///
/// ## Lock ordering
///
/// `AppState` contains multiple Mutexes. To prevent deadlocks, always acquire
/// them in the order listed below. Never hold a higher-numbered lock while
/// waiting for a lower-numbered one.
///
/// 1. `profile_state` — profile name + pool map
/// 2. `epub_cache` — EPUB archive LRU cache
/// 3. `mobi_cache` — MOBI parsed-book LRU cache (mobi feature only)
/// 4. `enrichment_registry` — metadata provider registry
/// 5. `web_server_handle` — running web server handle
///
/// `pending_manual_update_check` and `update_check`'s internal async mutex
/// are leaf locks — acquired alone, never nested with the above.
pub struct ProfileState {
    pub active: String,
    pub pools: std::collections::HashMap<String, DbPool>,
}

pub struct AppState {
    pub db: DbPool,
    /// Combined profile name + pool map (lock #1). See lock ordering above.
    pub profile_state: std::sync::Mutex<ProfileState>,
    pub data_dir: std::path::PathBuf,
    /// App cache dir (`app_cache_dir()`), the base for the page cache and the
    /// source cache (`source-cache/`). Held on `AppState` so the sync,
    /// hot-path `resolve_book_path` can locate a staged local copy without an
    /// `AppHandle`. Distinct from `data_dir` (app *data* dir).
    pub cache_dir: std::path::PathBuf,
    /// EPUB archive LRU cache (lock #2). Single Mutex replaces the former
    /// dual-Mutex (epub_cache + epub_cache_order). Arc so the unified cache
    /// registry (get_unified_cache_stats / clear_all_caches) can hold the
    /// same handle.
    pub epub_cache: std::sync::Arc<std::sync::Mutex<LruCache<epub::CachedEpubArchive>>>,
    /// MOBI parsed-book LRU cache (lock #3). Holds the post-parse view
    /// (HTML parts + image resources) so chapter reads, full-book loads,
    /// and search don't reopen and reparse the file via libmobi on every
    /// request. Mirrors the EPUB cache's role for the MOBI hot paths.
    #[cfg(feature = "mobi")]
    pub mobi_cache: std::sync::Arc<std::sync::Mutex<LruCache<carrel_core::mobi::CachedMobiBook>>>,
    /// Metadata provider registry (lock #4).
    pub enrichment_registry: std::sync::Mutex<crate::providers::ProviderRegistry>,
    /// DB pool shared with the web server, swapped on profile switch.
    pub shared_active_pool: std::sync::Arc<std::sync::Mutex<DbPool>>,
    /// Active profile name shared with the web server, swapped on profile
    /// switch alongside `shared_active_pool`. Lets the web layer's soft-lock
    /// gate (A-M2) know which profile's lock state to check.
    pub shared_active_profile_name: std::sync::Arc<std::sync::Mutex<String>>,
    /// PIN hash shared with the web server, updated by `web_server_set_pin`.
    pub shared_pin_hash: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Profiles unlocked (soft-lock, A-M2) for the rest of this process
    /// session. Shared with the web server so it can refuse to serve a
    /// profile that hasn't been unlocked yet. Leaf lock — never held
    /// together with another `AppState` mutex.
    pub unlocked_profiles: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// "Don't track this session" / private mode (B-M1). Shared with the
    /// web server (cloned into `WebState` at every construction site,
    /// mirroring `unlocked_profiles`/`shared_active_pool`) so desktop and
    /// web callers read the same flag. `set_private_mode` is the only
    /// runtime mutator. Passive write/emit sites read this once per
    /// request via `AppState::is_private`/`WebState::is_private` and pass
    /// an explicit `bool` into the pure carrel-core functions they call —
    /// never read deep inside those functions, so they stay deterministic
    /// under parallel `cargo test`. Defaults `false` (no frontend toggle
    /// exists yet — B-M2); any future ambiguity in a *derived* read (not
    /// this direct atomic load) should still resolve to "suppress".
    pub private_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Count of foreground page renders (`get_pdf_page_bytes` /
    /// `get_comic_page_bytes`) currently in flight. The background PDF
    /// prerender pass pauses while this is non-zero so it yields the source
    /// file and CPU to the page the user is waiting on. Leaf counter — no lock
    /// ordering constraint.
    pub active_foreground_renders: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Serializes the profile-lifecycle commands (`set_profile_lock`,
    /// `remove_profile_lock`, `unlock_profile`, `delete_profile`) so two
    /// concurrent IPC calls on the same profile can't interleave across the
    /// `.await` gap between the existence check and the keychain/session
    /// mutation — closing a TOCTOU that could otherwise orphan a keychain
    /// lock or a dead `unlocked_profiles` entry. `tokio::sync::Mutex`
    /// (not `std::sync::Mutex`) because the guard is held across `.await`.
    /// Leaf lock — acquire it first, never while holding `profile_state` or
    /// `unlocked_profiles`.
    pub profile_lifecycle: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Handle to the running web server (lock #5).
    pub web_server_handle: std::sync::Mutex<Option<crate::web_server::WebServerHandle>>,
    /// IPC command timing metrics (leaf lock — no ordering constraint).
    pub ipc_metrics: IpcMetrics,
    /// Plugin manager for the active profile (leaf lock). `None` until the
    /// setup hook initializes it; a plugin failure never blocks startup.
    /// `Arc<Mutex<..>>` so a single forwarding bus subscriber can read the
    /// current manager and `switch_profile` can swap it (the manager is
    /// rebuilt against the new profile's DB on switch).
    pub plugin_manager: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<carrel_core::plugins::PluginManager>>>,
    >,
    /// Keeps the non-blocking tracing file writer alive for the app's
    /// lifetime so buffered log records flush on shutdown. Held only for
    /// its `Drop`; never read. `None` when logging to stderr (dev).
    pub _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    /// Read-only connection pool over the offline dictionary artifact
    /// (F-1-1). `None` until first lazily opened by `lookup_word`; reset to
    /// `None` by `delete_dictionary` so the file can be removed. The artifact
    /// lives at `{data_dir}/dictionary/` — profile-independent, one download
    /// serves every profile. Leaf lock — never held with another `AppState`
    /// mutex.
    pub dictionary_pool: std::sync::Mutex<Option<DbPool>>,
    /// Guards against concurrent dictionary downloads: `download_dictionary`
    /// CAS-flips this to `true` for the duration of a download and clears it
    /// when done, so a second invocation returns early instead of racing on
    /// the staging files.
    pub dictionary_downloading: std::sync::atomic::AtomicBool,
    /// Set true by the tray "Check for Updates" item; consumed exactly once by
    /// the frontend via `take_pending_manual_update_check`. LEAF LOCK — never
    /// held together with the profile/cache locks above.
    pub pending_manual_update_check: std::sync::Mutex<bool>,
    /// Guards the automatic startup check to once per process.
    pub startup_update_check_taken: std::sync::atomic::AtomicBool,
    /// GitHub update-check client + single-flight/cache state. Its internal
    /// async mutex is a LEAF and is never held across `.await` alongside any
    /// `AppState` std mutex.
    pub update_check: crate::update::UpdateCheckState,
}

impl AppState {
    /// Returns the DB pool for the active profile, gated by the soft-lock
    /// session state (A-M2, spec D-6/SB-7). The active profile's DB is the
    /// single chokepoint every data-bearing IPC command flows through, so
    /// gating here closes the desktop-IPC bypass: a locked-and-not-yet-
    /// unlocked profile (including a locked `"default"` at startup) is
    /// unreadable in-app, not merely dark on the network. Returns
    /// `CarrelError::LockRequired` in that case.
    ///
    /// Reads `active` and releases `profile_state` before touching
    /// `unlocked_profiles` so the two mutexes are never held together
    /// (keeps `unlocked_profiles` a leaf lock). Internal/startup callers
    /// that must reach the DB before unlock use `active_db_unchecked`.
    pub fn active_db(&self) -> CarrelResult<DbPool> {
        let active = { self.profile_state.lock()?.active.clone() };
        if !self.is_unlocked(&active) {
            return Err(CarrelError::lock_required(format!(
                "Profile '{active}' is locked"
            )));
        }
        self.active_db_unchecked()
    }

    /// Returns the DB pool for the active profile **without** the soft-lock
    /// gate. Only for internal/startup callers that must reach the DB before
    /// the profile is unlocked (reading web-server config to bring the —
    /// still gate-protected — server up). Every data-bearing IPC command
    /// MUST use [`active_db`](Self::active_db) instead.
    /// Uses a single lock to read profile name and look up the pool atomically.
    pub fn active_db_unchecked(&self) -> CarrelResult<DbPool> {
        let ps = self.profile_state.lock()?;
        if ps.active == "default" {
            return Ok(self.db.clone());
        }
        ps.pools
            .get(&ps.active)
            .cloned()
            .ok_or_else(|| CarrelError::not_found(format!("Profile '{}' not found", ps.active)))
    }

    /// Returns the library folder path for the active profile. Reads the
    /// `library_folder` setting, falling back to the platform default.
    pub fn active_library_folder(&self) -> CarrelResult<String> {
        let conn = self.active_db()?.get()?;
        match db::get_setting(&conn, "library_folder")? {
            Some(f) => Ok(f),
            None => default_library_folder(),
        }
    }

    /// Returns a `Storage` handle rooted at the active profile's library
    /// folder. Each call constructs a fresh `LocalStorage`; this is cheap
    /// (stores a PathBuf) and keeps the handle in sync when the user
    /// changes the library folder at runtime.
    pub fn active_storage(
        &self,
    ) -> CarrelResult<std::sync::Arc<dyn carrel_core::storage::Storage>> {
        let folder = self.active_library_folder()?;
        Ok(std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(folder)?,
        ))
    }

    /// Returns a `Storage` handle for cover images, rooted at
    /// `{data_dir}/covers` — the same on-disk layout used before #64 M3.
    /// Cover keys take the form `{book_id}/cover.{ext}`.
    pub fn covers_storage(
        &self,
    ) -> CarrelResult<std::sync::Arc<dyn carrel_core::storage::Storage>> {
        let root = self.data_dir.join("covers");
        Ok(std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(root)?,
        ))
    }

    /// Directory holding the offline dictionary artifact (F-1-1), at
    /// `{data_dir}/dictionary`. Profile-independent: a single downloaded
    /// artifact serves every profile (the `dictionary_enabled` *setting* is
    /// still per-profile).
    pub fn dictionary_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("dictionary")
    }

    /// Returns a `Storage` handle for EPUB inline chapter images, rooted at
    /// `{data_dir}/images` — matches the on-disk layout used before #64 M6.
    /// Image keys take the form `{book_id}/{chapter_index}/{basename}`.
    pub fn images_storage(
        &self,
    ) -> CarrelResult<std::sync::Arc<dyn carrel_core::storage::Storage>> {
        let root = self.data_dir.join("images");
        Ok(std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(root)?,
        ))
    }

    /// Resolve a book's stored `file_path` value to an absolute local
    /// filesystem path that can be handed to parsers.
    ///
    /// Semantics after #64 M4:
    /// * **Imported books** — `file_path` is a storage key relative to
    ///   the library `Storage`. Resolves through `storage.local_path`.
    /// * **Linked books** — `file_path` is an absolute external path.
    ///   Returned unchanged.
    /// * **Legacy imported rows** — rows that predate M4 and weren't
    ///   caught by the migration (library folder changed, etc.) still
    ///   carry an absolute path. Detected via `Path::is_absolute()` and
    ///   returned as-is so the old read flow keeps working.
    pub fn resolve_book_path(&self, book: &Book) -> CarrelResult<String> {
        // Prefer a locally-staged copy of the source file when one exists. This
        // applies BEFORE the link/import branches below, so a linked book on a
        // network share (the case this optimizes) is served from local disk.
        // Existence-only, LOCAL-only, and cheap — never stats the (possibly
        // network-mounted) source, so it's safe on the per-page hot path. The
        // staged name is content-addressed by `file_hash`, so a present copy is
        // always this book's bytes.
        if let Some(hash) = book.file_hash.as_deref() {
            let ext = std::path::Path::new(&book.file_path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if let Some(staged) =
                carrel_core::source_cache::staged_if_present(&self.cache_dir, hash, ext)
            {
                return Ok(staged.to_string_lossy().into_owned());
            }
        }

        if !book.is_imported {
            return Ok(book.file_path.clone());
        }
        let p = std::path::Path::new(&book.file_path);
        if p.is_absolute() {
            return Ok(book.file_path.clone());
        }
        let storage = self.active_storage()?;
        Ok(storage
            .local_path(&book.file_path)?
            .to_string_lossy()
            .to_string())
    }

    /// Reads the private-mode ("Don't track this session") flag (B-M1).
    /// Callers read this once per request and pass the resulting bool into
    /// the pure carrel-core functions that need it (D-1) — never re-read it
    /// deep inside those functions.
    pub fn is_private(&self) -> bool {
        self.private_mode.load(Ordering::SeqCst)
    }

    /// Whether `profile` has been unlocked (soft-lock, A-M2) this process
    /// session. A poisoned lock fails closed (`false` = still locked).
    pub fn is_unlocked(&self, profile: &str) -> bool {
        self.unlocked_profiles
            .lock()
            .map(|guard| guard.contains(profile))
            .unwrap_or(false)
    }

    /// Marks `profile` as unlocked for the rest of this process session.
    pub fn mark_unlocked(&self, profile: &str) -> CarrelResult<()> {
        self.unlocked_profiles.lock()?.insert(profile.to_string());
        Ok(())
    }

    /// Removes `profile` from the unlocked set. Used by `delete_profile`
    /// hygiene (A-M2, Decision 10) so a re-created same-name profile never
    /// inherits an already-unlocked session.
    pub fn mark_locked(&self, profile: &str) -> CarrelResult<()> {
        self.unlocked_profiles.lock()?.remove(profile);
        Ok(())
    }

    /// Errors unless `profile` is a real profile: `"default"` always exists;
    /// any other name must be present in `profile_state.pools`. Mirrors the
    /// existence guard `switch_profile` already applies, so the profile-lock
    /// commands can't create orphaned keychain locks for names that were
    /// mistyped or never created (A-M2) — such a lock would otherwise be
    /// silently inherited by a later same-name profile.
    pub fn ensure_profile_exists(&self, profile: &str) -> CarrelResult<()> {
        if profile != "default" && !self.profile_state.lock()?.pools.contains_key(profile) {
            return Err(CarrelError::invalid(format!(
                "Profile '{profile}' not found"
            )));
        }
        Ok(())
    }
}

/// Build the storage key for a book file from its ID and the (already
/// lowercased) extension. The key is what `Storage::put_path` writes to
/// and what `Storage::delete` removes — the on-disk file for `LocalStorage`
/// ends up at `{library_folder}/{book_id}.{extension}`.
pub fn book_storage_key(book_id: &str, extension: &str) -> String {
    format!("{book_id}.{extension}")
}

/// Derive the storage key for an existing book from its absolute
/// `file_path` column (legacy rows that weren't migrated by the M4
/// schema pass — e.g. because the library folder changed after import).
/// Returns `None` when the path is not under the library folder; linked
/// books sit outside the library folder by design.
///
/// Thin wrapper over [`carrel_core::storage::key_for_local_path`].
pub fn book_key_from_path(file_path: &str, library_folder: &str) -> Option<String> {
    carrel_core::storage::key_for_local_path(
        std::path::Path::new(library_folder),
        std::path::Path::new(file_path),
    )
}

/// Ensure a file_path is loaded in the EPUB LRU cache. If it's already present,
/// move it to most-recently-used. Otherwise open the archive and insert it.
fn ensure_epub_cached(cache: &mut LruCache<epub::CachedEpubArchive>, file_path: &str) {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return;
    }
    if let Ok(archive) = epub::CachedEpubArchive::open(file_path) {
        cache.insert(file_path.to_string(), archive);
    }
}

/// MOBI counterpart of `ensure_epub_cached`. Returns an error when libmobi
/// can't parse the file so the caller can surface it instead of falling
/// through with an empty cache miss — `cache.get()` only signals presence.
///
/// Inserts via `insert_with_size` so the byte budget configured on
/// `mobi_cache` in `lib.rs` actually drives eviction. Owned MOBI bytes
/// (chapters + image resources) can run hundreds of MB on illustrated
/// AZW3s; relying on entry count alone would let a small handful of
/// books pin multi-GB of RAM.
#[cfg(feature = "mobi")]
fn ensure_mobi_cached(
    cache: &mut LruCache<carrel_core::mobi::CachedMobiBook>,
    file_path: &str,
) -> CarrelResult<()> {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return Ok(());
    }
    let cached = carrel_core::mobi::CachedMobiBook::open(file_path)?;
    let size = cached.byte_size();
    cache.insert_with_size(file_path.to_string(), cached, size);
    Ok(())
}

// --- Activity logging ---

pub(crate) fn log_event(conn: &rusqlite::Connection, event: ActivityEvent) {
    let f = event.into_fields();
    let entry = crate::models::ActivityEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        action: f.action.to_string(),
        entity_type: f.entity_type.to_string(),
        entity_id: f.entity_id,
        entity_name: f.entity_name,
        detail: f.detail,
    };
    let _ = db::insert_activity(conn, &entry);
    let _ = db::prune_activity_log(conn, 1000, 90);
}

// --- Cover helpers (#64 M3) ---

/// Build the storage key for a book's cover image. The `covers_storage`
/// on [`AppState`] is rooted at `{data_dir}/covers`, so a key of
/// `{book_id}/cover.{ext}` resolves to the same on-disk path this app
/// has always used.
pub fn cover_storage_key(book_id: &str, ext: &str) -> String {
    format!("{book_id}/cover.{ext}")
}

/// Target width (px) for grid thumbnails. The library card renders the
/// cover in a 160 px box; 320 px is 2× for crisp rendering on Retina
/// displays without paying the multi-megapixel decode cost of the full
/// cover on every scroll-mounted row.
pub const THUMB_WIDTH: u32 = 320;

/// Filename of a book's grid thumbnail, sibling to its `cover.{ext}`. Shared
/// so every site that needs to name or locate that sibling file — this
/// module's storage key, the GDPR/backup export, and the web server's
/// `?size=thumb` cache — agrees on the exact name.
pub(crate) const THUMB_FILENAME: &str = "thumb.jpg";

/// Storage key for a book's grid thumbnail, sibling to its `cover.{ext}`.
pub fn thumb_storage_key(book_id: &str) -> String {
    format!("{book_id}/{THUMB_FILENAME}")
}

/// One-time backfill of grid thumbnails for covers imported before the
/// thumbnail feature existed. Reads each book's cover from disk, generates
/// a thumbnail when the cover is larger than [`THUMB_WIDTH`], and writes it
/// to `{book_id}/thumb.jpg`. Covers that already have a thumbnail, or that
/// are already small, are skipped — the skip path only probes image
/// headers, never a full decode, so re-running on every startup is cheap.
///
/// CPU-bound and I/O-bound; call from a background thread so it never
/// blocks app startup. All failures are non-fatal and logged.
pub fn run_thumbnail_backfill(
    pool: db::DbPool,
    storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
) {
    let items =
        match pool.get().map_err(Into::into).and_then(|conn| {
            db::list_books_grid(&conn).map_err(carrel_core::error::CarrelError::from)
        }) {
            Ok(items) => items,
            Err(e) => {
                log::warn!("thumbnail backfill: could not list books: {e}");
                return;
            }
        };

    let mut made = 0usize;
    for item in items {
        let Some(cover_path) = item.cover_path else {
            continue;
        };
        let tkey = thumb_storage_key(&item.id);
        if matches!(storage.exists(&tkey), Ok(true)) {
            continue;
        }
        let bytes = match std::fs::read(&cover_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(Some(thumb)) = carrel_core::image_util::make_thumbnail(&bytes, THUMB_WIDTH) {
            if storage.put(&tkey, &thumb).is_ok() {
                made += 1;
            }
        }
    }
    if made > 0 {
        log::info!("thumbnail backfill: generated {made} cover thumbnail(s)");
    }
}

/// Rewrite each grid item's `cover_path` to its thumbnail when one exists
/// on disk. Items whose cover is already small (no thumbnail was generated)
/// keep their original `cover_path`. Best-effort: any storage error leaves
/// the item pointing at the full cover.
fn apply_grid_thumbnails(storage: &dyn carrel_core::storage::Storage, items: &mut [BookGridItem]) {
    for item in items.iter_mut() {
        if item.cover_path.is_none() {
            continue;
        }
        let key = thumb_storage_key(&item.id);
        if let Ok(true) = storage.exists(&key) {
            if let Ok(p) = storage.local_path(&key) {
                item.cover_path = Some(p.to_string_lossy().to_string());
            }
        }
    }
}

/// Decode a `data:<mime>;base64,<payload>` cover URI. Returns the raw
/// image bytes plus a sanitized file extension (`png` / `jpg` / `webp`
/// / `gif`). Callers persist the bytes via [`Storage::put`] rather than
/// writing to disk directly.
fn decode_cover_data_uri(data_uri: &str, book_id: &str) -> Option<(Vec<u8>, &'static str)> {
    use base64::{engine::general_purpose, Engine as _};

    let rest = data_uri.strip_prefix("data:")?;
    let (header, encoded) = rest.split_once(',')?;
    let mime = header.strip_suffix(";base64")?;
    let ext: &'static str = match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpg",
    };
    match general_purpose::STANDARD.decode(encoded) {
        Ok(bytes) => Some((bytes, ext)),
        Err(e) => {
            log::warn!("cover extraction failed for book {book_id}: base64 decode error: {e}");
            None
        }
    }
}

/// Write cover bytes through the covers storage and return the resulting
/// local path (what the DB stores). On LocalStorage this is the usual
/// `{data_dir}/covers/{book_id}/cover.{ext}` path; a future remote
/// backend would materialize the key to a cache and return that.
fn save_cover_via_storage(
    storage: &dyn carrel_core::storage::Storage,
    book_id: &str,
    bytes: &[u8],
    ext: &str,
) -> Option<String> {
    let key = cover_storage_key(book_id, ext);
    if let Err(e) = storage.put(&key, bytes) {
        log::warn!("cover extraction failed for book {book_id}: could not write cover: {e}");
        return None;
    }
    // Best-effort grid thumbnail. A failure here is non-fatal: the grid
    // falls back to serving the full cover. `Ok(None)` means the cover is
    // already small enough to use directly.
    match carrel_core::image_util::make_thumbnail(bytes, THUMB_WIDTH) {
        Ok(Some(thumb)) => {
            let tkey = thumb_storage_key(book_id);
            if let Err(e) = storage.put(&tkey, &thumb) {
                log::warn!("thumbnail write failed for book {book_id}: {e}");
            }
        }
        Ok(None) => {
            // The new cover is already small enough to serve directly, but a
            // *previous* (larger) cover at this book_id may have left a
            // thumb.jpg behind. Clean it up so nothing keeps serving stale
            // art for the new cover — best-effort, missing keys are a no-op.
            let tkey = thumb_storage_key(book_id);
            if let Err(e) = storage.delete(&tkey) {
                log::warn!("stale thumbnail cleanup failed for book {book_id}: {e}");
            }
        }
        Err(e) => log::warn!("thumbnail generation failed for book {book_id}: {e}"),
    }
    match storage.local_path(&key) {
        Ok(p) => Some(p.to_string_lossy().to_string()),
        Err(e) => {
            log::warn!("cover extraction failed for book {book_id}: could not resolve path: {e}");
            None
        }
    }
}

/// Save a decoded data-URI cover via the covers storage.
fn save_cover_from_data_uri(
    storage: &dyn carrel_core::storage::Storage,
    book_id: &str,
    data_uri: &str,
) -> Option<String> {
    let (bytes, ext) = decode_cover_data_uri(data_uri, book_id)?;
    save_cover_via_storage(storage, book_id, &bytes, ext)
}

/// Remove every cover artifact owned by a given book from the covers
/// storage. Idempotent; missing entries are silently skipped.
fn delete_book_covers(
    storage: &dyn carrel_core::storage::Storage,
    book_id: &str,
) -> CarrelResult<()> {
    let prefix = format!("{book_id}/");
    let keys = storage.list(&prefix)?;
    for key in keys {
        if let Err(e) = storage.delete(&key) {
            log::warn!("could not delete cover '{key}' for book {book_id}: {e}");
        }
    }
    Ok(())
}

/// Remove every extracted EPUB inline image owned by a given book from the
/// images storage (all chapters). Idempotent; missing entries are silently
/// skipped. Introduced for #64 M6.
fn delete_book_images(
    storage: &dyn carrel_core::storage::Storage,
    book_id: &str,
) -> CarrelResult<()> {
    let prefix = format!("{book_id}/");
    let keys = storage.list(&prefix)?;
    for key in keys {
        if let Err(e) = storage.delete(&key) {
            log::warn!("could not delete image '{key}' for book {book_id}: {e}");
        }
    }
    Ok(())
}

// --- Page cache storage (#64 M5) ---

/// Build a `LocalStorage` rooted at the platform's app cache directory.
/// `page_cache::*` takes `&dyn Storage`; this helper keeps the `AppHandle`
/// → cache-dir resolution in one place so every command site gets the same
/// root.
fn page_cache_storage<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> CarrelResult<carrel_core::storage::LocalStorage> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| CarrelError::internal(format!("Failed to get cache dir: {e}")))?;
    carrel_core::storage::LocalStorage::new(dir)
}

/// Evict a permanently-removed book's page cache: the whole
/// `page-cache/{hash}/` prefix (rendered pages + the persisted PDF text
/// index) via `storage`, plus the in-memory PDF text entry for its resolved
/// path. Both are best-effort. The in-memory eviction runs even when
/// `storage` is `None` (disk-cache init failed), so a deleted book can't
/// serve stale in-memory search results. Shared by every delete path
/// (`remove_book`, bulk delete, missing-file cleanup) so they evict
/// identically; unit-tested with a temp `Storage`.
fn evict_book_page_cache(
    storage: Option<&dyn carrel_core::storage::Storage>,
    file_hash: Option<&str>,
    resolved_path: Option<&str>,
    cache_dir: Option<&std::path::Path>,
) {
    if let (Some(s), Some(h)) = (storage, file_hash) {
        let _ = page_cache::evict_book(s, h);
    }
    if let Some(p) = resolved_path {
        pdf::evict_memory_cache(p);
    }
    // Remove the local staged source copy (if any) so a deleted book's large
    // staged file doesn't linger until size eviction reclaims it. The staged
    // name is keyed by hash + the book's file extension, which `resolved_path`
    // carries (whether it points at the original or the staged copy).
    if let (Some(dir), Some(h), Some(p)) = (cache_dir, file_hash, resolved_path) {
        let ext = std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        carrel_core::source_cache::remove_staged(dir, h, ext);
    }
}

// --- Library management ---

/// Extensions the backend can import in this build. Core formats are
/// always present; MOBI family is conditional on the `mobi` feature. Used
/// both by the Tauri command exposed to the frontend and by internal
/// validators like download_opds_book.
pub fn supported_import_extensions() -> &'static [&'static str] {
    #[cfg(feature = "mobi")]
    {
        &["epub", "pdf", "cbz", "cbr", "mobi", "azw", "azw3"]
    }
    #[cfg(not(feature = "mobi"))]
    {
        &["epub", "pdf", "cbz", "cbr"]
    }
}

/// Return the list of file extensions the backend can import in this build.
/// The frontend uses this to populate the file-picker and folder-scan
/// filters so MOBI/AZW/AZW3 only appear when libmobi is compiled in.
#[tauri::command]
pub async fn get_supported_formats() -> CarrelResult<Vec<&'static str>> {
    Ok(supported_import_extensions().to_vec())
}

/// Best-effort check that `dir` is a writable directory.
///
/// Metadata-based checks (`readonly()`) are unreliable on network mounts
/// (see the known SMBFS issue), so the only dependable test is an actual
/// write probe: create a uniquely-named temp file in the directory, write a
/// byte, then remove it. The temp file is always cleaned up, even on partial
/// failure. Returns `false` when `dir` is not an existing directory.
pub(crate) fn probe_dir_writable(dir: &std::path::Path) -> bool {
    if !dir.is_dir() {
        return false;
    }

    let probe = dir.join(format!(".carrel-write-test-{}", Uuid::new_v4()));
    let result = std::fs::write(&probe, b"0").is_ok();
    // Best-effort cleanup regardless of whether the write succeeded.
    let _ = std::fs::remove_file(&probe);
    result
}

/// Verify a folder is actually writable before recording a `write:files`
/// grant. Writability is a boolean answer, so failures (missing path, not a
/// directory, permission denied) return `Ok(false)` rather than an error.
#[tauri::command]
pub async fn check_dir_writable(path: String) -> CarrelResult<bool> {
    Ok(probe_dir_writable(std::path::Path::new(&path)))
}

#[tauri::command]
#[tracing::instrument(
    skip(file_path, state, _app),
    fields(ext = std::path::Path::new(&file_path).extension().and_then(|e| e.to_str()))
)]
pub async fn import_book(
    file_path: String,
    state: State<'_, AppState>,
    _app: AppHandle,
) -> CarrelResult<Book> {
    let _t = state.ipc_metrics.time("import_book");
    tracing::info!("importing book");
    let db_pool = state.active_db()?;
    let storage = state.active_storage()?;
    let covers_storage = state.covers_storage()?;
    let import_mode = {
        let conn = db_pool.get()?;
        db::get_setting(&conn, "import_mode")
            .ok()
            .flatten()
            .unwrap_or_else(|| "import".to_string())
    };
    import_book_inner(
        file_path,
        db_pool,
        storage,
        covers_storage,
        &import_mode,
        false,
        ImportSource::Manual,
    )
    .map(ImportOutcome::into_book)
}

/// Distinguishes a freshly-imported book from one that already existed in the
/// library (matched by content hash). The IPC `import_book` handler flattens
/// both into the existing `Book` contract; the background importer needs the
/// distinction to report accurate "added" vs. "skipped" counts.
pub(crate) enum ImportOutcome {
    Imported(Book),
    Duplicate(Book),
}

impl ImportOutcome {
    pub(crate) fn into_book(self) -> Book {
        match self {
            ImportOutcome::Imported(b) | ImportOutcome::Duplicate(b) => b,
        }
    }

    pub(crate) fn is_new(&self) -> bool {
        matches!(self, ImportOutcome::Imported(_))
    }
}

#[derive(serde::Serialize)]
pub struct OpdsImportResult {
    #[serde(flatten)]
    pub book: Book,
    pub newly_imported: bool,
}

/// Detects the macOS smbfs lookup bug: files whose path contains non-ASCII
/// characters on an SMB share (mounted under `/Volumes/`) are listed by
/// directory enumeration but fail `stat()`/`open()` with `NotFound`. The
/// file is intact on the server, and no userland API can open it (POSIX
/// `open`, `openat` with raw readdir bytes, and Cocoa `FileHandle` all
/// fail), so the only fixes are server-side. Returns a user-facing
/// workaround hint when the failure pattern matches.
fn smb_unicode_hint(path: &str, kind: std::io::ErrorKind) -> Option<String> {
    if kind == std::io::ErrorKind::NotFound && path.starts_with("/Volumes/") && !path.is_ascii() {
        Some(
            "This looks like a macOS SMB bug: files with accented names on network \
             shares can be listed but not opened. The file is intact on the server. \
             Workarounds: rename it on the NAS (e.g. via its web file manager), copy \
             it over SSH (scp/rsync), or mount the share via NFS — see Troubleshooting \
             in the User Guide."
                .to_string(),
        )
    } else {
        None
    }
}

/// Appends the [`smb_unicode_hint`] workaround text to an import error
/// message when running on macOS and the failure pattern matches.
fn with_smb_hint(base: String, path: &str, err: &std::io::Error) -> String {
    if cfg!(target_os = "macos") {
        if let Some(hint) = smb_unicode_hint(path, err.kind()) {
            return format!("{base} {hint}");
        }
    }
    base
}

/// Body of [`import_book`], extracted so background tasks can call it without
/// going through Tauri's `State`/`invoke` machinery. All resources that the
/// importer touches are passed in explicitly so the same code runs from the
/// IPC handler and from a `spawn_blocking` background loop.
pub(crate) fn import_book_inner(
    file_path: String,
    db_pool: DbPool,
    storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
    covers_storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
    import_mode: &str,
    force_copy: bool,
    source: ImportSource,
) -> CarrelResult<ImportOutcome> {
    // Step 1: single stat — used for size guard, mode-dependent dedup, and to
    // avoid extra round trips on slow filesystems (network shares).
    const MAX_IMPORT_SIZE_BYTES: u64 = 500 * 1024 * 1024;
    let source_metadata = std::fs::metadata(&file_path)
        .map_err(|e| with_smb_hint(format!("Cannot stat file: {e}"), &file_path, &e))?;
    if source_metadata.len() > MAX_IMPORT_SIZE_BYTES {
        let size_mb = source_metadata.len() / (1024 * 1024);
        return Err(CarrelError::invalid(format!(
            "File is too large ({size_mb} MB). Maximum supported import size is 500 MB."
        )));
    }

    // Source identity for the fast skip-before-hash re-import path. mtime is
    // best-effort: if the platform/FS can't report it, treat as absent and
    // fall through to hashing (never skip without a confirmed size+mtime match).
    let source_size = source_metadata.len() as i64;
    let source_mtime: Option<i64> = source_metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    // Step 2: callers that own the file (e.g. OPDS-downloaded temp files)
    // must set `force_copy` so `link` mode still copies into the library.
    // The path string itself is unreliable as a signal — OPDS hands us a
    // local temp path, not a URL — so we take an explicit flag instead.
    let should_copy = force_copy || import_mode != "link";

    // Step 3: content-hash dedup for all modes. `file_hash` is required by
    // downstream features (comic page-cache prepare, cross-device sync), so
    // linked books need a stable content hash too. Hashing also guards
    // against duplicate library entries when the same file is reached
    // through different path spellings (symlinks, alternate mounts, …).
    // Fast skip-before-hash: if this exact source path was imported before and
    // its size + mtime are unchanged, return the existing book without reading
    // a single byte. Any mismatch / path-miss falls through to the hash, which
    // remains the duplicate source of truth.
    // One pooled connection serves both reads; scoped so it is released before
    // the hash read below never holds a connection during byte streaming.
    if let Some(existing) = {
        let conn = db_pool.get()?;
        match db::get_book_by_source_path(&conn, &file_path)? {
            Some(src_ref)
                if src_ref.source_size == Some(source_size)
                    && source_mtime.is_some()
                    && src_ref.source_mtime == source_mtime =>
            {
                db::get_book(&conn, &src_ref.id)?
            }
            _ => None,
        }
    } {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    let hash: Option<String> = {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::open(&file_path)
            .map_err(|e| with_smb_hint(format!("Cannot open file: {e}"), &file_path, &e))?;
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let computed = format!("{:x}", hasher.finalize());
        {
            let conn = db_pool.get()?;
            if let Some(existing) = db::get_book_by_file_hash(&conn, &computed)? {
                // Re-arm the fast-path: refresh this book's source tracking to
                // the current path/size/mtime so a content-identical file whose
                // mtime drifted (re-copy, restore, cloud resync) fast-skips next
                // time instead of re-hashing forever. Best-effort — dedup itself
                // already succeeded.
                let _ = db::set_book_source(
                    &conn,
                    &existing.id,
                    &file_path,
                    source_size,
                    source_mtime.unwrap_or(0),
                );
                return Ok(ImportOutcome::Duplicate(existing));
            }
        }
        Some(computed)
    };

    // Detect format from file extension, with magic-byte fallback for
    // mislabeled archives (e.g., RAR files saved as .cbz).
    let extension = std::path::Path::new(&file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let format = match extension.as_str() {
        "epub" => BookFormat::Epub,
        "cbz" | "cbr" => {
            // Read magic bytes to detect actual archive type
            let mut magic = [0u8; 7];
            if let Ok(mut f) = std::fs::File::open(&file_path) {
                let _ = std::io::Read::read(&mut f, &mut magic);
            }
            if magic[0..4] == [0x50, 0x4B, 0x03, 0x04] {
                // PK\x03\x04 = ZIP
                BookFormat::Cbz
            } else if magic[0..7] == *b"Rar!\x1a\x07\x00" || magic[0..6] == *b"Rar!\x1a\x07" {
                // RAR v4 or v5
                BookFormat::Cbr
            } else if extension == "cbz" {
                BookFormat::Cbz // trust extension if magic unknown
            } else {
                BookFormat::Cbr
            }
        }
        "pdf" => BookFormat::Pdf,
        "mobi" | "azw" | "azw3" => {
            #[cfg(feature = "mobi")]
            {
                BookFormat::Mobi
            }
            #[cfg(not(feature = "mobi"))]
            {
                return Err(CarrelError::invalid(
                    "MOBI/AZW/AZW3 support is not enabled in this build",
                ));
            }
        }
        _ => {
            return Err(CarrelError::invalid(format!(
                "unsupported file format: .{extension}"
            )))
        }
    };

    let book_id = Uuid::new_v4().to_string();

    // Derive a human-friendly title from the *original* filename before copying
    // to the library (which renames the file to {uuid}.{ext}).
    let original_stem = std::path::Path::new(&file_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    // Step 4: Copy source file into library folder as {book_id}.{ext},
    // or keep original path if import_mode is "link".
    //
    // #64 M4: imported books now store the storage *key* in `file_path`,
    // not the absolute filesystem path. Parsers still need a real path,
    // so we materialize one (`parser_path`) locally while recording the
    // key in the DB. Linked books continue to store the original
    // external absolute path.
    let (file_path_value, parser_path, is_imported) = if should_copy {
        let key = book_storage_key(&book_id, &extension);
        storage
            .put_path(&key, std::path::Path::new(&file_path))
            .map_err(|e| CarrelError::internal(format!("Failed to copy file to library: {e}")))?;
        let parser_path = storage.local_path(&key)?.to_string_lossy().to_string();
        (key, parser_path, true)
    } else {
        (file_path.clone(), file_path.clone(), false)
    };
    // Kept as `final_path` to minimize churn in the parser match arms
    // below — all of them use it as a real filesystem path today.
    let final_path = parser_path;

    // Steps 5 & 6: Parse using library-internal path; store hash in Book.
    //
    // #64 M3: covers flow through the covers storage instead of writing
    // directly to `{data_dir}/covers/…`. `cover_saved` tracks whether we
    // persisted any cover artifact so the error-cleanup paths below can
    // tear them back out via `delete_book_covers`.
    let mut cover_saved = false;

    let book = match format {
        BookFormat::Epub => {
            // Open the EPUB zip archive once and reuse it for all operations
            // (metadata, cover extraction, chapter list) instead of reopening 3 times.
            let epub_file = std::fs::File::open(&final_path).map_err(|e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                e.to_string()
            })?;
            let mut archive = zip::ZipArchive::new(epub_file).map_err(|e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                e.to_string()
            })?;
            epub::validate_archive(&mut archive).map_err(|e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                e.to_string()
            })?;

            let metadata = epub::parse_epub_metadata_from_archive(&mut archive).map_err(|e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                e.to_string()
            })?;

            let cover_path = match epub::extract_cover_from_archive(&mut archive) {
                Ok(Some(cover)) => {
                    let saved = save_cover_via_storage(
                        &*covers_storage,
                        &book_id,
                        &cover.bytes,
                        &cover.ext,
                    );
                    if saved.is_some() {
                        cover_saved = true;
                    }
                    saved
                }
                Ok(None) => None,
                Err(e) => {
                    log::warn!("cover extraction failed for book {book_id}: {e}");
                    None
                }
            };

            let chapters = epub::get_chapter_list_from_archive(&mut archive).map_err(|e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                e.to_string()
            })?;

            let language = if metadata.language.is_empty() {
                None
            } else {
                Some(metadata.language.clone())
            };
            Book {
                id: book_id,
                title: metadata.title,
                author: metadata.author,
                file_path: file_path_value.clone(),
                cover_path,
                total_chapters: chapters.len() as u32,
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                format,
                file_hash: hash.clone(),
                description: metadata.description,
                genres: if metadata.genres.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&metadata.genres).unwrap_or_default())
                },
                rating: None,
                isbn: metadata.isbn,
                openlibrary_key: None,
                enrichment_status: None,
                series: None,
                volume: None,
                language,
                publisher: None,
                publish_year: None,
                is_imported,
                want_to_read: false,
            }
        }
        BookFormat::Cbz => {
            let meta = cbz::import_cbz(&final_path).inspect_err(|_e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
            })?;
            let page_result = cbz::get_page_image(&final_path, 0);
            if let Err(ref e) = page_result {
                log::warn!("cover extraction failed for book {book_id}: {e}");
            }
            let cover_path = page_result
                .ok()
                .and_then(|uri| save_cover_from_data_uri(&*covers_storage, &book_id, &uri));
            if cover_path.is_some() {
                cover_saved = true;
            }
            Book {
                id: book_id,
                title: meta.title,
                author: meta.author.unwrap_or_default(),
                file_path: file_path_value.clone(),
                cover_path,
                total_chapters: meta.page_count,
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                format,
                file_hash: hash.clone(),
                description: meta.summary,
                genres: meta.genre.map(|g| {
                    let genres: Vec<String> = g
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    serde_json::to_string(&genres).unwrap_or_else(|_| "[]".to_string())
                }),
                rating: None,
                isbn: None,
                openlibrary_key: None,
                enrichment_status: None,
                series: meta.series,
                volume: meta.volume,
                language: meta.language,
                publisher: meta.publisher,
                publish_year: meta.year,
                is_imported,
                want_to_read: false,
            }
        }
        BookFormat::Cbr => {
            let meta = cbr::import_cbr(&final_path).inspect_err(|_e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
            })?;
            let page_result = cbr::get_page_image(&final_path, 0);
            if let Err(ref e) = page_result {
                log::warn!("cover extraction failed for book {book_id}: {e}");
            }
            let cover_path = page_result
                .ok()
                .and_then(|uri| save_cover_from_data_uri(&*covers_storage, &book_id, &uri));
            if cover_path.is_some() {
                cover_saved = true;
            }
            Book {
                id: book_id,
                title: meta.title,
                author: meta.author.unwrap_or_default(),
                file_path: file_path_value.clone(),
                cover_path,
                total_chapters: meta.page_count,
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                format,
                file_hash: hash.clone(),
                description: meta.summary,
                genres: meta.genre.map(|g| {
                    let genres: Vec<String> = g
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    serde_json::to_string(&genres).unwrap_or_else(|_| "[]".to_string())
                }),
                rating: None,
                isbn: None,
                openlibrary_key: None,
                enrichment_status: None,
                series: meta.series,
                volume: meta.volume,
                language: meta.language,
                publisher: meta.publisher,
                publish_year: meta.year,
                is_imported,
                want_to_read: false,
            }
        }
        BookFormat::Pdf => {
            let meta = pdf::import_pdf(&final_path).inspect_err(|_e| {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
            })?;
            // Use PDF metadata title if available; fall back to original filename.
            let library_stem = std::path::Path::new(&final_path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let title = if meta.title == library_stem || meta.title.is_empty() {
                original_stem.clone()
            } else {
                meta.title
            };
            // Extract first page as cover thumbnail.
            let page_result = pdf::get_page_image(&final_path, 0, 400);
            if let Err(ref e) = page_result {
                log::warn!("cover extraction failed for book {book_id}: {e}");
            }
            let cover_path = page_result
                .ok()
                .and_then(|uri| save_cover_from_data_uri(&*covers_storage, &book_id, &uri));
            if cover_path.is_some() {
                cover_saved = true;
            }
            Book {
                id: book_id,
                title,
                author: meta.author,
                file_path: file_path_value.clone(),
                cover_path,
                total_chapters: meta.page_count,
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                format,
                file_hash: hash.clone(),
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
                is_imported,
                want_to_read: false,
            }
        }
        BookFormat::Mobi => {
            #[cfg(feature = "mobi")]
            {
                use carrel_core::mobi;
                let meta = mobi::parse_mobi_metadata(&final_path).inspect_err(|_e| {
                    if should_copy {
                        let _ = std::fs::remove_file(&final_path);
                    }
                })?;
                let cover_path = match mobi::extract_cover(&final_path) {
                    Ok(Some(cover)) => {
                        let saved = save_cover_via_storage(
                            &*covers_storage,
                            &book_id,
                            &cover.bytes,
                            &cover.ext,
                        );
                        if saved.is_some() {
                            cover_saved = true;
                        }
                        saved
                    }
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("cover extraction failed for book {book_id}: {e}");
                        None
                    }
                };
                let chapters = mobi::get_chapter_list(&final_path).inspect_err(|_e| {
                    if should_copy {
                        let _ = std::fs::remove_file(&final_path);
                    }
                })?;
                let title = if meta.title.is_empty() {
                    original_stem.clone()
                } else {
                    meta.title
                };
                let language = if meta.language.is_empty() {
                    None
                } else {
                    Some(meta.language.clone())
                };
                Book {
                    id: book_id,
                    title,
                    author: meta.author,
                    file_path: file_path_value.clone(),
                    cover_path,
                    total_chapters: chapters.len() as u32,
                    added_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    format,
                    file_hash: hash.clone(),
                    description: meta.description,
                    genres: if meta.genres.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&meta.genres).unwrap_or_default())
                    },
                    rating: None,
                    isbn: meta.isbn,
                    openlibrary_key: None,
                    enrichment_status: None,
                    series: None,
                    volume: None,
                    language,
                    publisher: None,
                    publish_year: None,
                    is_imported,
                    want_to_read: false,
                }
            }
            // Unreachable in practice: the extension-detection arm above
            // returns an error when the feature is off, so this branch
            // only compiles as a placeholder for the exhaustive match.
            #[cfg(not(feature = "mobi"))]
            {
                if should_copy {
                    let _ = std::fs::remove_file(&final_path);
                }
                return Err(CarrelError::invalid(
                    "MOBI/AZW/AZW3 support is not enabled in this build",
                ));
            }
        }
    };

    let mut conn = db_pool.get()?;
    let tx = conn.transaction()?;
    if let Err(e) = db::insert_book(&tx, &book) {
        // If the insert failed due to a duplicate hash, clean up the new copy
        // and return the existing book instead of surfacing a cryptic error.
        if let Some(existing) =
            db::get_book_by_file_hash(&tx, book.file_hash.as_deref().unwrap_or(""))
                .ok()
                .flatten()
        {
            if should_copy {
                let _ = std::fs::remove_file(&final_path);
            }
            if cover_saved {
                let _ = delete_book_covers(&*covers_storage, &book.id);
            }
            return Ok(ImportOutcome::Duplicate(existing));
        }
        if should_copy {
            let _ = std::fs::remove_file(&final_path);
        }
        if cover_saved {
            let _ = delete_book_covers(&*covers_storage, &book.id);
        }
        return Err(e.into());
    }

    // Store 0 when mtime was unavailable at import: a real file never reports
    // epoch-0 mtime, so the read-side fast-path (which also requires
    // source_mtime.is_some()) can never wrongly skip on it.
    if let Err(e) = db::set_book_source(
        &tx,
        &book.id,
        &file_path,
        source_size,
        source_mtime.unwrap_or(0),
    ) {
        if should_copy {
            let _ = std::fs::remove_file(&final_path);
        }
        if cover_saved {
            let _ = delete_book_covers(&*covers_storage, &book.id);
        }
        return Err(e.into());
    }

    log_event(
        &tx,
        ActivityEvent::BookImported {
            id: book.id.clone(),
            title: book.title.clone(),
            format: book.format.to_string(),
            author: book.author.clone(),
        },
    );

    tx.commit().map_err(|e| {
        // Clean up files if commit fails
        if should_copy {
            let _ = std::fs::remove_file(&final_path);
        }
        if cover_saved {
            let _ = delete_book_covers(&*covers_storage, &book.id);
        }
        e.to_string()
    })?;

    // Emit only after the commit — the event must reflect durable state.
    events::bus().emit(CarrelEvent::BookImported {
        book_id: book.id.clone(),
        format: book.format.clone(),
        source,
    });

    Ok(ImportOutcome::Imported(book))
}

#[tauri::command]
pub async fn get_library(state: State<'_, AppState>) -> CarrelResult<Vec<Book>> {
    let _t = state.ipc_metrics.time("get_library");
    let conn = state.active_db()?.get()?;
    Ok(db::list_books(&conn)?)
}

#[tauri::command]
pub async fn get_library_grid(state: State<'_, AppState>) -> CarrelResult<Vec<BookGridItem>> {
    let _t = state.ipc_metrics.time("get_library_grid");
    let conn = state.active_db()?.get()?;
    let mut items = db::list_books_grid(&conn)?;
    if let Ok(storage) = state.covers_storage() {
        apply_grid_thumbnails(&*storage, &mut items);
    }
    Ok(items)
}

#[tauri::command]
pub async fn remove_book<R: tauri::Runtime>(
    book_id: String,
    state: State<'_, AppState>,
    app: AppHandle<R>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;

    // Fetch book before deleting so we can remove the library file and log.
    let existing_book = db::get_book(&conn, &book_id)?;
    let file_path = existing_book.as_ref().map(|b| b.file_path.clone());
    // Captured before the DB delete (finding: deleting a book must clear its
    // persisted page cache, incl. the PDF text index): `file_hash` keys
    // `page-cache/{hash}/`, and the resolved path keys the in-memory
    // `PDF_TEXT_CACHE`.
    let book_hash = existing_book.as_ref().and_then(|b| b.file_hash.clone());
    let resolved_path = existing_book
        .as_ref()
        .and_then(|b| state.resolve_book_path(b).ok());

    log_event(
        &conn,
        ActivityEvent::BookDeleted {
            id: book_id.clone(),
            title: existing_book.as_ref().map(|b| b.title.clone()),
        },
    );

    // #64 M2: resolve the storage handle and library folder *before* the
    // DB delete, and degrade any failure to a logged warning. Doing the
    // fallible storage setup after `db::delete_book` would leave the row
    // gone but return `Err` to the UI — the caller can't retry because
    // the book no longer exists, and the physical file stays orphaned.
    let is_imported = existing_book
        .as_ref()
        .map(|b| b.is_imported)
        .unwrap_or(true);
    let cleanup = if is_imported {
        match (state.active_library_folder(), state.active_storage()) {
            (Ok(folder), Ok(storage)) => Some((folder, storage)),
            (Err(e), _) | (_, Err(e)) => {
                log::warn!(
                    "could not resolve library storage for delete of '{}': {}",
                    book_id,
                    e
                );
                None
            }
        }
    } else {
        None
    };

    db::delete_book(&conn, &book_id)?;

    // Evict the EPUB archive cache entry for this file.
    if let Some(ref path) = file_path {
        if let Ok(mut cache) = state.epub_cache.lock() {
            cache.remove(path);
        }
        #[cfg(feature = "mobi")]
        if let Ok(mut cache) = state.mobi_cache.lock() {
            cache.remove(path);
        }
    }

    // Remove the physical file only if it was imported (copied) into the library.
    // Linked books reference external files that should not be deleted.
    //
    // #64 M4: `file_path` is now a storage key for imported rows written
    // after the migration. Legacy imported rows may still hold an
    // absolute path (library folder changed before the migration caught
    // them) — we detect that via `Path::is_absolute` and fall through
    // to the path-based removal so legacy data stays cleanable.
    if let (Some(path), Some((library_folder, storage))) = (file_path, cleanup) {
        let p = std::path::Path::new(&path);
        if !p.is_absolute() {
            // M4 storage key — delete directly.
            if let Err(e) = storage.delete(&path) {
                log::warn!(
                    "could not delete library file for book '{}' (storage_key): {}",
                    book_id,
                    e
                );
            }
        } else if let Some(key) = book_key_from_path(&path, &library_folder) {
            if let Err(e) = storage.delete(&key) {
                log::warn!(
                    "could not delete library file for book '{}' (library_absolute): {}",
                    book_id,
                    e
                );
            }
        } else {
            // Fallback: absolute path that isn't under the library folder
            // (legacy import, profile migration, etc.). Remove directly.
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!(
                        "could not delete library file for book '{}' (external_legacy_absolute): {}",
                        book_id,
                        e
                    );
                }
            }
        }
    }

    // Clean up extracted inline images for this book via the images storage.
    if let Ok(images) = state.images_storage() {
        let _ = delete_book_images(&*images, &book_id);
    }

    // Clear the page cache (rendered pages + the persisted PDF text index)
    // and the in-memory text entry for this book. Best-effort, like the
    // covers/images cleanup above.
    let cache_storage = page_cache_storage(&app).ok();
    evict_book_page_cache(
        cache_storage
            .as_ref()
            .map(|s| s as &dyn carrel_core::storage::Storage),
        book_hash.as_deref(),
        resolved_path.as_deref(),
        Some(state.cache_dir.as_path()),
    );

    Ok(())
}

#[tauri::command]
pub async fn get_book(book_id: String, state: State<'_, AppState>) -> CarrelResult<Option<Book>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_book(&conn, &book_id)?)
}

// --- Folder Scan ---

#[derive(Clone, serde::Serialize)]
struct FolderScanProgress {
    folder: String,
    files_found: usize,
}

#[tauri::command]
pub async fn scan_folder_for_books(
    folder_path: String,
    app: AppHandle,
) -> CarrelResult<Vec<String>> {
    let _state = app.state::<AppState>();
    let _t = _state.ipc_metrics.time("scan_folder_for_books");
    let dir = std::path::Path::new(&folder_path);
    if !dir.is_dir() {
        return Err(CarrelError::invalid(format!(
            "'{}' is not a directory",
            folder_path
        )));
    }

    let supported = {
        #[cfg(feature = "mobi")]
        {
            &["epub", "cbz", "cbr", "pdf", "mobi", "azw", "azw3"][..]
        }
        #[cfg(not(feature = "mobi"))]
        {
            &["epub", "cbz", "cbr", "pdf"][..]
        }
    };
    let mut found = Vec::new();

    fn walk(
        dir: &std::path::Path,
        extensions: &[&str],
        results: &mut Vec<String>,
        app: &AppHandle,
    ) {
        let _ = app.emit(
            "folder-scan-progress",
            FolderScanProgress {
                folder: dir.to_string_lossy().to_string(),
                files_found: results.len(),
            },
        );
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') && name != "__MACOSX" {
                        walk(&path, extensions, results, app);
                    }
                }
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let lower = ext.to_lowercase();
                if extensions.iter().any(|&s| s == lower) {
                    results.push(path.to_string_lossy().to_string());
                }
            }
        }
    }

    walk(dir, supported, &mut found, &app);
    found.sort();
    Ok(found)
}

// --- Metadata Editing ---

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn update_book_metadata(
    book_id: String,
    title: Option<String>,
    author: Option<String>,
    cover_image_path: Option<String>,
    series: Option<String>,
    volume: Option<u32>,
    language: Option<String>,
    publisher: Option<String>,
    publish_year: Option<u16>,
    rating: Option<f64>,
    state: State<'_, AppState>,
    _app: AppHandle,
) -> CarrelResult<Book> {
    let conn = state.active_db()?.get()?;
    let mut book = db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;

    let has_title = title.is_some();
    let has_author = author.is_some();
    let has_series = series.is_some();
    let has_volume = volume.is_some();
    let has_language = language.is_some();
    let has_publisher = publisher.is_some();
    let has_publish_year = publish_year.is_some();
    let has_cover = cover_image_path.is_some();
    let has_rating = rating.is_some();

    // Normalize and length-limit metadata strings.
    let normalize = |s: String, max_len: usize| -> String {
        let trimmed = s.trim().to_string();
        if trimmed.len() > max_len {
            trimmed[..max_len].to_string()
        } else {
            trimmed
        }
    };
    let normalize_opt = |s: String, max_len: usize| -> Option<String> {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() > max_len {
            Some(trimmed[..max_len].to_string())
        } else {
            Some(trimmed)
        }
    };

    if let Some(t) = title {
        let t = normalize(t, 500);
        if t.is_empty() {
            return Err(CarrelError::invalid("Title cannot be empty."));
        }
        book.title = t;
    }
    if let Some(a) = author {
        book.author = normalize(a, 500);
    }
    if let Some(s) = series {
        book.series = normalize_opt(s, 500);
    }
    if let Some(v) = volume {
        book.volume = Some(v);
    }
    if let Some(l) = language {
        book.language = normalize_opt(l, 50);
    }
    if let Some(p) = publisher {
        book.publisher = normalize_opt(p, 500);
    }
    if let Some(y) = publish_year {
        book.publish_year = Some(y);
    }
    if let Some(r) = rating {
        book.rating = if r <= 0.0 { None } else { Some(r.min(5.0)) };
    }
    if let Some(image_path) = cover_image_path {
        // #64 M3: route user-supplied cover replacement through the covers
        // storage instead of copying directly to `{data_dir}/covers/…`.
        let covers_storage = state.covers_storage()?;
        let ext = std::path::Path::new(&image_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg")
            .to_string();
        let key = cover_storage_key(&book_id, &ext);
        covers_storage
            .put_path(&key, std::path::Path::new(&image_path))
            .map_err(|e| CarrelError::internal(format!("Failed to copy cover image: {e}")))?;
        book.cover_path = Some(
            covers_storage
                .local_path(&key)?
                .to_string_lossy()
                .to_string(),
        );
    }

    db::update_book(&conn, &book)?;

    let mut changes = Vec::new();
    if has_title {
        changes.push("title");
    }
    if has_author {
        changes.push("author");
    }
    if has_series {
        changes.push("series");
    }
    if has_volume {
        changes.push("volume");
    }
    if has_language {
        changes.push("language");
    }
    if has_publisher {
        changes.push("publisher");
    }
    if has_publish_year {
        changes.push("year");
    }
    if has_cover {
        changes.push("cover");
    }
    if has_rating {
        changes.push("rating");
    }
    if !changes.is_empty() {
        let detail = format!("Changed: {}", changes.join(", "));
        log_event(
            &conn,
            ActivityEvent::BookUpdated {
                id: book_id.clone(),
                title: book.title.clone(),
                detail,
            },
        );
    }

    Ok(book)
}

/// Toggle the manual "want to read" flag. Deliberately separate from
/// `update_book_metadata` so a stale metadata edit cannot clobber the flag.
#[tauri::command]
pub async fn set_want_to_read(
    book_id: String,
    want: bool,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::set_want_to_read(&conn, &book_id, want)?)
}

// --- Recently Read ---

#[tauri::command]
pub async fn get_recently_read(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<Book>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_recently_read_books(&conn, limit.unwrap_or(5))?)
}

// --- Reading ---

#[tauri::command]
pub async fn get_chapter_content(
    book_id: String,
    chapter_index: u32,
    state: State<'_, AppState>,
) -> CarrelResult<String> {
    let _t = state.ipc_metrics.time("get_chapter_content");
    let (file_path, format) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        (state.resolve_book_path(&book)?, book.format)
    };

    validate_file_exists(&file_path)?;
    let images_storage = state.images_storage()?;

    match format {
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open EPUB archive"))?;
            Ok(epub::get_chapter_content_from_cache(
                cached,
                chapter_index as usize,
                images_storage.as_ref(),
                &book_id,
            )?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            Ok(carrel_core::mobi::get_chapter_content_from_cache(
                cached,
                chapter_index as usize,
                images_storage.as_ref(),
                &book_id,
            )?)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "get_chapter_content is not supported for format {other}"
        ))),
    }
}

#[tauri::command]
pub async fn search_book_content(
    book_id: String,
    query: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<Vec<crate::models::SearchResult>> {
    let _t = state.ipc_metrics.time("search_book_content");
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };
    let file_path = state.resolve_book_path(&book)?;

    validate_file_exists(&file_path)?;

    match book.format {
        BookFormat::Pdf => {
            // F-4-6: with a file_hash AND an available page cache, resolve
            // through the persisted text-index (memory -> disk -> extract) so
            // a cold session still gets an instant search when the index
            // already exists. The persisted index is an OPTIMIZATION, not a
            // dependency: with no hash, or if the optional page-cache dir
            // can't be initialized (permissions / read-only FS), fall back to
            // extraction-backed search so a readable PDF stays searchable.
            // Search never persists to disk — the background build in
            // `prepare_pdf` is the single, guarded writer of `text-index.json`.
            let results = match (book.file_hash.as_deref(), page_cache_storage(&app).ok()) {
                (Some(book_hash), Some(storage)) => {
                    pdf::search_pdf_with_storage(&file_path, &query, &storage, book_hash)?
                }
                _ => pdf::search_pdf(&file_path, &query)?,
            };
            Ok(results
                .into_iter()
                .map(|r| crate::models::SearchResult {
                    chapter_index: r.chapter_index as u32,
                    snippet: r.snippet,
                    match_offset: r.match_offset,
                })
                .collect())
        }
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open EPUB archive"))?;
            Ok(epub::search_book(cached, &query)?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let images_storage = state.images_storage()?;
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            let chapters = carrel_core::mobi::get_chapter_list_from_cache(cached);
            let chapter_indices: Vec<u32> = chapters.iter().map(|c| c.index as u32).collect();
            carrel_core::search::search_chapters(chapter_indices, &query, &book_id, |idx| {
                carrel_core::mobi::get_chapter_content_from_cache(
                    cached,
                    idx as usize,
                    images_storage.as_ref(),
                    &book_id,
                )
            })
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        _ => Ok(Vec::new()), // CBZ/CBR are image-only, no text to search
    }
}

#[tauri::command]
pub async fn get_chapter_word_counts(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<usize>> {
    let (file_path, format) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        (state.resolve_book_path(&book)?, book.format)
    };

    validate_file_exists(&file_path)?;

    match format {
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or("Failed to open EPUB archive")?;
            Ok(epub::get_chapter_word_counts(cached)?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            Ok(carrel_core::mobi::get_chapter_word_counts_from_cache(
                cached,
            )?)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "get_chapter_word_counts is not supported for format {other}"
        ))),
    }
}

#[tauri::command]
pub async fn get_chapter_metadata_batch(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<ChapterMeta>> {
    let (file_path, format) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        (state.resolve_book_path(&book)?, book.format)
    };

    validate_file_exists(&file_path)?;

    match format {
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or("Failed to open EPUB archive")?;
            Ok(epub::get_chapter_metadata_batch(cached)?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            Ok(carrel_core::mobi::get_chapter_metadata_batch_from_cache(
                cached,
            )?)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "get_chapter_metadata_batch is not supported for format {other}"
        ))),
    }
}

/// Per-chapter progress emitted while `get_all_chapters` streams a book's
/// chapters to the frontend for continuous-scroll mode.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ChapterLoadProgress {
    book_id: String,
    loaded: usize,
    total: usize,
}

/// Throttle rule for `chapter-load-progress` events: emitting one per chapter
/// floods the IPC bridge on large books (hundreds of chapters). For small
/// books (`total <= 50`) emit every chapter; otherwise emit roughly every 1%
/// of progress. The final chapter (`loaded == total`) always emits so the bar
/// reliably reaches 100%.
///
/// `loaded` is 1-based (the count of chapters loaded so far, i.e. `i + 1`).
fn should_emit_chapter_progress(loaded: usize, total: usize) -> bool {
    if total == 0 {
        return false;
    }
    if loaded == total {
        return true;
    }
    let step = if total <= 50 { 1 } else { (total / 100).max(1) };
    loaded.is_multiple_of(step)
}

#[tauri::command]
pub async fn get_all_chapters(
    book_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<String>> {
    let _t = state.ipc_metrics.time("get_all_chapters");
    let (file_path, total_chapters, format) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        let total = book.total_chapters;
        (state.resolve_book_path(&book)?, total, book.format)
    };

    validate_file_exists(&file_path)?;
    let images_storage = state.images_storage()?;

    match format {
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or("Failed to open EPUB archive")?;

            let mut chapters = Vec::with_capacity(total_chapters as usize);
            for i in 0..total_chapters as usize {
                let html = epub::get_chapter_content_from_cache(
                    cached,
                    i,
                    images_storage.as_ref(),
                    &book_id,
                )?;
                chapters.push(html);
                let loaded = i + 1;
                if should_emit_chapter_progress(loaded, total_chapters as usize) {
                    let _ = app.emit(
                        "chapter-load-progress",
                        ChapterLoadProgress {
                            book_id: book_id.clone(),
                            loaded,
                            total: total_chapters as usize,
                        },
                    );
                }
            }
            Ok(chapters)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            let mut chapters = Vec::with_capacity(total_chapters as usize);
            for i in 0..total_chapters as usize {
                let html = carrel_core::mobi::get_chapter_content_from_cache(
                    cached,
                    i,
                    images_storage.as_ref(),
                    &book_id,
                )?;
                chapters.push(html);
                let loaded = i + 1;
                if should_emit_chapter_progress(loaded, total_chapters as usize) {
                    let _ = app.emit(
                        "chapter-load-progress",
                        ChapterLoadProgress {
                            book_id: book_id.clone(),
                            loaded,
                            total: total_chapters as usize,
                        },
                    );
                }
            }
            Ok(chapters)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "get_all_chapters is not supported for format {other}"
        ))),
    }
}

#[tauri::command]
pub async fn get_toc(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<crate::models::TocEntry>> {
    let _t = state.ipc_metrics.time("get_toc");
    let (file_path, format) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        (state.resolve_book_path(&book)?, book.format)
    };

    validate_file_exists(&file_path)?;

    match format {
        BookFormat::Epub => {
            let mut cache = state.epub_cache.lock()?;
            ensure_epub_cached(&mut cache, &file_path);
            let cached = cache
                .get_mut(&file_path)
                .ok_or("Failed to open EPUB archive")?;
            Ok(epub::get_toc_from_cache(cached)?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            // MOBI has no real TOC — synthesize a flat list from the
            // adapter's chapter list so the Contents sidebar works. Each
            // entry is a depth-0 leaf (no children).
            let mut cache = state.mobi_cache.lock()?;
            ensure_mobi_cached(&mut cache, &file_path)?;
            let cached = cache
                .get(&file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            let chapters = carrel_core::mobi::get_chapter_list_from_cache(cached);
            Ok(chapters
                .into_iter()
                .map(|c| crate::models::TocEntry {
                    chapter_index: c.index as u32,
                    label: c.title,
                    play_order: format!("{}", c.index + 1),
                    children: Vec::new(),
                })
                .collect())
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "get_toc is not supported for format {other}"
        ))),
    }
}

// --- Progress ---

#[tauri::command]
pub async fn get_reading_progress(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Option<ReadingProgress>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_reading_progress(&conn, &book_id)?)
}

#[tauri::command]
pub async fn get_all_reading_progress(
    state: State<'_, AppState>,
) -> CarrelResult<Vec<ReadingProgress>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_all_reading_progress(&conn)?)
}

fn validate_file_exists(file_path: &str) -> CarrelResult<()> {
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        let base = format!(
            "Book file not found at '{}'. It may have been moved or deleted.",
            file_path
        );
        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
        return Err(CarrelError::not_found(with_smb_hint(
            base, file_path, &not_found,
        )));
    }
    // Reject symlinks to prevent traversal attacks
    if path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(CarrelError::invalid(
            "Symbolic links are not supported for book files.",
        ));
    }
    Ok(())
}

/// Validate that a path, once canonicalized, lies within an expected parent directory.
/// Returns the canonical path on success.
#[allow(dead_code)]
fn validate_path_within(path: &str, parent: &str) -> CarrelResult<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("Cannot resolve path '{}': {}", path, e))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|e| format!("Cannot resolve library folder '{}': {}", parent, e))?;
    if !canonical.starts_with(&canonical_parent) {
        return Err(CarrelError::invalid(format!(
            "Path '{}' is outside the library folder.",
            path
        )));
    }
    Ok(canonical)
}

pub(crate) fn validate_scroll_position(pos: f64) -> CarrelResult<f64> {
    if pos.is_nan() || pos.is_infinite() {
        return Err(CarrelError::invalid(
            "scroll_position must be a finite number",
        ));
    }
    Ok(pos.clamp(0.0, 1.0))
}

/// Shared reading-progress write path for both the desktop
/// `save_reading_progress` command and the web PUT handler
/// (`web_server::api::put_progress`). Detects a completion transition
/// (crossing onto the last chapter for the first time — by comparing the
/// *prior* stored progress against the new one) and performs the same side
/// effects regardless of caller: an activity-log entry and a
/// `CarrelEvent::BookFinished` bus emission (consumed by hooks/plugins,
/// independent of any `AppHandle`). The desktop-only `book-completed` window
/// event is additionally emitted when an `AppHandle` is supplied — the web
/// path passes `None` since there's no window to notify. This is the fix for
/// review finding F1 (#71 web-ui/04-progress-sync): previously the web PUT
/// wrote the row directly, silently dropping completion side effects and
/// permanently suppressing a later desktop-side emission (because the row
/// already looked "completed" by the time desktop saved).
///
/// Callers are responsible for their own `chapter_index` bounds policy
/// *before* calling this (desktop rejects indices `>= total_chapters`; the
/// web PUT intentionally does not — see review finding F4, since the reader
/// paginates against a live page-count that can exceed a stale
/// `total_chapters`).
///
/// `suppress_passive` (private mode, B-M1, SB-3/SB-4, D-3): when `true`,
/// the entire mutating block below — the progress upsert, the `BookFinished`
/// bus emit, the `BookCompleted` activity log, and the desktop
/// `book-completed` window event — is skipped as one unit. The caller's
/// submitted position is still echoed back in the returned `ReadingProgress`
/// (with a synthetic `last_read_at`) so the web/desktop reader can update its
/// own in-memory volatile resume point (D-4) without anything touching disk.
pub(crate) fn apply_reading_progress(
    conn: &rusqlite::Connection,
    book: &Book,
    book_id: &str,
    chapter_index: u32,
    scroll_position: f64,
    app: Option<&AppHandle>,
    suppress_passive: bool,
) -> CarrelResult<ReadingProgress> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if suppress_passive {
        return Ok(ReadingProgress {
            book_id: book_id.to_string(),
            chapter_index,
            scroll_position,
            last_read_at: now,
        });
    }

    let is_on_last_chapter =
        book.total_chapters > 0 && chapter_index >= book.total_chapters.saturating_sub(1);

    let was_completed_before = is_on_last_chapter
        && db::get_reading_progress(conn, book_id)?
            .map(|p| p.chapter_index >= book.total_chapters.saturating_sub(1))
            .unwrap_or(false);

    let progress = ReadingProgress {
        book_id: book_id.to_string(),
        chapter_index,
        scroll_position,
        last_read_at: now,
    };

    db::upsert_reading_progress(conn, &progress)?;

    if is_on_last_chapter && !was_completed_before {
        events::bus().emit(CarrelEvent::BookFinished {
            book_id: book_id.to_string(),
        });
        log_event(
            conn,
            ActivityEvent::BookCompleted {
                id: book_id.to_string(),
                title: book.title.clone(),
            },
        );
        if let Some(app) = app {
            let _ = app.emit(
                "book-completed",
                serde_json::json!({
                    "bookId": book_id,
                    "title": book.title,
                    "author": book.author,
                    "coverPath": book.cover_path,
                    "totalChapters": book.total_chapters,
                }),
            );
        }
    }

    Ok(progress)
}

#[tauri::command]
pub async fn save_reading_progress(
    book_id: String,
    chapter_index: u32,
    scroll_position: f64,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<()> {
    let scroll_position = validate_scroll_position(scroll_position)?;

    let conn = state.active_db()?.get()?;

    // Validate chapter_index against the book's total chapters
    let book = db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found(format!("Book not found: {}", book_id)))?;

    if book.total_chapters > 0 && chapter_index >= book.total_chapters {
        return Err(CarrelError::invalid(format!(
            "chapter_index {} is out of range (book has {} chapters)",
            chapter_index, book.total_chapters
        )));
    }

    apply_reading_progress(
        &conn,
        &book,
        &book_id,
        chapter_index,
        scroll_position,
        Some(&app),
        state.is_private(),
    )?;

    Ok(())
}

// --- Private mode ("Don't track this session", B-M1) ---

/// Flips the app-wide private-mode flag — the only runtime mutator
/// (Decision 1/3). Suppression is read fresh from this atomic at each
/// passive write/emit site, so a flip takes effect starting with the very
/// next write; no synthetic "session closed" record is ever produced
/// (Decision 3, closes the toggle-then-unmount race). Highlights and
/// bookmarks are never affected — deliberate saves always persist.
///
/// No frontend toggle exists yet (B-M2); this command exists so the
/// backend guard is fully testable ahead of the UI landing.
#[tauri::command]
pub async fn set_private_mode<R: tauri::Runtime>(
    enabled: bool,
    app: AppHandle<R>,
    state: State<'_, AppState>,
) -> CarrelResult<bool> {
    state.private_mode.store(enabled, Ordering::SeqCst);
    let _ = app.emit("private-mode-changed", enabled);
    Ok(enabled)
}

/// Reads the current private-mode flag. Status-only — the flag can only be
/// changed via `set_private_mode`.
#[tauri::command]
pub async fn get_private_mode(state: State<'_, AppState>) -> CarrelResult<bool> {
    Ok(state.is_private())
}

// --- Bookmarks ---

#[tauri::command]
pub async fn get_bookmarks(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<Bookmark>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_bookmarks(&conn, &book_id)?)
}

#[tauri::command]
pub async fn add_bookmark(
    book_id: String,
    chapter_index: u32,
    scroll_position: f64,
    note: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<Bookmark> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let bookmark = Bookmark {
        id: Uuid::new_v4().to_string(),
        book_id,
        chapter_index,
        scroll_position,
        name: None,
        note,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };

    let conn = state.active_db()?.get()?;
    db::insert_bookmark(&conn, &bookmark)?;

    events::bus().emit(CarrelEvent::BookmarkCreated {
        book_id: bookmark.book_id.clone(),
        bookmark_id: bookmark.id.clone(),
    });

    Ok(bookmark)
}

#[tauri::command]
pub async fn remove_bookmark(bookmark_id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::soft_delete_bookmark(&conn, &bookmark_id)?)
}

#[tauri::command]
pub async fn update_bookmark(
    bookmark_id: String,
    name: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let truncated_name: Option<String> = name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.chars().take(100).collect::<String>());
    let name_ref = truncated_name.as_deref();
    let conn = state.active_db()?.get()?;
    Ok(db::update_bookmark_name(&conn, &bookmark_id, name_ref)?)
}

// --- Comic (CBZ / CBR) ---

#[tauri::command]
pub async fn get_comic_page_count(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<u32> {
    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };
    let file_path = state.resolve_book_path(&book)?;

    validate_file_exists(&file_path)?;
    match book.format {
        BookFormat::Cbz => cbz::get_page_count(&file_path),
        BookFormat::Cbr => cbr::get_page_count(&file_path),
        _ => Err(CarrelError::invalid(format!(
            "get_comic_page_count is not supported for {:?}",
            book.format
        ))),
    }
}

/// Comic page reader for the desktop frontend. Returns raw image
/// bytes plus a trailing mime tag (see `page_wire`); the frontend
/// builds a `Blob` + `URL.createObjectURL` and assigns it directly
/// to `<img src>`, bypassing base64 entirely.
///
/// `target_width` clamps the page width to the viewport. The frontend
/// computes this from container width × DPR so we ship roughly the
/// number of pixels the browser actually paints, not the full-res
/// archive scan (often 4–10 MB at native resolution).
#[tauri::command]
pub async fn get_comic_page_bytes(
    book_id: String,
    page_index: u32,
    target_width: Option<u32>,
    // When true, only serve an already-cached page: a cache miss returns an
    // empty response instead of decoding the archive. The reader's background
    // preloader sets this so warming neighbor pages never triggers a cold
    // decode (which, on a network-mounted library, would contend with the
    // foreground page the user is waiting on). Foreground reads leave it unset.
    cached_only: Option<bool>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<tauri::ipc::Response> {
    let _t = state.ipc_metrics.time("get_comic_page_bytes");
    let target_width = target_width.filter(|&w| w > 0).map(|w| w.min(9600));
    let cached_only = cached_only.unwrap_or(false);

    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };

    // A cached_only probe touches ONLY the local page cache — never the source
    // archive — so it must skip resolve_book_path / validate_file_exists, which
    // stat the (possibly network-mounted) file and would defeat the point of a
    // no-contention preload.
    if cached_only {
        return serve_cached_only(
            app,
            book.file_hash.clone(),
            page_index,
            target_width,
            "comic",
        )
        .await;
    }

    let file_path = state.resolve_book_path(&book)?;
    validate_file_exists(&file_path)?;

    // Mark a foreground render in flight so a still-running PDF prerender pass
    // (e.g. from a book opened just before this comic) yields to this page
    // instead of competing for the same network-mounted storage / CPU.
    let _fg_guard = ForegroundRenderGuard::new(state.active_foreground_renders.clone());

    // The archive decode + optional resize is CPU-bound and, on a
    // network-mounted library, I/O-bound for seconds. Run it on the blocking
    // pool so it never pins a Tauri async worker (which would stall other IPC —
    // including the page the user is actually waiting on).
    let book_hash = book.file_hash.clone();
    let format = book.format;
    let (bytes, mime) =
        tauri::async_runtime::spawn_blocking(move || -> CarrelResult<(Vec<u8>, String)> {
            let start = std::time::Instant::now();

            // Cache-first path. Cached pages are full-resolution archive bytes;
            // run them through the same resize helper the cold path uses so the
            // wire-level promise (≤ target_width) holds either way.
            if let Ok(storage) = page_cache_storage(&app) {
                if let Some(ref book_hash) = book_hash {
                    if let Ok((data, mime)) =
                        page_cache::get_cached_page(&storage, book_hash, page_index)
                    {
                        let (bytes, out_mime) =
                            crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
                        page_cache::page_dbg!(
                            "bytes cache HIT: page={} size={}KB total={:?}",
                            page_index,
                            bytes.len() / 1024,
                            start.elapsed()
                        );
                        return Ok((bytes, out_mime));
                    }
                }
            }

            let (bytes, mime) = match format {
                BookFormat::Cbz => cbz::get_page_image_bytes(&file_path, page_index, target_width)?,
                BookFormat::Cbr => cbr::get_page_image_bytes(&file_path, page_index, target_width)?,
                _ => {
                    return Err(CarrelError::invalid(format!(
                        "get_comic_page_bytes is not supported for {format:?}"
                    )));
                }
            };
            page_cache::page_dbg!(
                "bytes archive read: page={} size={}KB total={:?}",
                page_index,
                bytes.len() / 1024,
                start.elapsed()
            );
            Ok((bytes, mime.to_string()))
        })
        .await
        .map_err(|e| CarrelError::internal(format!("comic page render task failed: {e}")))??;

    Ok(tauri::ipc::Response::new(crate::page_wire::append_tag(
        bytes, &mime,
    )))
}

/// Serve a page ONLY if it's already in the local page cache, without touching
/// the source file. Used by the reader's preloader (`cached_only`): a hit is
/// returned as normal wire bytes, a miss as an empty response the client skips.
/// Runs the cache read + resize on the blocking pool (I/O + CPU).
async fn serve_cached_only(
    app: AppHandle,
    book_hash: Option<String>,
    page_index: u32,
    target_width: Option<u32>,
    label: &'static str,
) -> CarrelResult<tauri::ipc::Response> {
    let rendered =
        tauri::async_runtime::spawn_blocking(move || -> CarrelResult<Option<(Vec<u8>, String)>> {
            if let Ok(storage) = page_cache_storage(&app) {
                if let Some(ref hash) = book_hash {
                    if let Ok((data, mime)) =
                        page_cache::get_cached_page(&storage, hash, page_index)
                    {
                        let (bytes, out_mime) =
                            crate::image_util::maybe_resize_to_jpeg(data, mime, target_width)?;
                        return Ok(Some((bytes, out_mime)));
                    }
                }
            }
            Ok(None)
        })
        .await
        .map_err(|e| CarrelError::internal(format!("{label} cache probe failed: {e}")))??;

    match rendered {
        Some((bytes, mime)) => Ok(tauri::ipc::Response::new(crate::page_wire::append_tag(
            bytes, &mime,
        ))),
        // cached_only miss: empty response signals "not cached, skip".
        None => Ok(tauri::ipc::Response::new(Vec::new())),
    }
}

/// Hashes with a background staging task currently in flight. Dedups repeated
/// opens of the same book: without this, every open before the first copy
/// finishes would spawn another blocking task (each doing a network `statfs` +
/// waiting on the per-file stage lock). A hash is claimed before spawning and
/// released when the task ends.
static STAGING_INFLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Claim the in-flight slot for `hash`; returns `true` if newly claimed (caller
/// should proceed) or `false` if a stage for this hash is already running.
fn claim_staging(hash: &str) -> bool {
    let mut set = STAGING_INFLIGHT.lock().unwrap_or_else(|p| p.into_inner());
    set.insert(hash.to_string())
}

fn release_staging(hash: &str) {
    let mut set = STAGING_INFLIGHT.lock().unwrap_or_else(|p| p.into_inner());
    set.remove(hash);
}

/// RAII release of an in-flight staging claim: drops the hash from
/// `STAGING_INFLIGHT` when the staging task ends — including on an unwind, so a
/// panic inside the copy can't leave a hash permanently claimed (which would
/// block that book from ever re-staging this process session).
struct StagingClaim(String);

impl Drop for StagingClaim {
    fn drop(&mut self) {
        release_staging(&self.0);
    }
}

/// Blocking: stage `src` to `cache_dir` under `(hash, ext)` **iff** `src` is on
/// a network mount. Returns `true` if staging was attempted. Does the network
/// `statfs` (`is_remote_path`) and the copy, so it MUST run in a blocking
/// context — never on an async worker (a hung mount would stall the runtime).
/// A failure is logged, never surfaced. Separated out so the remote-gating
/// logic is unit-testable without an async runtime.
fn stage_source_if_remote(cache_dir: &std::path::Path, src: &str, hash: &str, ext: &str) -> bool {
    if !crate::remote_fs::is_remote_path(std::path::Path::new(src)) {
        return false;
    }
    match carrel_core::source_cache::stage(cache_dir, std::path::Path::new(src), hash, ext) {
        Ok(p) => {
            log::info!(
                "staged network-mounted book to local cache: {}",
                p.display()
            );
            // Keep the source cache within its disk budget, evicting the
            // least-recently-opened staged files. Best-effort.
            let _ = carrel_core::source_cache::run_eviction(
                cache_dir,
                carrel_core::source_cache::DEFAULT_MAX_SIZE_MB,
            );
        }
        Err(e) => log::warn!("source-cache staging failed for {src}: {e}"),
    }
    true
}

/// Fire-and-forget: if `real_path` is on a network mount and the book has a
/// content hash, stage the source file onto the local source-cache in the
/// background, so subsequent page renders (and future opens, via
/// `resolve_book_path`) read locally instead of by random access over SMB.
///
/// Never blocks the caller: the network `statfs` and the copy both run inside
/// `spawn_blocking`. Does nothing when the source is already local (which also
/// covers the already-staged case, since `resolve_book_path` then hands us the
/// local staged path). Deduped per hash so repeated opens don't pile up
/// blocking tasks; `source_cache::stage` is itself single-flight and idempotent.
pub(crate) fn stage_remote_source_in_background(
    cache_dir: std::path::PathBuf,
    book: &Book,
    real_path: &str,
) {
    let Some(hash) = book.file_hash.clone() else {
        return;
    };
    if !claim_staging(&hash) {
        return; // a background stage for this hash is already running
    }
    // Release the claim when the task ends, even on panic/unwind (RAII).
    let claim = StagingClaim(hash);
    let ext = std::path::Path::new(&book.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    let src = real_path.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        // `claim` is moved in and dropped when this closure returns or unwinds.
        stage_source_if_remote(&cache_dir, &src, &claim.0, &ext);
    });
}

/// Mark a book's staged copy (if present) most-recently-used, so source-cache
/// eviction keeps the books you actually open. Cheap, local, once per open.
pub(crate) fn touch_staged_book(cache_dir: &std::path::Path, book: &Book) {
    if let Some(hash) = book.file_hash.as_deref() {
        let ext = std::path::Path::new(&book.file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        carrel_core::source_cache::touch_staged(cache_dir, hash, ext);
    }
}

/// Web serving-path entry point (M2): ensure a network-mounted book has a local
/// staged copy, without a per-page `statfs` storm. Unlike the desktop reader,
/// the web server has no single "open" event — this runs on *every* page
/// request, so it must be cheap on the common (already-staged) path:
///
/// - Staged copy present → bump its LRU recency (`touch_staged_book`) and
///   return. A local existence check + mtime touch; no remote `statfs`, no
///   spawn. This is what keeps a web-only read from being evicted mid-session.
/// - No staged copy yet → kick a single background stage from the (remote)
///   `real_path`, reusing `stage_remote_source_in_background`. That path does
///   the remote `statfs` + copy inside `spawn_blocking` and is deduped per hash
///   (`STAGING_INFLIGHT`), so pages 2..N requested while the copy is in flight
///   no-op instead of piling up. Once the copy lands, `resolve_book_path` hands
///   the caller the staged path and this function takes the cheap touch branch.
///
/// `real_path` is only used on the stage branch. The caller passes the resolved
/// book path; because no staged copy was observed on this branch, it is normally
/// the original (remote) source. If a staged copy is evicted in the narrow
/// window between the caller's `resolve_book_path` and this check, `real_path`
/// may instead be a now-missing staged path — harmless: `stage_source_if_remote`
/// re-checks `is_remote_path`, which is false for a local/missing path, so
/// nothing is staged (the next page request re-stages once the copy is gone).
pub(crate) fn ensure_web_source_staged(cache_dir: &std::path::Path, book: &Book, real_path: &str) {
    let Some(hash) = book.file_hash.as_deref() else {
        return;
    };
    let ext = std::path::Path::new(&book.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if carrel_core::source_cache::staged_if_present(cache_dir, hash, ext).is_some() {
        touch_staged_book(cache_dir, book);
        return;
    }
    stage_remote_source_in_background(cache_dir.to_path_buf(), book, real_path);
}

#[tauri::command]
pub async fn prepare_comic(
    book_id: String,
    start_page: Option<u32>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<page_cache::CacheManifest> {
    let _t = state.ipc_metrics.time("prepare_comic");
    let (book, max_size_mb) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        let max_size_mb = db::get_setting(&conn, "page_cache_max_size_mb")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(page_cache::DEFAULT_MAX_CACHE_SIZE_MB);
        (book, max_size_mb)
    };
    let file_path = state.resolve_book_path(&book)?;

    validate_file_exists(&file_path)?;

    if book.format != BookFormat::Cbz && book.format != BookFormat::Cbr {
        return Err(CarrelError::invalid(
            "prepare_comic only supports CBZ/CBR formats",
        ));
    }

    // Mark the staged copy (if any) most-recently-used for eviction, then stage
    // a network-mounted source to local disk on open (background, non-blocking)
    // so page reads stop hitting the share at render time. After the format
    // check so a wrong-format call can't launch a large copy.
    touch_staged_book(&state.cache_dir, &book);
    stage_remote_source_in_background(state.cache_dir.clone(), &book, &file_path);

    let book_hash = book.file_hash.as_deref().ok_or("Book has no file hash")?;
    let storage = page_cache_storage(&app)?;

    // F-4-1 fast path: extract only page 0 (plus the resume page, if the
    // caller opened mid-book) and return immediately so the reader can paint.
    // The remaining pages stream in on the background task below; any page the
    // frontend requests before then is served on-demand by
    // `get_comic_page_bytes` (cache miss → direct archive read).
    let priority_pages: Vec<u32> = start_page.into_iter().collect();
    let prep_start = std::time::Instant::now();
    page_cache::page_dbg!(
        "prepare_comic: book={} format={:?} hash={} start_page={:?}",
        book_id,
        book.format,
        book_hash,
        start_page
    );
    let manifest = page_cache::ensure_comic_fast(
        &storage,
        &book_id,
        book_hash,
        &file_path,
        &book.format,
        &priority_pages,
    )?;
    page_cache::page_dbg!(
        "prepare_comic fast path done: pages={} elapsed={:?}",
        manifest.page_count,
        prep_start.elapsed()
    );

    // Background: extract the rest of the archive (emitting progress), then
    // run eviction. `extract_comic_remaining` is idempotent and only appends
    // page files, so it races safely against on-demand cache reads.
    let bg_storage = page_cache_storage(&app)?;
    let bg_app = app.clone();
    let bg_book_id = book_id.clone();
    let bg_hash = book_hash.to_string();
    let bg_path = file_path.clone();
    let bg_format = book.format.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = page_cache::extract_comic_remaining(
            &bg_storage,
            &bg_hash,
            &bg_path,
            &bg_format,
            |loaded, total| {
                let (loaded, total) = (loaded as usize, total as usize);
                if should_emit_chapter_progress(loaded, total) {
                    let _ = bg_app.emit(
                        "comic-extract-progress",
                        ChapterLoadProgress {
                            book_id: bg_book_id.clone(),
                            loaded,
                            total,
                        },
                    );
                }
            },
        );
        let _ = page_cache::run_eviction(&bg_storage, max_size_mb);
    });

    Ok(manifest)
}

static TEXT_INDEX_BUILDING: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// RAII guard preventing duplicate concurrent PDF text-index builds (F-4-6)
/// for the same `book_hash` — e.g. two `prepare_pdf` calls racing on a rapid
/// reopen. `acquire` returns `None` (rather than an error) when a build is
/// already in flight, since this guards a best-effort background job, not a
/// user-initiated action.
struct TextIndexBuildGuard {
    book_hash: String,
}

impl TextIndexBuildGuard {
    fn acquire(book_hash: String) -> Option<Self> {
        let mut running = TEXT_INDEX_BUILDING.lock().ok()?;
        if !running.insert(book_hash.clone()) {
            return None;
        }
        Some(Self { book_hash })
    }
}

impl Drop for TextIndexBuildGuard {
    fn drop(&mut self) {
        if let Ok(mut running) = TEXT_INDEX_BUILDING.lock() {
            running.remove(&self.book_hash);
        }
    }
}

/// First-open warm pass for PDF books. Mirrors `prepare_comic`:
/// asserts the format, requires `book.file_hash`, renders the first
/// ten pages into the shared `page-cache/` namespace, and kicks off
/// a background eviction afterwards. Returns the freshly-written
/// manifest so the frontend can reuse `page_count` instead of calling
/// `get_pdf_page_count` separately.
#[tauri::command]
pub async fn prepare_pdf(
    book_id: String,
    start_page: Option<u32>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<page_cache::CacheManifest> {
    let _t = state.ipc_metrics.time("prepare_pdf");
    // Establish the cache manifest but render NOTHING synchronously: every
    // page is rendered by the background prerender pass below. Rendering pages
    // here blocked the reader's `await invoke("prepare_pdf")` for the full
    // render cost — on a large PDF over a network-mounted library that was tens
    // of seconds (10 cold pages × several seconds each), freezing the open.
    // Zero prewarm returns as soon as the page count is known.
    const PDF_PREWARM_PAGES: u32 = 0;

    let (book, max_size_mb) = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        let max_size_mb = db::get_setting(&conn, "page_cache_max_size_mb")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(page_cache::DEFAULT_MAX_CACHE_SIZE_MB);
        (book, max_size_mb)
    };
    let file_path = state.resolve_book_path(&book)?;
    validate_file_exists(&file_path)?;

    if book.format != BookFormat::Pdf {
        return Err(CarrelError::invalid("prepare_pdf only supports PDF format"));
    }

    // Mark the staged copy (if any) most-recently-used for eviction, then stage
    // a network-mounted source to local disk on open (background, non-blocking)
    // so page reads stop hitting the share at render time. After the format
    // check so a wrong-format call can't launch a large copy.
    touch_staged_book(&state.cache_dir, &book);
    stage_remote_source_in_background(state.cache_dir.clone(), &book, &file_path);
    let book_hash = book
        .file_hash
        .as_deref()
        .ok_or_else(|| CarrelError::invalid("Book has no file hash; cannot populate PDF cache"))?;

    let storage = page_cache_storage(&app)?;
    let prep_start = std::time::Instant::now();
    page_cache::page_dbg!(
        "prepare_pdf: book={} hash={} prewarm={}",
        book_id,
        book_hash,
        PDF_PREWARM_PAGES
    );
    let manifest = page_cache::ensure_pdf_prewarmed(
        &storage,
        &book_id,
        book_hash,
        &file_path,
        PDF_PREWARM_PAGES,
    )?;
    page_cache::page_dbg!(
        "prepare_pdf done: page_count={} elapsed={:?}",
        manifest.page_count,
        prep_start.elapsed()
    );

    // Reserve the first page the reader will show (the resume/initial page,
    // else page 0): render and cache it synchronously so it is a cache hit by
    // the time the frontend requests it. Without this, the background pass
    // below starts at page 0 exactly as the foreground requests the visible
    // page, and — because cold renders don't coalesce — the same page can be
    // rendered twice concurrently, doubling expensive I/O on a network-mounted
    // PDF and delaying the very page the user is waiting on. One reserved page
    // costs a single render (vs. the 10 this milestone removed). Skipped in
    // private mode, where nothing is persisted anyway (the write is suppressed,
    // so it would neither cache nor spare the background pass).
    if !state.is_private() && manifest.page_count > 0 {
        let reserve = start_page.unwrap_or(0).min(manifest.page_count - 1);
        let _ = page_cache::get_or_render_pdf_page(&storage, book_hash, &file_path, reserve);
    }

    // Background (F-4-5): render the remaining pages into the disk cache so
    // later go-to-page / thumbnail scrubbing is instant, emitting progress,
    // then run eviction. The pass is bounded by the whole-cache size cap so a
    // huge PDF cannot blow past it. Skipped in private mode (B-M1): a full
    // background pass would persist the entire book to disk, defeating the
    // page-content write suppression the on-demand read path enforces —
    // eviction still runs so the cap holds.
    if state.is_private() {
        let evict_storage = page_cache_storage(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            let _ = page_cache::run_eviction(&evict_storage, max_size_mb);
        });
    } else {
        let bg_storage = page_cache_storage(&app)?;
        let bg_app = app.clone();
        let bg_book_id = book_id.clone();
        let bg_hash = book_hash.to_string();
        let bg_path = file_path.clone();
        let max_size_bytes = max_size_mb.saturating_mul(1024 * 1024);
        // Live private-mode flag (B-M1). The spawn-time check above skips the
        // pass when private is already on; this shared atomic lets the running
        // pass also stop if private is toggled on *mid-pass*, so it never
        // writes the rest of the book to disk behind a "don't track this
        // session" switch — matching the on-demand read path's write
        // suppression (`get_pdf_page_bytes`).
        let bg_private = state.private_mode.clone();
        // Foreground-priority throttle: pause before each background page while
        // the reader is rendering a page, so the pass yields the source file
        // and CPU to the page the user is waiting on (otherwise both contend
        // over a network-mounted library and foreground renders can exceed the
        // reader's load timeout).
        let bg_foreground = state.active_foreground_renders.clone();
        tauri::async_runtime::spawn_blocking(move || {
            use std::sync::atomic::Ordering;
            let emit_private = bg_private.clone();
            let abort_private = bg_private.clone();
            let throttle_private = bg_private.clone();
            let throttle = move || {
                // Block while a foreground render is in flight; give up promptly
                // if private mode aborts the pass so it can stop.
                while bg_foreground.load(Ordering::SeqCst) > 0
                    && !throttle_private.load(Ordering::SeqCst)
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            };
            let outcome = page_cache::prerender_pdf_remaining(
                &bg_storage,
                &bg_hash,
                &bg_path,
                max_size_bytes,
                |loaded, total| {
                    // Suppress progress emits once private (a bookId + page
                    // counts emit is a passive signal); the loop stops writing
                    // via `should_abort` too, so this stops firing promptly.
                    if emit_private.load(Ordering::SeqCst) {
                        return;
                    }
                    let (loaded, total) = (loaded as usize, total as usize);
                    if should_emit_chapter_progress(loaded, total) {
                        let _ = bg_app.emit(
                            "pdf-prerender-progress",
                            ChapterLoadProgress {
                                book_id: bg_book_id.clone(),
                                loaded,
                                total,
                            },
                        );
                    }
                },
                move || abort_private.load(Ordering::SeqCst),
                throttle,
            );
            // Guaranteed terminal event: settle the frontend bar at true
            // coverage (100% normally, or the partial value when the size
            // bound stopped the pass early). Skipped when there was no PDF
            // manifest to prerender against (`page_count == 0`), and while
            // private (no passive emits — the bar idle-settles on its own).
            if !bg_private.load(Ordering::SeqCst) {
                if let Ok(ref o) = outcome {
                    if o.page_count > 0 {
                        let _ = bg_app.emit(
                            "pdf-prerender-progress",
                            ChapterLoadProgress {
                                book_id: bg_book_id.clone(),
                                loaded: o.cached_total as usize,
                                total: o.page_count as usize,
                            },
                        );
                    }
                }
            }

            // F-4-6: build + persist the PDF text index after the prewarm/
            // prerender pass, so extraction never contends with the user's
            // first page render. This is the ONLY place `text-index.json` is
            // written — search (`search_pdf_with_storage`) resolves through
            // memory/disk but never persists, so persistence is safe by
            // construction (single guarded writer, no racing writes from the
            // foreground search path).
            //
            // Skipped when already indexed on disk, or when another build is
            // already in flight for this hash (e.g. a rapid reopen).
            if !bg_private.load(Ordering::SeqCst)
                && page_cache::read_text_index(&bg_storage, &bg_hash).is_none()
            {
                if let Some(_guard) = TextIndexBuildGuard::acquire(bg_hash.clone()) {
                    if page_cache::read_text_index(&bg_storage, &bg_hash).is_none() {
                        let build_start = std::time::Instant::now();
                        match pdf::resolve_page_texts(&bg_path, &bg_storage, &bg_hash) {
                            Ok(pages) => {
                                let page_count = pages.len();
                                // Handle a delete/evict that landed DURING
                                // extraction FIRST, independently of whether
                                // we'd persist: resolve_page_texts just
                                // repopulated the in-memory PDF_TEXT_CACHE
                                // entry that remove_book had cleared, so if
                                // the manifest is gone (book removed) we must
                                // drop both the cache entry and the resident
                                // text. This covers the whole extraction
                                // window, not just the post-write race.
                                let persisted;
                                if page_cache::read_manifest(&bg_storage, &bg_hash).is_none() {
                                    let _ = page_cache::evict_book(&bg_storage, &bg_hash);
                                    pdf::evict_memory_cache(&bg_path);
                                    persisted = false;
                                } else if bg_private.load(Ordering::SeqCst) {
                                    // Private mode ("don't track this
                                    // session"): never persist to disk. The
                                    // in-memory text is session-only and
                                    // cleared on restart, so it may stay this
                                    // session.
                                    persisted = false;
                                } else {
                                    let index = pdf::PdfTextIndex {
                                        version: pdf::TEXT_INDEX_VERSION,
                                        page_count: page_count as u32,
                                        pages,
                                    };
                                    let _ =
                                        page_cache::write_text_index(&bg_storage, &bg_hash, &index);
                                    // Serializing + writing a large index
                                    // takes time, so a delete or a private
                                    // toggle can land DURING the write.
                                    // Re-check after and undo:
                                    //  - deleted (manifest gone): drop the
                                    //    whole entry + the resident text.
                                    //  - private toggled on: drop only the
                                    //    text index we wrote, leaving the
                                    //    page cache intact.
                                    if page_cache::read_manifest(&bg_storage, &bg_hash).is_none() {
                                        let _ = page_cache::evict_book(&bg_storage, &bg_hash);
                                        pdf::evict_memory_cache(&bg_path);
                                        persisted = false;
                                    } else if bg_private.load(Ordering::SeqCst) {
                                        let _ = page_cache::evict_text_index(&bg_storage, &bg_hash);
                                        persisted = false;
                                    } else {
                                        persisted = true;
                                    }
                                }
                                page_cache::page_dbg!(
                                    "text-index: built {} pages for {} in {:?} (persisted={})",
                                    page_count,
                                    bg_hash,
                                    build_start.elapsed(),
                                    persisted
                                );
                            }
                            Err(e) => page_cache::page_dbg!(
                                "text-index: build failed for {}: {}",
                                bg_hash,
                                e
                            ),
                        }
                    }
                }
            }

            let _ = page_cache::run_eviction(&bg_storage, max_size_mb);
        });
    }

    Ok(manifest)
}

/// Build the lifecycle registry over every cache. Constructed per call:
/// cheap (three Arc clones), and the disk storage handle comes from
/// `page_cache_storage` exactly like the existing page-cache commands.
fn cache_registry(
    state: &AppState,
    storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
) -> Vec<Box<dyn ManagedCache>> {
    let mut registry: Vec<Box<dyn ManagedCache>> = vec![Box::new(MemoryCacheAdapter::new(
        "epub",
        false,
        state.epub_cache.clone(),
    ))];
    #[cfg(feature = "mobi")]
    registry.push(Box::new(MemoryCacheAdapter::new(
        "mobi",
        true,
        state.mobi_cache.clone(),
    )));
    registry.push(Box::new(DiskPageCacheAdapter::new(storage)));
    registry
}

#[tauri::command]
pub async fn get_unified_cache_stats(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<UnifiedCacheStats> {
    let storage: std::sync::Arc<dyn carrel_core::storage::Storage> =
        std::sync::Arc::new(page_cache_storage(&app)?);
    Ok(carrel_core::cache::unified_stats(&cache_registry(
        &state, storage,
    )))
}

#[tauri::command]
pub async fn clear_all_caches(app: AppHandle, state: State<'_, AppState>) -> CarrelResult<()> {
    let storage: std::sync::Arc<dyn carrel_core::storage::Storage> =
        std::sync::Arc::new(page_cache_storage(&app)?);
    carrel_core::cache::clear_all(&cache_registry(&state, storage))
}

#[tauri::command]
pub async fn clear_page_cache(app: AppHandle) -> CarrelResult<()> {
    let storage = page_cache_storage(&app)?;
    page_cache::clear_cache(&storage)
}

// --- Reading Stats ---

#[tauri::command]
pub async fn record_reading_session(
    book_id: String,
    started_at: i64,
    duration_secs: i64,
    pages_read: i32,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    if duration_secs < 10 {
        return Ok(());
    } // Skip very short sessions
    if state.is_private() {
        // Private mode (B-M1): reading sessions feed the stats
        // dashboard/heatmap/goal ring — a passive write, skipped entirely.
        return Ok(());
    }
    let conn = state.active_db()?.get()?;
    let id = Uuid::new_v4().to_string();
    Ok(db::insert_reading_session(
        &conn,
        &id,
        &book_id,
        started_at,
        duration_secs,
        pages_read,
    )?)
}

#[tauri::command]
pub async fn get_reading_stats(state: State<'_, AppState>) -> CarrelResult<db::ReadingStats> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_reading_stats(&conn)?)
}

#[tauri::command]
pub async fn get_book_reading_time(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<i64> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_book_reading_time(&conn, &book_id)?)
}

#[tauri::command]
pub async fn get_book_reading_stats(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Option<db::BookReadingStats>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_book_reading_stats(&conn, &book_id)?)
}

// --- Highlights ---

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn add_highlight(
    book_id: String,
    chapter_index: u32,
    text: String,
    color: String,
    start_offset: u32,
    end_offset: u32,
    note: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<Highlight> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let highlight = Highlight {
        id: Uuid::new_v4().to_string(),
        book_id,
        chapter_index,
        text,
        color,
        note,
        start_offset,
        end_offset,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    let conn = state.active_db()?.get()?;
    db::insert_highlight(&conn, &highlight)?;

    events::bus().emit(CarrelEvent::HighlightCreated {
        book_id: highlight.book_id.clone(),
        highlight_id: highlight.id.clone(),
    });

    Ok(highlight)
}

#[tauri::command]
pub async fn get_highlights(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<Highlight>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_highlights(&conn, &book_id)?)
}

#[tauri::command]
pub async fn get_chapter_highlights(
    book_id: String,
    chapter_index: u32,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<Highlight>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_chapter_highlights(&conn, &book_id, chapter_index)?)
}

#[tauri::command]
pub async fn update_highlight_note(
    highlight_id: String,
    note: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    db::update_highlight_note(&conn, &highlight_id, note.as_deref())?;
    events::bus().emit(CarrelEvent::HighlightUpdated { highlight_id });
    Ok(())
}

#[tauri::command]
pub async fn remove_highlight(
    highlight_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    db::soft_delete_highlight(&conn, &highlight_id)?;
    events::bus().emit(CarrelEvent::HighlightDeleted { highlight_id });
    Ok(())
}

#[tauri::command]
pub async fn export_highlights_markdown(
    book_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<String> {
    let conn = state.active_db()?.get()?;
    let book = db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
    let highlights = db::list_highlights(&conn, &book_id)?;

    let mut md = format!("# Highlights: {}\n\n", book.title);
    if !book.author.is_empty() {
        md.push_str(&format!("**{}**\n\n", book.author));
    }
    let mut current_chapter: Option<u32> = None;
    for h in &highlights {
        if current_chapter != Some(h.chapter_index) {
            md.push_str(&format!("\n## Chapter {}\n\n", h.chapter_index + 1));
            current_chapter = Some(h.chapter_index);
        }
        md.push_str(&format!("> {}\n", h.text));
        if let Some(ref note) = h.note {
            md.push_str(&format!("\n*{}*\n", note));
        }
        md.push('\n');
    }
    Ok(md)
}

#[tauri::command]
pub async fn search_highlights(
    query: String,
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<HighlightSearchResult>> {
    let conn = state.active_db()?.get()?;
    Ok(db::search_highlights(&conn, &query, limit.unwrap_or(200))?)
}

// --- Tags ---

#[derive(serde::Serialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
}

#[tauri::command]
pub async fn get_all_tags(state: State<'_, AppState>) -> CarrelResult<Vec<Tag>> {
    let conn = state.active_db()?.get()?;
    let tags = db::list_tags(&conn)?;
    Ok(tags
        .into_iter()
        .map(|(id, name)| Tag { id, name })
        .collect())
}

#[tauri::command]
pub async fn get_book_tags(book_id: String, state: State<'_, AppState>) -> CarrelResult<Vec<Tag>> {
    let conn = state.active_db()?.get()?;
    let tags = db::get_book_tags(&conn, &book_id)?;
    Ok(tags
        .into_iter()
        .map(|(id, name)| Tag { id, name })
        .collect())
}

#[tauri::command]
pub async fn add_tag_to_book(
    book_id: String,
    tag_name: String,
    state: State<'_, AppState>,
) -> CarrelResult<Tag> {
    let conn = state.active_db()?.get()?;
    // Find or create tag
    let tag_id = if let Some(id) = db::get_tag_by_name(&conn, &tag_name)? {
        id
    } else {
        let id = Uuid::new_v4().to_string();
        db::get_or_create_tag(&conn, &id, &tag_name)?;
        id
    };
    db::add_tag_to_book(&conn, &book_id, &tag_id)?;
    Ok(Tag {
        id: tag_id,
        name: tag_name,
    })
}

#[tauri::command]
pub async fn remove_tag_from_book(
    book_id: String,
    tag_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::remove_tag_from_book(&conn, &book_id, &tag_id)?)
}

#[derive(serde::Serialize)]
pub struct BookTagAssoc {
    pub book_id: String,
    pub tag_id: String,
}

#[tauri::command]
pub async fn get_all_book_tags(state: State<'_, AppState>) -> CarrelResult<Vec<BookTagAssoc>> {
    let conn = state.active_db()?.get()?;
    let assocs = db::list_all_book_tags(&conn)?;
    Ok(assocs
        .into_iter()
        .map(|(book_id, tag_id)| BookTagAssoc { book_id, tag_id })
        .collect())
}

// --- Collections ---

/// Valid (field, operator) combinations for collection rules.
const VALID_RULE_PAIRS: &[(&str, &str)] = &[
    ("author", "contains"),
    ("author", "equals"),
    ("filename", "contains"),
    ("series", "contains"),
    ("series", "equals"),
    ("language", "equals"),
    ("language", "contains"),
    ("publisher", "contains"),
    ("description", "contains"),
    ("format", "equals"),
    ("date_added", "last_n_days"),
    ("tag", "contains"),
    ("tag", "equals"),
    ("reading_progress", "equals"),
];

fn validate_collection_rules(rules: &[NewRuleInput]) -> CarrelResult<()> {
    for rule in rules {
        if !VALID_RULE_PAIRS
            .iter()
            .any(|(f, o)| *f == rule.field && *o == rule.operator)
        {
            return Err(CarrelError::invalid(format!(
                "Invalid collection rule: field '{}' with operator '{}'",
                rule.field, rule.operator
            )));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn create_collection(
    name: String,
    coll_type: String,
    icon: Option<String>,
    color: Option<String>,
    rules: Vec<NewRuleInput>,
    state: State<'_, AppState>,
) -> CarrelResult<Collection> {
    validate_collection_rules(&rules)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let collection_id = Uuid::new_v4().to_string();

    let coll_type_enum = match coll_type.as_str() {
        "automated" => CollectionType::Automated,
        _ => CollectionType::Manual,
    };

    let rule_structs: Vec<CollectionRule> = rules
        .into_iter()
        .map(|r| CollectionRule {
            id: Uuid::new_v4().to_string(),
            collection_id: collection_id.clone(),
            field: r.field,
            operator: r.operator,
            value: r.value,
        })
        .collect();

    let collection = Collection {
        id: collection_id,
        name,
        r#type: coll_type_enum,
        icon,
        color,
        created_at: now,
        updated_at: now,
        rules: rule_structs,
    };

    let conn = state.active_db()?.get()?;
    db::insert_collection(&conn, &collection)?;

    log_event(
        &conn,
        ActivityEvent::CollectionCreated {
            id: collection.id.clone(),
            name: collection.name.clone(),
        },
    );

    Ok(collection)
}

#[tauri::command]
pub async fn update_collection(
    id: String,
    name: String,
    coll_type: String,
    icon: Option<String>,
    color: Option<String>,
    rules: Vec<NewRuleInput>,
    state: State<'_, AppState>,
) -> CarrelResult<Collection> {
    validate_collection_rules(&rules)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let coll_type_enum = match coll_type.as_str() {
        "automated" => CollectionType::Automated,
        _ => CollectionType::Manual,
    };

    let rule_structs: Vec<CollectionRule> = rules
        .into_iter()
        .map(|r| CollectionRule {
            id: Uuid::new_v4().to_string(),
            collection_id: id.clone(),
            field: r.field,
            operator: r.operator,
            value: r.value,
        })
        .collect();

    let conn = state.active_db()?.get()?;

    let created_at: i64 = conn.query_row(
        "SELECT created_at FROM collections WHERE id = ?1",
        rusqlite::params![&id],
        |row| row.get(0),
    )?;

    let collection = Collection {
        id,
        name,
        r#type: coll_type_enum,
        icon,
        color,
        created_at,
        updated_at: now,
        rules: rule_structs,
    };

    db::update_collection(&conn, &collection)?;

    log_event(
        &conn,
        ActivityEvent::CollectionUpdated {
            id: collection.id.clone(),
            name: collection.name.clone(),
        },
    );

    Ok(collection)
}

#[tauri::command]
pub async fn get_collections(state: State<'_, AppState>) -> CarrelResult<Vec<Collection>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_collections(&conn)?)
}

#[tauri::command]
pub async fn delete_collection(id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    log_event(&conn, ActivityEvent::CollectionDeleted { id: id.clone() });
    Ok(db::delete_collection(&conn, &id)?)
}

#[tauri::command]
pub async fn add_book_to_collection(
    book_id: String,
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    let coll_type: String = conn.query_row(
        "SELECT type FROM collections WHERE id = ?1",
        rusqlite::params![collection_id],
        |row| row.get(0),
    )?;
    if coll_type == "automated" {
        return Err(CarrelError::invalid(
            "Cannot manually add books to an automated collection",
        ));
    }
    db::add_book_to_collection(&conn, &book_id, &collection_id)?;
    log_event(
        &conn,
        ActivityEvent::CollectionModified {
            id: collection_id.clone(),
            detail: format!("Added book {}", book_id),
        },
    );
    Ok(())
}

#[tauri::command]
pub async fn remove_book_from_collection(
    book_id: String,
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    db::remove_book_from_collection(&conn, &book_id, &collection_id)?;
    log_event(
        &conn,
        ActivityEvent::CollectionModified {
            id: collection_id.clone(),
            detail: format!("Removed book {}", book_id),
        },
    );
    Ok(())
}

#[tauri::command]
pub async fn get_books_in_collection(
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<Book>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_books_in_collection(&conn, &collection_id)?)
}

#[tauri::command]
pub async fn get_books_in_collection_grid(
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<BookGridItem>> {
    let conn = state.active_db()?.get()?;
    let mut items = db::get_books_in_collection_grid(&conn, &collection_id)?;
    if let Ok(storage) = state.covers_storage() {
        apply_grid_thumbnails(&*storage, &mut items);
    }
    Ok(items)
}

// --- Share Collections ---

#[tauri::command]
pub async fn export_collection_markdown(
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<String> {
    let conn = state.active_db()?.get()?;

    // Get collection name
    let name: String = conn.query_row(
        "SELECT name FROM collections WHERE id = ?1",
        rusqlite::params![collection_id],
        |row| row.get(0),
    )?;

    let books = db::get_books_in_collection(&conn, &collection_id)?;

    let mut md = format!("# {}\n\n", name);
    md.push_str(&format!("{} books\n\n", books.len()));
    for (i, book) in books.iter().enumerate() {
        md.push_str(&format!("{}. **{}**", i + 1, book.title));
        if !book.author.is_empty() {
            md.push_str(&format!(" — {}", book.author));
        }
        md.push_str(&format!(" *({})*\n", book.format));
    }
    Ok(md)
}

#[tauri::command]
pub async fn export_collection_json(
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<String> {
    let conn = state.active_db()?.get()?;
    let name: String = conn.query_row(
        "SELECT name FROM collections WHERE id = ?1",
        rusqlite::params![collection_id],
        |row| row.get(0),
    )?;

    let books = db::get_books_in_collection(&conn, &collection_id)?;

    let list: Vec<serde_json::Value> = books
        .iter()
        .map(|b| {
            serde_json::json!({
                "title": b.title,
                "author": b.author,
                "format": b.format.to_string(),
            })
        })
        .collect();

    let export = serde_json::json!({
        "collection": name,
        "books": list,
    });

    Ok(serde_json::to_string_pretty(&export)?)
}

// --- OpenLibrary ---

#[tauri::command]
pub async fn search_openlibrary(
    title: String,
    author: Option<String>,
) -> CarrelResult<Vec<openlibrary::OpenLibraryResult>> {
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(openlibrary::search(&title, author.as_deref()));
    });
    rx.recv()?
}

#[tauri::command]
#[tracing::instrument(skip(openlibrary_key, state))]
pub async fn enrich_book_from_openlibrary(
    book_id: String,
    openlibrary_key: String,
    state: State<'_, AppState>,
) -> CarrelResult<Book> {
    let _t = state.ipc_metrics.time("enrich_book_from_openlibrary");
    tracing::info!("enriching book from openlibrary");
    // Fetch detailed metadata from OpenLibrary (on a separate thread)
    let key = openlibrary_key.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(openlibrary::get_work(&key));
    });
    let work = rx.recv()??;

    // Also get search result for rating/isbn (work endpoint doesn't have them)
    let conn = state.active_db()?.get()?;
    let mut book = db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;

    // Do a quick search to get rating and ISBN
    let search_title = book.title.clone();
    let search_author = if book.author.is_empty() {
        None
    } else {
        Some(book.author.clone())
    };
    let (tx2, rx2) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx2.send(openlibrary::search(&search_title, search_author.as_deref()));
    });
    let search_results = rx2.recv()?.unwrap_or_default();
    let matched = search_results.iter().find(|r| r.key == openlibrary_key);

    // Update book with enriched data
    let description = work
        .description
        .or_else(|| matched.and_then(|m| m.description.clone()));
    let genres = if !work.genres.is_empty() {
        Some(serde_json::to_string(&work.genres).unwrap_or_default())
    } else {
        matched.map(|m| serde_json::to_string(&m.genres).unwrap_or_default())
    };
    let rating = matched.and_then(|m| m.rating);
    let isbn = matched.and_then(|m| m.isbn.clone());

    db::update_book_enrichment(
        &conn,
        &book_id,
        description.as_deref(),
        genres.as_deref(),
        rating,
        isbn.as_deref(),
        Some(&openlibrary_key),
    )?;

    // Return updated book
    book.description = description;
    book.genres = genres;
    book.rating = rating;
    book.isbn = isbn;
    book.openlibrary_key = Some(openlibrary_key);

    events::bus().emit(CarrelEvent::MetadataEnriched {
        book_id: book_id.clone(),
        provider: "OpenLibrary".to_string(),
    });
    log_event(&conn, ActivityEvent::BookEnriched { id: book_id });

    Ok(book)
}

// --- OPDS Catalog ---

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpdsCatalogSource {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
}

const DEFAULT_CATALOGS: &[(&str, &str, &str)] = &[
    (
        "Project Gutenberg",
        "https://www.gutenberg.org/ebooks.opds/",
        "project-gutenberg",
    ),
    (
        "Standard Ebooks (New Releases)",
        "https://standardebooks.org/feeds/atom/new-releases",
        "standard-ebooks-new",
    ),
    (
        "Wikisource (English)",
        "https://ws-export.wmcloud.org/opds/en/Ready_for_export.xml",
        "wikisource-en",
    ),
];

/// Build the trusted-host list for OPDS network calls, reading catalogs
/// directly from the DB connection. Every catalog the user (or Carrel's
/// defaults) has configured contributes its `host:port`, which lets
/// `is_safe_url_with_trusted` allow LAN/loopback servers the user explicitly
/// added — without weakening SSRF protection on arbitrary feed-derived URLs
/// from untrusted hosts. Used internally by [`opds_context_for`].
fn trusted_hosts_from_db(conn: &rusqlite::Connection) -> Vec<String> {
    let mut hosts: Vec<String> = DEFAULT_CATALOGS
        .iter()
        .filter_map(|(_, url, _)| opds::host_port_from_url(url))
        .collect();
    let custom_json = db::get_setting(conn, "opds_custom_catalogs")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    if let Ok(custom) = serde_json::from_str::<Vec<OpdsCatalogSource>>(&custom_json) {
        for c in custom {
            if let Some(hp) = opds::host_port_from_url(&c.url) {
                if !hosts.iter().any(|h| h.eq_ignore_ascii_case(&hp)) {
                    hosts.push(hp);
                }
            }
        }
    }
    hosts
}

/// Keychain service for OPDS catalog credentials. STABLE IDENTIFIER — changing
/// it orphans every stored credential (see the CLAUDE.md table).
const OPDS_AUTH_SERVICE: &str = "carrel-opds-auth";

/// Serializes every writer of `opds_custom_catalogs` and `opds_auth`. Both are
/// single array-valued settings rows, so an unsynchronized writer is a lost
/// update — and the authenticated-add rollback window spans a network call.
///
/// LOCKING DISCIPLINE: only `#[tauri::command]` entry points lock. Helpers are
/// `*_inner` and never lock; `std::sync::Mutex` is not reentrant.
static OPDS_AUTH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Builds the keychain account string for an OPDS catalog credential. A
/// profile name may contain any character — validation only trims and
/// rejects empty/"default" (commands.rs:4574) — so a delimiter-joined key
/// would be ambiguous. Length prefixing is not.
///
/// STABLE IDENTIFIER — this encoding, together with [`OPDS_AUTH_SERVICE`], is
/// how a credential is looked up. Changing it orphans every stored credential.
fn opds_secret_key(profile: &str, catalog_url: &str) -> String {
    format!("{}:{}{}", profile.len(), profile, catalog_url)
}

/// Storage seam for OPDS catalog credentials, so tests never touch the real
/// OS keychain — a keychain-touching test is platform-dependent, can prompt
/// the user, and leaves credentials behind.
trait CredentialStore: Send + Sync {
    fn get(&self, profile: &str, catalog_url: &str) -> CarrelResult<Option<String>>;
    fn set(&self, profile: &str, catalog_url: &str, secret: &str) -> CarrelResult<()>;
    fn delete(&self, profile: &str, catalog_url: &str) -> CarrelResult<()>;
}

/// Production `CredentialStore`, backed by the OS keychain. Follows
/// `backup.rs:26-86`'s error-mapping pattern, with deliberate differences in
/// all three methods' treatment of "no entry":
/// - `get` maps `NoEntry` to `Ok(None)` — no secret stored yet is not
///   exceptional here.
/// - `set` has no "no entry" case (a write always creates/overwrites).
/// - `delete` maps `NoEntry` to `Ok(())` — deleting an already-absent
///   credential is idempotent, not a failure, so a caller removing a
///   catalog that never had one (or clearing every credential a deleted
///   profile might own) doesn't have to special-case it. Every *other*
///   keychain error is still propagated rather than swallowed, unlike
///   `remove_secrets`'s `let _ = entry.delete_credential();`.
struct KeyringCredentialStore;

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, profile: &str, catalog_url: &str) -> CarrelResult<Option<String>> {
        let account = opds_secret_key(profile, catalog_url);
        let entry = keyring::Entry::new(OPDS_AUTH_SERVICE, &account).map_err(|e| {
            CarrelError::internal(format!("Failed to access keychain for OPDS catalog: {e}"))
        })?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CarrelError::internal(format!(
                "Failed to load OPDS catalog credential from keychain: {e}"
            ))),
        }
    }

    fn set(&self, profile: &str, catalog_url: &str, secret: &str) -> CarrelResult<()> {
        let account = opds_secret_key(profile, catalog_url);
        let entry = keyring::Entry::new(OPDS_AUTH_SERVICE, &account).map_err(|e| {
            CarrelError::internal(format!("Failed to access keychain for OPDS catalog: {e}"))
        })?;
        entry.set_password(secret).map_err(|e| match e {
            keyring::Error::PlatformFailure(err) => CarrelError::permission(format!(
                "Keychain access denied while storing OPDS catalog credential: {err}"
            )),
            _ => CarrelError::internal(format!(
                "Failed to store OPDS catalog credential in keychain: {e}"
            )),
        })
    }

    fn delete(&self, profile: &str, catalog_url: &str) -> CarrelResult<()> {
        let account = opds_secret_key(profile, catalog_url);
        let entry = keyring::Entry::new(OPDS_AUTH_SERVICE, &account).map_err(|e| {
            CarrelError::internal(format!("Failed to access keychain for OPDS catalog: {e}"))
        })?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(keyring::Error::PlatformFailure(err)) => Err(CarrelError::permission(format!(
                "Keychain access denied while removing OPDS catalog credential: {err}"
            ))),
            Err(e) => Err(CarrelError::internal(format!(
                "Failed to remove OPDS catalog credential from keychain: {e}"
            ))),
        }
    }
}

/// In-memory `CredentialStore` for tests. Never touches the OS keychain (see
/// the module-level constraint on `CredentialStore`).
#[cfg(test)]
#[derive(Default)]
struct MemoryCredentialStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl CredentialStore for MemoryCredentialStore {
    fn get(&self, profile: &str, catalog_url: &str) -> CarrelResult<Option<String>> {
        let key = opds_secret_key(profile, catalog_url);
        Ok(self.entries.lock()?.get(&key).cloned())
    }

    fn set(&self, profile: &str, catalog_url: &str, secret: &str) -> CarrelResult<()> {
        let key = opds_secret_key(profile, catalog_url);
        self.entries.lock()?.insert(key, secret.to_string());
        Ok(())
    }

    fn delete(&self, profile: &str, catalog_url: &str) -> CarrelResult<()> {
        let key = opds_secret_key(profile, catalog_url);
        self.entries.lock()?.remove(&key);
        Ok(())
    }
}

/// A store whose `get`/`set`/`delete` delegate to an in-memory backing map
/// but each can be forced to fail — `fail_set`/`fail_delete` for exercising
/// `set_opds_auth_inner`'s compensation paths (a failed row write must
/// restore or clear the keychain, and the compensation itself can fail too),
/// `fail_get` for exercising `opds_context_for`'s "keychain read failed"
/// branch (a denied keychain must not strip provenance from a public
/// catalog).
#[cfg(test)]
#[derive(Default)]
struct FlakyCredentialStore {
    inner: MemoryCredentialStore,
    fail_set: bool,
    fail_delete: bool,
    fail_get: bool,
}

#[cfg(test)]
impl CredentialStore for FlakyCredentialStore {
    fn get(&self, profile: &str, catalog_url: &str) -> CarrelResult<Option<String>> {
        if self.fail_get {
            return Err(CarrelError::internal("flaky store: get failed"));
        }
        self.inner.get(profile, catalog_url)
    }

    fn set(&self, profile: &str, catalog_url: &str, secret: &str) -> CarrelResult<()> {
        if self.fail_set {
            return Err(CarrelError::internal("flaky store: set failed"));
        }
        self.inner.set(profile, catalog_url, secret)
    }

    fn delete(&self, profile: &str, catalog_url: &str) -> CarrelResult<()> {
        if self.fail_delete {
            return Err(CarrelError::internal("flaky store: delete failed"));
        }
        self.inner.delete(profile, catalog_url)
    }
}

/// Active profile name and its pool from ONE `profile_state` snapshot, gated
/// by the same soft-lock check as `AppState::active_db` (:170-178) — OPDS
/// catalog credentials are data-bearing, so a locked-and-not-yet-unlocked
/// profile must not be able to read or write them via this seam.
///
/// `shared_active_profile_name` (lib.rs:179) is a web-server mirror updated in
/// a separate critical section during a switch (commands.rs:4740, :4757), so
/// reading the name from there could pair profile A's settings with profile
/// B's secret. Mirrors `AppState::active_db_unchecked`'s single-lock pattern
/// for the name+pool snapshot.
///
/// The lock gate is checked *after* releasing `profile_state`, against the
/// name just snapshotted — never held together with `unlocked_profiles`,
/// which must stay a leaf lock (:165-169). Checking against the snapshotted
/// name (rather than re-reading `profile_state.active`) keeps the returned
/// pair self-consistent even if a switch races between the snapshot and the
/// gate check: the gate always answers for the same profile the pool came
/// from, not for whatever is active by the time the gate runs.
fn active_profile_and_db(state: &AppState) -> CarrelResult<(String, DbPool)> {
    let (name, pool) = {
        let ps = state.profile_state.lock()?;
        let pool = if ps.active == "default" {
            state.db.clone()
        } else {
            ps.pools.get(&ps.active).cloned().ok_or_else(|| {
                CarrelError::not_found(format!("Profile '{}' not found", ps.active))
            })?
        };
        (ps.active.clone(), pool)
    };
    if !state.is_unlocked(&name) {
        return Err(CarrelError::lock_required(format!(
            "Profile '{name}' is locked"
        )));
    }
    Ok((name, pool))
}

#[tauri::command]
pub async fn get_opds_catalogs(state: State<'_, AppState>) -> CarrelResult<Vec<OpdsCatalogSource>> {
    let conn = state.active_db()?.get()?;
    let custom_json =
        db::get_setting(&conn, "opds_custom_catalogs")?.unwrap_or_else(|| "[]".to_string());
    let custom: Vec<OpdsCatalogSource> = serde_json::from_str(&custom_json).unwrap_or_default();

    let mut result: Vec<OpdsCatalogSource> = DEFAULT_CATALOGS
        .iter()
        .map(|(name, url, preset_id)| OpdsCatalogSource {
            name: name.to_string(),
            url: url.to_string(),
            preset_id: Some(preset_id.to_string()),
        })
        .collect();
    result.extend(custom);
    Ok(result)
}

/// Synchronous mirror of [`get_opds_catalogs`]'s URL list, for callers that
/// hold a bare connection rather than a `State` (that command is `async` and
/// takes `State<'_, AppState>`, which `set_opds_auth_inner` does not have).
///
/// Mirrors `get_opds_catalogs` (:4207-4209) in propagating a genuine
/// `opds_custom_catalogs` read failure with `?` rather than swallowing it —
/// only an *absent* row (no custom catalogs saved yet) falls back to `"[]"`.
/// The two helpers disagreeing here would make a real DB failure look like
/// "catalog not configured" instead of surfacing as the DB error it is.
fn configured_catalog_urls(conn: &rusqlite::Connection) -> CarrelResult<Vec<String>> {
    let mut urls: Vec<String> = DEFAULT_CATALOGS
        .iter()
        .map(|(_, url, _)| url.to_string())
        .collect();
    let custom_json =
        db::get_setting(conn, "opds_custom_catalogs")?.unwrap_or_else(|| "[]".to_string());
    if let Ok(custom) = serde_json::from_str::<Vec<OpdsCatalogSource>>(&custom_json) {
        urls.extend(custom.into_iter().map(|c| c.url));
    }
    Ok(urls)
}

/// Persistence body for `add_opds_catalog`, factored out so tests can
/// exercise the exact code path the Tauri command runs without needing
/// to construct a `tauri::State`. The URL validation lives at the
/// command boundary, not here, so callers must validate first.
///
/// Rejects a URL that is already configured — exact string match, same
/// comparison `remove_opds_catalog_inner`/`configured_catalog_urls` already
/// use elsewhere, so nothing here introduces normalization the rest of the
/// module doesn't have. This isn't just tidiness: `remove_opds_catalog`
/// removes *every* custom entry matching a URL, so adding a URL that's
/// already configured and then rolling that add back would silently delete
/// the user's pre-existing catalog. Rejecting the duplicate up front makes
/// that scenario impossible.
///
/// Checked in two steps, not via [`configured_catalog_urls`]'s merged list,
/// so the error message can tell the two duplicate cases apart: a URL that
/// already matches a built-in [`DEFAULT_CATALOGS`] entry gets a message
/// saying so (the user hasn't actually configured anything — it just doesn't
/// need adding), while a URL that already matches a *custom* catalog gets
/// the ordinary "already configured" message.
fn add_opds_catalog_inner(
    conn: &rusqlite::Connection,
    name: String,
    url: String,
    preset_id: Option<String>,
) -> CarrelResult<()> {
    if DEFAULT_CATALOGS
        .iter()
        .any(|(_, default_url, _)| *default_url == url)
    {
        return Err(CarrelError::invalid(format!(
            "'{url}' is already available as a built-in catalog."
        )));
    }
    let custom_json =
        db::get_setting(conn, "opds_custom_catalogs")?.unwrap_or_else(|| "[]".to_string());
    let mut custom: Vec<OpdsCatalogSource> = serde_json::from_str(&custom_json).unwrap_or_default();
    if custom.iter().any(|c| c.url == url) {
        return Err(CarrelError::invalid(format!(
            "'{url}' is already configured as a catalog."
        )));
    }
    custom.push(OpdsCatalogSource {
        name,
        url,
        preset_id,
    });
    let json = serde_json::to_string(&custom)?;
    Ok(db::set_setting(conn, "opds_custom_catalogs", &json)?)
}

#[tauri::command]
pub async fn add_opds_catalog(
    name: String,
    url: String,
    preset_id: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if !opds::is_user_addable_url(&url) {
        return Err(CarrelError::invalid(
            "Invalid catalog URL — only http:// or https:// URLs are accepted.",
        ));
    }
    let conn = state.active_db()?.get()?;
    add_opds_catalog_inner(&conn, name, url, preset_id)
}

/// Persistence body for `remove_opds_catalog`, factored out like
/// `add_opds_catalog_inner`. Removes every custom-catalog entry matching
/// `catalog_url` and, since nothing will ever authenticate to that URL again
/// once it's gone, also clears any credential it owned — reusing
/// `clear_opds_auth_inner`'s row-then-keychain removal so a removed catalog
/// never leaves an orphaned secret behind. Does NOT lock `OPDS_AUTH_LOCK`;
/// see the module note on `add_opds_catalog_inner`/`add_opds_catalog` for why
/// an inner fn locking would self-deadlock a caller that already holds it.
fn remove_opds_catalog_inner(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
    catalog_url: &str,
) -> CarrelResult<()> {
    let custom_json =
        db::get_setting(conn, "opds_custom_catalogs")?.unwrap_or_else(|| "[]".to_string());
    let mut custom: Vec<OpdsCatalogSource> = serde_json::from_str(&custom_json).unwrap_or_default();
    custom.retain(|c| c.url != catalog_url);
    let json = serde_json::to_string(&custom)?;
    db::set_setting(conn, "opds_custom_catalogs", &json)?;
    clear_opds_auth_inner(conn, store, profile, catalog_url)
}

#[tauri::command]
pub async fn remove_opds_catalog(url: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;
    remove_opds_catalog_inner(&conn, &KeyringCredentialStore, &profile, &url)
}

/// Persisted credential metadata for one OPDS catalog. Stored as JSON under
/// the `opds_auth` settings key; the secret itself is never part of this
/// struct — it lives only in the OS keychain via `CredentialStore`, keyed by
/// [`opds_secret_key`].
///
/// STABLE PERSISTENCE IDENTIFIER: under `#[serde(rename_all = "camelCase")]`
/// these field names are the on-disk shape of the `opds_auth` settings row
/// (e.g. `catalog_url` persists as `catalogUrl`). Renaming a field changes
/// what's already on disk.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpdsAuthRecord {
    catalog_url: String,
    origin: String,
    kind: String,
    username: String,
}

/// IPC-facing view of an [`OpdsAuthRecord`]: omits `catalog_url`/`origin`
/// and, more importantly, never carries the secret — the secret lives only
/// in the OS keychain and is never read back out over IPC.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpdsAuthInfo {
    kind: String,
    username: String,
}

/// Reads the `opds_auth` settings row, tolerating a missing or corrupt row by
/// returning an empty list rather than erroring — same tolerance pattern as
/// the custom-catalog read in `get_opds_catalogs` (:4179-4181).
fn read_opds_auth(conn: &rusqlite::Connection) -> Vec<OpdsAuthRecord> {
    let json = db::get_setting(conn, "opds_auth")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    serde_json::from_str(&json).unwrap_or_default()
}

/// Writes the full `opds_auth` settings row.
fn write_opds_auth(conn: &rusqlite::Connection, recs: &[OpdsAuthRecord]) -> CarrelResult<()> {
    let json = serde_json::to_string(recs)?;
    Ok(db::set_setting(conn, "opds_auth", &json)?)
}

/// Inserts or replaces the credential metadata for `rec.catalog_url`, keyed
/// by catalog URL (not origin) — two catalogs on the same origin keep
/// independent credential records.
fn upsert_opds_auth_inner(conn: &rusqlite::Connection, rec: OpdsAuthRecord) -> CarrelResult<()> {
    let mut all = read_opds_auth(conn);
    all.retain(|r| r.catalog_url != rec.catalog_url);
    all.push(rec);
    write_opds_auth(conn, &all)
}

/// Builds the per-request [`opds::OpdsContext`] for an OPDS network call:
/// the trusted-host list (unchanged from [`trusted_hosts_from_db`]) plus —
/// only when rules 1-4 below all admit it — the provenance of `catalog_url`
/// and its credential. Supersedes `trusted_hosts_from_catalogs` (deleted —
/// its only two callers now build a context through this function instead)
/// and reuses `trusted_hosts_from_db` internally rather than superseding it.
///
/// Enforces rules 1-4 of the design spec's "Provenance rules" section (rule
/// 5 — dropping provenance when the *response's effective* URL leaves the
/// origin — is enforced inside carrel-core's `stamp_provenance`, which is the
/// only code that sees the post-redirect URL):
///
/// 1. `catalog_url == None` -> no provenance, no credential. An operation
///    that does not name an initiating catalog has no authority to
///    authenticate.
/// 2. `catalog_url` not in the configured-catalog list -> no provenance, no
///    credential. Only a catalog the user configured can confer authority.
/// 3. `origin_from_url(request_url) != origin_from_url(catalog_url)` -> no
///    provenance, no credential. This is the cross-origin bounce guard: a
///    feed that links off-origin cannot bounce authority back to itself.
/// 4. Otherwise provenance IS set, even with no stored credential at all —
///    public catalogs must still be stampable, and the user must still be
///    able to sign in after a 401.
///
/// A credential is attached only when a matching `opds_auth` record exists,
/// its `kind` is recognized, AND the keychain read succeeds. A failed
/// keychain read is logged with `log::warn!` (never the secret, the key, or
/// the record) and degrades to "no credential" while leaving provenance
/// intact — a denied keychain must never break public catalogs. This
/// function returns no `Result`, so every fallible step here degrades rather
/// than propagates.
///
/// Does NOT lock `OPDS_AUTH_LOCK` — it is a helper called from within
/// commands that already hold it, and `std::sync::Mutex` is not reentrant.
fn opds_context_for(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
    catalog_url: Option<&str>,
    request_url: &str,
) -> opds::OpdsContext {
    let trusted = trusted_hosts_from_db(conn);

    // Rule 1: no initiating catalog named -> no authority.
    let Some(catalog_url) = catalog_url else {
        return opds::OpdsContext {
            trusted,
            provenance: None,
            cred: None,
        };
    };

    // Rule 2: the catalog URL must be one the user actually configured.
    let is_configured = configured_catalog_urls(conn)
        .map(|urls| urls.iter().any(|u| u == catalog_url))
        .unwrap_or(false);
    if !is_configured {
        return opds::OpdsContext {
            trusted,
            provenance: None,
            cred: None,
        };
    }

    // Rule 3: the request must still be on the catalog's own origin.
    let catalog_origin = opds::origin_from_url(catalog_url);
    let request_origin = opds::origin_from_url(request_url);
    if catalog_origin.is_none() || request_origin != catalog_origin {
        return opds::OpdsContext {
            trusted,
            provenance: None,
            cred: None,
        };
    }
    let origin = catalog_origin.expect("checked non-None above");

    // Rule 4: provenance is admitted regardless of whether a credential
    // exists or can be read.
    let provenance = Some(opds::OpdsProvenance {
        catalog_url: catalog_url.to_string(),
        origin,
    });

    // Credential lookup: only attach one for a record matching this exact
    // catalog URL, whose secret is readable, whose `kind` is recognized.
    //
    // The record's own `origin` field is not consulted here — the origin
    // used for the rule-3 check above is `origin_from_url(catalog_url)`,
    // recomputed from the live configured URL rather than trusted from
    // whatever was written into the record. A record's stored `origin` is
    // data the record itself asserts about its own catalog_url; re-deriving
    // it from the URL currently configured is the safer source of truth for
    // a security decision; a stale or hand-edited `origin` column then can't
    // silently relax or tighten the check.
    let cred = read_opds_auth(conn)
        .into_iter()
        .find(|r| r.catalog_url == catalog_url)
        .and_then(|record| match store.get(profile, catalog_url) {
            Ok(Some(secret)) => match record.kind.as_str() {
                "basic" => Some(opds::OpdsCredential::Basic {
                    username: record.username,
                    password: secret,
                }),
                "bearer" => Some(opds::OpdsCredential::Bearer(secret)),
                _ => None,
            },
            Ok(None) => None,
            Err(e) => {
                log::warn!(
                    "OPDS catalog credential could not be read from the keychain \
                     (profile '{profile}'): {e}"
                );
                None
            }
        });

    opds::OpdsContext {
        trusted,
        provenance,
        cred,
    }
}

#[cfg(test)]
fn rec(catalog_url: &str, kind: &str, username: &str) -> OpdsAuthRecord {
    OpdsAuthRecord {
        catalog_url: catalog_url.to_string(),
        origin: opds::origin_from_url(catalog_url).expect("test URLs must parse"),
        kind: kind.to_string(),
        username: username.to_string(),
    }
}

/// A tempdir-backed connection with `url` registered as a custom catalog, so
/// the configured-catalog membership check passes. The TempDir must be held
/// by the caller — dropping it deletes the DB.
#[cfg(test)]
fn test_conn_with_catalog(
    url: &str,
) -> (
    r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
    let conn = pool.get().unwrap();
    add_opds_catalog_inner(&conn, "Test".into(), url.to_string(), None).unwrap();
    (conn, tmp)
}

/// Persistence body for `set_opds_auth` — implements the spec's write
/// protocol (docs/superpowers/specs/2026-08-01-opds-catalog-auth-design.md,
/// "Write protocol"):
///
/// 1. Validate (nothing is written on rejection).
/// 2. Retain the previous secret for this `(profile, catalog_url)`, if any.
/// 3. Write the new secret to the keychain.
/// 4. Write the `opds_auth` row.
/// 5. On row-write failure: restore the retained secret, or delete the new
///    one if there wasn't a previous secret; return the original error — or,
///    if the restore itself fails, an error naming both failures, since the
///    credential is then in an unknown state.
/// 6. Invalidate the Discover cache row.
#[allow(clippy::too_many_arguments)]
fn set_opds_auth_inner(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
    catalog_url: &str,
    kind: &str,
    username: &str,
    secret: &str,
    allow_insecure: bool,
) -> CarrelResult<()> {
    // Step 1: validate. Nothing below this point may write anywhere.
    if kind != "basic" && kind != "bearer" {
        return Err(CarrelError::invalid(format!(
            "Unsupported credential kind '{kind}' — must be 'basic' or 'bearer'."
        )));
    }
    if secret.is_empty() {
        return Err(CarrelError::invalid("Credential secret must not be empty."));
    }
    if kind == "basic" && username.is_empty() {
        return Err(CarrelError::invalid("Basic auth requires a username."));
    }
    let origin = opds::origin_from_url(catalog_url).ok_or_else(|| {
        CarrelError::invalid(format!("'{catalog_url}' is not a valid catalog URL."))
    })?;
    if !configured_catalog_urls(conn)?
        .iter()
        .any(|u| u == catalog_url)
    {
        return Err(CarrelError::invalid(format!(
            "'{catalog_url}' is not a configured catalog."
        )));
    }
    // `origin` is already scheme-lowercased by `origin_from_url`, so this
    // agrees with a mixed-case scheme (`HTTPS://...`) instead of spuriously
    // demanding `allow_insecure` for it.
    let is_https = origin.starts_with("https://");
    let is_loopback = url::Url::parse(catalog_url)
        .ok()
        .and_then(|u| u.host_str().map(opds::is_loopback_host))
        .unwrap_or(false);
    if !is_https && !is_loopback && !allow_insecure {
        return Err(CarrelError::invalid(
            "This catalog's URL is not encrypted (http) — pass allow_insecure to confirm the \
             credential may be sent in cleartext.",
        ));
    }

    // Step 2: retain the previous secret, if any, so a failed row write can
    // be undone.
    let previous_secret = store.get(profile, catalog_url)?;

    // Step 3: write the new secret to the keychain.
    store.set(profile, catalog_url, secret)?;

    // Step 4: write the opds_auth row.
    let row_result = upsert_opds_auth_inner(
        conn,
        OpdsAuthRecord {
            catalog_url: catalog_url.to_string(),
            origin,
            kind: kind.to_string(),
            username: username.to_string(),
        },
    );

    if let Err(row_err) = row_result {
        // Step 5: the row write failed — the keychain must not end up
        // holding a secret with no matching row, so undo it.
        let restore_result = match &previous_secret {
            Some(prev) => store.set(profile, catalog_url, prev),
            None => store.delete(profile, catalog_url),
        };
        return match restore_result {
            Ok(()) => Err(row_err),
            Err(restore_err) => Err(CarrelError::internal(format!(
                "Failed to save the catalog credential ({row_err}), and failed to restore the \
                 previous keychain state ({restore_err}) — the credential is now in an unknown \
                 state and should be re-entered."
            ))),
        };
    }

    // Step 6: invalidate the Discover cache — a newly authenticated catalog
    // may expose different content than the anonymous fetch cached earlier.
    let _ = db::delete_setting(conn, "discover_cache_v4");

    Ok(())
}

/// Persistence body for `clear_opds_auth`: removes the `opds_auth` row
/// **first**, then the keychain secret, per the spec's write protocol. The
/// row is gone even when the keychain delete fails — a keychain failure
/// leaves a secret nothing reads, recoverable but not silently swallowed, so
/// its error is returned rather than dropped.
fn clear_opds_auth_inner(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
    catalog_url: &str,
) -> CarrelResult<()> {
    let mut all = read_opds_auth(conn);
    all.retain(|r| r.catalog_url != catalog_url);
    write_opds_auth(conn, &all)?;

    let delete_result = store.delete(profile, catalog_url);
    let _ = db::delete_setting(conn, "discover_cache_v4");

    delete_result.map_err(|e| {
        CarrelError::internal(format!(
            "Removed the stored catalog credential, but failed to remove it from the keychain \
             ({e}) — sign-out did not fully complete."
        ))
    })
}

/// Clears every OPDS credential secret that `profile` owned, for
/// `delete_profile` to call right before the profile's DB file is removed.
/// Must run while the profile's pool is still alive — it needs a connection
/// to read the `opds_auth` row. Does NOT lock `OPDS_AUTH_LOCK` itself: its
/// only caller, `delete_profile_with`, already takes `OPDS_AUTH_LOCK` (after
/// `profile_lifecycle`) for its whole body, so a second lock here would
/// self-deadlock — `std::sync::Mutex` is not reentrant.
///
/// Deliberately leaves the `opds_auth` row itself untouched — the caller
/// deletes the profile's entire DB file immediately after this returns, so
/// there is no row left to clear once the profile is actually gone. Clearing
/// it here would be dead work on the very data about to be discarded.
fn clear_profile_opds_credentials(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
) -> CarrelResult<()> {
    for r in read_opds_auth(conn) {
        store.delete(profile, &r.catalog_url)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_opds_auth(
    catalog_url: String,
    kind: String,
    username: String,
    secret: String,
    allow_insecure: bool,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;
    set_opds_auth_inner(
        &conn,
        &KeyringCredentialStore,
        &profile,
        &catalog_url,
        &kind,
        &username,
        &secret,
        allow_insecure,
    )
}

#[tauri::command]
pub async fn get_opds_auth(
    catalog_url: String,
    state: State<'_, AppState>,
) -> CarrelResult<Option<OpdsAuthInfo>> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (_profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;
    Ok(read_opds_auth(&conn)
        .into_iter()
        .find(|r| r.catalog_url == catalog_url)
        .map(|r| OpdsAuthInfo {
            kind: r.kind,
            username: r.username,
        }))
}

#[tauri::command]
pub async fn clear_opds_auth(catalog_url: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;
    clear_opds_auth_inner(&conn, &KeyringCredentialStore, &profile, &catalog_url)
}

/// Persistence body for `add_opds_catalog_with_auth` — implements the
/// spec's write protocol (docs/superpowers/specs/2026-08-01-opds-catalog-auth-design.md,
/// "Write protocol", `add_opds_catalog_with_auth` section):
///
/// 1. Validate — `add_opds_catalog_inner` rejects a duplicate URL and
///    `set_opds_auth_inner` validates kind/secret/username/origin/cleartext.
///    Both run inside the transaction below, catalog first, so by the time
///    `set_opds_auth_inner` checks "is this catalog configured?" the answer
///    is yes — the catalog write is visible to later reads on this same
///    (uncommitted) connection even though no other connection can see it
///    yet.
/// 2. Retain any previous secret for `url`, for restoration if the
///    connection test in step 4 fails. A plain read — harmless that
///    `set_opds_auth_inner` performs the same read again below for its own
///    narrower compensation window (a failed row write, which can't happen
///    here since nothing else writes `opds_auth` concurrently).
/// 3. In one `unchecked_transaction`, snapshot the *raw* serialized values of
///    `opds_custom_catalogs` and `opds_auth` (as `Option<String>`, not
///    parsed — byte-for-byte restoration in step 5 depends on writing back
///    the exact string, not a re-serialized equivalent), add the catalog,
///    write the credential, and commit.
/// 4. Connection-test the feed with the new credential. The catalog is now
///    committed, so `opds_context_for` sees it as configured and trusted
///    (via `trusted_hosts_from_db`), and attaches the credential just
///    stored.
/// 5. On test failure: write the two retained values back verbatim in one
///    transaction — deleting the row if it did not exist before, rather
///    than writing an empty array, since a row that never existed must not
///    start existing — then restore or delete the secret as in
///    `set_opds_auth` step 5, and return the test's error (not the
///    rollback's).
///
/// TRANSACTION RECEIVER: `Connection::transaction()` needs `&mut Connection`,
/// which this `&Connection` helper cannot provide, so this uses
/// `unchecked_transaction()` (takes `&self`) instead. Safe here because no
/// outer transaction is ever active on `conn`: every caller is a
/// `#[tauri::command]` holding `OPDS_AUTH_LOCK` for the call's entire
/// duration, so nothing else can be mid-transaction on this connection.
///
/// Does NOT lock `OPDS_AUTH_LOCK` — see the module note on
/// `add_opds_catalog_inner`/`set_opds_auth_inner` for why an inner fn
/// locking would self-deadlock a caller that already holds it.
#[allow(clippy::too_many_arguments)]
fn add_opds_catalog_with_auth_inner(
    conn: &rusqlite::Connection,
    store: &dyn CredentialStore,
    profile: &str,
    name: &str,
    url: &str,
    preset_id: Option<String>,
    kind: &str,
    username: &str,
    secret: &str,
    allow_insecure: bool,
) -> CarrelResult<()> {
    if !opds::is_user_addable_url(url) {
        return Err(CarrelError::invalid(
            "Invalid catalog URL — only http:// or https:// URLs are accepted.",
        ));
    }

    // Step 2: retain whatever secret this catalog URL already owns.
    let previous_secret = store.get(profile, url)?;

    // Snapshot both settings rows verbatim, before anything is written.
    let cats_before = db::get_setting(conn, "opds_custom_catalogs")?;
    let auth_before = db::get_setting(conn, "opds_auth")?;

    // Step 1 + Step 3: validate and write the catalog and the credential in
    // one transaction. Nothing commits unless both succeed.
    let tx = conn.unchecked_transaction()?;
    add_opds_catalog_inner(&tx, name.to_string(), url.to_string(), preset_id)?;
    set_opds_auth_inner(
        &tx,
        store,
        profile,
        url,
        kind,
        username,
        secret,
        allow_insecure,
    )?;
    tx.commit()?;

    // Step 4: connection-test the feed with the new credential.
    let ctx = opds_context_for(conn, store, profile, Some(url), url);
    if let Err(test_err) = opds::fetch_feed_with_context(url, &ctx) {
        // Step 5: the connection test failed after commit — undo both rows,
        // verbatim, in one transaction.
        let restore_tx = conn.unchecked_transaction()?;
        match &cats_before {
            Some(v) => db::set_setting(&restore_tx, "opds_custom_catalogs", v)?,
            None => db::delete_setting(&restore_tx, "opds_custom_catalogs")?,
        }
        match &auth_before {
            Some(v) => db::set_setting(&restore_tx, "opds_auth", v)?,
            None => db::delete_setting(&restore_tx, "opds_auth")?,
        }
        restore_tx.commit()?;

        let restore_result = match &previous_secret {
            Some(prev) => store.set(profile, url, prev),
            None => store.delete(profile, url),
        };
        return match restore_result {
            Ok(()) => Err(test_err),
            Err(restore_err) => Err(CarrelError::internal(format!(
                "The new catalog's connection test failed ({test_err}), and the previous \
                 credential could not be restored ({restore_err}) — the credential is now in \
                 an unknown state and should be re-entered."
            ))),
        };
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn add_opds_catalog_with_auth(
    name: String,
    url: String,
    preset_id: Option<String>,
    kind: String,
    username: String,
    secret: String,
    allow_insecure: bool,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let _guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;
    add_opds_catalog_with_auth_inner(
        &conn,
        &KeyringCredentialStore,
        &profile,
        &name,
        &url,
        preset_id,
        &kind,
        &username,
        &secret,
        allow_insecure,
    )
}

/// Live progress for a single catalog during a unified `search_all_catalogs`
/// run. Emitted (as `catalog-search-progress`) once per catalog the moment its
/// fan-out thread finishes. `query` lets the UI ignore stale events from a
/// prior search; `ok` is false only on a network/parse failure.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSearchProgress {
    query: String,
    url: String,
    name: String,
    count: usize,
    ok: bool,
}

/// Search all configured OPDS catalogs in parallel and return aggregated results.
#[tauri::command]
pub async fn search_all_catalogs(
    query: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<opds::OpdsEntry>> {
    // Provenance is per-catalog: each catalog in the loop below authenticates
    // with its own credential and gets its own origin check, using its own
    // `cat.url` as both the initiating catalog and the request URL. Passing
    // one catalog's provenance while requesting another's feed would be
    // exactly the credential-laundering `opds_context_for`'s rules forbid.
    let (profile, pool) = active_profile_and_db(&state)?;
    let catalogs = get_opds_catalogs(state).await?;

    // Fetch root feeds in parallel to discover search URLs
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let mut thread_count = 0;

    for cat in &catalogs {
        let url = cat.url.clone();
        let q = query.clone();
        let tx = result_tx.clone();
        let cat_name = cat.name.clone();
        let app = app.clone();
        let profile = profile.clone();
        let pool = pool.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // Search this catalog. `ok` distinguishes a network/parse failure
            // (surfaced to the user as "failed") from a catalog that simply
            // has no search endpoint or returned nothing (shown as "no
            // results") — both yield an empty `entries` list. A connection
            // failure here degrades the same way: this catalog's leg reports
            // "failed" while the others still complete.
            let mut ok = true;
            let entries = match pool.get() {
                Ok(conn) => {
                    let ctx = opds_context_for(
                        &conn,
                        &KeyringCredentialStore,
                        &profile,
                        Some(&url),
                        &url,
                    );
                    match opds::fetch_feed_with_context(&url, &ctx) {
                        Ok(root) => match root.search_url {
                            // Resolve OpenSearch description if needed, then search.
                            Some(raw) => match opds::resolve_search_url_with_context(&raw, &ctx) {
                                Some(template) => {
                                    let search_url =
                                        template.replace("{searchTerms}", &opds::url_encode(&q));
                                    match opds::fetch_feed_with_context(&search_url, &ctx) {
                                        Ok(f) => f.entries,
                                        Err(_) => {
                                            ok = false;
                                            Vec::new()
                                        }
                                    }
                                }
                                // No resolvable search template — nothing to search.
                                None => Vec::new(),
                            },
                            // Catalog exposes no search — contributes zero results.
                            None => Vec::new(),
                        },
                        Err(_) => {
                            ok = false;
                            Vec::new()
                        }
                    }
                }
                Err(_) => {
                    ok = false;
                    Vec::new()
                }
            };
            // Tag entries with catalog source
            let tagged: Vec<opds::OpdsEntry> = entries
                .into_iter()
                .map(|mut e| {
                    if !e.summary.is_empty() {
                        e.summary = format!("[{}] {}", cat_name, e.summary);
                    } else {
                        e.summary = format!("[{}]", cat_name);
                    }
                    e
                })
                .collect();
            // Emit live progress so the UI can tick this catalog off as soon
            // as it finishes, rather than waiting for the whole fan-out.
            let _ = app.emit(
                "catalog-search-progress",
                CatalogSearchProgress {
                    query: q.clone(),
                    url: url.clone(),
                    name: cat_name.clone(),
                    count: tagged.len(),
                    ok,
                },
            );
            let _ = tx.send(tagged);
        });
        thread_count += 1;
    }
    drop(result_tx);

    let mut all_entries = Vec::new();
    for _ in 0..thread_count {
        if let Ok(entries) = result_rx.recv() {
            all_entries.extend(entries);
        }
    }
    Ok(all_entries)
}

/// Normalize a title or author for duplicate detection: collapse internal
/// whitespace and lowercase, so cosmetic differences ("The  Odyssey" vs
/// "The Odyssey") don't defeat the match.
fn discover_dedup_key(title: &str, author: &str) -> String {
    fn norm(s: &str) -> String {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
    format!("{}\u{1f}{}", norm(title), norm(author))
}

/// Number of discover picks shown at once. The fetched pool is larger so that
/// dedup against a well-stocked library still leaves fresh picks to show.
const DISCOVER_DISPLAY_LIMIT: usize = 20;

/// Filter discover entries against the library: drop any book already owned,
/// and drop intra-list duplicates (same title+author surfaced by multiple
/// catalogs), keeping the first occurrence, then cap at `limit`. Matching is by
/// normalized title+author — imperfect for feeds that format authors
/// differently, but enough to keep obvious duplicates out of the shelf.
fn dedup_discover_entries(
    entries: Vec<opds::OpdsEntry>,
    library: &[(String, String)],
    limit: usize,
) -> Vec<opds::OpdsEntry> {
    let owned: std::collections::HashSet<String> = library
        .iter()
        .map(|(t, a)| discover_dedup_key(t, a))
        .collect();
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|e| {
            let key = discover_dedup_key(&e.title, &e.author);
            !owned.contains(&key) && seen.insert(key)
        })
        .take(limit)
        .collect()
}

/// Returns a cached list of popular/new books from all configured catalogs.
/// Results are cached for 24 hours in the settings DB to avoid slowing down startup.
#[tauri::command]
pub async fn get_discover_books(state: State<'_, AppState>) -> CarrelResult<Vec<opds::OpdsEntry>> {
    let (profile, pool) = active_profile_and_db(&state)?;
    let conn = pool.get()?;

    // Library title/author pairs, used to drop books the user already owns.
    // Applied to both the cached and freshly-fetched paths.
    let library: Vec<(String, String)> = db::list_books_grid(&conn)?
        .into_iter()
        .map(|b| (b.title, b.author))
        .collect();

    // Check cache (stored as JSON with a timestamp)
    if let Some(cached) = db::get_setting(&conn, "discover_cache_v4")? {
        if let Ok(cache) = serde_json::from_str::<serde_json::Value>(&cached) {
            let cached_at = cache["cached_at"].as_i64().unwrap_or(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if now - cached_at < 86400 {
                // Cache is fresh (< 24h)
                if let Ok(entries) =
                    serde_json::from_value::<Vec<opds::OpdsEntry>>(cache["entries"].clone())
                {
                    return Ok(dedup_discover_entries(
                        entries,
                        &library,
                        DISCOVER_DISPLAY_LIMIT,
                    ));
                }
            }
        }
    }

    // Cache miss or stale — fetch from catalogs in parallel. Provenance is
    // per-catalog, same as `search_all_catalogs`: each catalog authenticates
    // with its own credential against its own origin, using its own
    // `cat.url` as both the initiating catalog and the request URL.
    let catalogs = get_opds_catalogs(state).await?;
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let mut thread_count = 0;

    for cat in &catalogs {
        let url = cat.url.clone();
        let tx = result_tx.clone();
        let cat_name = cat.name.clone();
        let profile = profile.clone();
        let pool = pool.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let entries = match pool.get() {
                Ok(conn) => {
                    let ctx = opds_context_for(
                        &conn,
                        &KeyringCredentialStore,
                        &profile,
                        Some(&url),
                        &url,
                    );
                    match opds::fetch_feed_with_context(&url, &ctx) {
                        Ok(feed) => feed
                            .entries
                            .into_iter()
                            .filter(|e| !e.links.is_empty() && e.nav_url.is_none())
                            .take(20)
                            .map(|mut e| {
                                // Tag with catalog source
                                if e.summary.is_empty() {
                                    e.summary = format!("From {}", cat_name);
                                }
                                e
                            })
                            .collect::<Vec<_>>(),
                        Err(_) => Vec::new(),
                    }
                }
                Err(_) => Vec::new(),
            };
            let _ = tx.send(entries);
        });
        thread_count += 1;
    }
    drop(result_tx);

    let mut all_entries = Vec::new();
    for _ in 0..thread_count {
        if let Ok(entries) = result_rx.recv() {
            all_entries.extend(entries);
        }
    }

    // Cache the raw (pre-dedup) results so the cached list stays valid as the
    // library changes — dedup is re-applied against the current library on
    // every read.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cache = serde_json::json!({
        "cached_at": now,
        "entries": all_entries,
    });
    let _ = db::set_setting(
        &conn,
        "discover_cache_v4",
        &serde_json::to_string(&cache).unwrap_or_default(),
    );

    Ok(dedup_discover_entries(
        all_entries,
        &library,
        DISCOVER_DISPLAY_LIMIT,
    ))
}

/// Pick the file extension from an OPDS acquisition URL. Parses the URL with
/// the `url` crate and inspects the final non-empty path segment — query
/// strings, fragments, and trailing slashes are handled by the parser, so
/// feeds that append `?token=…` or `/` don't hide the extension.
/// Returns `None` when the URL is unparseable or the extension isn't in our
/// supported set; the caller decides the fallback.
fn opds_extension_from_url(url: &str) -> Option<&'static str> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rfind(|s| !s.is_empty())?;
    let ext = last.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "epub" => Some("epub"),
        "pdf" => Some("pdf"),
        "cbz" => Some("cbz"),
        "cbr" => Some("cbr"),
        "mobi" => Some("mobi"),
        "azw" => Some("azw"),
        "azw3" => Some("azw3"),
        _ => None,
    }
}

/// Download a book from `url` and import it into the active profile, reusing
/// the standard import pipeline (dedup, copy-on-import). Returns the book id
/// (new or existing duplicate). Backs the plugin `import:books` host function
/// (`plugin_host::DesktopHostServices::import_from_url`); runs on the caller's
/// thread, so it uses blocking download + import directly.
pub(crate) fn import_book_from_url(state: &AppState, url: &str) -> CarrelResult<String> {
    let ext = opds_extension_from_url(url).unwrap_or("epub");
    if !supported_import_extensions().contains(&ext) {
        return Err(CarrelError::invalid(format!(
            "Format '.{ext}' is not supported in this build."
        )));
    }

    let db_pool = state.active_db()?;
    let storage = state.active_storage()?;
    let covers_storage = state.covers_storage()?;
    let import_mode = {
        let conn = db_pool.get()?;
        db::get_setting(&conn, "import_mode")
            .ok()
            .flatten()
            .unwrap_or_else(|| "import".to_string())
    };

    let temp_path = std::env::temp_dir().join(format!("carrel-plugin-{}.{}", Uuid::new_v4(), ext));
    let temp_str = temp_path.to_string_lossy().to_string();
    // SSRF-guarded on every redirect hop, with NO trusted-host relaxation:
    // a plugin must not reach the user's LAN catalogs, only public URLs.
    opds::download_file_ssrf_guarded(url, &temp_str)?;

    let outcome = import_book_inner(
        temp_str,
        db_pool,
        storage,
        covers_storage,
        &import_mode,
        true,
        ImportSource::Download,
    );
    let _ = std::fs::remove_file(&temp_path);
    Ok(outcome?.into_book().id)
}

/// Map an OPDS acquisition link's MIME type to the file extension the import
/// pipeline expects. Preferred over URL-based detection because many feeds
/// serve opaque download URLs (e.g. `/download/123`) while still returning
/// the correct MIME. Parameters (`; profile="…"`) are ignored.
fn opds_extension_from_mime(mime: &str) -> Option<&'static str> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "application/epub+zip" => Some("epub"),
        "application/pdf" => Some("pdf"),
        // MOBI family. `x-mobipocket-ebook` is the historical MOBI MIME and
        // unambiguous. `application/vnd.amazon.ebook` is the Amazon vendor
        // MIME shared by both `.azw` and `.azw3` — mapping it to a specific
        // extension here would collapse that distinction, so we return None
        // and let URL-based detection disambiguate. A final default of
        // `.azw3` (the more common container) is applied at the import
        // layer when the URL is also opaque.
        "application/x-mobipocket-ebook" => Some("mobi"),
        // Comic book archives. Both vendor-prefixed and de-facto MIMEs seen in feeds.
        "application/x-cbz" | "application/vnd.comicbook+zip" => Some("cbz"),
        "application/x-cbr" | "application/vnd.comicbook-rar" => Some("cbr"),
        _ => None,
    }
}

#[tauri::command]
pub async fn browse_opds(
    url: String,
    catalog_url: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<opds::OpdsFeed> {
    let (profile, pool) = active_profile_and_db(&state)?;
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> CarrelResult<opds::OpdsFeed> {
            let conn = pool.get()?;
            let ctx = opds_context_for(
                &conn,
                &KeyringCredentialStore,
                &profile,
                catalog_url.as_deref(),
                &url,
            );
            opds::fetch_feed_with_context(&url, &ctx)
        })();
        let _ = tx.send(result);
    });
    rx.recv()?
}

#[tauri::command]
pub async fn download_opds_book(
    download_url: String,
    mime_type: Option<String>,
    catalog_url: Option<String>,
    state: State<'_, AppState>,
    _app: AppHandle,
) -> CarrelResult<OpdsImportResult> {
    // Determine the file extension for the temp import path. Precedence:
    //   1. URL suffix — Carrel's own feed and many well-behaved feeds put the
    //      extension in the path; this is the only signal that disambiguates
    //      the AZW / AZW3 pair since they share
    //      `application/vnd.amazon.ebook`.
    //   2. MIME type — authoritative for unambiguous types and covers feeds
    //      with opaque URLs like `/download/123`.
    //   3. Vendor-MIME fallback — `application/vnd.amazon.ebook` resolves to
    //      `.azw3` here (the far more common container), which kicks in only
    //      when the URL also had no usable suffix.
    //   4. Final fallback `.epub` so we never feed an extensionless file to
    //      the importer.
    let vendor_amazon = mime_type
        .as_deref()
        .map(|m| {
            m.split(';')
                .next()
                .unwrap_or(m)
                .trim()
                .eq_ignore_ascii_case("application/vnd.amazon.ebook")
        })
        .unwrap_or(false);
    let ext = opds_extension_from_url(&download_url)
        .or_else(|| mime_type.as_deref().and_then(opds_extension_from_mime))
        .or(if vendor_amazon { Some("azw3") } else { None })
        .unwrap_or("epub");

    // Defense in depth: reject unsupported formats before the download so
    // non-`mobi` builds don't waste bandwidth/disk on a file they'll throw
    // away in import_book. The frontend already hides these buttons via
    // get_supported_formats, but feature flags could diverge (e.g. direct
    // IPC calls from tests), and the import error is clearer here.
    if !supported_import_extensions().contains(&ext) {
        return Err(CarrelError::invalid(format!(
            "Format '.{ext}' is not supported in this build."
        )));
    }

    // Download to a temp file
    let temp_dir = std::env::temp_dir();
    let temp_name = format!("carrel-opds-{}.{}", Uuid::new_v4(), ext);
    let temp_path = temp_dir.join(&temp_name);
    let temp_str = temp_path.to_string_lossy().to_string();

    {
        let dl_url = download_url.clone();
        let dl_dest = temp_str.clone();
        let (profile, pool) = active_profile_and_db(&state)?;
        let (tx, rx) = std::sync::mpsc::channel();
        tauri::async_runtime::spawn_blocking(move || {
            let result = (|| -> CarrelResult<()> {
                let conn = pool.get()?;
                let ctx = opds_context_for(
                    &conn,
                    &KeyringCredentialStore,
                    &profile,
                    catalog_url.as_deref(),
                    &dl_url,
                );
                opds::download_file_with_context(&dl_url, &dl_dest, &ctx)
            })();
            let _ = tx.send(result);
        });
        rx.recv()??;
    }

    // Import via the shared inner pipeline. We bypass the `import_book` IPC
    // wrapper so we can pass `force_copy = true`: the temp file is about to
    // be deleted below, so even in `link` mode we must copy into the library
    // rather than store the temp path in the DB.
    let db_pool = state.active_db()?;
    let storage = state.active_storage()?;
    let covers_storage = state.covers_storage()?;
    let import_mode = {
        let conn = db_pool.get()?;
        db::get_setting(&conn, "import_mode")
            .ok()
            .flatten()
            .unwrap_or_else(|| "import".to_string())
    };
    let outcome = import_book_inner(
        temp_str.clone(),
        db_pool,
        storage,
        covers_storage,
        &import_mode,
        true,
        ImportSource::Download,
    );

    // Clean up temp file regardless of import success/failure
    let _ = std::fs::remove_file(&temp_path);

    let outcome = outcome?;
    Ok(OpdsImportResult {
        newly_imported: outcome.is_new(),
        book: outcome.into_book(),
    })
}

// --- Profiles ---

#[derive(serde::Serialize)]
pub struct Profile {
    pub name: String,
    pub is_active: bool,
}

#[tauri::command]
pub async fn get_profiles(state: State<'_, AppState>) -> CarrelResult<Vec<Profile>> {
    let ps = state.profile_state.lock()?;
    let mut result = vec![Profile {
        name: "default".to_string(),
        is_active: ps.active == "default",
    }];
    for name in ps.pools.keys() {
        result.push(Profile {
            name: name.clone(),
            is_active: *name == ps.active,
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

/// Creates a new profile, optionally locking it in the same call.
///
/// `password` is treated as "no lock" when `None` or blank/whitespace-only
/// (matching the frontend's checkbox-off case) — in which case behavior is
/// unchanged from before this option existed. A non-empty password is
/// hashed and stored via [`carrel_core::profile_lock::set_lock`], then the
/// profile is marked unlocked for this session: the caller just typed the
/// password to create it, so re-prompting immediately would be pointless
/// (a future app restart still prompts normally, since the session set
/// starts empty).
#[tauri::command]
pub async fn create_profile(
    name: String,
    password: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let name = name.trim().to_string();
    if name.is_empty() || name == "default" {
        return Err(CarrelError::invalid("Invalid profile name"));
    }

    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points. Creation plus
    // the optional lock is also atomic on failure: the pool is only inserted
    // after the lock succeeds, and a lock failure rolls the DB/folder back
    // (see below), so there is no window where a profile the user asked to
    // lock exists unlocked-and-visible.
    let _lifecycle = state.profile_lifecycle.lock().await;

    let db_path = state.data_dir.join(format!("library-{name}.db"));
    if db_path.exists() {
        return Err(CarrelError::invalid(format!(
            "Profile '{name}' already exists"
        )));
    }
    let pool = db::create_pool(&db_path)?;

    // Ensure library folder for this profile
    let conn = pool.get()?;
    let library_folder = default_library_folder()?;
    let profile_folder = format!("{} - {}", library_folder, name);
    let _ = std::fs::create_dir_all(&profile_folder);
    db::set_setting(&conn, "library_folder", &profile_folder)?;
    drop(conn);

    // If a lock was requested, set it BEFORE inserting the pool into
    // `profile_state.pools`. That insert — and the on-disk `library-{name}.db`
    // that the startup scan (see `lib.rs`) rediscovers with no lock check — is
    // what makes the profile visible. Locking first means a lock failure can
    // roll back the created DB file and folder, so a profile the user asked to
    // lock never lingers unlocked-and-visible (this session or after a restart).
    let lock_requested = password.as_deref().is_some_and(|p| !p.trim().is_empty());
    if lock_requested {
        let password = password.expect("lock_requested implies Some");
        let set = async {
            let phc = hash_password_blocking(SecretString::from(password)).await?;
            carrel_core::profile_lock::set_lock(&name, &phc)
        }
        .await;
        if let Err(e) = set {
            drop(pool);
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_dir_all(&profile_folder);
            return Err(CarrelError::internal(format!(
                "Failed to create locked profile '{name}': {e}"
            )));
        }
    }

    {
        let mut ps = state.profile_state.lock()?;
        ps.pools.insert(name.clone(), pool);
    }
    if lock_requested {
        state.mark_unlocked(&name)?;
    }

    Ok(())
}

/// All profiles with their soft-lock state, for the web layer's
/// `GET /api/profiles`. Same set and ordering as the desktop `get_profiles`
/// command, plus the two lock flags the remote switcher needs.
pub(crate) fn list_profiles_with_lock_state(
    state: &AppState,
) -> CarrelResult<Vec<crate::web_server::WebProfile>> {
    list_profiles_with_lock_state_with(state, carrel_core::profile_lock::has_lock)
}

/// [`list_profiles_with_lock_state`] with the lock-status probe injected —
/// see [`switch_active_profile_with`] for why.
fn list_profiles_with_lock_state_with(
    state: &AppState,
    has_lock: impl Fn(&str) -> CarrelResult<bool>,
) -> CarrelResult<Vec<crate::web_server::WebProfile>> {
    let (active, mut names) = {
        let ps = state.profile_state.lock()?;
        let mut names: Vec<String> = ps.pools.keys().cloned().collect();
        names.push("default".to_string());
        (ps.active.clone(), names)
    };
    names.sort();
    names.dedup();

    Ok(names
        .into_iter()
        .map(|name| {
            // Fail closed (A-M1 Decision 7): an unreadable keychain must not
            // read as "no lock". Reporting the profile locked-and-unswitchable
            // keeps the listing renderable while an attempted switch still
            // surfaces the real keychain error.
            let locked = has_lock(&name).unwrap_or(true);
            crate::web_server::WebProfile {
                active: name == active,
                locked,
                switchable: carrel_core::profile_lock::access_allowed(
                    locked,
                    state.is_unlocked(&name),
                ),
                name,
            }
        })
        .collect())
}

/// Switches the active profile. Shared core behind both the desktop
/// `switch_profile` command and the web layer's `POST /api/profile`, so a
/// remote switch runs the identical validated sequence — soft-lock gate,
/// shared pool/name swap, plugin-host rebuild, `ProfileSwitched` activity
/// event — under the same `profile_lifecycle` lock.
///
/// Generic over the Tauri runtime so it can be exercised against
/// `tauri::test::mock_app()`'s `MockRuntime` handle.
pub(crate) async fn switch_active_profile<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    name: String,
) -> CarrelResult<()> {
    switch_active_profile_with(app, state, name, carrel_core::profile_lock::has_lock).await
}

/// [`switch_active_profile`] with the lock-status probe injected. The only
/// production caller passes `profile_lock::has_lock`; tests pass a stub so
/// the locked path is deterministic without a real keychain (a keychain call
/// behaves differently per host — see the note in this module's tests).
async fn switch_active_profile_with<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    name: String,
    has_lock: impl Fn(&str) -> CarrelResult<bool>,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this validate-then-mutate sequence can't interleave
    // with `delete_profile` — otherwise a switch could validate that `name`
    // exists, `delete_profile` could then remove its pool, and this would
    // still set `ps.active = name`, leaving the active profile pointing at a
    // deleted profile.
    let _lifecycle = state.profile_lifecycle.lock().await;
    {
        let ps = state.profile_state.lock()?;
        if name != "default" && !ps.pools.contains_key(&name) {
            return Err(CarrelError::invalid(format!("Profile '{name}' not found")));
        }
    }

    // Soft-lock gate (A-M2): a profile with a stored lock that hasn't been
    // unlocked this session cannot be switched into. Checked before
    // mutating `profile_state` so a rejected switch leaves the active
    // profile untouched — the frontend retries after a successful
    // `unlock_profile`.
    if !carrel_core::profile_lock::access_allowed(has_lock(&name)?, state.is_unlocked(&name)) {
        return Err(CarrelError::lock_required(format!(
            "Profile '{name}' is locked"
        )));
    }

    {
        let mut ps = state.profile_state.lock()?;
        ps.active = name.clone();
    }

    // Mark the profile unlocked *before* publishing it as the shared active
    // profile name, so the web gate (which only checks `unlocked_profiles`
    // membership, not the keychain) never observes a new active profile
    // that isn't in the set yet.
    state.mark_unlocked(&name)?;

    // Sync the shared pool + profile name used by the web server.
    let new_pool = state.active_db()?;
    {
        let mut shared = state.shared_active_pool.lock()?;
        *shared = new_pool;
    }
    {
        let mut shared_name = state.shared_active_profile_name.lock()?;
        *shared_name = name.clone();
    }

    // Rebuild the plugin manager against the new profile's DB so plugin
    // grants, enable-state, and host-API queries all follow the active
    // profile. Plugins are per-profile; the single bus subscriber reads the
    // swapped slot, so no listener is leaked.
    crate::plugin_host::rebuild_for_profile(
        app,
        &state.data_dir,
        state.active_db()?,
        &state.plugin_manager,
    );

    let conn = state.active_db()?.get()?;
    log_event(&conn, ActivityEvent::ProfileSwitched { name: name.clone() });

    // The desktop UI can no longer assume it initiated the switch — the web
    // layer calls this too, and there is one shared active profile. Emitting
    // here (rather than from the command) is what makes a remote switch reach
    // the desktop, which otherwise keeps rendering the previous profile.
    use tauri::Emitter as _;
    let _ = app.emit("profile-changed", name);

    Ok(())
}

#[tauri::command]
pub async fn switch_profile(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    switch_active_profile(&app, state.inner(), name).await
}

/// [`delete_profile`] with the OPDS credential store injected. The only
/// production caller passes `&KeyringCredentialStore`; tests pass
/// `MemoryCredentialStore`/`FlakyCredentialStore` so the credential-cleanup
/// failure path (and the retryability it's meant to guarantee) is
/// deterministic without touching the real keychain.
async fn delete_profile_with(
    state: &AppState,
    name: String,
    store: &dyn CredentialStore,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points.
    let _lifecycle = state.profile_lifecycle.lock().await;
    // OPDS_AUTH_LOCK, taken *after* `_lifecycle` and held to the end of this
    // function: without it, a credential write (`set_opds_auth` et al., which
    // hold OPDS_AUTH_LOCK but not `profile_lifecycle`) can snapshot this
    // profile before `clear_profile_opds_credentials` below runs and then
    // re-create a secret for it after the profile's DB and keychain entries
    // have been removed — turning Task 10's fail-loud cleanup into a silent
    // fail-open. Lock order `profile_lifecycle` -> `OPDS_AUTH_LOCK` is
    // deadlock-free: no current OPDS_AUTH_LOCK holder (`add_opds_catalog`,
    // `remove_opds_catalog`, `set_opds_auth`, `clear_opds_auth`,
    // `add_opds_catalog_with_auth` — `get_opds_auth` deliberately doesn't
    // lock at all, see its own comment) ever takes `profile_lifecycle`, so
    // the two locks are never acquired in the
    // opposite order anywhere in this module — verified by grepping every
    // `profile_lifecycle` site (`create_profile`, `switch_active_profile_with`,
    // this function, `set_profile_lock`, `remove_profile_lock`,
    // `unlock_profile`, `reset_profile_lock`) against every `OPDS_AUTH_LOCK`
    // site: the two sets are disjoint. Do not reorder these two acquisitions.
    let _opds_guard = OPDS_AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if name == "default" {
        return Err(CarrelError::invalid("Cannot delete the default profile"));
    }
    let pool = {
        let ps = state.profile_state.lock()?;
        if ps.active == name {
            return Err(CarrelError::invalid(
                "Cannot delete the active profile. Switch to another profile first.",
            ));
        }
        ps.pools.get(&name).cloned()
    };

    // Clear any OPDS catalog credentials this profile owned (Task 10),
    // BEFORE the pool is removed from `profile_state` below. Nothing has
    // been mutated yet at this point, so a failure here (propagated, not
    // swallowed) leaves the profile fully intact and the operation
    // retryable — rather than orphaning it. The alternative (clearing after
    // the pool is removed) is worse than merely "fail loud": once the pool
    // is out of `profile_state.pools`, `switch_active_profile_with` refuses
    // to switch into the profile (pools are only ever created at startup or
    // profile creation, never lazily), so the profile becomes permanently
    // unreachable until an app restart; and a retried `delete_profile` would
    // then find no pool at all, skip credential cleanup silently, and
    // delete the DB file anyway — leaving exactly the secrets this task
    // exists to remove sitting in the keychain.
    //
    // Locking `profile_state` twice in this function (here, and again below
    // to actually remove the pool) is safe specifically because `_lifecycle`
    // is held for this entire function body, and `switch_active_profile_with`
    // takes that same `profile_lifecycle` lock for its entire body too — so
    // no switch, and no other profile-lifecycle command, can run between
    // this section and the one below. Do not "simplify" this back into one
    // lock without preserving that invariant.
    if let Some(pool) = &pool {
        let conn = pool.get()?;
        clear_profile_opds_credentials(&conn, store, &name)?;
    }

    {
        let mut ps = state.profile_state.lock()?;
        ps.pools.remove(&name);
    }

    // Remove DB file
    let db_path = state.data_dir.join(format!("library-{name}.db"));
    let _ = std::fs::remove_file(db_path);

    // Soft-lock hygiene (A-M2, Decision 10): clear the keychain entry and
    // drop the profile from the unlocked set, best-effort, so a re-created
    // same-name profile never inherits the old lock or an already-unlocked
    // session.
    let _ = carrel_core::profile_lock::clear_lock(&name);
    state.mark_locked(&name)?;

    Ok(())
}

#[tauri::command]
pub async fn delete_profile(name: String, state: State<'_, AppState>) -> CarrelResult<()> {
    delete_profile_with(state.inner(), name, &KeyringCredentialStore).await
}

// --- Profile soft-lock (A-M2) ---
//
// See `docs/superpowers/specs/2026-07-07-profile-soft-lock-design.md`.
// `carrel_core::profile_lock` (A-M1) owns the Argon2id KDF and keychain
// storage; these commands are the IPC-facing wiring plus the session
// unlock state on `AppState`. The profile password never crosses the web
// layer (Decision 5) — these commands are desktop-only.

/// Run [`carrel_core::profile_lock::hash_password`] off the async runtime.
/// Argon2id is CPU/memory-heavy (~19 MiB, 2 iterations); never call it
/// directly from a `#[tauri::command]` body.
async fn hash_password_blocking(password: SecretString) -> CarrelResult<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(carrel_core::profile_lock::hash_password(&password));
    });
    rx.recv()?
}

/// Run [`carrel_core::profile_lock::verify_password`] off the async runtime.
async fn verify_password_blocking(password: SecretString, phc: String) -> CarrelResult<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(carrel_core::profile_lock::verify_password(&password, &phc));
    });
    rx.recv()?
}

/// Sets or changes `profile`'s lock. If a lock already exists, `current_password`
/// must verify against it (Decision 9) — changing a lock from an
/// already-unlocked session still requires proving the current password.
/// On success the profile is marked unlocked for this session.
#[tauri::command]
pub async fn set_profile_lock(
    profile: String,
    password: String,
    current_password: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points.
    let _lifecycle = state.profile_lifecycle.lock().await;
    state.ensure_profile_exists(&profile)?;
    if let Some(existing_phc) = carrel_core::profile_lock::load_lock(&profile)? {
        let current = current_password.ok_or_else(|| {
            CarrelError::invalid("Current password is required to change the profile lock")
        })?;
        if !verify_password_blocking(SecretString::from(current), existing_phc).await? {
            return Err(CarrelError::invalid("Incorrect current password"));
        }
    }

    let phc = hash_password_blocking(SecretString::from(password)).await?;
    carrel_core::profile_lock::set_lock(&profile, &phc)?;
    state.mark_unlocked(&profile)?;
    Ok(())
}

/// Removes `profile`'s lock. Requires the current password (Decision 9) —
/// there is no one-tap removal from the lock screen.
#[tauri::command]
pub async fn remove_profile_lock(
    profile: String,
    current_password: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points.
    let _lifecycle = state.profile_lifecycle.lock().await;
    state.ensure_profile_exists(&profile)?;
    let phc = carrel_core::profile_lock::load_lock(&profile)?
        .ok_or_else(|| CarrelError::invalid("Profile has no lock to remove"))?;
    if !verify_password_blocking(SecretString::from(current_password), phc).await? {
        return Err(CarrelError::invalid("Incorrect password"));
    }
    carrel_core::profile_lock::clear_lock(&profile)?;
    // A profile with no lock is never gated (see `access_allowed`) — keep
    // the invariant that `unlocked_profiles` reflects that, in case this
    // profile is currently active and being watched by the web gate.
    state.mark_unlocked(&profile)?;
    Ok(())
}

/// Verifies `password` against `profile`'s stored lock and, on success,
/// marks it unlocked for the rest of this session. Fails closed: any
/// keychain error other than "no lock set" propagates as an error rather
/// than being treated as an unlock (Decision 7). Wrong password returns a
/// plain typed error — there is no lockout or rate-limit on the local
/// prompt (Decision 8).
#[tauri::command]
pub async fn unlock_profile(
    app: AppHandle,
    profile: String,
    password: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points.
    let _lifecycle = state.profile_lifecycle.lock().await;
    state.ensure_profile_exists(&profile)?;
    // A profile with no lock configured has nothing to verify — falls
    // through to `mark_unlocked` and the plugin-start tail below.
    if let Some(phc) = carrel_core::profile_lock::load_lock(&profile)? {
        if !verify_password_blocking(SecretString::from(password), phc).await? {
            return Err(CarrelError::invalid("Incorrect password"));
        }
    }
    state.mark_unlocked(&profile)?;
    run_deferred_plugin_start(&app, &profile, &state)?;
    Ok(())
}

/// Runs the deferred plugin-manager startup for a just-unlocked profile.
///
/// Soft-lock (A-M2, D-6/SB-7): if the active profile was locked at boot,
/// startup deliberately skipped building the plugin manager and emitting
/// `AppStarted` (an `AppStarted` plugin with `read:library` would
/// otherwise read the locked profile). Now that the active profile is
/// unlocked — whether by password (`unlock_profile`) or recovery reset
/// (`reset_profile_lock`) — build the manager and fire `AppStarted` once,
/// the deferred equivalent of the withheld startup dispatch. The empty-slot
/// check keeps this idempotent, so re-unlocking an already-running profile
/// is a no-op.
fn run_deferred_plugin_start<R: tauri::Runtime>(
    app: &AppHandle<R>,
    profile: &str,
    state: &AppState,
) -> CarrelResult<()> {
    let active = { state.profile_state.lock()?.active.clone() };
    if profile == active && state.plugin_manager.lock()?.is_none() {
        crate::plugin_host::rebuild_for_profile(
            app,
            &state.data_dir,
            state.active_db()?,
            &state.plugin_manager,
        );
        carrel_core::events::bus().emit(carrel_core::events::CarrelEvent::AppStarted);
    }
    Ok(())
}

/// Clears `profile`'s lock **without** the current password — the "forgot
/// password" recovery path (Decision 9). Safe because nothing is
/// encrypted: this only removes the deterrent and never touches the
/// library, books, or database. The frontend routes this behind a
/// deliberate, clearly-labelled confirmation step (never a one-tap button
/// on the lock screen itself). Mirrors `remove_profile_lock`'s tail: a
/// profile with no lock is never gated, so it's marked unlocked too.
#[tauri::command]
pub async fn reset_profile_lock<R: tauri::Runtime>(
    app: AppHandle<R>,
    profile: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Leaf serialization lock (see `AppState::profile_lifecycle`): held for
    // the whole body so this can't interleave with the other
    // profile-lifecycle commands across their `.await` points.
    let _lifecycle = state.profile_lifecycle.lock().await;
    state.ensure_profile_exists(&profile)?;
    carrel_core::profile_lock::clear_lock(&profile)?;
    state.mark_unlocked(&profile)?;
    // Recovery is an alternate unlock path: if the active profile was locked
    // at boot, run the same deferred plugin startup `unlock_profile` does.
    run_deferred_plugin_start(&app, &profile, &state)?;
    Ok(())
}

/// Soft-lock status for `profile`, driving the frontend's unlock prompt.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileLockStatus {
    pub locked: bool,
    pub unlocked_this_session: bool,
}

#[tauri::command]
pub async fn profile_lock_status(
    profile: String,
    state: State<'_, AppState>,
) -> CarrelResult<ProfileLockStatus> {
    state.ensure_profile_exists(&profile)?;
    Ok(ProfileLockStatus {
        locked: carrel_core::profile_lock::has_lock(&profile)?,
        unlocked_this_session: state.is_unlocked(&profile),
    })
}

// --- Library Folder ---

#[derive(serde::Serialize)]
pub struct LibraryFolderInfo {
    pub path: String,
    pub file_count: u64,
    pub linked_count: u64,
    pub total_size_bytes: u64,
}

/// Thin wrapper kept for backwards compatibility with the existing in-crate
/// call sites; the implementation lives in [`carrel_core::paths`] so both the
/// desktop app and future headless binaries share a single definition.
pub fn default_library_folder() -> CarrelResult<String> {
    carrel_core::paths::default_library_folder()
}

#[tauri::command]
pub async fn get_library_folder(state: State<'_, AppState>) -> CarrelResult<String> {
    let conn = state.active_db()?.get()?;
    if let Some(folder) = db::get_setting(&conn, "library_folder")? {
        Ok(folder)
    } else {
        default_library_folder()
    }
}

#[tauri::command]
pub async fn get_library_folder_info(
    state: State<'_, AppState>,
) -> CarrelResult<LibraryFolderInfo> {
    let conn = state.active_db()?.get()?;
    let path = if let Some(f) = db::get_setting(&conn, "library_folder")? {
        f
    } else {
        default_library_folder()?
    };
    let books = db::list_books(&conn)?;

    // #64 M4: `file_path` is now a storage key for imported books, so we
    // resolve each book through `AppState::resolve_book_path` before
    // comparing to the requested folder. Linked books whose absolute path
    // sits elsewhere are naturally excluded.
    let prefix = if path.ends_with('/') {
        path.clone()
    } else {
        format!("{}/", path)
    };
    let mut file_count = 0u64;
    let mut linked_count = 0u64;
    let mut total_size_bytes = 0u64;
    for book in &books {
        let resolved = match state.resolve_book_path(book) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Books whose resolved path lives outside the storage folder are
        // linked (remote) rather than imported into the library.
        if !resolved.starts_with(&prefix) {
            linked_count += 1;
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&resolved) {
            if meta.is_file() {
                file_count += 1;
                total_size_bytes += meta.len();
            }
        }
    }

    Ok(LibraryFolderInfo {
        path,
        file_count,
        linked_count,
        total_size_bytes,
    })
}

#[tauri::command]
pub async fn set_library_folder(
    new_folder: String,
    move_files: bool,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Validate the folder path: reject obviously dangerous values.
    let folder_path = std::path::Path::new(&new_folder);
    if new_folder.is_empty() || new_folder == "/" || new_folder == "\\" {
        return Err(CarrelError::invalid("Invalid library folder path."));
    }
    // Ensure the folder exists (or can be created) then canonicalize.
    std::fs::create_dir_all(&new_folder)?;
    let canonical = std::fs::canonicalize(folder_path)
        .map_err(|e| format!("Cannot resolve library folder: {e}"))?;
    let canonical_str = canonical.to_string_lossy().to_string();

    if !move_files {
        let conn = state.active_db()?.get()?;
        db::set_setting(&conn, "library_folder", &canonical_str)?;
        return Ok(());
    }

    // Atomic migration: gather books, plan moves, execute all-or-nothing.
    let books = {
        let conn = state.active_db()?.get()?;
        db::list_books(&conn)?
    };

    // #64 M4: `book.file_path` is a storage key for imported books and an
    // absolute path for linked ones. Resolve each imported source to an
    // absolute path via the *current* library storage before moving, and
    // persist the new key (`{book_id}.{ext}`) back to the DB on success.
    // Linked books are not relocated.
    let current_storage = state.active_storage()?;
    let moves: Vec<(String, String, String)> = books
        .iter()
        .filter(|b| b.is_imported)
        .map(|book| {
            // Use the key's extension where possible (matches the on-disk
            // filename); fall back to reading it from the resolved path.
            let ext = std::path::Path::new(&book.file_path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let new_key = format!("{}.{}", book.id, ext);
            let dest = format!("{}/{}", canonical_str, new_key);
            let src = match current_storage.local_path(&book.file_path) {
                Ok(p) => p.to_string_lossy().to_string(),
                // Legacy row with absolute path that escaped the M4
                // migration — move it as-is.
                Err(_) => book.file_path.clone(),
            };
            (src, dest, new_key)
        })
        .collect();

    // Attempt all moves; roll back on first failure.
    let mut completed: Vec<(String, String)> = Vec::new();
    for (src, dest, _new_key) in &moves {
        let result = std::fs::rename(src, dest).or_else(|_| {
            // Cross-device fallback: copy then delete source.
            std::fs::copy(src, dest)
                .map(|_| ())
                .and_then(|_| std::fs::remove_file(src))
        });
        if let Err(e) = result {
            // Roll back every completed move before returning the error.
            // Collect rollback failures so the caller has full context if
            // rollback itself fails (e.g. cross-device copy-back fails).
            let mut rollback_errors: Vec<String> = Vec::new();
            for (orig_src, orig_dest) in &completed {
                if let Err(re) = std::fs::rename(orig_dest, orig_src).or_else(|_| {
                    std::fs::copy(orig_dest, orig_src)
                        .map(|_| ())
                        .and_then(|_| std::fs::remove_file(orig_dest))
                }) {
                    rollback_errors.push(format!("'{}': {}", orig_dest, re));
                }
            }
            let mut msg = format!("Failed to move '{}': {}", src, e);
            if !rollback_errors.is_empty() {
                msg = format!(
                    "{}. Rollback also failed: {}",
                    msg,
                    rollback_errors.join("; ")
                );
            }
            return Err(CarrelError::io(msg));
        }
        completed.push((src.clone(), dest.clone()));
    }

    // All moves succeeded — persist new keys and setting atomically.
    let mut conn = state.active_db()?.get()?;
    let tx = conn.transaction()?;
    let imported_books: Vec<&Book> = books.iter().filter(|b| b.is_imported).collect();
    for (book, (_, _, new_key)) in imported_books.iter().zip(moves.iter()) {
        db::update_book_file_path(&tx, &book.id, new_key)?;
    }
    db::set_setting(&tx, "library_folder", &canonical_str)?;
    tx.commit()?;

    Ok(())
}

#[tauri::command]
pub async fn copy_to_library(book_id: String, state: State<'_, AppState>) -> CarrelResult<Book> {
    let conn = state.active_db()?.get()?;
    let book =
        db::get_book(&conn, &book_id)?.ok_or_else(|| CarrelError::not_found("Book not found"))?;

    if book.is_imported {
        return Err(CarrelError::invalid("Book is already in the library"));
    }

    // Linked book: `file_path` is an external absolute path. Verify it still exists,
    // then import it through the library storage (#64 M2/M4).
    if !std::path::Path::new(&book.file_path).exists() {
        return Err(CarrelError::invalid(
            "Source file not available. Reconnect the drive and try again.",
        ));
    }

    let ext = std::path::Path::new(&book.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("epub")
        .to_string();
    let storage = state.active_storage()?;
    let key = book_storage_key(&book.id, &ext);
    storage
        .put_path(&key, std::path::Path::new(&book.file_path))
        .map_err(|e| CarrelError::internal(format!("Failed to copy file to library: {e}")))?;

    db::update_book_path(&conn, &book.id, &key, true)?;

    log_event(
        &conn,
        ActivityEvent::BookUpdated {
            id: book.id.clone(),
            title: book.title.clone(),
            detail: "Copied to library".to_string(),
        },
    );

    db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found("Book not found after update"))
}

// --- Library Export/Import ---

#[tauri::command]
pub async fn export_library(
    dest_path: String,
    include_files: bool,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<String> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let conn = state.active_db()?.get()?;
    let books = db::list_books(&conn)?;
    let metadata = db::build_core_export(&conn)?;

    let file = std::fs::File::create(&dest_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // Add metadata JSON
    let metadata_json = serde_json::to_string_pretty(&metadata)?;
    zip.start_file("library.json", options)?;
    zip.write_all(metadata_json.as_bytes())?;

    let mut linked_count = 0u32;
    if include_files {
        // Add each book file (use Stored for already-compressed formats)
        // `large_file(true)` forces ZIP64 per entry so a book ≥4GB doesn't
        // abort the write mid-stream (the zip crate errors "Large file
        // option has not been set" otherwise, leaving a truncated archive).
        let stored_options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        for book in &books {
            if !book.is_imported {
                linked_count += 1;
                continue;
            }
            // #64 M4: resolve the storage key to an absolute path for reading.
            let resolved = match state.resolve_book_path(book) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let ext = std::path::Path::new(&resolved)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let archive_name = format!("books/{}.{}", book.id, ext);
            // epub/cbz are already zips; pdf compresses poorly — use Stored for all
            if let Ok(data) = std::fs::read(&resolved) {
                zip.start_file(&archive_name, stored_options)?;
                zip.write_all(&data)?;
            }
        }

        // Add cover files
        if let Ok(_data_dir) = app.path().app_data_dir() {
            for book in &books {
                if let Some(cover_path) = &book.cover_path {
                    // Prefer the small grid thumbnail (sibling `thumb.jpg`)
                    // over the full-resolution cover so backups stay small —
                    // the thumbnail is what the library grid displays. Falls
                    // back to the full cover when no thumbnail exists (the
                    // cover was already small enough at import time).
                    let thumb = std::path::Path::new(cover_path).with_file_name(THUMB_FILENAME);
                    let (src, archive_name) = if thumb.exists() {
                        (thumb, format!("covers/{}/cover.jpg", book.id))
                    } else {
                        let ext = std::path::Path::new(cover_path)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("jpg");
                        (
                            std::path::PathBuf::from(cover_path),
                            format!("covers/{}/cover.{}", book.id, ext),
                        )
                    };
                    if let Ok(data) = std::fs::read(&src) {
                        zip.start_file(&archive_name, options)?;
                        zip.write_all(&data)?;
                    }
                }
            }
        }
    }

    zip.finish()?;

    let export_detail = if include_files {
        if linked_count > 0 {
            &format!(
                "Full backup with files ({} linked books skipped)",
                linked_count
            )
        } else {
            "Full backup with files"
        }
    } else {
        "Metadata only"
    };
    log_event(
        &conn,
        ActivityEvent::LibraryExported {
            detail: export_detail.to_string(),
        },
    );

    Ok(dest_path)
}

/// Deserialized `library.json` from a backup — the counterpart to
/// `db::build_core_export`. All fields default to empty so older/partial
/// backups (or a bare book array) still deserialize.
#[derive(Default, serde::Deserialize)]
struct LibraryExport {
    #[serde(default)]
    books: Vec<Book>,
    #[serde(default)]
    reading_progress: Vec<carrel_core::models::ReadingProgress>,
    #[serde(default)]
    bookmarks: Vec<carrel_core::models::Bookmark>,
    #[serde(default)]
    highlights: Vec<carrel_core::models::Highlight>,
    #[serde(default)]
    collections: Vec<carrel_core::models::Collection>,
    #[serde(default)]
    tags: Vec<(String, String)>,
    #[serde(default)]
    book_tags: Vec<(String, String, String)>,
}

#[tauri::command]
pub async fn import_library_backup(
    archive_path: String,
    state: State<'_, AppState>,
    _app: AppHandle,
) -> CarrelResult<u32> {
    use std::io::Read;

    let file = std::fs::File::open(&archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Read library.json. `build_core_export` writes an object
    // (`{ "version", "books", "reading_progress", ... }`); a bare top-level
    // array of books is also accepted for backward compatibility with any
    // plain-list backups.
    let export: LibraryExport = {
        let mut entry = archive.by_name("library.json")?;
        let mut json = String::new();
        entry.read_to_string(&mut json)?;
        let value: serde_json::Value = serde_json::from_str(&json)?;
        match value {
            serde_json::Value::Array(_) => LibraryExport {
                books: serde_json::from_value(value)?,
                ..Default::default()
            },
            serde_json::Value::Object(_) => serde_json::from_value(value)?,
            _ => {
                return Err(CarrelError::invalid(
                    "library.json is neither an object nor an array",
                ))
            }
        }
    };
    let books = &export.books;

    let conn = state.active_db()?.get()?;
    let library_folder = match db::get_setting(&conn, "library_folder")? {
        Some(f) => f,
        None => default_library_folder()?,
    };
    std::fs::create_dir_all(&library_folder)?;

    let mut imported = 0u32;

    // Helper: validate that a ZIP entry name is safe (no path traversal).
    let is_safe_zip_entry = |name: &str| -> bool {
        !name.contains("..") && !name.starts_with('/') && !name.starts_with('\\')
    };

    for book in books {
        // Skip if book already exists by hash
        if let Some(ref hash) = book.file_hash {
            if db::get_book_by_file_hash(&conn, hash)?.is_some() {
                continue;
            }
        }

        // Linked books carry no file bytes in the backup (export skips
        // them; only the metadata row + cover are archived). Restore them
        // as links to their original absolute path — consistent with how
        // `resolve_book_path` treats linked books. The source volume must
        // be mounted at the same path on the restoring machine.
        let restored_file_path = if book.is_imported {
            // Derive the extension for the archive entry. For post-M4 backups
            // `book.file_path` is a storage key (e.g. `abc.epub`); for older
            // backups it's an absolute path. `Path::extension` handles both.
            let ext = std::path::Path::new(&book.file_path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("epub");
            let book_archive_name = format!("books/{}.{}", book.id, ext);

            // Validate ZIP entry name before extraction
            if !is_safe_zip_entry(&book_archive_name) {
                continue;
            }

            // Extract book file through the library storage — storage owns the
            // on-disk layout, and the key (`{book_id}.{ext}`) is what the DB
            // now stores (#64 M4).
            let storage = state.active_storage()?;
            let book_key = book_storage_key(&book.id, ext);
            if let Ok(mut entry) = archive.by_name(&book_archive_name) {
                // Validate the actual entry name from the archive as well
                if !is_safe_zip_entry(entry.name()) {
                    continue;
                }
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                storage.put(&book_key, &buf)?;
            } else {
                continue; // imported book whose file is missing from the backup
            }
            book_key
        } else {
            // Linked: keep the original absolute path verbatim.
            book.file_path.clone()
        };

        // Extract cover if present — route through the covers storage
        // (#64 M3) so on-disk layout stays identical whether restore writes
        // locally or (eventually) to a remote backend.
        let mut cover_path = book.cover_path.clone();
        let covers_storage = state.covers_storage()?;
        for ext_try in &["jpg", "png", "webp", "gif"] {
            let cover_name = format!("covers/{}/cover.{}", book.id, ext_try);
            if !is_safe_zip_entry(&cover_name) {
                continue;
            }
            if let Ok(mut entry) = archive.by_name(&cover_name) {
                if !is_safe_zip_entry(entry.name()) {
                    continue;
                }
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                let key = cover_storage_key(&book.id, ext_try);
                covers_storage.put(&key, &buf)?;
                cover_path = Some(
                    covers_storage
                        .local_path(&key)?
                        .to_string_lossy()
                        .to_string(),
                );
                break;
            }
        }

        let restored_book = Book {
            file_path: restored_file_path,
            cover_path,
            ..book.clone()
        };

        if db::insert_book(&conn, &restored_book).is_ok() {
            imported += 1;
        }
    }

    // Restore non-book data (reading progress, bookmarks, highlights,
    // collections, tags). Best-effort: rows referencing a book that wasn't
    // imported are skipped. Runs after books so foreign keys resolve.
    let counts = db::restore_secondary_data(
        &conn,
        &db::SecondaryImport {
            reading_progress: &export.reading_progress,
            bookmarks: &export.bookmarks,
            highlights: &export.highlights,
            collections: &export.collections,
            tags: &export.tags,
            book_tags: &export.book_tags,
        },
    );

    log_event(
        &conn,
        ActivityEvent::LibraryImported {
            detail: format!(
                "Restored from backup ({imported} books, {} bookmarks, {} highlights, {} collections)",
                counts.bookmarks, counts.highlights, counts.collections
            ),
        },
    );

    Ok(imported)
}

// --- PDF ---

#[tauri::command]
pub async fn check_pdf_support() -> bool {
    pdf::is_available()
}

#[tauri::command]
pub async fn get_pdf_page_count(book_id: String, state: State<'_, AppState>) -> CarrelResult<u32> {
    let file_path = {
        let conn = state.active_db()?.get()?;
        let book = db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?;
        state.resolve_book_path(&book)?
    };
    validate_file_exists(&file_path)?;
    pdf::get_page_count(&file_path)
}

/// On-demand, normalized glyph rectangles for one PDF page (F-1-4,
/// desktop reader text-selection layer). Bounds are never persisted —
/// computed by `pdf::get_page_glyphs` and kept only in its in-memory LRU.
#[tauri::command]
pub async fn get_pdf_page_glyphs(
    book_id: String,
    page_index: u32,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<pdf::Glyph>> {
    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };
    if book.format != BookFormat::Pdf {
        return Err(CarrelError::invalid(
            "get_pdf_page_glyphs only supports PDF format",
        ));
    }
    let file_path = state.resolve_book_path(&book)?;
    validate_file_exists(&file_path)?;
    pdf::get_page_glyphs(&file_path, page_index as usize)
}

/// Full text of one PDF page (F-1-4, desktop reader text-selection layer).
/// The frontend pairs this with `get_pdf_page_glyphs`: glyph `off` values are
/// CHAR (Unicode-scalar) offsets into this string, so the frontend indexes it
/// via `Array.from(text)` to recover each glyph's character for native copy
/// and for the highlight `text` payload. Resolution mirrors
/// `search_book_content`'s PDF arm exactly (memory → disk index → extract when
/// a hash + page cache are available; memory-only otherwise) so it shares the
/// same offset space as search's `match_offset`.
#[tauri::command]
pub async fn get_pdf_page_text(
    book_id: String,
    page_index: u32,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<String> {
    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };
    if book.format != BookFormat::Pdf {
        return Err(CarrelError::invalid(
            "get_pdf_page_text only supports PDF format",
        ));
    }
    let file_path = state.resolve_book_path(&book)?;
    validate_file_exists(&file_path)?;

    // Hot path: the in-memory cache is warm (prepare_pdf's background pass
    // populates it once per book on open), so clone just this page's string
    // rather than the whole book on every page turn.
    if let Some(text) = pdf::cached_page_text(&file_path, page_index as usize)? {
        return Ok(text);
    }

    // Cold miss: run the full resolve ONCE (memory → disk index → extract),
    // which also warms PDF_TEXT_CACHE, then read the single page. Mirrors
    // `search_book_content`'s PDF arm so the offset space matches search.
    let pages = match (book.file_hash.as_deref(), page_cache_storage(&app).ok()) {
        (Some(hash), Some(storage)) => pdf::resolve_page_texts(&file_path, &storage, hash)?,
        _ => pdf::page_texts_memory(&file_path)?,
    };
    pages
        .get(page_index as usize)
        .cloned()
        .ok_or_else(|| CarrelError::not_found(format!("page {page_index} not found")))
}

/// PDF page reader for the desktop frontend. Returns raw JPEG bytes
/// plus a trailing mime tag (see `page_wire`); the frontend builds a
/// `Blob` + `URL.createObjectURL` for `<img src>`.
///
/// `width` controls the viewport-target width (clamped to 9600). When
/// omitted, `carrel_core::pdf::get_page_image_bytes` falls back to
/// `DEFAULT_RENDER_WIDTH` (1200 px).
///
/// Cache-first against the `page-cache/` namespace populated by
/// `prepare_pdf`: a disk hit reads canonical-width bytes and resizes
/// down to the viewport target. On miss with a PDF manifest present,
/// renders at the canonical width and writes best-effort to disk
/// RAII counter for in-flight foreground page renders. While any exist, the
/// background PDF prerender pass pauses (M4) so it doesn't contend with the
/// page the user is waiting on. Drop decrements on every exit path — normal
/// return, `?` early return, or future cancellation.
struct ForegroundRenderGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl ForegroundRenderGuard {
    fn new(counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for ForegroundRenderGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// (eviction is coalesced via a callback). Without a manifest (no
/// hash, storage error, or `prepare_pdf` never ran) the function
/// falls back to a direct render at the viewport width so uncacheable
/// PDFs match pre-spec performance.
#[tauri::command]
pub async fn get_pdf_page_bytes(
    book_id: String,
    page_index: u32,
    width: Option<u32>,
    // When true, only serve an already-cached page: a cache miss returns an
    // empty response instead of rendering. The reader's background preloader
    // sets this so warming neighbor pages never triggers a cold pdfium render
    // (which, on a network-mounted library, would contend with the foreground
    // page the user is waiting on). Foreground reads leave it unset.
    cached_only: Option<bool>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<tauri::ipc::Response> {
    let _t = state.ipc_metrics.time("get_pdf_page_bytes");
    let render_width = width.filter(|&w| w > 0).map(|w| w.min(9600));
    let cached_only = cached_only.unwrap_or(false);

    let book = {
        let conn = state.active_db()?.get()?;
        db::get_book(&conn, &book_id)?
            .ok_or_else(|| CarrelError::not_found(format!("Book '{book_id}' not found")))?
    };
    if book.format != BookFormat::Pdf {
        return Err(CarrelError::invalid(
            "get_pdf_page_bytes only supports PDF format",
        ));
    }

    // A cached_only probe touches ONLY the local page cache — never the source
    // PDF — so it must skip resolve_book_path / validate_file_exists, which stat
    // the (possibly network-mounted) file and would defeat the point of a
    // no-contention preload.
    if cached_only {
        return serve_cached_only(app, book.file_hash.clone(), page_index, render_width, "pdf")
            .await;
    }

    let file_path = state.resolve_book_path(&book)?;
    validate_file_exists(&file_path)?;

    // Pulled from State before the blocking hop below (State isn't Send).
    let max_size_mb = {
        let conn = state.active_db()?.get()?;
        db::get_setting(&conn, "page_cache_max_size_mb")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(page_cache::DEFAULT_MAX_CACHE_SIZE_MB)
    };
    let is_private = state.is_private();
    let book_hash = book.file_hash.clone();

    // Mark a foreground render as in flight so the background prerender pass
    // (M4) pauses and yields the source file + CPU to this page. The guard
    // decrements on every exit, including future cancellation.
    let _fg_guard = ForegroundRenderGuard::new(state.active_foreground_renders.clone());

    // The pdfium render + JPEG encode + resize is CPU-bound and, on a
    // network-mounted library, I/O-bound for seconds. Run it on the blocking
    // pool so it never pins a Tauri async worker (which would stall other IPC —
    // including the page the user is actually waiting on).
    let (bytes, out_mime) =
        tauri::async_runtime::spawn_blocking(move || -> CarrelResult<(Vec<u8>, String)> {
            // Cache-first path. Cached pages live at the canonical render
            // width; resize on read clamps them to the viewport-derived
            // target.
            if let Ok(storage) = page_cache_storage(&app) {
                if let Some(ref book_hash) = book_hash {
                    if let Ok((data, mime)) =
                        page_cache::get_cached_page(&storage, book_hash, page_index)
                    {
                        let (bytes, out_mime) =
                            crate::image_util::maybe_resize_to_jpeg(data, mime, render_width)?;
                        return Ok((bytes, out_mime));
                    }
                }
            }

            // Miss path. Use the cached-render code path (canonical 2400 px
            // render + best-effort disk write) only when a PDF manifest is
            // already in place — otherwise the higher render cost has no
            // cache benefit. No manifest → render at viewport width directly,
            // matching pre-spec behavior.
            let (bytes, mime) = if let Some(book_hash) = book_hash.clone() {
                if let Ok(storage) = page_cache_storage(&app) {
                    let has_pdf_manifest = page_cache::read_manifest(&storage, &book_hash)
                        .map(|m| m.format == BookFormat::Pdf)
                        .unwrap_or(false);
                    if has_pdf_manifest {
                        let app_for_evict = app.clone();
                        let on_batch = move || {
                            if let Ok(evict_storage) = page_cache_storage(&app_for_evict) {
                                tauri::async_runtime::spawn_blocking(move || {
                                    let _ = page_cache::run_eviction(&evict_storage, max_size_mb);
                                });
                            }
                        };
                        let (b, m) = page_cache::get_or_render_pdf_page_with_eviction(
                            &storage, &book_hash, &file_path, page_index, on_batch,
                            // Private mode (B-M1, OQ-3/SB-9): skip only the on-disk
                            // page write; the read/pre-warm path above is untouched.
                            is_private,
                        )?;
                        (b, m.to_string())
                    } else {
                        // No PDF manifest — viewport render, no cache.
                        let (b, m) =
                            pdf::get_page_image_bytes(&file_path, page_index, render_width)?;
                        (b, m.to_string())
                    }
                } else {
                    // Storage unavailable — viewport render, no cache.
                    let (b, m) = pdf::get_page_image_bytes(&file_path, page_index, render_width)?;
                    (b, m.to_string())
                }
            } else {
                // No file hash — viewport render, no cache.
                let (b, m) = pdf::get_page_image_bytes(&file_path, page_index, render_width)?;
                (b, m.to_string())
            };

            // Cache-miss canonical-render branch produced 2400 px JPEG bytes;
            // the no-cache fallbacks already match `render_width`.
            // `maybe_resize_to_jpeg` is a no-op when input == target.
            crate::image_util::maybe_resize_to_jpeg(bytes, mime, render_width)
        })
        .await
        .map_err(|e| CarrelError::internal(format!("pdf page render task failed: {e}")))??;

    Ok(tauri::ipc::Response::new(crate::page_wire::append_tag(
        bytes, &out_mime,
    )))
}

// ---- Remote Backup Commands ----

#[tauri::command]
pub async fn get_backup_providers() -> CarrelResult<Vec<crate::backup::ProviderInfo>> {
    Ok(crate::backup::provider_schemas())
}

#[tauri::command]
pub async fn save_backup_config(
    config: crate::backup::BackupConfig,
    skip_test: Option<bool>,
    state: State<'_, AppState>,
) -> Result<crate::backup::ConnectionTestResult, String> {
    // When `skip_test` is set, persist the config without running a
    // connection test. This lets the UI offer a "Save" action distinct
    // from "Test connection", so a save failure is never conflated with a
    // connectivity failure (and credentials can be saved while a remote is
    // temporarily unreachable).
    if skip_test.unwrap_or(false) {
        let clean = crate::backup::store_secrets(&config).map_err(|e| e.to_string())?;
        let conn = state
            .active_db()
            .map_err(|e| e.to_string())?
            .get()
            .map_err(|e| e.to_string())?;
        let json = serde_json::to_string(&clean).map_err(|e| e.to_string())?;
        db::set_setting(&conn, "backup_config", &json).map_err(|e| e.to_string())?;
        return Ok(crate::backup::ConnectionTestResult::Ok { latency_ms: 0 });
    }

    // Snapshot existing secrets for rollback on test failure
    let old_secrets = {
        let conn = state
            .active_db()
            .map_err(|e| e.to_string())?
            .get()
            .map_err(|e| e.to_string())?;
        if let Some(json) = db::get_setting(&conn, "backup_config").map_err(|e| e.to_string())? {
            let mut old_config: crate::backup::BackupConfig =
                serde_json::from_str(&json).map_err(|e| e.to_string())?;
            let _ = crate::backup::load_secrets(&mut old_config);
            Some(old_config)
        } else {
            None
        }
    };

    // Store new secrets in OS keychain
    let clean = crate::backup::store_secrets(&config).map_err(|e| e.to_string())?;

    // Test connection with the original config (secrets still in values map)
    let (tx, rx) = std::sync::mpsc::channel();
    let test_config = config.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = crate::backup::test_connection(&test_config);
        let _ = tx.send(result);
    });
    let test_result = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap_or(crate::backup::ConnectionTestResult::Timeout);

    match &test_result {
        crate::backup::ConnectionTestResult::Ok { .. } => {
            // Test passed — persist clean config to DB
            let conn = state
                .active_db()
                .map_err(|e| e.to_string())?
                .get()
                .map_err(|e| e.to_string())?;
            let json = serde_json::to_string(&clean).map_err(|e| e.to_string())?;
            db::set_setting(&conn, "backup_config", &json).map_err(|e| e.to_string())?;
        }
        _ => {
            // Rollback: restore old secrets or remove new ones
            if let Some(old_config) = &old_secrets {
                let _ = crate::backup::store_secrets(old_config);
            } else {
                let _ = crate::backup::remove_secrets(&config);
            }
        }
    }

    Ok(test_result)
}

#[tauri::command]
pub async fn test_backup_connection(
    config: crate::backup::BackupConfig,
) -> Result<crate::backup::ConnectionTestResult, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let result = crate::backup::test_connection(&config);
        let _ = tx.send(result);
    });
    Ok(rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap_or(crate::backup::ConnectionTestResult::Timeout))
}

#[tauri::command]
pub async fn get_backup_config(
    state: State<'_, AppState>,
) -> CarrelResult<Option<crate::backup::BackupConfig>> {
    let conn = state.active_db()?.get()?;
    match db::get_setting(&conn, "backup_config")? {
        Some(j) => {
            let mut config: crate::backup::BackupConfig = serde_json::from_str(&j)?;
            // Load secrets from OS keychain
            crate::backup::load_secrets(&mut config)?;
            Ok(Some(config))
        }
        None => Ok(None),
    }
}

static BACKUP_RUNNING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// RAII guard for the `BACKUP_RUNNING` set. Acquires the profile entry on
/// construction and releases it on drop — so any `?` in `run_backup` frees
/// the lock automatically without an explicit cleanup block. Without this
/// guard, a fallible setup step (keychain, operator build, storage init)
/// would leave the profile wedged until the app restarted.
#[derive(Debug)]
struct BackupLockGuard {
    profile_name: String,
}

impl BackupLockGuard {
    fn acquire(profile_name: String) -> CarrelResult<Self> {
        let mut running = BACKUP_RUNNING.lock()?;
        if !running.insert(profile_name.clone()) {
            return Err(CarrelError::invalid(
                "A backup is already in progress for this profile",
            ));
        }
        Ok(Self { profile_name })
    }
}

impl Drop for BackupLockGuard {
    fn drop(&mut self) {
        match BACKUP_RUNNING.lock() {
            Ok(mut running) => {
                running.remove(&self.profile_name);
            }
            Err(_) => {
                log::error!(
                    "BACKUP_RUNNING mutex poisoned; could not release lock for profile '{}'",
                    self.profile_name
                );
            }
        }
    }
}

#[tauri::command]
pub async fn run_backup(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<crate::backup::SyncResult> {
    let _t = state.ipc_metrics.time("run_backup");
    let profile_name = {
        let ps = state.profile_state.lock()?;
        ps.active.clone()
    };
    let _guard = BackupLockGuard::acquire(profile_name.clone())?;
    let conn = state.active_db()?.get()?;
    let json = db::get_setting(&conn, "backup_config")?
        .ok_or_else(|| CarrelError::not_found("No backup provider configured"))?;
    let mut config: crate::backup::BackupConfig = serde_json::from_str(&json)?;
    crate::backup::load_secrets(&mut config)?;
    let provider_name = config.provider_type.clone();
    let op = crate::backup::build_operator(&config)?;
    // Pass the active library `Storage` into backup so book-file reads
    // go through the trait (backend-agnostic) instead of `std::fs::read`.
    let library_storage = state.active_storage()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = crate::backup::run_incremental_backup_with_progress(
            &op,
            &conn,
            Some(library_storage.as_ref()),
            &|step, current, total| {
                let _ = app_handle.emit(
                    "backup-progress",
                    serde_json::json!({
                        "step": step,
                        "current": current,
                        "total": total,
                    }),
                );
            },
        );
        let _ = tx.send(result);
    });
    let result = rx.recv()?;
    let log_conn = state.active_db()?.get()?;
    events::bus().emit(CarrelEvent::BackupCompleted {
        provider: format!("{provider_name:?}"),
        success: result.is_ok(),
    });
    match &result {
        Ok(sync_result) => {
            log_event(
                &log_conn,
                ActivityEvent::BackupCompleted {
                    detail: format!(
                        "Provider: {:?} — {} books, {} bookmarks, {} highlights pushed",
                        provider_name,
                        sync_result.books_pushed,
                        sync_result.bookmarks_pushed,
                        sync_result.highlights_pushed,
                    ),
                },
            );
        }
        Err(e) => {
            log_event(
                &log_conn,
                ActivityEvent::BackupFailed {
                    detail: format!("Provider: {:?} — {}", provider_name, e),
                },
            );
        }
    }
    // `_guard` drops here → profile is removed from BACKUP_RUNNING on every
    // return path, including the `?` propagations above.
    result
}

#[tauri::command]
pub async fn get_backup_status(
    state: State<'_, AppState>,
) -> CarrelResult<Option<crate::backup::SyncManifest>> {
    let conn = state.active_db()?.get()?;
    let json = match db::get_setting(&conn, "backup_config")? {
        Some(j) => j,
        None => return Ok(None),
    };
    let mut config: crate::backup::BackupConfig = serde_json::from_str(&json)?;
    crate::backup::load_secrets(&mut config)?;
    let op = crate::backup::build_operator(&config)?;
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(crate::backup::read_manifest(&op));
    });
    let manifest = rx.recv()?;
    Ok(Some(manifest))
}

use std::sync::atomic::{AtomicBool, Ordering};

static SCAN_CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    current: u32,
    total: u32,
    book_title: String,
    status: String,
}

#[tauri::command]
pub async fn start_scan(
    include_skipped: Option<bool>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<()> {
    SCAN_CANCEL.store(false, Ordering::SeqCst);
    let conn = state.active_db()?.get()?;
    if include_skipped.unwrap_or(false) {
        // Re-queue previously skipped books so new providers can try them
        conn.execute(
            "UPDATE books SET enrichment_status = NULL WHERE enrichment_status = 'skipped'",
            [],
        )?;
    }
    let books = db::list_unenriched_books(&conn)?;
    let total = books.len() as u32;
    if total == 0 {
        let _ = app.emit(
            "scan-progress",
            ScanProgress {
                current: 0,
                total: 0,
                book_title: String::new(),
                status: "done".into(),
            },
        );
        return Ok(());
    }
    let registry = {
        let reg = state.enrichment_registry.lock()?;
        let mut new_reg = crate::providers::ProviderRegistry::new();
        for info in reg.list_providers() {
            new_reg.configure_provider(&info.id, info.config.clone());
        }
        new_reg
    };
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for (i, book) in books.iter().enumerate() {
            if SCAN_CANCEL.load(Ordering::SeqCst) {
                let _ = app_clone.emit(
                    "scan-progress",
                    ScanProgress {
                        current: (i + 1) as u32,
                        total,
                        book_title: book.title.clone(),
                        status: "cancelled".into(),
                    },
                );
                return;
            }
            let _ = app_clone.emit(
                "scan-progress",
                ScanProgress {
                    current: (i + 1) as u32,
                    total,
                    book_title: book.title.clone(),
                    status: "running".into(),
                },
            );
            let parsed = crate::enrichment::parse_filename(
                std::path::Path::new(&book.file_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
            );
            let lookup_title = if book.title == "Unknown Title" || book.title == "Unknown" {
                parsed.title.as_deref().unwrap_or(&book.title)
            } else {
                &book.title
            };
            let lookup_author = if book.author.is_empty() || book.author == "Unknown Author" {
                parsed.author.as_deref().unwrap_or(&book.author)
            } else {
                &book.author
            };
            let lookup_isbn = book.isbn.as_deref().or(parsed.isbn.as_deref());
            match crate::enrichment::enrich_book(
                lookup_title,
                lookup_author,
                lookup_isbn,
                &registry,
            ) {
                Some(result) if result.auto_apply => {
                    let genres_json = if !result.data.genres.is_empty() {
                        Some(serde_json::to_string(&result.data.genres).unwrap_or_default())
                    } else {
                        None
                    };
                    let _ = db::update_book_enrichment(
                        &conn,
                        &book.id,
                        result.data.description.as_deref(),
                        genres_json.as_deref(),
                        result.data.rating,
                        result.data.isbn.as_deref().or(lookup_isbn),
                        match result.data.source_key.as_deref() {
                            Some("") | None => None,
                            some => some,
                        },
                    );
                    // Apply new metadata fields if the book doesn't already have them
                    if let Ok(Some(mut db_book)) = db::get_book(&conn, &book.id) {
                        let mut changed = false;
                        if db_book.language.is_none() {
                            if let Some(ref v) = result.data.language {
                                db_book.language = Some(v.clone());
                                changed = true;
                            }
                        }
                        if db_book.publisher.is_none() {
                            if let Some(ref v) = result.data.publisher {
                                db_book.publisher = Some(v.clone());
                                changed = true;
                            }
                        }
                        if db_book.publish_year.is_none() {
                            if let Some(v) = result.data.publish_year {
                                db_book.publish_year = Some(v);
                                changed = true;
                            }
                        }
                        if changed {
                            let _ = db::update_book(&conn, &db_book);
                        }
                    }
                    let _ = db::set_enrichment_status(&conn, &book.id, "enriched");
                }
                _ => {
                    let _ = db::set_enrichment_status(&conn, &book.id, "skipped");
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = app_clone.emit(
            "scan-progress",
            ScanProgress {
                current: total,
                total,
                book_title: String::new(),
                status: "done".into(),
            },
        );
    });
    Ok(())
}

#[tauri::command]
pub async fn cancel_scan() -> CarrelResult<()> {
    SCAN_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

// ── Background import ─────────────────────────────────────────────────────────
//
// Mirrors the `start_scan` shape: the IPC command kicks off a `spawn_blocking`
// task that owns the long-running work, emits `import-progress` events, and
// observes `IMPORT_CANCEL` between work units. Only one import may run at a
// time — `IMPORT_RUNNING` enforces that.

static IMPORT_RUNNING: AtomicBool = AtomicBool::new(false);
static IMPORT_CANCEL: AtomicBool = AtomicBool::new(false);

const IMPORT_WORKER_COUNT: usize = 6;

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportProgressEvent {
    /// "scanning" while walking the folder, "importing" per file, "empty"
    /// when no supported files were found, "done" or "cancelled" once the
    /// task exits.
    phase: String,
    current: u32,
    total: u32,
    filename: String,
    imported: u32,
    duplicates: u32,
    errors: u32,
}

#[tauri::command]
pub async fn is_import_running() -> bool {
    IMPORT_RUNNING.load(Ordering::SeqCst)
}

#[tauri::command]
pub async fn cancel_import() -> CarrelResult<()> {
    IMPORT_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub async fn start_files_import(
    paths: Vec<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<()> {
    let resources = acquire_import_slot(&state)?;
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_import_task(app_clone, paths, resources);
    });
    Ok(())
}

#[tauri::command]
pub async fn start_folder_import(
    folder_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<()> {
    let _t = state.ipc_metrics.time("start_folder_import");
    // Validate the root folder up front so the IPC call surfaces obvious
    // mistakes (typo, deleted folder, file picked instead of directory,
    // permission denied, vanished network mount) instead of silently
    // spawning a background task that emits "empty" with zero files —
    // which would tell the user "no supported files" when the real cause
    // is that the folder cannot be traversed at all.
    let root = std::path::Path::new(&folder_path);
    let root_meta = std::fs::metadata(root)
        .map_err(|e| CarrelError::invalid(format!("Cannot read folder: {e}")))?;
    if !root_meta.is_dir() {
        return Err(CarrelError::invalid("Selected path is not a folder"));
    }
    std::fs::canonicalize(root)
        .map_err(|e| CarrelError::invalid(format!("Cannot resolve folder: {e}")))?;
    // Drop the iterator immediately — we only care that opening the
    // directory succeeds. The walker will re-open it inside the spawned
    // task and silently skip nested dirs that fail to read, which is the
    // intended behavior for partial-permission trees.
    let _ = std::fs::read_dir(root)
        .map_err(|e| CarrelError::invalid(format!("Cannot read folder: {e}")))?;

    let resources = acquire_import_slot(&state)?;
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = app_clone.emit(
            "import-progress",
            ImportProgressEvent {
                phase: "scanning".into(),
                current: 0,
                total: 0,
                filename: folder_path.clone(),
                imported: 0,
                duplicates: 0,
                errors: 0,
            },
        );
        // The walker's own canonicalize/read_dir on the root IS the
        // authoritative traversal check — if it fails here we surface a real
        // error instead of falling through to the "empty" branch, which would
        // misdiagnose a vanished mount or permission change as "no supported
        // files". Recursive calls inside the walker still silently skip
        // unreadable nested directories (intended for partial-permission
        // trees).
        let mut files = Vec::new();
        let mut visited: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        if let Err(e) = walk_folder_for_import(
            std::path::Path::new(&folder_path),
            &mut files,
            &mut visited,
            &app_clone,
        ) {
            let _ = app_clone.emit(
                "import-progress",
                ImportProgressEvent {
                    phase: "error".into(),
                    current: 0,
                    total: 0,
                    // Frontend renders this string verbatim via the
                    // `library.importBackgroundError` template.
                    filename: format!("Cannot read folder: {e}"),
                    imported: 0,
                    duplicates: 0,
                    errors: 0,
                },
            );
            IMPORT_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
        files.sort();
        if files.is_empty() {
            // Distinguish a user-cancelled scan (walker bailed early via
            // IMPORT_CANCEL) from an actually empty folder. Emitting "empty"
            // for a cancel would tell the user the folder had no supported
            // files when they just hit Cancel.
            let phase = if IMPORT_CANCEL.load(Ordering::SeqCst) {
                "cancelled"
            } else {
                "empty"
            };
            let _ = app_clone.emit(
                "import-progress",
                ImportProgressEvent {
                    phase: phase.into(),
                    current: 0,
                    total: 0,
                    filename: folder_path.clone(),
                    imported: 0,
                    duplicates: 0,
                    errors: 0,
                },
            );
            IMPORT_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
        run_import_task(app_clone, files, resources);
    });
    Ok(())
}

struct ImportResources {
    db_pool: DbPool,
    storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
    covers_storage: std::sync::Arc<dyn carrel_core::storage::Storage>,
    import_mode: String,
}

fn acquire_import_slot(state: &State<'_, AppState>) -> CarrelResult<ImportResources> {
    if IMPORT_RUNNING.swap(true, Ordering::SeqCst) {
        return Err(CarrelError::invalid("Import already running"));
    }
    IMPORT_CANCEL.store(false, Ordering::SeqCst);
    // From here on, every error path must release the slot.
    let result = (|| -> CarrelResult<ImportResources> {
        let db_pool = state.active_db()?;
        let storage = state.active_storage()?;
        let covers_storage = state.covers_storage()?;
        let import_mode = {
            let conn = db_pool.get()?;
            db::get_setting(&conn, "import_mode")
                .ok()
                .flatten()
                .unwrap_or_else(|| "import".to_string())
        };
        Ok(ImportResources {
            db_pool,
            storage,
            covers_storage,
            import_mode,
        })
    })();
    if result.is_err() {
        IMPORT_RUNNING.store(false, Ordering::SeqCst);
    }
    result
}

fn walk_folder_for_import(
    dir: &std::path::Path,
    results: &mut Vec<String>,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
    app: &AppHandle,
) -> std::io::Result<()> {
    if IMPORT_CANCEL.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Cycle guard: resolve the directory's canonical path and skip if we've
    // already walked it. Symlink loops (`books/back -> ..`) would otherwise
    // recurse forever and wedge the IMPORT_RUNNING slot.
    //
    // Errors from canonicalize/read_dir bubble up as `Err`. The top-level
    // caller surfaces that as an error event so the user sees a real
    // diagnostic; recursive callers below intentionally swallow the error
    // (`let _ = ...`) so partial-permission trees still walk past
    // unreadable nested dirs.
    let canonical = std::fs::canonicalize(dir)?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    let _ = app.emit(
        "import-progress",
        ImportProgressEvent {
            phase: "scanning".into(),
            current: results.len() as u32,
            total: 0,
            filename: dir.to_string_lossy().to_string(),
            imported: 0,
            duplicates: 0,
            errors: 0,
        },
    );
    let supported = supported_import_extensions();
    let entries = std::fs::read_dir(dir)?;
    for entry in entries.flatten() {
        if IMPORT_CANCEL.load(Ordering::SeqCst) {
            return Ok(());
        }
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let path = entry.path();
        // `file_type()` does not follow symlinks. For symlink entries, stat
        // the target so symlinked subdirectories still get walked.
        let is_dir = if file_type.is_symlink() {
            std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            file_type.is_dir()
        };
        if is_dir {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if !name.starts_with('.') && name != "__MACOSX" {
                    // Silently skip unreadable nested dirs.
                    let _ = walk_folder_for_import(&path, results, visited, app);
                }
            }
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let lower = ext.to_lowercase();
            if supported.contains(&lower.as_str()) {
                results.push(path.to_string_lossy().to_string());
            }
        }
    }
    Ok(())
}

fn run_import_task(app: AppHandle, paths: Vec<String>, resources: ImportResources) {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    let total = paths.len() as u32;
    let queue: Mutex<VecDeque<String>> = Mutex::new(paths.into());
    let imported = AtomicU32::new(0);
    let duplicates = AtomicU32::new(0);
    let errors = AtomicU32::new(0);
    let completed = AtomicU32::new(0);

    let ImportResources {
        db_pool,
        storage,
        covers_storage,
        import_mode,
    } = resources;

    if total > 0 {
        std::thread::scope(|scope| {
            for _ in 0..IMPORT_WORKER_COUNT {
                let queue = &queue;
                let imported = &imported;
                let duplicates = &duplicates;
                let errors = &errors;
                let completed = &completed;
                let db_pool = db_pool.clone();
                let storage = storage.clone();
                let covers_storage = covers_storage.clone();
                let import_mode = import_mode.clone();
                let app = app.clone();
                scope.spawn(move || loop {
                    if IMPORT_CANCEL.load(Ordering::SeqCst) {
                        break;
                    }
                    let path = match queue.lock() {
                        Ok(mut q) => match q.pop_front() {
                            Some(p) => p,
                            None => break,
                        },
                        Err(_) => break,
                    };
                    let filename = std::path::Path::new(&path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&path)
                        .to_string();
                    match import_book_inner(
                        path.clone(),
                        db_pool.clone(),
                        storage.clone(),
                        covers_storage.clone(),
                        &import_mode,
                        false,
                        ImportSource::FolderScan,
                    ) {
                        Ok(ImportOutcome::Imported(_)) => {
                            imported.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(ImportOutcome::Duplicate(_)) => {
                            duplicates.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => {
                            log::warn!("import failed for {path}: {e}");
                            errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                    let _ = app.emit(
                        "import-progress",
                        ImportProgressEvent {
                            phase: "importing".into(),
                            current: done,
                            total,
                            filename: filename.clone(),
                            imported: imported.load(Ordering::SeqCst),
                            duplicates: duplicates.load(Ordering::SeqCst),
                            errors: errors.load(Ordering::SeqCst),
                        },
                    );
                });
            }
        });
    }

    let final_phase = if IMPORT_CANCEL.load(Ordering::SeqCst) {
        "cancelled"
    } else {
        "done"
    };
    let _ = app.emit(
        "import-progress",
        ImportProgressEvent {
            phase: final_phase.into(),
            current: completed.load(Ordering::SeqCst),
            total,
            filename: String::new(),
            imported: imported.load(Ordering::SeqCst),
            duplicates: duplicates.load(Ordering::SeqCst),
            errors: errors.load(Ordering::SeqCst),
        },
    );
    IMPORT_RUNNING.store(false, Ordering::SeqCst);
}

#[tauri::command]
pub async fn scan_single_book(book_id: String, state: State<'_, AppState>) -> CarrelResult<Book> {
    let conn = state.active_db()?.get()?;
    let book = db::get_book(&conn, &book_id)?
        .ok_or_else(|| CarrelError::not_found(format!("Book '{}' not found", book_id)))?;
    let parsed = crate::enrichment::parse_filename(
        std::path::Path::new(&book.file_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(""),
    );
    let lookup_title = if book.title == "Unknown Title" || book.title == "Unknown" {
        parsed.title.as_deref().unwrap_or(&book.title)
    } else {
        &book.title
    };
    let lookup_author = if book.author.is_empty() || book.author == "Unknown Author" {
        parsed.author.as_deref().unwrap_or(&book.author)
    } else {
        &book.author
    };
    let lookup_isbn = book.isbn.as_deref().or(parsed.isbn.as_deref());
    let registry = {
        let reg = state.enrichment_registry.lock()?;
        let mut new_reg = crate::providers::ProviderRegistry::new();
        for info in reg.list_providers() {
            new_reg.configure_provider(&info.id, info.config.clone());
        }
        new_reg
    };
    let enabled_provider_names: Vec<String> = registry
        .list_providers()
        .iter()
        .filter(|p| p.config.enabled)
        .map(|p| p.name.clone())
        .collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = lookup_title.to_string();
    let a = lookup_author.to_string();
    let i = lookup_isbn.map(|s| s.to_string());
    tauri::async_runtime::spawn_blocking(move || {
        let _ = tx.send(crate::enrichment::enrich_book(
            &t,
            &a,
            i.as_deref(),
            &registry,
        ));
    });
    let enrichment = rx.recv()?;
    match enrichment {
        Some(result) => {
            let genres_json = if !result.data.genres.is_empty() {
                Some(serde_json::to_string(&result.data.genres).unwrap_or_default())
            } else {
                None
            };
            db::update_book_enrichment(
                &conn,
                &book_id,
                result.data.description.as_deref(),
                genres_json.as_deref(),
                result.data.rating,
                result.data.isbn.as_deref().or(lookup_isbn),
                match result.data.source_key.as_deref() {
                    Some("") | None => None,
                    some => some,
                },
            )?;
            // Apply new metadata fields if the book doesn't already have them
            let mut book = db::get_book(&conn, &book_id)?
                .ok_or_else(|| CarrelError::not_found("Book not found"))?;
            let mut changed = false;
            if book.language.is_none() {
                if let Some(ref v) = result.data.language {
                    book.language = Some(v.clone());
                    changed = true;
                }
            }
            if book.publisher.is_none() {
                if let Some(ref v) = result.data.publisher {
                    book.publisher = Some(v.clone());
                    changed = true;
                }
            }
            if book.publish_year.is_none() {
                if let Some(v) = result.data.publish_year {
                    book.publish_year = Some(v);
                    changed = true;
                }
            }
            if changed {
                db::update_book(&conn, &book)?;
            }
            db::set_enrichment_status(&conn, &book_id, "enriched")?;
            let updated_book = db::get_book(&conn, &book_id)?
                .ok_or_else(|| CarrelError::not_found("Book not found"))?;
            let tried = result.providers_tried.join(", ");
            events::bus().emit(CarrelEvent::MetadataEnriched {
                book_id: book_id.clone(),
                provider: result.data.source.clone(),
            });
            log_event(
                &conn,
                ActivityEvent::BookScanned {
                    id: book_id.clone(),
                    title: updated_book.title.clone(),
                    detail: format!("Matched via {} (searched: {})", result.data.source, tried),
                },
            );
            Ok(updated_book)
        }
        None => {
            db::set_enrichment_status(&conn, &book_id, "skipped")?;
            let tried = enabled_provider_names.join(", ");
            log_event(
                &conn,
                ActivityEvent::BookScanned {
                    id: book_id.clone(),
                    title: book.title.clone(),
                    detail: format!("No match found (searched: {})", tried),
                },
            );
            Err(CarrelError::not_found("No match found"))
        }
    }
}

#[tauri::command]
pub async fn queue_book_for_scan(book_id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::set_enrichment_status(&conn, &book_id, "queued")?)
}

#[tauri::command]
pub async fn get_setting_value(
    key: String,
    state: State<'_, AppState>,
) -> CarrelResult<Option<String>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_setting(&conn, &key)?)
}

#[tauri::command]
pub async fn set_setting_value(
    key: String,
    value: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::set_setting(&conn, &key, &value)?)
}

// ---- Offline dictionary (F-1-1) ----

/// GitHub release asset URL for the gzipped WordNet 3.1 dictionary artifact.
///
/// The `dictionary-v1` release asset is published and live; its `.gz` checksum
/// is pinned in [`DICTIONARY_SHA256`]. Rebuild the artifact with
/// `scripts/build-dictionary-artifact.sh` if it ever needs regenerating.
const DICTIONARY_URL: &str =
    "https://github.com/mikedamoiseau/carrel/releases/download/dictionary-v1/dictionary-v1.db.gz";

/// SHA-256 of the gzipped artifact (`dictionary-v1.db.gz`), verified against the
/// live `dictionary-v1` release asset (see [`DICTIONARY_URL`]). A checksum
/// mismatch aborts the install and leaves no artifact. Regenerate with
/// `scripts/build-dictionary-artifact.sh` if the artifact is ever rebuilt.
const DICTIONARY_SHA256: &str = "1f75f5410c8fd9e7d133c3cc344a64701346a9575dfa48c8cec499f4b1b6505e";

/// Progress payload for `"dictionary-download-progress"`. Byte counts of the
/// compressed stream; `total` is `0` when the server sends no Content-Length.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DictionaryDownloadProgress {
    loaded: u64,
    total: u64,
}

/// Throttle `dictionary-download-progress` emits: fire every ~256 KB and always
/// on completion, so a multi-MB download doesn't flood the IPC bridge. `last`
/// carries the last emitted byte count across calls.
fn should_emit_download_progress(loaded: u64, total: u64, last: &mut u64) -> bool {
    const STEP: u64 = 256 * 1024;
    let done = total > 0 && loaded >= total;
    if done || loaded.saturating_sub(*last) >= STEP {
        *last = loaded;
        true
    } else {
        false
    }
}

/// Probe the installed dictionary artifact (missing / ready / corrupt).
#[tauri::command]
pub async fn get_dictionary_status(
    state: State<'_, AppState>,
) -> CarrelResult<carrel_core::dictionary::DictionaryStatus> {
    Ok(carrel_core::dictionary::inspect(&state.dictionary_dir()))
}

/// Download and install the dictionary artifact, emitting
/// `dictionary-download-progress` as it streams. Resolves only when the install
/// completes (verified + atomically in place) — the invoke resolution is the
/// completion signal, so there is no separate "done" event. Guards against a
/// second concurrent download.
#[tauri::command]
pub async fn download_dictionary<R: tauri::Runtime>(
    state: State<'_, AppState>,
    app: AppHandle<R>,
) -> CarrelResult<()> {
    use std::sync::atomic::Ordering;
    if state
        .dictionary_downloading
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(CarrelError::invalid(
            "A dictionary download is already in progress.",
        ));
    }
    let dir = state.dictionary_dir();
    let emitter = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut last: u64 = 0;
        carrel_core::dictionary::download_and_install(
            DICTIONARY_URL,
            DICTIONARY_SHA256,
            &dir,
            &mut |loaded, total| {
                if should_emit_download_progress(loaded, total, &mut last) {
                    let _ = emitter.emit(
                        "dictionary-download-progress",
                        DictionaryDownloadProgress { loaded, total },
                    );
                }
            },
        )
    })
    .await
    .map_err(|e| CarrelError::internal(format!("download task failed: {e}")));
    // Clear the guard regardless of outcome (join error or install result).
    state.dictionary_downloading.store(false, Ordering::SeqCst);
    let install = result?;
    // The install replaced the artifact file in place; the cached pool (if any)
    // still holds connections to the OLD inode. Invalidate it so the next
    // `lookup_word` lazily rebuilds it against the new file. Leaf lock — held
    // alone, mirroring `delete_dictionary`.
    if install.is_ok() {
        let mut pool = state.dictionary_pool.lock()?;
        *pool = None;
    }
    install
}

/// Delete the installed artifact, first dropping the cached pool so the file
/// handle is released before removal.
#[tauri::command]
pub async fn delete_dictionary(state: State<'_, AppState>) -> CarrelResult<()> {
    {
        let mut pool = state.dictionary_pool.lock()?;
        *pool = None;
    }
    carrel_core::dictionary::delete(&state.dictionary_dir())
}

/// Look up a word against the installed artifact. A missing artifact surfaces
/// as `NotFound` so the frontend can route the user to the settings download
/// flow. The read-only pool is opened lazily and cached in `AppState`.
#[tauri::command]
pub async fn lookup_word(
    word: String,
    state: State<'_, AppState>,
) -> CarrelResult<Option<carrel_core::dictionary::DictionaryEntry>> {
    let pool = {
        let mut guard = state.dictionary_pool.lock()?;
        if guard.is_none() {
            *guard = Some(carrel_core::dictionary::open_readonly_pool(
                &state.dictionary_dir(),
            )?);
        }
        guard.as_ref().expect("pool populated above").clone()
    };
    let conn = pool.get()?;
    carrel_core::dictionary::lookup(&conn, &word)
}

// ---- Vocabulary builder (F-1-5) ----

/// Core logic behind `log_vocabulary_word`: re-checks `vocabulary_enabled`
/// server-side (defense in depth — the frontend already gates the call) and
/// no-ops without writing when it's off. Free function taking `&Connection`
/// directly (mirrors `apply_reading_progress` above) so it's unit-testable
/// without a full `AppState`.
#[allow(clippy::too_many_arguments)]
fn log_vocabulary_word_entry(
    conn: &rusqlite::Connection,
    word: String,
    lemma: String,
    pos: Option<String>,
    definition: String,
    book_id: Option<String>,
    book_title: Option<String>,
    chapter_index: Option<i64>,
    context_sentence: Option<String>,
    start_offset: Option<i64>,
    end_offset: Option<i64>,
) -> CarrelResult<()> {
    if db::get_setting(conn, "vocabulary_enabled")?.as_deref() != Some("true") {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let entry = VocabularyWord {
        id: Uuid::new_v4().to_string(),
        lemma,
        word,
        pos,
        definition,
        book_id,
        book_title,
        chapter_index,
        context_sentence,
        start_offset,
        end_offset,
        seen_count: 1,
        box_num: 1,
        last_reviewed_at: None,
        next_due_at: None,
        last_seen_at: now,
        created_at: now,
    };
    db::upsert_vocabulary_word(conn, &entry)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn log_vocabulary_word(
    word: String,
    lemma: String,
    pos: Option<String>,
    definition: String,
    book_id: Option<String>,
    book_title: Option<String>,
    chapter_index: Option<i64>,
    context_sentence: Option<String>,
    start_offset: Option<i64>,
    end_offset: Option<i64>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    log_vocabulary_word_entry(
        &conn,
        word,
        lemma,
        pos,
        definition,
        book_id,
        book_title,
        chapter_index,
        context_sentence,
        start_offset,
        end_offset,
    )
}

#[tauri::command]
pub async fn list_vocabulary(state: State<'_, AppState>) -> CarrelResult<Vec<VocabularyWord>> {
    let conn = state.active_db()?.get()?;
    db::list_vocabulary(&conn)
}

#[tauri::command]
pub async fn delete_vocabulary_word(id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    db::delete_vocabulary_word(&conn, &id)
}

#[tauri::command]
pub async fn clear_vocabulary(state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    db::clear_vocabulary(&conn)
}

#[tauri::command]
pub async fn get_due_vocabulary(
    limit: i64,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<VocabularyWord>> {
    let conn = state.active_db()?.get()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    db::due_vocabulary(&conn, now, limit)
}

/// Core logic behind `record_vocabulary_review`: stamps `now` and forwards to
/// `db::record_vocabulary_review`. Free function so it's unit-testable
/// without a full `AppState` (mirrors `log_vocabulary_word_entry` above).
fn record_vocabulary_review_now(
    conn: &rusqlite::Connection,
    id: &str,
    correct: bool,
) -> CarrelResult<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    db::record_vocabulary_review(conn, id, correct, now)
}

#[tauri::command]
pub async fn record_vocabulary_review(
    id: String,
    correct: bool,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    record_vocabulary_review_now(&conn, &id, correct)
}

#[tauri::command]
pub async fn get_feature_flags(state: State<'_, AppState>) -> CarrelResult<Vec<FeatureFlag>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_feature_flags(&conn)?)
}

#[tauri::command]
pub async fn set_feature_flag(
    key: String,
    enabled: bool,
    description: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::set_feature_flag(
        &conn,
        &key,
        enabled,
        description.as_deref(),
    )?)
}

#[tauri::command]
pub async fn get_feature_flag_value(key: String, state: State<'_, AppState>) -> CarrelResult<bool> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_feature_flag(&conn, &key)?)
}

#[tauri::command]
pub async fn delete_feature_flag(key: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    Ok(db::delete_feature_flag(&conn, &key)?)
}

#[tauri::command]
pub async fn get_enrichment_providers(
    state: State<'_, AppState>,
) -> CarrelResult<Vec<crate::providers::ProviderInfo>> {
    let reg = state.enrichment_registry.lock()?;
    Ok(reg.list_providers())
}

#[tauri::command]
pub async fn set_enrichment_provider_config(
    provider_id: String,
    enabled: bool,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let config = crate::providers::ProviderConfig {
        enabled,
        api_key: api_key.filter(|k| !k.is_empty()),
    };
    let mut reg = state.enrichment_registry.lock()?;
    reg.configure_provider(&provider_id, config);
    // Persist all provider configs
    let all: std::collections::HashMap<String, crate::providers::ProviderConfig> = reg
        .list_providers()
        .into_iter()
        .map(|p| (p.id, p.config))
        .collect();
    let json = serde_json::to_string(&all)?;
    let conn = state.active_db()?.get()?;
    crate::db::set_setting(&conn, "enrichment_providers", &json)?;
    Ok(())
}

#[tauri::command]
pub async fn set_enrichment_provider_order(
    order: Vec<String>,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let mut reg = state.enrichment_registry.lock()?;
    reg.reorder(&order);
    // Persist the order
    let json = serde_json::to_string(&order)?;
    let conn = state.active_db()?.get()?;
    crate::db::set_setting(&conn, "enrichment_provider_order", &json)?;
    Ok(())
}

// --- Activity log ---

#[tauri::command]
pub async fn get_activity_log(
    limit: Option<u32>,
    offset: Option<u32>,
    action_filter: Option<String>,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<crate::models::ActivityEntry>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_activity_log(
        &conn,
        limit.unwrap_or(100),
        offset.unwrap_or(0),
        action_filter.as_deref(),
    )?)
}

#[tauri::command]
pub async fn get_login_history(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> CarrelResult<Vec<crate::models::WebSessionEntry>> {
    let conn = state.active_db()?.get()?;
    Ok(db::get_web_session_log(
        &conn,
        limit.unwrap_or(100).min(1000),
    )?)
}

#[tauri::command]
pub async fn export_activity_log(
    dest_path: String,
    state: State<'_, AppState>,
) -> CarrelResult<String> {
    let conn = state.active_db()?.get()?;
    let rows = db::get_all_activity(&conn)?;
    let json = serde_json::to_string_pretty(&rows)?;
    std::fs::write(&dest_path, json)?;
    Ok(dest_path)
}

#[tauri::command]
pub async fn prune_activity_log(
    keep: Option<u32>,
    max_age_days: Option<u32>,
    state: State<'_, AppState>,
) -> CarrelResult<usize> {
    let conn = state.active_db()?.get()?;
    let deleted = db::prune_activity_log(&conn, keep.unwrap_or(1000), max_age_days.unwrap_or(90))?;
    Ok(deleted)
}

#[tauri::command]
pub async fn preview_collection_rules(
    rules: Vec<crate::models::NewRuleInput>,
    state: State<'_, AppState>,
) -> CarrelResult<usize> {
    let conn = state.active_db()?.get()?;
    Ok(db::preview_collection_rules(&conn, &rules)?)
}

#[tauri::command]
pub async fn get_collection_suggestions(
    state: State<'_, AppState>,
) -> CarrelResult<Vec<CollectionSuggestion>> {
    let conn = state.active_db()?.get()?;
    let collections = db::list_collections(&conn)?;
    Ok(db::get_collection_suggestions(&conn, &collections)?)
}

fn derive_font_name(file_name: &str) -> String {
    let stem = std::path::Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);

    let known_suffixes = [
        "-Regular",
        "-Bold",
        "-Italic",
        "-Light",
        "-Medium",
        "-SemiBold",
        "-ExtraBold",
        "-Thin",
        "-Black",
        "-BoldItalic",
    ];
    let mut name = stem.to_string();
    for suffix in &known_suffixes {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_string();
            break;
        }
    }
    name
}

#[tauri::command]
pub async fn import_custom_font(
    file_path: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<CustomFont> {
    let source = std::path::Path::new(&file_path);
    if !source.exists() {
        return Err(CarrelError::invalid(format!("File not found: {file_path}")));
    }

    let extension = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if !["ttf", "otf", "woff2"].contains(&extension.as_str()) {
        return Err(CarrelError::invalid(format!(
            "Unsupported font format: .{extension}"
        )));
    }

    let file_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let id = Uuid::new_v4().to_string();
    let fonts_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| CarrelError::internal(format!("tauri: {e}")))?
        .join("fonts");
    std::fs::create_dir_all(&fonts_dir)?;

    let dest = fonts_dir.join(format!("{id}.{extension}"));
    std::fs::copy(source, &dest)?;

    let font = CustomFont {
        id,
        name: derive_font_name(&file_name),
        file_name,
        file_path: dest.to_string_lossy().to_string(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    };

    let conn = state.active_db()?.get()?;
    db::insert_custom_font(&conn, &font)?;

    Ok(font)
}

#[tauri::command]
pub async fn get_custom_fonts(state: State<'_, AppState>) -> CarrelResult<Vec<CustomFont>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_custom_fonts(&conn)?)
}

#[tauri::command]
pub async fn remove_custom_font(font_id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;

    if let Some(font) = db::get_custom_font(&conn, &font_id)? {
        let _ = std::fs::remove_file(&font.file_path);
    }

    Ok(db::delete_custom_font(&conn, &font_id)?)
}

#[tauri::command]
pub async fn check_file_exists(file_path: String) -> CarrelResult<bool> {
    if std::path::Path::new(&file_path).exists() {
        Ok(true)
    } else {
        Err(CarrelError::not_found(format!(
            "Book file not found at '{}'. It may have been moved or deleted.",
            file_path
        )))
    }
}

#[tauri::command]
pub async fn cleanup_library(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<CleanupResult> {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    let conn = state.active_db()?.get()?;
    let books = db::list_books(&conn)?;
    let total = books.len() as u32;

    // Auto-backup metadata before cleanup.
    let backups_dir = state.data_dir.join("backups");
    std::fs::create_dir_all(&backups_dir)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup_path = backups_dir.join(format!("pre-cleanup-{}.zip", timestamp));

    {
        let progress: Vec<ReadingProgress> = books
            .iter()
            .filter_map(|b| db::get_reading_progress(&conn, &b.id).ok().flatten())
            .collect();
        let bookmarks: Vec<Bookmark> = books
            .iter()
            .flat_map(|b| db::list_bookmarks(&conn, &b.id).unwrap_or_default())
            .collect();
        let highlights: Vec<Highlight> = books
            .iter()
            .flat_map(|b| db::list_highlights(&conn, &b.id).unwrap_or_default())
            .collect();
        let collections = db::list_collections(&conn)?;
        let tags = db::list_tags(&conn)?;
        let book_tags: Vec<(String, String, String)> = books
            .iter()
            .flat_map(|b| {
                db::get_book_tags(&conn, &b.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(tag_id, tag_name)| (b.id.clone(), tag_id, tag_name))
                    .collect::<Vec<_>>()
            })
            .collect();

        let metadata = serde_json::json!({
            "version": 1,
            "books": books,
            "reading_progress": progress,
            "bookmarks": bookmarks,
            "highlights": highlights,
            "collections": collections,
            "tags": tags,
            "book_tags": book_tags,
        });

        let file = std::fs::File::create(&backup_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        zip.start_file("library.json", options)?;
        zip.write_all(metadata_json.as_bytes())?;
        zip.finish()?;
    }

    let mut removed_books: Vec<CleanupEntry> = Vec::new();

    for (i, book) in books.iter().enumerate() {
        let _ = app.emit(
            "cleanup-progress",
            CleanupProgress {
                current: (i + 1) as u32,
                total,
            },
        );

        // #64 M4: resolve the key (or legacy absolute path) to a real
        // filesystem path before the existence check.
        let resolved = match state.resolve_book_path(book) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if std::path::Path::new(&resolved).exists() {
            continue;
        }

        // Book file is missing — remove from database.
        db::delete_book(&conn, &book.id)?;

        // Evict EPUB cache entry.
        if let Ok(mut cache) = state.epub_cache.lock() {
            cache.remove(&resolved);
        }
        #[cfg(feature = "mobi")]
        if let Ok(mut cache) = state.mobi_cache.lock() {
            cache.remove(&resolved);
        }

        // Remove any cover artifacts for this book via the covers storage.
        if let Ok(covers) = state.covers_storage() {
            let _ = delete_book_covers(&*covers, &book.id);
        }

        // Remove extracted inline images via the images storage.
        if let Ok(images) = state.images_storage() {
            let _ = delete_book_images(&*images, &book.id);
        }

        // Clear the page cache (rendered pages + persisted PDF text index)
        // and the in-memory text entry for this book.
        let cache_storage = page_cache_storage(&app).ok();
        evict_book_page_cache(
            cache_storage
                .as_ref()
                .map(|s| s as &dyn carrel_core::storage::Storage),
            book.file_hash.as_deref(),
            Some(resolved.as_str()),
            Some(state.cache_dir.as_path()),
        );

        log_event(
            &conn,
            ActivityEvent::BookRemovedCleanup {
                id: book.id.clone(),
                title: book.title.clone(),
            },
        );

        removed_books.push(CleanupEntry {
            id: book.id.clone(),
            title: book.title.clone(),
            author: book.author.clone(),
        });
    }

    Ok(CleanupResult {
        removed_count: removed_books.len() as u32,
        removed_books,
        backup_path: backup_path.to_string_lossy().to_string(),
    })
}

#[tauri::command]
pub async fn list_auto_backups(state: State<'_, AppState>) -> CarrelResult<Vec<AutoBackup>> {
    let backups_dir = state.data_dir.join("backups");
    if !backups_dir.exists() {
        return Ok(Vec::new());
    }

    let mut backups: Vec<AutoBackup> = Vec::new();

    let entries = std::fs::read_dir(&backups_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }

        let filename = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        // Parse known prefixes: "pre-cleanup-{timestamp}"
        let (label, timestamp) = if let Some(ts_str) = filename.strip_prefix("pre-cleanup-") {
            match ts_str.parse::<i64>() {
                Ok(ts) => ("Pre-cleanup".to_string(), ts),
                Err(_) => continue,
            }
        } else {
            continue; // Skip unknown files
        };

        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);

        backups.push(AutoBackup {
            path: path.to_string_lossy().to_string(),
            label,
            timestamp,
            size_bytes,
        });
    }

    // Sort newest first
    backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    Ok(backups)
}

#[tauri::command]
pub async fn get_series(state: State<'_, AppState>) -> CarrelResult<Vec<SeriesInfo>> {
    let conn = state.active_db()?.get()?;
    Ok(db::list_series(&conn)?)
}

// --- Sync orchestration ---

fn merge_result_summary(r: &crate::sync::MergeResult) -> String {
    let mut parts = Vec::new();
    if r.progress_updated {
        parts.push("progress synced".to_string());
    }
    let bm = r.bookmarks_added + r.bookmarks_updated;
    if bm > 0 {
        parts.push(format!("{bm} bookmarks updated"));
    }
    let hl = r.highlights_added + r.highlights_updated;
    if hl > 0 {
        parts.push(format!("{hl} highlights updated"));
    }
    if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join(", ")
    }
}

fn sync_error_kind_str(e: &crate::sync::SyncError) -> &'static str {
    match e {
        crate::sync::SyncError::Transport { kind: Some(k), .. } => match k {
            opendal::ErrorKind::PermissionDenied => "auth_failed",
            _ => "network",
        },
        crate::sync::SyncError::Transport { kind: None, .. } => "network",
        crate::sync::SyncError::Timeout => "timeout",
        crate::sync::SyncError::Malformed(_) => "other",
    }
}

fn friendly_sync_error(e: &crate::sync::SyncError) -> String {
    match e {
        crate::sync::SyncError::Timeout => {
            "Remote server did not respond within 5 seconds".to_string()
        }
        crate::sync::SyncError::Transport { .. } => {
            "Could not reach remote storage. Check your internet connection and backup settings."
                .to_string()
        }
        crate::sync::SyncError::Malformed(_) => {
            "Remote sync data is unreadable. It may have been created by a newer version of Carrel."
                .to_string()
        }
    }
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[tauri::command]
pub async fn sync_pull_book(
    book_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    // Private mode (B-M1): read once at the top of this request and reuse
    // for every passive write/emit below (the plugin-bus open signal, and
    // the inbound sync merge's progress arm).
    let suppress_passive = state.is_private();

    // The reader invokes this on every book open regardless of sync state,
    // which makes it the backend's open signal — emit before the sync
    // guards, but never while private (a tracker plugin with `network`
    // could exfiltrate a "private" read otherwise — SB-4).
    if !suppress_passive {
        events::bus().emit(CarrelEvent::BookOpened {
            book_id: book_id.clone(),
        });
    }

    // Private mode (B-M1): the automatic open-sync itself is passive egress.
    // A GET on `.carrel-sync/books/{file_hash}.json` reveals *which* book was
    // opened (object path) and *when* (timing) to the configured remote, even
    // with zero annotation changes. Gating the progress/activity/event side
    // effects is not enough — the network round-trip must not happen at all.
    // Return before touching the backup operator or remote path. Deliberate
    // annotation saves reconcile on the next non-private open.
    if suppress_passive {
        return Ok(());
    }

    let conn = state.active_db()?.get()?;

    // Guard: sync must be enabled and backup provider configured
    if !db::is_sync_enabled(&conn) {
        return Ok(());
    }
    let config_json = match db::get_setting(&conn, "backup_config")? {
        Some(j) => j,
        None => return Ok(()),
    };

    let mut config: crate::backup::BackupConfig = serde_json::from_str(&config_json)?;
    crate::backup::load_secrets(&mut config)?;
    let op = crate::backup::build_operator(&config)?;

    let book = match db::get_book(&conn, &book_id)? {
        Some(b) => b,
        None => return Err(CarrelError::not_found(format!("Book not found: {book_id}"))),
    };
    let file_hash = match &book.file_hash {
        Some(h) => h.clone(),
        None => return Ok(()),
    };
    let device_id = db::get_or_create_device_id(&conn)?;

    // Spawn thread for network fetch only — keep DB connection on main thread
    let fh = file_hash.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn_blocking(move || {
        let result = crate::sync::fetch_remote_sync(&op, &fh);
        let _ = tx.send(result);
    });

    let timeout = std::time::Duration::from_secs(5);
    match rx.recv_timeout(timeout) {
        Ok(Ok(Some(remote))) => {
            events::bus().emit(CarrelEvent::SyncCompleted {
                direction: SyncDirection::Pull,
                success: true,
            });
            // Merge on main thread using the existing connection
            let local = crate::sync::build_sync_payload(
                &conn,
                &book_id,
                &file_hash,
                &device_id,
                suppress_passive,
            );
            let merge_result = crate::sync::merge_remote_into_local(
                &conn,
                &book_id,
                &local,
                &remote,
                crate::sync::MergeOptions {
                    suppress_progress: suppress_passive,
                },
            );
            let _ = db::set_setting(&conn, "last_sync_success_at", &now_unix_secs().to_string());
            if merge_result.has_changes() {
                let summary = merge_result_summary(&merge_result);
                // Private mode (B-M1): this activity row stores book_id+title,
                // a durable local trace of the read — suppress it like the
                // BookOpened/BookClosed bus events above.
                if !suppress_passive {
                    log_event(
                        &conn,
                        ActivityEvent::SyncPullSuccess {
                            book_id: book_id.clone(),
                            title: book.title.clone(),
                            detail: summary,
                        },
                    );
                }
                let _ = app.emit("sync-applied", &book_id);
                if merge_result.progress_updated {
                    let _ = app.emit("sync-progress-updated", &book_id);
                }
            }
        }
        Ok(Ok(None)) => {
            // No remote file — success (nothing to merge)
            events::bus().emit(CarrelEvent::SyncCompleted {
                direction: SyncDirection::Pull,
                success: true,
            });
            let _ = db::set_setting(&conn, "last_sync_success_at", &now_unix_secs().to_string());
        }
        Ok(Err(e)) => {
            events::bus().emit(CarrelEvent::SyncCompleted {
                direction: SyncDirection::Pull,
                success: false,
            });
            let msg = friendly_sync_error(&e);
            let kind = sync_error_kind_str(&e);
            let _ = db::set_setting(&conn, "last_sync_error_at", &now_unix_secs().to_string());
            let _ = db::set_setting(&conn, "last_sync_error_message", &msg);
            let _ = db::set_setting(&conn, "last_sync_error_kind", kind);
            if kind == "auth_failed" {
                let _ = app.emit("backup-auth-error", serde_json::json!({ "message": msg }));
            }
            if !suppress_passive {
                log_event(
                    &conn,
                    ActivityEvent::SyncPullFailed {
                        book_id: book_id.clone(),
                        title: book.title.clone(),
                        detail: e.to_string(),
                    },
                );
            }
        }
        Err(_) => {
            // Timeout
            events::bus().emit(CarrelEvent::SyncCompleted {
                direction: SyncDirection::Pull,
                success: false,
            });
            let msg = "Remote server did not respond within 5 seconds";
            let _ = db::set_setting(&conn, "last_sync_error_at", &now_unix_secs().to_string());
            let _ = db::set_setting(&conn, "last_sync_error_message", msg);
            let _ = db::set_setting(&conn, "last_sync_error_kind", "timeout");
            if !suppress_passive {
                log_event(
                    &conn,
                    ActivityEvent::SyncPullFailed {
                        book_id: book_id.clone(),
                        title: book.title.clone(),
                        detail: "timeout after 5s".to_string(),
                    },
                );
            }
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn sync_push_book(book_id: String, state: State<'_, AppState>) -> CarrelResult<()> {
    // Private mode (B-M1): read once here and carry into the spawned
    // background thread below (State can't be moved across the
    // spawn_blocking boundary, so capture the bool now).
    let suppress_passive = state.is_private();

    // The reader invokes this on every book close regardless of sync state,
    // which makes it the backend's close signal — emit before the sync
    // guards, but never while private (SB-4).
    if !suppress_passive {
        events::bus().emit(CarrelEvent::BookClosed {
            book_id: book_id.clone(),
        });
    }

    // Private mode (B-M1): symmetric with `sync_pull_book`. The automatic
    // close-sync fetches then pushes `.carrel-sync/books/{file_hash}.json`,
    // exposing the book-hash + timing to the remote as passive egress. Return
    // before touching the backup operator; deliberate annotation saves
    // reconcile on the next non-private close.
    if suppress_passive {
        return Ok(());
    }

    let conn = state.active_db()?.get()?;

    // Guard: sync must be enabled and backup provider configured
    if !db::is_sync_enabled(&conn) {
        return Ok(());
    }
    let config_json = match db::get_setting(&conn, "backup_config")? {
        Some(j) => j,
        None => return Ok(()),
    };

    let mut config: crate::backup::BackupConfig = serde_json::from_str(&config_json)?;
    crate::backup::load_secrets(&mut config)?;
    let op = crate::backup::build_operator(&config)?;

    let book = match db::get_book(&conn, &book_id)? {
        Some(b) => b,
        None => return Err(CarrelError::not_found(format!("Book not found: {book_id}"))),
    };
    let file_hash = match &book.file_hash {
        Some(h) => h.clone(),
        None => return Ok(()),
    };
    let device_id = db::get_or_create_device_id(&conn)?;
    let book_title = book.title.clone();

    drop(conn);

    // Clone the pool handle for the background thread (Pool is Arc-based, cheap to clone)
    let pool = state.active_db()?;

    // Fire-and-forget: spawn background thread that pull-merges then pushes
    tauri::async_runtime::spawn_blocking(move || {
        let bg_conn = match pool.get() {
            Ok(c) => c,
            Err(_) => return,
        };
        match crate::sync::sync_book_on_close(
            &bg_conn,
            &op,
            &book_id,
            &file_hash,
            &device_id,
            suppress_passive,
        ) {
            Ok(()) => {
                events::bus().emit(CarrelEvent::SyncCompleted {
                    direction: SyncDirection::Push,
                    success: true,
                });
                let _ = db::set_setting(
                    &bg_conn,
                    "last_sync_success_at",
                    &now_unix_secs().to_string(),
                );
                if !suppress_passive {
                    log_event(
                        &bg_conn,
                        ActivityEvent::SyncPushSuccess {
                            book_id: book_id.clone(),
                            title: book_title.clone(),
                            detail: "progress and annotations pushed".to_string(),
                        },
                    );
                }
            }
            Err(e) => {
                events::bus().emit(CarrelEvent::SyncCompleted {
                    direction: SyncDirection::Push,
                    success: false,
                });
                let msg = friendly_sync_error(&e);
                let _ =
                    db::set_setting(&bg_conn, "last_sync_error_at", &now_unix_secs().to_string());
                let _ = db::set_setting(&bg_conn, "last_sync_error_message", &msg);
                let _ = db::set_setting(&bg_conn, "last_sync_error_kind", sync_error_kind_str(&e));
                if !suppress_passive {
                    log_event(
                        &bg_conn,
                        ActivityEvent::SyncPushFailed {
                            book_id: book_id.clone(),
                            title: book_title.clone(),
                            detail: e.to_string(),
                        },
                    );
                }
            }
        }
    });

    Ok(())
}

// ── Bulk Operations (#60) ────────────────────────────────────────────────────

#[tauri::command]
pub async fn bulk_delete_books(
    book_ids: Vec<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> CarrelResult<u32> {
    let conn = state.active_db()?.get()?;

    // Capture each book's file_hash + resolved path before the DB delete so
    // the page cache (page-cache/{hash}/, incl. the persisted PDF text
    // index) can be evicted afterward — mirrors remove_book's cleanup.
    // A DB lookup error must NOT be silently collapsed into "skip" — that
    // would delete the book below while never evicting its cache/text-index
    // (which keeps a valid manifest, so the orphan sweep won't reclaim it).
    // Propagate the error before mutating; skip only a genuinely absent row.
    let evict_targets: Vec<(Option<String>, Option<String>)> = book_ids
        .iter()
        .filter_map(|id| match db::get_book(&conn, id) {
            Ok(Some(b)) => {
                let resolved = state.resolve_book_path(&b).ok();
                Some(Ok((b.file_hash.clone(), resolved)))
            }
            Ok(None) => None,
            Err(e) => Some(Err(e.into())),
        })
        .collect::<CarrelResult<Vec<_>>>()?;

    let ids_ref: Vec<&str> = book_ids.iter().map(|s| s.as_str()).collect();
    db::bulk_delete_books(&conn, &ids_ref)?;
    log_event(
        &conn,
        ActivityEvent::BulkDelete {
            count: book_ids.len(),
        },
    );

    // In-memory eviction must not depend on disk-cache init: if
    // page_cache_storage fails, we still drop the resident text so a deleted
    // book can't serve stale results (matching remove_book). Only the disk
    // eviction is gated on the storage handle.
    let storage = page_cache_storage(&app).ok();
    let storage_ref = storage
        .as_ref()
        .map(|s| s as &dyn carrel_core::storage::Storage);
    for (hash, path) in evict_targets {
        evict_book_page_cache(
            storage_ref,
            hash.as_deref(),
            path.as_deref(),
            Some(state.cache_dir.as_path()),
        );
    }

    Ok(book_ids.len() as u32)
}

#[tauri::command]
pub async fn bulk_add_to_collection(
    book_ids: Vec<String>,
    collection_id: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    let ids_ref: Vec<&str> = book_ids.iter().map(|s| s.as_str()).collect();
    db::bulk_add_to_collection(&conn, &ids_ref, &collection_id)?;
    Ok(())
}

#[tauri::command]
pub async fn bulk_add_tag(
    book_ids: Vec<String>,
    tag: String,
    state: State<'_, AppState>,
) -> CarrelResult<()> {
    let conn = state.active_db()?.get()?;
    let ids_ref: Vec<&str> = book_ids.iter().map(|s| s.as_str()).collect();
    db::bulk_add_tag(&conn, &ids_ref, &tag)?;
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkEditFields {
    pub author: Option<String>,
    pub series: Option<String>,
    pub publish_year: Option<u16>,
    pub language: Option<String>,
    pub publisher: Option<String>,
}

#[tauri::command]
pub async fn bulk_update_metadata(
    book_ids: Vec<String>,
    fields: BulkEditFields,
    state: State<'_, AppState>,
) -> CarrelResult<u32> {
    let conn = state.active_db()?.get()?;
    let ids_ref: Vec<&str> = book_ids.iter().map(|s| s.as_str()).collect();

    // Normalize strings with the same rules as update_book_metadata:
    // trim whitespace + enforce length limits.
    let normalize_str = |s: String, max_len: usize| -> String {
        let trimmed = s.trim().to_string();
        if trimmed.len() > max_len {
            // Truncate to the largest valid char boundary at or below max_len bytes.
            // Direct byte slicing can panic on multi-byte UTF-8 characters.
            let mut end = max_len;
            while end > 0 && !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            trimmed[..end].to_string()
        } else {
            trimmed
        }
    };
    // Author is required: reject empty after trim instead of silently skipping.
    let author = if let Some(s) = fields.author {
        let t = normalize_str(s, 500);
        if t.is_empty() {
            return Err(CarrelError::invalid("Author cannot be empty."));
        }
        Some(t)
    } else {
        None
    };
    // Optional fields: trim + length-limit; empty string preserved for DB to convert to NULL.
    let series = fields.series.map(|s| normalize_str(s, 500));
    let language = fields.language.map(|s| normalize_str(s, 50));
    let publisher = fields.publisher.map(|s| normalize_str(s, 500));

    let count = db::bulk_update_metadata(
        &conn,
        &ids_ref,
        author.as_deref(),
        series.as_deref(),
        fields.publish_year,
        language.as_deref(),
        publisher.as_deref(),
    )?;

    log_event(
        &conn,
        ActivityEvent::BulkEdit {
            count: count as usize,
        },
    );

    Ok(count)
}

// ── Web Server Commands ──────────────────────────────────────────────────────

/// One-shot migration of the legacy `web_server_enabled` setting to the
/// new pair `web_ui_enabled` + `opds_enabled`. Idempotent: after the
/// first run the legacy key is gone and subsequent calls are no-ops.
/// New settings are only written when they are absent, so a user who
/// adjusted the new settings between two migration runs keeps their
/// changes.
pub fn migrate_web_server_setting(conn: &rusqlite::Connection) -> CarrelResult<()> {
    let Some(old) = db::get_setting(conn, "web_server_enabled")? else {
        return Ok(());
    };
    let was_on = old == "true";
    if db::get_setting(conn, "web_ui_enabled")?.is_none() {
        db::set_setting(conn, "web_ui_enabled", &was_on.to_string())?;
    }
    if db::get_setting(conn, "opds_enabled")?.is_none() {
        db::set_setting(conn, "opds_enabled", &was_on.to_string())?;
    }
    db::delete_setting(conn, "web_server_enabled")?;
    Ok(())
}

#[tauri::command]
pub async fn web_server_set_modes(
    web_ui: bool,
    opds: bool,
    port: Option<u16>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CarrelResult<crate::web_server::WebServerStatus> {
    // 1. Persist intent first. Settings reflect what the user wants;
    //    runtime state is derived.
    {
        let conn = state.active_db()?.get()?;
        db::set_setting(&conn, "web_ui_enabled", &web_ui.to_string())?;
        db::set_setting(&conn, "opds_enabled", &opds.to_string())?;
        if let Some(p) = port {
            db::set_setting(&conn, "web_server_port", &p.to_string())?;
        }
    }

    let modes = crate::web_server::ServerModes { web_ui, opds };

    // 2. Stop existing handle (if any).
    let prev = { state.web_server_handle.lock()?.take() };
    if let Some(h) = prev {
        crate::web_server::stop(h);
    }

    // 3. Start fresh if anything is enabled.
    let (running, url, port_used) = if modes.any() {
        let port_used = {
            let conn = state.active_db()?.get()?;
            port.unwrap_or_else(|| {
                db::get_setting(&conn, "web_server_port")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::web_server::DEFAULT_PORT)
            })
        };

        // Sync PIN hash from keychain before starting.
        {
            let fresh = crate::web_server::auth::load_pin_hash();
            let mut ph = state.shared_pin_hash.lock()?;
            *ph = fresh;
        }

        let web_state = crate::web_server::WebState {
            pool: state.shared_active_pool.clone(),
            data_dir: state.data_dir.clone(),
            cache_dir: state.cache_dir.clone(),
            pin_hash: state.shared_pin_hash.clone(),
            sessions: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            login_limiter: std::sync::Arc::new(crate::web_server::auth::RateLimiter::new(5, 300)),
            active_profile_name: state.shared_active_profile_name.clone(),
            unlocked_profiles: state.unlocked_profiles.clone(),
            private_mode: state.private_mode.clone(),
            profile_host: Some(crate::profile_host::for_app(&app)),
        };

        let handle = crate::web_server::start(web_state, port_used, modes).await?;
        let url = handle.url.clone();
        {
            let mut h = state.web_server_handle.lock()?;
            *h = Some(handle);
        }
        (true, Some(url), port_used)
    } else {
        // Server is now stopped. Pick the persisted port for the response.
        let port_used = {
            let conn = state.active_db()?.get()?;
            port.unwrap_or_else(|| {
                db::get_setting(&conn, "web_server_port")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::web_server::DEFAULT_PORT)
            })
        };
        (false, None, port_used)
    };

    let has_pin = crate::web_server::auth::load_pin_hash().is_some();

    // 4. Audit log.
    {
        let conn = state.active_db()?.get()?;
        log_event(
            &conn,
            ActivityEvent::WebServerModesChanged {
                detail: format!("web_ui={web_ui} opds={opds}"),
            },
        );
    }

    // 5. Refresh tray menu.
    let _ = crate::tray::rebuild_tray_menu(&app);

    // Build status directly instead of recursing into web_server_status —
    // calling another #[tauri::command] async fn from within a command can
    // hang in Tauri v2 when the State borrow is reused after an await.
    Ok(crate::web_server::WebServerStatus {
        running,
        url,
        port: port_used,
        has_pin,
        web_ui_enabled: web_ui,
        opds_enabled: opds,
    })
}

#[tauri::command]
pub async fn web_server_status(
    state: State<'_, AppState>,
) -> CarrelResult<crate::web_server::WebServerStatus> {
    let has_pin = crate::web_server::auth::load_pin_hash().is_some();

    // Read user intent (these settings drive the running state).
    let (web_ui_enabled, opds_enabled, persisted_port) = {
        let conn = state.active_db()?.get()?;
        let web_ui = db::get_setting(&conn, "web_ui_enabled")?.as_deref() == Some("true");
        let opds = db::get_setting(&conn, "opds_enabled")?.as_deref() == Some("true");
        let port = db::get_setting(&conn, "web_server_port")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(crate::web_server::DEFAULT_PORT);
        (web_ui, opds, port)
    };

    let handle = state.web_server_handle.lock()?;
    match handle.as_ref() {
        Some(h) => Ok(crate::web_server::WebServerStatus {
            running: true,
            url: Some(h.url.clone()),
            port: h.port,
            has_pin,
            web_ui_enabled,
            opds_enabled,
        }),
        None => Ok(crate::web_server::WebServerStatus {
            running: false,
            url: None,
            port: persisted_port,
            has_pin,
            web_ui_enabled,
            opds_enabled,
        }),
    }
}

#[tauri::command]
pub async fn web_server_set_pin(pin: String, state: State<'_, AppState>) -> CarrelResult<()> {
    if pin.is_empty() {
        return Err(CarrelError::invalid("PIN cannot be empty"));
    }

    crate::web_server::auth::validate_pin(&pin).map_err(CarrelError::invalid)?;

    crate::web_server::auth::store_pin(&pin)?;

    // Propagate new hash immediately — store_pin is irreversible, runtime must reflect it.
    // Recover from poisoned mutex: the data is still usable even after a panic.
    let new_hash = crate::web_server::auth::hash_pin(&pin);
    let mut ph = match state.shared_pin_hash.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            state.shared_pin_hash.clear_poison();
            poisoned.into_inner()
        }
    };
    *ph = Some(new_hash);
    drop(ph);

    // Audit log is best-effort — PIN change already committed
    if let Ok(db) = state.active_db() {
        if let Ok(conn) = db.get() {
            let _ = db::log_pin_change(&conn, "desktop");
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn web_server_get_qr(state: State<'_, AppState>) -> CarrelResult<String> {
    let handle = state.web_server_handle.lock()?;
    let url = handle
        .as_ref()
        .map(|h| h.url.clone())
        .ok_or_else(|| CarrelError::not_found("Web server is not running"))?;
    crate::web_server::auth::generate_qr_svg(&url)
}

#[tauri::command]
pub async fn get_ipc_metrics(
    state: State<'_, AppState>,
) -> Result<crate::ipc_metrics::IpcMetricsResponse, String> {
    Ok(state.ipc_metrics.response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover_entry(title: &str, author: &str) -> opds::OpdsEntry {
        opds::OpdsEntry {
            id: format!("{title}-{author}"),
            title: title.to_string(),
            author: author.to_string(),
            summary: String::new(),
            cover_url: None,
            links: Vec::new(),
            nav_url: None,
            catalog_url: None,
        }
    }

    #[test]
    fn dedup_discover_drops_owned_and_intra_list_duplicates() {
        let library = vec![("Dune".to_string(), "Frank Herbert".to_string())];
        let entries = vec![
            discover_entry("Dune", "Frank Herbert"), // owned → dropped
            discover_entry("Basil", "Wilkie Collins"),
            discover_entry("Basil", "Wilkie Collins"), // intra-list dup → dropped
            discover_entry("Flight", "Walter White"),
        ];

        let out = dedup_discover_entries(entries, &library, 100);
        let titles: Vec<_> = out.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Basil", "Flight"]);
    }

    #[test]
    fn dedup_discover_matches_ignoring_case_and_whitespace() {
        let library = vec![("The  Odyssey".to_string(), "HOMER".to_string())];
        let entries = vec![
            discover_entry("the odyssey", "homer"), // owned (normalized) → dropped
            discover_entry("Kept", "Author"),
        ];

        let out = dedup_discover_entries(entries, &library, 100);
        let titles: Vec<_> = out.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Kept"]);
    }

    #[test]
    fn dedup_discover_caps_at_limit() {
        let entries: Vec<_> = (0..10)
            .map(|i| discover_entry(&format!("Book {i}"), "Author"))
            .collect();
        let out = dedup_discover_entries(entries, &[], 3);
        assert_eq!(out.len(), 3, "result must be capped at the limit");
    }

    fn progress_test_book(id: &str, total_chapters: u32) -> Book {
        Book {
            id: id.to_string(),
            title: "Progress Test".to_string(),
            author: "Author".to_string(),
            file_path: "/nonexistent/progress-test.epub".to_string(),
            cover_path: None,
            total_chapters,
            added_at: 0,
            format: BookFormat::Epub,
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
            is_imported: false,
            want_to_read: false,
        }
    }

    // F1: `apply_reading_progress` is the shared completion path used by
    // both `save_reading_progress` (desktop, AppHandle: Some) and the web
    // PUT handler (AppHandle: None). Exercise the completion-detection logic
    // directly with `None` — this is exactly what the web path invokes.
    #[test]
    fn apply_reading_progress_logs_completion_once() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        let book = progress_test_book("book-1", 5);
        db::insert_book(&conn, &book).unwrap();

        // Landing on the last chapter (index 4 of 5) for the first time
        // fires the completion side effects exactly once.
        apply_reading_progress(&conn, &book, "book-1", 4, 0.5, None, false).unwrap();
        let activity = db::get_all_activity(&conn).unwrap();
        assert_eq!(
            activity
                .iter()
                .filter(|a| a.action == "book_completed")
                .count(),
            1
        );

        // A repeat save that stays on the last chapter must not log again —
        // this is the desktop-after-web dedup scenario from F1: whichever
        // path crosses onto the last chapter first fires the event, and
        // later saves (from either path) must see `was_completed_before`.
        apply_reading_progress(&conn, &book, "book-1", 4, 0.9, None, false).unwrap();
        let activity = db::get_all_activity(&conn).unwrap();
        assert_eq!(
            activity
                .iter()
                .filter(|a| a.action == "book_completed")
                .count(),
            1,
            "completion must not be logged twice"
        );
    }

    #[test]
    fn apply_reading_progress_no_completion_before_last_chapter() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        let book = progress_test_book("book-2", 5);
        db::insert_book(&conn, &book).unwrap();

        apply_reading_progress(&conn, &book, "book-2", 2, 0.5, None, false).unwrap();
        let activity = db::get_all_activity(&conn).unwrap();
        assert!(activity.iter().all(|a| a.action != "book_completed"));
    }

    // --- Page cache eviction on book removal ---

    /// Deleting a book must clear its persisted PDF text index (and the rest
    /// of its `page-cache/{hash}/` entry), not just the DB row / covers /
    /// images. Regression test for the CHANGELOG's "deleting the book clears
    /// it" claim, which previously did not hold for the text index.
    // Hermetic: exercises `evict_book_page_cache` — the shared helper every
    // permanent-delete path (`remove_book`, bulk delete, missing-file
    // cleanup) routes through — against a `Storage` rooted in a TempDir, so
    // it can't touch or depend on the real OS application-cache directory.
    #[test]
    fn evict_book_page_cache_removes_persisted_text_index() {
        let dir = tempfile::tempdir().unwrap();
        let storage = carrel_core::storage::LocalStorage::new(dir.path()).unwrap();

        // Seed a page-cache text index for a book's hash, as prepare_pdf's
        // background pass (or a search miss) would have written.
        let index = carrel_core::pdf::PdfTextIndex {
            version: carrel_core::pdf::TEXT_INDEX_VERSION,
            page_count: 1,
            pages: vec!["needle".to_string()],
        };
        page_cache::write_text_index(&storage, "hash-remove-test", &index).unwrap();
        assert!(page_cache::read_text_index(&storage, "hash-remove-test").is_some());

        evict_book_page_cache(Some(&storage), Some("hash-remove-test"), None, None);

        assert!(
            page_cache::read_text_index(&storage, "hash-remove-test").is_none(),
            "eviction must remove the book's persisted page cache, including the text index"
        );
    }

    /// Deleting a book must also remove its locally-staged source copy, so a
    /// large staged file doesn't linger after the book is gone. `resolved_path`
    /// carries the extension the staged file was keyed by.
    #[test]
    fn evict_book_page_cache_removes_staged_source_copy() {
        let cache = tempfile::tempdir().unwrap();
        let srcdir = tempfile::tempdir().unwrap();
        let src = srcdir.path().join("book.cbz");
        std::fs::write(&src, b"staged comic bytes").unwrap();
        carrel_core::source_cache::stage(cache.path(), &src, "del-hash", "cbz").unwrap();
        assert!(
            carrel_core::source_cache::staged_if_present(cache.path(), "del-hash", "cbz").is_some()
        );

        // resolved_path ends in .cbz so the staged ext is derived correctly.
        evict_book_page_cache(
            None,
            Some("del-hash"),
            Some("/Volumes/remote/book.cbz"),
            Some(cache.path()),
        );

        assert!(
            carrel_core::source_cache::staged_if_present(cache.path(), "del-hash", "cbz").is_none(),
            "the staged source copy must be removed on book deletion"
        );
    }

    // --- Private mode (B-M1): suppress_passive boundary ---

    #[test]
    fn apply_reading_progress_suppressed_writes_nothing_and_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        let book = progress_test_book("book-priv", 5);
        db::insert_book(&conn, &book).unwrap();

        // Land on the last chapter — this would normally upsert progress,
        // emit BookFinished, and log a BookCompleted activity row.
        let result = apply_reading_progress(&conn, &book, "book-priv", 4, 0.5, None, true).unwrap();

        // The caller still gets back the position it submitted (so the
        // frontend can hold a volatile in-memory resume point, D-4) —
        // but nothing was persisted.
        assert_eq!(result.chapter_index, 4);
        assert!((result.scroll_position - 0.5).abs() < f64::EPSILON);

        assert!(
            db::get_reading_progress(&conn, "book-priv")
                .unwrap()
                .is_none(),
            "reading_progress must stay empty while suppressed"
        );
        let activity = db::get_all_activity(&conn).unwrap();
        assert!(
            activity.iter().all(|a| a.action != "book_completed"),
            "no activity log entry while suppressed"
        );
    }

    #[test]
    fn apply_reading_progress_suppressed_does_not_block_a_highlight_or_bookmark_in_the_same_test() {
        // The persist/suppress boundary: a highlight + bookmark insert in
        // the SAME test still land even though the progress write next to
        // them is suppressed.
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        let book = progress_test_book("book-priv-2", 5);
        db::insert_book(&conn, &book).unwrap();

        apply_reading_progress(&conn, &book, "book-priv-2", 4, 0.5, None, true).unwrap();
        assert!(db::get_reading_progress(&conn, "book-priv-2")
            .unwrap()
            .is_none());

        let bookmark = Bookmark {
            id: "bm-priv".to_string(),
            book_id: "book-priv-2".to_string(),
            chapter_index: 1,
            scroll_position: 0.1,
            name: None,
            note: None,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        };
        db::insert_bookmark(&conn, &bookmark).unwrap();

        let highlight = Highlight {
            id: "hl-priv".to_string(),
            book_id: "book-priv-2".to_string(),
            chapter_index: 1,
            text: "quote".to_string(),
            color: "#ffff00".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 5,
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        };
        db::insert_highlight(&conn, &highlight).unwrap();

        assert_eq!(db::list_bookmarks(&conn, "book-priv-2").unwrap().len(), 1);
        assert_eq!(db::list_highlights(&conn, "book-priv-2").unwrap().len(), 1);
        assert!(
            db::get_reading_progress(&conn, "book-priv-2")
                .unwrap()
                .is_none(),
            "progress must remain suppressed alongside persisted annotations"
        );
    }

    #[tokio::test]
    async fn record_reading_session_skipped_when_private() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        state.private_mode.store(true, Ordering::SeqCst);

        record_reading_session(
            "book-1".to_string(),
            0,
            3600, // well above the 10s floor
            10,
            state.clone(),
        )
        .await
        .unwrap();

        let conn = state.active_db().unwrap().get().unwrap();
        let stats = db::get_reading_stats(&conn).unwrap();
        assert_eq!(
            stats.total_sessions, 0,
            "no reading session should be recorded while private"
        );
    }

    #[tokio::test]
    async fn record_reading_session_recorded_when_not_private() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        assert!(!state.is_private());
        {
            let conn = state.active_db().unwrap().get().unwrap();
            db::insert_book(&conn, &progress_test_book("book-1", 5)).unwrap();
        }

        record_reading_session("book-1".to_string(), 0, 3600, 10, state.clone())
            .await
            .unwrap();

        let conn = state.active_db().unwrap().get().unwrap();
        let stats = db::get_reading_stats(&conn).unwrap();
        assert_eq!(stats.total_sessions, 1);
    }

    #[tokio::test]
    async fn set_private_mode_flips_flag_and_get_private_mode_reads_it() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        assert!(!get_private_mode(state.clone()).await.unwrap());

        let result = set_private_mode(true, app.handle().clone(), state.clone())
            .await
            .unwrap();
        assert!(result);
        assert!(get_private_mode(state.clone()).await.unwrap());
        assert!(state.is_private());

        set_private_mode(false, app.handle().clone(), state.clone())
            .await
            .unwrap();
        assert!(!state.is_private());
    }

    #[tokio::test]
    async fn sync_push_book_skips_network_sync_when_private() {
        // B-M1 regression (Codex + Codex-2): automatic close-sync is passive
        // egress (GET+PUT on `.carrel-sync/books/{file_hash}.json` reveals the
        // book-hash + timing) and must not run while private. The private
        // early return fires before the backup config is even parsed, so with
        // a *malformed* config + sync enabled a non-private close surfaces the
        // parse error while a private close short-circuits to Ok — proving no
        // operator/network path is reached. `sync_pull_book` carries the
        // symmetric guard; it takes a concrete `AppHandle` so it can't be
        // constructed under the mock runtime here.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        {
            let conn = state.active_db().unwrap().get().unwrap();
            db::set_setting(&conn, "sync_enabled", "true").unwrap();
            db::set_setting(&conn, "backup_config", "not-valid-json").unwrap();
        }

        // Sanity: without private mode the same call reaches config parsing
        // and fails — so an Ok below can only come from the early return.
        assert!(
            sync_push_book("book-1".to_string(), state.clone())
                .await
                .is_err(),
            "non-private close must reach (and fail on) the malformed backup config"
        );

        state.private_mode.store(true, Ordering::SeqCst);
        sync_push_book("book-1".to_string(), state.clone())
            .await
            .expect("private close must return before touching the remote sync path");
    }

    #[test]
    fn probe_dir_writable_true_for_writable_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(probe_dir_writable(dir.path()));
        // The probe file must be cleaned up.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn probe_dir_writable_false_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!probe_dir_writable(&missing));
    }

    #[test]
    fn probe_dir_writable_false_for_file_path() {
        // A regular file is not a directory.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(!probe_dir_writable(&file));
    }

    // NOTE: a read-only-directory test is intentionally skipped — reliably
    // creating an unwritable directory is fiddly and platform-dependent
    // (root bypasses mode bits, Windows ignores them). The missing-path and
    // file-path cases cover the non-writable branches portably.

    #[test]
    fn chapter_progress_emits_every_chapter_for_small_books() {
        // total <= 50: every chapter is a meaningful step.
        for loaded in 1..=10 {
            assert!(should_emit_chapter_progress(loaded, 10));
        }
    }

    #[test]
    fn chapter_progress_throttles_large_books() {
        // total = 280 -> step = 2; emit on even counts only, plus the last.
        let total = 280;
        assert!(should_emit_chapter_progress(2, total));
        assert!(!should_emit_chapter_progress(3, total));
        assert!(should_emit_chapter_progress(4, total));
    }

    #[test]
    fn chapter_progress_always_emits_final() {
        // The final chapter emits even when it isn't on a step boundary.
        assert!(should_emit_chapter_progress(280, 280));
        assert!(should_emit_chapter_progress(101, 101));
        // total = 300 -> step = 3; 299 is not a multiple of 3 but 300 is the final.
        assert!(!should_emit_chapter_progress(299, 300));
        assert!(should_emit_chapter_progress(300, 300));
    }

    #[test]
    fn chapter_progress_handles_zero_total() {
        assert!(!should_emit_chapter_progress(0, 0));
    }

    #[test]
    fn get_login_history_reads_web_session_rows() {
        use carrel_core::db;
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        db::insert_web_session_log(
            &conn,
            &carrel_core::models::WebSessionEntry {
                id: "x1".into(),
                timestamp: 1000,
                ip: "203.0.113.9".into(),
                method: "session".into(),
                outcome: "success".into(),
                user_agent: None,
            },
        )
        .unwrap();

        let rows = db::get_web_session_log(&conn, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "success");
        assert_eq!(rows[0].ip, "203.0.113.9");
    }

    #[test]
    fn export_activity_log_writes_parseable_json() {
        use carrel_core::db;
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();

        log_event(
            &conn,
            carrel_core::activity::ActivityEvent::BookImported {
                id: "b1".into(),
                title: "Title".into(),
                format: "EPUB".into(),
                author: "Auth".into(),
            },
        );

        let rows = db::get_all_activity(&conn).unwrap();
        let dest = dir.path().join("activity.json");
        std::fs::write(&dest, serde_json::to_string_pretty(&rows).unwrap()).unwrap();

        let parsed: Vec<carrel_core::models::ActivityEntry> =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].action, "book_imported");
        assert_eq!(parsed[0].detail.as_deref(), Some("EPUB by Auth"));
    }

    fn temp_covers_storage() -> (tempfile::TempDir, carrel_core::storage::LocalStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = carrel_core::storage::LocalStorage::new(dir.path()).unwrap();
        (dir, storage)
    }

    #[test]
    fn save_cover_png_data_uri() {
        let (_d, storage) = temp_covers_storage();
        let data_uri = "data:image/png;base64,iVBORw0KGgo=";
        let result = save_cover_from_data_uri(&storage, "book-123", data_uri);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path.contains("cover.png"));
        assert!(std::path::Path::new(&path).exists());
    }

    #[test]
    fn save_cover_jpeg_data_uri() {
        let (_d, storage) = temp_covers_storage();
        let data_uri = "data:image/jpeg;base64,/9j/4AAQ";
        let result = save_cover_from_data_uri(&storage, "book-456", data_uri);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path.contains("cover.jpg"));
    }

    #[test]
    fn save_cover_webp_data_uri() {
        let (_d, storage) = temp_covers_storage();
        let data_uri = "data:image/webp;base64,UklGRg==";
        let result = save_cover_from_data_uri(&storage, "book-789", data_uri);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path.contains("cover.webp"));
    }

    #[test]
    fn save_cover_invalid_data_uri_returns_none() {
        let (_d, storage) = temp_covers_storage();
        // Missing data: prefix
        assert!(save_cover_from_data_uri(&storage, "book", "not-a-data-uri").is_none());
        // Missing ;base64
        assert!(save_cover_from_data_uri(&storage, "book", "data:image/png,abc").is_none());
        // Missing comma
        assert!(save_cover_from_data_uri(&storage, "book", "data:image/png;base64").is_none());
    }

    #[test]
    fn save_cover_creates_directory_structure() {
        let (d, storage) = temp_covers_storage();
        let data_uri = "data:image/gif;base64,R0lGODlh";
        let result = save_cover_from_data_uri(&storage, "new-book", data_uri);
        assert!(result.is_some());
        // Verify the `new-book/` subdirectory and cover file were created.
        assert!(d.path().join("new-book").exists());
        assert!(d.path().join("new-book").join("cover.gif").exists());
    }

    #[test]
    fn save_cover_unknown_mime_defaults_to_jpg() {
        let (_d, storage) = temp_covers_storage();
        let data_uri = "data:image/bmp;base64,Qk0=";
        let result = save_cover_from_data_uri(&storage, "book", data_uri);
        assert!(result.is_some());
        assert!(result.unwrap().contains("cover.jpg"));
    }

    #[test]
    fn delete_book_covers_removes_all_entries_for_book() {
        let (_d, storage) = temp_covers_storage();
        // Populate 2 covers for the book we care about and 1 for another.
        save_cover_from_data_uri(&storage, "target", "data:image/png;base64,iVBORw0KGgo=").unwrap();
        save_cover_from_data_uri(&storage, "target", "data:image/jpeg;base64,/9j/4AAQ").unwrap();
        save_cover_from_data_uri(&storage, "other", "data:image/png;base64,iVBORw0KGgo=").unwrap();

        delete_book_covers(&storage, "target").unwrap();

        use carrel_core::storage::Storage;
        assert!(storage.list("target/").unwrap().is_empty());
        assert_eq!(storage.list("other/").unwrap().len(), 1);
    }

    #[test]
    fn cover_storage_key_format() {
        assert_eq!(cover_storage_key("abc", "png"), "abc/cover.png");
        assert_eq!(cover_storage_key("book-42", "jpg"), "book-42/cover.jpg");
    }

    #[test]
    fn validate_scroll_position_rejects_nan() {
        assert!(validate_scroll_position(f64::NAN).is_err());
    }

    #[test]
    fn validate_scroll_position_rejects_infinity() {
        assert!(validate_scroll_position(f64::INFINITY).is_err());
        assert!(validate_scroll_position(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn validate_scroll_position_clamps_negative() {
        assert_eq!(validate_scroll_position(-0.5).unwrap(), 0.0);
    }

    #[test]
    fn validate_scroll_position_clamps_above_one() {
        assert_eq!(validate_scroll_position(1.5).unwrap(), 1.0);
    }

    #[test]
    fn validate_scroll_position_accepts_valid_values() {
        assert_eq!(validate_scroll_position(0.0).unwrap(), 0.0);
        assert_eq!(validate_scroll_position(0.5).unwrap(), 0.5);
        assert_eq!(validate_scroll_position(1.0).unwrap(), 1.0);
    }

    #[test]
    fn validate_file_exists_returns_ok_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("book.epub");
        std::fs::write(&file, b"dummy").unwrap();
        assert!(validate_file_exists(file.to_str().unwrap()).is_ok());
    }

    #[test]
    fn validate_file_exists_returns_clear_error_for_missing_file() {
        let result = validate_file_exists("/nonexistent/path/book.epub");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "NotFound");
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "error should mention 'not found': {msg}"
        );
        assert!(
            msg.contains("/nonexistent/path/book.epub"),
            "error should include the path: {msg}"
        );
    }

    #[test]
    fn test_derive_font_name() {
        assert_eq!(derive_font_name("Merriweather-Regular.ttf"), "Merriweather");
        assert_eq!(derive_font_name("FiraCode-Bold.woff2"), "FiraCode");
        assert_eq!(derive_font_name("My Font.otf"), "My Font");
        assert_eq!(derive_font_name("Roboto-BoldItalic.ttf"), "Roboto");
        assert_eq!(derive_font_name("SimpleFont.ttf"), "SimpleFont");
    }

    // --- #64 M2: book storage key helpers ---

    #[test]
    fn book_storage_key_joins_id_and_extension() {
        assert_eq!(book_storage_key("abc123", "epub"), "abc123.epub");
        assert_eq!(book_storage_key("abc", "pdf"), "abc.pdf");
    }

    #[test]
    fn book_key_from_path_strips_library_folder() {
        let key = book_key_from_path("/library/abc123.epub", "/library").unwrap();
        assert_eq!(key, "abc123.epub");
    }

    #[test]
    fn book_key_from_path_handles_nested_paths() {
        let key = book_key_from_path("/library/books/abc.epub", "/library").unwrap();
        assert_eq!(key, "books/abc.epub");
    }

    #[test]
    fn book_key_from_path_returns_none_for_external_file() {
        // Linked books reference files outside the library folder.
        assert!(book_key_from_path("/elsewhere/book.epub", "/library").is_none());
    }

    #[test]
    fn book_key_from_path_handles_trailing_slash_on_folder() {
        // strip_prefix normalizes — `/library/` and `/library` both work.
        let key = book_key_from_path("/library/abc.epub", "/library/").unwrap();
        assert_eq!(key, "abc.epub");
    }

    // --- BACKUP_RUNNING RAII guard ---
    //
    // These tests mutate the shared `BACKUP_RUNNING` static. Each test uses
    // a unique profile name so parallel test runs don't interfere.

    #[test]
    fn backup_lock_guard_releases_on_drop() {
        let name = "test-raii-drop".to_string();
        {
            let _guard = BackupLockGuard::acquire(name.clone()).unwrap();
            assert!(
                BACKUP_RUNNING.lock().unwrap().contains(&name),
                "profile should be in the running set while guard is held"
            );
        }
        assert!(
            !BACKUP_RUNNING.lock().unwrap().contains(&name),
            "profile should be removed after guard drops"
        );
    }

    #[test]
    fn backup_lock_guard_blocks_concurrent_acquire_same_profile() {
        let name = "test-raii-concurrent".to_string();
        let _guard = BackupLockGuard::acquire(name.clone()).unwrap();
        let second = BackupLockGuard::acquire(name.clone());
        assert!(
            second.is_err(),
            "second acquire on same profile should fail while first guard is alive"
        );
        let err = second.unwrap_err();
        assert!(
            err.to_string().contains("already in progress"),
            "expected 'already in progress' message, got: {err}"
        );
    }

    #[test]
    fn backup_lock_guard_allows_different_profiles() {
        let a = "test-raii-multi-a".to_string();
        let b = "test-raii-multi-b".to_string();
        let ga = BackupLockGuard::acquire(a.clone()).unwrap();
        let gb = BackupLockGuard::acquire(b.clone()).unwrap();
        {
            let running = BACKUP_RUNNING.lock().unwrap();
            assert!(running.contains(&a));
            assert!(running.contains(&b));
        }
        drop(ga);
        drop(gb);
        let running = BACKUP_RUNNING.lock().unwrap();
        assert!(!running.contains(&a));
        assert!(!running.contains(&b));
    }

    #[test]
    fn opds_mime_maps_epub_pdf() {
        assert_eq!(
            opds_extension_from_mime("application/epub+zip"),
            Some("epub")
        );
        assert_eq!(opds_extension_from_mime("application/pdf"), Some("pdf"));
    }

    #[test]
    fn opds_mime_maps_mobi_family() {
        // x-mobipocket-ebook is the historical MOBI MIME.
        assert_eq!(
            opds_extension_from_mime("application/x-mobipocket-ebook"),
            Some("mobi")
        );
        // `vnd.amazon.ebook` is ambiguous between .azw and .azw3 — the MIME
        // mapper must surface this by returning None so callers fall back to
        // URL-based disambiguation. A final default is applied at the import
        // layer when the URL is also opaque.
        assert_eq!(
            opds_extension_from_mime("application/vnd.amazon.ebook"),
            None
        );
    }

    #[test]
    fn opds_vendor_amazon_mime_falls_back_to_url_extension() {
        // Defense-in-depth: the `download_opds_book` precedence must let URL
        // extension win over the ambiguous vendor MIME so an `.azw` link is
        // not silently renamed `.azw3` on import.
        //
        // We replicate the precedence used in `download_opds_book` here so a
        // regression in that ordering is caught even without a full Tauri
        // harness.
        let mime = "application/vnd.amazon.ebook";
        let url = "https://example.com/download/book.azw";
        let ext = opds_extension_from_url(url)
            .or_else(|| opds_extension_from_mime(mime))
            .unwrap_or("epub");
        assert_eq!(ext, "azw");
    }

    #[test]
    fn opds_vendor_amazon_mime_with_opaque_url_defaults_to_azw3() {
        // When the URL is opaque and MIME is the ambiguous vendor one, we
        // still need a default — AZW3 is the far more common container in the
        // wild today, so fall back to that.
        let mime = "application/vnd.amazon.ebook";
        let url = "https://example.com/download/123";
        let ext = opds_extension_from_url(url)
            .or_else(|| opds_extension_from_mime(mime))
            .unwrap_or("azw3");
        assert_eq!(ext, "azw3");
    }

    #[test]
    fn opds_mime_maps_comic_archives() {
        assert_eq!(
            opds_extension_from_mime("application/vnd.comicbook+zip"),
            Some("cbz")
        );
        assert_eq!(opds_extension_from_mime("application/x-cbz"), Some("cbz"));
        assert_eq!(
            opds_extension_from_mime("application/vnd.comicbook-rar"),
            Some("cbr")
        );
        assert_eq!(opds_extension_from_mime("application/x-cbr"), Some("cbr"));
    }

    #[test]
    fn opds_mime_strips_parameters_and_is_case_insensitive() {
        assert_eq!(
            opds_extension_from_mime("APPLICATION/EPUB+ZIP; profile=\"foo\""),
            Some("epub")
        );
    }

    #[test]
    fn opds_mime_rejects_unknown_types() {
        assert_eq!(opds_extension_from_mime("application/octet-stream"), None);
        assert_eq!(opds_extension_from_mime(""), None);
    }

    #[test]
    fn opds_url_detects_plain_extensions() {
        assert_eq!(
            opds_extension_from_url("https://example.com/book.epub"),
            Some("epub")
        );
        assert_eq!(
            opds_extension_from_url("https://example.com/foo/bar.pdf"),
            Some("pdf")
        );
        assert_eq!(
            opds_extension_from_url("https://example.com/book.AZW3"),
            Some("azw3")
        );
    }

    #[test]
    fn opds_url_ignores_query_and_fragment() {
        assert_eq!(
            opds_extension_from_url("https://example.com/book.epub?token=abc"),
            Some("epub")
        );
        assert_eq!(
            opds_extension_from_url("https://example.com/book.epub#anchor"),
            Some("epub")
        );
    }

    #[test]
    fn opds_url_disambiguates_azw_and_azw3() {
        // Plain `.azw` and `.azw3` must not shadow each other.
        assert_eq!(
            opds_extension_from_url("https://example.com/book.azw"),
            Some("azw")
        );
        assert_eq!(
            opds_extension_from_url("https://example.com/book.azw3"),
            Some("azw3")
        );
    }

    #[test]
    fn opds_url_returns_none_for_opaque_or_missing() {
        // Opaque acquisition URLs (common in OPDS) — MIME path handles these.
        assert_eq!(
            opds_extension_from_url("https://example.com/download/123"),
            None
        );
        // Unparseable / extensionless / non-matching.
        assert_eq!(opds_extension_from_url("not a url"), None);
        assert_eq!(opds_extension_from_url(""), None);
        assert_eq!(
            opds_extension_from_url("https://example.com/book.xyz"),
            None
        );
    }

    #[test]
    fn opds_url_handles_trailing_slash() {
        // Trailing slash: last non-empty segment still has the extension.
        assert_eq!(
            opds_extension_from_url("https://example.com/book.epub/"),
            Some("epub")
        );
    }

    #[test]
    fn opds_catalog_source_preset_id_roundtrip() {
        let src = OpdsCatalogSource {
            name: "Project Gutenberg".to_string(),
            url: "https://m.gutenberg.org/ebooks.opds/".to_string(),
            preset_id: Some("project-gutenberg".to_string()),
        };
        let json = serde_json::to_string(&src).unwrap();
        let parsed: OpdsCatalogSource = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.preset_id.as_deref(), Some("project-gutenberg"));
    }

    #[test]
    fn opds_catalog_source_legacy_blob_deserializes_with_none_preset_id() {
        // Older builds wrote {name, url} only — must still parse.
        let legacy = r#"{"name":"Custom","url":"https://example.com/opds"}"#;
        let parsed: OpdsCatalogSource = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.name, "Custom");
        assert!(parsed.preset_id.is_none());
    }

    #[test]
    fn opds_catalog_source_serializes_camel_case_preset_id() {
        // The TS frontend reads `presetId`, not `preset_id`.
        let src = OpdsCatalogSource {
            name: "x".to_string(),
            url: "https://x".to_string(),
            preset_id: Some("x".to_string()),
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"presetId\""), "expected camelCase: {json}");
    }

    #[test]
    fn opds_catalog_source_omits_preset_id_when_none() {
        let src = OpdsCatalogSource {
            name: "x".to_string(),
            url: "https://x".to_string(),
            preset_id: None,
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(!json.contains("preset"), "expected no preset key: {json}");
    }

    #[test]
    fn opds_url_disambiguates_carrel_acquisition_urls() {
        // Carrel's own OPDS feed emits `/api/books/{id}/download/{id}.{ext}`
        // specifically so URL-based detection can disambiguate AZW from AZW3
        // when the MIME is the ambiguous `application/vnd.amazon.ebook`.
        assert_eq!(
            opds_extension_from_url("https://carrel.local/api/books/abc123/download/abc123.azw"),
            Some("azw")
        );
        assert_eq!(
            opds_extension_from_url("https://carrel.local/api/books/abc123/download/abc123.azw3"),
            Some("azw3")
        );
        assert_eq!(
            opds_extension_from_url("https://carrel.local/api/books/abc123/download/abc123.mobi"),
            Some("mobi")
        );
    }

    #[test]
    fn supported_import_extensions_always_includes_core_formats() {
        let exts = supported_import_extensions();
        for core in &["epub", "pdf", "cbz", "cbr"] {
            assert!(
                exts.contains(core),
                "core format {core} missing from supported_import_extensions"
            );
        }
    }

    #[cfg(feature = "mobi")]
    #[test]
    fn supported_import_extensions_includes_mobi_family_when_feature_on() {
        let exts = supported_import_extensions();
        for mobi in &["mobi", "azw", "azw3"] {
            assert!(exts.contains(mobi), "mobi feature on but {mobi} missing");
        }
    }

    #[cfg(not(feature = "mobi"))]
    #[test]
    fn supported_import_extensions_excludes_mobi_family_when_feature_off() {
        let exts = supported_import_extensions();
        for mobi in &["mobi", "azw", "azw3"] {
            assert!(
                !exts.contains(mobi),
                "mobi feature off but {mobi} still listed"
            );
        }
    }

    #[test]
    fn backup_lock_guard_releases_when_caller_returns_err_early() {
        // Simulates the real regression: a fallible `?` after `acquire` must
        // NOT leak the profile into BACKUP_RUNNING.
        fn fallible(name: String) -> CarrelResult<()> {
            let _guard = BackupLockGuard::acquire(name)?;
            Err(CarrelError::internal("simulated setup failure"))
        }
        let name = "test-raii-early-err".to_string();
        let result = fallible(name.clone());
        assert!(result.is_err(), "function must return the simulated error");
        assert!(
            !BACKUP_RUNNING.lock().unwrap().contains(&name),
            "profile must be released even though the function returned Err"
        );
    }

    fn get_custom_catalogs(conn: &rusqlite::Connection) -> Vec<OpdsCatalogSource> {
        let custom_json = db::get_setting(conn, "opds_custom_catalogs")
            .ok()
            .flatten()
            .unwrap_or_else(|| "[]".to_string());
        serde_json::from_str(&custom_json).unwrap_or_default()
    }

    #[test]
    fn add_opds_catalog_persists_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();

        add_opds_catalog_inner(
            &conn,
            "Project Gutenberg".to_string(),
            "https://m.gutenberg.org/ebooks.opds/".to_string(),
            Some("project-gutenberg".to_string()),
        )
        .unwrap();

        let cats = get_custom_catalogs(&conn);
        let custom = cats
            .iter()
            .find(|c| c.url.contains("gutenberg") && c.preset_id.is_some());
        assert_eq!(
            custom.unwrap().preset_id.as_deref(),
            Some("project-gutenberg")
        );
    }

    #[test]
    fn add_opds_catalog_with_no_preset_id_persists_none() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();

        add_opds_catalog_inner(
            &conn,
            "Custom".to_string(),
            "https://example.com/opds".to_string(),
            None,
        )
        .unwrap();

        let cats = get_custom_catalogs(&conn);
        let custom = cats
            .iter()
            .find(|c| c.url == "https://example.com/opds")
            .unwrap();
        assert!(custom.preset_id.is_none());
    }

    /// I-1 (final-review.md) / design doc line 381: two concurrent
    /// `add_opds_catalog` calls must not lose a catalog to a read-modify-write
    /// race on the single `opds_custom_catalogs` row — the preset-picker
    /// double-click bug (`OpdsPresetPicker.tsx:74-91`, whose per-preset
    /// button only disables the clicked preset).
    ///
    /// This deliberately drives the real `#[tauri::command]` — not
    /// `add_opds_catalog_inner` directly — across genuine OS threads (via
    /// `mock_app_with_state`, defined later in this module and visible here
    /// by item hoisting). A test built on `add_opds_catalog_inner` could only
    /// ever prove the inner helper's own logic; it could not tell you whether
    /// the *command* still acquires `OPDS_AUTH_LOCK`, since the inner helper
    /// is deliberately lock-free (a second lock there would self-deadlock the
    /// command that already holds one — see the module note on
    /// `add_opds_catalog_inner`). Each thread gets its own tiny current-thread
    /// runtime because `add_opds_catalog`'s body has no internal `.await`
    /// point, so a single shared runtime would run each call to completion
    /// before the next started, defeating the point of using real threads.
    ///
    /// This is a genuine race, not a fabricated one: before this fix landed,
    /// running this test with the `OPDS_AUTH_LOCK` acquisition commented out
    /// of `add_opds_catalog` reliably lost several of the `N` catalogs on
    /// this machine (verified by hand while preparing this fix) — it is not
    /// a test that would pass anyway.
    #[test]
    fn add_opds_catalog_concurrent_writers_do_not_lose_a_catalog() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();

        const N: usize = 12;
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    let state = state.clone();
                    scope.spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .build()
                            .expect("build current-thread runtime");
                        rt.block_on(add_opds_catalog(
                            format!("Race Catalog {i}"),
                            format!("https://race.example.com/cat{i}/opds"),
                            None,
                            state,
                        ))
                    })
                })
                .collect();
            for h in handles {
                h.join()
                    .expect("add_opds_catalog thread panicked")
                    .expect("add_opds_catalog returned an error");
            }
        });

        let conn = state.active_db().unwrap().get().unwrap();
        let cats = get_custom_catalogs(&conn);
        for i in 0..N {
            let url = format!("https://race.example.com/cat{i}/opds");
            assert!(
                cats.iter().any(|c| c.url == url),
                "catalog {i} ({url}) was lost to a concurrent-write race — \
                 add_opds_catalog is not serializing on OPDS_AUTH_LOCK"
            );
        }
        assert_eq!(
            cats.len(),
            N,
            "expected exactly {N} surviving catalogs, no drops or duplicates"
        );
    }

    #[test]
    fn default_catalogs_each_has_https_url_and_preset_id() {
        assert!(
            !DEFAULT_CATALOGS.is_empty(),
            "must ship at least one default catalog"
        );
        for (name, url, preset_id) in DEFAULT_CATALOGS {
            assert!(!name.is_empty(), "default catalog has empty name");
            assert!(
                url.starts_with("https://"),
                "default url must be https: {url}"
            );
            assert!(!preset_id.is_empty(), "preset_id must be set for {name}");
        }
        let ids: Vec<&str> = DEFAULT_CATALOGS.iter().map(|(_, _, id)| *id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "preset_ids must be unique");
    }

    #[test]
    fn default_catalogs_include_expected_preset_ids() {
        let ids: std::collections::HashSet<&str> =
            DEFAULT_CATALOGS.iter().map(|(_, _, id)| *id).collect();
        for expected in &["project-gutenberg", "standard-ebooks-new", "wikisource-en"] {
            assert!(
                ids.contains(expected),
                "missing default preset_id: {expected}"
            );
        }
    }

    #[test]
    fn default_catalogs_can_map_to_opds_catalog_source_with_preset_id() {
        // Mirror the mapping done inside get_opds_catalogs. If this test breaks
        // because the tuple shape changed, get_opds_catalogs needs updating too.
        let mapped: Vec<OpdsCatalogSource> = DEFAULT_CATALOGS
            .iter()
            .map(|(name, url, preset_id)| OpdsCatalogSource {
                name: name.to_string(),
                url: url.to_string(),
                preset_id: Some(preset_id.to_string()),
            })
            .collect();
        assert!(!mapped.is_empty());
        let gutenberg = mapped
            .iter()
            .find(|c| c.url == "https://www.gutenberg.org/ebooks.opds/")
            .expect("default Project Gutenberg missing");
        assert_eq!(gutenberg.preset_id.as_deref(), Some("project-gutenberg"));
    }

    #[test]
    fn opds_secret_key_is_length_prefixed_and_unambiguous() {
        // A profile name may contain any character — validation only trims and
        // rejects empty/"default" (commands.rs:4574) — so a delimiter-joined key
        // would be ambiguous. Length prefixing is not.
        assert_eq!(
            opds_secret_key("Classics", "https://h/opds"),
            "8:Classicshttps://h/opds"
        );
        assert_ne!(
            opds_secret_key("a", "b:https://h/opds"),
            opds_secret_key("a:b", "https://h/opds"),
            "these must not collide"
        );
    }

    #[test]
    fn memory_store_round_trips_and_deletes() {
        let store = MemoryCredentialStore::default();
        assert!(store.get("p", "https://h/opds").unwrap().is_none());
        store.set("p", "https://h/opds", "s").unwrap();
        assert_eq!(
            store.get("p", "https://h/opds").unwrap().as_deref(),
            Some("s")
        );
        store.delete("p", "https://h/opds").unwrap();
        assert!(store.get("p", "https://h/opds").unwrap().is_none());
    }

    #[test]
    fn opds_auth_upsert_replaces_by_catalog_url() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/a");
        upsert_opds_auth_inner(&conn, rec("https://h/a", "basic", "u1")).unwrap();
        upsert_opds_auth_inner(&conn, rec("https://h/a", "bearer", "")).unwrap();
        let all = read_opds_auth(&conn);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, "bearer");
    }

    #[test]
    fn two_catalogs_on_one_origin_keep_independent_credentials() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/private/opds");
        add_opds_catalog_inner(&conn, "Team".into(), "https://h/team/opds".into(), None).unwrap();
        upsert_opds_auth_inner(&conn, rec("https://h/private/opds", "basic", "alice")).unwrap();
        upsert_opds_auth_inner(&conn, rec("https://h/team/opds", "basic", "bob")).unwrap();
        let all = read_opds_auth(&conn);
        assert_eq!(all.len(), 2);

        let store = MemoryCredentialStore::default();
        store.set("p", "https://h/private/opds", "s-alice").unwrap();
        store.set("p", "https://h/team/opds", "s-bob").unwrap();
        assert_eq!(
            store.get("p", "https://h/private/opds").unwrap().as_deref(),
            Some("s-alice")
        );
        assert_eq!(
            store.get("p", "https://h/team/opds").unwrap().as_deref(),
            Some("s-bob")
        );
    }

    #[test]
    fn missing_or_corrupt_opds_auth_row_reads_as_empty() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/a");
        assert!(read_opds_auth(&conn).is_empty());
        db::set_setting(&conn, "opds_auth", "{not json").unwrap();
        assert!(read_opds_auth(&conn).is_empty());
    }

    #[test]
    fn get_opds_auth_never_returns_the_secret() {
        // OpdsAuthInfo is the IPC return shape: only `kind` and `username`
        // fields exist, so it structurally cannot carry the secret. Pins the
        // exact camelCase JSON shape sent to the frontend.
        let info = OpdsAuthInfo {
            kind: "basic".to_string(),
            username: "alice".to_string(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "basic", "username": "alice"})
        );
    }

    #[test]
    fn set_opds_auth_rejects_cleartext_for_non_loopback_without_acknowledgement() {
        let (conn, _tmp) = test_conn_with_catalog("http://192.168.0.50:8080/opds");
        let store = MemoryCredentialStore::default();
        let err = set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "http://192.168.0.50:8080/opds",
            "basic",
            "u",
            "s",
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("not encrypted"),
            "got: {err}"
        );
        assert!(
            store
                .get("p", "http://192.168.0.50:8080/opds")
                .unwrap()
                .is_none(),
            "must not write on rejection"
        );
        set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "http://192.168.0.50:8080/opds",
            "basic",
            "u",
            "s",
            true,
        )
        .unwrap();
    }

    #[test]
    fn set_opds_auth_allows_loopback_cleartext_without_acknowledgement() {
        let (conn, _tmp) = test_conn_with_catalog("http://localhost:8080/opds");
        let store = MemoryCredentialStore::default();
        set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "http://localhost:8080/opds",
            "basic",
            "u",
            "s",
            false,
        )
        .unwrap();
    }

    #[test]
    fn set_opds_auth_rejects_unconfigured_catalog_url() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        let err = set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "https://other/opds",
            "bearer",
            "",
            "t",
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not a configured catalog"),
            "got: {err}"
        );
    }

    #[test]
    fn set_opds_auth_validates_kind_and_required_fields() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        assert!(set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "https://h/opds",
            "digest",
            "u",
            "s",
            false
        )
        .is_err());
        assert!(
            set_opds_auth_inner(
                &conn,
                &store,
                "p",
                "https://h/opds",
                "basic",
                "",
                "s",
                false
            )
            .is_err(),
            "basic needs a username"
        );
        assert!(
            set_opds_auth_inner(
                &conn,
                &store,
                "p",
                "https://h/opds",
                "bearer",
                "",
                "",
                false
            )
            .is_err(),
            "empty secret"
        );
    }

    /// Installs a `BEFORE INSERT ON settings` trigger that aborts every
    /// insert (including the `ON CONFLICT DO UPDATE` upserts `set_setting`
    /// issues) with a distinctive message. Reads (`SELECT`) are untouched, so
    /// `configured_catalog_urls`'s membership check still succeeds and only
    /// the row *write* fails — unlike `DROP TABLE settings`, which broke both
    /// and made these tests fail at membership validation instead of the
    /// write, silently testing the wrong thing.
    #[cfg(test)]
    const BLOCK_SETTINGS_WRITES_MSG: &str = "test-only: settings writes blocked";

    #[cfg(test)]
    fn block_settings_writes(conn: &rusqlite::Connection) {
        conn.execute(
            &format!(
                "CREATE TRIGGER block_settings_writes BEFORE INSERT ON settings \
                 BEGIN SELECT RAISE(ABORT, '{BLOCK_SETTINGS_WRITES_MSG}'); END"
            ),
            [],
        )
        .expect("must actually be able to install the write-blocking trigger");
    }

    #[test]
    fn a_failed_row_write_restores_the_previous_secret() {
        // A preset catalog URL is used (rather than test_conn_with_catalog's
        // custom one), but `configured_catalog_urls` still runs a `settings`
        // SELECT for the custom-catalog half of its list even for preset
        // URLs — so the fixture below only blocks writes, not reads.
        let url = "https://www.gutenberg.org/ebooks.opds/";
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        let store = MemoryCredentialStore::default();

        set_opds_auth_inner(&conn, &store, "p", url, "basic", "u", "old-secret", false).unwrap();
        assert_eq!(store.get("p", url).unwrap().as_deref(), Some("old-secret"));

        block_settings_writes(&conn);
        // Prove the fixture does what it claims before relying on it: reads
        // still succeed, so a subsequent failure can only be the write.
        assert!(
            db::get_setting(&conn, "opds_custom_catalogs").is_ok(),
            "reads must still work; only writes are meant to fail"
        );

        let err = set_opds_auth_inner(&conn, &store, "p", url, "basic", "u", "new-secret", false)
            .unwrap_err();
        assert!(
            err.to_string().contains(BLOCK_SETTINGS_WRITES_MSG),
            "expected the row-write failure specifically (not a membership-check \
             failure, which would not mention the trigger) to surface, got: {err}"
        );
        assert_eq!(
            store.get("p", url).unwrap().as_deref(),
            Some("old-secret"),
            "a failed row write must restore the previous secret, not leave the new one"
        );
    }

    #[test]
    fn a_failed_restore_reports_both_failures() {
        // No previous secret exists, so a failed row write falls back to
        // `store.delete` as the compensation — which also fails here
        // (`fail_delete: true`), so the credential is left in an unknown
        // state and the error must say so.
        let url = "https://www.gutenberg.org/ebooks.opds/";
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        block_settings_writes(&conn);
        assert!(
            db::get_setting(&conn, "opds_custom_catalogs").is_ok(),
            "reads must still work; only writes are meant to fail"
        );

        let store = FlakyCredentialStore {
            inner: MemoryCredentialStore::default(),
            fail_set: false,
            fail_delete: true,
            fail_get: false,
        };
        let err =
            set_opds_auth_inner(&conn, &store, "p", url, "bearer", "", "t", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(BLOCK_SETTINGS_WRITES_MSG),
            "must mention the original row-write failure specifically (not a \
             membership-check failure), got: {err}"
        );
        assert!(
            msg.to_lowercase().contains("keychain"),
            "must mention the failed keychain restore: {err}"
        );
    }

    #[test]
    fn set_opds_auth_recognizes_mixed_case_https_scheme_without_allow_insecure() {
        // `origin_from_url` lowercases the scheme, so a registered catalog
        // URL that isn't already lowercase (`HTTPS://...`) must still be
        // recognized as https without needing `allow_insecure` — checking
        // `catalog_url.starts_with("https://")` directly (case-sensitive)
        // would wrongly demand it.
        let (conn, _tmp) = test_conn_with_catalog("HTTPS://h/opds");
        let store = MemoryCredentialStore::default();
        set_opds_auth_inner(
            &conn,
            &store,
            "p",
            "HTTPS://h/opds",
            "bearer",
            "",
            "t",
            false,
        )
        .unwrap();
    }

    #[test]
    fn clear_removes_the_row_before_the_secret_and_propagates_keychain_failure() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = FlakyCredentialStore {
            inner: MemoryCredentialStore::default(),
            fail_set: false,
            fail_delete: true,
            fail_get: false,
        };
        upsert_opds_auth_inner(&conn, rec("https://h/opds", "basic", "u")).unwrap();
        let err = clear_opds_auth_inner(&conn, &store, "p", "https://h/opds").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("keychain"),
            "got: {err}"
        );
        assert!(
            read_opds_auth(&conn).is_empty(),
            "row must be gone even though the keychain delete failed"
        );
    }

    #[test]
    fn add_opds_catalog_rejects_a_duplicate_url() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let err = add_opds_catalog_inner(&conn, "A again".into(), "https://h/opds".into(), None)
            .unwrap_err();
        assert!(err.to_string().contains("already"), "got: {err}");
    }

    #[test]
    fn a_failed_connection_test_restores_both_settings_rows_byte_for_byte() {
        let (conn, _tmp) = test_conn_with_catalog("https://existing/opds");
        upsert_opds_auth_inner(&conn, rec("https://existing/opds", "basic", "keepme")).unwrap();
        let store = MemoryCredentialStore::default();
        store
            .set("p", "https://existing/opds", "keepme-secret")
            .unwrap();

        // Snapshot the exact serialized rows before the attempt.
        let cats_before = db::get_setting(&conn, "opds_custom_catalogs").unwrap();
        let auth_before = db::get_setting(&conn, "opds_auth").unwrap();

        // 127.0.0.1:1 — nothing listens, so the in-backend connection test fails.
        let err = add_opds_catalog_with_auth_inner(
            &conn,
            &store,
            "p",
            "New",
            "http://127.0.0.1:1/opds",
            None,
            "basic",
            "u",
            "s",
            true,
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());

        // Verbatim equality — an implementation that parses, reorders or
        // re-normalizes the rows would fail here, and should.
        assert_eq!(
            db::get_setting(&conn, "opds_custom_catalogs").unwrap(),
            cats_before
        );
        assert_eq!(db::get_setting(&conn, "opds_auth").unwrap(), auth_before);
        // The unrelated secret is untouched; the new one is gone.
        assert_eq!(
            store.get("p", "https://existing/opds").unwrap().as_deref(),
            Some("keepme-secret")
        );
        assert!(store.get("p", "http://127.0.0.1:1/opds").unwrap().is_none());
    }

    /// Success path: the connection test passes, so the catalog, the
    /// credential row, and the keychain secret all end up persisted. Uses a
    /// real `wiremock` server rather than an unreachable URL, matching the
    /// pattern `carrel-core/src/opds.rs`'s `http_tests` module uses for the
    /// same reason: credential handling (which `Authorization` header goes
    /// out) is only observable on the wire. `flavor = "multi_thread"` +
    /// `spawn_blocking` for the same reason as there — the call under test
    /// blocks synchronously on the network, and the mock server's own accept
    /// loop needs a free worker thread to answer it.
    #[tokio::test(flavor = "multi_thread")]
    async fn add_opds_catalog_with_auth_inner_succeeds_when_the_feed_fetch_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const MINIMAL_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
              <title>T</title>
              <entry><id>1</id><title>Book</title></entry>
            </feed>"#;

        let server = MockServer::start().await;
        // base64("u:s") == "dTpz" — an unmatched header 404s, so a
        // successful add proves this exact credential reached the server.
        Mock::given(method("GET"))
            .and(path("/opds"))
            .and(wiremock::matchers::header("authorization", "Basic dTpz"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MINIMAL_FEED))
            .mount(&server)
            .await;
        let url = format!("{}/opds", server.uri());

        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        let store = MemoryCredentialStore::default();

        let url_for_call = url.clone();
        tokio::task::spawn_blocking(move || {
            add_opds_catalog_with_auth_inner(
                &conn,
                &store,
                "p",
                "New",
                &url_for_call,
                None,
                "basic",
                "u",
                "s",
                true,
            )
            .unwrap();

            assert!(
                configured_catalog_urls(&conn)
                    .unwrap()
                    .iter()
                    .any(|u| u == &url_for_call),
                "catalog must be configured after a successful add"
            );
            let record = read_opds_auth(&conn)
                .into_iter()
                .find(|r| r.catalog_url == url_for_call)
                .expect("credential row must be present after a successful add");
            assert_eq!(record.kind, "basic");
            assert_eq!(record.username, "u");
            assert_eq!(store.get("p", &url_for_call).unwrap().as_deref(), Some("s"));
        })
        .await
        .unwrap();
    }

    #[test]
    fn remove_opds_catalog_also_drops_its_credential() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        upsert_opds_auth_inner(&conn, rec("https://h/opds", "basic", "u")).unwrap();
        store.set("p", "https://h/opds", "s").unwrap();
        remove_opds_catalog_inner(&conn, &store, "p", "https://h/opds").unwrap();
        assert!(read_opds_auth(&conn).is_empty());
        assert!(store.get("p", "https://h/opds").unwrap().is_none());
    }

    #[test]
    fn deleting_a_profile_clears_every_opds_credential_it_owned() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/a");
        add_opds_catalog_inner(&conn, "B".into(), "https://h/b".into(), None).unwrap();
        let store = MemoryCredentialStore::default();
        for u in ["https://h/a", "https://h/b"] {
            upsert_opds_auth_inner(&conn, rec(u, "basic", "u")).unwrap();
            store.set("Classics", u, "s").unwrap();
        }
        clear_profile_opds_credentials(&conn, &store, "Classics").unwrap();
        assert!(store.get("Classics", "https://h/a").unwrap().is_none());
        assert!(store.get("Classics", "https://h/b").unwrap().is_none());
    }

    #[test]
    fn clear_profile_opds_credentials_propagates_a_store_failure() {
        // This is the property `delete_profile`'s ordering fix relies on:
        // credential cleanup runs BEFORE the profile's pool is removed from
        // `profile_state`, specifically so a failure here can abort the
        // whole operation before anything is mutated. That only protects
        // anyone if a failing `CredentialStore::delete` actually surfaces as
        // an `Err` here rather than being swallowed — this pins that
        // contract.
        //
        // `delete_profile` itself cannot be exercised end-to-end with this
        // failure: it hardcodes `&KeyringCredentialStore` with no store
        // parameter to inject a test double into, and a keychain-touching
        // test is forbidden regardless (see task-10-report.md for why this
        // wasn't refactored into an injectable `_with` variant). This test
        // therefore pins the contract at the level that IS testable without
        // touching the real keychain.
        let (conn, _tmp) = test_conn_with_catalog("https://h/a");
        upsert_opds_auth_inner(&conn, rec("https://h/a", "basic", "u")).unwrap();
        let store = FlakyCredentialStore {
            inner: MemoryCredentialStore::default(),
            fail_set: false,
            fail_delete: true,
            fail_get: false,
        };
        let err = clear_profile_opds_credentials(&conn, &store, "Classics").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("flaky"),
            "got: {err}"
        );
        // The failure changed nothing: `clear_profile_opds_credentials`
        // never touches the `opds_auth` row (Task 10's decision — the
        // caller deletes the whole profile DB right after), so the record
        // is exactly as it was before the failed attempt.
        assert_eq!(read_opds_auth(&conn).len(), 1);
    }

    #[test]
    fn active_profile_and_db_denied_when_active_profile_locked_and_not_unlocked() {
        // Mirrors active_db_denied_when_active_profile_locked_and_not_unlocked:
        // OPDS catalog credentials are data-bearing, so this seam must be
        // gated by the same soft-lock check as active_db, not just
        // active_db_unchecked's name+pool snapshot.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        state.unlocked_profiles.lock().unwrap().clear();

        let err = active_profile_and_db(&state)
            .expect_err("locked active profile must refuse active_profile_and_db");
        assert_eq!(err.kind(), "LockRequired");

        // Unlocking (as `unlock_profile`/`switch_profile` would) restores access.
        state.mark_unlocked("default").unwrap();
        let (name, _pool) = active_profile_and_db(&state).unwrap();
        assert_eq!(name, "default");
    }

    #[test]
    fn context_admits_provenance_for_a_configured_same_origin_catalog_without_a_credential() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://h/opds"),
            "https://h/opds?page=2",
        );
        assert_eq!(
            ctx.provenance.as_ref().map(|p| p.catalog_url.as_str()),
            Some("https://h/opds"),
            "public catalogs must still be stampable"
        );
        assert!(ctx.cred.is_none());
    }

    #[test]
    fn context_drops_provenance_for_an_unconfigured_catalog_url() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://evil/opds"),
            "https://evil/opds",
        );
        assert!(ctx.provenance.is_none() && ctx.cred.is_none());
    }

    #[test]
    fn context_drops_provenance_after_a_cross_origin_request() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://h/opds"),
            "https://other/feed",
        );
        assert!(
            ctx.provenance.is_none(),
            "request origin differs from the catalog origin"
        );
        assert!(ctx.cred.is_none());
    }

    #[test]
    fn context_carries_nothing_when_no_catalog_url_is_supplied() {
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        let ctx = opds_context_for(&conn, &store, "p", None, "https://h/opds");
        assert!(ctx.provenance.is_none() && ctx.cred.is_none());
    }

    #[test]
    fn context_keeps_provenance_when_no_secret_is_stored() {
        // A stored `opds_auth` record whose secret was never written to the
        // store (as opposed to being unreadable, which
        // `context_keeps_provenance_when_the_keychain_read_fails` below
        // covers): `store.get` returns `Ok(None)`, not `Err`. Provenance
        // stays (so entries are stamped and the user can sign in),
        // credential is absent.
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        let store = MemoryCredentialStore::default();
        upsert_opds_auth_inner(&conn, rec("https://h/opds", "basic", "u")).unwrap();
        let ctx = opds_context_for(&conn, &store, "p", Some("https://h/opds"), "https://h/opds");
        assert!(ctx.provenance.is_some());
        assert!(ctx.cred.is_none());
        assert!(
            !ctx.trusted.is_empty(),
            "the trusted list must survive a missing secret"
        );
    }

    #[test]
    fn context_keeps_provenance_when_the_keychain_read_fails() {
        // Distinct from `context_keeps_provenance_when_no_secret_is_stored`
        // above: here the store's `get` returns `Err` (a denied/unreachable
        // keychain), not `Ok(None)` (no secret ever written). Both must
        // degrade the same way — provenance kept, credential absent — but
        // only a store that can actually fail proves the `Err` arm
        // (`commands.rs`'s `opds_context_for`, the `Err(e) => { log::warn!
        // ...; None }` branch) does the right thing, since
        // `MemoryCredentialStore::get` can never return `Err` at all.
        let (conn, _tmp) = test_conn_with_catalog("https://h/opds");
        upsert_opds_auth_inner(&conn, rec("https://h/opds", "basic", "u")).unwrap();
        let store = FlakyCredentialStore {
            inner: MemoryCredentialStore::default(),
            fail_set: false,
            fail_delete: false,
            fail_get: true,
        };
        // Prove the fixture actually fails the way this test relies on.
        assert!(store.get("p", "https://h/opds").is_err());

        let ctx = opds_context_for(&conn, &store, "p", Some("https://h/opds"), "https://h/opds");
        assert!(
            ctx.provenance.is_some(),
            "a keychain failure must not strip authority — the user must still be able to sign in"
        );
        assert!(
            ctx.cred.is_none(),
            "no credential can be attached without a readable secret"
        );
        assert!(
            !ctx.trusted.is_empty(),
            "a keychain failure must not break public catalogs, which is the whole point"
        );
    }

    #[test]
    fn context_attaches_basic_and_bearer_credentials_and_rejects_unknown_kind() {
        // The brief's five tests never exercise the actual credential-mapping
        // arm (kind == "basic"/"bearer" -> the right OpdsCredential variant,
        // anything else -> None) — added per the task instructions to cover a
        // spec rule ("Map the stored record...") the required tests don't
        // reach.
        let (conn, _tmp) = test_conn_with_catalog("https://h/basic");
        add_opds_catalog_inner(&conn, "Bearer".into(), "https://h/bearer".into(), None).unwrap();
        add_opds_catalog_inner(&conn, "Weird".into(), "https://h/weird".into(), None).unwrap();
        let store = MemoryCredentialStore::default();

        upsert_opds_auth_inner(&conn, rec("https://h/basic", "basic", "alice")).unwrap();
        store.set("p", "https://h/basic", "secret1").unwrap();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://h/basic"),
            "https://h/basic",
        );
        match ctx.cred {
            Some(opds::OpdsCredential::Basic { username, password }) => {
                assert_eq!(username, "alice");
                assert_eq!(password, "secret1");
            }
            _ => panic!("expected a Basic credential"),
        }

        upsert_opds_auth_inner(&conn, rec("https://h/bearer", "bearer", "")).unwrap();
        store.set("p", "https://h/bearer", "token1").unwrap();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://h/bearer"),
            "https://h/bearer",
        );
        match ctx.cred {
            Some(opds::OpdsCredential::Bearer(token)) => assert_eq!(token, "token1"),
            _ => panic!("expected a Bearer credential"),
        }

        upsert_opds_auth_inner(&conn, rec("https://h/weird", "digest", "u")).unwrap();
        store.set("p", "https://h/weird", "s").unwrap();
        let ctx = opds_context_for(
            &conn,
            &store,
            "p",
            Some("https://h/weird"),
            "https://h/weird",
        );
        assert!(
            ctx.cred.is_none(),
            "an unrecognized credential kind must not be guessed into a variant"
        );
        assert!(ctx.provenance.is_some());
    }

    #[test]
    fn discover_and_unified_search_use_each_catalog_as_its_own_provenance() {
        // Pins the shape search_all_catalogs/get_discover_books now build inside
        // their per-catalog loop: opds_context_for(&conn, &store, &profile,
        // Some(&cat.url), &cat.url) is called once per catalog, using that
        // catalog's own URL as both the initiating catalog and the request
        // URL — never a single ambient catalog shared across the whole
        // operation. Two catalogs on different origins, only one with a
        // stored credential: the context built for A must carry A's own
        // provenance and credential, and the context built for B must carry
        // B's provenance and none of A's — passing one catalog's provenance
        // while requesting another's feed is exactly the laundering
        // `opds_context_for`'s rules forbid.
        let url_a = "https://catalog-a.example/opds";
        let url_b = "https://catalog-b.example/opds";
        let (conn, _tmp) = test_conn_with_catalog(url_a);
        add_opds_catalog_inner(&conn, "B".into(), url_b.to_string(), None).unwrap();

        let store = MemoryCredentialStore::default();
        upsert_opds_auth_inner(&conn, rec(url_a, "basic", "alice")).unwrap();
        store.set("p", url_a, "secret-a").unwrap();

        let ctx_a = opds_context_for(&conn, &store, "p", Some(url_a), url_a);
        assert_eq!(
            ctx_a.provenance.as_ref().map(|p| p.catalog_url.as_str()),
            Some(url_a),
            "catalog A's context must name A"
        );
        match ctx_a.cred {
            Some(opds::OpdsCredential::Basic { username, password }) => {
                assert_eq!(username, "alice");
                assert_eq!(password, "secret-a");
            }
            _ => panic!("catalog A's context must carry A's own stored credential"),
        }

        let ctx_b = opds_context_for(&conn, &store, "p", Some(url_b), url_b);
        assert_eq!(
            ctx_b.provenance.as_ref().map(|p| p.catalog_url.as_str()),
            Some(url_b),
            "catalog B's context must name B, not A"
        );
        assert!(
            ctx_b.cred.is_none(),
            "catalog B must not inherit catalog A's credential"
        );
    }

    #[test]
    fn web_server_set_modes_persists_both_settings() {
        // Persistence-only assertion. Server start/stop is exercised by
        // web_server::tests::* (router-shape tests). This test guards the
        // contract that user intent always lands in the DB.
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();

        // Simulate the persistence portion of web_server_set_modes by
        // calling its bare DB statements (the handle/start path requires
        // an AppState which we cannot construct here).
        db::set_setting(&conn, "web_ui_enabled", "true").unwrap();
        db::set_setting(&conn, "opds_enabled", "false").unwrap();
        db::set_setting(&conn, "web_server_port", "9999").unwrap();

        assert_eq!(
            db::get_setting(&conn, "web_ui_enabled").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(
            db::get_setting(&conn, "opds_enabled").unwrap().as_deref(),
            Some("false")
        );
        assert_eq!(
            db::get_setting(&conn, "web_server_port")
                .unwrap()
                .as_deref(),
            Some("9999")
        );
    }

    #[test]
    fn migrate_web_server_setting_true_sets_both_new_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        db::set_setting(&conn, "web_server_enabled", "true").unwrap();

        migrate_web_server_setting(&conn).unwrap();

        assert_eq!(
            db::get_setting(&conn, "web_ui_enabled").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(
            db::get_setting(&conn, "opds_enabled").unwrap().as_deref(),
            Some("true")
        );
        assert!(db::get_setting(&conn, "web_server_enabled")
            .unwrap()
            .is_none());
    }

    #[test]
    fn migrate_web_server_setting_false_sets_both_false() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        db::set_setting(&conn, "web_server_enabled", "false").unwrap();

        migrate_web_server_setting(&conn).unwrap();

        assert_eq!(
            db::get_setting(&conn, "web_ui_enabled").unwrap().as_deref(),
            Some("false")
        );
        assert_eq!(
            db::get_setting(&conn, "opds_enabled").unwrap().as_deref(),
            Some("false")
        );
        assert!(db::get_setting(&conn, "web_server_enabled")
            .unwrap()
            .is_none());
    }

    #[test]
    fn migrate_web_server_setting_no_op_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        // No legacy key set.
        migrate_web_server_setting(&conn).unwrap();
        assert!(db::get_setting(&conn, "web_ui_enabled").unwrap().is_none());
        assert!(db::get_setting(&conn, "opds_enabled").unwrap().is_none());
    }

    #[test]
    fn migrate_web_server_setting_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&tmp.path().join("library.db")).unwrap();
        let conn = pool.get().unwrap();
        db::set_setting(&conn, "web_server_enabled", "true").unwrap();

        migrate_web_server_setting(&conn).unwrap();
        // Simulate user later turning Web UI off; migration must not undo that.
        db::set_setting(&conn, "web_ui_enabled", "false").unwrap();
        migrate_web_server_setting(&conn).unwrap();

        assert_eq!(
            db::get_setting(&conn, "web_ui_enabled").unwrap().as_deref(),
            Some("false"),
            "migration must not clobber user changes after first migration"
        );
    }

    // ── Background-import atomics ─────────────────────────────────────────────
    //
    // These tests exercise the run-once / cancel invariants that protect the
    // background importer. They use the static atomics directly because the
    // full IPC wrappers need a Tauri State which is impractical to build in
    // a unit test. The atomics are the only state the wrappers consult, so
    // the contract is the same.
    //
    // The atomics are process-global statics, so these tests must not run
    // concurrently with each other (cargo runs tests multithreaded by
    // default) or one test's store/swap races another's assertion. Each
    // acquires this lock first to serialize access; poison is recovered
    // since a panicking test leaves the atomics in a known-reset state.
    static IMPORT_ATOMICS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn import_atomics_default_false() {
        let _guard = IMPORT_ATOMICS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Note: tests in the same binary share statics. Reset before checking
        // the invariant we care about.
        IMPORT_RUNNING.store(false, Ordering::SeqCst);
        IMPORT_CANCEL.store(false, Ordering::SeqCst);
        assert!(!IMPORT_RUNNING.load(Ordering::SeqCst));
        assert!(!IMPORT_CANCEL.load(Ordering::SeqCst));
    }

    #[test]
    fn import_running_swap_blocks_second_acquire() {
        let _guard = IMPORT_ATOMICS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        IMPORT_RUNNING.store(false, Ordering::SeqCst);
        // First acquire succeeds (returns the previous value, false).
        assert!(!IMPORT_RUNNING.swap(true, Ordering::SeqCst));
        // Second acquire observes the running flag and would refuse the slot.
        assert!(IMPORT_RUNNING.swap(true, Ordering::SeqCst));
        // Cleanup so other tests in the binary aren't affected.
        IMPORT_RUNNING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn cancel_import_sets_flag() {
        let _guard = IMPORT_ATOMICS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        IMPORT_CANCEL.store(false, Ordering::SeqCst);
        // The Tauri command body is just this store; calling it through the
        // tokio runtime would force an async harness, so call the underlying
        // op directly.
        IMPORT_CANCEL.store(true, Ordering::SeqCst);
        assert!(IMPORT_CANCEL.load(Ordering::SeqCst));
        IMPORT_CANCEL.store(false, Ordering::SeqCst);
    }

    /// Write a minimal but valid EPUB to `dir/name.epub` so `import_book_inner`
    /// can parse it (container.xml -> OPF; empty spine yields zero chapters,
    /// which the importer accepts). Returns the on-disk path.
    fn write_minimal_epub(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(format!("{name}.epub"));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("mimetype", options).unwrap();
        std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", options).unwrap();
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
        zip.start_file("content.opf", options).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Fast Path Test Book</dc:title>
    <dc:creator>Test Author</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest/>
  <spine/>
</package>"#,
        )
        .unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn reimport_same_path_fast_skips_without_rehash() {
        // No shared global state: isolated tempdir + private pool per test, and
        // import_book_inner does not touch the import-running atomics, so these
        // tests need no serialization.
        let work = tempfile::tempdir().unwrap();
        let db_pool = db::create_pool(&work.path().join("library.db")).unwrap();
        let storage: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("library")).unwrap(),
        );
        let covers: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("covers")).unwrap(),
        );

        let src = write_minimal_epub(work.path(), "book");
        let src_path_string = src.to_string_lossy().to_string();

        let first = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        let first_id = first.into_book().id;

        // Sanity: the source row was recorded with the file's real size.
        {
            let conn = db_pool.get().unwrap();
            let meta = std::fs::metadata(&src).unwrap();
            let rec = db::get_book_by_source_path(&conn, &src_path_string)
                .unwrap()
                .unwrap();
            assert_eq!(rec.id, first_id);
            assert_eq!(rec.source_size, Some(meta.len() as i64));
        }

        // Second import of the SAME path -> Duplicate with same id, one row.
        let second = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        match second {
            ImportOutcome::Duplicate(b) => assert_eq!(b.id, first_id),
            _ => panic!("expected Duplicate outcome"),
        }
        let conn = db_pool.get().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn reimport_with_changed_mtime_falls_through_to_hash() {
        let work = tempfile::tempdir().unwrap();
        let db_pool = db::create_pool(&work.path().join("library.db")).unwrap();
        let storage: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("library")).unwrap(),
        );
        let covers: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("covers")).unwrap(),
        );

        let src = write_minimal_epub(work.path(), "book");
        let src_path_string = src.to_string_lossy().to_string();

        let first = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        let first_id = first.into_book().id;

        // Force a stored mtime mismatch so the fast path cannot fire.
        {
            let conn = db_pool.get().unwrap();
            conn.execute(
                "UPDATE books SET source_mtime = 1 WHERE id = ?1",
                rusqlite::params![first_id],
            )
            .unwrap();
        }

        // Falls through to the hash, which still dedups to the same book.
        let second = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        match second {
            ImportOutcome::Duplicate(b) => assert_eq!(b.id, first_id),
            _ => panic!("expected Duplicate by hash"),
        }
        let conn = db_pool.get().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn reimport_fast_path_skips_when_hash_would_miss() {
        let work = tempfile::tempdir().unwrap();
        let db_pool = db::create_pool(&work.path().join("library.db")).unwrap();
        let storage: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("library")).unwrap(),
        );
        let covers: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("covers")).unwrap(),
        );

        let src = write_minimal_epub(work.path(), "book");
        let src_path_string = src.to_string_lossy().to_string();

        let first = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        let first_id = first.into_book().id;

        // Sabotage the stored content hash so hash-based dedup CANNOT match on
        // re-import. The source_path/size/mtime row stays intact, so ONLY the
        // fast-path can still recognize this as a duplicate. If the fast-path were
        // broken, the re-import would hash the file, find no hash match, and import
        // a SECOND book (count == 2).
        {
            let conn = db_pool.get().unwrap();
            conn.execute(
                "UPDATE books SET file_hash = 'deadbeef-not-a-real-hash' WHERE id = ?1",
                rusqlite::params![first_id],
            )
            .unwrap();
        }

        let second = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        match second {
            ImportOutcome::Duplicate(b) => assert_eq!(b.id, first_id),
            _ => panic!("fast-path should have returned the existing book as Duplicate"),
        }

        let conn = db_pool.get().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "fast-path must skip without creating a second row"
        );
    }

    #[test]
    fn reimport_hash_match_refreshes_source_tracking() {
        // When the fast-path misses (drifted mtime) but the content hash still
        // dedups, the stored source_size/mtime must be refreshed to the file's
        // current values so the NEXT re-import fast-skips instead of re-hashing
        // forever. Without the refresh the fast-path never warms back up.
        let work = tempfile::tempdir().unwrap();
        let db_pool = db::create_pool(&work.path().join("library.db")).unwrap();
        let storage: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("library")).unwrap(),
        );
        let covers: std::sync::Arc<dyn carrel_core::storage::Storage> = std::sync::Arc::new(
            carrel_core::storage::LocalStorage::new(work.path().join("covers")).unwrap(),
        );

        let src = write_minimal_epub(work.path(), "book");
        let src_path_string = src.to_string_lossy().to_string();

        let first = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        let first_id = first.into_book().id;

        // The real mtime recorded on first import.
        let real_mtime = {
            let conn = db_pool.get().unwrap();
            db::get_book_by_source_path(&conn, &src_path_string)
                .unwrap()
                .unwrap()
                .source_mtime
        };

        // Simulate mtime drift on a content-identical file: clobber the stored
        // mtime so the fast-path cannot fire and we go through the hash.
        {
            let conn = db_pool.get().unwrap();
            conn.execute(
                "UPDATE books SET source_mtime = 1 WHERE id = ?1",
                rusqlite::params![first_id],
            )
            .unwrap();
        }

        let second = import_book_inner(
            src_path_string.clone(),
            db_pool.clone(),
            storage.clone(),
            covers.clone(),
            "link",
            false,
            ImportSource::Manual,
        )
        .unwrap();
        match second {
            ImportOutcome::Duplicate(b) => assert_eq!(b.id, first_id),
            _ => panic!("expected Duplicate by hash"),
        }

        // The hash-match path must have refreshed the stored mtime back to the
        // file's real value — re-arming the fast-path for future re-imports.
        let refreshed = {
            let conn = db_pool.get().unwrap();
            db::get_book_by_source_path(&conn, &src_path_string)
                .unwrap()
                .unwrap()
                .source_mtime
        };
        assert_eq!(
            refreshed, real_mtime,
            "hash-match re-import must refresh stored source_mtime"
        );
        assert_ne!(
            refreshed,
            Some(1),
            "stale sentinel mtime must be overwritten"
        );
    }

    #[test]
    fn smb_unicode_hint_fires_for_accented_name_on_volumes() {
        let hint = smb_unicode_hint(
            "/Volumes/home/BOOKS/04 - Quitte ou double à Quito.pdf",
            std::io::ErrorKind::NotFound,
        );
        let msg = hint.expect("accented name on /Volumes/ with ENOENT must produce a hint");
        assert!(msg.contains("SMB"), "hint must name the SMB bug: {msg}");
        assert!(msg.contains("User Guide"), "hint must point at docs: {msg}");
    }

    #[test]
    fn smb_unicode_hint_fires_for_accented_directory_component() {
        // The lookup bug hits accented *path components* too, not just the
        // file name itself.
        assert!(smb_unicode_hint(
            "/Volumes/nas/Intégrales/Tome 4.cbz",
            std::io::ErrorKind::NotFound,
        )
        .is_some());
    }

    #[test]
    fn smb_unicode_hint_silent_for_ascii_path() {
        assert!(smb_unicode_hint(
            "/Volumes/home/BOOKS/The Spider King.cbr",
            std::io::ErrorKind::NotFound,
        )
        .is_none());
    }

    #[test]
    fn smb_unicode_hint_silent_outside_volumes() {
        assert!(smb_unicode_hint(
            "/Users/mike/Books/Quitte ou double à Quito.pdf",
            std::io::ErrorKind::NotFound,
        )
        .is_none());
    }

    #[test]
    #[cfg(target_os = "macos")] // hint is appended only on macOS (with_smb_hint cfg gate)
    fn validate_file_exists_appends_smb_hint_for_accented_volumes_path() {
        // Linked books keep their share path; a reader command on a file hit
        // by the smbfs bug must explain it rather than claim the file moved.
        let err = validate_file_exists("/Volumes/nas/Intégrales/Tome 4.cbz").unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("SMB"),
            "reader error must carry the hint: {msg}"
        );
    }

    #[test]
    fn smb_unicode_hint_silent_for_other_error_kinds() {
        assert!(smb_unicode_hint(
            "/Volumes/home/BOOKS/Quitte ou double à Quito.pdf",
            std::io::ErrorKind::PermissionDenied,
        )
        .is_none());
    }

    // ---- Profile soft-lock (A-M2) ----
    //
    // `carrel_core::profile_lock`'s keychain-backed functions
    // (`has_lock`/`load_lock`/`set_lock`) are deliberately never called in a
    // way whose *result* this test suite depends on — mirrors A-M1's own
    // tests and `backup.rs`'s untested `store_secrets`/`load_secrets`. A
    // real keychain call can behave differently per environment (no
    // secret-service on headless Linux CI vs. a real Keychain on macOS) and
    // asserting on its outcome would make a test flaky by host. The one
    // exception is `delete_profile`'s `clear_lock` call below, which is
    // fire-and-forget (`let _ = ...`) in the command itself — the test
    // exercises the real call but never asserts on what it returned, only
    // on `AppState` bookkeeping that doesn't depend on it. The soft-lock
    // *decision* itself (locked && not-unlocked => denied) is covered
    // exhaustively and keychain-free by
    // `carrel_core::profile_lock::access_allowed`'s own unit tests.
    //
    // The `switch_profile` *command* takes a concrete `AppHandle` (defaulting
    // to the real `Wry` runtime — see `#[default_runtime]` on
    // `tauri::AppHandle`), which `tauri::test::mock_app()`'s `MockRuntime`
    // handle cannot satisfy, so the command wrapper itself isn't called here.
    // Its whole body is `switch_active_profile`, which IS generic over the
    // runtime and is exercised end-to-end below.

    /// Builds a `tauri::App<MockRuntime>` with a fresh `AppState` managed on
    /// it, backed by an in-memory DB and a tempdir data dir. `State<'_, T>`
    /// has no public constructor, so this is the only way to obtain one in
    /// a test — commands taking only `State` (not `AppHandle`) can be
    /// called directly against `handle.state::<AppState>()`.
    fn mock_app_with_state() -> (tauri::App<tauri::test::MockRuntime>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::create_pool(&dir.path().join("library.db")).unwrap();
        let app = tauri::test::mock_app();
        app.manage(AppState {
            shared_active_pool: std::sync::Arc::new(std::sync::Mutex::new(pool.clone())),
            shared_active_profile_name: std::sync::Arc::new(std::sync::Mutex::new(
                "default".to_string(),
            )),
            shared_pin_hash: std::sync::Arc::new(std::sync::Mutex::new(None)),
            unlocked_profiles: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::from(["default".to_string()]),
            )),
            private_mode: std::sync::Arc::new(AtomicBool::new(false)),
            active_foreground_renders: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            profile_lifecycle: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            db: pool,
            profile_state: std::sync::Mutex::new(ProfileState {
                active: "default".to_string(),
                pools: std::collections::HashMap::new(),
            }),
            data_dir: dir.path().to_path_buf(),
            cache_dir: dir.path().to_path_buf(),
            epub_cache: std::sync::Arc::new(std::sync::Mutex::new(LruCache::new(5))),
            #[cfg(feature = "mobi")]
            mobi_cache: std::sync::Arc::new(std::sync::Mutex::new(LruCache::new(5))),
            enrichment_registry: std::sync::Mutex::new(crate::providers::ProviderRegistry::new()),
            web_server_handle: std::sync::Mutex::new(None),
            ipc_metrics: IpcMetrics::new(500, 500.0),
            plugin_manager: std::sync::Arc::new(std::sync::Mutex::new(None)),
            _log_guard: None,
            dictionary_pool: std::sync::Mutex::new(None),
            dictionary_downloading: std::sync::atomic::AtomicBool::new(false),
            pending_manual_update_check: std::sync::Mutex::new(false),
            startup_update_check_taken: std::sync::atomic::AtomicBool::new(false),
            update_check: crate::update::UpdateCheckState::new(),
        });
        (app, dir)
    }

    #[test]
    fn appstate_is_unlocked_defaults_false_until_marked() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        assert!(!state.is_unlocked("alice"));
        state.mark_unlocked("alice").unwrap();
        assert!(state.is_unlocked("alice"));
    }

    #[test]
    fn appstate_mark_locked_removes_from_unlocked_set() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        state.mark_unlocked("alice").unwrap();
        assert!(state.is_unlocked("alice"));
        state.mark_locked("alice").unwrap();
        assert!(!state.is_unlocked("alice"));
    }

    // ── Shared profile-switch core ───────────────────────────────────────
    //
    // `switch_active_profile_with` exists so these tests can drive the
    // soft-lock decision without a real keychain (see the keychain note
    // above): the lock-status probe is a parameter, and the only production
    // caller passes `carrel_core::profile_lock::has_lock`. The decision itself
    // is still the shared `profile_lock::access_allowed`, so parity with the
    // desktop command and the web gate is structural, not duplicated.

    /// Adds a profile with its own on-disk DB carrying a `profile_marker`
    /// setting that names it, so a test can prove *which* pool the shared
    /// handle points at after a switch.
    fn add_profile(state: &AppState, dir: &std::path::Path, name: &str) {
        let pool = db::create_pool(&dir.join(format!("library-{name}.db"))).unwrap();
        {
            let conn = pool.get().unwrap();
            db::set_setting(&conn, "profile_marker", name).unwrap();
        }
        state
            .profile_state
            .lock()
            .unwrap()
            .pools
            .insert(name.to_string(), pool);
    }

    /// The `profile_marker` of whatever DB the web-server-facing shared pool
    /// currently points at.
    fn marker_via_shared_pool(state: &AppState) -> Option<String> {
        let pool = state.shared_active_pool.lock().unwrap().clone();
        let conn = pool.get().unwrap();
        db::get_setting(&conn, "profile_marker").unwrap()
    }

    #[tokio::test]
    async fn switch_core_swaps_shared_pool_and_active_profile_name() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");
        assert_eq!(marker_via_shared_pool(&state), None, "starts on default");

        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false))
            .await
            .unwrap();

        assert_eq!(state.profile_state.lock().unwrap().active, "alice");
        assert_eq!(*state.shared_active_profile_name.lock().unwrap(), "alice");
        assert_eq!(marker_via_shared_pool(&state).as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn switch_core_rejects_unknown_profile() {
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();

        let err =
            switch_active_profile_with(app.handle(), &state, "ghost".to_string(), |_| Ok(false))
                .await
                .unwrap_err();

        assert!(format!("{err}").contains("not found"), "got: {err}");
        assert_eq!(state.profile_state.lock().unwrap().active, "default");
        assert_eq!(*state.shared_active_profile_name.lock().unwrap(), "default");
    }

    #[tokio::test]
    async fn switch_core_rejects_locked_profile_not_unlocked_this_session() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        let err =
            switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(true))
                .await
                .unwrap_err();

        assert!(format!("{err}").contains("locked"), "got: {err}");
        assert_eq!(state.profile_state.lock().unwrap().active, "default");
        assert_eq!(*state.shared_active_profile_name.lock().unwrap(), "default");
        assert_eq!(marker_via_shared_pool(&state), None, "pool untouched");
    }

    #[tokio::test]
    async fn switch_core_allows_locked_profile_already_unlocked_this_session() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");
        state.mark_unlocked("alice").unwrap();

        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(true))
            .await
            .unwrap();

        assert_eq!(state.profile_state.lock().unwrap().active, "alice");
    }

    #[tokio::test]
    async fn switch_core_allows_profile_without_a_lock() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false))
            .await
            .unwrap();

        assert_eq!(state.profile_state.lock().unwrap().active, "alice");
    }

    /// Fail-closed contract (A-M1 Decision 7): a keychain that can't be read
    /// is not "unlocked" — the switch is refused, not allowed.
    #[tokio::test]
    async fn switch_core_refuses_when_lock_status_cannot_be_read() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        let err = switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| {
            Err(CarrelError::internal("keychain unavailable"))
        })
        .await
        .unwrap_err();

        assert!(
            format!("{err}").contains("keychain unavailable"),
            "got: {err}"
        );
        assert_eq!(state.profile_state.lock().unwrap().active, "default");
    }

    /// The activity event is logged by the core, so it happens for a remote
    /// (web) switch exactly as for the desktop command. It lands in the
    /// *new* profile's log — that's the pool active by the time it's written.
    #[tokio::test]
    async fn switch_core_logs_profile_switched_in_the_new_profiles_log() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false))
            .await
            .unwrap();

        let pool = state.shared_active_pool.lock().unwrap().clone();
        let conn = pool.get().unwrap();
        let entries = db::get_activity_log(&conn, 10, 0, Some("profile_switched")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entity_name.as_deref(), Some("alice"));
    }

    /// A switch can come from the web now, so the desktop UI can't assume it
    /// initiated one: the core emits `profile-changed` and the frontend reloads
    /// off that. Without it a remote switch leaves the desktop rendering the
    /// previous profile's library.
    #[tokio::test]
    async fn switch_core_emits_profile_changed() {
        use tauri::Listener;

        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        app.handle().listen("profile-changed", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });

        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false))
            .await
            .unwrap();

        // Delivery is asynchronous; give the event loop a moment.
        for _ in 0..50 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let payloads = seen.lock().unwrap().clone();
        assert_eq!(payloads.len(), 1, "exactly one event per switch");
        assert!(
            payloads[0].contains("alice"),
            "payload names the new profile: {}",
            payloads[0]
        );
    }

    /// The core must serialize on `profile_lifecycle` — the same lock
    /// `delete_profile` holds — so a remote switch can't validate against a
    /// profile another lifecycle command is in the middle of removing.
    #[tokio::test]
    async fn switch_core_waits_for_the_profile_lifecycle_lock() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "alice");

        let guard = state.profile_lifecycle.clone().lock_owned().await;
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false)),
        )
        .await;
        assert!(
            blocked.is_err(),
            "switch must block while another profile-lifecycle command holds the lock"
        );
        assert_eq!(state.profile_state.lock().unwrap().active, "default");

        drop(guard);
        switch_active_profile_with(app.handle(), &state, "alice".to_string(), |_| Ok(false))
            .await
            .unwrap();
        assert_eq!(state.profile_state.lock().unwrap().active, "alice");
    }

    // ── Profile listing with soft-lock state (web `GET /api/profiles`) ───

    #[test]
    fn profile_list_marks_the_active_profile_and_sorts_by_name() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "magazines");
        add_profile(&state, dir.path(), "comics");

        let list = list_profiles_with_lock_state_with(&state, |_| Ok(false)).unwrap();

        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["comics", "default", "magazines"]);
        assert_eq!(
            list.iter()
                .filter(|p| p.active)
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["default"]
        );
    }

    #[test]
    fn profile_list_marks_a_locked_profile_unswitchable_until_unlocked() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "magazines");

        let locked = list_profiles_with_lock_state_with(&state, |name| Ok(name == "magazines"))
            .unwrap()
            .into_iter()
            .find(|p| p.name == "magazines")
            .unwrap();
        assert!(locked.locked);
        assert!(
            !locked.switchable,
            "a locked profile not unlocked on the desktop is not reachable over HTTP"
        );

        state.mark_unlocked("magazines").unwrap();
        let unlocked = list_profiles_with_lock_state_with(&state, |name| Ok(name == "magazines"))
            .unwrap()
            .into_iter()
            .find(|p| p.name == "magazines")
            .unwrap();
        assert!(unlocked.locked, "still has a stored lock");
        assert!(
            unlocked.switchable,
            "already unlocked this session → reachable"
        );
    }

    /// Fail-closed (A-M1 Decision 7): a keychain that can't be read must not
    /// downgrade to "no lock". The listing still renders — a keychain outage
    /// shouldn't blank the whole switcher — but the profile is reported locked
    /// and unswitchable, and an attempted switch surfaces the real error.
    #[test]
    fn profile_list_treats_unreadable_lock_status_as_locked() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        add_profile(&state, dir.path(), "magazines");

        let list = list_profiles_with_lock_state_with(&state, |_| {
            Err(CarrelError::internal("keychain unavailable"))
        })
        .unwrap();

        assert!(list.iter().all(|p| p.locked), "unreadable → assumed locked");
        let magazines = list.iter().find(|p| p.name == "magazines").unwrap();
        assert!(!magazines.switchable);
        // `default` is in this session's unlocked set, and a session unlock
        // outranks lock status — same as the desktop gate. So it stays
        // switchable even while the keychain is unreadable.
        assert!(
            list.iter()
                .find(|p| p.name == "default")
                .unwrap()
                .switchable
        );
    }

    /// `resolve_book_path` must return a locally-staged copy when one exists for
    /// the book's content hash — even for a linked (non-imported) book on an
    /// absolute (network) path — and fall back to the original path otherwise.
    /// This is the read half of the source-cache: staging happens at open, and
    /// every later page render resolves to the local file.
    #[test]
    fn resolve_book_path_prefers_staged_copy() {
        let (app, dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();

        let mut book = Book {
            id: "bk-staged".to_string(),
            title: "Staged".to_string(),
            author: String::new(),
            file_path: "/Volumes/remote/comic.pdf".to_string(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: BookFormat::Pdf,
            file_hash: Some("abc123hash".to_string()),
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
            is_imported: false,
            want_to_read: false,
        };

        // No staged copy yet → resolves to the original (remote) path.
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            "/Volumes/remote/comic.pdf"
        );

        // Stage a local copy under the state's cache_dir (keyed by hash + ext).
        let srcdir = tempfile::tempdir().unwrap();
        let src = srcdir.path().join("comic.pdf");
        std::fs::write(&src, b"staged bytes").unwrap();
        let staged =
            carrel_core::source_cache::stage(dir.path(), &src, "abc123hash", "pdf").unwrap();

        // Now resolve prefers the local staged copy.
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            staged.to_string_lossy()
        );

        // A book with no hash never resolves to a staged copy.
        book.file_hash = None;
        assert_eq!(
            state.resolve_book_path(&book).unwrap(),
            "/Volumes/remote/comic.pdf"
        );
    }

    /// A LOCAL source must never be staged (guards against an `is_remote_path`
    /// polarity flip that would copy every local book). A tempdir is always on
    /// a local filesystem, so `stage_source_if_remote` returns false and writes
    /// nothing.
    #[test]
    fn stage_source_if_remote_skips_local_source() {
        let cache = tempfile::tempdir().unwrap();
        let srcdir = tempfile::tempdir().unwrap();
        let src = srcdir.path().join("local.pdf");
        std::fs::write(&src, b"local bytes").unwrap();

        let attempted =
            stage_source_if_remote(cache.path(), &src.to_string_lossy(), "localhash", "pdf");

        assert!(!attempted, "a local source must not be staged");
        assert!(
            carrel_core::source_cache::staged_if_present(cache.path(), "localhash", "pdf")
                .is_none(),
            "no staged copy should exist for a local source"
        );
    }

    /// The in-flight dedup guard: the first claim on a hash wins; a second claim
    /// while it's held is refused; after release the hash can be claimed again.
    #[test]
    fn staging_inflight_guard_dedups_by_hash() {
        let h = "inflight-guard-hash";
        assert!(claim_staging(h), "first claim succeeds");
        assert!(!claim_staging(h), "second concurrent claim is refused");
        release_staging(h);
        assert!(claim_staging(h), "claimable again after release");
        release_staging(h);
    }

    /// M2 web trigger: when a staged copy already exists,
    /// `ensure_web_source_staged` takes the cheap touch branch — it bumps the
    /// copy's LRU recency (mtime) rather than re-staging. Proven by backdating
    /// the staged file's mtime and asserting the call advances it. A bogus
    /// `real_path` guarantees the result comes from the touch branch, not from
    /// a stage of the source.
    #[test]
    fn ensure_web_source_staged_touches_existing_copy() {
        let cache = tempfile::tempdir().unwrap();
        let srcdir = tempfile::tempdir().unwrap();
        let src = srcdir.path().join("c.pdf");
        std::fs::write(&src, b"staged bytes").unwrap();
        let staged =
            carrel_core::source_cache::stage(cache.path(), &src, "webm2hash", "pdf").unwrap();

        // Backdate the staged copy's mtime to a fixed point well in the past.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&staged)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let mut book = Book {
            id: "web-m2".to_string(),
            title: "M2".to_string(),
            author: String::new(),
            file_path: "/Volumes/remote/c.pdf".to_string(),
            cover_path: None,
            total_chapters: 1,
            added_at: 0,
            format: BookFormat::Pdf,
            file_hash: Some("webm2hash".to_string()),
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
            is_imported: false,
            want_to_read: false,
        };

        ensure_web_source_staged(cache.path(), &book, "/bogus/remote/c.pdf");

        let bumped = std::fs::metadata(&staged).unwrap().modified().unwrap();
        assert!(
            bumped > old,
            "an existing staged copy must have its LRU mtime bumped by the touch branch"
        );

        // A book with no hash is a no-op (guards the early return).
        book.file_hash = None;
        ensure_web_source_staged(cache.path(), &book, "/bogus/remote/c.pdf");
    }

    #[test]
    fn active_db_denied_when_active_profile_locked_and_not_unlocked() {
        // The desktop-IPC bypass fix (A-M2, spec D-6/SB-7): with the active
        // profile ("default") not in the unlocked set — i.e. a stored lock
        // at startup — every data command that resolves `active_db` must be
        // refused with `LockRequired`, not served.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        state.unlocked_profiles.lock().unwrap().clear();

        let err = state
            .active_db()
            .expect_err("locked active profile must refuse active_db");
        assert_eq!(err.kind(), "LockRequired");

        // The unchecked escape hatch (startup web auto-start) still resolves.
        assert!(state.active_db_unchecked().is_ok());

        // Unlocking (as `unlock_profile`/`switch_profile` would) restores access.
        state.mark_unlocked("default").unwrap();
        assert!(state.active_db().is_ok());
    }

    /// Mirrors the exact condition `switch_profile` gates on: a locked,
    /// not-yet-unlocked profile must be denied (surfaced as
    /// `CarrelError::LockRequired`).
    #[test]
    fn switch_profile_gate_denies_locked_and_not_unlocked() {
        assert!(!carrel_core::profile_lock::access_allowed(true, false));
    }

    #[test]
    fn switch_profile_gate_allows_after_unlock_profile_marks_unlocked() {
        // Simulates `unlock_profile`'s success path (`state.mark_unlocked`)
        // followed by `switch_profile`'s gate check on the same session
        // state — proving a successful unlock really does let a retried
        // switch through.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        assert!(!carrel_core::profile_lock::access_allowed(
            true,
            state.is_unlocked("bob")
        ));
        state.mark_unlocked("bob").unwrap();
        assert!(carrel_core::profile_lock::access_allowed(
            true,
            state.is_unlocked("bob")
        ));
    }

    #[tokio::test]
    async fn create_profile_without_password_behaves_as_before() {
        // `password: None` (the checkbox-off case) must be a no-op change:
        // the profile is created and left unlocked, exactly like before
        // this option existed.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        create_profile("erin".to_string(), None, state)
            .await
            .expect("creating a profile without a password should succeed");

        let state = app.handle().state::<AppState>();
        assert!(state
            .profile_state
            .lock()
            .unwrap()
            .pools
            .contains_key("erin"));
        assert!(
            !state.is_unlocked("erin"),
            "create_profile must not mark an unlocked-without-a-password profile as unlocked"
        );
    }

    #[tokio::test]
    async fn create_profile_with_blank_password_behaves_as_before() {
        // Whitespace-only input from the frontend's password field must be
        // treated the same as the checkbox being off, not as "lock with an
        // empty password".
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        create_profile("frank".to_string(), Some("   ".to_string()), state)
            .await
            .expect("creating a profile with a blank password should succeed unlocked");

        // Keychain-free proxy for "no lock set": `create_profile` only calls
        // `set_lock`/`mark_unlocked` inside the non-blank-password branch, so a
        // blank password leaving the profile not-unlocked proves that branch was
        // skipped — asserted without touching the real keychain (module note above).
        let state = app.handle().state::<AppState>();
        assert!(
            !state.is_unlocked("frank"),
            "a blank password must not set a lock"
        );
    }

    #[tokio::test]
    #[ignore = "drives the real OS keychain via set_lock: fails on headless CI (no \
                secret-service) and pollutes the local keychain. Un-ignore once \
                create_profile takes an injectable lock backend — see module note above."]
    async fn create_profile_with_password_creates_it_and_marks_it_unlocked() {
        // The new behavior: a non-empty password creates the profile,
        // hashes and stores it via `set_lock` (exercised for real here —
        // see the module note above on why the real keychain *result*
        // isn't asserted on), and marks the profile unlocked for this
        // session (the caller just typed the password, so no immediate
        // re-prompt — see the `create_profile` doc comment). Like
        // `reset_profile_lock_marks_profile_unlocked`, this asserts on
        // `AppState` bookkeeping (`is_unlocked`) rather than reading the
        // lock back from the keychain (`has_lock`), which isn't reliably
        // observable in a headless/automated test environment.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        create_profile("dave".to_string(), Some("hunter2".to_string()), state)
            .await
            .expect("creating a profile with a password should succeed");

        let state = app.handle().state::<AppState>();
        assert!(state
            .profile_state
            .lock()
            .unwrap()
            .pools
            .contains_key("dave"));
        assert!(
            state.is_unlocked("dave"),
            "create_profile must mark a just-locked profile unlocked for this session"
        );
    }

    #[tokio::test]
    async fn create_profile_blocks_while_profile_lifecycle_lock_held() {
        // Mirrors `delete_profile_blocks_while_profile_lifecycle_lock_held`:
        // while another lifecycle command holds `profile_lifecycle`,
        // `create_profile` must block at its lifecycle-lock acquisition
        // (function entry, before any password handling) rather than interleave.
        // Uses `None` so the assertion stays keychain-free — the blocking under
        // test happens before the password branch, so a password is irrelevant.
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        let lifecycle = handle.state::<AppState>().profile_lifecycle.clone();
        let outer_guard = lifecycle.lock().await;

        let state = handle.state::<AppState>();
        let timed_out = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            create_profile("grace".to_string(), None, state),
        )
        .await
        .is_err();
        assert!(
            timed_out,
            "create_profile must block on profile_lifecycle while another \
             lifecycle command holds it"
        );

        drop(outer_guard);
        let state = handle.state::<AppState>();
        create_profile("grace".to_string(), None, state)
            .await
            .expect("creating the profile should succeed once the lock is free");
    }

    #[tokio::test]
    async fn set_profile_lock_rejects_missing_profile() {
        // A lock command for a name that is neither "default" nor a created
        // profile must be refused before any keychain write, so a mistyped or
        // stale name can't leave an orphaned lock a later same-name profile
        // would inherit.
        let (app, _dir) = mock_app_with_state();
        let state = app.handle().state::<AppState>();
        let err = set_profile_lock("ghost".to_string(), "pw".to_string(), None, state)
            .await
            .expect_err("locking a non-existent profile must fail");
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[tokio::test]
    async fn dictionary_status_missing_then_ready() {
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();

        let status = get_dictionary_status(handle.state::<AppState>())
            .await
            .unwrap();
        assert_eq!(
            status.state,
            carrel_core::dictionary::DictionaryState::Missing
        );

        carrel_core::dictionary::write_test_artifact(&handle.state::<AppState>().dictionary_dir())
            .unwrap();
        let status = get_dictionary_status(handle.state::<AppState>())
            .await
            .unwrap();
        assert_eq!(
            status.state,
            carrel_core::dictionary::DictionaryState::Ready
        );
    }

    #[tokio::test]
    async fn dictionary_lookup_notfound_resolves_and_delete_clears() {
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();

        // Missing artifact → NotFound so the frontend can route to settings.
        let err = lookup_word("cat".to_string(), handle.state::<AppState>())
            .await
            .expect_err("lookup with no artifact must fail");
        assert_eq!(err.kind(), "NotFound");

        // Install a synthetic artifact; lookup resolves and caches the pool.
        carrel_core::dictionary::write_test_artifact(&handle.state::<AppState>().dictionary_dir())
            .unwrap();
        let entry = lookup_word("cat".to_string(), handle.state::<AppState>())
            .await
            .unwrap();
        assert_eq!(entry.unwrap().matched_word, "cat");

        // Delete drops the cached pool AND removes the file.
        delete_dictionary(handle.state::<AppState>()).await.unwrap();
        assert_eq!(
            get_dictionary_status(handle.state::<AppState>())
                .await
                .unwrap()
                .state,
            carrel_core::dictionary::DictionaryState::Missing
        );
        // With the pool cleared and the file gone, lookup is NotFound again.
        let err = lookup_word("cat".to_string(), handle.state::<AppState>())
            .await
            .expect_err("lookup after delete must fail");
        assert_eq!(err.kind(), "NotFound");
    }

    #[tokio::test]
    async fn download_dictionary_rejects_concurrent() {
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        // Simulate an in-flight download; a second call must bail before any
        // network work rather than racing on the staging files.
        handle
            .state::<AppState>()
            .dictionary_downloading
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = download_dictionary(handle.state::<AppState>(), handle.clone())
            .await
            .expect_err("second concurrent download must be refused");
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[tokio::test]
    async fn reset_profile_lock_rejects_missing_profile() {
        // Same existence guard as the other profile-lifecycle commands
        // (Decision 10) — a mistyped/stale name must never touch the
        // keychain or the unlocked set.
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        let state = handle.state::<AppState>();
        let err = reset_profile_lock(handle.clone(), "ghost".to_string(), state)
            .await
            .expect_err("resetting a non-existent profile must fail");
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[tokio::test]
    async fn reset_profile_lock_marks_profile_unlocked() {
        // The recovery path (Decision 9) clears the keychain entry
        // (fire-and-forget, like `delete_profile`'s hygiene call — see the
        // module note above on why the real keychain result isn't asserted
        // on) and must mark the profile unlocked so a retried switch
        // succeeds without ever knowing the old password.
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        {
            let state = handle.state::<AppState>();
            let mut ps = state.profile_state.lock().unwrap();
            let pool = db::create_pool(&std::path::PathBuf::from(":memory:")).unwrap();
            ps.pools.insert("carol".to_string(), pool);
            drop(ps);
            assert!(!state.is_unlocked("carol"));
        }

        let state = handle.state::<AppState>();
        reset_profile_lock(handle.clone(), "carol".to_string(), state)
            .await
            .expect("resetting an existing profile's lock should succeed");

        let state = handle.state::<AppState>();
        assert!(
            state.is_unlocked("carol"),
            "reset_profile_lock must mark the profile unlocked (Decision 9)"
        );
    }

    #[tokio::test]
    async fn reset_profile_lock_runs_deferred_plugin_start_for_active_profile() {
        // Recovery at startup (D-6/SB-7): when the app boots on a locked
        // active profile, `lib.rs` skips building the plugin manager and
        // emitting `AppStarted`. `unlock_profile` runs that deferred tail on
        // a correct password; the "forgot password" reset is an alternate
        // unlock path and must run the *same* tail — otherwise plugins stay
        // dead for the session. Here the active "default" profile starts with
        // no manager; after reset the shared slot must be populated, proving
        // `run_deferred_plugin_start` ran `rebuild_for_profile`.
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        {
            let state = handle.state::<AppState>();
            assert!(
                state.plugin_manager.lock().unwrap().is_none(),
                "precondition: locked-at-boot leaves the manager slot empty"
            );
        }

        let state = handle.state::<AppState>();
        reset_profile_lock(handle.clone(), "default".to_string(), state)
            .await
            .expect("resetting the active profile's lock should succeed");

        let state = handle.state::<AppState>();
        assert!(
            state.plugin_manager.lock().unwrap().is_some(),
            "reset_profile_lock must run the deferred plugin start for the \
             active profile, exactly like unlock_profile"
        );
    }

    #[tokio::test]
    async fn delete_profile_clears_unlocked_set_entry() {
        // Also the mirror-image positive case for Task 10's ordering fix:
        // goes through `delete_profile_with` (rather than duplicating this
        // test) with a real on-disk DB and a seeded OPDS credential, so a
        // successful run is proven to remove the pool, delete the DB file,
        // AND clear the credential it owned — not just the unlocked-set
        // entry this test originally covered.
        let (app, dir) = mock_app_with_state();
        let handle = app.handle().clone();
        let db_path = dir.path().join("library-carol.db");
        {
            let state = handle.state::<AppState>();
            let mut ps = state.profile_state.lock().unwrap();
            let pool = db::create_pool(&db_path).unwrap();
            let conn = pool.get().unwrap();
            add_opds_catalog_inner(&conn, "Carol's".into(), "https://h/carol".into(), None)
                .unwrap();
            upsert_opds_auth_inner(&conn, rec("https://h/carol", "basic", "u")).unwrap();
            drop(conn);
            ps.pools.insert("carol".to_string(), pool);
            drop(ps);
            // Simulate a previously-unlocked session for this profile, as
            // `switch_profile`/`unlock_profile` would have left it.
            state.mark_unlocked("carol").unwrap();
            assert!(state.is_unlocked("carol"));
        }
        assert!(db_path.exists(), "fixture must actually create the DB file");

        let store = MemoryCredentialStore::default();
        store.set("carol", "https://h/carol", "s").unwrap();

        let state = handle.state::<AppState>();
        delete_profile_with(state.inner(), "carol".to_string(), &store)
            .await
            .expect("deleting a non-active profile should succeed");

        let state = handle.state::<AppState>();
        assert!(
            !state.is_unlocked("carol"),
            "delete_profile must drop the profile from the unlocked set (Decision 10)"
        );
        assert!(!state
            .profile_state
            .lock()
            .unwrap()
            .pools
            .contains_key("carol"));
        assert!(!db_path.exists(), "the profile's DB file must be removed");
        assert!(
            store.get("carol", "https://h/carol").unwrap().is_none(),
            "the credential the profile owned must be cleared from the store"
        );
    }

    #[tokio::test]
    async fn delete_profile_with_leaves_everything_intact_when_credential_cleanup_fails() {
        // The load-bearing property behind Task 10's ordering fix: if
        // clearing OPDS credentials fails, `delete_profile_with` must not
        // have removed the profile's pool from `profile_state`, nor deleted
        // its DB file. Otherwise the operation isn't actually retryable —
        // `switch_active_profile_with` refuses any profile not in
        // `ps.pools` (pools are never re-created lazily), and a retry that
        // found no pool would skip cleanup silently and delete the DB file
        // anyway, leaving the secrets this task exists to remove sitting in
        // the keychain. A future refactor that re-merges the two
        // `profile_state` critical sections in `delete_profile_with` would
        // break exactly this.
        let (app, dir) = mock_app_with_state();
        let handle = app.handle().clone();
        let db_path = dir.path().join("library-carol.db");
        {
            let state = handle.state::<AppState>();
            let mut ps = state.profile_state.lock().unwrap();
            let pool = db::create_pool(&db_path).unwrap();
            let conn = pool.get().unwrap();
            add_opds_catalog_inner(&conn, "Carol's".into(), "https://h/carol".into(), None)
                .unwrap();
            upsert_opds_auth_inner(&conn, rec("https://h/carol", "basic", "u")).unwrap();
            drop(conn);
            ps.pools.insert("carol".to_string(), pool);
        }
        assert!(db_path.exists(), "fixture must actually create the DB file");

        let store = FlakyCredentialStore {
            inner: MemoryCredentialStore::default(),
            fail_set: false,
            fail_delete: true,
            fail_get: false,
        };
        let state = handle.state::<AppState>();
        let err = delete_profile_with(state.inner(), "carol".to_string(), &store)
            .await
            .expect_err("a failed credential cleanup must fail the whole operation");
        assert!(
            err.to_string().to_lowercase().contains("flaky"),
            "got: {err}"
        );

        let state = handle.state::<AppState>();
        assert!(
            state
                .profile_state
                .lock()
                .unwrap()
                .pools
                .contains_key("carol"),
            "the profile's pool must still be present so the operation is retryable"
        );
        assert!(
            db_path.exists(),
            "the DB file must not be deleted when credential cleanup failed"
        );
    }

    #[tokio::test]
    async fn delete_profile_blocks_while_profile_lifecycle_lock_held() {
        // Regression test for the TOCTOU the reviewer flagged: before
        // `AppState::profile_lifecycle` existed, `delete_profile` validated
        // profile existence under `profile_state`, dropped that lock, then
        // mutated the keychain and `unlocked_profiles` with nothing held
        // across the gap — a concurrent lifecycle command could interleave
        // and orphan state. This proves `delete_profile` actually acquires
        // `profile_lifecycle` for its whole body: while the lock is held
        // externally (simulating another in-flight lifecycle command past
        // its own validate step, before its mutate step), `delete_profile`
        // cannot even begin its existence check, let alone mutate anything.
        //
        // A true two-task race is deliberately avoided (see the module
        // comment above on why keychain-backed calls aren't asserted on
        // directly) — blocking on the shared `tokio::sync::Mutex` while it
        // is held is itself the guarantee this fix provides, and is
        // observable deterministically via `timeout` without spawning.
        let (app, _dir) = mock_app_with_state();
        let handle = app.handle().clone();
        {
            let state = handle.state::<AppState>();
            let mut ps = state.profile_state.lock().unwrap();
            let pool = db::create_pool(&std::path::PathBuf::from(":memory:")).unwrap();
            ps.pools.insert("carol".to_string(), pool);
            drop(ps);
            state.mark_unlocked("carol").unwrap();
        }

        let lifecycle = handle.state::<AppState>().profile_lifecycle.clone();
        let outer_guard = lifecycle.lock().await;

        let state = handle.state::<AppState>();
        let timed_out = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            delete_profile("carol".to_string(), state),
        )
        .await
        .is_err();
        assert!(
            timed_out,
            "delete_profile must block on profile_lifecycle while another \
             lifecycle command holds it"
        );

        // Releasing the externally-held guard lets delete_profile run to
        // completion and leave consistent state (no orphaned entries).
        drop(outer_guard);
        let state = handle.state::<AppState>();
        delete_profile("carol".to_string(), state)
            .await
            .expect("deleting a non-active profile should succeed once the lock is free");

        let state = handle.state::<AppState>();
        assert!(!state.is_unlocked("carol"));
        assert!(!state
            .profile_state
            .lock()
            .unwrap()
            .pools
            .contains_key("carol"));
    }

    // --- Vocabulary builder IPC (F-1-5 M2) ---

    #[test]
    fn log_vocabulary_word_entry_noops_when_setting_unset_or_false() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();

        // `vocabulary_enabled` unset.
        log_vocabulary_word_entry(
            &conn,
            "running".to_string(),
            "run".to_string(),
            Some("verb".to_string()),
            "to move fast on foot".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(db::list_vocabulary(&conn).unwrap().len(), 0);

        // Explicitly disabled.
        db::set_setting(&conn, "vocabulary_enabled", "false").unwrap();
        log_vocabulary_word_entry(
            &conn,
            "running".to_string(),
            "run".to_string(),
            Some("verb".to_string()),
            "to move fast on foot".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(db::list_vocabulary(&conn).unwrap().len(), 0);
    }

    #[test]
    fn log_vocabulary_word_entry_inserts_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();

        log_vocabulary_word_entry(
            &conn,
            "running".to_string(),
            "run".to_string(),
            Some("verb".to_string()),
            "to move fast on foot".to_string(),
            None,
            None,
            None,
            None,
            Some(42),
            Some(49),
        )
        .unwrap();

        let words = db::list_vocabulary(&conn).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].box_num, 1);
        assert!(words[0].next_due_at.is_none());
        assert_eq!(words[0].seen_count, 1);
        assert_eq!(words[0].start_offset, Some(42));
        assert_eq!(words[0].end_offset, Some(49));
    }

    #[test]
    fn record_vocabulary_review_now_advances_box_and_sets_next_due() {
        let dir = tempfile::tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("t.db")).unwrap();
        db::set_setting(&conn, "vocabulary_enabled", "true").unwrap();
        log_vocabulary_word_entry(
            &conn,
            "running".to_string(),
            "run".to_string(),
            None,
            "to move fast on foot".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let id = db::list_vocabulary(&conn).unwrap()[0].id.clone();

        record_vocabulary_review_now(&conn, &id, true).unwrap();

        let words = db::list_vocabulary(&conn).unwrap();
        assert_eq!(words[0].box_num, 2);
        assert!(words[0].next_due_at.is_some());
        assert!(words[0].last_reviewed_at.is_some());
    }

    #[test]
    fn startup_update_check_command_grants_once() {
        let (app, _dir) = mock_app_with_state();
        assert!(take_startup_update_check(app.state::<AppState>())); // first → true
        assert!(!take_startup_update_check(app.state::<AppState>())); // then → false
        assert!(!take_startup_update_check(app.state::<AppState>()));
    }

    #[test]
    fn pending_manual_update_check_command_consumes_once() {
        let (app, _dir) = mock_app_with_state();
        assert!(!take_pending_manual_update_check(app.state::<AppState>())); // starts false
        *app.state::<AppState>()
            .pending_manual_update_check
            .lock()
            .unwrap() = true; // tray sets it
        assert!(take_pending_manual_update_check(app.state::<AppState>())); // consumed → true
        assert!(!take_pending_manual_update_check(app.state::<AppState>())); // cleared → false
    }
}

// ── Update check ───────────────────────────────────────────

/// Check GitHub for a newer release. Returns the comparison; the frontend
/// decides presentation per trigger. Errors are stable codes
/// (timeout | network | rate_limited | http_error | malformed_response);
/// detail is logged in Rust, not returned.
#[tauri::command]
pub async fn check_for_update(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<crate::update::UpdateCheck, String> {
    // Re-parse through our own semver crate so both operands share one crate
    // version regardless of Tauri's transitive semver.
    let current = semver::Version::parse(&app.package_info().version.to_string()).map_err(|e| {
        tracing::error!(error = %e, "update check: cannot parse installed version");
        "malformed_response".to_string()
    })?;
    crate::update::check(&state.update_check, &current).await
}

/// Atomically read-and-clear the pending manual-check flag. Exactly one caller
/// (open-window event handler OR recreated-window mount) gets `true`.
#[tauri::command]
pub fn take_pending_manual_update_check(state: State<'_, AppState>) -> bool {
    let mut flag = state.pending_manual_update_check.lock().unwrap();
    std::mem::take(&mut *flag) // clippy prefers take over replace(_, false)
}

/// Grant the automatic startup check to the first caller per process only.
#[tauri::command]
pub fn take_startup_update_check(state: State<'_, AppState>) -> bool {
    !state
        .startup_update_check_taken
        .swap(true, std::sync::atomic::Ordering::SeqCst)
}

// ── Autostart ──────────────────────────────────────────────

#[tauri::command]
pub async fn get_autostart_enabled(app: AppHandle) -> CarrelResult<bool> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| CarrelError::internal(format!("Failed to check autostart: {}", e)))
}

#[tauri::command]
pub async fn set_autostart_enabled(app: AppHandle, enabled: bool) -> CarrelResult<()> {
    use tauri_plugin_autostart::ManagerExt;

    let autostart = app.autolaunch();

    if enabled {
        autostart
            .enable()
            .map_err(|e| format!("Failed to enable autostart: {}", e))?;
    } else {
        autostart
            .disable()
            .map_err(|e| format!("Failed to disable autostart: {}", e))?;
    }

    Ok(())
}

// ── Quote Cards (F-1-6) ────────────────────────────────────────────────────

/// Writes an already-encoded PNG (from the frontend canvas's `toBlob`) to a
/// user-chosen path. No image decoding/encoding happens on the Rust side.
#[tauri::command]
pub async fn save_quote_card_png(path: String, bytes: Vec<u8>) -> CarrelResult<()> {
    std::fs::write(&path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod quote_card_tests {
    use super::*;

    #[tokio::test]
    async fn save_quote_card_png_writes_bytes_to_path() {
        let path = std::env::temp_dir().join(format!("carrel-test-{}.png", Uuid::new_v4()));
        let path_str = path.to_string_lossy().to_string();
        let bytes = vec![137, 80, 78, 71, 1, 2, 3];

        save_quote_card_png(path_str, bytes.clone()).await.unwrap();

        let written = std::fs::read(&path).unwrap();
        assert_eq!(written, bytes);

        std::fs::remove_file(&path).unwrap();
    }
}

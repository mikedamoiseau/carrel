//! Transparent local staging cache for remote (network-mounted) book files.
//!
//! Rendering a PDF/comic that lives on a network share reads the source file
//! by random access at render time (link-import mode reads it in place), which
//! is punishingly slow over SMB — a single page can take seconds to minutes.
//! This module stages the whole source file onto fast local disk once, so
//! every subsequent page render reads locally instead.
//!
//! Staging is **content-addressed**: the staged copy is named by the book's
//! content hash (`source-cache/{hash}.{ext}`). Because the key is the content
//! itself, a changed remote file gets a *different* key — a stale staged copy
//! can never be served for changed content, so no mtime/version bookkeeping is
//! needed. Callers that lack a content hash simply skip staging.
//!
//! This layer is deliberately filesystem-direct (`std::fs`, not the `Storage`
//! abstraction the page cache uses): the entire purpose is a real local copy
//! with an atomic publish, which the abstraction does not model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::error::{FolioError, FolioResult};

/// Subdirectory under the app cache dir holding every staged source file.
const SOURCE_CACHE_DIR: &str = "source-cache";

/// Subdirectory (under [`SOURCE_CACHE_DIR`]) holding in-progress temp copies.
/// Temps live one level DEEPER than staged files, so a temp path can never
/// equal a staged destination path (which is always a single component directly
/// under `source-cache/`) — this is what prevents a `(hash, ext)` split from
/// colliding with another key's temp (e.g. `("foo","pdf")`'s temp vs
/// `(".foo.pdf","tmp")`'s destination). Eviction ([M3]) skips this subdir.
const SOURCE_CACHE_TMP_DIR: &str = "tmp";

/// Single-flight locks keyed by the STAGED FILENAME (`{hash}.{ext}`), not the
/// raw hash. Keying by the final filename means two stagers that map to the same
/// destination always share a lock even if they split the name differently
/// (e.g. `("a","b.c")` vs `("a.b","c")` both target `a.b.c`) — so they can never
/// race the same file or temp path. Stagers of different destinations run fully
/// in parallel.
///
/// This is an **in-process** primitive: `STAGE_LOCKS` is process-local, so two
/// OS processes staging the same file would not serialize. Carrel runs as a
/// single process (the embedded web server shares it), so this holds for the
/// app as-is. Entries are kept for the process lifetime — one small
/// `Arc<Mutex<()>>` per distinct file ever staged, bounded by the library size.
static STAGE_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get (or create) the single-flight lock for staged-file `key`.
fn lock_for(key: &str) -> Arc<Mutex<()>> {
    let mut map = STAGE_LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// True when `s` is safe to interpolate as a single path component: non-empty,
/// not a directory-traversal token, and free of path separators / drive / NUL.
/// This is the guard that keeps a crafted `hash`/`ext` from escaping the cache
/// dir (`..`, `/etc/...`, `C:\...`) or resolving the destination to the
/// `source-cache` directory itself (empty name).
fn is_safe_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0'])
}

/// The staged filename for `hash`/`ext` (no leading dot on `ext`; empty `ext`
/// yields a bare hash). Pure — does not touch the filesystem. Assumes the
/// components are already validated by [`is_safe_component`] (enforced in
/// [`stage`] / [`is_staged`]).
fn staged_file_name(hash: &str, ext: &str) -> String {
    if ext.is_empty() {
        hash.to_string()
    } else {
        format!("{hash}.{ext}")
    }
}

/// Validate the `(hash, ext)` key: `hash` must be a safe component, and `ext`
/// must be empty or a safe component. Returns the staged filename on success.
fn validated_file_name(hash: &str, ext: &str) -> FolioResult<String> {
    if !is_safe_component(hash) || !(ext.is_empty() || is_safe_component(ext)) {
        return Err(FolioError::invalid(format!(
            "unsafe source-cache key (hash={hash:?}, ext={ext:?})"
        )));
    }
    Ok(staged_file_name(hash, ext))
}

/// Absolute path a staged copy of `hash` (extension `ext`, without the dot)
/// would occupy under `base` (the app cache dir). Pure.
pub fn staged_path(base: &Path, hash: &str, ext: &str) -> PathBuf {
    base.join(SOURCE_CACHE_DIR)
        .join(staged_file_name(hash, ext))
}

/// Return the staged path if a valid staged copy already exists for
/// `(hash, ext)`, else `None`. LOCAL-only and cheap (one `symlink_metadata`) —
/// never touches the source, so it is safe on the render hot path where the
/// source lives on a slow network mount.
///
/// Existence alone is sufficient: [`stage`] publishes atomically (copy to a
/// temp, then rename), so a file present at the destination is always the
/// complete copy — there is no partial-file window at this path. Rejects unsafe
/// keys and symlinks with the same guards as [`is_staged`], so a crafted key
/// can't escape the cache dir and a planted symlink isn't trusted.
pub fn staged_if_present(base: &Path, hash: &str, ext: &str) -> Option<PathBuf> {
    if !is_safe_component(hash) || !(ext.is_empty() || is_safe_component(ext)) {
        return None;
    }
    let p = staged_path(base, hash, ext);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_file() => Some(p),
        _ => None,
    }
}

/// Default size budget (MB) for the whole source cache. Staged files are entire
/// book files — a single scanned comic can be ~500 MB — so this is far larger
/// than the page cache's rendered-JPEG budget. ~4 GB holds a working set of
/// several large books; least-recently-opened files are evicted past it.
pub const DEFAULT_MAX_SIZE_MB: u64 = 4096;

/// Bump a staged file's modified-time to now, marking it most-recently-used for
/// [`run_eviction`]'s LRU ordering. Called once per open (cheap, local). No-op
/// if the book isn't staged. Best-effort — a failure just leaves the old mtime,
/// which at worst evicts a still-wanted file slightly early.
pub fn touch_staged(base: &Path, hash: &str, ext: &str) {
    if let Some(p) = staged_if_present(base, hash, ext) {
        if let Ok(f) = std::fs::File::options().write(true).open(&p) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

/// Remove the staged copy for `(hash, ext)`, if present. Best-effort — used when
/// a book is deleted so its (large) staged file doesn't linger until size
/// eviction reclaims it.
///
/// Also removes any in-progress temp for this key. It takes the same per-key
/// stage lock `stage` holds for its whole copy→rename lifetime, so a stage that
/// is *already running* fully publishes before we remove — removing both the
/// final file and the temp then leaves nothing behind.
///
/// KNOWN LIMITATION (bounded, benign): staging is queued asynchronously, so if a
/// book is deleted in the brief window after open but before its queued stage
/// task calls [`stage`], this runs first (lock uncontended, nothing to remove)
/// and the task later publishes an orphaned copy. That orphan is not a leak: the
/// cache stays within its size budget regardless (size eviction reclaims it as a
/// normal LRU entry), and a re-import of the same content reuses it. Fully
/// closing this would need a delete-vs-queue tombstone; deemed not worth the
/// machinery for a transient, self-healing orphan.
pub fn remove_staged(base: &Path, hash: &str, ext: &str) {
    if !is_safe_component(hash) || !(ext.is_empty() || is_safe_component(ext)) {
        return;
    }
    let name = staged_file_name(hash, ext);
    let lock = lock_for(&name);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    let dir = base.join(SOURCE_CACHE_DIR);
    let _ = std::fs::remove_file(dir.join(&name));
    let _ = std::fs::remove_file(dir.join(SOURCE_CACHE_TMP_DIR).join(format!("{name}.tmp")));
}

/// Evict least-recently-modified staged files until the source cache's total
/// size is within `max_size_mb`. mtime is bumped on each open ([`touch_staged`]),
/// so this is last-open LRU. Only regular files directly under `source-cache/`
/// are considered — the `tmp/` subdir (in-progress copies) is skipped. Best-
/// effort: a file that can't be stat'd or removed is skipped, never aborting.
pub fn run_eviction(base: &Path, max_size_mb: u64) -> FolioResult<()> {
    let dir = base.join(SOURCE_CACHE_DIR);

    // First reclaim abandoned temp copies — a stage that crashed after creating
    // its temp but before the rename leaves a (potentially ~500 MB) file in
    // `tmp/` that the staged-file sweep below never counts or removes, so the
    // "bounded" cache could grow without limit. Anything in `tmp/` older than
    // the threshold is certainly not an active copy (even a 500 MB copy over a
    // slow share finishes in minutes), so younger temps of live stages are left
    // untouched.
    reclaim_stale_temps(&dir.join(SOURCE_CACHE_TMP_DIR));

    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(()), // no cache dir yet — nothing to evict
    };

    // Collect regular files directly under source-cache/. `DirEntry::metadata`
    // does not traverse symlinks, so the `tmp/` subdir (a directory) and any
    // symlink are excluded by the `is_file` check.
    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    for entry in rd.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((entry.path(), meta.len(), mtime));
    }

    let budget = max_size_mb.saturating_mul(1024 * 1024);
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= budget {
        return Ok(());
    }

    // Oldest modified-time first — last-open LRU, since `touch_staged` bumps
    // mtime on each open. Two concurrent stages can each run this from their own
    // snapshot and over-evict a file or two (both targeting the same oldest);
    // the result stays within budget and the extra files re-stage on demand, so
    // no lock coordinates them.
    files.sort_by_key(|(_, _, mtime)| *mtime);
    for (path, size, _) in files {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }

    Ok(())
}

/// Seconds after which a file in `tmp/` is considered abandoned (a crashed
/// stage), safe to reclaim. Generously longer than any real copy: even a
/// multi-hundred-MB copy over a slow network share completes in minutes.
const STALE_TMP_SECS: u64 = 3600;

/// Delete temp files in `tmp_dir` older than [`STALE_TMP_SECS`]. Best-effort;
/// leaves younger temps (potentially live in-progress copies) untouched.
///
/// Each temp is handled under its per-key stage lock: an in-progress copy holds
/// that lock for its whole lifetime, so once acquired here the temp is either
/// already renamed away (stage finished) or truly abandoned (the stager is
/// gone). The staleness re-check runs UNDER the lock, so a temp replaced by a
/// fresh stage between enumeration and removal is re-judged (and, being live,
/// spared) rather than deleted on stale metadata.
fn reclaim_stale_temps(tmp_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(tmp_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        // Temp files are named `{staged_file_name}.tmp`; the key without the
        // suffix is the stage lock key.
        let file_name = entry.file_name();
        let Some(key) = file_name.to_str().and_then(|n| n.strip_suffix(".tmp")) else {
            continue;
        };
        let lock = lock_for(key);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        // Re-stat UNDER the lock (state may have changed since enumeration).
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if now
            .duration_since(mtime)
            .map(|age| age.as_secs() > STALE_TMP_SECS)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// True when a staged copy exists AND its byte length equals `expected_size`.
///
/// The size check is the integrity guard: a content-addressed name plus an
/// exact byte count rejects a truncated/partial file (e.g. a copy interrupted
/// by a crash before the atomic rename — which should never publish, but the
/// check is cheap insurance).
pub fn is_staged(base: &Path, hash: &str, ext: &str, expected_size: u64) -> bool {
    if !is_safe_component(hash) || !(ext.is_empty() || is_safe_component(ext)) {
        return false;
    }
    // `symlink_metadata` does NOT follow symlinks, so a pre-planted symlink at
    // the staged path (whose target happens to match `expected_size`) is
    // rejected — `is_file()` is false for a symlink — rather than trusted as a
    // valid staged copy that redirects reads to an unrelated/remote file.
    match std::fs::symlink_metadata(staged_path(base, hash, ext)) {
        Ok(m) => m.file_type().is_file() && m.len() == expected_size,
        Err(_) => false,
    }
}

/// Stage `src` into the local source-cache under `hash`, returning the path of
/// the staged copy.
///
/// - **Single-flight**: concurrent callers for the same destination serialize
///   on a per-file lock; the losers observe the winner's finished copy and
///   return it without recopying.
/// - **Idempotent**: if a valid staged copy (size matches `src`) already exists,
///   returns immediately without recopying.
/// - **Atomic publish**: copies to a temp file in a dedicated `tmp/` subdir,
///   then renames it into place, so a concurrent reader (or [`is_staged`]) never
///   sees a partially written file at the final path. Any error after the temp
///   is created removes it, so a failed stage leaves no litter.
///
/// Rejects an unsafe `hash`/`ext` (path separators, `..`, empty hash) so the
/// destination can never escape the cache dir. Trusts the caller's `hash` as
/// the content identity: a source mutated to *different content of the same
/// length* after this returns is not detected (re-hashing 100s of MB per open
/// is prohibitive) — but changed content normally yields a different `hash`,
/// hence a different destination.
pub fn stage(base: &Path, src: &Path, hash: &str, ext: &str) -> FolioResult<PathBuf> {
    let file_name = validated_file_name(hash, ext)?;
    let dest = base.join(SOURCE_CACHE_DIR).join(&file_name);

    // Serialize stagers of THIS destination; other destinations are unaffected.
    let lock = lock_for(&file_name);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

    // Measure the source UNDER the lock (also surfaces a missing/unreadable
    // source as an error), so the size we validate the copy against is the one
    // in effect for this staging attempt.
    let src_len = std::fs::metadata(src)?.len();

    // Re-check under the lock: another stager may have finished while we waited.
    // `symlink_metadata` so a symlink planted at `dest` isn't trusted.
    if let Ok(m) = std::fs::symlink_metadata(&dest) {
        if m.file_type().is_file() && m.len() == src_len {
            return Ok(dest);
        }
    }

    let dir = dest
        .parent()
        .ok_or_else(|| FolioError::internal("source-cache path has no parent"))?;
    // The temp lives in `source-cache/tmp/`, still on the same filesystem as
    // `dest` so the final rename is a cheap intra-filesystem move. Being one
    // level deeper than any staged destination, its path can never collide with
    // another key's destination. The per-file lock means no other stager of
    // this destination writes the same temp path concurrently.
    let tmp_dir = dir.join(SOURCE_CACHE_TMP_DIR);
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join(format!("{file_name}.tmp"));
    // Remove any stale temp (e.g. leftover from a crashed prior stage, or a
    // pre-planted symlink) so `fs::copy` writes a fresh regular file rather
    // than following a symlink.
    let _ = std::fs::remove_file(&tmp);

    // Any failure from here on must not leave the temp behind.
    let publish = || -> FolioResult<()> {
        std::fs::copy(src, &tmp)?;
        // `fs::copy` carries the source's permissions, so a read-only source
        // (e.g. mode 0444 on a share) yields a read-only staged file. Ensure the
        // owner can write it, so `touch_staged` (which needs a write handle for
        // `set_modified`) can bump the mtime — otherwise the file's LRU position
        // would freeze and it could be evicted while actively read. Best-effort.
        if let Ok(meta) = std::fs::metadata(&tmp) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                if mode & 0o200 == 0 {
                    let mut perms = meta.permissions();
                    perms.set_mode(mode | 0o200); // add owner-write only
                    let _ = std::fs::set_permissions(&tmp, perms);
                }
            }
            #[cfg(not(unix))]
            {
                let mut perms = meta.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(&tmp, perms);
                }
            }
        }
        // Guard against a short copy (e.g. the source shrank mid-copy): never
        // publish a file whose length doesn't match the source we measured.
        let copied = std::fs::metadata(&tmp)?.len();
        if copied != src_len {
            return Err(FolioError::io(format!(
                "staged copy size mismatch: {copied} != {src_len}"
            )));
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    };
    if let Err(e) = publish() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Write `bytes` to a fresh file under `dir` and return its path.
    fn write_src(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// Names of any leftover files in the temp subdir (`source-cache/tmp/`).
    /// Empty on both success (renamed away) and clean failure (removed).
    fn tmp_leftovers(base: &Path) -> Vec<String> {
        let tmp_dir = base.join(SOURCE_CACHE_DIR).join(SOURCE_CACHE_TMP_DIR);
        match std::fs::read_dir(&tmp_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(), // subdir may not exist — that's zero leftovers
        }
    }

    #[test]
    fn staged_path_shape() {
        let base = Path::new("/cache");
        assert_eq!(
            staged_path(base, "abc123", "pdf"),
            Path::new("/cache/source-cache/abc123.pdf")
        );
        // Empty extension → bare hash, no trailing dot.
        assert_eq!(
            staged_path(base, "abc123", ""),
            Path::new("/cache/source-cache/abc123")
        );
    }

    #[test]
    fn stage_copies_file_and_returns_staged_path() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let data = b"hello remote comic bytes";
        let src = write_src(srcdir.path(), "book.pdf", data);

        let dest = stage(base.path(), &src, "deadbeef", "pdf").unwrap();

        assert_eq!(dest, staged_path(base.path(), "deadbeef", "pdf"));
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn is_staged_reflects_presence_and_size() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let data = b"0123456789";
        let src = write_src(srcdir.path(), "b.pdf", data);

        assert!(!is_staged(base.path(), "h1", "pdf", data.len() as u64));
        stage(base.path(), &src, "h1", "pdf").unwrap();
        assert!(is_staged(base.path(), "h1", "pdf", data.len() as u64));
        // Wrong expected size ⇒ not a valid staged copy.
        assert!(!is_staged(base.path(), "h1", "pdf", 999));
    }

    #[test]
    fn staged_if_present_reflects_staged_copy() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "b.pdf", b"payload");

        // Absent before staging.
        assert!(staged_if_present(base.path(), "hp", "pdf").is_none());
        // Present (and equal to staged_path) after staging.
        let dest = stage(base.path(), &src, "hp", "pdf").unwrap();
        assert_eq!(staged_if_present(base.path(), "hp", "pdf"), Some(dest));
        // Unsafe key never resolves.
        assert!(staged_if_present(base.path(), "../escape", "pdf").is_none());
    }

    /// Pin a file's modified-time to `UNIX_EPOCH + secs` for deterministic LRU
    /// ordering in eviction tests.
    fn set_mtime(path: &Path, secs: u64) {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn one_mb() -> Vec<u8> {
        vec![0u8; 1024 * 1024]
    }

    #[test]
    fn run_eviction_removes_oldest_until_within_budget() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "s.bin", &one_mb());

        let a = stage(base.path(), &src, "aaa", "bin").unwrap();
        let b = stage(base.path(), &src, "bbb", "bin").unwrap();
        let c = stage(base.path(), &src, "ccc", "bin").unwrap();
        set_mtime(&a, 100); // oldest
        set_mtime(&b, 200);
        set_mtime(&c, 300); // newest

        // 3 MB staged, 2 MB budget ⇒ evict the single oldest (a).
        run_eviction(base.path(), 2).unwrap();

        assert!(staged_if_present(base.path(), "aaa", "bin").is_none());
        assert!(staged_if_present(base.path(), "bbb", "bin").is_some());
        assert!(staged_if_present(base.path(), "ccc", "bin").is_some());
    }

    #[test]
    fn run_eviction_noop_when_under_budget() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "s.bin", &one_mb());
        stage(base.path(), &src, "keep1", "bin").unwrap();
        stage(base.path(), &src, "keep2", "bin").unwrap();

        run_eviction(base.path(), 100).unwrap();

        assert!(staged_if_present(base.path(), "keep1", "bin").is_some());
        assert!(staged_if_present(base.path(), "keep2", "bin").is_some());
    }

    #[test]
    fn run_eviction_skips_tmp_subdir() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "s.bin", &one_mb());
        stage(base.path(), &src, "staged", "bin").unwrap();

        // Plant a large file inside the tmp/ subdir; eviction must ignore it
        // (neither count it toward the budget nor delete it).
        let tmp_dir = base
            .path()
            .join(SOURCE_CACHE_DIR)
            .join(SOURCE_CACHE_TMP_DIR);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp_file = tmp_dir.join("in-progress.tmp");
        std::fs::write(&tmp_file, one_mb()).unwrap();

        // Tiny budget: would evict everything countable, but tmp/ is off-limits.
        run_eviction(base.path(), 0).unwrap();

        assert!(tmp_file.exists(), "tmp/ contents must never be evicted");
    }

    #[test]
    fn touch_staged_marks_recent() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "s.bin", b"x");
        let dest = stage(base.path(), &src, "touchme", "bin").unwrap();
        set_mtime(&dest, 1_000);

        touch_staged(base.path(), "touchme", "bin");

        let m = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert!(
            m > SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
            "touch must advance the mtime"
        );
    }

    #[test]
    fn remove_staged_deletes_copy() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "s.bin", b"x");
        stage(base.path(), &src, "goner", "bin").unwrap();
        assert!(staged_if_present(base.path(), "goner", "bin").is_some());

        remove_staged(base.path(), "goner", "bin");

        assert!(staged_if_present(base.path(), "goner", "bin").is_none());
    }

    #[test]
    fn remove_staged_also_removes_inflight_temp() {
        let base = TempDir::new().unwrap();
        let tmp_dir = base
            .path()
            .join(SOURCE_CACHE_DIR)
            .join(SOURCE_CACHE_TMP_DIR);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        // An in-flight temp for key ("h","pdf") — as a mid-copy stage would have.
        let temp = tmp_dir.join("h.pdf.tmp");
        std::fs::write(&temp, b"partial").unwrap();

        remove_staged(base.path(), "h", "pdf");

        assert!(!temp.exists(), "the in-flight temp must be removed too");
    }

    #[test]
    fn run_eviction_reclaims_stale_temp_but_keeps_fresh() {
        let base = TempDir::new().unwrap();
        let tmp_dir = base
            .path()
            .join(SOURCE_CACHE_DIR)
            .join(SOURCE_CACHE_TMP_DIR);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let stale = tmp_dir.join("abandoned.tmp");
        std::fs::write(&stale, b"crashed copy").unwrap();
        set_mtime(&stale, 1_000); // 1970 — far older than STALE_TMP_SECS

        let fresh = tmp_dir.join("in-progress.tmp");
        std::fs::write(&fresh, b"live copy").unwrap(); // mtime ≈ now

        // Runs even under budget (reclaim happens before the size check).
        run_eviction(base.path(), 100).unwrap();

        assert!(!stale.exists(), "an abandoned temp must be reclaimed");
        assert!(fresh.exists(), "a fresh (live) temp must be left alone");
    }

    #[cfg(unix)]
    #[test]
    fn run_eviction_excludes_symlinks() {
        use std::os::unix::fs::symlink;
        let base = TempDir::new().unwrap();
        let cache = base.path().join(SOURCE_CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        // A large real file OUTSIDE the cache, symlinked INTO it.
        let target = base.path().join("big.bin");
        std::fs::write(&target, one_mb()).unwrap();
        let link = cache.join("linkhash.bin");
        symlink(&target, &link).unwrap();

        // Budget 0 would evict everything countable, but a symlink is neither
        // counted nor removed (the security comment leans on this).
        run_eviction(base.path(), 0).unwrap();

        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "a symlink under source-cache/ must not be evicted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_makes_readonly_source_copy_writable() {
        use std::os::unix::fs::PermissionsExt;
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "ro.bin", b"read-only source");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o444)).unwrap();

        let dest = stage(base.path(), &src, "rohash", "bin").unwrap();

        // The staged copy must be writable so LRU touch works.
        assert!(!std::fs::metadata(&dest).unwrap().permissions().readonly());
        set_mtime(&dest, 1_000);
        touch_staged(base.path(), "rohash", "bin");
        let m = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert!(
            m > SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000),
            "touch must advance the mtime of a copy from a read-only source"
        );
    }

    #[test]
    fn stage_is_idempotent() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let data = b"same content twice";
        let src = write_src(srcdir.path(), "b.pdf", data);

        let a = stage(base.path(), &src, "h2", "pdf").unwrap();
        let b = stage(base.path(), &src, "h2", "pdf").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&b).unwrap(), data);
    }

    #[test]
    fn stage_leaves_no_temp_file() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "b.pdf", b"payload");

        stage(base.path(), &src, "h3", "pdf").unwrap();

        assert!(
            tmp_leftovers(base.path()).is_empty(),
            "temp files left behind: {:?}",
            tmp_leftovers(base.path())
        );
    }

    #[test]
    fn stage_missing_source_errors() {
        let base = TempDir::new().unwrap();
        let missing = base.path().join("does-not-exist.pdf");
        assert!(stage(base.path(), &missing, "h4", "pdf").is_err());
    }

    #[test]
    fn stage_rejects_unsafe_keys() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "b.pdf", b"x");

        // Traversal / separators / absolute / empty in the hash.
        for bad in ["..", "../evil", "/etc/passwd", "a/b", "", "a:b", "a\0b"] {
            assert!(
                stage(base.path(), &src, bad, "pdf").is_err(),
                "hash {bad:?} must be rejected"
            );
        }
        // Separators / traversal in the extension.
        for bad_ext in ["a/b", "..", "e\\f"] {
            assert!(
                stage(base.path(), &src, "goodhash", bad_ext).is_err(),
                "ext {bad_ext:?} must be rejected"
            );
        }
        // Nothing escaped into (or above) the cache dir.
        assert!(!base.path().join("source-cache").join("evil").exists());
    }

    #[test]
    fn is_staged_false_for_unsafe_key() {
        let base = TempDir::new().unwrap();
        assert!(!is_staged(base.path(), "../escape", "pdf", 0));
        assert!(!is_staged(base.path(), "", "pdf", 0));
    }

    #[test]
    fn stage_empty_ext_round_trip() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let data = b"no extension here";
        let src = write_src(srcdir.path(), "raw", data);

        let dest = stage(base.path(), &src, "barehash", "").unwrap();

        assert_eq!(dest, staged_path(base.path(), "barehash", ""));
        assert_eq!(dest.file_name().unwrap(), "barehash");
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(is_staged(base.path(), "barehash", "", data.len() as u64));
        // The temp for a bare hash is `tmp/barehash.tmp` — must not linger.
        assert!(tmp_leftovers(base.path()).is_empty());
    }

    #[test]
    fn stage_removes_temp_when_publish_fails() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let src = write_src(srcdir.path(), "b.pdf", b"some bytes");

        // Pre-create the destination as a DIRECTORY so the final rename fails
        // AFTER the temp copy succeeds — exercising the error-path cleanup.
        let dest = staged_path(base.path(), "collide", "pdf");
        std::fs::create_dir_all(&dest).unwrap();

        assert!(stage(base.path(), &src, "collide", "pdf").is_err());

        assert!(
            tmp_leftovers(base.path()).is_empty(),
            "temp left after failed publish: {:?}",
            tmp_leftovers(base.path())
        );
    }

    /// Regression: a key whose destination equals another key's *temp* name
    /// must not collide. `("foo","pdf")` (temp `tmp/foo.pdf.tmp`) and
    /// `(".foo.pdf","tmp")` (destination `.foo.pdf.tmp`) once shared a path in
    /// the flat namespace; with temps in `tmp/` they are fully independent.
    #[test]
    fn stage_temp_name_does_not_collide_with_another_destination() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        let a = write_src(srcdir.path(), "a", b"content of foo.pdf");
        let b = write_src(srcdir.path(), "b", b"different content entirely");

        let da = stage(base.path(), &a, "foo", "pdf").unwrap();
        let db = stage(base.path(), &b, ".foo.pdf", "tmp").unwrap();

        assert_ne!(da, db);
        assert_eq!(std::fs::read(&da).unwrap(), b"content of foo.pdf");
        assert_eq!(std::fs::read(&db).unwrap(), b"different content entirely");
        assert!(tmp_leftovers(base.path()).is_empty());
    }

    #[test]
    fn stage_single_flight_concurrent_same_hash() {
        let base = TempDir::new().unwrap();
        let srcdir = TempDir::new().unwrap();
        // A payload big enough that concurrent copies would overlap in time.
        let data = Arc::new(vec![7u8; 2 * 1024 * 1024]);
        let src = write_src(srcdir.path(), "big.pdf", &data);

        let base_path = base.path().to_path_buf();
        let ok = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let bp = base_path.clone();
                let sp = src.clone();
                let ok = Arc::clone(&ok);
                let data = Arc::clone(&data);
                s.spawn(move || {
                    let dest = stage(&bp, &sp, "race", "pdf").unwrap();
                    if std::fs::read(&dest).unwrap() == *data {
                        ok.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(
            ok.load(Ordering::SeqCst),
            8,
            "all stagers see intact content"
        );
        // No temp litter after the race.
        let cache_dir = base_path.join(SOURCE_CACHE_DIR);
        let tmp_count = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(tmp_count, 0);
    }
}

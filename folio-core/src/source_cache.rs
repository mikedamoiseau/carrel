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
/// OS processes staging the same file would not serialize. Folio runs as a
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

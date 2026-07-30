//! Storage abstraction for Carrel's persistent artifacts.
//!
//! Book files, covers, and related blobs are accessed through a `Storage`
//! trait so the backend can be swapped (local filesystem today, S3 or other
//! object stores in the paid Carrel Server). The desktop app uses
//! [`LocalStorage`] rooted at the library folder; on-disk layout stays
//! identical to the pre-refactor code, with the one behavior *improvement*
//! that overwriting writes are now atomic (temp-file + rename) so a failed
//! write cannot truncate an existing object.
//!
//! # Key scheme
//!
//! Keys are opaque UTF-8 strings using `/` as the one and only separator.
//! They are always relative — leading slashes, backslashes anywhere,
//! empty segments, and `..` / `.` segments are rejected. Valid examples:
//!
//! - `books/abc123.epub`
//! - `covers/42/cover.jpg`
//! - `a.epub`
//!
//! # Roadmap
//!
//! See CHANGELOG 2.0.0, "Pluggable `Storage` trait".

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{CarrelError, CarrelResult};

/// Backend-agnostic interface for reading and writing Carrel's persistent
/// artifacts.
///
/// All implementations must be thread-safe (`Send + Sync`). Keys must pass
/// [`validate_key`] — paths containing `..`, absolute paths, or empty
/// segments are rejected.
pub trait Storage: Send + Sync {
    /// Write the given bytes at `key`, overwriting any existing object.
    fn put(&self, key: &str, bytes: &[u8]) -> CarrelResult<()>;

    /// Read the entire object at `key` into memory.
    fn get(&self, key: &str) -> CarrelResult<Vec<u8>>;

    /// Return `true` if an object exists at `key`.
    fn exists(&self, key: &str) -> CarrelResult<bool>;

    /// Delete the object at `key`. Deleting a missing key is a no-op.
    fn delete(&self, key: &str) -> CarrelResult<()>;

    /// List all keys whose path starts with `prefix`. Pass `""` to list
    /// everything. Returns keys in implementation-defined order.
    fn list(&self, prefix: &str) -> CarrelResult<Vec<String>>;

    /// Return the byte length of the object at `key`.
    fn size(&self, key: &str) -> CarrelResult<u64>;

    /// Copy a file from the local filesystem into storage at `key`.
    ///
    /// Backends that can do this efficiently (e.g. `LocalStorage` with
    /// `std::fs::copy`) should override; the default falls back to
    /// read-then-put.
    fn put_path(&self, key: &str, src: &Path) -> CarrelResult<()> {
        let bytes = fs::read(src)?;
        self.put(key, &bytes)
    }

    /// Resolve `key` to a local filesystem path that callers can hand to
    /// libraries requiring `&Path` access (pdfium, zip, unrar).
    ///
    /// For [`LocalStorage`] this returns the underlying path directly.
    /// Remote backends must first materialize the object to a local cache
    /// — those implementations are introduced in the paid Carrel Server
    /// crate.
    fn local_path(&self, key: &str) -> CarrelResult<PathBuf>;
}

/// Reject keys that are absolute, empty, contain `..` / empty segments, or
/// contain a backslash.
///
/// Backslashes are rejected outright: the documented separator is `/`, and
/// accepting `\` would let the same logical key map to different on-disk
/// layouts depending on platform (Unix treats `books\x` as one filename
/// with a literal backslash; Windows-flavored callers might expect it to
/// behave as a path separator). Rejecting keeps the contract backend-stable.
///
/// Returned error is [`CarrelError::InvalidInput`] so callers surface a
/// consistent message to users.
pub fn validate_key(key: &str) -> CarrelResult<()> {
    if key.is_empty() {
        return Err(CarrelError::invalid("storage key is empty"));
    }
    if key.contains('\\') {
        return Err(CarrelError::invalid(format!(
            "storage key must not contain backslashes: {key}"
        )));
    }
    if key.starts_with('/') {
        return Err(CarrelError::invalid(format!(
            "storage key must be relative: {key}"
        )));
    }
    // Reject Windows drive prefixes like `C:`.
    if key.len() >= 2 && key.as_bytes()[1] == b':' {
        return Err(CarrelError::invalid(format!(
            "storage key must be relative: {key}"
        )));
    }
    for segment in key.split('/') {
        if segment.is_empty() {
            return Err(CarrelError::invalid(format!(
                "storage key has empty segment: {key}"
            )));
        }
        if segment == ".." || segment == "." {
            return Err(CarrelError::invalid(format!(
                "storage key contains traversal segment: {key}"
            )));
        }
    }
    Ok(())
}

/// Derive the storage key for an absolute local path under a given root.
///
/// Used by the desktop app's M4 migration (converting legacy absolute
/// `file_path` values to storage keys) and by the `remove_book` path
/// when it receives a legacy row. Returns `None` when `path` does not
/// sit under `root` — for linked books whose file lives elsewhere, and
/// for rows imported under a different library folder that has since
/// been changed.
///
/// Returned keys use `/` as the separator, matching the [`Storage`]
/// contract, regardless of the underlying platform.
pub fn key_for_local_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root).ok().and_then(|rel| {
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("/"))
        }
    })
}

/// Filesystem-backed [`Storage`] rooted at a directory on disk.
///
/// The desktop app configures one `LocalStorage` per profile, pointing at
/// the user's library folder. Every key resolves to
/// `{root}/{key}` with `/` translated to the platform separator.
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// Create a new `LocalStorage` rooted at `root`. The directory is
    /// created if it does not already exist.
    pub fn new(root: impl Into<PathBuf>) -> CarrelResult<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Return the root directory for this storage.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, key: &str) -> CarrelResult<PathBuf> {
        validate_key(key)?;
        let mut path = self.root.clone();
        for segment in key.split('/') {
            path.push(segment);
        }
        Ok(path)
    }
}

impl Storage for LocalStorage {
    fn put(&self, key: &str, bytes: &[u8]) -> CarrelResult<()> {
        let dest = self.resolve(key)?;
        write_atomic(&dest, |f| f.write_all(bytes))
    }

    fn get(&self, key: &str) -> CarrelResult<Vec<u8>> {
        let path = self.resolve(key)?;
        Ok(fs::read(&path)?)
    }

    fn exists(&self, key: &str) -> CarrelResult<bool> {
        let path = self.resolve(key)?;
        Ok(path.exists())
    }

    fn delete(&self, key: &str) -> CarrelResult<()> {
        let path = self.resolve(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn list(&self, prefix: &str) -> CarrelResult<Vec<String>> {
        // A prefix is a starts-with filter — not a key. Reject absolute and
        // traversal forms, but accept trailing slashes and other shapes that
        // are invalid as keys.
        if prefix.starts_with('/') || prefix.contains('\\') {
            return Err(CarrelError::invalid(format!(
                "list prefix must be relative and slash-separated: {prefix}"
            )));
        }
        if prefix.split('/').any(|s| s == "..") {
            return Err(CarrelError::invalid(format!(
                "list prefix contains traversal segment: {prefix}"
            )));
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        if !prefix.is_empty() {
            out.retain(|k| k.starts_with(prefix));
        }
        Ok(out)
    }

    fn size(&self, key: &str) -> CarrelResult<u64> {
        let path = self.resolve(key)?;
        Ok(fs::metadata(&path)?.len())
    }

    fn put_path(&self, key: &str, src: &Path) -> CarrelResult<()> {
        let dest = self.resolve(key)?;
        write_atomic(&dest, |f| {
            let mut src_file = fs::File::open(src)?;
            std::io::copy(&mut src_file, f)?;
            Ok(())
        })
    }

    fn local_path(&self, key: &str) -> CarrelResult<PathBuf> {
        self.resolve(key)
    }
}

/// Write to `dest` atomically: the caller's closure writes into a temp file
/// in the same parent directory, the file is flushed to disk, and only on
/// success is it renamed over `dest`. A failure partway through leaves the
/// existing destination untouched, and the temp file is cleaned up.
///
/// Public so callers outside this module with their own already-resolved
/// absolute paths (e.g. the web server's cover-thumbnail cache in
/// `carrel::web_server::api`) can reuse the same atomicity guarantee instead
/// of duplicating the temp-file-plus-rename dance.
pub fn write_atomic<F>(dest: &Path, write: F) -> CarrelResult<()>
where
    F: FnOnce(&mut fs::File) -> std::io::Result<()>,
{
    let parent = dest
        .parent()
        .ok_or_else(|| CarrelError::invalid("destination has no parent directory"))?;
    fs::create_dir_all(parent)?;

    // Unique sibling temp name so we can rename atomically onto `dest`.
    let tmp_name = format!(
        ".{}.tmp.{}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("carrel"),
        uuid::Uuid::new_v4()
    );
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp_path)?;
        write(&mut f)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup; ignore errors (e.g. temp already gone).
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    if let Err(e) = fs::rename(&tmp_path, dest) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    Ok(())
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> CarrelResult<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(base) {
            // Normalize separators to `/` so keys remain portable across
            // platforms — a Windows-written key still reads on Linux.
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(key);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_storage() -> (TempDir, LocalStorage) {
        let dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(dir.path()).unwrap();
        (dir, storage)
    }

    // --- validate_key ---

    #[test]
    fn validate_key_accepts_simple_keys() {
        assert!(validate_key("a.epub").is_ok());
        assert!(validate_key("books/abc.epub").is_ok());
        assert!(validate_key("covers/42/cover.jpg").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn validate_key_rejects_absolute() {
        assert!(validate_key("/books/x").is_err());
        assert!(validate_key("\\books\\x").is_err());
        assert!(validate_key("C:/books/x").is_err());
    }

    #[test]
    fn validate_key_rejects_traversal() {
        assert!(validate_key("../etc/passwd").is_err());
        assert!(validate_key("books/../secret").is_err());
        assert!(validate_key("books/./x").is_err());
    }

    #[test]
    fn validate_key_rejects_empty_segments() {
        assert!(validate_key("books//x").is_err());
        assert!(validate_key("books/").is_err());
    }

    // --- LocalStorage round-trips ---

    #[test]
    fn local_storage_creates_root_if_missing() {
        let parent = TempDir::new().unwrap();
        let root = parent.path().join("does-not-exist-yet");
        assert!(!root.exists());
        let _ = LocalStorage::new(&root).unwrap();
        assert!(root.exists());
    }

    #[test]
    fn put_and_get_round_trip() {
        let (_d, storage) = temp_storage();
        storage.put("a.txt", b"hello").unwrap();
        assert_eq!(storage.get("a.txt").unwrap(), b"hello");
    }

    #[test]
    fn put_creates_nested_dirs() {
        let (_d, storage) = temp_storage();
        storage.put("books/2024/x.epub", b"data").unwrap();
        assert_eq!(storage.get("books/2024/x.epub").unwrap(), b"data");
    }

    #[test]
    fn exists_reports_presence() {
        let (_d, storage) = temp_storage();
        assert!(!storage.exists("nope").unwrap());
        storage.put("here", b"").unwrap();
        assert!(storage.exists("here").unwrap());
    }

    #[test]
    fn delete_removes_object() {
        let (_d, storage) = temp_storage();
        storage.put("gone", b"x").unwrap();
        storage.delete("gone").unwrap();
        assert!(!storage.exists("gone").unwrap());
    }

    #[test]
    fn delete_missing_key_is_noop() {
        let (_d, storage) = temp_storage();
        // Should not error even though nothing is there.
        storage.delete("never-existed").unwrap();
    }

    #[test]
    fn size_returns_byte_length() {
        let (_d, storage) = temp_storage();
        storage.put("k", b"abcdef").unwrap();
        assert_eq!(storage.size("k").unwrap(), 6);
    }

    #[test]
    fn size_missing_key_errors() {
        let (_d, storage) = temp_storage();
        assert!(storage.size("missing").is_err());
    }

    // --- list ---

    #[test]
    fn list_returns_all_keys_when_prefix_empty() {
        let (_d, storage) = temp_storage();
        storage.put("a", b"").unwrap();
        storage.put("b/c", b"").unwrap();
        storage.put("b/d/e", b"").unwrap();
        let mut all = storage.list("").unwrap();
        all.sort();
        assert_eq!(all, vec!["a".to_string(), "b/c".into(), "b/d/e".into()]);
    }

    #[test]
    fn list_filters_by_prefix() {
        let (_d, storage) = temp_storage();
        storage.put("books/a", b"").unwrap();
        storage.put("books/b", b"").unwrap();
        storage.put("covers/c", b"").unwrap();
        let mut books = storage.list("books/").unwrap();
        books.sort();
        assert_eq!(books, vec!["books/a".to_string(), "books/b".into()]);
    }

    #[test]
    fn list_empty_storage_returns_empty_vec() {
        let (_d, storage) = temp_storage();
        assert!(storage.list("").unwrap().is_empty());
    }

    // --- put_path ---

    #[test]
    fn put_path_copies_from_filesystem() {
        let (_d, storage) = temp_storage();
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("src.epub");
        fs::write(&src, b"book bytes").unwrap();
        storage.put_path("books/x.epub", &src).unwrap();
        assert_eq!(storage.get("books/x.epub").unwrap(), b"book bytes");
    }

    #[test]
    fn put_path_creates_nested_dirs() {
        let (_d, storage) = temp_storage();
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("src");
        fs::write(&src, b"data").unwrap();
        storage.put_path("deeply/nested/key", &src).unwrap();
        assert!(storage.exists("deeply/nested/key").unwrap());
    }

    // --- local_path ---

    #[test]
    fn local_path_returns_resolved_path() {
        let (d, storage) = temp_storage();
        let p = storage.local_path("books/x.epub").unwrap();
        assert_eq!(p, d.path().join("books").join("x.epub"));
    }

    #[test]
    fn local_path_rejects_traversal() {
        let (_d, storage) = temp_storage();
        assert!(storage.local_path("../escape").is_err());
    }

    // --- invalid keys ---

    #[test]
    fn put_rejects_invalid_key() {
        let (_d, storage) = temp_storage();
        assert!(storage.put("../escape", b"x").is_err());
        assert!(storage.put("", b"x").is_err());
    }

    #[test]
    fn get_rejects_invalid_key() {
        let (_d, storage) = temp_storage();
        assert!(storage.get("/abs").is_err());
    }

    // --- trait object usability ---

    #[test]
    fn storage_is_usable_as_trait_object() {
        let (_d, storage) = temp_storage();
        let dyn_storage: &dyn Storage = &storage;
        dyn_storage.put("k", b"v").unwrap();
        assert_eq!(dyn_storage.get("k").unwrap(), b"v");
    }

    // --- backslash rejection ---

    #[test]
    fn validate_key_rejects_backslash_anywhere() {
        assert!(validate_key("books\\x.epub").is_err());
        assert!(validate_key("a\\b").is_err());
        // Even a single trailing backslash.
        assert!(validate_key("a\\").is_err());
    }

    #[test]
    fn put_rejects_backslash_key() {
        let (_d, storage) = temp_storage();
        assert!(storage.put("books\\x.epub", b"data").is_err());
    }

    // --- atomic overwrite behavior ---

    #[test]
    fn put_path_failure_preserves_existing_object() {
        let (_d, storage) = temp_storage();
        storage.put("book.epub", b"original-contents").unwrap();
        // Source does not exist; the copy into the temp file must fail.
        let missing = PathBuf::from("/definitely/does/not/exist/source.epub");
        let result = storage.put_path("book.epub", &missing);
        assert!(result.is_err(), "put_path should fail on missing source");
        // Original bytes must be untouched — no truncation, no partial write.
        assert_eq!(storage.get("book.epub").unwrap(), b"original-contents");
    }

    #[test]
    fn put_path_failure_leaves_no_temp_files_behind() {
        let (d, storage) = temp_storage();
        storage.put("book.epub", b"original").unwrap();
        let missing = PathBuf::from("/definitely/does/not/exist/source.epub");
        let _ = storage.put_path("book.epub", &missing);
        // The parent directory should only contain the original file, no
        // `.tmp.*` stragglers.
        let entries: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["book.epub".to_string()]);
    }

    #[test]
    fn put_overwrites_existing_object() {
        // Happy-path sanity check: atomic writes still overwrite.
        let (_d, storage) = temp_storage();
        storage.put("book.epub", b"v1").unwrap();
        storage.put("book.epub", b"v2-longer-than-v1").unwrap();
        assert_eq!(storage.get("book.epub").unwrap(), b"v2-longer-than-v1");
    }

    // --- key_for_local_path (#64 M4) ---

    #[test]
    fn key_for_local_path_strips_root_prefix() {
        let root = Path::new("/library");
        let path = Path::new("/library/abc.epub");
        assert_eq!(key_for_local_path(root, path), Some("abc.epub".to_string()));
    }

    #[test]
    fn key_for_local_path_handles_nested_paths() {
        let root = Path::new("/library");
        let path = Path::new("/library/books/abc.epub");
        assert_eq!(
            key_for_local_path(root, path),
            Some("books/abc.epub".to_string())
        );
    }

    #[test]
    fn key_for_local_path_returns_none_for_external_path() {
        let root = Path::new("/library");
        let path = Path::new("/elsewhere/book.epub");
        assert!(key_for_local_path(root, path).is_none());
    }

    #[test]
    fn key_for_local_path_handles_trailing_slash_on_root() {
        // Path::strip_prefix normalizes trailing separators.
        let root = Path::new("/library/");
        let path = Path::new("/library/abc.epub");
        assert_eq!(key_for_local_path(root, path), Some("abc.epub".to_string()));
    }

    #[test]
    fn key_for_local_path_none_when_path_equals_root() {
        let root = Path::new("/library");
        let path = Path::new("/library");
        assert!(key_for_local_path(root, path).is_none());
    }
}

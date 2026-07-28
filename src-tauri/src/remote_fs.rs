//! Detect whether a path lives on a **network-mounted** filesystem.
//!
//! Books imported in "link" mode are read in place. When that place is an SMB/
//! NFS share, rendering a PDF/comic does random-access reads over the network
//! at render time — punishingly slow. The source-cache stages such files onto
//! local disk first; this module answers the "is it worth staging?" question so
//! we never needlessly copy an already-local file.
//!
//! macOS: `statfs(2)` reports `MNT_LOCAL` for locally-attached filesystems; its
//! absence means the mount is remote. Other platforms currently return `false`
//! (conservative — never stage), since this optimization targets the macOS
//! desktop first; a real Linux/Windows detector can replace the stub later.

use std::path::Path;

/// True when `path` resolves onto a network-mounted filesystem.
///
/// Conservative on failure: if the filesystem type can't be determined (path
/// missing, syscall error, unsupported platform), returns `false` so we treat
/// it as local and skip staging rather than copy something we shouldn't.
#[cfg(target_os = "macos")]
pub fn is_remote_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false; // path contained an interior NUL — treat as undeterminable
    };
    // SAFETY: `statfs` fills the caller-provided buffer; we pass a valid
    // NUL-terminated path and a zeroed, correctly-typed `statfs` struct, and
    // only read the result when the call reports success (rc == 0).
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return false;
    }
    // `MNT_LOCAL` set ⇒ the filesystem is stored locally; absent ⇒ remote.
    (buf.f_flags & libc::MNT_LOCAL as u32) == 0
}

#[cfg(not(target_os = "macos"))]
pub fn is_remote_path(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly created temp directory is always on a local filesystem, so it
    /// must never be flagged remote — this pins the "don't stage local files"
    /// guarantee. (On non-macOS the fn is a `false` stub, so this also holds.)
    #[test]
    fn local_tempdir_is_not_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!is_remote_path(dir.path()));
    }

    /// A non-existent path can't be classified, so detection must fail closed
    /// to `false` (never treat an undeterminable path as remote).
    #[test]
    fn nonexistent_path_is_not_remote() {
        assert!(!is_remote_path(Path::new(
            "/this/path/definitely/does/not/exist/anywhere-xyz"
        )));
    }
}

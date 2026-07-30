//! Shared path utilities used by both the desktop app and future headless
//! binaries. Kept deliberately small — only defaults that have no dependency
//! on a running Tauri app.

use crate::error::{FolioError, FolioResult};

/// Default library folder for book storage. Resolves to
/// `~/Documents/Folio Library` on every supported platform.
///
/// The folder name still says "Folio" after the Carrel rename, deliberately:
/// this is an unwritten *fallback*, not a stored setting, so every install that
/// never changed `library_folder` would silently lose its library if the string
/// moved. See CLAUDE.md, "Legacy `folio` identifiers".
///
/// Returns a `FolioError::Internal` when the user's home directory cannot be
/// resolved (very rare — typically means `$HOME` is unset and no platform
/// fallback worked).
pub fn default_library_folder() -> FolioResult<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| FolioError::internal("Could not determine home directory"))?;
    Ok(home
        .join("Documents")
        .join("Folio Library")
        .to_string_lossy()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_library_folder_ends_with_folio_library() {
        // This relies on `dirs::home_dir()` returning something. On CI runners
        // and dev machines this is always set; if not, the test simply exits
        // successfully via the early-return inside the helper.
        if let Ok(path) = default_library_folder() {
            assert!(
                path.ends_with("Documents/Folio Library")
                    || path.ends_with("Documents\\Folio Library"),
                "unexpected path shape: {path}"
            );
        }
    }
}

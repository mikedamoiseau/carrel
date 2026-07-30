//! Shared path utilities used by both the desktop app and future headless
//! binaries. Kept deliberately small — only defaults that have no dependency
//! on a running Tauri app.

use crate::error::{CarrelError, CarrelResult};

/// Default library folder for book storage. Resolves to
/// `~/Documents/Carrel Library` on every supported platform.
///
/// This is an unwritten *fallback*, not a stored setting: an install that never
/// changed `library_folder` resolves it fresh on every launch, so changing this
/// string relocates that install's library. Change it only alongside a
/// migration.
///
/// Returns a `CarrelError::Internal` when the user's home directory cannot be
/// resolved (very rare — typically means `$HOME` is unset and no platform
/// fallback worked).
pub fn default_library_folder() -> CarrelResult<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| CarrelError::internal("Could not determine home directory"))?;
    Ok(home
        .join("Documents")
        .join("Carrel Library")
        .to_string_lossy()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_library_folder_ends_with_carrel_library() {
        // This relies on `dirs::home_dir()` returning something. On CI runners
        // and dev machines this is always set; if not, the test simply exits
        // successfully via the early-return inside the helper.
        if let Ok(path) = default_library_folder() {
            assert!(
                path.ends_with("Documents/Carrel Library")
                    || path.ends_with("Documents\\Carrel Library"),
                "unexpected path shape: {path}"
            );
        }
    }
}

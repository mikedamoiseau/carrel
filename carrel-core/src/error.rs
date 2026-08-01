//! Unified error type for the Carrel backend (#55).
//!
//! All fallible operations across the Rust backend return
//! [`CarrelResult<T>`] = `Result<T, CarrelError>`. The enum is categorized so
//! the frontend can map errors by `kind` rather than string-matching raw
//! messages, and so the forthcoming `carrel-core` crate (see roadmap #63) has
//! a stable typed error at its public surface.
//!
//! ## Serialization
//!
//! At the Tauri command boundary the error serializes as a JSON object:
//!
//! ```json
//! { "kind": "NotFound", "message": "Book file not found at /tmp/x.epub" }
//! ```
//!
//! The frontend's `friendlyError()` maps the `kind` to a translation key
//! first, falling back to message substring matching for backwards
//! compatibility.
//!
//! ## IPC contract stability
//!
//! The strings returned by [`CarrelError::kind`] (`"NotFound"`,
//! `"PermissionDenied"`, `"InvalidInput"`, `"Network"`, `"RateLimited"`,
//! `"Database"`, `"Io"`, `"Serialization"`, `"Internal"`) are a public
//! contract consumed by `src/lib/errors.ts`. **Renaming or removing a variant
//! is a breaking change** — update `KIND_TO_KEY` in the frontend in the same
//! commit. Adding a new variant is backwards-compatible: the frontend falls
//! back to message substring matching if it doesn't recognize a kind.
//!
//! ## Dependency direction (carrel-core extraction, #63)
//!
//! This module lives at the root of `carrel-core` with zero dependencies on
//! application-layer crates. Every `From<X> for CarrelError` impl targets a
//! type owned by a third-party crate, `std`, or `carrel-core` itself. Tauri-,
//! axum-, and other GUI/IPC-layer error conversions must stay in the
//! application crate (wrapping `CarrelError` from the outside).
//!
//! `From<EpubError>` and `From<SyncError>` impls will be added here when
//! those modules migrate into `carrel-core` in later extraction milestones.

use serde::{Serialize, Serializer};
use std::fmt;

/// Categorized error for all Carrel backend operations.
#[derive(Debug)]
pub enum CarrelError {
    /// Entity does not exist (book, file, setting row, page index, etc.).
    NotFound(String),
    /// OS-level permission denied (filesystem, keychain).
    PermissionDenied(String),
    /// Caller-supplied input is invalid (malformed archive, bad UUID, path
    /// escape, unsupported format).
    InvalidInput(String),
    /// Network-level failure (HTTP, DNS, timeout, connection refused,
    /// remote-storage transport errors).
    Network(String),
    /// Provider refused requests due to rate limiting (HTTP 429) and
    /// retries were exhausted.
    RateLimited(String),
    /// SQLite / r2d2 database error.
    Database(String),
    /// Filesystem I/O error (non-NotFound, non-PermissionDenied).
    Io(String),
    /// JSON or other serialization / deserialization failure.
    Serialization(String),
    /// The target profile has a soft-lock configured (see the `profile_lock`
    /// module) and has not been unlocked this desktop session. The frontend
    /// matches this `kind` to show the unlock prompt instead of a generic
    /// error toast.
    LockRequired(String),
    /// Everything else — lock poisoning, unexpected states, cancellation.
    Internal(String),
}

impl CarrelError {
    /// Stable string tag used in the serialized JSON payload.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "NotFound",
            Self::PermissionDenied(_) => "PermissionDenied",
            Self::InvalidInput(_) => "InvalidInput",
            Self::Network(_) => "Network",
            Self::RateLimited(_) => "RateLimited",
            Self::Database(_) => "Database",
            Self::Io(_) => "Io",
            Self::Serialization(_) => "Serialization",
            Self::LockRequired(_) => "LockRequired",
            Self::Internal(_) => "Internal",
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }
    pub fn network(msg: impl Into<String>) -> Self {
        Self::Network(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn permission(msg: impl Into<String>) -> Self {
        Self::PermissionDenied(msg.into())
    }
    pub fn database(msg: impl Into<String>) -> Self {
        Self::Database(msg.into())
    }
    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(msg.into())
    }
    pub fn lock_required(msg: impl Into<String>) -> Self {
        Self::LockRequired(msg.into())
    }
}

/// Replace the user's home directory prefix with `~` in error messages so
/// absolute paths don't leak into logs, bug reports, or serialized payloads.
/// Looked up lazily and cached — if `$HOME` is unset, messages pass through
/// unchanged.
fn redact_home(msg: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    use std::sync::OnceLock;
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
        // Skip redaction for obviously-generic roots so tests running as
        // `/` or `/root` still show useful paths.
        if h == "/" || h == "/root" {
            return None;
        }
        Some(h)
    });
    match home {
        Some(h) if msg.contains(h.as_str()) => Cow::Owned(msg.replace(h.as_str(), "~")),
        _ => Cow::Borrowed(msg),
    }
}

impl fmt::Display for CarrelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display the message only — no category prefix — so existing substring
        // matching on the frontend (file-not-found, URL blocked, too large, …)
        // keeps working until every call site migrates to kind-based routing.
        let raw = match self {
            Self::NotFound(m)
            | Self::PermissionDenied(m)
            | Self::InvalidInput(m)
            | Self::Network(m)
            | Self::RateLimited(m)
            | Self::Database(m)
            | Self::Io(m)
            | Self::Serialization(m)
            | Self::LockRequired(m)
            | Self::Internal(m) => m.as_str(),
        };
        f.write_str(&redact_home(raw))
    }
}

impl std::error::Error for CarrelError {}

impl Serialize for CarrelError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("CarrelError", 2)?;
        st.serialize_field("kind", self.kind())?;
        st.serialize_field("message", &self.to_string())?;
        st.end()
    }
}

// ---- From conversions for common backend errors ----

impl From<String> for CarrelError {
    fn from(s: String) -> Self {
        Self::Internal(s)
    }
}

impl From<&str> for CarrelError {
    fn from(s: &str) -> Self {
        Self::Internal(s.to_string())
    }
}

impl From<rusqlite::Error> for CarrelError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

impl From<r2d2::Error> for CarrelError {
    fn from(e: r2d2::Error) -> Self {
        Self::Database(e.to_string())
    }
}

impl From<std::io::Error> for CarrelError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => Self::NotFound(e.to_string()),
            ErrorKind::PermissionDenied => Self::PermissionDenied(e.to_string()),
            _ => Self::Io(e.to_string()),
        }
    }
}

impl From<serde_json::Error> for CarrelError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<reqwest::Error> for CarrelError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_decode() {
            Self::Serialization(e.to_string())
        } else {
            Self::Network(e.to_string())
        }
    }
}

impl From<zip::result::ZipError> for CarrelError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::InvalidInput(format!("not a valid zip: {e}"))
    }
}

impl From<keyring::Error> for CarrelError {
    fn from(e: keyring::Error) -> Self {
        match e {
            keyring::Error::NoEntry => Self::NotFound("keychain entry not found".to_string()),
            _ => Self::Internal(format!("keychain: {e}")),
        }
    }
}

impl From<opendal::Error> for CarrelError {
    fn from(e: opendal::Error) -> Self {
        use opendal::ErrorKind as K;
        match e.kind() {
            K::NotFound => Self::NotFound(e.to_string()),
            K::PermissionDenied => Self::PermissionDenied(e.to_string()),
            K::ConfigInvalid => Self::InvalidInput(e.to_string()),
            _ => Self::Network(e.to_string()),
        }
    }
}

impl From<uuid::Error> for CarrelError {
    fn from(e: uuid::Error) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<chrono::ParseError> for CarrelError {
    fn from(e: chrono::ParseError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<url::ParseError> for CarrelError {
    fn from(e: url::ParseError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<std::num::ParseIntError> for CarrelError {
    fn from(e: std::num::ParseIntError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl From<std::num::ParseFloatError> for CarrelError {
    fn from(e: std::num::ParseFloatError) -> Self {
        Self::InvalidInput(e.to_string())
    }
}

impl<T> From<std::sync::PoisonError<T>> for CarrelError {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        Self::Internal("lock poisoned".to_string())
    }
}

impl From<std::sync::mpsc::RecvError> for CarrelError {
    fn from(e: std::sync::mpsc::RecvError) -> Self {
        Self::Internal(format!("thread channel closed: {e}"))
    }
}

impl From<image::ImageError> for CarrelError {
    fn from(e: image::ImageError) -> Self {
        Self::Internal(format!("image: {e}"))
    }
}

/// Canonical `Result` alias for the Carrel backend.
pub type CarrelResult<T> = Result<T, CarrelError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tags_match_variants() {
        assert_eq!(CarrelError::NotFound("x".into()).kind(), "NotFound");
        assert_eq!(CarrelError::InvalidInput("x".into()).kind(), "InvalidInput");
        assert_eq!(CarrelError::Network("x".into()).kind(), "Network");
        assert_eq!(CarrelError::RateLimited("x".into()).kind(), "RateLimited");
        assert_eq!(CarrelError::Database("x".into()).kind(), "Database");
        assert_eq!(CarrelError::Io("x".into()).kind(), "Io");
        assert_eq!(
            CarrelError::PermissionDenied("x".into()).kind(),
            "PermissionDenied"
        );
        assert_eq!(
            CarrelError::Serialization("x".into()).kind(),
            "Serialization"
        );
        assert_eq!(CarrelError::LockRequired("x".into()).kind(), "LockRequired");
        assert_eq!(CarrelError::Internal("x".into()).kind(), "Internal");
    }

    #[test]
    fn display_is_just_the_message() {
        let e = CarrelError::NotFound("Book file not found".into());
        assert_eq!(e.to_string(), "Book file not found");
    }

    #[test]
    fn serializes_as_kind_and_message() {
        let e = CarrelError::InvalidInput("bad uuid".into());
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"InvalidInput\""));
        assert!(json.contains("\"message\":\"bad uuid\""));
    }

    #[test]
    fn io_not_found_maps_to_not_found() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err: CarrelError = io.into();
        assert_eq!(err.kind(), "NotFound");
    }

    #[test]
    fn io_permission_denied_maps_to_permission_denied() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: CarrelError = io.into();
        assert_eq!(err.kind(), "PermissionDenied");
    }

    #[test]
    fn rusqlite_error_maps_to_database() {
        let err: CarrelError = rusqlite::Error::InvalidQuery.into();
        assert_eq!(err.kind(), "Database");
    }

    #[test]
    fn keyring_no_entry_maps_to_not_found() {
        let err: CarrelError = keyring::Error::NoEntry.into();
        assert_eq!(err.kind(), "NotFound");
    }

    #[test]
    fn zip_error_maps_to_invalid_input() {
        let err: CarrelError = zip::result::ZipError::InvalidArchive("bad").into();
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[test]
    fn poison_error_maps_to_internal() {
        let mutex = std::sync::Mutex::new(());
        let _guard = mutex.lock().unwrap();
        // Manually construct a PoisonError since triggering it naturally
        // requires a panic in another thread.
        let err: CarrelError = std::sync::PoisonError::new(()).into();
        assert_eq!(err.kind(), "Internal");
        assert_eq!(err.to_string(), "lock poisoned");
    }

    #[test]
    fn mpsc_recv_error_maps_to_internal() {
        let err: CarrelError = std::sync::mpsc::RecvError.into();
        assert_eq!(err.kind(), "Internal");
        assert!(err.to_string().contains("thread channel closed"));
    }

    #[test]
    fn serde_json_error_maps_to_serialization() {
        let err: CarrelError = serde_json::from_str::<serde_json::Value>("not json")
            .unwrap_err()
            .into();
        assert_eq!(err.kind(), "Serialization");
    }

    #[test]
    fn uuid_parse_error_maps_to_invalid_input() {
        let err: CarrelError = uuid::Uuid::parse_str("not a uuid").unwrap_err().into();
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[test]
    fn url_parse_error_maps_to_invalid_input() {
        let err: CarrelError = url::Url::parse("::not a url").unwrap_err().into();
        assert_eq!(err.kind(), "InvalidInput");
    }

    #[test]
    fn display_redacts_home_dir() {
        // This test is best-effort: if $HOME is unset or a generic root,
        // redaction is a no-op and the assertion just holds trivially.
        let home = std::env::var("HOME").unwrap_or_default();
        let raw = format!("Cannot open {home}/Documents/book.epub");
        let err = CarrelError::not_found(raw);
        let rendered = err.to_string();
        if !home.is_empty() && home != "/" && home != "/root" {
            assert!(
                !rendered.contains(&home),
                "rendered message should redact $HOME, got: {rendered}"
            );
            assert!(rendered.contains("~/Documents/book.epub"));
        }
    }
}

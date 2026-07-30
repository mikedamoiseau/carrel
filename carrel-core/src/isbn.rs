//! Shared ISBN helpers. The regex and extractor live here so both the EPUB
//! parser (in core) and the enrichment module (currently in the desktop
//! crate, scheduled for core in #63 M4) can reference a single source of
//! truth without cross-crate dependencies on application layers.

use regex::Regex;
use std::sync::LazyLock;

/// Matches ISBN-10 (with optional trailing X check digit) and ISBN-13
/// (starting with 978 or 979) after whitespace/dash stripping.
pub static ISBN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(97[89]\d{10}|\d{9}[\dXx])$").unwrap());

/// Extract an ISBN from a `dc:identifier` string. Accepts `urn:isbn:`,
/// `isbn:`, and `ISBN:` prefixes. Returns `None` when the trimmed value is
/// not a valid ISBN-10 or ISBN-13.
pub fn extract_isbn(identifier: &str) -> Option<String> {
    let s = identifier
        .trim()
        .strip_prefix("urn:isbn:")
        .or_else(|| identifier.trim().strip_prefix("isbn:"))
        .or_else(|| identifier.trim().strip_prefix("ISBN:"))
        .unwrap_or(identifier.trim());
    let cleaned = s.replace(['-', ' '], "");
    if ISBN_RE.is_match(&cleaned) {
        Some(cleaned)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_isbn_with_urn_prefix() {
        assert_eq!(
            extract_isbn("urn:isbn:9780441013593"),
            Some("9780441013593".into())
        );
    }

    #[test]
    fn extract_isbn_with_isbn_prefix() {
        assert_eq!(
            extract_isbn("ISBN:9780441013593"),
            Some("9780441013593".into())
        );
    }

    #[test]
    fn extract_isbn_bare() {
        assert_eq!(extract_isbn("9780441013593"), Some("9780441013593".into()));
    }

    #[test]
    fn extract_isbn_with_dashes() {
        assert_eq!(
            extract_isbn("978-0-441-01359-3"),
            Some("9780441013593".into())
        );
    }

    #[test]
    fn extract_isbn_rejects_non_isbn() {
        assert_eq!(extract_isbn("not-an-isbn"), None);
        assert_eq!(extract_isbn(""), None);
    }
}

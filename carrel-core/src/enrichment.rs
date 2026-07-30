use crate::isbn::ISBN_RE;
use regex::Regex;
use std::sync::LazyLock;

/// Metadata extracted from a filename.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedFilename {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<u16>,
    pub isbn: Option<String>,
}

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d{4})\)|\[(\d{4})\]").unwrap());

static PAREN_AUTHOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^)]{3,50})\)\s*$").unwrap());

/// Parse a filename (without extension) into structured metadata.
pub fn parse_filename(stem: &str) -> ParsedFilename {
    let mut result = ParsedFilename::default();

    let cleaned = stem.replace('_', " ");
    let cleaned = cleaned.trim();

    // Check if entire stem is an ISBN
    let no_spaces = cleaned.replace([' ', '-'], "");
    if ISBN_RE.is_match(&no_spaces) {
        result.isbn = Some(no_spaces);
        return result;
    }

    // Extract year from (YYYY) or [YYYY]
    if let Some(caps) = YEAR_RE.captures(cleaned) {
        let year_str = caps.get(1).or(caps.get(2)).unwrap().as_str();
        if let Ok(y) = year_str.parse::<u16>() {
            if (1800..=2100).contains(&y) {
                result.year = Some(y);
            }
        }
    }
    let without_year = YEAR_RE.replace_all(cleaned, "").trim().to_string();

    // Check for trailing (Author Name) pattern — common for comics
    if let Some(caps) = PAREN_AUTHOR_RE.captures(&without_year) {
        let author = caps.get(1).unwrap().as_str().trim();
        let before = without_year[..caps.get(0).unwrap().start()].trim();
        if author.chars().any(|c| c.is_alphabetic()) && !author.chars().all(|c| c.is_ascii_digit())
        {
            result.author = Some(author.to_string());
            result.title = Some(before.to_string());
            return result;
        }
    }

    let work = without_year.trim();

    // Try splitting on " - " (most common separator)
    if let Some((left, right)) = work.split_once(" - ") {
        let left = left.trim();
        let right = right.trim();
        let left_words = left.split_whitespace().count();
        let right_words = right.split_whitespace().count();
        if left_words <= 3 {
            // Short left side is likely an author name: "Author - Title"
            result.author = Some(left.to_string());
            result.title = Some(right.to_string());
        } else if right_words <= 3 {
            // Long left, short right: "Title - Author"
            result.title = Some(left.to_string());
            result.author = Some(right.to_string());
        } else {
            // Both sides long — treat full work as title
            result.title = Some(work.to_string());
        }
        return result;
    }

    // Try splitting on " by " (case-insensitive)
    if let Some(idx) = work.to_lowercase().find(" by ") {
        let title = work[..idx].trim();
        let author = work[idx + 4..].trim();
        if !title.is_empty() && !author.is_empty() {
            result.title = Some(title.to_string());
            result.author = Some(author.to_string());
            return result;
        }
    }

    // Fallback: entire cleaned stem is the title
    if !work.is_empty() {
        result.title = Some(work.to_string());
    }

    result
}

/// Check if a string is a valid ISBN-10 or ISBN-13.
pub fn is_valid_isbn(s: &str) -> bool {
    let cleaned = s.replace(['-', ' '], "");
    ISBN_RE.is_match(&cleaned)
}

/// Extract ISBN from a dc:identifier string. Delegates to `carrel_core::isbn`
/// so the desktop enrichment module and the core EPUB parser share a single
/// definition.
pub use crate::isbn::extract_isbn;

use crate::providers::EnrichmentData;

#[derive(Debug, Clone)]
pub struct EnrichmentResult {
    pub data: EnrichmentData,
    pub confidence: f64,
    pub auto_apply: bool,
    /// Names of providers that were queried during this enrichment (e.g. ["Google Books", "OpenLibrary"]).
    pub providers_tried: Vec<String>,
}

pub fn title_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .filter(|w| !matches!(*w, "the" | "a" | "an" | "of" | "and"))
        .map(|s| s.to_string())
        .collect();
    let words_b: std::collections::HashSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .filter(|w| !matches!(*w, "the" | "a" | "an" | "of" | "and"))
        .map(|s| s.to_string())
        .collect();
    if words_a.is_empty() && words_b.is_empty() {
        return 0.0;
    }
    let intersection = words_a.intersection(&words_b).count() as f64;
    let union = words_a.union(&words_b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

#[tracing::instrument(
    skip(title, author, isbn, registry),
    fields(has_isbn = isbn.is_some())
)]
pub fn enrich_book(
    title: &str,
    author: &str,
    isbn: Option<&str>,
    registry: &crate::providers::ProviderRegistry,
) -> Option<EnrichmentResult> {
    tracing::info!("enriching book");
    let mut all_tried = Vec::new();

    // Tier 1: ISBN lookup
    if let Some(isbn) = isbn {
        let outcome = registry.search_by_isbn(isbn);
        all_tried.extend(outcome.providers_tried);
        if let Some(first) = outcome.results.into_iter().next() {
            if !first.title.is_empty() {
                return Some(EnrichmentResult {
                    data: first,
                    confidence: 0.95,
                    auto_apply: true,
                    providers_tried: all_tried,
                });
            }
        }
    }
    // Tier 2: Title + Author search
    let author_opt = if author.is_empty() || author == "Unknown Author" {
        None
    } else {
        Some(author)
    };
    let outcome = registry.search_by_title(title, author_opt);
    all_tried.extend(outcome.providers_tried);
    let first = outcome.results.into_iter().next()?;
    let sim = title_similarity(title, &first.title);
    if sim >= 0.85 {
        Some(EnrichmentResult {
            data: first,
            confidence: sim,
            auto_apply: true,
            providers_tried: all_tried,
        })
    } else if sim >= 0.5 {
        Some(EnrichmentResult {
            data: first,
            confidence: sim,
            auto_apply: false,
            providers_tried: all_tried,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_isbn_only_filename() {
        let r = parse_filename("9780441013593");
        assert_eq!(r.isbn.as_deref(), Some("9780441013593"));
        assert!(r.title.is_none());
    }

    #[test]
    fn parse_isbn_with_dashes() {
        let r = parse_filename("978-0-441-01359-3");
        assert_eq!(r.isbn.as_deref(), Some("9780441013593"));
    }

    #[test]
    fn parse_author_dash_title_with_year() {
        let r = parse_filename("Frank Herbert - Dune (1965)");
        assert_eq!(r.author.as_deref(), Some("Frank Herbert"));
        assert_eq!(r.title.as_deref(), Some("Dune"));
        assert_eq!(r.year, Some(1965));
    }

    #[test]
    fn parse_title_by_author() {
        let r = parse_filename("Dune by Frank Herbert");
        assert_eq!(r.title.as_deref(), Some("Dune"));
        assert_eq!(r.author.as_deref(), Some("Frank Herbert"));
    }

    #[test]
    fn parse_comic_with_paren_author() {
        let r = parse_filename("Aria - T01 - La fugue d'Aria (Michel Weyland)");
        assert_eq!(r.title.as_deref(), Some("Aria - T01 - La fugue d'Aria"));
        assert_eq!(r.author.as_deref(), Some("Michel Weyland"));
    }

    #[test]
    fn parse_comic_no_author() {
        let r = parse_filename("Aria T39 - Flammes salvatrices");
        assert!(r.title.is_some());
    }

    #[test]
    fn parse_underscores_replaced() {
        let r = parse_filename("Dune_by_Frank_Herbert");
        assert_eq!(r.title.as_deref(), Some("Dune"));
        assert_eq!(r.author.as_deref(), Some("Frank Herbert"));
    }

    #[test]
    fn parse_simple_title() {
        let r = parse_filename("Dune");
        assert_eq!(r.title.as_deref(), Some("Dune"));
        assert!(r.author.is_none());
    }

    #[test]
    fn parse_year_in_brackets() {
        let r = parse_filename("Foundation [1951]");
        assert_eq!(r.title.as_deref(), Some("Foundation"));
        assert_eq!(r.year, Some(1951));
    }

    #[test]
    fn parse_empty_filename() {
        let r = parse_filename("");
        assert!(r.title.is_none());
        assert!(r.author.is_none());
    }

    #[test]
    fn extract_isbn_from_urn() {
        assert_eq!(
            extract_isbn("urn:isbn:9780441013593"),
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
    fn extract_isbn_invalid() {
        assert_eq!(extract_isbn("not-an-isbn"), None);
        assert_eq!(extract_isbn("12345"), None);
    }

    #[test]
    fn title_similarity_exact_match() {
        assert!((title_similarity("Dune", "Dune") - 1.0).abs() < 0.01);
    }

    #[test]
    fn title_similarity_with_articles() {
        assert!((title_similarity("The Lord of the Rings", "Lord of Rings") - 1.0).abs() < 0.01);
    }

    #[test]
    fn title_similarity_different() {
        assert!(title_similarity("Dune", "Foundation") < 0.1);
    }

    #[test]
    fn title_similarity_partial() {
        let sim = title_similarity("Dune Messiah", "Dune");
        assert!(sim > 0.3 && sim < 0.8);
    }
}

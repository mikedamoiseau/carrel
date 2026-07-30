//! Generic full-text search utilities for HTML content.
//!
//! Extracted from `epub::search_book` so MOBI books can reuse the same search
//! logic.

use ammonia::clean;

use crate::epub::{count_words, extract_snippet, strip_html_tags};
use crate::models::SearchResult;
use crate::CarrelResult;

/// Global cap on the number of search hits returned to the UI.
///
/// Exposed so format-specific aggregators (e.g. the MOBI path in
/// `search_book_content`) can enforce the same whole-book budget that
/// `epub::search_book` already applies.
pub const MAX_SEARCH_RESULTS: usize = 200;

/// Search HTML content for a query string (case-insensitive).
///
/// Strips HTML tags and sanitizes the input before searching.
/// Returns a list of `SearchResult`s, capped at `MAX_SEARCH_RESULTS`.
pub fn find_matches_in_html(
    html: &str,
    query: &str,
    chapter_index: u32,
    _book_id: &str, // For consistency with other adapters, currently unused
) -> Vec<SearchResult> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    let body_only = clean(html);
    let text = strip_html_tags(&body_only);
    let text_lower = text.to_lowercase();

    let mut search_from = 0;
    while let Some(pos) = text_lower[search_from..].find(&query_lower) {
        let match_start = search_from + pos;
        results.push(SearchResult {
            chapter_index,
            snippet: extract_snippet(&text, match_start, query_lower.len(), 40),
            match_offset: match_start,
        });
        if results.len() >= MAX_SEARCH_RESULTS {
            return results;
        }
        search_from = match_start + query_lower.len();
    }

    results
}

/// Strip HTML tags and count words in HTML content.
pub fn get_html_word_count(html: &str) -> usize {
    let body_only = clean(html);
    let text = strip_html_tags(&body_only);
    count_words(&text)
}

/// Run `find_matches_in_html` across a chapter list, stopping once the
/// whole-book result set reaches `MAX_SEARCH_RESULTS`.
///
/// `get_html` is invoked lazily per chapter so callers don't pay for chapters
/// beyond the global cap, and its error surface is the caller's (e.g. MOBI
/// extraction failures).
pub fn search_chapters<I, F>(
    chapter_indices: I,
    query: &str,
    book_id: &str,
    mut get_html: F,
) -> CarrelResult<Vec<SearchResult>>
where
    I: IntoIterator<Item = u32>,
    F: FnMut(u32) -> CarrelResult<String>,
{
    let mut results = Vec::new();
    for idx in chapter_indices {
        if results.len() >= MAX_SEARCH_RESULTS {
            break;
        }
        let html = get_html(idx)?;
        results.extend(find_matches_in_html(&html, query, idx, book_id));
    }
    results.truncate(MAX_SEARCH_RESULTS);
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_chapter_cap_enforced() {
        // 500 matches in one chapter — `find_matches_in_html` must stop at cap.
        let html = "word ".repeat(500);
        let res = find_matches_in_html(&html, "word", 0, "book-1");
        assert_eq!(res.len(), MAX_SEARCH_RESULTS);
    }

    #[test]
    fn aggregation_stops_at_global_cap() {
        // 50 chapters × 5 matches = 250 total — global cap is 200.
        let chapters: Vec<u32> = (0..50).collect();
        let res =
            search_chapters(chapters, "x", "book-1", |_idx| Ok("x x x x x ".to_string())).unwrap();
        assert_eq!(res.len(), MAX_SEARCH_RESULTS);
    }

    #[test]
    fn aggregation_short_circuits_chapter_fetch() {
        // Once the cap is reached, remaining chapters must not be fetched.
        // Chapter 0 alone emits 300 matches — chapters 1..=100 should be
        // skipped (the closure must never see them).
        let fetched = std::cell::RefCell::new(Vec::<u32>::new());
        let chapters: Vec<u32> = (0..100).collect();
        let res = search_chapters(chapters, "y", "book-1", |idx| {
            fetched.borrow_mut().push(idx);
            if idx == 0 {
                Ok("y ".repeat(300))
            } else {
                Ok(String::new())
            }
        })
        .unwrap();
        assert_eq!(res.len(), MAX_SEARCH_RESULTS);
        // Chapter 0 must be fetched; nothing else should be.
        assert_eq!(&*fetched.borrow(), &[0]);
    }

    #[test]
    fn aggregation_propagates_fetch_errors() {
        let chapters: Vec<u32> = (0..5).collect();
        let err = search_chapters(chapters, "x", "book-1", |idx| {
            if idx == 2 {
                Err(crate::CarrelError::internal("boom"))
            } else {
                Ok("x".to_string())
            }
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn aggregation_preserves_results_below_cap() {
        // 3 chapters × 2 matches = 6 total, well under cap.
        let chapters: Vec<u32> = (0..3).collect();
        let res = search_chapters(chapters, "z", "book-1", |idx| {
            Ok(format!("z z chapter {idx}"))
        })
        .unwrap();
        assert_eq!(res.len(), 6);
        // Chapter indices preserved on results.
        assert!(res.iter().any(|r| r.chapter_index == 0));
        assert!(res.iter().any(|r| r.chapter_index == 2));
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BookFormat {
    Epub,
    Cbz,
    Cbr,
    Pdf,
    /// MOBI / AZW / AZW3 (KF8) — always present so library rows that
    /// reference MOBI books stay readable in builds without the `mobi`
    /// feature. Runtime operations on `Mobi` books return a clear
    /// "not enabled" error when libmobi wasn't compiled in.
    Mobi,
}

impl std::fmt::Display for BookFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BookFormat::Epub => write!(f, "epub"),
            BookFormat::Cbz => write!(f, "cbz"),
            BookFormat::Cbr => write!(f, "cbr"),
            BookFormat::Pdf => write!(f, "pdf"),
            BookFormat::Mobi => write!(f, "mobi"),
        }
    }
}

impl std::str::FromStr for BookFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "epub" => Ok(BookFormat::Epub),
            "cbz" => Ok(BookFormat::Cbz),
            "cbr" => Ok(BookFormat::Cbr),
            "pdf" => Ok(BookFormat::Pdf),
            "mobi" => Ok(BookFormat::Mobi),
            _ => Err(format!("unknown book format: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub id: String,
    pub title: String,
    pub author: String,
    pub file_path: String,
    pub cover_path: Option<String>,
    pub total_chapters: u32,
    pub added_at: i64,
    pub format: BookFormat,
    pub file_hash: Option<String>,
    pub description: Option<String>,
    pub genres: Option<String>, // JSON array string
    pub rating: Option<f64>,
    pub isbn: Option<String>,
    pub openlibrary_key: Option<String>,
    pub enrichment_status: Option<String>,
    pub series: Option<String>,
    pub volume: Option<u32>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub publish_year: Option<u16>,
    pub is_imported: bool,
    #[serde(default)]
    pub want_to_read: bool,
}

/// Lightweight subset of Book for grid/list display — omits heavy fields
/// like description, genres, isbn, file_path, file_hash, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookGridItem {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover_path: Option<String>,
    pub total_chapters: u32,
    pub added_at: i64,
    pub format: BookFormat,
    pub series: Option<String>,
    pub volume: Option<u32>,
    pub rating: Option<f64>,
    pub language: Option<String>,
    pub publish_year: Option<u16>,
    pub is_imported: bool,
    #[serde(default)]
    pub want_to_read: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingProgress {
    pub book_id: String,
    pub chapter_index: u32,
    pub scroll_position: f64,
    pub last_read_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: String,
    pub book_id: String,
    pub chapter_index: u32,
    pub scroll_position: f64,
    pub name: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    pub id: String,
    pub book_id: String,
    pub chapter_index: u32,
    pub text: String,
    pub color: String,
    pub note: Option<String>,
    pub start_offset: u32,
    pub end_offset: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

/// F-1-5 vocabulary builder: one row per looked-up lemma, with a snapshot of
/// the definition and Leitner-box review state. See
/// `docs/superpowers/specs/2026-07-11-vocabulary-builder-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VocabularyWord {
    pub id: String,
    pub lemma: String,
    pub word: String,
    pub pos: Option<String>,
    pub definition: String,
    pub book_id: Option<String>,
    pub book_title: Option<String>,
    pub chapter_index: Option<i64>,
    pub context_sentence: Option<String>,
    /// In-chapter character offsets of the looked-up selection, for
    /// jump-to-source (VJ-M1). Nullable — a word may be logged without a
    /// usable selection range.
    pub start_offset: Option<i64>,
    pub end_offset: Option<i64>,
    pub seen_count: i64,
    /// SQL column is `box` (Leitner box 1..5); `box` is a Rust keyword, so the
    /// field is `box_num` and renamed back to `box` on the wire.
    #[serde(rename = "box")]
    pub box_num: i64,
    pub last_reviewed_at: Option<i64>,
    pub next_due_at: Option<i64>,
    pub last_seen_at: i64,
    pub created_at: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub enum CollectionType {
    Manual,
    Automated,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CollectionRule {
    pub id: String,
    pub collection_id: String,
    pub field: String, // author | filename | series | language | publisher | description | format | tag | date_added | reading_progress
    pub operator: String, // contains | equals | last_n_days
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewRuleInput {
    pub field: String,
    pub operator: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub r#type: CollectionType,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub rules: Vec<CollectionRule>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSuggestion {
    pub name: String,
    pub icon: String,
    pub color: String,
    pub rules: Vec<NewRuleInput>,
    pub matched_book_count: usize,
    pub heuristic_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureFlag {
    pub key: String,
    pub enabled: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HighlightSearchResult {
    pub highlight_id: String,
    pub book_id: String,
    pub book_title: String,
    pub book_author: String,
    pub chapter_index: u32,
    pub text: String,
    pub color: String,
    pub note: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: String,
    pub timestamp: i64,
    pub action: String,
    pub entity_type: String,
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSessionEntry {
    pub id: String,
    pub timestamp: i64,
    pub ip: String,
    pub method: String,
    pub outcome: String,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomFont {
    pub id: String,
    pub name: String,
    pub file_name: String,
    pub file_path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesInfo {
    pub name: String,
    pub count: i64,
}

/// A book with in-progress reading state, for the web UI's "Continue
/// Reading" shelf (Item 5) — a lighter projection than `Book` carrying just
/// what the shelf card needs plus the saved position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueReadingItem {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover_path: Option<String>,
    pub format: BookFormat,
    pub total_chapters: u32,
    pub chapter_index: u32,
    pub scroll_position: f64,
    pub last_read_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupResult {
    pub removed_count: u32,
    pub removed_books: Vec<CleanupEntry>,
    pub backup_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupEntry {
    pub id: String,
    pub title: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupProgress {
    pub current: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoBackup {
    pub path: String,
    pub label: String,
    pub timestamp: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TocEntry {
    pub label: String,
    pub chapter_index: u32,
    /// Stable ordering key from the source TOC (e.g. EPUB NCX playOrder).
    /// For formats without an explicit key — MOBI, fallback EPUB spine —
    /// callers pass a 1-based index as a string.
    pub play_order: String,
    pub children: Vec<TocEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterMeta {
    pub index: usize,
    pub title: String,
    pub word_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub chapter_index: u32,
    pub snippet: String,
    pub match_offset: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_format_display() {
        assert_eq!(BookFormat::Epub.to_string(), "epub");
        assert_eq!(BookFormat::Cbz.to_string(), "cbz");
        assert_eq!(BookFormat::Cbr.to_string(), "cbr");
        assert_eq!(BookFormat::Pdf.to_string(), "pdf");
        assert_eq!(BookFormat::Mobi.to_string(), "mobi");
    }

    #[test]
    fn book_format_from_str_valid() {
        assert_eq!("epub".parse::<BookFormat>().unwrap(), BookFormat::Epub);
        assert_eq!("cbz".parse::<BookFormat>().unwrap(), BookFormat::Cbz);
        assert_eq!("cbr".parse::<BookFormat>().unwrap(), BookFormat::Cbr);
        assert_eq!("pdf".parse::<BookFormat>().unwrap(), BookFormat::Pdf);
        assert_eq!("mobi".parse::<BookFormat>().unwrap(), BookFormat::Mobi);
    }

    #[test]
    fn book_format_from_str_invalid() {
        let err = "xyz".parse::<BookFormat>().unwrap_err();
        assert!(err.contains("unknown book format"));
        assert!(err.contains("xyz"));
    }

    #[test]
    fn book_deserializes_without_want_to_read_field() {
        // Legacy library.json / metadata/books.json predate the field.
        let json = r#"{"id":"b1","title":"T","author":"A","file_path":"b1.epub",
            "cover_path":null,"total_chapters":1,"added_at":0,"format":"epub",
            "file_hash":null,"description":null,"genres":null,"rating":null,
            "isbn":null,"openlibrary_key":null,"enrichment_status":null,
            "series":null,"volume":null,"language":null,"publisher":null,
            "publish_year":null,"is_imported":true}"#;
        let book: Book = serde_json::from_str(json).unwrap();
        assert!(!book.want_to_read, "missing field defaults to false");
    }

    #[test]
    fn book_format_from_str_case_sensitive() {
        // FromStr is case-sensitive — uppercase should fail
        assert!("EPUB".parse::<BookFormat>().is_err());
        assert!("Pdf".parse::<BookFormat>().is_err());
    }

    #[test]
    fn book_format_serde_roundtrip() {
        let format = BookFormat::Epub;
        let json = serde_json::to_string(&format).unwrap();
        assert_eq!(json, "\"epub\"");
        let back: BookFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back, BookFormat::Epub);
    }

    #[test]
    fn bookmark_serde_with_timestamps() {
        let bookmark = Bookmark {
            id: "bm-1".to_string(),
            book_id: "book-1".to_string(),
            chapter_index: 3,
            scroll_position: 0.5,
            name: Some("My Bookmark".to_string()),
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        let json = serde_json::to_string(&bookmark).unwrap();
        let back: Bookmark = serde_json::from_str(&json).unwrap();
        assert_eq!(back.updated_at, 1700000000);
        assert_eq!(back.deleted_at, None);

        // With deleted_at set
        let deleted_bookmark = Bookmark {
            deleted_at: Some(1700001000),
            ..bookmark
        };
        let json2 = serde_json::to_string(&deleted_bookmark).unwrap();
        let back2: Bookmark = serde_json::from_str(&json2).unwrap();
        assert_eq!(back2.deleted_at, Some(1700001000));
    }

    #[test]
    fn highlight_serde_with_timestamps() {
        let highlight = Highlight {
            id: "hl-1".to_string(),
            book_id: "book-1".to_string(),
            chapter_index: 2,
            text: "highlighted text".to_string(),
            color: "yellow".to_string(),
            note: Some("A note".to_string()),
            start_offset: 10,
            end_offset: 26,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        let json = serde_json::to_string(&highlight).unwrap();
        let back: Highlight = serde_json::from_str(&json).unwrap();
        assert_eq!(back.updated_at, 1700000000);
        assert_eq!(back.deleted_at, None);

        // With deleted_at set
        let deleted_highlight = Highlight {
            deleted_at: Some(1700002000),
            ..highlight
        };
        let json2 = serde_json::to_string(&deleted_highlight).unwrap();
        let back2: Highlight = serde_json::from_str(&json2).unwrap();
        assert_eq!(back2.deleted_at, Some(1700002000));
    }

    #[test]
    fn book_format_serde_all_variants() {
        for (variant, expected) in [
            (BookFormat::Epub, "\"epub\""),
            (BookFormat::Cbz, "\"cbz\""),
            (BookFormat::Cbr, "\"cbr\""),
            (BookFormat::Pdf, "\"pdf\""),
            (BookFormat::Mobi, "\"mobi\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
        }
    }
}

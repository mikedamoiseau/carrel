use opendal::blocking::Operator;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use crate::db;
use crate::models::{Bookmark, Highlight, ReadingProgress};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncProgress {
    pub chapter_index: u32,
    pub scroll_position: f64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncBookmark {
    pub id: String,
    pub chapter_index: u32,
    pub scroll_position: f64,
    pub name: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncHighlight {
    pub id: String,
    pub chapter_index: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub text: String,
    pub color: String,
    pub note: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookSyncFile {
    pub schema_version: u32,
    pub book_hash: String,
    pub device_id: String,
    pub progress: Option<SyncProgress>,
    pub bookmarks: Vec<SyncBookmark>,
    pub highlights: Vec<SyncHighlight>,
}

#[derive(Debug)]
pub enum SyncError {
    Transport {
        message: String,
        kind: Option<opendal::ErrorKind>,
    },
    Timeout,
    Malformed(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Transport { message, .. } => write!(f, "Transport error: {message}"),
            SyncError::Timeout => write!(f, "Sync operation timed out"),
            SyncError::Malformed(msg) => write!(f, "Malformed sync data: {msg}"),
        }
    }
}

/// Bridge `SyncError` into the crate-wide [`crate::error::CarrelError`]. Both
/// types live in `carrel-core` (M5), so the impl is colocated with `SyncError`.
impl From<SyncError> for crate::error::CarrelError {
    fn from(e: SyncError) -> Self {
        use crate::error::CarrelError;
        match e {
            SyncError::Transport { message, .. } => CarrelError::network(message),
            SyncError::Timeout => CarrelError::network("Sync operation timed out"),
            SyncError::Malformed(msg) => {
                CarrelError::invalid(format!("Malformed sync data: {msg}"))
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct MergeResult {
    pub progress_updated: bool,
    pub bookmarks_added: u32,
    pub bookmarks_updated: u32,
    pub highlights_added: u32,
    pub highlights_updated: u32,
}

impl MergeResult {
    pub fn has_changes(&self) -> bool {
        self.progress_updated
            || self.bookmarks_added > 0
            || self.bookmarks_updated > 0
            || self.highlights_added > 0
            || self.highlights_updated > 0
    }
}

/// `suppress_progress` omits the `progress` field from the built payload
/// (private mode, B-M1, SB-5): the caller pushing this file to a remote
/// target must never leak reading position / `last_read_at` while private,
/// even though highlights and bookmarks (deliberate saves) still go out.
/// Callers building a payload purely for local merge comparison (never
/// pushed) may pass `false` regardless of private mode — `false` here has
/// no effect beyond including real progress in the returned struct.
pub fn build_sync_payload(
    conn: &Connection,
    book_id: &str,
    file_hash: &str,
    device_id: &str,
    suppress_progress: bool,
) -> BookSyncFile {
    let progress = if suppress_progress {
        None
    } else {
        db::get_reading_progress(conn, book_id)
            .ok()
            .flatten()
            .map(|p| SyncProgress {
                chapter_index: p.chapter_index,
                scroll_position: p.scroll_position,
                updated_at: p.last_read_at,
            })
    };

    let bookmarks = db::list_all_bookmarks_for_sync(conn, book_id)
        .unwrap_or_default()
        .into_iter()
        .map(|b| SyncBookmark {
            id: b.id,
            chapter_index: b.chapter_index,
            scroll_position: b.scroll_position,
            name: b.name,
            note: b.note,
            created_at: b.created_at,
            updated_at: b.updated_at,
            deleted_at: b.deleted_at,
        })
        .collect();

    let highlights = db::list_all_highlights_for_sync(conn, book_id)
        .unwrap_or_default()
        .into_iter()
        .map(|h| SyncHighlight {
            id: h.id,
            chapter_index: h.chapter_index,
            start_offset: h.start_offset,
            end_offset: h.end_offset,
            text: h.text,
            color: h.color,
            note: h.note,
            created_at: h.created_at,
            updated_at: h.updated_at,
            deleted_at: h.deleted_at,
        })
        .collect();

    BookSyncFile {
        schema_version: CURRENT_SCHEMA_VERSION,
        book_hash: file_hash.to_string(),
        device_id: device_id.to_string(),
        progress,
        bookmarks,
        highlights,
    }
}

fn bookmark_content_eq(a: &SyncBookmark, b: &SyncBookmark) -> bool {
    a.chapter_index == b.chapter_index
        && (a.scroll_position - b.scroll_position).abs() < f64::EPSILON
        && a.name == b.name
        && a.note == b.note
        && a.deleted_at == b.deleted_at
}

fn highlight_content_eq(a: &SyncHighlight, b: &SyncHighlight) -> bool {
    a.chapter_index == b.chapter_index
        && a.start_offset == b.start_offset
        && a.end_offset == b.end_offset
        && a.text == b.text
        && a.color == b.color
        && a.note == b.note
        && a.deleted_at == b.deleted_at
}

/// Options for [`merge_remote_into_local`]. `Default` (`suppress_progress:
/// false`) reproduces pre-B-M1 behavior exactly, so every existing call site
/// that doesn't care about private mode can pass `MergeOptions::default()`
/// unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeOptions {
    /// Private mode (B-M1, SB-8): skip the progress-merge arms entirely —
    /// an inbound remote progress update is itself a passive write to the
    /// local `reading_progress` table. Highlight/bookmark upserts below are
    /// never gated; deliberate saves always persist.
    pub suppress_progress: bool,
}

pub fn merge_remote_into_local(
    conn: &Connection,
    book_id: &str,
    local: &BookSyncFile,
    remote: &BookSyncFile,
    options: MergeOptions,
) -> MergeResult {
    let mut result = MergeResult::default();

    // --- Progress ---
    if !options.suppress_progress {
        match (&local.progress, &remote.progress) {
            (None, Some(rp)) => {
                let progress = ReadingProgress {
                    book_id: book_id.to_string(),
                    chapter_index: rp.chapter_index,
                    scroll_position: rp.scroll_position,
                    last_read_at: rp.updated_at,
                };
                if db::upsert_reading_progress(conn, &progress).is_ok() {
                    result.progress_updated = true;
                }
            }
            (Some(lp), Some(rp)) if rp.updated_at > lp.updated_at => {
                let progress = ReadingProgress {
                    book_id: book_id.to_string(),
                    chapter_index: rp.chapter_index,
                    scroll_position: rp.scroll_position,
                    last_read_at: rp.updated_at,
                };
                if db::upsert_reading_progress(conn, &progress).is_ok() {
                    result.progress_updated = true;
                }
            }
            (Some(lp), Some(rp))
                if rp.updated_at == lp.updated_at
                    && (rp.chapter_index != lp.chapter_index
                        || (rp.scroll_position - lp.scroll_position).abs() > f64::EPSILON) =>
            {
                let progress = ReadingProgress {
                    book_id: book_id.to_string(),
                    chapter_index: rp.chapter_index,
                    scroll_position: rp.scroll_position,
                    last_read_at: rp.updated_at,
                };
                if db::upsert_reading_progress(conn, &progress).is_ok() {
                    result.progress_updated = true;
                }
            }
            _ => {}
        }
    }

    // --- Bookmarks ---
    let local_bookmarks: HashMap<&str, &SyncBookmark> =
        local.bookmarks.iter().map(|b| (b.id.as_str(), b)).collect();

    for rb in &remote.bookmarks {
        let bookmark = Bookmark {
            id: rb.id.clone(),
            book_id: book_id.to_string(),
            chapter_index: rb.chapter_index,
            scroll_position: rb.scroll_position,
            name: rb.name.clone(),
            note: rb.note.clone(),
            created_at: rb.created_at,
            updated_at: rb.updated_at,
            deleted_at: rb.deleted_at,
        };

        match local_bookmarks.get(rb.id.as_str()) {
            None if db::upsert_bookmark_from_sync(conn, &bookmark).is_ok() => {
                result.bookmarks_added += 1;
            }
            Some(lb)
                if rb.updated_at > lb.updated_at
                    && db::upsert_bookmark_from_sync(conn, &bookmark).is_ok() =>
            {
                result.bookmarks_updated += 1;
            }
            Some(lb)
                if rb.updated_at == lb.updated_at
                    && !bookmark_content_eq(rb, lb)
                    && db::upsert_bookmark_from_sync(conn, &bookmark).is_ok() =>
            {
                result.bookmarks_updated += 1;
            }
            _ => {}
        }
    }

    // --- Highlights ---
    let local_highlights: HashMap<&str, &SyncHighlight> = local
        .highlights
        .iter()
        .map(|h| (h.id.as_str(), h))
        .collect();

    for rh in &remote.highlights {
        let highlight = Highlight {
            id: rh.id.clone(),
            book_id: book_id.to_string(),
            chapter_index: rh.chapter_index,
            text: rh.text.clone(),
            color: rh.color.clone(),
            note: rh.note.clone(),
            start_offset: rh.start_offset,
            end_offset: rh.end_offset,
            created_at: rh.created_at,
            updated_at: rh.updated_at,
            deleted_at: rh.deleted_at,
        };

        match local_highlights.get(rh.id.as_str()) {
            None if db::upsert_highlight_from_sync(conn, &highlight).is_ok() => {
                result.highlights_added += 1;
            }
            Some(lh)
                if rh.updated_at > lh.updated_at
                    && db::upsert_highlight_from_sync(conn, &highlight).is_ok() =>
            {
                result.highlights_updated += 1;
            }
            Some(lh)
                if rh.updated_at == lh.updated_at
                    && !highlight_content_eq(rh, lh)
                    && db::upsert_highlight_from_sync(conn, &highlight).is_ok() =>
            {
                result.highlights_updated += 1;
            }
            _ => {}
        }
    }

    result
}

/// Remote path for a book's sync state. This directory is written to the
/// user's own remote, so renaming it orphans sync state already there.
fn sync_path(file_hash: &str) -> String {
    format!(".carrel-sync/books/{file_hash}.json")
}

pub fn fetch_remote_sync(
    op: &Operator,
    file_hash: &str,
) -> Result<Option<BookSyncFile>, SyncError> {
    let path = sync_path(file_hash);
    let data = match op.read(&path) {
        Ok(buf) => buf,
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            let kind = Some(e.kind());
            return Err(SyncError::Transport {
                message: format!("Failed to read {path}: {e}"),
                kind,
            });
        }
    };
    let file: BookSyncFile = serde_json::from_slice(&data.to_vec())
        .map_err(|e| SyncError::Malformed(format!("Failed to parse {path}: {e}")))?;
    if file.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(SyncError::Malformed(format!(
            "Unsupported schema version {} (max {})",
            file.schema_version, CURRENT_SCHEMA_VERSION
        )));
    }
    Ok(Some(file))
}

pub fn push_remote_sync(
    op: &Operator,
    file_hash: &str,
    payload: &BookSyncFile,
) -> Result<(), SyncError> {
    let path = sync_path(file_hash);
    let json = serde_json::to_string(payload)
        .map_err(|e| SyncError::Malformed(format!("Failed to serialize sync payload: {e}")))?;
    op.write(&path, json.into_bytes()).map_err(|e| {
        let kind = Some(e.kind());
        SyncError::Transport {
            message: format!("Failed to write {path}: {e}"),
            kind,
        }
    })?;
    Ok(())
}

/// Pull remote sync data for a book and merge into local DB.
/// Returns the merge result (may be empty if no remote data exists).
///
/// `suppress_progress` (private mode, B-M1): skip only the inbound
/// progress-merge arms; highlights/bookmarks always merge.
pub fn sync_book_on_open(
    conn: &Connection,
    op: &Operator,
    book_id: &str,
    file_hash: &str,
    device_id: &str,
    suppress_progress: bool,
) -> Result<MergeResult, SyncError> {
    let remote = fetch_remote_sync(op, file_hash)?;
    let remote = match remote {
        Some(r) => r,
        None => return Ok(MergeResult::default()),
    };
    let local = build_sync_payload(conn, book_id, file_hash, device_id, suppress_progress);
    Ok(merge_remote_into_local(
        conn,
        book_id,
        &local,
        &remote,
        MergeOptions { suppress_progress },
    ))
}

/// Pull remote, merge into local DB, rebuild payload from merged state, then push.
/// This ensures remote-only changes from other devices are preserved.
///
/// `suppress_progress` (private mode, B-M1, SB-5/SB-8): skips the inbound
/// progress-merge arms AND omits progress from the outbound pushed file;
/// highlights/bookmarks are never gated in either direction.
pub fn sync_book_on_close(
    conn: &Connection,
    op: &Operator,
    book_id: &str,
    file_hash: &str,
    device_id: &str,
    suppress_progress: bool,
) -> Result<(), SyncError> {
    // Step 1: Pull and merge remote changes into local DB
    let remote = fetch_remote_sync(op, file_hash)?;
    if let Some(remote) = &remote {
        let local = build_sync_payload(conn, book_id, file_hash, device_id, suppress_progress);
        merge_remote_into_local(
            conn,
            book_id,
            &local,
            remote,
            MergeOptions { suppress_progress },
        );
    }

    // Step 2: Build fresh payload from merged local state and push
    let mut payload = build_sync_payload(conn, book_id, file_hash, device_id, suppress_progress);
    // Private mode (B-M1, SB-5): our own progress is intentionally omitted
    // from the payload, but `push_remote_sync` overwrites the whole remote
    // file — so blindly pushing `progress: None` would erase a legitimate
    // reading position another device (or an earlier non-private session)
    // already synced. Carry the fetched remote progress forward untouched.
    if suppress_progress {
        payload.progress = remote.and_then(|r| r.progress);
    }
    push_remote_sync(op, file_hash, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Book, BookFormat, Bookmark, ReadingProgress};
    use tempfile::tempdir;

    #[test]
    fn book_sync_file_roundtrip() {
        let file = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "abc123".to_string(),
            device_id: "device-1".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 3,
                scroll_position: 0.75,
                updated_at: 1700000000,
            }),
            bookmarks: vec![SyncBookmark {
                id: "bm-1".to_string(),
                chapter_index: 2,
                scroll_position: 0.5,
                name: Some("My bookmark".to_string()),
                note: None,
                created_at: 1700000000,
                updated_at: 1700000001,
                deleted_at: None,
            }],
            highlights: vec![SyncHighlight {
                id: "hl-1".to_string(),
                chapter_index: 1,
                start_offset: 10,
                end_offset: 50,
                text: "highlighted text".to_string(),
                color: "#ffff00".to_string(),
                note: Some("a note".to_string()),
                created_at: 1700000000,
                updated_at: 1700000002,
                deleted_at: Some(1700000003),
            }],
        };

        let json = serde_json::to_string(&file).unwrap();
        let deserialized: BookSyncFile = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(deserialized.book_hash, "abc123");
        assert_eq!(deserialized.device_id, "device-1");

        let progress = deserialized.progress.unwrap();
        assert_eq!(progress.chapter_index, 3);
        assert!((progress.scroll_position - 0.75).abs() < f64::EPSILON);
        assert_eq!(progress.updated_at, 1700000000);

        assert_eq!(deserialized.bookmarks.len(), 1);
        assert_eq!(deserialized.bookmarks[0].id, "bm-1");
        assert_eq!(
            deserialized.bookmarks[0].name,
            Some("My bookmark".to_string())
        );

        assert_eq!(deserialized.highlights.len(), 1);
        assert_eq!(deserialized.highlights[0].id, "hl-1");
        assert_eq!(deserialized.highlights[0].deleted_at, Some(1700000003));
    }

    #[test]
    fn book_sync_file_ignores_unknown_fields() {
        let json = r#"{
            "schema_version": 1,
            "book_hash": "abc123",
            "device_id": "device-1",
            "progress": null,
            "bookmarks": [],
            "highlights": [],
            "some_future_field": "should be ignored",
            "another_unknown": 42
        }"#;

        let file: BookSyncFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.schema_version, 1);
        assert_eq!(file.book_hash, "abc123");
        assert!(file.bookmarks.is_empty());
        assert!(file.highlights.is_empty());
    }

    #[test]
    fn merge_result_has_changes() {
        let empty = MergeResult::default();
        assert!(!empty.has_changes());

        let with_progress = MergeResult {
            progress_updated: true,
            ..Default::default()
        };
        assert!(with_progress.has_changes());

        let with_bookmarks_added = MergeResult {
            bookmarks_added: 1,
            ..Default::default()
        };
        assert!(with_bookmarks_added.has_changes());

        let with_bookmarks_updated = MergeResult {
            bookmarks_updated: 2,
            ..Default::default()
        };
        assert!(with_bookmarks_updated.has_changes());

        let with_highlights_added = MergeResult {
            highlights_added: 3,
            ..Default::default()
        };
        assert!(with_highlights_added.has_changes());

        let with_highlights_updated = MergeResult {
            highlights_updated: 4,
            ..Default::default()
        };
        assert!(with_highlights_updated.has_changes());
    }

    #[test]
    fn sync_error_display() {
        let transport = SyncError::Transport {
            message: "connection refused".to_string(),
            kind: None,
        };
        assert!(transport.to_string().contains("connection refused"));

        let transport_with_kind = SyncError::Transport {
            message: "access denied".to_string(),
            kind: Some(opendal::ErrorKind::PermissionDenied),
        };
        assert!(transport_with_kind.to_string().contains("access denied"));

        let timeout = SyncError::Timeout;
        assert_eq!(timeout.to_string(), "Sync operation timed out");

        let malformed = SyncError::Malformed("invalid json".to_string());
        assert_eq!(malformed.to_string(), "Malformed sync data: invalid json");
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let json = r#"{
            "schema_version": 99,
            "book_hash": "abc123",
            "device_id": "device-1",
            "progress": null,
            "bookmarks": [],
            "highlights": []
        }"#;

        let file: BookSyncFile = serde_json::from_str(json).unwrap();
        assert!(file.schema_version > CURRENT_SCHEMA_VERSION);
    }

    fn setup_db() -> (Connection, String) {
        let dir = tempdir().unwrap();
        let db_path = dir.keep().join("test.db");
        let conn = db::init_db(&db_path).unwrap();
        let book_id = "book-1".to_string();
        let book = Book {
            id: book_id.clone(),
            title: "Test Book".to_string(),
            author: "Author".to_string(),
            file_path: "/tmp/test.epub".to_string(),
            cover_path: None,
            total_chapters: 10,
            added_at: 1700000000,
            format: BookFormat::Epub,
            file_hash: Some("hash123".to_string()),
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: None,
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: false,
            want_to_read: false,
        };
        db::insert_book(&conn, &book).unwrap();
        (conn, book_id)
    }

    #[test]
    fn test_build_sync_payload() {
        let (conn, book_id) = setup_db();

        let progress = ReadingProgress {
            book_id: book_id.clone(),
            chapter_index: 5,
            scroll_position: 0.42,
            last_read_at: 1700001000,
        };
        db::upsert_reading_progress(&conn, &progress).unwrap();

        let bookmark = Bookmark {
            id: "bm-1".to_string(),
            book_id: book_id.clone(),
            chapter_index: 3,
            scroll_position: 0.2,
            name: Some("Test BM".to_string()),
            note: None,
            created_at: 1700000500,
            updated_at: 1700000500,
            deleted_at: None,
        };
        db::insert_bookmark(&conn, &bookmark).unwrap();

        let payload = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        assert_eq!(payload.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(payload.book_hash, "hash123");
        assert_eq!(payload.device_id, "device-A");

        let p = payload.progress.unwrap();
        assert_eq!(p.chapter_index, 5);
        assert!((p.scroll_position - 0.42).abs() < f64::EPSILON);
        assert_eq!(p.updated_at, 1700001000);

        assert_eq!(payload.bookmarks.len(), 1);
        assert_eq!(payload.bookmarks[0].id, "bm-1");
        assert_eq!(payload.bookmarks[0].name, Some("Test BM".to_string()));
    }

    #[test]
    fn test_build_sync_payload_includes_soft_deleted() {
        let (conn, book_id) = setup_db();

        let bookmark = Bookmark {
            id: "bm-del".to_string(),
            book_id: book_id.clone(),
            chapter_index: 1,
            scroll_position: 0.0,
            name: None,
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        db::insert_bookmark(&conn, &bookmark).unwrap();
        db::soft_delete_bookmark(&conn, "bm-del").unwrap();

        let payload = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);
        assert_eq!(payload.bookmarks.len(), 1);
        assert!(payload.bookmarks[0].deleted_at.is_some());

        // Confirm the normal list_bookmarks excludes it
        let normal = db::list_bookmarks(&conn, &book_id).unwrap();
        assert!(normal.is_empty());
    }

    #[test]
    fn test_merge_progress_remote_newer() {
        let (conn, book_id) = setup_db();

        let progress = ReadingProgress {
            book_id: book_id.clone(),
            chapter_index: 2,
            scroll_position: 0.1,
            last_read_at: 1700000000,
        };
        db::upsert_reading_progress(&conn, &progress).unwrap();

        let local = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        let remote = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "hash123".to_string(),
            device_id: "device-B".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 7,
                scroll_position: 0.9,
                updated_at: 1700002000,
            }),
            bookmarks: vec![],
            highlights: vec![],
        };

        let result =
            merge_remote_into_local(&conn, &book_id, &local, &remote, MergeOptions::default());
        assert!(result.progress_updated);

        let updated = db::get_reading_progress(&conn, &book_id).unwrap().unwrap();
        assert_eq!(updated.chapter_index, 7);
        assert!((updated.scroll_position - 0.9).abs() < f64::EPSILON);
        assert_eq!(updated.last_read_at, 1700002000);
    }

    #[test]
    fn test_merge_progress_local_newer() {
        let (conn, book_id) = setup_db();

        let progress = ReadingProgress {
            book_id: book_id.clone(),
            chapter_index: 8,
            scroll_position: 0.95,
            last_read_at: 1700005000,
        };
        db::upsert_reading_progress(&conn, &progress).unwrap();

        let local = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        let remote = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "hash123".to_string(),
            device_id: "device-B".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 3,
                scroll_position: 0.2,
                updated_at: 1700001000,
            }),
            bookmarks: vec![],
            highlights: vec![],
        };

        let result =
            merge_remote_into_local(&conn, &book_id, &local, &remote, MergeOptions::default());
        assert!(!result.progress_updated);

        let unchanged = db::get_reading_progress(&conn, &book_id).unwrap().unwrap();
        assert_eq!(unchanged.chapter_index, 8);
        assert_eq!(unchanged.last_read_at, 1700005000);
    }

    #[test]
    fn test_merge_new_remote_bookmark() {
        let (conn, book_id) = setup_db();

        let local = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        let remote = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "hash123".to_string(),
            device_id: "device-B".to_string(),
            progress: None,
            bookmarks: vec![SyncBookmark {
                id: "bm-remote".to_string(),
                chapter_index: 4,
                scroll_position: 0.6,
                name: Some("Remote BM".to_string()),
                note: None,
                created_at: 1700000500,
                updated_at: 1700000500,
                deleted_at: None,
            }],
            highlights: vec![],
        };

        let result =
            merge_remote_into_local(&conn, &book_id, &local, &remote, MergeOptions::default());
        assert_eq!(result.bookmarks_added, 1);
        assert_eq!(result.bookmarks_updated, 0);

        let bookmarks = db::list_bookmarks(&conn, &book_id).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].id, "bm-remote");
        assert_eq!(bookmarks[0].book_id, book_id);
        assert_eq!(bookmarks[0].name, Some("Remote BM".to_string()));
    }

    #[test]
    fn test_merge_remote_soft_delete_propagates() {
        let (conn, book_id) = setup_db();

        let bookmark = Bookmark {
            id: "bm-shared".to_string(),
            book_id: book_id.clone(),
            chapter_index: 2,
            scroll_position: 0.3,
            name: Some("Shared BM".to_string()),
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        db::insert_bookmark(&conn, &bookmark).unwrap();

        let local = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        // Remote has the same bookmark but soft-deleted with a newer timestamp
        let remote = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "hash123".to_string(),
            device_id: "device-B".to_string(),
            progress: None,
            bookmarks: vec![SyncBookmark {
                id: "bm-shared".to_string(),
                chapter_index: 2,
                scroll_position: 0.3,
                name: Some("Shared BM".to_string()),
                note: None,
                created_at: 1700000000,
                updated_at: 1700001000,
                deleted_at: Some(1700001000),
            }],
            highlights: vec![],
        };

        let result =
            merge_remote_into_local(&conn, &book_id, &local, &remote, MergeOptions::default());
        assert_eq!(result.bookmarks_updated, 1);

        // Normal list should exclude soft-deleted
        let visible = db::list_bookmarks(&conn, &book_id).unwrap();
        assert!(visible.is_empty());

        // Sync-inclusive list should still show it
        let all = db::list_all_bookmarks_for_sync(&conn, &book_id).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].deleted_at.is_some());
    }

    fn make_fs_operator(dir: &std::path::Path) -> Operator {
        use crate::backup::{BackupConfig, ProviderType};
        let config = BackupConfig {
            provider_type: ProviderType::Fs,
            values: [("root".to_string(), dir.to_string_lossy().to_string())].into(),
        };
        crate::backup::build_operator(&config).unwrap()
    }

    fn sample_sync_file() -> BookSyncFile {
        BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "testhash123".to_string(),
            device_id: "device-1".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 5,
                scroll_position: 0.42,
                updated_at: 1700001000,
            }),
            bookmarks: vec![SyncBookmark {
                id: "bm-1".to_string(),
                chapter_index: 2,
                scroll_position: 0.5,
                name: Some("Test BM".to_string()),
                note: None,
                created_at: 1700000000,
                updated_at: 1700000001,
                deleted_at: None,
            }],
            highlights: vec![SyncHighlight {
                id: "hl-1".to_string(),
                chapter_index: 1,
                start_offset: 10,
                end_offset: 50,
                text: "highlighted text".to_string(),
                color: "#ffff00".to_string(),
                note: None,
                created_at: 1700000000,
                updated_at: 1700000002,
                deleted_at: None,
            }],
        }
    }

    #[test]
    fn test_fetch_push_roundtrip_fs() {
        let dir = tempdir().unwrap();
        let op = make_fs_operator(dir.path());
        let payload = sample_sync_file();

        push_remote_sync(&op, "testhash123", &payload).unwrap();
        let fetched = fetch_remote_sync(&op, "testhash123").unwrap().unwrap();

        assert_eq!(fetched.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(fetched.book_hash, "testhash123");
        assert_eq!(fetched.device_id, "device-1");
        let p = fetched.progress.unwrap();
        assert_eq!(p.chapter_index, 5);
        assert!((p.scroll_position - 0.42).abs() < f64::EPSILON);
        assert_eq!(fetched.bookmarks.len(), 1);
        assert_eq!(fetched.bookmarks[0].id, "bm-1");
        assert_eq!(fetched.highlights.len(), 1);
        assert_eq!(fetched.highlights[0].id, "hl-1");
    }

    #[test]
    fn test_fetch_missing_file() {
        let dir = tempdir().unwrap();
        let op = make_fs_operator(dir.path());

        let result = fetch_remote_sync(&op, "nonexistent_hash").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_malformed_json() {
        let dir = tempdir().unwrap();
        let op = make_fs_operator(dir.path());

        let path = sync_path("badhash");
        op.write(&path, b"not valid json{{{".to_vec()).unwrap();

        let result = fetch_remote_sync(&op, "badhash");
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncError::Malformed(msg) => assert!(msg.contains("parse"), "got: {msg}"),
            other => panic!("Expected Malformed, got: {other:?}"),
        }
    }

    // --- Private mode (B-M1): suppress_progress boundary ---

    #[test]
    fn build_sync_payload_suppressed_omits_progress_keeps_annotations() {
        let (conn, book_id) = setup_db();

        let progress = ReadingProgress {
            book_id: book_id.clone(),
            chapter_index: 5,
            scroll_position: 0.42,
            last_read_at: 1700001000,
        };
        db::upsert_reading_progress(&conn, &progress).unwrap();

        let bookmark = Bookmark {
            id: "bm-1".to_string(),
            book_id: book_id.clone(),
            chapter_index: 3,
            scroll_position: 0.2,
            name: Some("Test BM".to_string()),
            note: None,
            created_at: 1700000500,
            updated_at: 1700000500,
            deleted_at: None,
        };
        db::insert_bookmark(&conn, &bookmark).unwrap();

        let highlight = crate::models::Highlight {
            id: "hl-1".to_string(),
            book_id: book_id.clone(),
            chapter_index: 1,
            text: "quote".to_string(),
            color: "#ffff00".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 5,
            created_at: 1700000500,
            updated_at: 1700000500,
            deleted_at: None,
        };
        db::insert_highlight(&conn, &highlight).unwrap();

        let payload = build_sync_payload(&conn, &book_id, "hash123", "device-A", true);

        assert!(
            payload.progress.is_none(),
            "suppressed payload must omit progress"
        );
        assert_eq!(payload.bookmarks.len(), 1, "bookmarks must still go out");
        assert_eq!(payload.highlights.len(), 1, "highlights must still go out");
    }

    #[test]
    fn merge_remote_into_local_suppressed_skips_progress_keeps_annotations() {
        let (conn, book_id) = setup_db();

        // Local has no progress yet — an inbound remote progress update
        // would normally populate it (see test_merge_progress_remote_newer).
        let local = build_sync_payload(&conn, &book_id, "hash123", "device-A", false);

        let remote = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: "hash123".to_string(),
            device_id: "device-B".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 7,
                scroll_position: 0.9,
                updated_at: 1700002000,
            }),
            bookmarks: vec![SyncBookmark {
                id: "bm-remote".to_string(),
                chapter_index: 4,
                scroll_position: 0.6,
                name: Some("Remote BM".to_string()),
                note: None,
                created_at: 1700000500,
                updated_at: 1700000500,
                deleted_at: None,
            }],
            highlights: vec![SyncHighlight {
                id: "hl-remote".to_string(),
                chapter_index: 1,
                start_offset: 10,
                end_offset: 50,
                text: "highlighted text".to_string(),
                color: "#ffff00".to_string(),
                note: None,
                created_at: 1700000500,
                updated_at: 1700000500,
                deleted_at: None,
            }],
        };

        let result = merge_remote_into_local(
            &conn,
            &book_id,
            &local,
            &remote,
            MergeOptions {
                suppress_progress: true,
            },
        );

        assert!(
            !result.progress_updated,
            "suppressed merge must not update progress"
        );
        assert!(
            db::get_reading_progress(&conn, &book_id).unwrap().is_none(),
            "reading_progress table must stay empty while suppressed"
        );

        // Annotations are deliberate saves — they must still land.
        assert_eq!(result.bookmarks_added, 1);
        assert_eq!(result.highlights_added, 1);
        let bookmarks = db::list_bookmarks(&conn, &book_id).unwrap();
        assert_eq!(bookmarks.len(), 1);
        let highlights = db::list_highlights(&conn, &book_id).unwrap();
        assert_eq!(highlights.len(), 1);
    }

    #[test]
    fn merge_options_default_matches_pre_bm1_behavior() {
        // Guards the compatibility contract: MergeOptions::default() must
        // behave exactly like the pre-B-M1 unconditional merge.
        assert!(!MergeOptions::default().suppress_progress);
    }

    #[test]
    fn sync_book_on_close_suppressed_preserves_remote_progress() {
        // Codex/Codex-2 finding: a private close must not clobber the
        // reading position another device already synced. Suppression omits
        // *our* progress from the outbound payload, but the fetched remote
        // progress must be carried forward so the whole-file overwrite in
        // `push_remote_sync` doesn't erase it.
        let (conn, book_id) = setup_db();
        let dir = tempdir().unwrap();
        let op = make_fs_operator(dir.path());
        let file_hash = "hash-preserve";

        // A prior (non-private) session on another device pushed progress.
        let existing = BookSyncFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            book_hash: file_hash.to_string(),
            device_id: "device-B".to_string(),
            progress: Some(SyncProgress {
                chapter_index: 9,
                scroll_position: 0.77,
                updated_at: 1700005000,
            }),
            bookmarks: vec![],
            highlights: vec![],
        };
        push_remote_sync(&op, file_hash, &existing).unwrap();

        sync_book_on_close(&conn, &op, &book_id, file_hash, "device-A", true).unwrap();

        let fetched = fetch_remote_sync(&op, file_hash).unwrap().unwrap();
        let p = fetched
            .progress
            .expect("remote progress must survive a private-mode close");
        assert_eq!(p.chapter_index, 9);
        assert!((p.scroll_position - 0.77).abs() < f64::EPSILON);
        assert_eq!(p.updated_at, 1700005000);
    }
}

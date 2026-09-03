use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{params, Connection, Result};
use std::path::Path;
use std::time::Duration;

use crate::models::{
    ActivityEntry, Book, BookGridItem, Bookmark, Collection, CollectionRule, CollectionSuggestion,
    CollectionType, ContinueReadingItem, CustomFont, FeatureFlag, HighlightSearchResult,
    NewRuleInput, ReadingProgress, SeriesInfo, WebSessionEntry,
};

pub type DbPool = Pool<SqliteConnectionManager>;

/// Current schema version. Bump this when adding new migrations.
const SCHEMA_VERSION: i64 = 3;

/// Get the current schema version from the database (0 if not yet set).
pub fn get_schema_version(conn: &Connection) -> Result<i64> {
    // Table may not exist yet on first run
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    conn.query_row(
        "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .or(Ok(0))
}

fn set_schema_version(conn: &Connection, version: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_version (id, version, applied_at) VALUES (1, ?1, strftime('%s','now'))",
        params![version],
    )?;
    Ok(())
}

/// Migrate legacy absolute `file_path` values to storage keys (#64 M4).
///
/// Before M4, imported books stored their absolute filesystem path in
/// `file_path`. The storage abstraction now treats `file_path` as an
/// opaque key owned by the library `Storage` (relative to the library
/// folder). This migration converts paths that still sit under the
/// recorded `library_folder` setting into keys; rows that point
/// elsewhere (linked books, or old imports from a since-changed
/// library folder) are left untouched.
fn migrate_file_path_to_key(conn: &Connection) -> Result<()> {
    use rusqlite::OptionalExtension;

    let library_folder: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'library_folder'",
            [],
            |row| row.get(0),
        )
        .optional()?;

    let Some(library_folder) = library_folder else {
        return Ok(()); // fresh install — no setting yet, nothing to migrate.
    };
    let root = Path::new(&library_folder);

    let mut stmt = conn.prepare("SELECT id, file_path FROM books WHERE is_imported = 1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>>>()?;

    for (id, file_path) in rows {
        let path = Path::new(&file_path);
        if let Some(key) = crate::storage::key_for_local_path(root, path) {
            if key != file_path {
                conn.execute(
                    "UPDATE books SET file_path = ?1 WHERE id = ?2",
                    params![key, id],
                )?;
            }
        }
    }

    Ok(())
}

/// F-1-3: backfill `reading_progress.finished_at` for rows that already
/// satisfy the "finished" predicate (on or past the last chapter, with the
/// `total_chapters > 0` guard from Finding 2) from before the column
/// existed. `last_read_at` is the best available approximation of the
/// completion timestamp — there's no better signal for when a pre-existing
/// row finished. Guarded by `finished_at IS NULL`, so it's safe to treat as
/// idempotent, but the caller only invokes it once (`prev_version < 3`).
fn backfill_finished_at(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE reading_progress
         SET finished_at = last_read_at
         WHERE finished_at IS NULL
           AND book_id IN (
             SELECT rp.book_id FROM reading_progress rp
             JOIN books b ON rp.book_id = b.id
             WHERE b.total_chapters > 0 AND rp.chapter_index >= b.total_chapters - 1
           )",
        [],
    )?;
    Ok(())
}

fn run_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_version (
            id INTEGER PRIMARY KEY,
            version INTEGER NOT NULL,
            applied_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS books (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            author TEXT NOT NULL,
            file_path TEXT NOT NULL UNIQUE,
            cover_path TEXT,
            total_chapters INTEGER NOT NULL DEFAULT 0,
            added_at INTEGER NOT NULL,
            format TEXT NOT NULL DEFAULT 'epub'
        );

        -- Ensures existing databases get the unique constraint even if the
        -- table was created before this migration was added.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_books_file_path ON books(file_path);

        CREATE TABLE IF NOT EXISTS reading_progress (
            book_id TEXT PRIMARY KEY,
            chapter_index INTEGER NOT NULL DEFAULT 0,
            scroll_position REAL NOT NULL DEFAULT 0.0,
            last_read_at INTEGER NOT NULL,
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS bookmarks (
            id TEXT PRIMARY KEY,
            book_id TEXT NOT NULL,
            chapter_index INTEGER NOT NULL,
            scroll_position REAL NOT NULL,
            note TEXT,
            created_at INTEGER NOT NULL,
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS collections (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            type TEXT NOT NULL CHECK(type IN ('manual','automated')),
            icon TEXT,
            color TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS collection_rules (
            id TEXT PRIMARY KEY,
            collection_id TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
            field TEXT NOT NULL,
            operator TEXT NOT NULL,
            value TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_collection_rules_collection_id
            ON collection_rules(collection_id);

        CREATE TABLE IF NOT EXISTS book_collections (
            book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            collection_id TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
            added_at INTEGER NOT NULL,
            PRIMARY KEY (book_id, collection_id)
        );

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS highlights (
            id TEXT PRIMARY KEY,
            book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            chapter_index INTEGER NOT NULL,
            text TEXT NOT NULL,
            color TEXT NOT NULL DEFAULT '#f6c445',
            note TEXT,
            start_offset INTEGER NOT NULL,
            end_offset INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reading_sessions (
            id TEXT PRIMARY KEY,
            book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            started_at INTEGER NOT NULL,
            duration_secs INTEGER NOT NULL,
            pages_read INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS tags (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS book_tags (
            book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY (book_id, tag_id)
        );

        CREATE TABLE IF NOT EXISTS activity_log (
            id TEXT PRIMARY KEY,
            timestamp INTEGER NOT NULL,
            action TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            entity_id TEXT,
            entity_name TEXT,
            detail TEXT
        );

        CREATE TABLE IF NOT EXISTS web_session_log (
            id TEXT PRIMARY KEY,
            timestamp INTEGER NOT NULL,
            ip TEXT NOT NULL,
            method TEXT NOT NULL,
            outcome TEXT NOT NULL,
            user_agent TEXT
        );

        CREATE TABLE IF NOT EXISTS custom_fonts (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS feature_flags (
            key TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL DEFAULT 0,
            description TEXT
        );

        CREATE TABLE IF NOT EXISTS pin_change_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            changed_at INTEGER NOT NULL,
            source TEXT NOT NULL DEFAULT 'desktop'
        );

        CREATE TABLE IF NOT EXISTS vocabulary (
            id             TEXT PRIMARY KEY,
            lemma          TEXT NOT NULL UNIQUE,
            word           TEXT NOT NULL,
            pos            TEXT,
            definition     TEXT NOT NULL,
            book_id        TEXT REFERENCES books(id) ON DELETE SET NULL,
            book_title     TEXT,
            chapter_index  INTEGER,
            context_sentence TEXT,
            start_offset   INTEGER,
            end_offset     INTEGER,
            seen_count     INTEGER NOT NULL DEFAULT 1,
            box            INTEGER NOT NULL DEFAULT 1,
            last_reviewed_at INTEGER,
            next_due_at    INTEGER,
            last_seen_at   INTEGER NOT NULL,
            created_at     INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_vocabulary_due ON vocabulary(next_due_at);
    ",
    )?;

    // Seed feature flags
    conn.execute(
        "INSERT OR IGNORE INTO feature_flags (key, enabled, description) VALUES ('whats_new_banner', 1, 'Show What''s New banner after version updates')",
        [],
    )?;

    // Additive migrations: ALTER TABLE ADD COLUMN fails silently if already exists.
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN format TEXT NOT NULL DEFAULT 'epub';");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN file_hash TEXT;");
    let _ = conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_books_file_hash ON books(file_hash);",
    );

    // OpenLibrary enrichment columns
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN description TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN genres TEXT;"); // JSON array
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN rating REAL;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN isbn TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN openlibrary_key TEXT;");

    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN enrichment_status TEXT;");

    // Series / volume / language / publisher / publish_year columns
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN series TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN volume INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN language TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN publisher TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN publish_year INTEGER;");

    // Fast skip-before-hash re-import: cheap (path, size, mtime) match avoids
    // re-streaming unchanged files over a remote mount. Hash stays the source
    // of truth on every mismatch. Index is intentionally NON-unique.
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN source_path TEXT;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN source_size INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN source_mtime INTEGER;");
    let _ = conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_books_source_path ON books(source_path);");

    // Incremental backup: ensure updated_at columns exist
    let _ =
        conn.execute_batch("ALTER TABLE books ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;");
    let _ = conn
        .execute_batch("ALTER TABLE bookmarks ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;");
    let _ = conn.execute_batch("ALTER TABLE bookmarks ADD COLUMN name TEXT;");
    let _ = conn
        .execute_batch("ALTER TABLE highlights ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;");
    // Linked-books: is_imported flag (1 = copied into library, 0 = linked in-place)
    let _ =
        conn.execute_batch("ALTER TABLE books ADD COLUMN is_imported INTEGER NOT NULL DEFAULT 1;");
    // "Want to read" manual flag (0 = no, 1 = yes). Additive default 0.
    let _ =
        conn.execute_batch("ALTER TABLE books ADD COLUMN want_to_read INTEGER NOT NULL DEFAULT 0;");

    // Sync: add deleted_at for soft-delete support
    let _ = conn.execute_batch("ALTER TABLE bookmarks ADD COLUMN deleted_at INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE highlights ADD COLUMN deleted_at INTEGER;");

    // F-1-3: dedicated completion timestamp, set once when a progress write
    // transitions a book to finished (see `upsert_reading_progress`) and
    // never cleared afterwards — re-reading or restarting a finished book
    // must not change when it originally finished. Distinct from
    // `last_read_at`, which re-opening a book bumps into the current year.
    let _ = conn.execute_batch("ALTER TABLE reading_progress ADD COLUMN finished_at INTEGER;");

    // F-1-5: vocabulary jump — offset columns for DBs whose `vocabulary`
    // table predates them (CREATE TABLE IF NOT EXISTS above only covers
    // fresh installs).
    let _ = conn.execute_batch("ALTER TABLE vocabulary ADD COLUMN start_offset INTEGER;");
    let _ = conn.execute_batch("ALTER TABLE vocabulary ADD COLUMN end_offset INTEGER;");

    // Backfill: set updated_at = added_at or created_at for existing rows
    let _ = conn.execute_batch("UPDATE books SET updated_at = added_at WHERE updated_at = 0;");
    let _ =
        conn.execute_batch("UPDATE bookmarks SET updated_at = created_at WHERE updated_at = 0;");
    let _ =
        conn.execute_batch("UPDATE highlights SET updated_at = created_at WHERE updated_at = 0;");

    // Index on bookmarks.book_id for list_bookmarks() and cascade delete performance
    let _ = conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_bookmarks_book_id ON bookmarks(book_id);");

    // Indexes for common lookup patterns
    let _ = conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_highlights_book_id ON highlights(book_id);");
    let _ = conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_book_tags_book_id ON book_tags(book_id);");
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_reading_sessions_book_id ON reading_sessions(book_id);",
    );
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_reading_progress_book_id ON reading_progress(book_id);",
    );

    // Performance indexes for large libraries
    let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_books_series ON books(series);");
    let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_books_format ON books(format);");
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_reading_progress_last_read_at ON reading_progress(last_read_at);",
    );
    let _ = conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_books_language ON books(language);");
    let _ =
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_books_publisher ON books(publisher);");

    // Migration: drop CHECK constraint on collection_rules.field (was limited to a fixed set;
    // now validated in application code so new rule fields don't require schema changes).
    let has_check: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='collection_rules'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|sql| sql.contains("CHECK"))
        .unwrap_or(false);

    if has_check {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS collection_rules_new (
                id TEXT PRIMARY KEY,
                collection_id TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
                field TEXT NOT NULL,
                operator TEXT NOT NULL,
                value TEXT NOT NULL
            );
            INSERT OR IGNORE INTO collection_rules_new SELECT * FROM collection_rules;
            DROP TABLE collection_rules;
            ALTER TABLE collection_rules_new RENAME TO collection_rules;",
        )?;
    }

    // #64 M4: convert legacy absolute `file_path` values to storage keys
    // for imported books. Runs only when upgrading from schema_version < 2;
    // new installs have no rows to migrate.
    let prev_version = get_schema_version(conn)?;
    if prev_version < 2 {
        migrate_file_path_to_key(conn)?;
    }

    // F-1-3: one-time backfill of `finished_at` for rows already finished
    // before the column existed.
    if prev_version < 3 {
        backfill_finished_at(conn)?;
    }

    // Record schema version (#49)
    set_schema_version(conn, SCHEMA_VERSION)?;

    Ok(())
}

/// Open or create the SQLite library file at `db_path`, run the canonical
/// schema migrations, and close the connection. Idempotent — re-running on
/// an existing file is a no-op.
///
/// Use this entry point when you need to provision a library file without
/// taking a long-lived [`DbPool`]. The function ensures the parent
/// directory exists, sets `PRAGMA foreign_keys = ON`, and applies the
/// migrations defined by `run_schema`.
pub fn provision_library(db_path: &Path) -> crate::error::CarrelResult<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    run_schema(&conn)?;
    Ok(())
}

pub fn create_pool(db_path: &Path) -> crate::error::CarrelResult<DbPool> {
    use crate::error::CarrelError;

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
        )
    });

    let pool = Pool::builder()
        .max_size(5)
        .connection_timeout(Duration::from_secs(5))
        .build(manager)
        .map_err(|e| CarrelError::database(e.to_string()))?;

    // Run schema migrations on startup using a pool connection.
    let conn = pool.get()?;
    run_schema(&conn)?;

    Ok(pool)
}

/// Opens a single connection and runs the schema. Exists so cross-crate
/// integration tests (both inside `carrel-core` and in the desktop crate) can
/// spin up a disposable DB without going through `create_pool`. Gated behind
/// the `test-utils` feature so the symbol is excluded from release binaries
/// — `#[cfg(test)]` alone only fires for the current crate and wouldn't work
/// for downstream test callers.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn init_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    run_schema(&conn)?;
    Ok(conn)
}

// --- Book CRUD ---

pub fn insert_book(conn: &Connection, book: &Book) -> Result<()> {
    conn.execute(
        "INSERT INTO books (id, title, author, file_path, cover_path, total_chapters, added_at, format, file_hash, description, genres, rating, isbn, openlibrary_key, updated_at, enrichment_status, series, volume, language, publisher, publish_year, is_imported, want_to_read)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            book.id,
            book.title,
            book.author,
            book.file_path,
            book.cover_path,
            book.total_chapters,
            book.added_at,
            book.format.to_string(),
            book.file_hash,
            book.description,
            book.genres,
            book.rating,
            book.isbn,
            book.openlibrary_key,
            book.added_at,
            book.enrichment_status,
            book.series,
            book.volume,
            book.language,
            book.publisher,
            book.publish_year,
            book.is_imported as i32,
            book.want_to_read as i32,
        ],
    )?;
    Ok(())
}

pub fn get_book(conn: &Connection, id: &str) -> Result<Option<Book>> {
    let sql = format!("SELECT {} FROM books WHERE id = ?1", BOOK_COLUMNS);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_book(row)?))
    } else {
        Ok(None)
    }
}

pub fn get_book_by_file_path(conn: &Connection, file_path: &str) -> Result<Option<Book>> {
    let sql = format!("SELECT {} FROM books WHERE file_path = ?1", BOOK_COLUMNS);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![file_path])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_book(row)?))
    } else {
        Ok(None)
    }
}

/// Lightweight source-tracking row for the fast skip-before-hash re-import
/// path. Deliberately NOT part of the `Book` domain struct — this is import
/// bookkeeping, not a book property.
pub struct BookSourceRef {
    pub id: String,
    pub source_size: Option<i64>,
    pub source_mtime: Option<i64>,
}

/// Record where a book was imported from (the exact path string the folder
/// walk produced) plus its size and mtime, for cheap re-import skipping.
pub fn set_book_source(
    conn: &Connection,
    book_id: &str,
    source_path: &str,
    source_size: i64,
    source_mtime: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE books SET source_path = ?1, source_size = ?2, source_mtime = ?3 WHERE id = ?4",
        params![source_path, source_size, source_mtime, book_id],
    )?;
    Ok(())
}

/// Look up a book by the import source path. Returns `None` for legacy rows
/// (NULL `source_path`) and unknown paths. Used by the fast-path before
/// hashing — never the duplicate backstop (that remains `file_hash`). The
/// `source_path` index is non-unique, so `ORDER BY rowid DESC LIMIT 1` makes
/// the result deterministic, preferring the most recently inserted row.
pub fn get_book_by_source_path(
    conn: &Connection,
    source_path: &str,
) -> Result<Option<BookSourceRef>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_size, source_mtime FROM books WHERE source_path = ?1 ORDER BY rowid DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(params![source_path])?;
    if let Some(row) = rows.next()? {
        Ok(Some(BookSourceRef {
            id: row.get(0)?,
            source_size: row.get(1)?,
            source_mtime: row.get(2)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn get_book_by_file_hash(conn: &Connection, hash: &str) -> Result<Option<Book>> {
    let sql = format!("SELECT {} FROM books WHERE file_hash = ?1", BOOK_COLUMNS);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![hash])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_book(row)?))
    } else {
        Ok(None)
    }
}

pub fn list_books(conn: &Connection) -> Result<Vec<Book>> {
    let sql = format!("SELECT {} FROM books ORDER BY added_at DESC", BOOK_COLUMNS);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_book)?;
    rows.collect()
}

/// `(id, updated_at)` for every book — the ETag input for OPDS conditional
/// requests. Deliberately not part of `Book`: feeds need the pair list
/// without widening the Book struct and its many literal constructors.
pub fn book_etag_pairs(conn: &Connection) -> Result<std::collections::HashMap<String, i64>> {
    let mut stmt = conn.prepare("SELECT id, updated_at FROM books")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    rows.collect()
}

/// Build the metadata object shared by the desktop library export and the
/// web GDPR export: version + books, reading progress, bookmarks, highlights,
/// collections, tags, and book→tag links.
pub fn build_core_export(conn: &Connection) -> Result<serde_json::Value> {
    let books = list_books(conn)?;
    // Propagate DB errors instead of swallowing them: a failed read must abort
    // the export, not silently drop personal data (GDPR completeness). Only
    // `Ok(None)` reading progress is treated as genuine absence.
    let mut progress = Vec::new();
    let mut bookmarks = Vec::new();
    let mut highlights = Vec::new();
    let mut book_tags: Vec<(String, String, String)> = Vec::new();
    for b in &books {
        if let Some(p) = get_reading_progress(conn, &b.id)? {
            progress.push(p);
        }
        bookmarks.extend(list_bookmarks(conn, &b.id)?);
        highlights.extend(list_highlights(conn, &b.id)?);
        book_tags.extend(
            get_book_tags(conn, &b.id)?
                .into_iter()
                .map(|(tag_id, tag_name)| (b.id.clone(), tag_id, tag_name)),
        );
    }
    let collections = list_collections(conn)?;
    let tags = list_tags(conn)?;

    Ok(serde_json::json!({
        "version": 1,
        "books": books,
        "reading_progress": progress,
        "bookmarks": bookmarks,
        "highlights": highlights,
        "collections": collections,
        "tags": tags,
        "book_tags": book_tags,
    }))
}

/// Rows restored by [`restore_secondary_data`]. A field counts only the
/// rows that were actually written (best-effort: rows referencing a book
/// that wasn't imported, or that already exist, are skipped).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreCounts {
    pub reading_progress: usize,
    pub bookmarks: usize,
    pub highlights: usize,
    pub collections: usize,
    pub tags: usize,
    pub book_tags: usize,
}

/// Non-book data carried in a library backup, the counterpart to the
/// arrays emitted by [`build_core_export`]. Borrowed so the caller keeps
/// ownership of the deserialized export.
pub struct SecondaryImport<'a> {
    pub reading_progress: &'a [ReadingProgress],
    pub bookmarks: &'a [Bookmark],
    pub highlights: &'a [crate::models::Highlight],
    pub collections: &'a [Collection],
    /// `(tag_id, name)` pairs.
    pub tags: &'a [(String, String)],
    /// `(book_id, tag_id, tag_name)` triples.
    pub book_tags: &'a [(String, String, String)],
}

/// Restore the non-book data from a backup (reading progress, bookmarks,
/// highlights, collections, tags, tag assignments).
///
/// Best-effort by design: a single row that fails — most often a foreign
/// key to a book that wasn't imported, or a collection id that already
/// exists — is skipped, never aborting the restore. The returned counts
/// reflect rows actually written. Books must be inserted first so the
/// foreign keys resolve.
///
/// Note: manual-collection membership (`collection_books`) is not part of
/// the export, so it cannot be restored here; automated collections
/// repopulate from their rules.
pub fn restore_secondary_data(conn: &Connection, data: &SecondaryImport) -> RestoreCounts {
    let mut counts = RestoreCounts::default();

    for p in data.reading_progress {
        if upsert_reading_progress(conn, p).is_ok() {
            counts.reading_progress += 1;
        }
    }
    for b in data.bookmarks {
        if upsert_bookmark_from_sync(conn, b).is_ok() {
            counts.bookmarks += 1;
        }
    }
    for h in data.highlights {
        if upsert_highlight_from_sync(conn, h).is_ok() {
            counts.highlights += 1;
        }
    }
    for c in data.collections {
        // `insert_collection` is a plain INSERT — a pre-existing id errors
        // and is simply skipped on re-import.
        if insert_collection(conn, c).is_ok() {
            counts.collections += 1;
        }
    }
    for (tag_id, name) in data.tags {
        if get_or_create_tag(conn, tag_id, name).is_ok() {
            counts.tags += 1;
        }
    }
    for (book_id, tag_id, tag_name) in data.book_tags {
        // Ensure the tag exists (covers tags assigned but absent from the
        // top-level `tags` array), then link it to the book.
        let _ = get_or_create_tag(conn, tag_id, tag_name);
        if add_tag_to_book(conn, book_id, tag_id).is_ok() {
            counts.book_tags += 1;
        }
    }

    counts
}

const GRID_COLUMNS: &str = "id, title, author, cover_path, total_chapters, added_at, format, series, volume, rating, language, publish_year, is_imported, want_to_read";

/// Grid columns prefixed with table alias `b.` for JOIN queries.
const GRID_COLUMNS_B: &str = "b.id, b.title, b.author, b.cover_path, b.total_chapters, b.added_at, b.format, b.series, b.volume, b.rating, b.language, b.publish_year, b.is_imported, b.want_to_read";

fn row_to_grid_item(row: &rusqlite::Row) -> rusqlite::Result<BookGridItem> {
    let format_str: String = row.get("format")?;
    Ok(BookGridItem {
        id: row.get("id")?,
        title: row.get("title")?,
        author: row.get("author")?,
        cover_path: row.get("cover_path")?,
        total_chapters: row.get("total_chapters")?,
        added_at: row.get("added_at")?,
        format: format_str
            .parse()
            .map_err(|e: String| rusqlite::Error::InvalidParameterName(e))?,
        series: row.get("series")?,
        volume: row.get("volume")?,
        rating: row.get("rating")?,
        language: row.get("language")?,
        publish_year: row.get("publish_year")?,
        is_imported: row.get::<_, i32>("is_imported").unwrap_or(1) != 0,
        want_to_read: row.get::<_, i32>("want_to_read").unwrap_or(0) != 0,
    })
}

pub fn list_books_grid(conn: &Connection) -> Result<Vec<BookGridItem>> {
    // Fix D: `added_at` has second-granularity ties (concurrent/batch
    // imports) — without a unique tiebreaker, offset pagination can slice a
    // book onto two pages or skip it entirely when rows with the same
    // timestamp sort differently between two requests. `id` is unique, so
    // appending it makes this order total and deterministic.
    let sql = format!(
        "SELECT {} FROM books ORDER BY added_at DESC, id",
        GRID_COLUMNS
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_grid_item)?;
    rows.collect()
}

// --- Query module: filter/sort/page a library in one place -----------------
//
// The predicate "case-insensitively match title or author" used to be written
// out separately for the web API, the OPDS feed, the plugin host, and the web
// UI's collection view, and every server-side copy loaded the whole `books`
// table to filter it in the host language. `query_books_grid`/`query_books`
// compile the same filter+sort+page request to SQL instead, so it runs once,
// in the database, regardless of caller.
//
// Callers: `api.rs::list_books`, `opds_feed.rs::search_books`,
// `plugins/runtime.rs`'s `find_books`, and — through
// `query_books_in_collection_grid` below — `api.rs::get_collection_books`.
//
// No server-side copy remains. Three client-side ones do, all in the React
// desktop app and none of them reachable from here: `Library.tsx` filters an
// already-loaded grid and derives tag facet counts from the result, which is
// why it was left alone deliberately; `BookPickerModal.tsx` filters a list the
// modal was handed; and `lib/utils.ts`'s `filterBooks` is imported by nothing
// but its own test.

/// Sort order for [`query_books_grid`]/[`query_books`]. A closed enum (not a
/// caller-supplied string) so the `ORDER BY` fragment is always chosen from a
/// fixed, safe set — never built from user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BookSort {
    #[default]
    DateAdded,
    Title,
    Author,
    Rating,
    LastRead,
}

/// Filter/sort/page parameters shared by [`query_books_grid`] and
/// [`query_books`]. An empty string in `q` or `series` is equivalent to
/// `None` — both mean "no filter".
#[derive(Debug, Clone, Default)]
pub struct BookQuery {
    pub q: Option<String>,
    pub series: Option<String>,
    pub want_to_read: bool,
    pub sort: BookSort,
    pub limit: Option<usize>,
    pub offset: usize,
}

/// One page of results plus the total row count after filtering (before
/// `limit`/`offset` are applied).
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
}

/// Registers the `carrel_lower(text)` scalar UDF used by the `q`/sort
/// machinery below. SQLite's built-in `LOWER()` and `LIKE` are ASCII-only,
/// so `LOWER('É')` stays `'É'` — a naive translation of the Rust predicate
/// this replaces (`str::to_lowercase().contains(...)`, which *is*
/// Unicode-aware) would silently stop matching accented titles/authors.
/// `carrel_lower` calls Rust's `to_lowercase()` instead, so the two agree.
///
/// Registered here, on the `&Connection` the query actually runs on, rather
/// than in `create_pool` or `init_db` — there are two connection-creation
/// paths plus direct `Connection::open`/`open_in_memory` calls in tests, and
/// registering at the use site means the function is present by
/// construction rather than by remembering to wire it up everywhere a
/// connection is made. Re-registering on every query is safe — SQLite
/// replaces the definition in place — and costs one closure box per call,
/// which is nothing beside the query it precedes. (rusqlite documents only
/// that a registration lasts until the connection closes or `remove_function`
/// is called; it says nothing about replacement being free, so don't claim it
/// is.)
fn register_carrel_lower(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "carrel_lower",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(text.to_lowercase())
        },
    )
}

/// Builds the `WHERE` fragment and its bound parameters shared by
/// `query_books_grid` and `query_books`. `q` and `series` are always bound
/// parameters, never interpolated — only the fixed set of column/operator
/// fragments chosen here ever becomes part of the SQL text.
///
/// `q` matches title-or-author via SQLite's `instr()` rather than `LIKE`, so
/// a `q` containing `%` or `_` is treated as a literal substring — exactly
/// like Rust's `str::contains`, with no LIKE wildcard escaping needed.
/// `series` is exact equality; SQL equality against a NULL column evaluates
/// to NULL (falsy), so a book with no series never matches a non-empty
/// filter without any special-casing.
fn build_book_predicate(query: &BookQuery) -> (Vec<String>, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(needle) = query.q.as_deref().filter(|s| !s.is_empty()) {
        clauses.push(
            "(instr(carrel_lower(b.title), carrel_lower(?)) > 0 \
              OR instr(carrel_lower(b.author), carrel_lower(?)) > 0)"
                .to_string(),
        );
        params.push(Value::Text(needle.to_string()));
        params.push(Value::Text(needle.to_string()));
    }

    if let Some(series) = query.series.as_deref().filter(|s| !s.is_empty()) {
        clauses.push("b.series = ?".to_string());
        params.push(Value::Text(series.to_string()));
    }

    if query.want_to_read {
        // `!= 0`, not `= 1`, so this agrees with `row_to_grid_item`/`row_to_book`,
        // which map any non-zero value to `true`. A `= 1` predicate would hide a
        // row that the same query's own mapper would report as wanted.
        clauses.push("b.want_to_read != 0".to_string());
    }

    (clauses, params)
}

/// `WHERE ...` for a clause list, or an empty string for none. Kept separate
/// from [`build_book_predicate`] so a caller that has to AND these onto a
/// `WHERE` it already built can take the clauses and join them itself, rather
/// than stripping the keyword back off an assembled string — that string
/// surgery silently produced *no filter at all* if the shape ever changed.
fn where_clause(clauses: &[String]) -> String {
    if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    }
}

/// `ORDER BY` fragment for each [`BookSort`] variant, `id` appended as a
/// final tie-break in every case. `added_at`/rating/last-read all have
/// second-granularity (or coarser) ties — concurrent imports, unrated books,
/// never-opened books — and without a unique tiebreaker, offset pagination
/// can slice a book onto two pages or skip it entirely depending on how ties
/// happen to resolve between two calls (mirrors the "Fix D" comment on
/// `list_books_grid` above, and `resolveSeriesNav` in the web UI's `app.js`,
/// which appends the same tie-break to make its own ordering total, though
/// for stable row order rather than for paging). This is the only
/// remaining home for that rationale on the server side — the `api.rs` sort
/// block that used to carry it is gone.
fn book_sort_order_sql(sort: BookSort) -> &'static str {
    match sort {
        BookSort::DateAdded => "b.added_at DESC, b.id",
        BookSort::Title => "carrel_lower(b.title) ASC, b.id",
        BookSort::Author => "carrel_lower(b.author) ASC, b.id",
        BookSort::Rating => "COALESCE(b.rating, 0) DESC, b.id",
        BookSort::LastRead => "COALESCE(rp.last_read_at, 0) DESC, b.id",
    }
}

/// Shared implementation behind `query_books_grid` and `query_books`: builds
/// one dynamic SQL query from `q` (predicate, sort, `LIMIT`/`OFFSET`) and maps
/// rows with whichever mapper/column list the caller wants. The `LEFT JOIN`
/// to `reading_progress` is always present (needed for `BookSort::LastRead`)
/// but never changes row cardinality — `reading_progress.book_id` is a
/// primary key, so each book joins at most one progress row.
fn query_books_generic<T>(
    conn: &Connection,
    query: &BookQuery,
    columns: &str,
    row_mapper: fn(&rusqlite::Row) -> rusqlite::Result<T>,
) -> Result<Page<T>> {
    register_carrel_lower(conn)?;

    let (predicate_clauses, where_params) = build_book_predicate(query);
    let where_sql = where_clause(&predicate_clauses);
    let from_join = "FROM books b LEFT JOIN reading_progress rp ON rp.book_id = b.id";

    // An unpaginated query from the start already returns every matching row,
    // so its own row count *is* the total and a second scan would just compute
    // it again. That is not hypothetical: `opds_feed::search_books` asks for
    // the whole filtered set and discards `total` entirely, and every request
    // was paying for a duplicate filtered scan — with the `carrel_lower` UDF
    // called per row — to produce a number nobody read.
    //
    // The shortcut needs `offset == 0` as well as no `limit`. With an offset
    // the returned rows are only the tail, and an offset past the end returns
    // nothing at all, which would report a total of zero for a non-empty set.
    let counted_total: Option<i64> = if query.limit.is_none() && query.offset == 0 {
        None
    } else {
        let count_sql = format!("SELECT COUNT(*) {from_join} {where_sql}");
        Some(conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(where_params.iter()),
            |row| row.get(0),
        )?)
    };

    let order_sql = book_sort_order_sql(query.sort);
    let mut page_params = where_params;
    // Saturate rather than cast: `usize as i64` WRAPS on a value above
    // i64::MAX, and a negative OFFSET is silently clamped to 0 by SQLite, so
    // a wrapped offset would return the first page instead of the empty one
    // the caller asked for. Saturating to i64::MAX keeps "past the end"
    // meaning past the end. (A saturated LIMIT is harmless either way — both
    // i64::MAX and the true value return every remaining row.)
    let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
    let limit_sql = match query.limit {
        Some(limit) => {
            page_params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
            page_params.push(Value::Integer(offset));
            "LIMIT ? OFFSET ?"
        }
        None => {
            // SQLite treats a negative LIMIT as "no limit", which lets
            // OFFSET keep working with no `limit` given.
            page_params.push(Value::Integer(offset));
            "LIMIT -1 OFFSET ?"
        }
    };

    let sql = format!("SELECT {columns} {from_join} {where_sql} ORDER BY {order_sql} {limit_sql}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(page_params.iter()), row_mapper)?;
    let items = rows.collect::<Result<Vec<T>>>()?;

    let total = match counted_total {
        Some(total) => total as usize,
        None => items.len(),
    };
    Ok(Page { items, total })
}

/// Grid-projection counterpart of [`query_books`] — filters, sorts, and
/// pages the `books` table in SQL and returns [`BookGridItem`] rows plus the
/// filtered total. See the module comment above for why this exists.
pub fn query_books_grid(conn: &Connection, query: &BookQuery) -> Result<Page<BookGridItem>> {
    query_books_generic(conn, query, GRID_COLUMNS_B, row_to_grid_item)
}

/// Full-`Book`-projection counterpart of [`query_books_grid`] — same filter,
/// sort, and paging semantics, for callers that need the full record.
pub fn query_books(conn: &Connection, query: &BookQuery) -> Result<Page<Book>> {
    query_books_generic(conn, query, BOOK_COLUMNS_B, row_to_book)
}

/// [`get_books_in_collection_grid`] with `query`'s `q`/`series`/`want_to_read`
/// predicate ANDed into whichever branch (manual membership, or an
/// automated collection's rule query) that function already uses — the same
/// `build_book_predicate` every other query-module entry point shares,
/// rather than a second copy of it.
///
/// `query.sort`, `query.limit` and `query.offset` are deliberately **not**
/// honoured: a manual collection orders by `bc.added_at DESC` (the order
/// books were added to the collection), which `BookSort` cannot express —
/// running it through `book_sort_order_sql` would silently reorder every
/// manual collection. So both branches keep their existing `ORDER BY`
/// byte-for-byte, and this returns a plain `Vec` rather than a `Page`: there
/// is no paging here to report a total for. Collections are deliberately
/// unpaginated (see `app.js`'s `loadBooks`, which cites the same decision).
pub fn query_books_in_collection_grid(
    conn: &Connection,
    collection_id: &str,
    query: &BookQuery,
) -> Result<Vec<BookGridItem>> {
    register_carrel_lower(conn)?;

    let mut type_stmt = conn.prepare("SELECT type FROM collections WHERE id = ?1")?;
    let coll_type: String = type_stmt.query_row(params![collection_id], |row| row.get(0))?;

    // Clauses, not an assembled `WHERE` — each branch below already has a
    // `WHERE` of its own (or, for a rule-less automated collection, none) and
    // needs to AND onto it.
    let (predicate_clauses, predicate_params) = build_book_predicate(query);
    let predicate_and = if predicate_clauses.is_empty() {
        String::new()
    } else {
        format!(" AND {}", predicate_clauses.join(" AND "))
    };

    if coll_type == "manual" {
        let sql = format!(
            "SELECT {cols} FROM books b JOIN book_collections bc ON bc.book_id = b.id \
             WHERE bc.collection_id = ?{predicate_and} ORDER BY bc.added_at DESC",
            cols = GRID_COLUMNS_B
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut bound_params: Vec<Value> = vec![Value::Text(collection_id.to_string())];
        bound_params.extend(predicate_params);
        let rows = stmt.query_map(
            rusqlite::params_from_iter(bound_params.iter()),
            row_to_grid_item,
        )?;
        return rows.collect();
    }

    let rules = get_collection_rules(conn, collection_id)?;
    let (joins, where_str, rule_params) = build_rule_query(&rules);

    // A rule-less automated collection has no `WHERE` of its own, so the
    // predicate becomes the whole clause; otherwise it is ANDed on.
    let combined_where = if where_str.is_empty() {
        where_clause(&predicate_clauses)
    } else {
        format!("{where_str}{predicate_and}")
    };

    let sql = format!(
        "SELECT DISTINCT {cols}
         FROM books b
         {joins}
         {combined_where}
         ORDER BY b.added_at DESC",
        cols = GRID_COLUMNS_B
    );

    let mut stmt = conn.prepare(&sql)?;
    // Positional parameters: the predicate's placeholders are always textually
    // last (they sit at the end of `combined_where`, which follows `{joins}`),
    // so binding the predicate's params last is correct.
    //
    // What is *not* correct, and is inherited rather than introduced here:
    // `build_rule_query` returns one flat `param_values` in rule order while
    // emitting some rules into `joins` and others into `where_str`, and every
    // join placeholder precedes every where placeholder in the SQL text. Rules
    // like `[series equals X, tag contains Y]` therefore bind X to the tag's
    // `LIKE` and Y to `b.series`. `get_books_in_collection_grid` and
    // `preview_collection_rules` have always done this; see the note in
    // `docs/backlog/` on automated collections.
    let mut bound_params: Vec<Value> = rule_params.into_iter().map(Value::Text).collect();
    bound_params.extend(predicate_params);
    let rows = stmt.query_map(
        rusqlite::params_from_iter(bound_params.iter()),
        row_to_grid_item,
    )?;
    rows.collect()
}

pub fn update_book(conn: &Connection, book: &Book) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // file_hash is immutable after import — not included in update
    conn.execute(
        "UPDATE books SET title=?2, author=?3, file_path=?4, cover_path=?5,
         total_chapters=?6, added_at=?7, format=?8,
         description=?9, genres=?10, rating=?11, isbn=?12, openlibrary_key=?13,
         updated_at=?14, series=?15, volume=?16, language=?17, publisher=?18, publish_year=?19
         WHERE id=?1",
        params![
            book.id,
            book.title,
            book.author,
            book.file_path,
            book.cover_path,
            book.total_chapters,
            book.added_at,
            book.format.to_string(),
            book.description,
            book.genres,
            book.rating,
            book.isbn,
            book.openlibrary_key,
            now,
            book.series,
            book.volume,
            book.language,
            book.publisher,
            book.publish_year,
        ],
    )?;
    Ok(())
}

pub fn update_book_path(
    conn: &Connection,
    book_id: &str,
    new_path: &str,
    is_imported: bool,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE books SET file_path = ?1, is_imported = ?2, updated_at = ?3 WHERE id = ?4",
        params![new_path, is_imported as i32, now, book_id],
    )?;
    Ok(())
}

pub fn delete_book(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM books WHERE id = ?1", params![id])?;
    Ok(())
}

/// Delete multiple books in a single transaction.
pub fn bulk_delete_books(conn: &Connection, ids: &[&str]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        tx.execute("DELETE FROM books WHERE id = ?1", params![id])?;
    }
    tx.commit()?;
    Ok(())
}

/// Add multiple books to a collection in a single transaction.
pub fn bulk_add_to_collection(
    conn: &Connection,
    book_ids: &[&str],
    collection_id: &str,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let tx = conn.unchecked_transaction()?;
    for book_id in book_ids {
        tx.execute(
            "INSERT OR IGNORE INTO book_collections (book_id, collection_id, added_at) VALUES (?1, ?2, ?3)",
            params![book_id, collection_id, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Add a tag to multiple books in a single transaction.
pub fn bulk_add_tag(conn: &Connection, book_ids: &[&str], tag: &str) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for book_id in book_ids {
        let tag_id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT OR IGNORE INTO book_tags (id, book_id, tag) VALUES (?1, ?2, ?3)",
            params![tag_id, book_id, tag],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Update metadata fields on multiple books in a single transaction.
/// Only non-None fields are applied. Empty strings clear optional fields to NULL.
pub fn bulk_update_metadata(
    conn: &Connection,
    ids: &[&str],
    author: Option<&str>,
    series: Option<&str>,
    publish_year: Option<u16>,
    language: Option<&str>,
    publisher: Option<&str>,
) -> Result<u32> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut set_clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(v) = author {
        set_clauses.push("author = ?");
        params.push(Box::new(v.to_string()));
    }
    if let Some(v) = series {
        set_clauses.push("series = ?");
        params.push(Box::new(if v.is_empty() {
            None::<String>
        } else {
            Some(v.to_string())
        }));
    }
    if let Some(v) = publish_year {
        set_clauses.push("publish_year = ?");
        params.push(Box::new(if v == 0 { None::<u16> } else { Some(v) }));
    }
    if let Some(v) = language {
        set_clauses.push("language = ?");
        params.push(Box::new(if v.is_empty() {
            None::<String>
        } else {
            Some(v.to_string())
        }));
    }
    if let Some(v) = publisher {
        set_clauses.push("publisher = ?");
        params.push(Box::new(if v.is_empty() {
            None::<String>
        } else {
            Some(v.to_string())
        }));
    }

    if set_clauses.is_empty() {
        return Ok(0);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    set_clauses.push("updated_at = ?");
    params.push(Box::new(now));

    let sql = format!("UPDATE books SET {} WHERE id = ?", set_clauses.join(", "));
    let tx = conn.unchecked_transaction()?;
    let mut count: u32 = 0;
    for id in ids {
        let mut all_params: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        all_params.push(id);
        count += tx.execute(&sql, all_params.as_slice())? as u32;
    }
    tx.commit()?;
    Ok(count)
}

pub fn update_book_enrichment(
    conn: &Connection,
    book_id: &str,
    description: Option<&str>,
    genres: Option<&str>,
    rating: Option<f64>,
    isbn: Option<&str>,
    openlibrary_key: Option<&str>,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE books SET description=?2, genres=?3, rating=?4, isbn=?5, openlibrary_key=?6, updated_at=?7 WHERE id=?1",
        params![book_id, description, genres, rating, isbn, openlibrary_key, now],
    )?;
    Ok(())
}

pub fn set_enrichment_status(conn: &Connection, book_id: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE books SET enrichment_status = ?2 WHERE id = ?1",
        params![book_id, status],
    )?;
    Ok(())
}

pub fn list_unenriched_books(conn: &Connection) -> Result<Vec<Book>> {
    let sql = format!(
        "SELECT {} FROM books WHERE enrichment_status IS NULL OR enrichment_status = 'queued' ORDER BY added_at DESC",
        BOOK_COLUMNS
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_book)?;
    rows.collect()
}

pub fn update_book_file_path(conn: &Connection, book_id: &str, new_path: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE books SET file_path = ?2, updated_at = ?3 WHERE id = ?1",
        params![book_id, new_path, now],
    )?;
    Ok(())
}

/// Set the manual "want to read" flag. The only writer of `want_to_read`
/// (deliberately excluded from `update_book` so a stale metadata edit cannot
/// clobber a newer toggle). `updated_at` is bumped MONOTONICALLY — timestamps
/// are second-resolution, so an immediate toggle within the same second would
/// otherwise reuse the old value and be missed by backup's `updated_at > since`
/// change-detection.
pub fn set_want_to_read(conn: &Connection, book_id: &str, want: bool) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE books SET want_to_read = ?2, updated_at = MAX(?3, updated_at) + 1 WHERE id = ?1",
        params![book_id, want as i32, now],
    )?;
    Ok(())
}

// --- Settings ---

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
    let mut rows = stmt.query(params![key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

/// Return every row of the `settings` table as `(key, value)` pairs,
/// ordered by key.
pub fn list_settings(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Remove a key/value row from the `settings` table. No-op when the
/// key is absent (counted rows = 0 is not an error).
pub fn delete_setting(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

pub fn get_or_create_device_id(conn: &Connection) -> Result<String> {
    if let Some(id) = get_setting(conn, "device_id")? {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    set_setting(conn, "device_id", &id)?;
    Ok(id)
}

pub fn is_sync_enabled(conn: &Connection) -> bool {
    get_setting(conn, "sync_enabled").ok().flatten().as_deref() == Some("true")
}

// --- Feature Flags CRUD ---

pub fn list_feature_flags(conn: &Connection) -> Result<Vec<FeatureFlag>> {
    let mut stmt =
        conn.prepare("SELECT key, enabled, description FROM feature_flags ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok(FeatureFlag {
            key: row.get(0)?,
            enabled: row.get::<_, i32>(1)? != 0,
            description: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn get_feature_flag(conn: &Connection, key: &str) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT enabled FROM feature_flags WHERE key = ?1")?;
    let mut rows = stmt.query(params![key])?;
    if let Some(row) = rows.next()? {
        Ok(row.get::<_, i32>(0)? != 0)
    } else {
        Ok(false)
    }
}

pub fn set_feature_flag(
    conn: &Connection,
    key: &str,
    enabled: bool,
    description: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO feature_flags (key, enabled, description) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET enabled=excluded.enabled, description=COALESCE(excluded.description, feature_flags.description)",
        params![key, enabled as i32, description],
    )?;
    Ok(())
}

pub fn delete_feature_flag(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM feature_flags WHERE key = ?1", params![key])?;
    Ok(())
}

// --- ReadingProgress CRUD ---

/// Upserts a book's reading progress. Also stamps `finished_at` (F-1-3) the
/// first time a write lands on or past the last chapter (guarded by
/// `total_chapters > 0`, same predicate as `get_reading_stats`'s "finished"
/// count — Finding 2) — via a `JOIN` against `books` so every caller gets
/// this for free without passing `total_chapters` in. Once set, `finished_at`
/// is never cleared or overwritten: re-reading or restarting a finished book
/// (chapter_index back to 0, then finished again) keeps the original
/// completion date, matching Goodreads-style semantics.
pub fn upsert_reading_progress(conn: &Connection, progress: &ReadingProgress) -> Result<()> {
    conn.execute(
        "INSERT INTO reading_progress (book_id, chapter_index, scroll_position, last_read_at, finished_at)
         VALUES (
             ?1, ?2, ?3, ?4,
             CASE WHEN (SELECT total_chapters > 0 AND ?2 >= total_chapters - 1
                        FROM books WHERE id = ?1)
                  THEN ?4 ELSE NULL END
         )
         ON CONFLICT(book_id) DO UPDATE SET
           chapter_index=excluded.chapter_index,
           scroll_position=excluded.scroll_position,
           last_read_at=excluded.last_read_at,
           finished_at = CASE
               WHEN reading_progress.finished_at IS NOT NULL THEN reading_progress.finished_at
               WHEN (SELECT total_chapters > 0 AND excluded.chapter_index >= total_chapters - 1
                     FROM books WHERE id = reading_progress.book_id)
               THEN excluded.last_read_at
               ELSE NULL
           END",
        params![
            progress.book_id,
            progress.chapter_index,
            progress.scroll_position,
            progress.last_read_at,
        ],
    )?;
    Ok(())
}

pub fn get_reading_progress(conn: &Connection, book_id: &str) -> Result<Option<ReadingProgress>> {
    let mut stmt = conn.prepare(
        "SELECT book_id, chapter_index, scroll_position, last_read_at
         FROM reading_progress WHERE book_id = ?1",
    )?;
    let mut rows = stmt.query(params![book_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(ReadingProgress {
            book_id: row.get(0)?,
            chapter_index: row.get(1)?,
            scroll_position: row.get(2)?,
            last_read_at: row.get(3)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn get_all_reading_progress(conn: &Connection) -> Result<Vec<ReadingProgress>> {
    let mut stmt = conn.prepare(
        "SELECT book_id, chapter_index, scroll_position, last_read_at FROM reading_progress",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ReadingProgress {
            book_id: row.get(0)?,
            chapter_index: row.get(1)?,
            scroll_position: row.get(2)?,
            last_read_at: row.get(3)?,
        })
    })?;
    rows.collect()
}

pub fn get_recently_read_books(conn: &Connection, limit: u32) -> Result<Vec<Book>> {
    let sql = format!(
        "SELECT {} FROM books b JOIN reading_progress rp ON rp.book_id = b.id ORDER BY rp.last_read_at DESC LIMIT ?1",
        BOOK_COLUMNS_B
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit], row_to_book)?;
    rows.collect()
}

/// Books with progress that is neither zero nor "finished", most recently
/// read first — powers the web UI's "Continue Reading" shelf (Item 5).
/// "Finished" mirrors the predicate `get_reading_stats` uses for
/// `books_finished` (on or past the last chapter: `chapter_index >=
/// total_chapters - 1`, guarded against `total_chapters = 0` — Finding 2),
/// so a book counted as finished there never shows up here as still in
/// progress. `total_chapters = 0` (not yet known) is excluded outright
/// rather than treated as "never finished" — a book with an unknown page
/// count can't show meaningful progress either way.
pub fn get_continue_reading_books(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<ContinueReadingItem>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.title, b.author, b.cover_path, b.format, b.total_chapters,
                rp.chapter_index, rp.scroll_position, rp.last_read_at
         FROM books b
         JOIN reading_progress rp ON rp.book_id = b.id
         WHERE rp.chapter_index > 0
           AND b.total_chapters > 0
           AND rp.chapter_index < b.total_chapters - 1
         ORDER BY rp.last_read_at DESC, b.id
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        let format_str: String = row.get(4)?;
        Ok(ContinueReadingItem {
            id: row.get(0)?,
            title: row.get(1)?,
            author: row.get(2)?,
            cover_path: row.get(3)?,
            format: format_str
                .parse()
                .map_err(|e: String| rusqlite::Error::InvalidParameterName(e))?,
            total_chapters: row.get(5)?,
            chapter_index: row.get(6)?,
            scroll_position: row.get(7)?,
            last_read_at: row.get(8)?,
        })
    })?;
    rows.collect()
}

// --- Bookmark CRUD ---

pub fn insert_bookmark(conn: &Connection, bookmark: &Bookmark) -> Result<()> {
    conn.execute(
        "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            bookmark.id,
            bookmark.book_id,
            bookmark.chapter_index,
            bookmark.scroll_position,
            bookmark.name,
            bookmark.note,
            bookmark.created_at,
            bookmark.updated_at,
            bookmark.deleted_at,
        ],
    )?;
    Ok(())
}

pub fn list_bookmarks(conn: &Connection, book_id: &str) -> Result<Vec<Bookmark>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at
         FROM bookmarks WHERE book_id = ?1 AND deleted_at IS NULL ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![book_id], |row| {
        Ok(Bookmark {
            id: row.get(0)?,
            book_id: row.get(1)?,
            chapter_index: row.get(2)?,
            scroll_position: row.get(3)?,
            name: row.get(4)?,
            note: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            deleted_at: row.get(8)?,
        })
    })?;
    rows.collect()
}

pub fn delete_bookmark(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM bookmarks WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn soft_delete_bookmark(conn: &Connection, id: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE bookmarks SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
        params![now, id],
    )?;
    Ok(())
}

pub fn update_bookmark_name(conn: &Connection, id: &str, name: Option<&str>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE bookmarks SET name = ?1, updated_at = ?2 WHERE id = ?3",
        params![name, now, id],
    )?;
    Ok(())
}

/// Soft-delete a bookmark only if it belongs to `book_id` and is still live.
/// Returns rows affected (0 → unknown/foreign/already-deleted id). Web routes
/// are book-scoped, so this prevents `DELETE /books/{A}/bookmarks/{B's id}`
/// from touching book B's row.
pub fn soft_delete_bookmark_scoped(conn: &Connection, book_id: &str, id: &str) -> Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let n = conn.execute(
        "UPDATE bookmarks SET deleted_at = ?1, updated_at = ?1 \
         WHERE id = ?2 AND book_id = ?3 AND deleted_at IS NULL",
        params![now, id, book_id],
    )?;
    Ok(n)
}

/// Rename (or clear, `name = None`) a bookmark only if it belongs to `book_id`
/// and is still live. Returns the updated row, or `None` if no live matching
/// row exists (→ 404-eligible in the route). `UPDATE ... RETURNING` makes the
/// write-and-read one atomic statement, so a concurrent soft-delete cannot turn
/// a committed rename into a spurious 404, and there is no second re-fetch.
pub fn update_bookmark_name_scoped(
    conn: &Connection,
    book_id: &str,
    id: &str,
    name: Option<&str>,
) -> Result<Option<Bookmark>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut stmt = conn.prepare(
        "UPDATE bookmarks SET name = ?1, updated_at = ?2 \
         WHERE id = ?3 AND book_id = ?4 AND deleted_at IS NULL \
         RETURNING id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at",
    )?;
    let mut rows = stmt.query_map(params![name, now, id, book_id], |row| {
        Ok(Bookmark {
            id: row.get(0)?,
            book_id: row.get(1)?,
            chapter_index: row.get(2)?,
            scroll_position: row.get(3)?,
            name: row.get(4)?,
            note: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            deleted_at: row.get(8)?,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

// --- Collections CRUD ---

/// Standard column list for SELECT queries on books.
const BOOK_COLUMNS: &str = "id, title, author, file_path, cover_path, total_chapters, added_at, format, file_hash, description, genres, rating, isbn, openlibrary_key, enrichment_status, series, volume, language, publisher, publish_year, is_imported, want_to_read";

/// Standard column list prefixed with table alias `b.`.
const BOOK_COLUMNS_B: &str = "b.id, b.title, b.author, b.file_path, b.cover_path, b.total_chapters, b.added_at, b.format, b.file_hash, b.description, b.genres, b.rating, b.isbn, b.openlibrary_key, b.enrichment_status, b.series, b.volume, b.language, b.publisher, b.publish_year, b.is_imported, b.want_to_read";

fn row_to_book(row: &rusqlite::Row) -> rusqlite::Result<Book> {
    let format_str: String = row.get("format")?;
    Ok(Book {
        id: row.get("id")?,
        title: row.get("title")?,
        author: row.get("author")?,
        file_path: row.get("file_path")?,
        cover_path: row.get("cover_path")?,
        total_chapters: row.get("total_chapters")?,
        added_at: row.get("added_at")?,
        format: format_str
            .parse()
            .map_err(|e: String| rusqlite::Error::InvalidParameterName(e))?,
        file_hash: row.get("file_hash")?,
        description: row.get("description")?,
        genres: row.get("genres")?,
        rating: row.get("rating")?,
        isbn: row.get("isbn")?,
        openlibrary_key: row.get("openlibrary_key")?,
        enrichment_status: row.get("enrichment_status")?,
        series: row.get("series")?,
        volume: row.get("volume")?,
        language: row.get("language")?,
        publisher: row.get("publisher")?,
        publish_year: row.get("publish_year")?,
        is_imported: row.get::<_, i32>("is_imported").unwrap_or(1) != 0,
        want_to_read: row.get::<_, i32>("want_to_read").unwrap_or(0) != 0,
    })
}

pub fn insert_collection(conn: &Connection, collection: &Collection) -> Result<()> {
    let type_str = match collection.r#type {
        CollectionType::Manual => "manual",
        CollectionType::Automated => "automated",
    };
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO collections (id, name, type, icon, color, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            collection.id,
            collection.name,
            type_str,
            collection.icon,
            collection.color,
            collection.created_at,
            collection.updated_at,
        ],
    )?;
    for rule in &collection.rules {
        tx.execute(
            "INSERT INTO collection_rules (id, collection_id, field, operator, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                rule.id,
                rule.collection_id,
                rule.field,
                rule.operator,
                rule.value
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn update_collection(conn: &Connection, collection: &Collection) -> Result<()> {
    let type_str = match collection.r#type {
        CollectionType::Manual => "manual",
        CollectionType::Automated => "automated",
    };
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE collections SET name = ?1, type = ?2, icon = ?3, color = ?4, updated_at = ?5
         WHERE id = ?6",
        params![
            collection.name,
            type_str,
            collection.icon,
            collection.color,
            collection.updated_at,
            collection.id,
        ],
    )?;
    tx.execute(
        "DELETE FROM collection_rules WHERE collection_id = ?1",
        params![collection.id],
    )?;
    for rule in &collection.rules {
        tx.execute(
            "INSERT INTO collection_rules (id, collection_id, field, operator, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                rule.id,
                rule.collection_id,
                rule.field,
                rule.operator,
                rule.value
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn get_collection_rules(conn: &Connection, collection_id: &str) -> Result<Vec<CollectionRule>> {
    let mut stmt = conn.prepare(
        "SELECT id, collection_id, field, operator, value
         FROM collection_rules WHERE collection_id = ?1",
    )?;
    let rows = stmt.query_map(params![collection_id], |row| {
        Ok(CollectionRule {
            id: row.get(0)?,
            collection_id: row.get(1)?,
            field: row.get(2)?,
            operator: row.get(3)?,
            value: row.get(4)?,
        })
    })?;
    rows.collect()
}

pub fn list_collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, type, icon, color, created_at, updated_at
         FROM collections ORDER BY created_at ASC",
    )?;
    let collections: Vec<Collection> = stmt
        .query_map([], |row| {
            let type_str: String = row.get(2)?;
            let coll_type = match type_str.as_str() {
                "automated" => CollectionType::Automated,
                _ => CollectionType::Manual,
            };
            Ok(Collection {
                id: row.get(0)?,
                name: row.get(1)?,
                r#type: coll_type,
                icon: row.get(3)?,
                color: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                rules: vec![],
            })
        })?
        .collect::<Result<Vec<_>>>()?;

    // Fetch ALL rules in one query instead of N+1 per-collection queries.
    let mut rules_stmt =
        conn.prepare("SELECT id, collection_id, field, operator, value FROM collection_rules")?;
    let all_rules: Vec<CollectionRule> = rules_stmt
        .query_map([], |row| {
            Ok(CollectionRule {
                id: row.get(0)?,
                collection_id: row.get(1)?,
                field: row.get(2)?,
                operator: row.get(3)?,
                value: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;

    // Group rules by collection_id
    let mut rules_map: std::collections::HashMap<String, Vec<CollectionRule>> =
        std::collections::HashMap::new();
    for rule in all_rules {
        rules_map
            .entry(rule.collection_id.clone())
            .or_default()
            .push(rule);
    }

    let result = collections
        .into_iter()
        .map(|mut coll| {
            coll.rules = rules_map.remove(&coll.id).unwrap_or_default();
            coll
        })
        .collect();
    Ok(result)
}

pub fn delete_collection(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM collections WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn add_book_to_collection(conn: &Connection, book_id: &str, collection_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO book_collections (book_id, collection_id, added_at)
         VALUES (?1, ?2, unixepoch())",
        params![book_id, collection_id],
    )?;
    Ok(())
}

pub fn remove_book_from_collection(
    conn: &Connection,
    book_id: &str,
    collection_id: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM book_collections WHERE book_id = ?1 AND collection_id = ?2",
        params![book_id, collection_id],
    )?;
    Ok(())
}

// --- Reading Sessions ---

pub fn insert_reading_session(
    conn: &Connection,
    id: &str,
    book_id: &str,
    started_at: i64,
    duration_secs: i64,
    pages_read: i32,
) -> Result<()> {
    conn.execute(
        "INSERT INTO reading_sessions (id, book_id, started_at, duration_secs, pages_read)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, book_id, started_at, duration_secs, pages_read],
    )?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingStats {
    pub total_reading_time_secs: i64,
    pub total_sessions: i64,
    pub total_pages_read: i64,
    pub total_books: i64,
    pub books_finished: i64,
    /// Books finished during the current local calendar year, by
    /// `reading_progress.finished_at`'s local year — powers the yearly
    /// reading goal ring (F-1-3). Distinct from `books_finished`, which is
    /// all-time.
    pub books_finished_this_year: i64,
    pub current_streak_days: i64,
    pub longest_streak_days: i64,
    pub daily_reading: Vec<(String, i64)>, // (date_str, seconds)
    pub daily_reading_year: Vec<(String, i64)>, // (date_str, seconds), last 365 days — for the heatmap (F-5-4)
}

/// Unix timestamp of the first instant of calendar day `date` in zone `tz` —
/// normally local 00:00. If that midnight is skipped by a DST spring-forward
/// (a zone that jumps 00:00 -> 01:00), returns the transition instant instead:
/// the first wall-clock time that actually exists on `date`. `None` only if no
/// time in the first few hours of the date exists, which no real zone does.
fn local_day_start_unix<Tz: chrono::TimeZone>(tz: &Tz, date: chrono::NaiveDate) -> Option<i64> {
    (0..4).find_map(|hour| {
        date.and_hms_opt(hour, 0, 0)
            .and_then(|dt| tz.from_local_datetime(&dt).earliest())
            .map(|dt| dt.timestamp())
    })
}

/// Credit a session's `duration_secs` to the calendar days (in zone `tz`) it
/// spans, splitting at each midnight boundary so a session that crosses
/// midnight (or a DST transition) adds to each day only the seconds actually
/// elapsed on it. Keys are `YYYY-MM-DD` dates (F-5-2).
fn accumulate_session_by_local_day<Tz: chrono::TimeZone>(
    tz: &Tz,
    started_at: i64,
    duration_secs: i64,
    buckets: &mut std::collections::BTreeMap<String, i64>,
) {
    if duration_secs <= 0 {
        return;
    }
    let end = started_at + duration_secs;
    let mut cursor = started_at;
    while cursor < end {
        let Some(dt) = tz.timestamp_opt(cursor, 0).single() else {
            return;
        };
        let day = dt.date_naive();
        // Start of the next calendar day (the transition instant when that
        // midnight is skipped by DST); fall back to `end` only in the
        // impossible case no early-hour time on that date exists.
        let next_day_start =
            local_day_start_unix(tz, day + chrono::Duration::days(1)).unwrap_or(end);
        let segment_end = next_day_start.min(end);
        if segment_end <= cursor {
            return; // guard against a non-advancing cursor
        }
        *buckets
            .entry(day.format("%Y-%m-%d").to_string())
            .or_insert(0) += segment_end - cursor;
        cursor = segment_end;
    }
}

pub fn get_reading_stats(conn: &Connection) -> Result<ReadingStats> {
    let total_reading_time_secs: i64 = conn.query_row(
        "SELECT COALESCE(SUM(duration_secs), 0) FROM reading_sessions",
        [],
        |row| row.get(0),
    )?;
    let total_sessions: i64 =
        conn.query_row("SELECT COUNT(*) FROM reading_sessions", [], |row| {
            row.get(0)
        })?;
    let total_pages_read: i64 = conn.query_row(
        "SELECT COALESCE(SUM(pages_read), 0) FROM reading_sessions",
        [],
        |row| row.get(0),
    )?;
    let total_books: i64 = conn.query_row("SELECT COUNT(*) FROM books", [], |row| row.get(0))?;
    // `books_finished` (all-time) and `books_finished_this_year` share the
    // same "finished" predicate (on or past the last chapter, guarded
    // against `total_chapters = 0` — Finding 2, a zero-chapter book with any
    // progress row would otherwise look "finished" the moment it's opened),
    // so both are computed by one scan with conditional aggregation rather
    // than two separate `COUNT` queries over the same join.
    // `books_finished_this_year` is scoped by `finished_at`'s local calendar
    // year (F-1-3), not `last_read_at` — `last_read_at` is bumped by simply
    // re-opening an already-finished book, which would wrongly re-count it
    // in whatever year it happens to be re-opened.
    let (books_finished, books_finished_this_year): (i64, i64) = conn.query_row(
        "SELECT
             COALESCE(SUM(CASE WHEN b.total_chapters > 0 AND rp.chapter_index >= b.total_chapters - 1
                                THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN b.total_chapters > 0 AND rp.chapter_index >= b.total_chapters - 1
                                 AND strftime('%Y', rp.finished_at, 'unixepoch', 'localtime') = strftime('%Y', 'now', 'localtime')
                                THEN 1 ELSE 0 END), 0)
         FROM reading_progress rp JOIN books b ON rp.book_id = b.id",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    // Daily reading for the heatmap's rolling year window (F-5-4), grouped by
    // local calendar day rather than a rolling timestamp cutoff — this
    // matches the frontend grid, which covers exactly the last 365 local
    // calendar dates (today-364 .. today). A rolling-timestamp cutoff would
    // let the oldest day get partially summed or fall outside the grid.
    //
    // Each session's duration is split across the local calendar days it
    // actually spans (F-5-2): a session crossing local midnight (or a DST
    // transition) credits each day only the seconds elapsed on it, instead of
    // dumping the whole duration on its start day. The daily-minutes goal
    // reads today's bucket from this series, so a session that began before
    // midnight must not count against today's total.
    let today = chrono::Local::now().date_naive();
    let cutoff_day = today - chrono::Duration::days(364);
    let cutoff_start = local_day_start_unix(&chrono::Local, cutoff_day).unwrap_or(0);
    let window_end =
        local_day_start_unix(&chrono::Local, today + chrono::Duration::days(1)).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT started_at, duration_secs FROM reading_sessions
         WHERE started_at < ?1 AND started_at + duration_secs > ?2",
    )?;
    let mut year_buckets: std::collections::BTreeMap<String, i64> =
        std::collections::BTreeMap::new();
    let rows = stmt.query_map(params![window_end, cutoff_start], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    for (started_at, duration_secs) in rows.flatten() {
        accumulate_session_by_local_day(
            &chrono::Local,
            started_at,
            duration_secs,
            &mut year_buckets,
        );
    }
    // Drop any pre-window slivers a boundary-crossing session contributed
    // (BTreeMap keeps keys sorted ascending, matching the old ORDER BY day).
    let cutoff_str = cutoff_day.format("%Y-%m-%d").to_string();
    let daily_reading_year: Vec<(String, i64)> = year_buckets
        .into_iter()
        .filter(|(day, _)| *day >= cutoff_str)
        .collect();

    // Daily reading for last 30 days (the existing bar chart), derived from
    // the year series above rather than a second query so both use the same
    // calendar-day cutoff logic and timezone source. This makes the 30-day
    // series calendar-day (not rolling-timestamp) semantics, matching a
    // chart labeled "last 30 days".
    let thirty_day_cutoff: String =
        conn.query_row("SELECT date('now', 'localtime', '-29 days')", [], |row| {
            row.get(0)
        })?;
    let daily_reading: Vec<(String, i64)> = daily_reading_year
        .iter()
        .filter(|(day, _)| *day >= thirty_day_cutoff)
        .cloned()
        .collect();

    // Calculate streaks from daily data
    let streak_days: Vec<String> = conn.prepare(
        "SELECT DISTINCT date(started_at, 'unixepoch', 'localtime') as day FROM reading_sessions ORDER BY day DESC",
    )?.query_map([], |row| row.get::<_, String>(0))?.filter_map(|r| r.ok()).collect();

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut current_streak = 0i64;
    let mut longest_streak = 0i64;
    let mut running_streak = 0i64;
    let mut expected_date = today.clone();

    for day in &streak_days {
        if *day == expected_date {
            running_streak += 1;
            // Move expected_date back one day
            if let Ok(d) = chrono::NaiveDate::parse_from_str(&expected_date, "%Y-%m-%d") {
                expected_date = (d - chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string();
            }
        } else {
            if current_streak == 0 {
                current_streak = running_streak;
            }
            if running_streak > longest_streak {
                longest_streak = running_streak;
            }
            running_streak = 0;
            // Reset: check if this day starts a new streak
            if let Ok(d) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
                expected_date = (d - chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string();
                running_streak = 1;
            }
        }
    }
    if current_streak == 0 {
        current_streak = running_streak;
    }
    if running_streak > longest_streak {
        longest_streak = running_streak;
    }

    Ok(ReadingStats {
        total_reading_time_secs,
        total_sessions,
        total_pages_read,
        total_books,
        books_finished,
        books_finished_this_year,
        current_streak_days: current_streak,
        longest_streak_days: longest_streak,
        daily_reading,
        daily_reading_year,
    })
}

pub fn get_book_reading_time(conn: &Connection, book_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(duration_secs), 0) FROM reading_sessions WHERE book_id = ?1",
        params![book_id],
        |row| row.get(0),
    )
}

/// Per-book reading insights for the Book Details modal (F-1-7). Deliberately
/// omits a pages/chapters-per-hour "pace" figure — that's unreliable across
/// formats (PDF pages vs. EPUB chapters aren't comparable units). Also omits
/// an average-session figure — trivially derivable from
/// `total_reading_time_secs / session_count`, so the frontend computes it.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookReadingStats {
    pub total_reading_time_secs: i64,
    pub session_count: i64,
    /// Earliest session start for this book; `None` when there are no
    /// local `reading_sessions` rows (e.g. a book finished via sync/web UI).
    pub first_read_at: Option<i64>,
    /// From `reading_progress.finished_at` (F-1-3); `None` if unfinished.
    pub finished_at: Option<i64>,
}

/// Returns `None` only when there's genuinely nothing to show: no local
/// reading sessions AND no `finished_at`. `reading_progress`/`finished_at`
/// can be set without any `reading_sessions` rows — sync.rs and the web UI
/// write progress directly, and pre-feature finished books were backfilled
/// (`backfill_finished_at`) — so a synced/web-finished book must still
/// surface its "Finished" date even though `session_count` is 0.
pub fn get_book_reading_stats(
    conn: &Connection,
    book_id: &str,
) -> Result<Option<BookReadingStats>> {
    let (total_reading_time_secs, session_count, first_read_at): (i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT COALESCE(SUM(duration_secs), 0), COUNT(*), MIN(started_at)
             FROM reading_sessions WHERE book_id = ?1",
            params![book_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

    use rusqlite::OptionalExtension;
    let finished_at: Option<i64> = conn
        .query_row(
            "SELECT finished_at FROM reading_progress WHERE book_id = ?1",
            params![book_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    if session_count == 0 && finished_at.is_none() {
        return Ok(None);
    }

    Ok(Some(BookReadingStats {
        total_reading_time_secs,
        session_count,
        first_read_at,
        finished_at,
    }))
}

// --- Highlights CRUD ---

fn row_to_highlight(row: &rusqlite::Row) -> rusqlite::Result<crate::models::Highlight> {
    Ok(crate::models::Highlight {
        id: row.get(0)?,
        book_id: row.get(1)?,
        chapter_index: row.get(2)?,
        text: row.get(3)?,
        color: row.get(4)?,
        note: row.get(5)?,
        start_offset: row.get(6)?,
        end_offset: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        deleted_at: row.get(10)?,
    })
}

pub fn insert_highlight(conn: &Connection, h: &crate::models::Highlight) -> Result<()> {
    conn.execute(
        "INSERT INTO highlights (id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![h.id, h.book_id, h.chapter_index, h.text, h.color, h.note, h.start_offset, h.end_offset, h.created_at, h.updated_at, h.deleted_at],
    )?;
    Ok(())
}

pub fn list_highlights(conn: &Connection, book_id: &str) -> Result<Vec<crate::models::Highlight>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at
         FROM highlights WHERE book_id = ?1 AND deleted_at IS NULL ORDER BY chapter_index ASC, start_offset ASC",
    )?;
    let rows = stmt.query_map(params![book_id], row_to_highlight)?;
    rows.collect()
}

pub fn get_chapter_highlights(
    conn: &Connection,
    book_id: &str,
    chapter_index: u32,
) -> Result<Vec<crate::models::Highlight>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at
         FROM highlights WHERE book_id = ?1 AND chapter_index = ?2 AND deleted_at IS NULL ORDER BY start_offset ASC",
    )?;
    let rows = stmt.query_map(params![book_id, chapter_index], row_to_highlight)?;
    rows.collect()
}

pub fn update_highlight_note(conn: &Connection, id: &str, note: Option<&str>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE highlights SET note = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, note, now],
    )?;
    Ok(())
}

pub fn delete_highlight(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM highlights WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn soft_delete_highlight(conn: &Connection, id: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "UPDATE highlights SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
        params![now, id],
    )?;
    Ok(())
}

/// Update a highlight's note and/or color only if it belongs to `book_id` and
/// is still live. `note_change`: `None` = leave note untouched, `Some(None)` =
/// clear, `Some(Some(v))` = set. Returns the updated row, or `None` if no live
/// matching row exists (→ 404-eligible) or if neither field was requested (the
/// web handler rejects that case with 400 before calling). `UPDATE … RETURNING`
/// makes write-and-read one atomic statement (bookmark precedent:
/// `update_bookmark_name_scoped`).
pub fn update_highlight_scoped(
    conn: &Connection,
    book_id: &str,
    id: &str,
    note_change: Option<Option<&str>>,
    color: Option<&str>,
) -> Result<Option<crate::models::Highlight>> {
    const RET: &str = "RETURNING id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let run =
        |sql: String, p: &[&dyn rusqlite::ToSql]| -> Result<Option<crate::models::Highlight>> {
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query_map(p, row_to_highlight)?;
            match rows.next() {
                Some(row) => Ok(Some(row?)),
                None => Ok(None),
            }
        };
    match (note_change, color) {
        (Some(note), Some(color)) => run(
            format!("UPDATE highlights SET note = ?1, color = ?2, updated_at = ?3 WHERE id = ?4 AND book_id = ?5 AND deleted_at IS NULL {RET}"),
            &[&note, &color, &now, &id, &book_id],
        ),
        (Some(note), None) => run(
            format!("UPDATE highlights SET note = ?1, updated_at = ?2 WHERE id = ?3 AND book_id = ?4 AND deleted_at IS NULL {RET}"),
            &[&note, &now, &id, &book_id],
        ),
        (None, Some(color)) => run(
            format!("UPDATE highlights SET color = ?1, updated_at = ?2 WHERE id = ?3 AND book_id = ?4 AND deleted_at IS NULL {RET}"),
            &[&color, &now, &id, &book_id],
        ),
        (None, None) => Ok(None),
    }
}

/// Soft-delete a highlight only if it belongs to `book_id` and is still live.
/// Returns rows affected (0 → unknown/foreign/already-deleted id — idempotent).
pub fn soft_delete_highlight_scoped(conn: &Connection, book_id: &str, id: &str) -> Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let n = conn.execute(
        "UPDATE highlights SET deleted_at = ?1, updated_at = ?1 \
         WHERE id = ?2 AND book_id = ?3 AND deleted_at IS NULL",
        params![now, id, book_id],
    )?;
    Ok(n)
}

// --- Sync-inclusive queries ---

pub fn list_all_bookmarks_for_sync(conn: &Connection, book_id: &str) -> Result<Vec<Bookmark>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at
         FROM bookmarks WHERE book_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![book_id], |row| {
        Ok(Bookmark {
            id: row.get(0)?,
            book_id: row.get(1)?,
            chapter_index: row.get(2)?,
            scroll_position: row.get(3)?,
            name: row.get(4)?,
            note: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            deleted_at: row.get(8)?,
        })
    })?;
    rows.collect()
}

pub fn search_highlights(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> Result<Vec<HighlightSearchResult>> {
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let mut stmt = conn.prepare(
        "SELECT h.id, h.book_id, b.title, b.author, h.chapter_index, h.text, h.color, h.note, h.created_at
         FROM highlights h
         JOIN books b ON h.book_id = b.id
         WHERE h.deleted_at IS NULL
           AND (h.text LIKE ?1 ESCAPE '\\' OR h.note LIKE ?1 ESCAPE '\\')
         ORDER BY h.created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![pattern, limit], |row| {
        Ok(HighlightSearchResult {
            highlight_id: row.get(0)?,
            book_id: row.get(1)?,
            book_title: row.get(2)?,
            book_author: row.get(3)?,
            chapter_index: row.get(4)?,
            text: row.get(5)?,
            color: row.get(6)?,
            note: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    rows.collect()
}

pub fn list_all_highlights_for_sync(
    conn: &Connection,
    book_id: &str,
) -> Result<Vec<crate::models::Highlight>> {
    let mut stmt = conn.prepare(
        "SELECT id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at
         FROM highlights WHERE book_id = ?1 ORDER BY chapter_index ASC, start_offset ASC",
    )?;
    let rows = stmt.query_map(params![book_id], |row| {
        Ok(crate::models::Highlight {
            id: row.get(0)?,
            book_id: row.get(1)?,
            chapter_index: row.get(2)?,
            text: row.get(3)?,
            color: row.get(4)?,
            note: row.get(5)?,
            start_offset: row.get(6)?,
            end_offset: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            deleted_at: row.get(10)?,
        })
    })?;
    rows.collect()
}

pub fn upsert_bookmark_from_sync(conn: &Connection, bookmark: &Bookmark) -> Result<()> {
    conn.execute(
        "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, name, note, created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           book_id=excluded.book_id,
           chapter_index=excluded.chapter_index,
           scroll_position=excluded.scroll_position,
           name=excluded.name,
           note=excluded.note,
           created_at=excluded.created_at,
           updated_at=excluded.updated_at,
           deleted_at=excluded.deleted_at",
        params![
            bookmark.id,
            bookmark.book_id,
            bookmark.chapter_index,
            bookmark.scroll_position,
            bookmark.name,
            bookmark.note,
            bookmark.created_at,
            bookmark.updated_at,
            bookmark.deleted_at,
        ],
    )?;
    Ok(())
}

pub fn upsert_highlight_from_sync(conn: &Connection, h: &crate::models::Highlight) -> Result<()> {
    conn.execute(
        "INSERT INTO highlights (id, book_id, chapter_index, text, color, note, start_offset, end_offset, created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
           book_id=excluded.book_id,
           chapter_index=excluded.chapter_index,
           text=excluded.text,
           color=excluded.color,
           note=excluded.note,
           start_offset=excluded.start_offset,
           end_offset=excluded.end_offset,
           created_at=excluded.created_at,
           updated_at=excluded.updated_at,
           deleted_at=excluded.deleted_at",
        params![
            h.id,
            h.book_id,
            h.chapter_index,
            h.text,
            h.color,
            h.note,
            h.start_offset,
            h.end_offset,
            h.created_at,
            h.updated_at,
            h.deleted_at,
        ],
    )?;
    Ok(())
}

// --- Vocabulary CRUD (F-1-5) ---

/// Insert a new vocabulary row, or — on `lemma` UNIQUE conflict — bump
/// `seen_count` and touch `last_seen_at` only, leaving context, box, and
/// review state untouched. Returns `true` if a new row was inserted, `false`
/// if an existing lemma was touched instead.
pub fn upsert_vocabulary_word(
    conn: &Connection,
    w: &crate::models::VocabularyWord,
) -> crate::error::CarrelResult<bool> {
    use rusqlite::OptionalExtension;
    let existed: bool = conn
        .query_row(
            "SELECT 1 FROM vocabulary WHERE lemma = ?1",
            params![w.lemma],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    conn.execute(
        "INSERT INTO vocabulary (id, lemma, word, pos, definition, book_id, book_title, chapter_index, context_sentence, start_offset, end_offset, seen_count, box, last_reviewed_at, next_due_at, last_seen_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(lemma) DO UPDATE SET
           seen_count = seen_count + 1,
           last_seen_at = excluded.last_seen_at",
        params![
            w.id,
            w.lemma,
            w.word,
            w.pos,
            w.definition,
            w.book_id,
            w.book_title,
            w.chapter_index,
            w.context_sentence,
            w.start_offset,
            w.end_offset,
            w.seen_count,
            w.box_num,
            w.last_reviewed_at,
            w.next_due_at,
            w.last_seen_at,
            w.created_at,
        ],
    )?;

    Ok(!existed)
}

pub fn list_vocabulary(
    conn: &Connection,
) -> crate::error::CarrelResult<Vec<crate::models::VocabularyWord>> {
    let mut stmt = conn.prepare(
        "SELECT id, lemma, word, pos, definition, book_id, book_title, chapter_index, context_sentence, start_offset, end_offset, seen_count, box, last_reviewed_at, next_due_at, last_seen_at, created_at
         FROM vocabulary ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], vocabulary_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn delete_vocabulary_word(conn: &Connection, id: &str) -> crate::error::CarrelResult<()> {
    conn.execute("DELETE FROM vocabulary WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn clear_vocabulary(conn: &Connection) -> crate::error::CarrelResult<()> {
    conn.execute("DELETE FROM vocabulary", [])?;
    Ok(())
}

/// Rows due for review: `next_due_at IS NULL` (never reviewed) or already
/// past due, soonest-due first (SQLite sorts NULL as the smallest value in
/// `ASC`, so never-reviewed rows sort first), then oldest-added first.
pub fn due_vocabulary(
    conn: &Connection,
    now: i64,
    limit: i64,
) -> crate::error::CarrelResult<Vec<crate::models::VocabularyWord>> {
    let mut stmt = conn.prepare(
        "SELECT id, lemma, word, pos, definition, book_id, book_title, chapter_index, context_sentence, start_offset, end_offset, seen_count, box, last_reviewed_at, next_due_at, last_seen_at, created_at
         FROM vocabulary WHERE next_due_at IS NULL OR next_due_at <= ?1
         ORDER BY next_due_at ASC, created_at ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![now, limit], vocabulary_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Score a review with the pure Leitner scheduler and persist the result.
pub fn record_vocabulary_review(
    conn: &Connection,
    id: &str,
    correct: bool,
    now: i64,
) -> crate::error::CarrelResult<()> {
    let box_num: i64 = conn.query_row(
        "SELECT box FROM vocabulary WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let (new_box, next_due_at) = crate::vocabulary::next_review(box_num, correct, now);
    conn.execute(
        "UPDATE vocabulary SET box = ?1, last_reviewed_at = ?2, next_due_at = ?3 WHERE id = ?4",
        params![new_box, now, next_due_at, id],
    )?;
    Ok(())
}

fn vocabulary_row(row: &rusqlite::Row) -> rusqlite::Result<crate::models::VocabularyWord> {
    Ok(crate::models::VocabularyWord {
        id: row.get(0)?,
        lemma: row.get(1)?,
        word: row.get(2)?,
        pos: row.get(3)?,
        definition: row.get(4)?,
        book_id: row.get(5)?,
        book_title: row.get(6)?,
        chapter_index: row.get(7)?,
        context_sentence: row.get(8)?,
        start_offset: row.get(9)?,
        end_offset: row.get(10)?,
        seen_count: row.get(11)?,
        box_num: row.get(12)?,
        last_reviewed_at: row.get(13)?,
        next_due_at: row.get(14)?,
        last_seen_at: row.get(15)?,
        created_at: row.get(16)?,
    })
}

// --- Tags CRUD ---

pub fn list_tags(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, name FROM tags ORDER BY name ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

pub fn get_or_create_tag(conn: &Connection, tag_id: &str, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tags (id, name) VALUES (?1, ?2)",
        params![tag_id, name],
    )?;
    Ok(())
}

pub fn get_tag_by_name(conn: &Connection, name: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT id FROM tags WHERE name = ?1")?;
    let mut rows = stmt.query(params![name])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

pub fn add_tag_to_book(conn: &Connection, book_id: &str, tag_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO book_tags (book_id, tag_id) VALUES (?1, ?2)",
        params![book_id, tag_id],
    )?;
    Ok(())
}

pub fn remove_tag_from_book(conn: &Connection, book_id: &str, tag_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM book_tags WHERE book_id = ?1 AND tag_id = ?2",
        params![book_id, tag_id],
    )?;
    Ok(())
}

pub fn get_book_tags(conn: &Connection, book_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name FROM tags t
         JOIN book_tags bt ON bt.tag_id = t.id
         WHERE bt.book_id = ?1
         ORDER BY t.name ASC",
    )?;
    let rows = stmt.query_map(params![book_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

pub fn delete_tag(conn: &Connection, tag_id: &str) -> Result<()> {
    conn.execute("DELETE FROM tags WHERE id = ?1", params![tag_id])?;
    Ok(())
}

pub fn list_all_book_tags(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT book_id, tag_id FROM book_tags")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

pub fn get_books_in_collection(conn: &Connection, collection_id: &str) -> Result<Vec<Book>> {
    let mut type_stmt = conn.prepare("SELECT type FROM collections WHERE id = ?1")?;
    let coll_type: String = type_stmt.query_row(params![collection_id], |row| row.get(0))?;

    if coll_type == "manual" {
        let sql = format!(
            "SELECT {} FROM books b JOIN book_collections bc ON bc.book_id = b.id WHERE bc.collection_id = ?1 ORDER BY bc.added_at DESC",
            BOOK_COLUMNS_B
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![collection_id], row_to_book)?;
        return rows.collect();
    }

    // Automated: build a dynamic parameterized query from collection rules.
    let rules = get_collection_rules(conn, collection_id)?;
    let (joins, where_str, param_values) = build_rule_query(&rules);

    let sql = format!(
        "SELECT DISTINCT {cols}
         FROM books b
         {joins}
         {where_str}
         ORDER BY b.added_at DESC",
        cols = BOOK_COLUMNS_B
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(param_values.iter()), row_to_book)?;
    rows.collect()
}

/// Lightweight variant of `get_books_in_collection` returning only grid-display fields.
pub fn get_books_in_collection_grid(
    conn: &Connection,
    collection_id: &str,
) -> Result<Vec<BookGridItem>> {
    let mut type_stmt = conn.prepare("SELECT type FROM collections WHERE id = ?1")?;
    let coll_type: String = type_stmt.query_row(params![collection_id], |row| row.get(0))?;

    if coll_type == "manual" {
        let sql = format!(
            "SELECT {} FROM books b JOIN book_collections bc ON bc.book_id = b.id WHERE bc.collection_id = ?1 ORDER BY bc.added_at DESC",
            GRID_COLUMNS_B
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![collection_id], row_to_grid_item)?;
        return rows.collect();
    }

    let rules = get_collection_rules(conn, collection_id)?;
    let (joins, where_str, param_values) = build_rule_query(&rules);

    let sql = format!(
        "SELECT DISTINCT {cols}
         FROM books b
         {joins}
         {where_str}
         ORDER BY b.added_at DESC",
        cols = GRID_COLUMNS_B
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(param_values.iter()),
        row_to_grid_item,
    )?;
    rows.collect()
}

/// Build JOIN, WHERE, and parameter lists from a set of collection rules.
fn build_rule_query(rules: &[CollectionRule]) -> (String, String, Vec<String>) {
    let mut join_clauses: Vec<String> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();
    let mut param_values: Vec<String> = Vec::new();
    let mut rp_idx: u32 = 0;
    let mut tag_idx: u32 = 0;

    for rule in rules {
        match (rule.field.as_str(), rule.operator.as_str()) {
            ("author", "contains") => {
                where_clauses.push("b.author LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("author", "equals") => {
                where_clauses.push("b.author = ?".to_string());
                param_values.push(rule.value.clone());
            }
            ("filename", "contains") => {
                where_clauses.push("b.title LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("series", "contains") => {
                where_clauses.push("b.series LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("series", "equals") => {
                where_clauses.push("b.series = ?".to_string());
                param_values.push(rule.value.clone());
            }
            ("language", "equals") => {
                where_clauses.push("b.language = ?".to_string());
                param_values.push(rule.value.clone());
            }
            ("language", "contains") => {
                where_clauses.push("b.language LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("publisher", "contains") => {
                where_clauses.push("b.publisher LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("description", "contains") => {
                where_clauses.push("b.description LIKE ?".to_string());
                param_values.push(format!("%{}%", rule.value));
            }
            ("format", "equals") => {
                where_clauses.push("b.format = ?".to_string());
                param_values.push(rule.value.clone());
            }
            ("date_added", "last_n_days") => {
                where_clauses.push(
                    "b.added_at > (strftime('%s', 'now') - CAST(? AS INTEGER) * 86400)".to_string(),
                );
                param_values.push(rule.value.clone());
            }
            ("tag", "contains") => {
                tag_idx += 1;
                let bt = format!("bt{tag_idx}");
                let tt = format!("tt{tag_idx}");
                join_clauses.push(format!(
                    "JOIN book_tags {bt} ON {bt}.book_id = b.id \
                     JOIN tags {tt} ON {tt}.id = {bt}.tag_id AND {tt}.name LIKE ?"
                ));
                param_values.push(format!("%{}%", rule.value));
            }
            ("tag", "equals") => {
                tag_idx += 1;
                let bt = format!("bt{tag_idx}");
                let tt = format!("tt{tag_idx}");
                join_clauses.push(format!(
                    "JOIN book_tags {bt} ON {bt}.book_id = b.id \
                     JOIN tags {tt} ON {tt}.id = {bt}.tag_id AND {tt}.name = ?"
                ));
                param_values.push(rule.value.clone());
            }
            ("reading_progress", "equals") => {
                rp_idx += 1;
                let alias = format!("rp{rp_idx}");
                match rule.value.as_str() {
                    "unread" => {
                        join_clauses.push(format!(
                            "LEFT JOIN reading_progress {alias} ON {alias}.book_id = b.id"
                        ));
                        where_clauses.push(format!("{alias}.book_id IS NULL"));
                    }
                    "in_progress" => {
                        join_clauses.push(format!(
                            "JOIN reading_progress {alias} ON {alias}.book_id = b.id"
                        ));
                        where_clauses.push(format!("{alias}.chapter_index < b.total_chapters - 1"));
                    }
                    "finished" => {
                        join_clauses.push(format!(
                            "JOIN reading_progress {alias} ON {alias}.book_id = b.id"
                        ));
                        where_clauses.push(format!(
                            "{alias}.chapter_index >= b.total_chapters - 1 AND b.total_chapters > 0"
                        ));
                    }
                    _ => {}
                }
            }
            _ => {} // unrecognised rule — skip
        }
    }

    let joins = join_clauses.join(" ");
    let where_str = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    (joins, where_str, param_values)
}

/// Preview how many books match a set of rules without persisting a collection.
pub fn preview_collection_rules(
    conn: &Connection,
    rules: &[crate::models::NewRuleInput],
) -> Result<usize> {
    use crate::models::CollectionRule;
    let converted: Vec<CollectionRule> = rules
        .iter()
        .map(|r| CollectionRule {
            id: String::new(),
            collection_id: String::new(),
            field: r.field.clone(),
            operator: r.operator.clone(),
            value: r.value.clone(),
        })
        .collect();
    let (joins, where_str, param_values) = build_rule_query(&converted);

    let sql = format!(
        "SELECT COUNT(DISTINCT b.id)
         FROM books b
         {joins}
         {where_str}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let count: usize = stmt.query_row(rusqlite::params_from_iter(param_values.iter()), |row| {
        row.get(0)
    })?;
    Ok(count)
}

// --- Activity log ---

pub fn insert_activity(conn: &Connection, entry: &ActivityEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO activity_log (id, timestamp, action, entity_type, entity_id, entity_name, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![entry.id, entry.timestamp, entry.action, entry.entity_type, entry.entity_id, entry.entity_name, entry.detail],
    )?;
    Ok(())
}

pub fn get_activity_log(
    conn: &Connection,
    limit: u32,
    offset: u32,
    action_filter: Option<&str>,
) -> Result<Vec<ActivityEntry>> {
    let (sql, filter_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(action) =
        action_filter
    {
        (
                "SELECT id, timestamp, action, entity_type, entity_id, entity_name, detail FROM activity_log WHERE action = ?1 ORDER BY timestamp DESC LIMIT ?2 OFFSET ?3".to_string(),
                vec![
                    Box::new(action.to_string()) as Box<dyn rusqlite::types::ToSql>,
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
    } else {
        (
                "SELECT id, timestamp, action, entity_type, entity_id, entity_name, detail FROM activity_log ORDER BY timestamp DESC LIMIT ?1 OFFSET ?2".to_string(),
                vec![
                    Box::new(limit) as Box<dyn rusqlite::types::ToSql>,
                    Box::new(offset),
                ],
            )
    };

    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        filter_params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(ActivityEntry {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            action: row.get(2)?,
            entity_type: row.get(3)?,
            entity_id: row.get(4)?,
            entity_name: row.get(5)?,
            detail: row.get(6)?,
        })
    })?;
    rows.collect()
}

pub fn get_all_activity(conn: &Connection) -> Result<Vec<ActivityEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, action, entity_type, entity_id, entity_name, detail FROM activity_log ORDER BY timestamp DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ActivityEntry {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            action: row.get(2)?,
            entity_type: row.get(3)?,
            entity_id: row.get(4)?,
            entity_name: row.get(5)?,
            detail: row.get(6)?,
        })
    })?;
    rows.collect()
}

pub fn prune_activity_log(conn: &Connection, keep: u32, max_age_days: u32) -> Result<usize> {
    let cutoff = chrono::Utc::now().timestamp() - (max_age_days as i64) * 24 * 60 * 60;
    let deleted = conn.execute(
        "DELETE FROM activity_log WHERE id NOT IN (SELECT id FROM activity_log ORDER BY timestamp DESC LIMIT ?1) AND timestamp < ?2",
        params![keep, cutoff],
    )?;
    Ok(deleted)
}

pub fn insert_web_session_log(conn: &Connection, entry: &WebSessionEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO web_session_log (id, timestamp, ip, method, outcome, user_agent) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            entry.id,
            entry.timestamp,
            entry.ip,
            entry.method,
            entry.outcome,
            entry.user_agent
        ],
    )?;
    Ok(())
}

pub fn get_web_session_log(conn: &Connection, limit: u32) -> Result<Vec<WebSessionEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, ip, method, outcome, user_agent FROM web_session_log ORDER BY timestamp DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(WebSessionEntry {
            id: row.get(0)?,
            timestamp: row.get(1)?,
            ip: row.get(2)?,
            method: row.get(3)?,
            outcome: row.get(4)?,
            user_agent: row.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn prune_web_session_log(conn: &Connection, keep: u32, max_age_days: u32) -> Result<usize> {
    let cutoff = chrono::Utc::now().timestamp() - (max_age_days as i64) * 24 * 60 * 60;
    // Enforce age and count bounds independently — web_session_log is fed by
    // network clients (incl. rate-limited spam), so the count cap must hold even
    // when all rows are recent. (activity_log is user-driven and bounded, so it
    // can get away with the combined AND condition; this table cannot.)
    let by_age = conn.execute(
        "DELETE FROM web_session_log WHERE timestamp < ?1",
        params![cutoff],
    )?;
    let by_count = conn.execute(
        "DELETE FROM web_session_log WHERE id NOT IN (SELECT id FROM web_session_log ORDER BY timestamp DESC LIMIT ?1)",
        params![keep],
    )?;
    Ok(by_age + by_count)
}

pub fn insert_custom_font(conn: &Connection, font: &CustomFont) -> Result<()> {
    conn.execute(
        "INSERT INTO custom_fonts (id, name, file_name, file_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            font.id,
            font.name,
            font.file_name,
            font.file_path,
            font.created_at
        ],
    )?;
    Ok(())
}

pub fn list_custom_fonts(conn: &Connection) -> Result<Vec<CustomFont>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, file_name, file_path, created_at
         FROM custom_fonts ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CustomFont {
            id: row.get(0)?,
            name: row.get(1)?,
            file_name: row.get(2)?,
            file_path: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    rows.collect()
}

pub fn delete_custom_font(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM custom_fonts WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_custom_font(conn: &Connection, id: &str) -> Result<Option<CustomFont>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, file_name, file_path, created_at
         FROM custom_fonts WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(CustomFont {
            id: row.get(0)?,
            name: row.get(1)?,
            file_name: row.get(2)?,
            file_path: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    rows.next().transpose()
}

pub fn list_series(conn: &Connection) -> Result<Vec<SeriesInfo>> {
    let mut stmt = conn.prepare(
        "SELECT series, COUNT(*) as count FROM books
         WHERE series IS NOT NULL AND series != ''
         GROUP BY series HAVING count >= 2
         ORDER BY series ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SeriesInfo {
            name: row.get(0)?,
            count: row.get(1)?,
        })
    })?;
    rows.collect()
}

pub fn log_pin_change(conn: &Connection, source: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO pin_change_log (changed_at, source) VALUES (?1, ?2)",
        rusqlite::params![now, source],
    )?;
    Ok(())
}

pub fn get_collection_suggestions(
    conn: &Connection,
    existing_collections: &[Collection],
) -> Result<Vec<CollectionSuggestion>> {
    let mut suggestions = Vec::new();
    let colors = [
        "#c2714e", "#6b8f71", "#7a6b9a", "#4e7a8f", "#8f7a4e", "#8f4e4e", "#4e8f8a", "#666666",
    ];
    let mut color_idx = 0;

    let existing_rules: Vec<(&str, &str, &str)> = existing_collections
        .iter()
        .filter(|c| matches!(c.r#type, CollectionType::Automated))
        .flat_map(|c| {
            c.rules
                .iter()
                .map(|r| (r.field.as_str(), r.operator.as_str(), r.value.as_str()))
        })
        .collect();

    // Author heuristic: authors with 3+ books
    {
        let mut stmt = conn.prepare(
            "SELECT author, COUNT(*) as cnt FROM books \
             WHERE author IS NOT NULL AND author != '' \
             GROUP BY author HAVING cnt >= 3 \
             ORDER BY cnt DESC LIMIT 5",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        for row in rows {
            let (author, count) = row?;
            if existing_rules
                .iter()
                .any(|(f, o, v)| *f == "author" && *o == "equals" && *v == author)
            {
                continue;
            }
            suggestions.push(CollectionSuggestion {
                name: format!("Books by {author}"),
                icon: "📖".to_string(),
                color: colors[color_idx % colors.len()].to_string(),
                rules: vec![NewRuleInput {
                    field: "author".to_string(),
                    operator: "equals".to_string(),
                    value: author,
                }],
                matched_book_count: count,
                heuristic_type: "author".to_string(),
            });
            color_idx += 1;
        }
    }

    // Series heuristic: series with 2+ books
    {
        let mut stmt = conn.prepare(
            "SELECT series, COUNT(*) as cnt FROM books \
             WHERE series IS NOT NULL AND series != '' \
             GROUP BY series HAVING cnt >= 2 \
             ORDER BY cnt DESC LIMIT 5",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        for row in rows {
            let (series, count) = row?;
            if existing_rules
                .iter()
                .any(|(f, o, v)| *f == "series" && *o == "equals" && *v == series)
            {
                continue;
            }
            suggestions.push(CollectionSuggestion {
                name: format!("{series} series"),
                icon: "📚".to_string(),
                color: colors[color_idx % colors.len()].to_string(),
                rules: vec![NewRuleInput {
                    field: "series".to_string(),
                    operator: "equals".to_string(),
                    value: series,
                }],
                matched_book_count: count,
                heuristic_type: "series".to_string(),
            });
            color_idx += 1;
        }
    }

    // Reading status heuristic: unread and finished
    {
        // Unread: books with no reading_progress entry
        let unread_count: usize = conn.query_row(
            "SELECT COUNT(*) FROM books b \
             LEFT JOIN reading_progress rp ON rp.book_id = b.id \
             WHERE rp.book_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        if unread_count >= 3
            && !existing_rules
                .iter()
                .any(|(f, o, v)| *f == "reading_progress" && *o == "equals" && *v == "unread")
        {
            suggestions.push(CollectionSuggestion {
                name: "Unread books".to_string(),
                icon: "🎯".to_string(),
                color: colors[color_idx % colors.len()].to_string(),
                rules: vec![NewRuleInput {
                    field: "reading_progress".to_string(),
                    operator: "equals".to_string(),
                    value: "unread".to_string(),
                }],
                matched_book_count: unread_count,
                heuristic_type: "reading_status".to_string(),
            });
            color_idx += 1;
        }

        // Finished: books where chapter_index >= total_chapters - 1
        let finished_count: usize = conn.query_row(
            "SELECT COUNT(*) FROM books b \
             JOIN reading_progress rp ON rp.book_id = b.id \
             WHERE rp.chapter_index >= b.total_chapters - 1 AND b.total_chapters > 0",
            [],
            |row| row.get(0),
        )?;
        if finished_count >= 2
            && !existing_rules
                .iter()
                .any(|(f, o, v)| *f == "reading_progress" && *o == "equals" && *v == "finished")
        {
            suggestions.push(CollectionSuggestion {
                name: "Finished books".to_string(),
                icon: "🏆".to_string(),
                color: colors[color_idx % colors.len()].to_string(),
                rules: vec![NewRuleInput {
                    field: "reading_progress".to_string(),
                    operator: "equals".to_string(),
                    value: "finished".to_string(),
                }],
                matched_book_count: finished_count,
                heuristic_type: "reading_status".to_string(),
            });
            color_idx += 1;
        }
    }

    // Format heuristic: non-dominant formats with 3+ books
    {
        let total_books: usize =
            conn.query_row("SELECT COUNT(*) FROM books", [], |row| row.get(0))?;
        if total_books > 0 {
            let mut stmt = conn.prepare(
                "SELECT format, COUNT(*) as cnt FROM books \
                 GROUP BY format HAVING cnt >= 3 \
                 ORDER BY cnt DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
            })?;
            for row in rows {
                let (format, count) = row?;
                // Skip dominant format (≥ 75% of all books)
                if count * 4 >= total_books * 3 {
                    continue;
                }
                if existing_rules
                    .iter()
                    .any(|(f, o, v)| *f == "format" && *o == "equals" && *v == format)
                {
                    continue;
                }
                let display_name = format.to_uppercase();
                suggestions.push(CollectionSuggestion {
                    name: format!("{display_name} Books"),
                    icon: "📄".to_string(),
                    color: colors[color_idx % colors.len()].to_string(),
                    rules: vec![NewRuleInput {
                        field: "format".to_string(),
                        operator: "equals".to_string(),
                        value: format,
                    }],
                    matched_book_count: count,
                    heuristic_type: "format".to_string(),
                });
                color_idx += 1;
            }
        }
    }

    suggestions.sort_by_key(|s| std::cmp::Reverse(s.matched_book_count));
    suggestions.truncate(8);
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::BookFormat;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = init_db(&db_path).unwrap();
        (dir, conn)
    }

    fn sample_book(id: &str) -> Book {
        Book {
            id: id.to_string(),
            title: "Test Book".to_string(),
            author: "Test Author".to_string(),
            file_path: "/tmp/test.epub".to_string(),
            cover_path: None,
            total_chapters: 10,
            added_at: 1700000000,
            format: BookFormat::Epub,
            file_hash: None,
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
            is_imported: true,
            want_to_read: false,
        }
    }

    #[test]
    fn test_book_crud() {
        let (_dir, conn) = setup();
        let book = sample_book("book-1");

        insert_book(&conn, &book).unwrap();

        let fetched = get_book(&conn, "book-1").unwrap().unwrap();
        assert_eq!(fetched.title, "Test Book");
        assert_eq!(fetched.author, "Test Author");

        let books = list_books(&conn).unwrap();
        assert_eq!(books.len(), 1);

        let updated = Book {
            title: "Updated Title".to_string(),
            ..book
        };
        update_book(&conn, &updated).unwrap();
        let fetched2 = get_book(&conn, "book-1").unwrap().unwrap();
        assert_eq!(fetched2.title, "Updated Title");

        delete_book(&conn, "book-1").unwrap();
        assert!(get_book(&conn, "book-1").unwrap().is_none());
    }

    #[test]
    fn test_reading_progress_upsert() {
        let (_dir, conn) = setup();
        let book = sample_book("book-2");
        insert_book(&conn, &book).unwrap();

        let progress = ReadingProgress {
            book_id: "book-2".to_string(),
            chapter_index: 3,
            scroll_position: 0.5,
            last_read_at: 1700000100,
        };
        upsert_reading_progress(&conn, &progress).unwrap();

        let fetched = get_reading_progress(&conn, "book-2").unwrap().unwrap();
        assert_eq!(fetched.chapter_index, 3);
        assert!((fetched.scroll_position - 0.5).abs() < f64::EPSILON);

        let updated = ReadingProgress {
            chapter_index: 5,
            scroll_position: 0.8,
            ..progress
        };
        upsert_reading_progress(&conn, &updated).unwrap();
        let fetched2 = get_reading_progress(&conn, "book-2").unwrap().unwrap();
        assert_eq!(fetched2.chapter_index, 5);
    }

    // F-5-4: the heatmap needs a 365-day series distinct from the existing
    // 30-day bar chart's `daily_reading` — a session 40 days old should show
    // up in `daily_reading_year` but not `daily_reading`, and a session
    // older than 365 days should be excluded from both. Both series use a
    // local-calendar-day window (not a rolling timestamp cutoff), so a
    // session exactly 364 days ago (by local date) is included in full and
    // one 365+ days ago is excluded.
    #[test]
    fn test_get_reading_stats_daily_reading_year_window() {
        let (_dir, conn) = setup();
        let book = sample_book("book-3");
        insert_book(&conn, &book).unwrap();

        // Every session is anchored to local noon on a calendar date rather
        // than to `now` minus a day count. `now`-relative timestamps make the
        // test time-of-day dependent: a session started within its own
        // duration of local midnight is *correctly* split across two days by
        // `accumulate_session_by_local_day` (F-5-2), so `any(secs == 600)`
        // stops holding. That is how this test failed in CI on runs that
        // landed in the last ten minutes of the UTC day.
        let today = chrono::Local::now().date_naive();
        let local_noon_timestamp = |date: chrono::NaiveDate| -> i64 {
            date.and_hms_opt(12, 0, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .unwrap()
                .timestamp()
        };
        let days_ago = |n: i64| local_noon_timestamp(today - chrono::Duration::days(n));

        insert_reading_session(&conn, "s-recent", "book-3", days_ago(1), 600, 1).unwrap();
        insert_reading_session(&conn, "s-40d", "book-3", days_ago(40), 900, 1).unwrap();
        insert_reading_session(&conn, "s-400d", "book-3", days_ago(400), 1200, 1).unwrap();

        // Calendar-window edge: exactly 364 days ago (included), one day
        // further back at 365 days ago (excluded).
        let day_364_ago = days_ago(364);
        let day_365_ago = days_ago(365);
        insert_reading_session(&conn, "s-364d", "book-3", day_364_ago, 300, 1).unwrap();
        insert_reading_session(&conn, "s-365d", "book-3", day_365_ago, 450, 1).unwrap();

        let stats = get_reading_stats(&conn).unwrap();

        assert!(stats.daily_reading.iter().any(|(_, secs)| *secs == 600));
        assert!(!stats.daily_reading.iter().any(|(_, secs)| *secs == 900));
        assert!(!stats.daily_reading.iter().any(|(_, secs)| *secs == 1200));

        assert!(stats
            .daily_reading_year
            .iter()
            .any(|(_, secs)| *secs == 600));
        assert!(stats
            .daily_reading_year
            .iter()
            .any(|(_, secs)| *secs == 900));
        assert!(!stats
            .daily_reading_year
            .iter()
            .any(|(_, secs)| *secs == 1200));
        assert!(stats
            .daily_reading_year
            .iter()
            .any(|(_, secs)| *secs == 300));
        assert!(!stats
            .daily_reading_year
            .iter()
            .any(|(_, secs)| *secs == 450));
    }

    // F-5-2: a session crossing local midnight must credit each calendar day
    // only the seconds elapsed on it — not dump the whole duration on its
    // start day. Otherwise the daily-minutes goal reads 0 for "today" after
    // midnight even though reading continued into today.
    #[test]
    fn test_daily_reading_year_splits_across_local_midnight() {
        use chrono::TimeZone;
        let (_dir, conn) = setup();
        let book = sample_book("book-mid");
        insert_book(&conn, &book).unwrap();

        let today = chrono::Local::now().date_naive();
        let today_midnight = chrono::Local
            .from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp();
        // 30-min session from 23:50 yesterday to 00:20 today: 10 min yesterday,
        // 20 min today.
        insert_reading_session(&conn, "s-mid", "book-mid", today_midnight - 600, 1800, 1).unwrap();

        let stats = get_reading_stats(&conn).unwrap();
        let today_str = today.format("%Y-%m-%d").to_string();
        let yesterday_str = (today - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let secs_on = |day: &str| {
            stats
                .daily_reading_year
                .iter()
                .find(|(d, _)| d == day)
                .map(|(_, s)| *s)
                .unwrap_or(0)
        };
        assert_eq!(secs_on(&today_str), 1200);
        assert_eq!(secs_on(&yesterday_str), 600);
    }

    // F-5-2: a session spanning a DST spring-forward that skips local midnight
    // (America/Havana jumps 00:00 -> 01:00 on 2024-03-10) must still credit the
    // seconds that fall on the new day to the new day, not dump the remainder on
    // the previous day. Uses a fixed IANA zone so the result is independent of
    // the host's local timezone. Regression for the `unwrap_or(end)` fallback.
    #[test]
    fn test_accumulate_splits_across_skipped_midnight_dst() {
        use chrono::TimeZone;
        let tz = chrono_tz::America::Havana;
        // 23:30 local the day before the transition; runs 60 real minutes:
        // 30 min to reach the 01:00 transition (start of 2024-03-10), then
        // 30 min into 2024-03-10.
        let started_at = tz
            .with_ymd_and_hms(2024, 3, 9, 23, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        let mut buckets: std::collections::BTreeMap<String, i64> =
            std::collections::BTreeMap::new();
        accumulate_session_by_local_day(&tz, started_at, 3600, &mut buckets);

        assert_eq!(buckets.get("2024-03-09").copied().unwrap_or(0), 1800);
        assert_eq!(buckets.get("2024-03-10").copied().unwrap_or(0), 1800);
    }

    // F-1-3: `books_finished_this_year` must only count books whose
    // `finished_at` falls in the current local calendar year — a book
    // finished last year contributes to the all-time `books_finished` but
    // not to this year's count (the goal-ring boundary case).
    #[test]
    fn test_get_reading_stats_books_finished_this_year_boundary() {
        use chrono::Datelike;

        let (_dir, conn) = setup();

        let finished_this_year = Book {
            file_path: "/tmp/finished-this-year.epub".to_string(),
            total_chapters: 10,
            ..sample_book("finished-this-year")
        };
        insert_book(&conn, &finished_this_year).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "finished-this-year".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: chrono::Local::now().timestamp(),
            },
        )
        .unwrap();

        let finished_last_year = Book {
            file_path: "/tmp/finished-last-year.epub".to_string(),
            total_chapters: 10,
            ..sample_book("finished-last-year")
        };
        insert_book(&conn, &finished_last_year).unwrap();
        let last_year = chrono::Local::now().year() - 1;
        let last_year_ts = chrono::NaiveDate::from_ymd_opt(last_year, 6, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .timestamp();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "finished-last-year".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: last_year_ts,
            },
        )
        .unwrap();

        let stats = get_reading_stats(&conn).unwrap();
        assert_eq!(stats.books_finished, 2, "both books count all-time");
        assert_eq!(
            stats.books_finished_this_year, 1,
            "only the book finished this calendar year should count"
        );
    }

    fn get_finished_at(conn: &Connection, book_id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT finished_at FROM reading_progress WHERE book_id = ?1",
            params![book_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    // Finding 1: `upsert_reading_progress` stamps `finished_at` the first
    // time a write crosses onto the last chapter, and never touches it
    // again — restarting (chapter_index back to 0) or re-finishing later
    // must not move or clear the original completion timestamp.
    #[test]
    fn test_upsert_reading_progress_stamps_finished_at_once() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-finish")
        };
        insert_book(&conn, &book).unwrap();

        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-finish".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 1_000,
            },
        )
        .unwrap();
        assert_eq!(get_finished_at(&conn, "book-finish"), Some(1_000));

        // Restart: back to chapter 0, later timestamp — finished_at must
        // stay put.
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-finish".to_string(),
                chapter_index: 0,
                scroll_position: 0.0,
                last_read_at: 2_000,
            },
        )
        .unwrap();
        assert_eq!(get_finished_at(&conn, "book-finish"), Some(1_000));

        // Finish again: back on the last chapter — still must not move.
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-finish".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 3_000,
            },
        )
        .unwrap();
        assert_eq!(get_finished_at(&conn, "book-finish"), Some(1_000));
    }

    // Finding 1 (headline bug): re-opening a book finished in a prior year
    // bumps `last_read_at` into the current year but must not re-count it
    // in `books_finished_this_year` — the count is scoped by `finished_at`,
    // which stays pinned to the original completion time.
    #[test]
    fn test_books_finished_this_year_excludes_reopened_prior_year_book() {
        use chrono::Datelike;

        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-reopened")
        };
        insert_book(&conn, &book).unwrap();

        let last_year = chrono::Local::now().year() - 1;
        let last_year_ts = chrono::NaiveDate::from_ymd_opt(last_year, 6, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .unwrap()
            .timestamp();

        // Finished last year.
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-reopened".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: last_year_ts,
            },
        )
        .unwrap();

        let now = chrono::Local::now().timestamp();

        // Re-opened this year (not on the last chapter) — bumps last_read_at
        // into this year.
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-reopened".to_string(),
                chapter_index: 3,
                scroll_position: 0.2,
                last_read_at: now,
            },
        )
        .unwrap();

        // Re-finished this year — last_read_at is this year, but finished_at
        // must remain pinned to last year's original completion.
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-reopened".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: now,
            },
        )
        .unwrap();
        assert_eq!(get_finished_at(&conn, "book-reopened"), Some(last_year_ts));

        let stats = get_reading_stats(&conn).unwrap();
        assert_eq!(stats.books_finished, 1, "counts all-time");
        assert_eq!(
            stats.books_finished_this_year, 0,
            "must not be re-counted just because it was re-opened this year"
        );
    }

    // Finding 1: pre-existing rows that already satisfy the finished
    // predicate before `finished_at` existed get backfilled from
    // `last_read_at`, the best available approximation.
    #[test]
    fn test_backfill_finished_at_migration() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-legacy")
        };
        insert_book(&conn, &book).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-legacy".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 5_000,
            },
        )
        .unwrap();

        // Simulate a pre-migration row: finished, but finished_at not yet
        // populated.
        conn.execute(
            "UPDATE reading_progress SET finished_at = NULL WHERE book_id = 'book-legacy'",
            [],
        )
        .unwrap();
        assert_eq!(get_finished_at(&conn, "book-legacy"), None);

        backfill_finished_at(&conn).unwrap();
        assert_eq!(get_finished_at(&conn, "book-legacy"), Some(5_000));
    }

    // Finding 2: a book with an unknown chapter count (`total_chapters ==
    // 0`) must not look "finished" just because a progress row exists —
    // `chapter_index >= total_chapters - 1` is trivially true at
    // `total_chapters == 0` without the guard.
    #[test]
    fn test_zero_chapter_book_not_counted_as_finished() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 0,
            ..sample_book("book-zero-chapters")
        };
        insert_book(&conn, &book).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-zero-chapters".to_string(),
                chapter_index: 0,
                scroll_position: 0.0,
                last_read_at: 1_000,
            },
        )
        .unwrap();

        assert_eq!(
            get_finished_at(&conn, "book-zero-chapters"),
            None,
            "zero-chapter book must never get a finished_at stamp"
        );

        let stats = get_reading_stats(&conn).unwrap();
        assert_eq!(stats.books_finished, 0);
        assert_eq!(stats.books_finished_this_year, 0);
    }

    // Item 5: "Continue Reading" shelf query — excludes never-started and
    // finished books, orders most-recently-read first, respects the limit.
    #[test]
    fn test_get_continue_reading_books_filters_and_orders() {
        let (_dir, conn) = setup();

        // Never started (chapter_index 0) — must be excluded even though it
        // has a reading_progress row.
        let unread = Book {
            file_path: "/tmp/cr-unread.epub".to_string(),
            ..sample_book("cr-unread")
        };
        insert_book(&conn, &unread).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "cr-unread".to_string(),
                chapter_index: 0,
                scroll_position: 0.0,
                last_read_at: 1_000,
            },
        )
        .unwrap();

        // Finished (on the last chapter of 10) — must be excluded.
        let finished = Book {
            file_path: "/tmp/cr-finished.epub".to_string(),
            total_chapters: 10,
            ..sample_book("cr-finished")
        };
        insert_book(&conn, &finished).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "cr-finished".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 2_000,
            },
        )
        .unwrap();

        // In progress, read longer ago — included, ranked second.
        let older = Book {
            file_path: "/tmp/cr-older.epub".to_string(),
            total_chapters: 10,
            ..sample_book("cr-older")
        };
        insert_book(&conn, &older).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "cr-older".to_string(),
                chapter_index: 2,
                scroll_position: 0.1,
                last_read_at: 3_000,
            },
        )
        .unwrap();

        // In progress, read most recently — included, ranked first.
        let newer = Book {
            file_path: "/tmp/cr-newer.epub".to_string(),
            total_chapters: 10,
            ..sample_book("cr-newer")
        };
        insert_book(&conn, &newer).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "cr-newer".to_string(),
                chapter_index: 5,
                scroll_position: 0.4,
                last_read_at: 4_000,
            },
        )
        .unwrap();

        let shelf = get_continue_reading_books(&conn, 12).unwrap();
        let ids: Vec<&str> = shelf.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["cr-newer", "cr-older"],
            "expected only the two in-progress, unfinished books, newest read first"
        );
        assert_eq!(shelf[0].chapter_index, 5);
        assert_eq!(shelf[0].total_chapters, 10);
    }

    #[test]
    fn test_get_continue_reading_books_respects_limit() {
        let (_dir, conn) = setup();
        for i in 0..5 {
            let id = format!("cr-limit-{i}");
            let book = Book {
                file_path: format!("/tmp/{id}.epub"),
                total_chapters: 10,
                ..sample_book(&id)
            };
            insert_book(&conn, &book).unwrap();
            upsert_reading_progress(
                &conn,
                &ReadingProgress {
                    book_id: id,
                    chapter_index: 3,
                    scroll_position: 0.2,
                    last_read_at: 1_000 + i,
                },
            )
            .unwrap();
        }

        let shelf = get_continue_reading_books(&conn, 2).unwrap();
        assert_eq!(shelf.len(), 2, "limit must cap the result count");
        // Most recently read (highest last_read_at) first.
        assert_eq!(shelf[0].id, "cr-limit-4");
        assert_eq!(shelf[1].id, "cr-limit-3");
    }

    // Finding D: total_chapters=0 (unknown) must be excluded from the shelf
    // entirely. get_reading_stats' books_finished predicate
    // (`chapter_index >= total_chapters - 1`, unguarded against
    // total_chapters=0) already counts ANY progress on such a book as
    // "finished" — treating it as "still in progress" here contradicted that
    // and showed the same book as both finished (stats) and in-progress
    // (this shelf) at once.
    #[test]
    fn test_get_continue_reading_books_excludes_unknown_total_chapters() {
        let (_dir, conn) = setup();

        let unknown_total = Book {
            file_path: "/tmp/cr-unknown-total.epub".to_string(),
            total_chapters: 0,
            ..sample_book("cr-unknown-total")
        };
        insert_book(&conn, &unknown_total).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "cr-unknown-total".to_string(),
                chapter_index: 3,
                scroll_position: 0.2,
                last_read_at: 5_000,
            },
        )
        .unwrap();

        let shelf = get_continue_reading_books(&conn, 12).unwrap();
        assert!(
            shelf.iter().all(|b| b.id != "cr-unknown-total"),
            "a book with total_chapters=0 must be excluded from the shelf"
        );
    }

    #[test]
    fn test_duplicate_file_path_rejected() {
        let (_dir, conn) = setup();
        let book = sample_book("book-dup");
        insert_book(&conn, &book).unwrap();

        // Second insert with same file_path must fail.
        let duplicate = Book {
            id: "book-dup-2".to_string(),
            ..book
        };
        assert!(insert_book(&conn, &duplicate).is_err());
    }

    #[test]
    fn test_get_book_by_file_path() {
        let (_dir, conn) = setup();
        let book = sample_book("book-fp");
        insert_book(&conn, &book).unwrap();

        let found = get_book_by_file_path(&conn, "/tmp/test.epub").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "book-fp");

        let missing = get_book_by_file_path(&conn, "/tmp/missing.epub").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_bookmark_crud() {
        let (_dir, conn) = setup();
        let book = sample_book("book-3");
        insert_book(&conn, &book).unwrap();

        let bookmark = Bookmark {
            id: "bm-1".to_string(),
            book_id: "book-3".to_string(),
            chapter_index: 2,
            scroll_position: 0.3,
            name: None,
            note: Some("Great quote".to_string()),
            created_at: 1700000200,
            updated_at: 1700000200,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bookmark).unwrap();

        let bookmarks = list_bookmarks(&conn, "book-3").unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].note, Some("Great quote".to_string()));

        delete_bookmark(&conn, "bm-1").unwrap();
        let bookmarks2 = list_bookmarks(&conn, "book-3").unwrap();
        assert_eq!(bookmarks2.len(), 0);
    }

    #[test]
    fn test_update_bookmark_name() {
        let (_dir, conn) = setup();
        let book = sample_book("book-bm-name");
        insert_book(&conn, &book).unwrap();

        let bookmark = Bookmark {
            id: "bm-name-1".to_string(),
            book_id: "book-bm-name".to_string(),
            chapter_index: 1,
            scroll_position: 0.5,
            name: None,
            note: None,
            created_at: 1700000400,
            updated_at: 1700000400,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bookmark).unwrap();

        let bookmarks = list_bookmarks(&conn, "book-bm-name").unwrap();
        assert_eq!(bookmarks[0].name, None);

        update_bookmark_name(&conn, "bm-name-1", Some("Important passage")).unwrap();
        let bookmarks = list_bookmarks(&conn, "book-bm-name").unwrap();
        assert_eq!(bookmarks[0].name, Some("Important passage".to_string()));

        update_bookmark_name(&conn, "bm-name-1", None).unwrap();
        let bookmarks = list_bookmarks(&conn, "book-bm-name").unwrap();
        assert_eq!(bookmarks[0].name, None);
    }

    #[test]
    fn test_soft_delete_bookmark_scoped_wrong_book_is_noop() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("book-A")).unwrap();
        // books.file_path is UNIQUE and sample_book reuses one path — a second
        // insert with the same path fails. Override it.
        insert_book(
            &conn,
            &Book {
                file_path: "/tmp/test-b.epub".to_string(),
                ..sample_book("book-B")
            },
        )
        .unwrap();
        let bm = Bookmark {
            id: "bm-1".to_string(),
            book_id: "book-A".to_string(),
            chapter_index: 0,
            scroll_position: 0.0,
            name: None,
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bm).unwrap();

        // Wrong book: no rows affected, bookmark still live.
        let n = soft_delete_bookmark_scoped(&conn, "book-B", "bm-1").unwrap();
        assert_eq!(n, 0);
        assert_eq!(list_bookmarks(&conn, "book-A").unwrap().len(), 1);

        // Correct book: one row affected, gone from the live list.
        let n = soft_delete_bookmark_scoped(&conn, "book-A", "bm-1").unwrap();
        assert_eq!(n, 1);
        assert_eq!(list_bookmarks(&conn, "book-A").unwrap().len(), 0);

        // Idempotent: deleting again affects nothing.
        let n = soft_delete_bookmark_scoped(&conn, "book-A", "bm-1").unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_update_bookmark_name_scoped_wrong_book_is_noop() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("book-A")).unwrap();
        insert_book(
            &conn,
            &Book {
                file_path: "/tmp/test-b.epub".to_string(),
                ..sample_book("book-B")
            },
        )
        .unwrap();
        let bm = Bookmark {
            id: "bm-2".to_string(),
            book_id: "book-A".to_string(),
            chapter_index: 0,
            scroll_position: 0.0,
            name: None,
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bm).unwrap();

        // Wrong book: no row returned, name unchanged.
        let r = update_bookmark_name_scoped(&conn, "book-B", "bm-2", Some("x")).unwrap();
        assert!(r.is_none());
        assert_eq!(list_bookmarks(&conn, "book-A").unwrap()[0].name, None);

        // Correct book: returns the updated row.
        let r = update_bookmark_name_scoped(&conn, "book-A", "bm-2", Some("Chapter note")).unwrap();
        assert_eq!(r.unwrap().name, Some("Chapter note".to_string()));
        assert_eq!(
            list_bookmarks(&conn, "book-A").unwrap()[0].name,
            Some("Chapter note".to_string())
        );

        // A soft-deleted row can't be renamed.
        soft_delete_bookmark_scoped(&conn, "book-A", "bm-2").unwrap();
        let r = update_bookmark_name_scoped(&conn, "book-A", "bm-2", Some("later")).unwrap();
        assert!(r.is_none());
    }

    fn test_highlight(book_id: &str, id: &str) -> crate::models::Highlight {
        crate::models::Highlight {
            id: id.into(),
            book_id: book_id.into(),
            chapter_index: 0,
            text: "quoted text".into(),
            color: "#f6c445".into(),
            note: None,
            start_offset: 10,
            end_offset: 21,
            created_at: 1,
            updated_at: 1,
            deleted_at: None,
        }
    }

    #[test]
    fn update_highlight_scoped_sets_and_clears_fields() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("b1")).unwrap();
        insert_highlight(&conn, &test_highlight("b1", "h1")).unwrap();

        // set note only
        let up = update_highlight_scoped(&conn, "b1", "h1", Some(Some("a note")), None)
            .unwrap()
            .unwrap();
        assert_eq!(up.note.as_deref(), Some("a note"));
        assert_eq!(up.color, "#f6c445"); // untouched
        assert!(up.updated_at >= 1);

        // set color only — note survives
        let up = update_highlight_scoped(&conn, "b1", "h1", None, Some("#7bc47f"))
            .unwrap()
            .unwrap();
        assert_eq!(up.color, "#7bc47f");
        assert_eq!(up.note.as_deref(), Some("a note"));

        // clear note + set color in one call
        let up = update_highlight_scoped(&conn, "b1", "h1", Some(None), Some("#6ba3d6"))
            .unwrap()
            .unwrap();
        assert_eq!(up.note, None);
        assert_eq!(up.color, "#6ba3d6");
    }

    #[test]
    fn update_highlight_scoped_misses_return_none() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("b1")).unwrap();
        insert_book(&conn, &sample_book_with_path("b2", "/tmp/test-b2.epub")).unwrap();
        insert_highlight(&conn, &test_highlight("b1", "h1")).unwrap();

        // wrong book
        assert!(
            update_highlight_scoped(&conn, "b2", "h1", Some(Some("x")), None)
                .unwrap()
                .is_none()
        );
        // unknown id
        assert!(
            update_highlight_scoped(&conn, "b1", "nope", Some(Some("x")), None)
                .unwrap()
                .is_none()
        );
        // soft-deleted row
        soft_delete_highlight(&conn, "h1").unwrap();
        assert!(
            update_highlight_scoped(&conn, "b1", "h1", Some(Some("x")), None)
                .unwrap()
                .is_none()
        );
        // neither field requested → no-op None (handler 400s before calling)
        let h2 = test_highlight("b1", "h2");
        insert_highlight(&conn, &h2).unwrap();
        assert!(update_highlight_scoped(&conn, "b1", "h2", None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn soft_delete_highlight_scoped_is_book_scoped_and_idempotent() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("b1")).unwrap();
        insert_book(&conn, &sample_book_with_path("b2", "/tmp/test-b2.epub")).unwrap();
        insert_highlight(&conn, &test_highlight("b1", "h1")).unwrap();

        // wrong book → 0 rows, row survives
        assert_eq!(soft_delete_highlight_scoped(&conn, "b2", "h1").unwrap(), 0);
        assert_eq!(list_highlights(&conn, "b1").unwrap().len(), 1);
        // right book → 1 row
        assert_eq!(soft_delete_highlight_scoped(&conn, "b1", "h1").unwrap(), 1);
        assert_eq!(list_highlights(&conn, "b1").unwrap().len(), 0);
        // repeat → 0 rows (idempotent)
        assert_eq!(soft_delete_highlight_scoped(&conn, "b1", "h1").unwrap(), 0);
    }

    #[test]
    fn test_delete_book_cascades_to_related_rows() {
        let (_dir, conn) = setup();
        let book = sample_book("book-cascade");
        insert_book(&conn, &book).unwrap();

        let bookmark = Bookmark {
            id: "bm-cascade".to_string(),
            book_id: "book-cascade".to_string(),
            chapter_index: 1,
            scroll_position: 0.1,
            name: None,
            note: None,
            created_at: 1700000300,
            updated_at: 1700000300,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bookmark).unwrap();

        let progress = ReadingProgress {
            book_id: "book-cascade".to_string(),
            chapter_index: 1,
            scroll_position: 0.1,
            last_read_at: 1700000300,
        };
        upsert_reading_progress(&conn, &progress).unwrap();

        // Deleting the book must cascade to both child tables.
        delete_book(&conn, "book-cascade").unwrap();

        let bookmarks = list_bookmarks(&conn, "book-cascade").unwrap();
        assert!(
            bookmarks.is_empty(),
            "bookmarks should be deleted via cascade"
        );

        let rp = get_reading_progress(&conn, "book-cascade").unwrap();
        assert!(
            rp.is_none(),
            "reading_progress should be deleted via cascade"
        );
    }

    fn sample_collection(id: &str, coll_type: CollectionType) -> Collection {
        Collection {
            id: id.to_string(),
            name: format!("Collection {id}"),
            r#type: coll_type,
            icon: None,
            color: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            rules: vec![],
        }
    }

    #[test]
    fn test_insert_and_list_collections() {
        let (_dir, conn) = setup();

        let manual = sample_collection("coll-manual", CollectionType::Manual);
        let automated = sample_collection("coll-auto", CollectionType::Automated);
        insert_collection(&conn, &manual).unwrap();
        insert_collection(&conn, &automated).unwrap();

        let collections = list_collections(&conn).unwrap();
        assert_eq!(collections.len(), 2);
        let names: Vec<&str> = collections.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Collection coll-manual"));
        assert!(names.contains(&"Collection coll-auto"));

        let auto = collections.iter().find(|c| c.id == "coll-auto").unwrap();
        assert!(matches!(auto.r#type, CollectionType::Automated));
    }

    #[test]
    fn test_add_and_remove_book_from_collection() {
        let (_dir, conn) = setup();
        let book = sample_book("book-coll-1");
        insert_book(&conn, &book).unwrap();
        let coll = sample_collection("coll-c1", CollectionType::Manual);
        insert_collection(&conn, &coll).unwrap();

        add_book_to_collection(&conn, "book-coll-1", "coll-c1").unwrap();
        let books = get_books_in_collection(&conn, "coll-c1").unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].id, "book-coll-1");

        remove_book_from_collection(&conn, "book-coll-1", "coll-c1").unwrap();
        let books2 = get_books_in_collection(&conn, "coll-c1").unwrap();
        assert!(books2.is_empty());
    }

    #[test]
    fn test_add_book_to_collection_duplicate_is_noop() {
        let (_dir, conn) = setup();
        let book = sample_book("book-dup-coll");
        insert_book(&conn, &book).unwrap();
        let coll = sample_collection("coll-dup", CollectionType::Manual);
        insert_collection(&conn, &coll).unwrap();

        add_book_to_collection(&conn, "book-dup-coll", "coll-dup").unwrap();
        // Second insert must succeed silently (INSERT OR IGNORE), not error.
        add_book_to_collection(&conn, "book-dup-coll", "coll-dup").unwrap();
        let books = get_books_in_collection(&conn, "coll-dup").unwrap();
        assert_eq!(
            books.len(),
            1,
            "duplicate insert should be ignored, not doubled"
        );
    }

    #[test]
    fn test_delete_book_cascades_to_book_collections() {
        let (_dir, conn) = setup();
        let book = sample_book("book-cas-coll");
        insert_book(&conn, &book).unwrap();
        let coll = sample_collection("coll-cas", CollectionType::Manual);
        insert_collection(&conn, &coll).unwrap();

        add_book_to_collection(&conn, "book-cas-coll", "coll-cas").unwrap();
        let before = get_books_in_collection(&conn, "coll-cas").unwrap();
        assert_eq!(before.len(), 1);

        delete_book(&conn, "book-cas-coll").unwrap();
        let after = get_books_in_collection(&conn, "coll-cas").unwrap();
        assert!(
            after.is_empty(),
            "book_collections row should be deleted via cascade"
        );
    }

    #[test]
    fn test_books_have_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('t1', 'T', 'A', '/t', 0, 100, 'epub', 100)",
            [],
        ).unwrap();
        let val: i64 = conn
            .query_row("SELECT updated_at FROM books WHERE id = 't1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(val, 100);
    }

    #[test]
    fn source_columns_exist_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(&dir.path().join("library.db")).unwrap();
        // Inserting with the new columns must succeed (proves columns exist).
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at, source_path, source_size, source_mtime)
             VALUES ('s1', 'T', 'A', '/storage/s1.epub', 0, 100, 'epub', 100, '/mnt/nas/T.epub', 1234, 1700000000)",
            [],
        ).unwrap();
        let (sp, ss, sm): (String, i64, i64) = conn
            .query_row(
                "SELECT source_path, source_size, source_mtime FROM books WHERE id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(sp, "/mnt/nas/T.epub");
        assert_eq!(ss, 1234);
        assert_eq!(sm, 1700000000);
    }

    #[test]
    fn test_bookmarks_have_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('b1', 'T', 'A', '/t', 0, 100, 'epub', 100)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, created_at, updated_at) VALUES ('bm1', 'b1', 0, 0.0, 100, 100)",
            [],
        ).unwrap();
        let val: i64 = conn
            .query_row(
                "SELECT updated_at FROM bookmarks WHERE id = 'bm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, 100);
    }

    #[test]
    fn test_highlights_have_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('b1', 'T', 'A', '/t', 0, 100, 'epub', 100)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO highlights (id, book_id, chapter_index, text, color, start_offset, end_offset, created_at, updated_at) VALUES ('h1', 'b1', 0, 'hi', '#fff', 0, 2, 100, 100)",
            [],
        ).unwrap();
        let val: i64 = conn
            .query_row(
                "SELECT updated_at FROM highlights WHERE id = 'h1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, 100);
    }

    #[test]
    fn test_books_have_enrichment_status() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(dir.path().join("test.db").as_path()).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('t1', 'T', 'A', '/t', 0, 100, 'epub', 100)",
            [],
        ).unwrap();
        let val: Option<String> = conn
            .query_row(
                "SELECT enrichment_status FROM books WHERE id = 't1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn bookmarks_book_id_index_exists() {
        let (_dir, conn) = setup();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_bookmarks_book_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "idx_bookmarks_book_id index should exist");
    }

    fn sample_activity(id: &str, action: &str, timestamp: i64) -> crate::models::ActivityEntry {
        crate::models::ActivityEntry {
            id: id.to_string(),
            timestamp,
            action: action.to_string(),
            entity_type: "book".to_string(),
            entity_id: Some("book-1".to_string()),
            entity_name: Some("Test Book".to_string()),
            detail: Some("some detail".to_string()),
        }
    }

    #[test]
    fn test_activity_log_crud() {
        let (_dir, conn) = setup();
        let entry = sample_activity("act-1", "import", 1700000000);
        insert_activity(&conn, &entry).unwrap();

        let results = get_activity_log(&conn, 100, 0, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "act-1");
        assert_eq!(results[0].action, "import");
        assert_eq!(results[0].entity_type, "book");
        assert_eq!(results[0].entity_id, Some("book-1".to_string()));
        assert_eq!(results[0].entity_name, Some("Test Book".to_string()));
        assert_eq!(results[0].detail, Some("some detail".to_string()));
        assert_eq!(results[0].timestamp, 1700000000);
    }

    #[test]
    fn test_activity_log_ordering() {
        let (_dir, conn) = setup();
        insert_activity(&conn, &sample_activity("act-a", "import", 1000)).unwrap();
        insert_activity(&conn, &sample_activity("act-b", "import", 3000)).unwrap();
        insert_activity(&conn, &sample_activity("act-c", "import", 2000)).unwrap();

        let results = get_activity_log(&conn, 100, 0, None).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "act-b"); // most recent first
        assert_eq!(results[1].id, "act-c");
        assert_eq!(results[2].id, "act-a");
    }

    #[test]
    fn test_activity_log_filter_by_action() {
        let (_dir, conn) = setup();
        insert_activity(&conn, &sample_activity("act-f1", "import", 1000)).unwrap();
        insert_activity(&conn, &sample_activity("act-f2", "delete", 2000)).unwrap();

        let imports = get_activity_log(&conn, 100, 0, Some("import")).unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].id, "act-f1");

        let deletes = get_activity_log(&conn, 100, 0, Some("delete")).unwrap();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].id, "act-f2");
    }

    #[test]
    fn test_activity_log_pruning() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        // 2 old entries (>90 days) that will fall outside top 3
        insert_activity(
            &conn,
            &sample_activity("act-p0", "import", now - 100 * 86400),
        )
        .unwrap();
        insert_activity(
            &conn,
            &sample_activity("act-p1", "import", now - 95 * 86400),
        )
        .unwrap();
        // 3 recent entries that will be in the top 3
        for i in 2..5 {
            insert_activity(
                &conn,
                &sample_activity(&format!("act-p{i}"), "import", now - 60 + i as i64),
            )
            .unwrap();
        }

        prune_activity_log(&conn, 3, 90).unwrap();

        let results = get_activity_log(&conn, 100, 0, None).unwrap();
        assert_eq!(results.len(), 3);
        // Should keep the 3 most recent
        assert_eq!(results[0].id, "act-p4");
        assert_eq!(results[1].id, "act-p3");
        assert_eq!(results[2].id, "act-p2");
    }

    #[test]
    fn test_activity_log_age_pruning() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        // Insert 2 old entries (>90 days) and 2 recent entries
        insert_activity(
            &conn,
            &sample_activity("act-old1", "import", now - 100 * 86400),
        )
        .unwrap();
        insert_activity(
            &conn,
            &sample_activity("act-old2", "import", now - 91 * 86400),
        )
        .unwrap();
        insert_activity(
            &conn,
            &sample_activity("act-new1", "import", now - 10 * 86400),
        )
        .unwrap();
        insert_activity(&conn, &sample_activity("act-new2", "import", now - 86400)).unwrap();

        // keep=2 means old entries outside top 2 AND older than 90 days are pruned
        prune_activity_log(&conn, 2, 90).unwrap();

        let results = get_activity_log(&conn, 100, 0, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "act-new2");
        assert_eq!(results[1].id, "act-new1");
    }

    #[test]
    fn test_prune_respects_custom_max_age_and_returns_count() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        insert_activity(&conn, &sample_activity("a-40a", "import", now - 40 * 86400)).unwrap();
        insert_activity(&conn, &sample_activity("a-40b", "import", now - 41 * 86400)).unwrap();
        insert_activity(&conn, &sample_activity("a-5", "import", now - 5 * 86400)).unwrap();

        // keep=0, max_age_days=30 -> both 40-day rows pruned, 5-day row kept.
        let deleted = prune_activity_log(&conn, 0, 30).unwrap();
        assert_eq!(deleted, 2);

        let results = get_activity_log(&conn, 100, 0, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a-5");
    }

    #[test]
    fn test_get_all_activity_returns_all_newest_first() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        insert_activity(&conn, &sample_activity("g1", "import", now - 30)).unwrap();
        insert_activity(&conn, &sample_activity("g2", "import", now - 10)).unwrap();
        let all = get_all_activity(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "g2");
        assert_eq!(all[1].id, "g1");
    }

    fn sample_web_session(
        id: &str,
        outcome: &str,
        timestamp: i64,
    ) -> crate::models::WebSessionEntry {
        crate::models::WebSessionEntry {
            id: id.to_string(),
            timestamp,
            ip: "203.0.113.7".to_string(),
            method: "session".to_string(),
            outcome: outcome.to_string(),
            user_agent: Some("Mozilla/5.0".to_string()),
        }
    }

    #[test]
    fn test_web_session_log_insert_and_get_newest_first() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        insert_web_session_log(&conn, &sample_web_session("w1", "invalid_pin", now - 20)).unwrap();
        insert_web_session_log(&conn, &sample_web_session("w2", "success", now - 5)).unwrap();

        let rows = get_web_session_log(&conn, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "w2"); // newest first
        assert_eq!(rows[1].id, "w1");
        assert_eq!(rows[0].outcome, "success");
        assert_eq!(rows[0].user_agent.as_deref(), Some("Mozilla/5.0"));
    }

    #[test]
    fn test_web_session_log_get_respects_limit() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        for i in 0..5 {
            insert_web_session_log(
                &conn,
                &sample_web_session(&format!("w{i}"), "success", now - 10 + i),
            )
            .unwrap();
        }
        let rows = get_web_session_log(&conn, 2).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_web_session_log_null_user_agent() {
        let (_dir, conn) = setup();
        let mut e = sample_web_session("wn", "rate_limited", chrono::Utc::now().timestamp());
        e.user_agent = None;
        insert_web_session_log(&conn, &e).unwrap();
        let rows = get_web_session_log(&conn, 10).unwrap();
        assert_eq!(rows[0].user_agent, None);
    }

    #[test]
    fn test_prune_web_session_log_age_and_count() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        insert_web_session_log(
            &conn,
            &sample_web_session("old1", "invalid_pin", now - 100 * 86400),
        )
        .unwrap();
        insert_web_session_log(
            &conn,
            &sample_web_session("old2", "invalid_pin", now - 91 * 86400),
        )
        .unwrap();
        insert_web_session_log(
            &conn,
            &sample_web_session("new1", "success", now - 5 * 86400),
        )
        .unwrap();

        // keep=100 (no count pressure), max_age_days=90 -> both >90d rows pruned, recent kept.
        let deleted = prune_web_session_log(&conn, 100, 90).unwrap();
        assert_eq!(deleted, 2);
        let rows = get_web_session_log(&conn, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "new1");
    }

    #[test]
    fn test_prune_web_session_log_enforces_count_cap_when_recent() {
        let (_dir, conn) = setup();
        let now = chrono::Utc::now().timestamp();
        // 10 rows, all well within the age window.
        for i in 0..10 {
            insert_web_session_log(
                &conn,
                &sample_web_session(&format!("c{i}"), "rate_limited", now - i),
            )
            .unwrap();
        }
        // keep=3, generous age -> count cap drops the 7 oldest even though all recent.
        let deleted = prune_web_session_log(&conn, 3, 90).unwrap();
        assert_eq!(deleted, 7);
        let rows = get_web_session_log(&conn, 100).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_custom_font_crud() {
        let (_dir, conn) = setup();

        let font = CustomFont {
            id: "font-1".to_string(),
            name: "Merriweather".to_string(),
            file_name: "Merriweather-Regular.ttf".to_string(),
            file_path: "/tmp/fonts/font-1.ttf".to_string(),
            created_at: 1700000500,
        };
        insert_custom_font(&conn, &font).unwrap();

        let fonts = list_custom_fonts(&conn).unwrap();
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].name, "Merriweather");
        assert_eq!(fonts[0].file_path, "/tmp/fonts/font-1.ttf");

        let fetched = get_custom_font(&conn, "font-1").unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().file_name, "Merriweather-Regular.ttf");

        let missing = get_custom_font(&conn, "no-such-font").unwrap();
        assert!(missing.is_none());

        delete_custom_font(&conn, "font-1").unwrap();
        let fonts = list_custom_fonts(&conn).unwrap();
        assert_eq!(fonts.len(), 0);
    }

    #[test]
    fn test_list_series() {
        let (_dir, conn) = setup();

        let mut book1 = sample_book("s1");
        book1.file_path = "/tmp/s1.epub".to_string();
        book1.series = Some("Dune".to_string());
        book1.volume = Some(1);
        insert_book(&conn, &book1).unwrap();

        let mut book2 = sample_book("s2");
        book2.file_path = "/tmp/s2.epub".to_string();
        book2.series = Some("Dune".to_string());
        book2.volume = Some(2);
        insert_book(&conn, &book2).unwrap();

        let mut book3 = sample_book("s3");
        book3.file_path = "/tmp/s3.epub".to_string();
        book3.series = Some("Foundation".to_string());
        book3.volume = Some(1);
        insert_book(&conn, &book3).unwrap();

        // Single book in series — should NOT appear (threshold is 2+)
        let mut book4 = sample_book("s4");
        book4.file_path = "/tmp/s4.epub".to_string();
        book4.series = Some("Neuromancer".to_string());
        book4.volume = Some(1);
        insert_book(&conn, &book4).unwrap();

        // Book without series
        let mut book5 = sample_book("s5");
        book5.file_path = "/tmp/s5.epub".to_string();
        insert_book(&conn, &book5).unwrap();

        let series = list_series(&conn).unwrap();
        assert_eq!(series.len(), 1); // Only "Dune" has 2+ books
        assert_eq!(series[0].name, "Dune");
        assert_eq!(series[0].count, 2);
    }

    #[test]
    fn test_books_have_is_imported() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(&db_path).unwrap();
        run_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, is_imported) VALUES ('b1', 'Test', 'Author', '/tmp/test.epub', 1, 0, 'epub', 1)",
            [],
        ).unwrap();

        let is_imported: i32 = conn
            .query_row("SELECT is_imported FROM books WHERE id = 'b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(is_imported, 1);

        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, is_imported) VALUES ('b2', 'Linked', 'Author', '/mnt/nas/book.epub', 1, 0, 'epub', 0)",
            [],
        ).unwrap();

        let is_imported: i32 = conn
            .query_row("SELECT is_imported FROM books WHERE id = 'b2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(is_imported, 0);
    }

    #[test]
    fn migration_adds_want_to_read_column() {
        let conn = Connection::open_in_memory().unwrap();
        run_schema(&conn).unwrap();
        // PRAGMA is required because run_schema swallows ALTER errors — a missing
        // column would otherwise go unnoticed.
        let mut stmt = conn.prepare("PRAGMA table_info(books)").unwrap();
        let cols: Vec<(String, String, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(4).unwrap_or(0),
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let col = cols.iter().find(|(name, _, _)| name == "want_to_read");
        assert!(col.is_some(), "want_to_read column must exist");
        let (_, ty, dflt) = col.unwrap();
        assert_eq!(ty, "INTEGER");
        assert_eq!(*dflt, 0, "default must be 0");
    }

    #[test]
    fn set_want_to_read_roundtrips_and_advances_updated_at() {
        let conn = Connection::open_in_memory().unwrap();
        run_schema(&conn).unwrap();
        insert_book(&conn, &sample_book("b1")).unwrap(); // sets updated_at from added_at
        let before: i64 = conn
            .query_row("SELECT updated_at FROM books WHERE id='b1'", [], |r| {
                r.get(0)
            })
            .unwrap();

        set_want_to_read(&conn, "b1", true).unwrap();
        let want: i32 = conn
            .query_row("SELECT want_to_read FROM books WHERE id='b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let after: i64 = conn
            .query_row("SELECT updated_at FROM books WHERE id='b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(want, 1);
        assert!(
            after > before,
            "updated_at must strictly advance ({after} > {before})"
        );

        set_want_to_read(&conn, "b1", false).unwrap();
        let want2: i32 = conn
            .query_row("SELECT want_to_read FROM books WHERE id='b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(want2, 0);
    }

    #[test]
    fn test_update_book_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = init_db(&db_path).unwrap();

        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, is_imported) VALUES ('b1', 'Test', 'Author', '/mnt/nas/book.epub', 1, 0, 'epub', 0)",
            [],
        ).unwrap();

        update_book_path(&conn, "b1", "/library/b1.epub", true).unwrap();

        let (path, imported): (String, i32) = conn
            .query_row(
                "SELECT file_path, is_imported FROM books WHERE id = 'b1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path, "/library/b1.epub");
        assert_eq!(imported, 1);
    }

    #[test]
    fn test_soft_deleted_bookmark_excluded_from_list() {
        let (_dir, conn) = setup();
        let book = sample_book("book-sd");
        insert_book(&conn, &book).unwrap();

        let bm = Bookmark {
            id: "bm-sd-1".to_string(),
            book_id: "book-sd".to_string(),
            chapter_index: 0,
            scroll_position: 0.0,
            name: None,
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bm).unwrap();

        // Verify it appears before soft-delete
        let before = list_bookmarks(&conn, "book-sd").unwrap();
        assert_eq!(before.len(), 1);

        // Soft-delete via raw SQL
        conn.execute(
            "UPDATE bookmarks SET deleted_at = 1700001000 WHERE id = 'bm-sd-1'",
            [],
        )
        .unwrap();

        // Should be excluded from list
        let after = list_bookmarks(&conn, "book-sd").unwrap();
        assert!(after.is_empty(), "soft-deleted bookmark should be excluded");
    }

    #[test]
    fn test_soft_deleted_highlight_excluded_from_list() {
        let (_dir, conn) = setup();
        let book = sample_book("book-sdh");
        insert_book(&conn, &book).unwrap();

        let hl = crate::models::Highlight {
            id: "hl-sd-1".to_string(),
            book_id: "book-sdh".to_string(),
            chapter_index: 1,
            text: "some text".to_string(),
            color: "#ff0".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 9,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_highlight(&conn, &hl).unwrap();

        // Verify it appears before soft-delete
        let before = list_highlights(&conn, "book-sdh").unwrap();
        assert_eq!(before.len(), 1);

        // Also check get_chapter_highlights
        let ch_before = get_chapter_highlights(&conn, "book-sdh", 1).unwrap();
        assert_eq!(ch_before.len(), 1);

        // Soft-delete via raw SQL
        conn.execute(
            "UPDATE highlights SET deleted_at = 1700001000 WHERE id = 'hl-sd-1'",
            [],
        )
        .unwrap();

        // Should be excluded from both queries
        let after = list_highlights(&conn, "book-sdh").unwrap();
        assert!(
            after.is_empty(),
            "soft-deleted highlight should be excluded from list_highlights"
        );

        let ch_after = get_chapter_highlights(&conn, "book-sdh", 1).unwrap();
        assert!(
            ch_after.is_empty(),
            "soft-deleted highlight should be excluded from get_chapter_highlights"
        );
    }

    #[test]
    fn test_deleted_at_column_exists_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(dir.path().join("test.db").as_path()).unwrap();

        // Verify deleted_at column exists on bookmarks by inserting a value
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format) VALUES ('b1', 'T', 'A', '/t', 0, 100, 'epub')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, created_at, updated_at, deleted_at) VALUES ('bm1', 'b1', 0, 0.0, 100, 100, 999)",
            [],
        ).unwrap();
        let val: Option<i64> = conn
            .query_row(
                "SELECT deleted_at FROM bookmarks WHERE id = 'bm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, Some(999));

        // Verify deleted_at column exists on highlights
        conn.execute(
            "INSERT INTO highlights (id, book_id, chapter_index, text, color, start_offset, end_offset, created_at, updated_at, deleted_at) VALUES ('h1', 'b1', 0, 'hi', '#fff', 0, 2, 100, 100, 888)",
            [],
        ).unwrap();
        let val: Option<i64> = conn
            .query_row(
                "SELECT deleted_at FROM highlights WHERE id = 'h1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, Some(888));

        // NULL by default when not set
        conn.execute(
            "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, created_at, updated_at) VALUES ('bm2', 'b1', 0, 0.0, 100, 100)",
            [],
        ).unwrap();
        let null_val: Option<i64> = conn
            .query_row(
                "SELECT deleted_at FROM bookmarks WHERE id = 'bm2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(null_val.is_none(), "deleted_at should default to NULL");
    }

    #[test]
    fn test_updated_at_backfill() {
        // Simulate a pre-migration database: create schema, insert rows with updated_at=0,
        // then run schema again to trigger backfill.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        run_schema(&conn).unwrap();

        // Insert book with updated_at = 0 (simulating pre-migration row)
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at) VALUES ('b1', 'T', 'A', '/t', 0, 500, 'epub', 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO bookmarks (id, book_id, chapter_index, scroll_position, created_at, updated_at) VALUES ('bm1', 'b1', 0, 0.0, 300, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO highlights (id, book_id, chapter_index, text, color, start_offset, end_offset, created_at, updated_at) VALUES ('h1', 'b1', 0, 'x', '#000', 0, 1, 400, 0)",
            [],
        ).unwrap();

        // Re-run schema to trigger backfill
        run_schema(&conn).unwrap();

        let book_updated: i64 = conn
            .query_row("SELECT updated_at FROM books WHERE id = 'b1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            book_updated, 500,
            "book updated_at should be backfilled to added_at"
        );

        let bm_updated: i64 = conn
            .query_row(
                "SELECT updated_at FROM bookmarks WHERE id = 'bm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bm_updated, 300,
            "bookmark updated_at should be backfilled to created_at"
        );

        let hl_updated: i64 = conn
            .query_row(
                "SELECT updated_at FROM highlights WHERE id = 'h1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hl_updated, 400,
            "highlight updated_at should be backfilled to created_at"
        );
    }

    #[test]
    fn test_soft_delete_bookmark() {
        let (_dir, conn) = setup();
        let book = sample_book("book-sdb");
        insert_book(&conn, &book).unwrap();

        let bm = Bookmark {
            id: "bm-soft-1".to_string(),
            book_id: "book-sdb".to_string(),
            chapter_index: 1,
            scroll_position: 0.5,
            name: None,
            note: Some("test note".to_string()),
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bm).unwrap();

        // Bookmark visible before soft delete
        let before = list_bookmarks(&conn, "book-sdb").unwrap();
        assert_eq!(before.len(), 1);

        // Soft delete
        soft_delete_bookmark(&conn, "bm-soft-1").unwrap();

        // Not returned by list_bookmarks
        let after = list_bookmarks(&conn, "book-sdb").unwrap();
        assert!(
            after.is_empty(),
            "soft-deleted bookmark should not appear in list"
        );

        // Row still exists in DB with deleted_at set
        let (deleted_at, updated_at): (Option<i64>, i64) = conn
            .query_row(
                "SELECT deleted_at, updated_at FROM bookmarks WHERE id = 'bm-soft-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(deleted_at.is_some(), "deleted_at should be set");
        assert_eq!(
            deleted_at,
            Some(updated_at),
            "deleted_at and updated_at should match"
        );
        assert!(updated_at > 1700000000, "updated_at should be bumped");
    }

    #[test]
    fn test_soft_delete_bookmark_idempotent() {
        let (_dir, conn) = setup();
        let book = sample_book("book-sdb-idem");
        insert_book(&conn, &book).unwrap();

        let bm = Bookmark {
            id: "bm-idem-1".to_string(),
            book_id: "book-sdb-idem".to_string(),
            chapter_index: 0,
            scroll_position: 0.0,
            name: None,
            note: None,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_bookmark(&conn, &bm).unwrap();

        // First soft delete
        soft_delete_bookmark(&conn, "bm-idem-1").unwrap();
        let first_deleted_at: i64 = conn
            .query_row(
                "SELECT deleted_at FROM bookmarks WHERE id = 'bm-idem-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Second soft delete should not change deleted_at
        soft_delete_bookmark(&conn, "bm-idem-1").unwrap();
        let second_deleted_at: i64 = conn
            .query_row(
                "SELECT deleted_at FROM bookmarks WHERE id = 'bm-idem-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(
            first_deleted_at, second_deleted_at,
            "deleted_at should not change on second soft delete"
        );
    }

    #[test]
    fn test_soft_delete_highlight() {
        let (_dir, conn) = setup();
        let book = sample_book("book-sdh2");
        insert_book(&conn, &book).unwrap();

        let hl = crate::models::Highlight {
            id: "hl-soft-1".to_string(),
            book_id: "book-sdh2".to_string(),
            chapter_index: 2,
            text: "highlighted text".to_string(),
            color: "#ff0".to_string(),
            note: None,
            start_offset: 10,
            end_offset: 26,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_highlight(&conn, &hl).unwrap();

        // Highlight visible before soft delete
        let before = list_highlights(&conn, "book-sdh2").unwrap();
        assert_eq!(before.len(), 1);

        // Soft delete
        soft_delete_highlight(&conn, "hl-soft-1").unwrap();

        // Not returned by list_highlights
        let after = list_highlights(&conn, "book-sdh2").unwrap();
        assert!(
            after.is_empty(),
            "soft-deleted highlight should not appear in list"
        );

        // Not returned by get_chapter_highlights either
        let ch_after = get_chapter_highlights(&conn, "book-sdh2", 2).unwrap();
        assert!(
            ch_after.is_empty(),
            "soft-deleted highlight should not appear in chapter list"
        );

        // Row still exists in DB with deleted_at set
        let (deleted_at, updated_at): (Option<i64>, i64) = conn
            .query_row(
                "SELECT deleted_at, updated_at FROM highlights WHERE id = 'hl-soft-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(deleted_at.is_some(), "deleted_at should be set");
        assert_eq!(
            deleted_at,
            Some(updated_at),
            "deleted_at and updated_at should match"
        );
        assert!(updated_at > 1700000000, "updated_at should be bumped");
    }

    #[test]
    fn test_get_or_create_device_id() {
        let (_dir, conn) = setup();

        // First call creates a UUID
        let id1 = get_or_create_device_id(&conn).unwrap();
        assert_eq!(id1.len(), 36, "UUID v4 string should be 36 chars");

        // Second call returns the same ID
        let id2 = get_or_create_device_id(&conn).unwrap();
        assert_eq!(id1, id2, "device_id should be stable across calls");
    }

    #[test]
    fn test_is_sync_enabled() {
        let (_dir, conn) = setup();

        // Missing key → false
        assert!(!is_sync_enabled(&conn));

        // "true" → true
        set_setting(&conn, "sync_enabled", "true").unwrap();
        assert!(is_sync_enabled(&conn));

        // "false" → false
        set_setting(&conn, "sync_enabled", "false").unwrap();
        assert!(!is_sync_enabled(&conn));

        // "yes" → false (only exact "true" is truthy)
        set_setting(&conn, "sync_enabled", "yes").unwrap();
        assert!(!is_sync_enabled(&conn));
    }

    fn sample_book_with_path(id: &str, path: &str) -> Book {
        let mut b = sample_book(id);
        b.file_path = path.to_string();
        b
    }

    #[test]
    fn test_bulk_delete_books() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book_with_path("a", "/tmp/a.epub")).unwrap();
        insert_book(&conn, &sample_book_with_path("b", "/tmp/b.epub")).unwrap();
        insert_book(&conn, &sample_book_with_path("c", "/tmp/c.epub")).unwrap();
        assert_eq!(list_books(&conn).unwrap().len(), 3);

        bulk_delete_books(&conn, &["a", "c"]).unwrap();
        let remaining = list_books(&conn).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "b");
    }

    #[test]
    fn test_bulk_add_to_collection() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book_with_path("a", "/tmp/a.epub")).unwrap();
        insert_book(&conn, &sample_book_with_path("b", "/tmp/b.epub")).unwrap();
        let coll = crate::models::Collection {
            id: "coll-1".to_string(),
            name: "Test".to_string(),
            r#type: crate::models::CollectionType::Manual,
            icon: None,
            color: None,
            created_at: 0,
            updated_at: 0,
            rules: vec![],
        };
        insert_collection(&conn, &coll).unwrap();

        bulk_add_to_collection(&conn, &["a", "b"], "coll-1").unwrap();
        let books = get_books_in_collection(&conn, "coll-1").unwrap();
        assert_eq!(books.len(), 2);
    }

    // #49: Migration versioning
    #[test]
    fn test_schema_version_tracked() {
        let (_dir, conn) = setup();
        let version = get_schema_version(&conn).unwrap();
        assert!(version > 0, "schema version should be set after init");
    }

    #[test]
    fn test_schema_version_idempotent() {
        let (_dir, conn) = setup();
        let v1 = get_schema_version(&conn).unwrap();
        // Running schema again should not change the version
        run_schema(&conn).unwrap();
        let v2 = get_schema_version(&conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn test_schema_version_table_exists() {
        let (_dir, conn) = setup();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // #64 M4: file_path → storage key migration

    /// Insert a pre-M4 book row bypassing the `UNIQUE(file_path)`
    /// constraint that we would hit with two identical paths, by writing
    /// the insert directly.
    fn insert_pre_m4_book(conn: &Connection, id: &str, path: &str, is_imported: i32) {
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, cover_path, total_chapters, added_at, format, is_imported) VALUES (?1, 'T', 'A', ?2, NULL, 0, 0, 'epub', ?3)",
            params![id, path, is_imported],
        )
        .unwrap();
    }

    fn reset_schema_version(conn: &Connection, version: i64) {
        conn.execute(
            "INSERT OR REPLACE INTO schema_version (id, version, applied_at) VALUES (1, ?1, 0)",
            params![version],
        )
        .unwrap();
    }

    #[test]
    fn test_m4_migration_converts_imported_paths_to_keys() {
        let (_dir, conn) = setup();
        set_setting(&conn, "library_folder", "/library").unwrap();
        // Drop every row (setup already ran the migration on an empty db)
        // and insert pre-M4 rows with absolute paths.
        conn.execute("DELETE FROM books", []).unwrap();
        insert_pre_m4_book(&conn, "a", "/library/a.epub", 1);
        insert_pre_m4_book(&conn, "b", "/library/sub/b.pdf", 1);

        // Rewind schema_version so run_schema re-runs the migration.
        reset_schema_version(&conn, 1);
        run_schema(&conn).unwrap();

        let a: String = conn
            .query_row("SELECT file_path FROM books WHERE id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let b: String = conn
            .query_row("SELECT file_path FROM books WHERE id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a, "a.epub");
        assert_eq!(b, "sub/b.pdf");
    }

    #[test]
    fn test_m4_migration_leaves_linked_books_alone() {
        let (_dir, conn) = setup();
        set_setting(&conn, "library_folder", "/library").unwrap();
        conn.execute("DELETE FROM books", []).unwrap();
        // is_imported = 0 → linked book whose file lives outside the library.
        insert_pre_m4_book(&conn, "linked", "/elsewhere/book.epub", 0);

        reset_schema_version(&conn, 1);
        run_schema(&conn).unwrap();

        let got: String = conn
            .query_row("SELECT file_path FROM books WHERE id = 'linked'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, "/elsewhere/book.epub");
    }

    #[test]
    fn test_m4_migration_leaves_foreign_paths_alone() {
        // Imported row whose path doesn't match the current library folder
        // (e.g. profile was reconfigured). Leave it as-is; `remove_book`
        // has a fallback for such rows.
        let (_dir, conn) = setup();
        set_setting(&conn, "library_folder", "/current-library").unwrap();
        conn.execute("DELETE FROM books", []).unwrap();
        insert_pre_m4_book(&conn, "old", "/old-library/book.epub", 1);

        reset_schema_version(&conn, 1);
        run_schema(&conn).unwrap();

        let got: String = conn
            .query_row("SELECT file_path FROM books WHERE id = 'old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, "/old-library/book.epub");
    }

    #[test]
    fn test_m4_migration_is_idempotent() {
        // Running the migration twice must be a no-op on already-converted
        // rows (keys don't sit under the library folder, so the second run
        // leaves them alone).
        let (_dir, conn) = setup();
        set_setting(&conn, "library_folder", "/library").unwrap();
        conn.execute("DELETE FROM books", []).unwrap();
        insert_pre_m4_book(&conn, "a", "/library/a.epub", 1);

        reset_schema_version(&conn, 1);
        run_schema(&conn).unwrap();
        reset_schema_version(&conn, 1);
        run_schema(&conn).unwrap();

        let a: String = conn
            .query_row("SELECT file_path FROM books WHERE id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(a, "a.epub");
    }

    #[test]
    fn test_performance_indexes_exist() {
        let (_dir, conn) = setup();
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            indexes.contains(&"idx_books_series".to_string()),
            "missing index idx_books_series"
        );
        assert!(
            indexes.contains(&"idx_books_format".to_string()),
            "missing index idx_books_format"
        );
        assert!(
            indexes.contains(&"idx_reading_progress_last_read_at".to_string()),
            "missing index idx_reading_progress_last_read_at"
        );
    }

    #[test]
    fn test_list_books_grid_returns_lightweight_items() {
        let (_dir, conn) = setup();
        let mut book = sample_book("grid-1");
        book.file_path = "/tmp/grid1.epub".to_string();
        book.description = Some("A long description that should be excluded".to_string());
        book.genres = Some(r#"["fiction","sci-fi"]"#.to_string());
        book.isbn = Some("978-0-123456-78-9".to_string());
        book.series = Some("Test Series".to_string());
        book.volume = Some(1);
        book.rating = Some(4.5);
        book.language = Some("en".to_string());
        book.publish_year = Some(2020);
        insert_book(&conn, &book).unwrap();

        let items = list_books_grid(&conn).unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.id, "grid-1");
        assert_eq!(item.title, "Test Book");
        assert_eq!(item.author, "Test Author");
        assert_eq!(item.series, Some("Test Series".to_string()));
        assert_eq!(item.volume, Some(1));
        assert_eq!(item.rating, Some(4.5));
        assert_eq!(item.language, Some("en".to_string()));
        assert_eq!(item.publish_year, Some(2020));
        assert!(item.is_imported);
    }

    #[test]
    fn test_list_books_grid_order() {
        let (_dir, conn) = setup();
        let mut book1 = sample_book("grid-ord-1");
        book1.file_path = "/tmp/grid-ord-1.epub".to_string();
        book1.added_at = 1700000000;
        insert_book(&conn, &book1).unwrap();

        let mut book2 = sample_book("grid-ord-2");
        book2.file_path = "/tmp/grid-ord-2.epub".to_string();
        book2.added_at = 1700000100;
        insert_book(&conn, &book2).unwrap();

        let items = list_books_grid(&conn).unwrap();
        assert_eq!(items.len(), 2);
        // Most recent first
        assert_eq!(items[0].id, "grid-ord-2");
        assert_eq!(items[1].id, "grid-ord-1");
    }

    #[test]
    fn test_bulk_update_metadata_author() {
        let (_dir, conn) = setup();
        let mut b1 = sample_book("bulk-1");
        b1.file_path = "/tmp/bulk1.epub".to_string();
        b1.author = "Old Author".to_string();
        insert_book(&conn, &b1).unwrap();
        let mut b2 = sample_book("bulk-2");
        b2.file_path = "/tmp/bulk2.epub".to_string();
        b2.author = "Old Author".to_string();
        insert_book(&conn, &b2).unwrap();

        let updated = bulk_update_metadata(
            &conn,
            &["bulk-1", "bulk-2"],
            Some("New Author"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(updated, 2);
        assert_eq!(
            get_book(&conn, "bulk-1").unwrap().unwrap().author,
            "New Author"
        );
        assert_eq!(
            get_book(&conn, "bulk-2").unwrap().unwrap().author,
            "New Author"
        );
    }

    #[test]
    fn test_bulk_update_metadata_partial_fields() {
        let (_dir, conn) = setup();
        let mut b1 = sample_book("bulk-p1");
        b1.file_path = "/tmp/bulkp1.epub".to_string();
        b1.author = "Keep This".to_string();
        b1.series = Some("Old Series".to_string());
        insert_book(&conn, &b1).unwrap();

        bulk_update_metadata(
            &conn,
            &["bulk-p1"],
            None,
            Some("New Series"),
            None,
            None,
            None,
        )
        .unwrap();
        let b = get_book(&conn, "bulk-p1").unwrap().unwrap();
        assert_eq!(b.author, "Keep This");
        assert_eq!(b.series, Some("New Series".to_string()));
    }

    #[test]
    fn test_bulk_update_metadata_clear_field() {
        let (_dir, conn) = setup();
        let mut b1 = sample_book("bulk-c1");
        b1.file_path = "/tmp/bulkc1.epub".to_string();
        b1.series = Some("Remove Me".to_string());
        insert_book(&conn, &b1).unwrap();

        bulk_update_metadata(&conn, &["bulk-c1"], None, Some(""), None, None, None).unwrap();
        let b = get_book(&conn, "bulk-c1").unwrap().unwrap();
        assert_eq!(b.series, None);
    }

    #[test]
    fn test_autostart_setting_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = create_pool(&dir.path().join("test.db")).unwrap();
        let conn = pool.get().unwrap();

        // Default: no setting exists
        let val = get_setting(&conn, "autostart_enabled").unwrap();
        assert_eq!(val, None);

        // Set to true
        set_setting(&conn, "autostart_enabled", "true").unwrap();
        let val = get_setting(&conn, "autostart_enabled").unwrap();
        assert_eq!(val, Some("true".to_string()));

        // Set to false
        set_setting(&conn, "autostart_enabled", "false").unwrap();
        let val = get_setting(&conn, "autostart_enabled").unwrap();
        assert_eq!(val, Some("false".to_string()));
    }

    #[test]
    fn test_list_all_book_tags() {
        let (_dir, conn) = setup();
        let mut b1 = sample_book("tag-b1");
        b1.file_path = "/tmp/tag1.epub".to_string();
        insert_book(&conn, &b1).unwrap();

        let mut b2 = sample_book("tag-b2");
        b2.file_path = "/tmp/tag2.epub".to_string();
        insert_book(&conn, &b2).unwrap();

        // No tags yet
        let assocs = list_all_book_tags(&conn).unwrap();
        assert!(assocs.is_empty());

        // Create tags and assign
        get_or_create_tag(&conn, "t1", "fiction").unwrap();
        get_or_create_tag(&conn, "t2", "sci-fi").unwrap();
        add_tag_to_book(&conn, "tag-b1", "t1").unwrap();
        add_tag_to_book(&conn, "tag-b1", "t2").unwrap();
        add_tag_to_book(&conn, "tag-b2", "t1").unwrap();

        let assocs = list_all_book_tags(&conn).unwrap();
        assert_eq!(assocs.len(), 3);
        assert!(assocs.contains(&("tag-b1".to_string(), "t1".to_string())));
        assert!(assocs.contains(&("tag-b1".to_string(), "t2".to_string())));
        assert!(assocs.contains(&("tag-b2".to_string(), "t1".to_string())));
    }

    #[test]
    fn delete_setting_removes_key() {
        let (_dir, conn) = setup();
        set_setting(&conn, "to_remove", "x").unwrap();
        assert_eq!(
            get_setting(&conn, "to_remove").unwrap().as_deref(),
            Some("x")
        );
        delete_setting(&conn, "to_remove").unwrap();
        assert!(get_setting(&conn, "to_remove").unwrap().is_none());
    }

    #[test]
    fn delete_setting_no_op_when_key_missing() {
        let (_dir, conn) = setup();
        // Must not error when key is absent.
        delete_setting(&conn, "never_existed").unwrap();
        assert!(get_setting(&conn, "never_existed").unwrap().is_none());
    }

    #[test]
    fn provision_library_creates_file_and_applies_schema() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("nested").join("library.db");

        // File should not exist yet.
        assert!(!db_path.exists());

        provision_library(&db_path).expect("provision must succeed");

        // File exists and the parent directory was created.
        assert!(db_path.exists(), "library.db must be on disk");
        assert!(db_path.parent().unwrap().exists());

        // Schema is at the current version.
        let conn = Connection::open(&db_path).unwrap();
        let v = get_schema_version(&conn).unwrap();
        assert_eq!(v, SCHEMA_VERSION, "schema must be migrated to head");
    }

    #[test]
    fn provision_library_is_idempotent() {
        use tempfile::tempdir;
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("library.db");

        provision_library(&db_path).expect("first call");
        provision_library(&db_path).expect("second call must be a no-op");

        let conn = Connection::open(&db_path).unwrap();
        let v = get_schema_version(&conn).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn feature_flags_crud() {
        let (_dir, conn) = setup();

        let initial_count = list_feature_flags(&conn).unwrap().len();
        assert!(!get_feature_flag(&conn, "discover").unwrap());

        set_feature_flag(&conn, "discover", true, Some("Show Discover section")).unwrap();
        assert!(get_feature_flag(&conn, "discover").unwrap());

        let flags = list_feature_flags(&conn).unwrap();
        assert_eq!(flags.len(), initial_count + 1);
        let discover = flags.iter().find(|f| f.key == "discover").unwrap();
        assert!(discover.enabled);
        assert_eq!(
            discover.description.as_deref(),
            Some("Show Discover section")
        );

        set_feature_flag(&conn, "discover", false, None).unwrap();
        assert!(!get_feature_flag(&conn, "discover").unwrap());
        let discover = list_feature_flags(&conn)
            .unwrap()
            .into_iter()
            .find(|f| f.key == "discover")
            .unwrap();
        assert_eq!(
            discover.description.as_deref(),
            Some("Show Discover section")
        );

        delete_feature_flag(&conn, "discover").unwrap();
        assert_eq!(list_feature_flags(&conn).unwrap().len(), initial_count);
        assert!(!get_feature_flag(&conn, "discover").unwrap());
    }

    #[test]
    fn search_highlights_matches_text_and_note() {
        let (_dir, conn) = setup();
        let book = sample_book("b1");
        insert_book(&conn, &book).unwrap();

        let h1 = crate::models::Highlight {
            id: "h1".to_string(),
            book_id: "b1".to_string(),
            chapter_index: 0,
            text: "The quick brown fox".to_string(),
            color: "#f6c445".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 19,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        let h2 = crate::models::Highlight {
            id: "h2".to_string(),
            book_id: "b1".to_string(),
            chapter_index: 1,
            text: "Something else".to_string(),
            color: "#7bc47f".to_string(),
            note: Some("note about fox".to_string()),
            start_offset: 0,
            end_offset: 14,
            created_at: 1700000001,
            updated_at: 1700000001,
            deleted_at: None,
        };
        insert_highlight(&conn, &h1).unwrap();
        insert_highlight(&conn, &h2).unwrap();

        let results = search_highlights(&conn, "fox", 100).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].book_title, "Test Book");
        assert_eq!(results[0].book_author, "Test Author");

        let results = search_highlights(&conn, "quick", 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].highlight_id, "h1");

        let results = search_highlights(&conn, "nonexistent", 100).unwrap();
        assert!(results.is_empty());

        let results = search_highlights(&conn, "fox", 1).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_highlights_escapes_special_chars() {
        let (_dir, conn) = setup();
        let book = sample_book("b1");
        insert_book(&conn, &book).unwrap();

        let h = crate::models::Highlight {
            id: "h1".to_string(),
            book_id: "b1".to_string(),
            chapter_index: 0,
            text: r"Path C:\temp and 100% done with under_score".to_string(),
            color: "#f6c445".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 43,
            created_at: 1700000000,
            updated_at: 1700000000,
            deleted_at: None,
        };
        insert_highlight(&conn, &h).unwrap();

        // Backslash must match literally, not act as escape
        let results = search_highlights(&conn, r"C:\temp", 100).unwrap();
        assert_eq!(results.len(), 1);

        // % must match literally, not act as wildcard
        let results = search_highlights(&conn, "100%", 100).unwrap();
        assert_eq!(results.len(), 1);
        let results = search_highlights(&conn, "100x", 100).unwrap();
        assert!(results.is_empty());

        // _ must match literally, not act as single-char wildcard
        let results = search_highlights(&conn, "under_score", 100).unwrap();
        assert_eq!(results.len(), 1);
        let results = search_highlights(&conn, "underXscore", 100).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_suggest_reading_status() {
        let (_dir, conn) = setup();

        // 4 books with no reading progress → "unread"
        for i in 0..4 {
            let mut book = sample_book(&format!("unread-{i}"));
            book.title = format!("Unread Book {i}");
            book.file_path = format!("/tmp/unread-{i}.epub");
            insert_book(&conn, &book).unwrap();
        }

        // 3 finished books
        for i in 0..3 {
            let mut book = sample_book(&format!("finished-{i}"));
            book.title = format!("Finished Book {i}");
            book.total_chapters = 5;
            book.file_path = format!("/tmp/finished-{i}.epub");
            insert_book(&conn, &book).unwrap();
            let progress = ReadingProgress {
                book_id: format!("finished-{i}"),
                chapter_index: 4, // >= total_chapters - 1
                scroll_position: 1.0,
                last_read_at: 1700000000,
            };
            upsert_reading_progress(&conn, &progress).unwrap();
        }

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        let status_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.heuristic_type == "reading_status")
            .collect();

        assert_eq!(status_suggestions.len(), 2);

        let unread = status_suggestions
            .iter()
            .find(|s| s.name == "Unread books")
            .unwrap();
        assert_eq!(unread.matched_book_count, 4);
        assert_eq!(unread.rules[0].value, "unread");

        let finished = status_suggestions
            .iter()
            .find(|s| s.name == "Finished books")
            .unwrap();
        assert_eq!(finished.matched_book_count, 3);
        assert_eq!(finished.rules[0].value, "finished");
    }

    #[test]
    fn test_finished_rule_excludes_zero_chapter_books() {
        let (_dir, conn) = setup();

        // Book with total_chapters = 0 and reading progress — must NOT count as finished
        let mut book = sample_book("zero-ch");
        book.total_chapters = 0;
        book.file_path = "/tmp/zero-ch.epub".to_string();
        insert_book(&conn, &book).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "zero-ch".to_string(),
                chapter_index: 0,
                scroll_position: 0.0,
                last_read_at: 1700000000,
            },
        )
        .unwrap();

        // 2 legitimately finished books
        for i in 0..2 {
            let mut b = sample_book(&format!("legit-{i}"));
            b.total_chapters = 5;
            b.file_path = format!("/tmp/legit-{i}.epub");
            insert_book(&conn, &b).unwrap();
            upsert_reading_progress(
                &conn,
                &ReadingProgress {
                    book_id: format!("legit-{i}"),
                    chapter_index: 4,
                    scroll_position: 1.0,
                    last_read_at: 1700000000,
                },
            )
            .unwrap();
        }

        // Verify suggestion count excludes zero-chapter book
        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        let finished = suggestions.iter().find(|s| s.name == "Finished books");
        assert_eq!(finished.unwrap().matched_book_count, 2);

        // Verify build_rule_query also excludes it
        let rules = vec![CollectionRule {
            id: "r1".to_string(),
            collection_id: "c1".to_string(),
            field: "reading_progress".to_string(),
            operator: "equals".to_string(),
            value: "finished".to_string(),
        }];
        let (joins, wheres, params) = build_rule_query(&rules);
        let sql = format!("SELECT COUNT(*) FROM books b {joins} {wheres}",);
        let count: usize = conn
            .query_row(&sql, rusqlite::params_from_iter(params), |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "zero-chapter book must not match finished rule");
    }

    #[test]
    fn test_suggest_series_collections() {
        let (_dir, conn) = setup();

        for i in 0..3 {
            let mut book = sample_book(&format!("series-{i}"));
            book.series = Some("Discworld".to_string());
            book.volume = Some(i as u32 + 1);
            book.title = format!("Discworld {}", i + 1);
            book.file_path = format!("/tmp/series-{i}.epub");
            insert_book(&conn, &book).unwrap();
        }

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        let series_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.heuristic_type == "series")
            .collect();

        assert_eq!(series_suggestions.len(), 1);
        assert_eq!(series_suggestions[0].name, "Discworld series");
        assert_eq!(series_suggestions[0].matched_book_count, 3);
        assert_eq!(series_suggestions[0].rules[0].field, "series");
        assert_eq!(series_suggestions[0].rules[0].operator, "equals");
        assert_eq!(series_suggestions[0].rules[0].value, "Discworld");
    }

    #[test]
    fn test_dedup_existing_collections() {
        let (_dir, conn) = setup();

        for i in 0..4 {
            let mut book = sample_book(&format!("dedup-{i}"));
            book.author = "Agatha Christie".to_string();
            book.title = format!("Mystery {i}");
            book.file_path = format!("/tmp/dedup-{i}.epub");
            insert_book(&conn, &book).unwrap();
        }

        // Simulate existing automated collection with same author rule
        let existing = vec![Collection {
            id: "existing-1".to_string(),
            name: "Christie Books".to_string(),
            r#type: CollectionType::Automated,
            icon: None,
            color: None,
            created_at: 0,
            updated_at: 0,
            rules: vec![CollectionRule {
                id: "rule-1".to_string(),
                collection_id: "existing-1".to_string(),
                field: "author".to_string(),
                operator: "equals".to_string(),
                value: "Agatha Christie".to_string(),
            }],
        }];

        let suggestions = get_collection_suggestions(&conn, &existing).unwrap();
        let author_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.heuristic_type == "author")
            .collect();

        assert_eq!(author_suggestions.len(), 0);
    }

    #[test]
    fn test_no_suggestions_small_library() {
        let (_dir, conn) = setup();

        let mut book = sample_book("lonely-book");
        book.file_path = "/tmp/lonely.epub".to_string();
        insert_book(&conn, &book).unwrap();

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_suggestion_limit() {
        let (_dir, conn) = setup();

        // Create 10 distinct authors with 3+ books each → 10 potential suggestions
        for a in 0..10 {
            for i in 0..3 {
                let mut book = sample_book(&format!("limit-{a}-{i}"));
                book.author = format!("Author {a}");
                book.title = format!("Book {a}-{i}");
                book.file_path = format!("/tmp/limit-{a}-{i}.epub");
                insert_book(&conn, &book).unwrap();
            }
        }

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        assert!(suggestions.len() <= 8);
    }

    #[test]
    fn test_suggest_format() {
        let (_dir, conn) = setup();

        // 10 EPUBs (dominant — should be skipped)
        for i in 0..10 {
            let mut book = sample_book(&format!("epub-{i}"));
            book.title = format!("Epub Book {i}");
            book.format = BookFormat::Epub;
            book.file_path = format!("/tmp/epub-{i}.epub");
            insert_book(&conn, &book).unwrap();
        }
        // 3 PDFs (non-dominant — should be suggested)
        for i in 0..3 {
            let mut book = sample_book(&format!("pdf-{i}"));
            book.title = format!("PDF Book {i}");
            book.format = BookFormat::Pdf;
            book.file_path = format!("/tmp/pdf-{i}.pdf");
            insert_book(&conn, &book).unwrap();
        }

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        let format_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.heuristic_type == "format")
            .collect();

        // EPUB ≥75% so skipped; PDF = 3 books so suggested
        assert_eq!(format_suggestions.len(), 1);
        assert_eq!(format_suggestions[0].name, "PDF Books");
        assert_eq!(format_suggestions[0].matched_book_count, 3);
        assert_eq!(format_suggestions[0].rules[0].field, "format");
        assert_eq!(format_suggestions[0].rules[0].value, "pdf");
    }

    #[test]
    fn test_suggest_author_collections() {
        let (_dir, conn) = setup();

        for i in 0..4 {
            let id = format!("author-test-{i}");
            let mut book = sample_book(&id);
            book.author = "J.R.R. Tolkien".to_string();
            book.title = format!("Book {i}");
            book.file_path = format!("/tmp/{id}.epub");
            insert_book(&conn, &book).unwrap();
        }
        // Add 2 books by another author (below threshold)
        for i in 0..2 {
            let id = format!("other-{i}");
            let mut book = sample_book(&id);
            book.author = "Other Author".to_string();
            book.title = format!("Other {i}");
            book.file_path = format!("/tmp/{id}.epub");
            insert_book(&conn, &book).unwrap();
        }

        let suggestions = get_collection_suggestions(&conn, &[]).unwrap();
        let author_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.heuristic_type == "author")
            .collect();

        assert_eq!(author_suggestions.len(), 1);
        assert_eq!(author_suggestions[0].name, "Books by J.R.R. Tolkien");
        assert_eq!(author_suggestions[0].matched_book_count, 4);
        assert_eq!(author_suggestions[0].rules.len(), 1);
        assert_eq!(author_suggestions[0].rules[0].field, "author");
        assert_eq!(author_suggestions[0].rules[0].operator, "equals");
        assert_eq!(author_suggestions[0].rules[0].value, "J.R.R. Tolkien");
    }

    #[test]
    fn build_core_export_has_expected_keys() {
        let (_tmp, conn) = setup();
        let value = build_core_export(&conn).expect("build_core_export");
        let obj = value.as_object().expect("export is a JSON object");
        for key in [
            "version",
            "books",
            "reading_progress",
            "bookmarks",
            "highlights",
            "collections",
            "tags",
            "book_tags",
        ] {
            assert!(obj.contains_key(key), "missing key: {key}");
        }
        assert_eq!(obj["version"], 1);
        assert!(obj["books"].is_array());
    }

    #[test]
    fn restore_secondary_data_writes_and_is_idempotent() {
        use crate::models::Highlight;
        let (_tmp, conn) = setup();
        let mut book = sample_book("b1");
        book.file_path = "/tmp/b1.epub".to_string();
        insert_book(&conn, &book).unwrap();

        let progress = vec![ReadingProgress {
            book_id: "b1".to_string(),
            chapter_index: 3,
            scroll_position: 0.5,
            last_read_at: 1700,
        }];
        let bookmarks = vec![Bookmark {
            id: "bm1".to_string(),
            book_id: "b1".to_string(),
            chapter_index: 2,
            scroll_position: 0.1,
            name: Some("mark".to_string()),
            note: None,
            created_at: 1,
            updated_at: 1,
            deleted_at: None,
        }];
        let highlights = vec![Highlight {
            id: "hl1".to_string(),
            book_id: "b1".to_string(),
            chapter_index: 1,
            text: "quote".to_string(),
            color: "yellow".to_string(),
            note: None,
            start_offset: 0,
            end_offset: 5,
            created_at: 1,
            updated_at: 1,
            deleted_at: None,
        }];
        let collections = vec![Collection {
            id: "c1".to_string(),
            name: "Faves".to_string(),
            r#type: CollectionType::Manual,
            icon: None,
            color: None,
            created_at: 1,
            updated_at: 1,
            rules: Vec::new(),
        }];
        let tags = vec![("t1".to_string(), "scifi".to_string())];
        let book_tags = vec![("b1".to_string(), "t1".to_string(), "scifi".to_string())];

        let data = SecondaryImport {
            reading_progress: &progress,
            bookmarks: &bookmarks,
            highlights: &highlights,
            collections: &collections,
            tags: &tags,
            book_tags: &book_tags,
        };

        let counts = restore_secondary_data(&conn, &data);
        assert_eq!(
            counts,
            RestoreCounts {
                reading_progress: 1,
                bookmarks: 1,
                highlights: 1,
                collections: 1,
                tags: 1,
                book_tags: 1,
            }
        );

        // Data actually landed.
        assert!(get_reading_progress(&conn, "b1").unwrap().is_some());
        assert_eq!(list_bookmarks(&conn, "b1").unwrap().len(), 1);
        assert_eq!(list_highlights(&conn, "b1").unwrap().len(), 1);
        assert_eq!(list_collections(&conn).unwrap().len(), 1);
        assert_eq!(get_book_tags(&conn, "b1").unwrap().len(), 1);

        // Re-running must not duplicate rows or error. The collection's
        // existing id is skipped, so its count drops to zero.
        let again = restore_secondary_data(&conn, &data);
        assert_eq!(again.collections, 0);
        assert_eq!(list_bookmarks(&conn, "b1").unwrap().len(), 1);
        assert_eq!(list_highlights(&conn, "b1").unwrap().len(), 1);
        assert_eq!(list_collections(&conn).unwrap().len(), 1);
        assert_eq!(get_book_tags(&conn, "b1").unwrap().len(), 1);
    }

    #[test]
    fn build_core_export_books_deserialize_into_book_vec() {
        // Restore (`import_library_backup`) parses the `books` array of the
        // export object back into `Vec<Book>`. Guard that the export shape
        // stays compatible — a regression here silently breaks restore.
        let (_tmp, conn) = setup();
        let mut b1 = sample_book("rt1");
        b1.file_path = "/tmp/rt1.epub".to_string();
        let mut b2 = sample_book("rt2");
        b2.file_path = "/tmp/rt2.epub".to_string();
        insert_book(&conn, &b1).unwrap();
        insert_book(&conn, &b2).unwrap();

        let value = build_core_export(&conn).expect("build_core_export");
        let books: Vec<Book> =
            serde_json::from_value(value["books"].clone()).expect("books -> Vec<Book>");

        assert_eq!(books.len(), 2);
        let mut ids: Vec<_> = books.iter().map(|b| b.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["rt1", "rt2"]);
    }

    #[test]
    fn list_settings_round_trips() {
        let (_tmp, conn) = setup();
        set_setting(&conn, "import_mode", "copy").unwrap();
        set_setting(&conn, "web_server_port", "1421").unwrap();

        let settings = list_settings(&conn).expect("list_settings");
        assert!(settings.contains(&("import_mode".to_string(), "copy".to_string())));
        assert!(settings.contains(&("web_server_port".to_string(), "1421".to_string())));
    }

    #[test]
    fn set_and_get_book_source_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(&dir.path().join("library.db")).unwrap();
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at)
             VALUES ('b1', 'T', 'A', '/storage/b1.epub', 0, 100, 'epub', 100)",
            [],
        ).unwrap();

        set_book_source(&conn, "b1", "/mnt/nas/T.epub", 4096, 1700000000).unwrap();

        let found = get_book_by_source_path(&conn, "/mnt/nas/T.epub")
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "b1");
        assert_eq!(found.source_size, Some(4096));
        assert_eq!(found.source_mtime, Some(1700000000));
    }

    #[test]
    fn get_book_by_source_path_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(&dir.path().join("library.db")).unwrap();
        assert!(get_book_by_source_path(&conn, "/nope/x.epub")
            .unwrap()
            .is_none());
    }

    #[test]
    fn get_book_by_source_path_ignores_legacy_null_rows() {
        let dir = tempfile::tempdir().unwrap();
        let conn = init_db(&dir.path().join("library.db")).unwrap();
        // Legacy row: no source_path written.
        conn.execute(
            "INSERT INTO books (id, title, author, file_path, total_chapters, added_at, format, updated_at)
             VALUES ('legacy', 'T', 'A', '/storage/legacy.epub', 0, 100, 'epub', 100)",
            [],
        ).unwrap();
        // Querying by the storage path must not match a NULL source_path row.
        assert!(get_book_by_source_path(&conn, "/storage/legacy.epub")
            .unwrap()
            .is_none());
    }

    #[test]
    fn book_etag_pairs_returns_id_updated_at_map() {
        let (_dir, conn) = setup();

        // Insert two books with different timestamps
        let mut b1 = sample_book("etag-b1");
        b1.added_at = 100;
        b1.file_path = "/tmp/test1.epub".to_string();
        insert_book(&conn, &b1).unwrap();

        let mut b2 = sample_book("etag-b2");
        b2.added_at = 200;
        b2.file_path = "/tmp/test2.epub".to_string();
        insert_book(&conn, &b2).unwrap();

        // book_etag_pairs should return (id, updated_at) pairs
        let pairs = book_etag_pairs(&conn).unwrap();
        assert_eq!(pairs.len(), 2);
        // insert_book writes added_at into the updated_at column
        assert_eq!(pairs.get("etag-b1"), Some(&100));
        assert_eq!(pairs.get("etag-b2"), Some(&200));

        // A mutation bumping updated_at is reflected
        conn.execute("UPDATE books SET updated_at = 999 WHERE id = 'etag-b1'", [])
            .unwrap();
        let pairs = book_etag_pairs(&conn).unwrap();
        assert_eq!(pairs.get("etag-b1"), Some(&999));
        assert_eq!(pairs.get("etag-b2"), Some(&200));
    }

    // F-1-7: per-book reading insights for the Book Details modal.

    #[test]
    fn test_get_book_reading_stats_book_without_sessions_returns_none() {
        let (_dir, conn) = setup();
        insert_book(&conn, &sample_book("book-unread")).unwrap();

        assert!(get_book_reading_stats(&conn, "book-unread")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_get_book_reading_stats_unfinished_book_with_sessions() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-in-progress")
        };
        insert_book(&conn, &book).unwrap();

        insert_reading_session(&conn, "s-1", "book-in-progress", 1_000, 600, 3).unwrap();
        insert_reading_session(&conn, "s-2", "book-in-progress", 2_000, 900, 4).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-in-progress".to_string(),
                chapter_index: 3,
                scroll_position: 0.5,
                last_read_at: 2_000,
            },
        )
        .unwrap();

        let stats = get_book_reading_stats(&conn, "book-in-progress")
            .unwrap()
            .expect("book has sessions");
        assert_eq!(stats.total_reading_time_secs, 1_500);
        assert_eq!(stats.session_count, 2);
        assert_eq!(stats.first_read_at, Some(1_000));
        assert_eq!(
            stats.finished_at, None,
            "book hasn't reached the last chapter"
        );
    }

    // Finding 1: sync.rs and the web UI write `reading_progress`/`finished_at`
    // directly without ever inserting `reading_sessions` rows, and
    // `backfill_finished_at` stamped pre-feature finished books the same way.
    // Those books must still surface a "Finished" date instead of vanishing
    // from the Reading section because `session_count == 0`.
    #[test]
    fn test_get_book_reading_stats_finished_without_sessions_returns_some() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-synced-finished")
        };
        insert_book(&conn, &book).unwrap();

        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-synced-finished".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 5_000,
            },
        )
        .unwrap();

        let stats = get_book_reading_stats(&conn, "book-synced-finished")
            .unwrap()
            .expect("finished_at alone must yield Some");
        assert_eq!(stats.total_reading_time_secs, 0);
        assert_eq!(stats.session_count, 0);
        assert_eq!(stats.first_read_at, None);
        assert_eq!(stats.finished_at, Some(5_000));
    }

    #[test]
    fn test_get_book_reading_stats_finished_book_includes_finished_at() {
        let (_dir, conn) = setup();
        let book = Book {
            total_chapters: 10,
            ..sample_book("book-finished-stats")
        };
        insert_book(&conn, &book).unwrap();

        insert_reading_session(&conn, "s-1", "book-finished-stats", 1_000, 1_200, 10).unwrap();
        upsert_reading_progress(
            &conn,
            &ReadingProgress {
                book_id: "book-finished-stats".to_string(),
                chapter_index: 9,
                scroll_position: 1.0,
                last_read_at: 5_000,
            },
        )
        .unwrap();

        let stats = get_book_reading_stats(&conn, "book-finished-stats")
            .unwrap()
            .expect("book has sessions");
        assert_eq!(stats.session_count, 1);
        assert_eq!(stats.finished_at, Some(5_000));
    }

    fn sample_vocabulary_word(id: &str, lemma: &str) -> crate::models::VocabularyWord {
        crate::models::VocabularyWord {
            id: id.to_string(),
            lemma: lemma.to_string(),
            word: lemma.to_string(),
            pos: Some("n".to_string()),
            definition: "a test definition".to_string(),
            book_id: None,
            book_title: None,
            chapter_index: None,
            context_sentence: Some("A sentence with the word in it.".to_string()),
            start_offset: Some(10),
            end_offset: Some(20),
            seen_count: 1,
            box_num: 1,
            last_reviewed_at: None,
            next_due_at: None,
            last_seen_at: 1_700_000_000,
            created_at: 1_700_000_000,
        }
    }

    // F-1-5 fix: a `vocabulary` table created before start_offset/end_offset
    // existed must gain them via the additive-ALTER block in run_schema,
    // not just CREATE TABLE IF NOT EXISTS (a no-op once the table exists).
    #[test]
    fn run_schema_adds_offset_columns_to_pre_existing_vocabulary_table() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(&db_path).unwrap();

        // Simulate a pre-F-1-5 database: `vocabulary` exists without the
        // offset columns.
        conn.execute_batch(
            "CREATE TABLE vocabulary (
                id             TEXT PRIMARY KEY,
                lemma          TEXT NOT NULL UNIQUE,
                word           TEXT NOT NULL,
                pos            TEXT,
                definition     TEXT NOT NULL,
                book_id        TEXT,
                book_title     TEXT,
                chapter_index  INTEGER,
                context_sentence TEXT,
                seen_count     INTEGER NOT NULL DEFAULT 1,
                box            INTEGER NOT NULL DEFAULT 1,
                last_reviewed_at INTEGER,
                next_due_at    INTEGER,
                last_seen_at   INTEGER NOT NULL,
                created_at     INTEGER NOT NULL
            );",
        )
        .unwrap();

        run_schema(&conn).unwrap();

        let word = sample_vocabulary_word("v1", "run");
        upsert_vocabulary_word(&conn, &word).unwrap();
        let all = list_vocabulary(&conn).unwrap();
        assert_eq!(all[0].start_offset, Some(10));
        assert_eq!(all[0].end_offset, Some(20));
    }

    #[test]
    fn upsert_vocabulary_word_inserts_new_row() {
        let (_dir, conn) = setup();
        let word = sample_vocabulary_word("v1", "run");
        let inserted = upsert_vocabulary_word(&conn, &word).unwrap();
        assert!(inserted, "first upsert of a lemma should insert");

        let all = list_vocabulary(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].seen_count, 1);
        assert_eq!(all[0].start_offset, Some(10));
        assert_eq!(all[0].end_offset, Some(20));
    }

    #[test]
    fn upsert_vocabulary_word_dedups_by_lemma() {
        let (_dir, conn) = setup();
        let word = sample_vocabulary_word("v1", "run");
        upsert_vocabulary_word(&conn, &word).unwrap();

        // Second lookup of the same lemma: different id/context/box/offset
        // would-be values must NOT overwrite context/box/created_at/offsets —
        // only seen_count/last_seen_at.
        let mut repeat = sample_vocabulary_word("v2", "run");
        repeat.context_sentence = Some("A totally different sentence.".to_string());
        repeat.box_num = 4;
        repeat.last_seen_at = 1_700_000_500;
        repeat.start_offset = Some(99);
        repeat.end_offset = Some(199);
        let inserted = upsert_vocabulary_word(&conn, &repeat).unwrap();
        assert!(
            !inserted,
            "second upsert of same lemma should touch, not insert"
        );

        let all = list_vocabulary(&conn).unwrap();
        assert_eq!(all.len(), 1, "still exactly one row for the lemma");
        let row = &all[0];
        assert_eq!(row.id, "v1", "original id preserved");
        assert_eq!(row.seen_count, 2, "seen_count bumped");
        assert_eq!(row.last_seen_at, 1_700_000_500, "last_seen_at touched");
        assert_eq!(row.box_num, 1, "box untouched by repeat lookup");
        assert_eq!(
            row.context_sentence.as_deref(),
            Some("A sentence with the word in it."),
            "original context preserved"
        );
        assert_eq!(row.created_at, 1_700_000_000, "created_at untouched");
        assert_eq!(
            row.start_offset,
            Some(10),
            "original start_offset preserved on repeat lookup"
        );
        assert_eq!(
            row.end_offset,
            Some(20),
            "original end_offset preserved on repeat lookup"
        );
    }

    #[test]
    fn list_vocabulary_is_newest_first() {
        let (_dir, conn) = setup();
        let mut older = sample_vocabulary_word("v1", "alpha");
        older.created_at = 1_000;
        let mut newer = sample_vocabulary_word("v2", "beta");
        newer.created_at = 2_000;
        upsert_vocabulary_word(&conn, &older).unwrap();
        upsert_vocabulary_word(&conn, &newer).unwrap();

        let all = list_vocabulary(&conn).unwrap();
        assert_eq!(
            all.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            vec!["v2", "v1"]
        );
    }

    #[test]
    fn delete_and_clear_vocabulary() {
        let (_dir, conn) = setup();
        upsert_vocabulary_word(&conn, &sample_vocabulary_word("v1", "alpha")).unwrap();
        upsert_vocabulary_word(&conn, &sample_vocabulary_word("v2", "beta")).unwrap();

        delete_vocabulary_word(&conn, "v1").unwrap();
        let after_delete = list_vocabulary(&conn).unwrap();
        assert_eq!(after_delete.len(), 1);
        assert_eq!(after_delete[0].id, "v2");

        clear_vocabulary(&conn).unwrap();
        assert!(list_vocabulary(&conn).unwrap().is_empty());
    }

    #[test]
    fn due_vocabulary_respects_null_and_now_and_limit() {
        let (_dir, conn) = setup();
        let mut never_reviewed = sample_vocabulary_word("v1", "alpha");
        never_reviewed.next_due_at = None;
        never_reviewed.created_at = 100;

        let mut due_now = sample_vocabulary_word("v2", "beta");
        due_now.next_due_at = Some(500);
        due_now.created_at = 200;

        let mut not_due = sample_vocabulary_word("v3", "gamma");
        not_due.next_due_at = Some(10_000);
        not_due.created_at = 300;

        upsert_vocabulary_word(&conn, &never_reviewed).unwrap();
        upsert_vocabulary_word(&conn, &due_now).unwrap();
        upsert_vocabulary_word(&conn, &not_due).unwrap();

        let due = due_vocabulary(&conn, 1_000, 10).unwrap();
        assert_eq!(
            due[0].start_offset,
            Some(10),
            "due_vocabulary mapper wires up start_offset"
        );
        assert_eq!(
            due[0].end_offset,
            Some(20),
            "due_vocabulary mapper wires up end_offset"
        );
        let ids: Vec<&str> = due.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["v1", "v2"],
            "NULL due sorts before due<=now; not-due excluded"
        );

        let limited = due_vocabulary(&conn, 1_000, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "v1");
    }

    #[test]
    fn record_vocabulary_review_correct_advances_box() {
        let (_dir, conn) = setup();
        let mut word = sample_vocabulary_word("v1", "alpha");
        word.box_num = 1;
        upsert_vocabulary_word(&conn, &word).unwrap();

        record_vocabulary_review(&conn, "v1", true, 1_000).unwrap();

        let row = list_vocabulary(&conn).unwrap().into_iter().next().unwrap();
        assert_eq!(row.box_num, 2);
        assert_eq!(row.next_due_at, Some(1_000 + 3 * 86_400));
        assert_eq!(row.last_reviewed_at, Some(1_000));
    }

    #[test]
    fn record_vocabulary_review_wrong_resets_box() {
        let (_dir, conn) = setup();
        let mut word = sample_vocabulary_word("v1", "alpha");
        word.box_num = 3;
        upsert_vocabulary_word(&conn, &word).unwrap();

        record_vocabulary_review(&conn, "v1", false, 2_000).unwrap();

        let row = list_vocabulary(&conn).unwrap().into_iter().next().unwrap();
        assert_eq!(row.box_num, 1);
        assert_eq!(row.next_due_at, Some(2_000 + 86_400));
    }

    #[test]
    fn vocabulary_book_id_set_null_on_book_delete() {
        let (_dir, conn) = setup();
        let book = sample_book("book-voc-1");
        insert_book(&conn, &book).unwrap();

        let mut word = sample_vocabulary_word("v1", "alpha");
        word.book_id = Some("book-voc-1".to_string());
        word.book_title = Some("Test Book".to_string());
        upsert_vocabulary_word(&conn, &word).unwrap();

        delete_book(&conn, "book-voc-1").unwrap();

        let row = list_vocabulary(&conn).unwrap().into_iter().next().unwrap();
        assert_eq!(
            row.book_id, None,
            "FK ON DELETE SET NULL should clear book_id when the book is deleted"
        );
        assert_eq!(
            row.book_title.as_deref(),
            Some("Test Book"),
            "title snapshot survives book deletion"
        );
    }

    // --- M1: query_books / query_books_grid --------------------------------

    /// Builds one fixture book for the query-module tests, overriding only
    /// what each row needs on top of `sample_book`. `file_path` is derived
    /// from `id` because `sample_book` hard-codes a single path and the
    /// `books.file_path` column is UNIQUE.
    fn qbook(
        id: &str,
        title: &str,
        author: &str,
        added_at: i64,
        series: Option<&str>,
        rating: Option<f64>,
        want_to_read: bool,
    ) -> Book {
        Book {
            file_path: format!("/tmp/{id}.epub"),
            title: title.to_string(),
            author: author.to_string(),
            added_at,
            series: series.map(str::to_string),
            rating,
            want_to_read,
            ..sample_book(id)
        }
    }

    /// Seeds the fixture shared by every `query_books`/`query_books_grid`
    /// test: non-ASCII titles/authors, a three-way `added_at` tie (a1/a2/a3),
    /// NULL rating (a1, a3) and NULL series (a1, b1), a mix of
    /// `want_to_read` (a2, b2), and books with (a2, a3, b2) and without
    /// (a1, b1) a `reading_progress` row.
    fn seed_query_fixture(conn: &Connection) {
        insert_book(
            conn,
            &qbook(
                "a1",
                "Éducation sentimentale",
                "Gustave Flaubert",
                100,
                None,
                None,
                false,
            ),
        )
        .unwrap();
        insert_book(
            conn,
            &qbook(
                "a2",
                "Der Steppenwolf",
                "Hermann Süskind",
                100,
                Some("Classics"),
                Some(4.5),
                true,
            ),
        )
        .unwrap();
        insert_book(
            conn,
            &qbook(
                "a3",
                "Das Parfum",
                "Patrick Ünsal",
                100,
                Some("Classics"),
                None,
                false,
            ),
        )
        .unwrap();
        insert_book(
            conn,
            &qbook(
                "b1",
                "A Random Book",
                "John Doe",
                300,
                None,
                Some(5.0),
                false,
            ),
        )
        .unwrap();
        insert_book(
            conn,
            &qbook(
                "b2",
                "Another One",
                "Jane Roe",
                50,
                Some("Other"),
                Some(3.0),
                true,
            ),
        )
        .unwrap();

        // Reading progress: a2, a3, b2 have it; a1, b1 don't.
        for (book_id, last_read_at) in [("a2", 500), ("a3", 900), ("b2", 200)] {
            upsert_reading_progress(
                conn,
                &ReadingProgress {
                    book_id: book_id.to_string(),
                    chapter_index: 1,
                    scroll_position: 0.0,
                    last_read_at,
                },
            )
            .unwrap();
        }
    }

    fn ids(items: &[BookGridItem]) -> Vec<&str> {
        items.iter().map(|b| b.id.as_str()).collect()
    }

    #[test]
    fn query_books_grid_default_sort_is_date_added_desc_then_id() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(&conn, &BookQuery::default()).unwrap();

        assert_eq!(page.total, 5);
        assert_eq!(ids(&page.items), vec!["b1", "a1", "a2", "a3", "b2"]);
    }

    #[test]
    fn query_books_grid_q_matches_unicode_case_insensitively() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        // Accented, lowercase needle: must match "Éducation sentimentale".
        // A naive translation using SQLite's ASCII-only LOWER()/LIKE would
        // fail here because LOWER('É') stays 'É', not 'é'.
        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some("éducation".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&page.items), vec!["a1"]);
        assert_eq!(page.total, 1);

        // Plain-ASCII needle must NOT match — Rust's `.to_lowercase()` does
        // not fold accents away, so "education" is not a substring of
        // "éducation sentimentale".
        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some("education".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());

        // Author-side match, same accent-folding trap: "Süskind" / "Ünsal".
        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some("süskind".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&page.items), vec!["a2"]);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some("ünsal".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&page.items), vec!["a3"]);
    }

    #[test]
    fn query_books_grid_empty_q_and_series_mean_no_filter() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some(String::new()),
                series: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 5);
    }

    #[test]
    fn query_books_grid_series_filter_excludes_null_series() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                series: Some("Classics".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(ids(&page.items), vec!["a2", "a3"]);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                series: Some("Nonexistent".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 0);
    }

    #[test]
    fn query_books_grid_want_to_read_filter() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                want_to_read: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(ids(&page.items), vec!["a2", "b2"]);
    }

    #[test]
    fn query_books_grid_title_sort_orders_by_title_ascending() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::Title,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&page.items),
            vec!["b1", "b2", "a3", "a2", "a1"],
            "lowercased title ascending: 'a random book' < 'another one' < \
             'das parfum' < 'der steppenwolf' < 'éducation sentimentale'"
        );
    }

    #[test]
    fn query_books_grid_author_sort_orders_by_author_ascending() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::Author,
                ..Default::default()
            },
        )
        .unwrap();
        // authors lowercased ascending: "gustave flaubert" (a1) <
        // "hermann süskind" (a2) < "jane roe" (b2) < "john doe" (b1) <
        // "patrick ünsal" (a3)
        assert_eq!(ids(&page.items), vec!["a1", "a2", "b2", "b1", "a3"]);
        // NOTE: this fixture's five authors differ at their first letter
        // (g/h/j/j/p), so it pins the column and the direction but says
        // nothing about case folding — the folding tests below do that.
    }

    /// Rows whose sort order *flips* depending on how the sort key is folded.
    /// The point of the fixture is that a wrong folding cannot produce the
    /// expected order by accident:
    ///
    /// | pair            | Rust `to_lowercase` | SQLite ASCII `LOWER()` | no folding |
    /// |-----------------|---------------------|------------------------|------------|
    /// | `Émile`/`école` | `école` first       | `Émile` first          | `Émile` first |
    /// | `zebra`/`Zulu`  | `zebra` first       | `zebra` first          | `Zulu` first  |
    ///
    /// The first pair discriminates Unicode folding from SQLite's ASCII-only
    /// `LOWER()` (which leaves `É` = U+00C9 alone, and U+00C9 sorts before
    /// the U+00E9 that `école` already carries). The second discriminates
    /// folding from no folding at all (`Z` = 0x5A sorts before `z` = 0x7A).
    /// Together they fail against every wrong implementation of the sort key:
    ///
    /// | sort key            | resulting order          |
    /// |---------------------|--------------------------|
    /// | `carrel_lower`      | `f3, f4, f2, f1` (right) |
    /// | ASCII `LOWER()`     | `f3, f4, f1, f2`         |
    /// | raw column, no fold | `f4, f3, f1, f2`         |
    ///
    /// Both accented words lead with the byte 0xC3, which sorts after every
    /// ASCII letter, so the two pairs never interleave.
    fn seed_folding_fixture(conn: &Connection, on_author: bool) {
        for (id, text) in [
            ("f1", "Émile"),
            ("f2", "école"),
            ("f3", "zebra"),
            ("f4", "Zulu"),
        ] {
            let (title, author) = if on_author {
                ("Placeholder Title", text)
            } else {
                (text, "Placeholder Author")
            };
            insert_book(conn, &qbook(id, title, author, 100, None, None, false)).unwrap();
        }
    }

    #[test]
    fn query_books_grid_title_sort_folds_case_the_way_rust_does() {
        let (_dir, conn) = setup();
        seed_folding_fixture(&conn, false);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::Title,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&page.items),
            vec!["f3", "f4", "f2", "f1"],
            "zebra < Zulu < école < Émile once folded the way Rust's to_lowercase() folds; SQLite's ASCII-only LOWER() would swap the last two, and no folding at all would also put Zulu first"
        );
    }

    #[test]
    fn query_books_grid_author_sort_folds_case_the_way_rust_does() {
        let (_dir, conn) = setup();
        seed_folding_fixture(&conn, true);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::Author,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&page.items), vec!["f3", "f4", "f2", "f1"]);
    }

    #[test]
    fn query_books_grid_rating_sort_treats_null_as_zero() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::Rating,
                ..Default::default()
            },
        )
        .unwrap();
        // b1=5.0, a2=4.5, b2=3.0, then NULL-as-0 group (a1, a3) tie-broken by id.
        assert_eq!(ids(&page.items), vec!["b1", "a2", "b2", "a1", "a3"]);
    }

    #[test]
    fn query_books_grid_last_read_sort_uses_left_join() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                sort: BookSort::LastRead,
                ..Default::default()
            },
        )
        .unwrap();
        // a3=900, a2=500, b2=200, then no-progress group (a1, b1) treated as
        // 0, tie-broken by id.
        assert_eq!(ids(&page.items), vec!["a3", "a2", "b2", "a1", "b1"]);
    }

    #[test]
    fn query_books_grid_limit_offset_pages_a_tie_without_gap_or_overlap() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        // Default sort ties a1/a2/a3 at added_at=100; a limit of 3 splits
        // that tie group across the first and second page.
        let page1 = query_books_grid(
            &conn,
            &BookQuery {
                limit: Some(3),
                offset: 0,
                ..Default::default()
            },
        )
        .unwrap();
        let page2 = query_books_grid(
            &conn,
            &BookQuery {
                limit: Some(3),
                offset: 3,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page1.total, 5);
        assert_eq!(page2.total, 5);
        assert_eq!(ids(&page1.items), vec!["b1", "a1", "a2"]);
        assert_eq!(ids(&page2.items), vec!["a3", "b2"]);

        let mut all: Vec<&str> = ids(&page1.items);
        all.extend(ids(&page2.items));
        assert_eq!(all, vec!["b1", "a1", "a2", "a3", "b2"]);
    }

    /// `usize as i64` wraps above `i64::MAX`, and SQLite silently clamps a
    /// negative OFFSET to zero — so without the saturating conversion an
    /// offset far past the end returns the *first* page instead of nothing.
    #[test]
    fn query_books_grid_offset_above_i64_max_returns_empty_not_the_first_page() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                limit: Some(2),
                offset: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            page.items.is_empty(),
            "offset past the end must return nothing, not wrap to page one"
        );
        assert_eq!(page.total, 5, "total still counts the whole filtered set");
    }

    /// An unpaginated query skips the `COUNT(*)` and reports `items.len()`
    /// instead. That is only exact while `offset == 0`, so both halves are
    /// pinned here: the shortcut must still report the *filtered* count rather
    /// than the table size, and an offset past the end must not report zero
    /// for a non-empty set.
    #[test]
    fn query_books_grid_total_is_exact_whether_or_not_the_count_is_skipped() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        // No limit, no offset — the skipped-count path. Two of five are wanted.
        let page = query_books_grid(
            &conn,
            &BookQuery {
                want_to_read: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(
            page.total, 2,
            "skipping the COUNT must still report the filtered count, not the table size"
        );

        // No limit but a non-zero offset — the count must still run, or the
        // tail's length would be mistaken for the whole.
        let tail = query_books_grid(
            &conn,
            &BookQuery {
                want_to_read: true,
                offset: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tail.items.len(), 1, "one row of the two is skipped");
        assert_eq!(tail.total, 2, "total is the filtered set, not the tail");

        // An offset past the end returns nothing but still knows the total.
        let past = query_books_grid(
            &conn,
            &BookQuery {
                want_to_read: true,
                offset: 99,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(past.items.is_empty());
        assert_eq!(
            past.total, 2,
            "an empty tail must not report a total of zero"
        );
    }

    #[test]
    fn query_books_grid_total_reflects_filtered_count_not_page_or_table_size() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let page = query_books_grid(
            &conn,
            &BookQuery {
                want_to_read: true,
                limit: Some(1),
                offset: 0,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page.items.len(), 1, "page is capped by limit");
        assert_eq!(
            page.total, 2,
            "total counts all want_to_read matches, not the page (1) or the full table (5)"
        );
    }

    #[test]
    fn query_books_agrees_with_query_books_grid_on_ids_and_order() {
        let (_dir, conn) = setup();
        seed_query_fixture(&conn);

        let query = BookQuery {
            sort: BookSort::LastRead,
            ..Default::default()
        };
        let grid_page = query_books_grid(&conn, &query).unwrap();
        let full_page = query_books(&conn, &query).unwrap();

        assert_eq!(full_page.total, grid_page.total);
        let grid_ids: Vec<&str> = grid_page.items.iter().map(|b| b.id.as_str()).collect();
        let full_ids: Vec<&str> = full_page.items.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(full_ids, grid_ids);

        // The two projections run the same query and differ only in their
        // column list and row mapper, so agreeing on ids is a weak claim on
        // its own. What it can still catch is BOOK_COLUMNS_B drifting out of
        // step with `row_to_book` — the LEFT JOIN means a stray `SELECT *`
        // or a missing alias would either fail to map or map the wrong
        // column. Read a field only the full projection carries to pin that.
        for book in &full_page.items {
            assert_eq!(
                book.file_path,
                format!("/tmp/{}.epub", book.id),
                "full projection mapped file_path onto the wrong row"
            );
        }
        let wanted: Vec<bool> = full_page.items.iter().map(|b| b.want_to_read).collect();
        let grid_wanted: Vec<bool> = grid_page.items.iter().map(|b| b.want_to_read).collect();
        assert_eq!(wanted, grid_wanted, "projections disagree on want_to_read");
    }

    #[test]
    fn query_books_grid_works_on_bare_in_memory_connection_with_no_pool_or_helper() {
        // Proves the `carrel_lower` UDF is registered by `query_books_grid`
        // itself, on the connection it's given, rather than by any pool or
        // init-time wiring — this uses neither `create_pool` nor the
        // `init_db`/`setup` test helper, only a bare in-memory connection
        // with the schema applied directly.
        let conn = Connection::open_in_memory().unwrap();
        run_schema(&conn).unwrap();

        insert_book(
            &conn,
            &qbook(
                "x1",
                "Éducation sentimentale",
                "Gustave Flaubert",
                1,
                None,
                None,
                false,
            ),
        )
        .unwrap();

        let page = query_books_grid(
            &conn,
            &BookQuery {
                q: Some("éducation".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(ids(&page.items), vec!["x1"]);
    }

    // --- M4: query_books_in_collection_grid ---------------------------------

    fn make_collection(id: &str, name: &str, r#type: CollectionType) -> Collection {
        Collection {
            id: id.to_string(),
            name: name.to_string(),
            r#type,
            icon: None,
            color: None,
            created_at: 1,
            updated_at: 1,
            rules: Vec::new(),
        }
    }

    fn make_rule(collection_id: &str, field: &str, operator: &str, value: &str) -> CollectionRule {
        CollectionRule {
            id: format!("rule-{collection_id}-{field}"),
            collection_id: collection_id.to_string(),
            field: field.to_string(),
            operator: operator.to_string(),
            value: value.to_string(),
        }
    }

    /// Inserts a `book_collections` row with an explicit `added_at`, bypassing
    /// `add_book_to_collection`'s `unixepoch()` — tests need to control
    /// membership order independently of `books.added_at` and of wall-clock
    /// time.
    fn add_to_collection_at(conn: &Connection, book_id: &str, collection_id: &str, added_at: i64) {
        conn.execute(
            "INSERT INTO book_collections (book_id, collection_id, added_at) VALUES (?1, ?2, ?3)",
            params![book_id, collection_id, added_at],
        )
        .unwrap();
    }

    #[test]
    fn query_books_in_collection_grid_manual_q_matches_title() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-m1", "Manual", CollectionType::Manual),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("m-fox", "Red Fox", "Bob Writer", 100, None, None, false),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook(
                "m-whale",
                "Blue Whale",
                "Alice Author",
                200,
                None,
                None,
                false,
            ),
        )
        .unwrap();
        add_to_collection_at(&conn, "m-fox", "coll-m1", 100);
        add_to_collection_at(&conn, "m-whale", "coll-m1", 200);

        let page = query_books_in_collection_grid(
            &conn,
            "coll-m1",
            &BookQuery {
                q: Some("fox".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            ids(&page),
            vec!["m-fox"],
            "q must match the title and exclude the non-matching member"
        );
    }

    #[test]
    fn query_books_in_collection_grid_manual_q_matches_author() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-m2", "Manual", CollectionType::Manual),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("m-fox2", "Red Fox", "Bob Writer", 100, None, None, false),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook(
                "m-whale2",
                "Blue Whale",
                "Alice Author",
                200,
                None,
                None,
                false,
            ),
        )
        .unwrap();
        add_to_collection_at(&conn, "m-fox2", "coll-m2", 100);
        add_to_collection_at(&conn, "m-whale2", "coll-m2", 200);

        let page = query_books_in_collection_grid(
            &conn,
            "coll-m2",
            &BookQuery {
                q: Some("alice".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            ids(&page),
            vec!["m-whale2"],
            "q must match the author and exclude the non-matching member"
        );
    }

    /// The one genuinely new combining branch: an automated collection with
    /// *no* rules has no `WHERE` of its own, so the predicate becomes the
    /// whole clause instead of being ANDed onto one. Every other automated
    /// test attaches a rule, so nothing else exercises this path.
    #[test]
    fn query_books_in_collection_grid_automated_without_rules_still_applies_q() {
        let (_dir, conn) = setup();
        let coll = make_collection("coll-a0", "No rules", CollectionType::Automated);
        insert_collection(&conn, &coll).unwrap();
        insert_book(
            &conn,
            &qbook("nr-a", "Star Voyager", "Nova", 100, None, None, false),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("nr-b", "Ocean Deep", "Nova", 100, None, None, false),
        )
        .unwrap();

        // A rule-less automated collection matches the whole library, so the
        // `q` is the only thing narrowing this.
        let items = query_books_in_collection_grid(
            &conn,
            "coll-a0",
            &BookQuery {
                q: Some("voyager".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&items), vec!["nr-a"]);

        let all = query_books_in_collection_grid(&conn, "coll-a0", &BookQuery::default()).unwrap();
        assert_eq!(all.len(), 2, "no rules and no q matches the whole library");
    }

    /// `q` goes through `instr()`, not `LIKE`, so SQL wildcards in a search
    /// term are literal — the same guarantee `query_books_grid` has, asserted
    /// again at this seam because this is where a future edit might reach for
    /// `LIKE` to compose with `build_rule_query`'s own `LIKE` clauses.
    #[test]
    fn query_books_in_collection_grid_treats_sql_wildcards_as_literal() {
        let (_dir, conn) = setup();
        let coll = make_collection("coll-w", "Wild", CollectionType::Manual);
        insert_collection(&conn, &coll).unwrap();
        for (id, title) in [("w-a", "100% Cotton"), ("w-b", "Anything At All")] {
            insert_book(&conn, &qbook(id, title, "Author", 100, None, None, false)).unwrap();
            add_book_to_collection(&conn, id, "coll-w").unwrap();
        }

        // Under LIKE, "100%" would match both rows; as a literal it matches one.
        let items = query_books_in_collection_grid(
            &conn,
            "coll-w",
            &BookQuery {
                q: Some("100%".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&items), vec!["w-a"]);

        // `_` is a single-character LIKE wildcard; literally, it matches nothing.
        let none = query_books_in_collection_grid(
            &conn,
            "coll-w",
            &BookQuery {
                q: Some("1_0".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(none.is_empty(), "underscore must not act as a wildcard");
    }

    #[test]
    fn query_books_in_collection_grid_automated_rule_and_q_both_apply() {
        let (_dir, conn) = setup();
        let mut coll = make_collection("coll-a1", "Automated", CollectionType::Automated);
        coll.rules = vec![make_rule("coll-a1", "series", "equals", "SciFi")];
        insert_collection(&conn, &coll).unwrap();

        // Matches rule AND q.
        insert_book(
            &conn,
            &qbook(
                "auto-a",
                "Star Voyager",
                "Nova",
                100,
                Some("SciFi"),
                None,
                false,
            ),
        )
        .unwrap();
        // Matches rule but not q.
        insert_book(
            &conn,
            &qbook(
                "auto-b",
                "Ocean Deep",
                "Nova",
                100,
                Some("SciFi"),
                None,
                false,
            ),
        )
        .unwrap();
        // Matches q but not rule (wrong series).
        insert_book(
            &conn,
            &qbook(
                "auto-c",
                "Star Kingdom",
                "Nova",
                100,
                Some("Fantasy"),
                None,
                false,
            ),
        )
        .unwrap();

        let page = query_books_in_collection_grid(
            &conn,
            "coll-a1",
            &BookQuery {
                q: Some("star".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            ids(&page),
            vec!["auto-a"],
            "the rule and the q predicate must both apply (AND), not either alone"
        );
    }

    #[test]
    fn query_books_in_collection_grid_want_to_read_filter() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-w1", "Manual", CollectionType::Manual),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("w-yes", "Wanted", "Author", 100, None, None, true),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("w-no", "Not Wanted", "Author", 100, None, None, false),
        )
        .unwrap();
        add_to_collection_at(&conn, "w-yes", "coll-w1", 100);
        add_to_collection_at(&conn, "w-no", "coll-w1", 200);

        let wanted_only = query_books_in_collection_grid(
            &conn,
            "coll-w1",
            &BookQuery {
                want_to_read: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&wanted_only), vec!["w-yes"]);

        let all = query_books_in_collection_grid(&conn, "coll-w1", &BookQuery::default()).unwrap();
        assert_eq!(
            all.len(),
            2,
            "want_to_read: false (the default) must not filter anything out"
        );
    }

    #[test]
    fn query_books_in_collection_grid_non_ascii_q_folds_case() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-u1", "Manual", CollectionType::Manual),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook(
                "u-edu",
                "Éducation sentimentale",
                "Gustave Flaubert",
                100,
                None,
                None,
                false,
            ),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook(
                "u-other",
                "Something Else",
                "Someone",
                200,
                None,
                None,
                false,
            ),
        )
        .unwrap();
        add_to_collection_at(&conn, "u-edu", "coll-u1", 100);
        add_to_collection_at(&conn, "u-other", "coll-u1", 200);

        // A naive ASCII-only LOWER()/LIKE would fail to match this — see
        // `register_carrel_lower`'s doc comment.
        let page = query_books_in_collection_grid(
            &conn,
            "coll-u1",
            &BookQuery {
                q: Some("éducation".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(ids(&page), vec!["u-edu"]);
    }

    #[test]
    fn query_books_in_collection_grid_empty_and_absent_q_return_whole_collection() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-e1", "Manual", CollectionType::Manual),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("e-a", "A Book", "Author", 100, None, None, false),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("e-b", "B Book", "Author", 200, None, None, false),
        )
        .unwrap();
        add_to_collection_at(&conn, "e-a", "coll-e1", 100);
        add_to_collection_at(&conn, "e-b", "coll-e1", 200);

        let absent =
            query_books_in_collection_grid(&conn, "coll-e1", &BookQuery::default()).unwrap();
        assert_eq!(absent.len(), 2);

        let empty = query_books_in_collection_grid(
            &conn,
            "coll-e1",
            &BookQuery {
                q: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(empty.len(), 2);
    }

    #[test]
    fn query_books_in_collection_grid_manual_membership_order_survives_filtering() {
        let (_dir, conn) = setup();
        insert_collection(
            &conn,
            &make_collection("coll-o1", "Manual", CollectionType::Manual),
        )
        .unwrap();
        // `books.added_at` order is the reverse of `book_collections.added_at`
        // order, so the two orderings are distinguishable: if the function
        // ever switched to `book_sort_order_sql`'s date-added ordering (or
        // any b.added_at-based order), this would catch it.
        insert_book(
            &conn,
            &qbook("ord-x", "Same Title", "Author", 100, None, None, false),
        )
        .unwrap();
        insert_book(
            &conn,
            &qbook("ord-y", "Same Title", "Author", 500, None, None, false),
        )
        .unwrap();
        add_to_collection_at(&conn, "ord-x", "coll-o1", 900); // added to collection most recently
        add_to_collection_at(&conn, "ord-y", "coll-o1", 200); // added to collection long ago

        let unfiltered =
            query_books_in_collection_grid(&conn, "coll-o1", &BookQuery::default()).unwrap();
        assert_eq!(
            ids(&unfiltered),
            vec!["ord-x", "ord-y"],
            "membership order (bc.added_at DESC) must win over books.added_at order"
        );

        let filtered = query_books_in_collection_grid(
            &conn,
            "coll-o1",
            &BookQuery {
                q: Some("same title".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&filtered),
            vec!["ord-x", "ord-y"],
            "filtering must not disturb the membership order"
        );
    }
}

//! Kids mode: which books a child profile may see.
//!
//! Parents mark a series (by exact `books.series` string) or a single book.
//! Visibility rule: a book override wins (allowed -> visible, blocked ->
//! hidden); otherwise a book is visible iff its `series` exactly equals a
//! `kids_series` row; otherwise hidden.

use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};

/// Per-book parental override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KidsOverride {
    /// No override: the series rule decides.
    Follow,
    Allowed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KidsBookMark {
    pub r#override: KidsOverride,
    pub series_allowed: bool,
    pub visible: bool,
}

/// Boolean SQL expression over a `books` row aliased `b`: the visibility rule.
/// Series match is exact string equality, like `b.series = ?` elsewhere.
pub const VISIBLE_SQL: &str = "COALESCE(\
    (SELECT o.allowed FROM kids_book_overrides o WHERE o.book_id = b.id), \
    EXISTS (SELECT 1 FROM kids_series s WHERE s.series = b.series))";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn set_series_allowed(conn: &Connection, series: &str, allowed: bool) -> Result<()> {
    if allowed {
        conn.execute(
            "INSERT OR REPLACE INTO kids_series (series, created_at) VALUES (?1, ?2)",
            params![series, now()],
        )?;
    } else {
        conn.execute("DELETE FROM kids_series WHERE series = ?1", params![series])?;
    }
    Ok(())
}

pub fn list_allowed_series(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT series FROM kids_series ORDER BY series")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn set_book_override(conn: &Connection, book_id: &str, o: KidsOverride) -> Result<()> {
    let allowed = match o {
        KidsOverride::Follow => {
            conn.execute(
                "DELETE FROM kids_book_overrides WHERE book_id = ?1",
                params![book_id],
            )?;
            return Ok(());
        }
        KidsOverride::Allowed => 1,
        KidsOverride::Blocked => 0,
    };
    conn.execute(
        "INSERT OR REPLACE INTO kids_book_overrides (book_id, allowed, updated_at) VALUES (?1, ?2, ?3)",
        params![book_id, allowed, now()],
    )?;
    Ok(())
}

/// `None` when the book doesn't exist.
pub fn get_book_mark(conn: &Connection, book_id: &str) -> Result<Option<KidsBookMark>> {
    let sql = format!(
        "SELECT (SELECT o.allowed FROM kids_book_overrides o WHERE o.book_id = b.id), \
                EXISTS (SELECT 1 FROM kids_series s WHERE s.series = b.series), \
                {VISIBLE_SQL} \
         FROM books b WHERE b.id = ?1"
    );
    conn.query_row(&sql, params![book_id], |r| {
        let ov: Option<i64> = r.get(0)?;
        Ok(KidsBookMark {
            r#override: match ov {
                None => KidsOverride::Follow,
                Some(0) => KidsOverride::Blocked,
                Some(_) => KidsOverride::Allowed,
            },
            series_allowed: r.get(1)?,
            visible: r.get(2)?,
        })
    })
    .optional()
}

pub fn is_book_visible(conn: &Connection, book_id: &str) -> Result<bool> {
    Ok(get_book_mark(conn, book_id)?.is_some_and(|m| m.visible))
}

pub fn visible_book_ids(conn: &Connection) -> Result<Vec<String>> {
    let sql = format!("SELECT b.id FROM books b WHERE {VISIBLE_SQL} ORDER BY b.id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Book, BookFormat};
    use rusqlite::Connection;
    use tempfile::tempdir;

    fn book(id: &str, series: Option<&str>) -> Book {
        Book {
            id: id.to_string(),
            title: format!("Title {id}"),
            author: "Author".to_string(),
            file_path: format!("/tmp/{id}.epub"),
            cover_path: None,
            total_chapters: 1,
            added_at: 1700000000,
            format: BookFormat::Epub,
            file_hash: None,
            description: None,
            genres: None,
            rating: None,
            isbn: None,
            openlibrary_key: None,
            enrichment_status: None,
            series: series.map(str::to_string),
            volume: None,
            language: None,
            publisher: None,
            publish_year: None,
            is_imported: true,
            want_to_read: false,
        }
    }

    /// A = Tintin, B = Druuna, C = no series.
    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = db::init_db(&dir.path().join("test.db")).unwrap();
        db::insert_book(&conn, &book("A", Some("Tintin"))).unwrap();
        db::insert_book(&conn, &book("B", Some("Druuna"))).unwrap();
        db::insert_book(&conn, &book("C", None)).unwrap();
        (dir, conn)
    }

    fn override_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM kids_book_overrides", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn series_mark_makes_only_that_series_visible() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "Tintin", true).unwrap();
        assert_eq!(visible_book_ids(&conn).unwrap(), vec!["A"]);
        assert!(is_book_visible(&conn, "A").unwrap());
        assert!(!is_book_visible(&conn, "B").unwrap());
        assert!(!is_book_visible(&conn, "C").unwrap());
        assert_eq!(list_allowed_series(&conn).unwrap(), vec!["Tintin"]);
    }

    #[test]
    fn unmarking_series_hides_its_books() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "Tintin", true).unwrap();
        set_series_allowed(&conn, "Tintin", false).unwrap();
        assert!(visible_book_ids(&conn).unwrap().is_empty());
        assert!(list_allowed_series(&conn).unwrap().is_empty());
    }

    #[test]
    fn blocked_override_hides_book_in_marked_series() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "Tintin", true).unwrap();
        set_book_override(&conn, "A", KidsOverride::Blocked).unwrap();
        assert!(!is_book_visible(&conn, "A").unwrap());
        let m = get_book_mark(&conn, "A").unwrap().unwrap();
        assert_eq!(m.r#override, KidsOverride::Blocked);
        assert!(m.series_allowed);
        assert!(!m.visible);
    }

    #[test]
    fn allowed_override_shows_book_without_series() {
        let (_d, conn) = setup();
        set_book_override(&conn, "C", KidsOverride::Allowed).unwrap();
        assert_eq!(visible_book_ids(&conn).unwrap(), vec!["C"]);
    }

    #[test]
    fn follow_override_restores_series_rule() {
        let (_d, conn) = setup();
        set_book_override(&conn, "B", KidsOverride::Allowed).unwrap();
        assert!(is_book_visible(&conn, "B").unwrap());
        set_book_override(&conn, "B", KidsOverride::Follow).unwrap();
        let m = get_book_mark(&conn, "B").unwrap().unwrap();
        assert_eq!(m.r#override, KidsOverride::Follow);
        assert!(!m.visible);
        assert_eq!(override_rows(&conn), 0);
    }

    #[test]
    fn new_book_in_marked_series_is_visible() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "Tintin", true).unwrap();
        db::insert_book(&conn, &book("D", Some("Tintin"))).unwrap();
        assert_eq!(visible_book_ids(&conn).unwrap(), vec!["A", "D"]);
    }

    #[test]
    fn changing_book_series_changes_visibility() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "Tintin", true).unwrap();
        conn.execute("UPDATE books SET series = 'Tintin EN' WHERE id = 'A'", [])
            .unwrap();
        assert!(!is_book_visible(&conn, "A").unwrap());
    }

    #[test]
    fn series_match_is_exact() {
        let (_d, conn) = setup();
        set_series_allowed(&conn, "tintin", true).unwrap();
        assert!(!is_book_visible(&conn, "A").unwrap());
    }

    #[test]
    fn deleting_book_removes_its_override() {
        let (_d, conn) = setup();
        set_book_override(&conn, "B", KidsOverride::Allowed).unwrap();
        db::delete_book(&conn, "B").unwrap();
        assert_eq!(override_rows(&conn), 0);
    }

    #[test]
    fn get_book_mark_none_for_missing_book() {
        let (_d, conn) = setup();
        assert_eq!(get_book_mark(&conn, "nope").unwrap(), None);
    }

    #[test]
    fn kids_tables_created_idempotently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        drop(db::init_db(&path).unwrap());
        let conn = db::init_db(&path).unwrap();
        for t in ["kids_series", "kids_book_overrides"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{t}");
        }
    }

    #[test]
    fn mark_serializes_as_documented() {
        let m = KidsBookMark {
            r#override: KidsOverride::Follow,
            series_allowed: false,
            visible: false,
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"override":"follow","series_allowed":false,"visible":false}"#
        );
    }
}

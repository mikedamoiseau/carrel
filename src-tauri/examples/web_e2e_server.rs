//! Deterministic seeded harness for the Playwright e2e suite (`e2e/`).
//!
//! Boots the embedded web server (`folio_lib::web_server`) against a fresh
//! temp on-disk SQLite DB seeded with a fixed fixture set, so the specs in
//! `e2e/` can assert exact numbers instead of resilient "at least one"
//! checks against a live, hand-curated library.
//!
//! This file is an example target of the `folio` package, compiled as an
//! external crate against the `folio_lib` library (see `[lib] name =
//! "folio_lib"` in `src-tauri/Cargo.toml`) — everything below goes through
//! `folio_lib`'s public API, the same surface `src-tauri/src/main.rs` uses.
//!
//! Run directly: `cargo run --example web_e2e_server` (from `src-tauri/`).
//! `e2e/playwright.config.ts`'s `webServer` block runs this automatically
//! and polls `/api/health` until it responds.
//!
//! ## Fixture set (numbers referenced directly by e2e specs — keep in sync)
//!
//! - 130 books, ids `e2e-book-001`..`e2e-book-130`, titles `Book 001`..
//!   `Book 130`. `added_at` increases with the numeric suffix, so the
//!   default `ORDER BY added_at DESC, id` (see `folio-core/src/db.rs`'s
//!   `list_books_grid`) puts `Book 130` first and `Book 001` last:
//!     - page 1 (offset 0,   limit 60) = Book 130 .. Book 071
//!     - page 2 (offset 60,  limit 60) = Book 070 .. Book 011
//!     - page 3 (offset 120, limit 60) = Book 010 .. Book 001 (10 books)
//! - `Book 099` and `Book 100` share an identical `added_at` to exercise
//!   the `id` ASC tiebreaker: with equal timestamps, `e2e-book-099` sorts
//!   before `e2e-book-100` even though 100 > 99.
//! - 12 books have reading progress (`chapter_index = 4`,
//!   `total_chapters = 10`, i.e. a 50% fill): `Book 005, 015, 025, 035,
//!   045, 055, 065, 075, 085, 095, 105, 115`. Each satisfies
//!   `chapter_index < total_chapters - 1`, so all 12 also qualify for the
//!   "Continue Reading" shelf (`db::get_continue_reading_books`). `Book
//!   075` falls on page 1 of the default grid *and* in the shelf, for the
//!   shelf/grid fill-percentage agreement check.
//! - `Book 060` has `total_chapters = 0` but a progress row
//!   (`chapter_index = 3`) — the UI must render no progress bar for it,
//!   and it's excluded from the "Continue Reading" shelf by the
//!   `total_chapters > 0` guard in `get_continue_reading_books`.
//! - `Book 130` is a CBZ with a real 2-page zip file on disk (2 tiny valid
//!   PNGs), seeded as a linked (`is_imported = false`) book with an
//!   absolute `file_path` so `WebState::resolve_book_path` returns it
//!   unchanged — no library-folder resolution needed.
//! - `Book 050` is an EPUB with a real 2-chapter file on disk (see
//!   `build_test_epub`), also linked with an absolute `file_path`. It's the
//!   only reflowable book whose chapter route returns real HTML, used by the
//!   F-4-4 chapter-prefetch e2e; chapter index 1 embeds one inline image.
//! - Every other book is `is_imported = false` with a fake, nonexistent
//!   relative `file_path`; that's fine since nothing beyond the grid/
//!   detail metadata is exercised for them (a missing cover 404s and the
//!   client falls back to its placeholder).

use folio_lib::db;
use folio_lib::models::{Book, BookFormat, ReadingProgress};
use folio_lib::web_server::{self, auth, ServerModes, WebState};
use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Base timestamp (seconds since epoch); book `n`'s `added_at` is this plus
/// `n * 100`, giving every book a distinct, strictly increasing value.
const BASE_TS: i64 = 1_700_000_000;

/// Numeric suffixes of the 12 books seeded with reading progress.
const PROGRESS_NS: [u32; 12] = [5, 15, 25, 35, 45, 55, 65, 75, 85, 95, 105, 115];

/// `Book 060` — has `total_chapters = 0` plus a progress row; must render
/// no progress bar anywhere.
const ZERO_CHAPTERS_N: u32 = 60;

/// `Book 130` — the CBZ book with a real file on disk.
const CBZ_N: u32 = 130;

/// `Book 050` — the EPUB book with a real multi-chapter file on disk (see
/// `build_test_epub`): 2 chapters, chapter index 1 embeds one inline image.
/// It's the fixture for the F-4-4 web-reader chapter-prefetch e2e — the only
/// reflowable book whose `/api/books/:id/chapters/:index` route returns real
/// HTML. Chosen to sit outside every count/ordering/progress assertion: it's
/// on page 2 of the default grid, is seeded with no reading-progress row, and
/// (being only 2 chapters) can never reach the Continue Reading shelf even
/// after the e2e turns a page — see `build_test_epub`.
const EPUB_N: u32 = 50;

/// `Book 099` / `Book 100` share an `added_at` to exercise the `id`
/// tiebreaker in the default sort.
const TIE_LOW_N: u32 = 99;
const TIE_HIGH_N: u32 = 100;

/// `Book 042` — seeded with the manual "want to read" flag set, so the
/// want-to-read filter/shelf/badge have a baseline DB-backed member on a fresh
/// load. Chosen to sit outside every other count/ordering/progress fixture.
const WANT_TO_READ_N: u32 = 42;

const TOTAL_BOOKS: u32 = 130;

fn added_at_for(n: u32) -> i64 {
    BASE_TS + i64::from(n) * 100
}

fn book_id(n: u32) -> String {
    format!("e2e-book-{n:03}")
}

/// Writes a tiny, valid 2-page CBZ (a zip of 2 real PNGs) to `path`.
fn build_test_cbz(path: &Path) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    for i in 1..=2u32 {
        let img =
            image::RgbImage::from_pixel(4, 4, image::Rgb([i as u8 * 40, 0, 255 - i as u8 * 40]));
        let mut png_bytes = Vec::new();
        {
            use image::ImageEncoder;
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder.write_image(img.as_raw(), 4, 4, image::ExtendedColorType::Rgb8)?;
        }
        zip.start_file(format!("page{i:02}.png"), options)?;
        zip.write_all(&png_bytes)?;
    }
    zip.finish()?;
    Ok(())
}

/// Writes a tiny, valid 2-chapter EPUB to `path`. Chapter index 1 embeds one
/// inline `<img>` so the F-4-4 prefetch e2e can assert both HTML prefetch and
/// inline-image warming. Structure mirrors what `folio_core::epub` parses:
/// `META-INF/container.xml` -> `OEBPS/content.opf` (spine of 2 xhtml items).
/// Two chapters (not three) is deliberate: the deepest a reader can navigate
/// is the last chapter, which fails `get_continue_reading_books`' guard
/// (`chapter_index < total_chapters - 1`), so the e2e turning to chapter 1
/// can never leave Book 050 on the Continue Reading shelf and perturb the
/// progress-shelf fixtures other specs rely on.
fn build_test_epub(path: &Path) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default();

    // `mimetype` first and stored uncompressed, per the EPUB OCF spec.
    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;

    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(
        br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
    )?;

    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>E2E Prefetch Book</dc:title>
    <dc:creator>E2E Author</dc:creator>
    <dc:language>en</dc:language>
    <dc:identifier id="bookid">e2e-epub-prefetch</dc:identifier>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch0" href="ch0.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="pic" href="pic.png" media-type="image/png"/>
  </manifest>
  <spine>
    <itemref idref="ch0"/>
    <itemref idref="ch1"/>
  </spine>
</package>"#,
    )?;

    let chapter = |title: &str, body: &str| -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title></head>
<body>{body}</body></html>"#
        )
    };
    zip.start_file("OEBPS/ch0.xhtml", deflated)?;
    // Long enough that `#reader-stage` scrolls at the test viewport, so the
    // same-chapter bookmark scroll-restore e2e (bookmarks.spec.ts) can measure
    // a non-zero scrollTop. Keeps the "chapter zero" marker (asserted by
    // several specs) as the first paragraph.
    let mut ch0_body = String::from("<p>E2E chapter zero content.</p>");
    for i in 0..60 {
        ch0_body.push_str(&format!(
            "<p>Paragraph {i} — lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
        ));
    }
    // Highlight e2e (highlights.spec.ts): a paragraph whose highlightable
    // region sits AFTER an entity (`&amp;` = 1 rendered char vs 5 raw) and a
    // non-BMP char (🦀 = 2 UTF-16 code units), proving offsets are UTF-16
    // code units of the rendered textContent. Additive — nothing above moves.
    ch0_body
        .push_str("<p>Fish &amp; chips 🦀 the quick brown fox jumps over the lazy dog again.</p>");
    zip.write_all(chapter("Ch0", &ch0_body).as_bytes())?;
    zip.start_file("OEBPS/ch1.xhtml", deflated)?;
    // Long enough that `#reader-stage` actually scrolls at the test viewport,
    // so the typography reflow-anchor e2e can measure a mid-chapter position.
    // Keeps the "chapter one" marker (asserted by the F-4-4 prefetch spec) and
    // the inline <img> (its image-prefetch spec). Chapter COUNT is unchanged
    // (still 2) — only ch1's body grows.
    let mut ch1_body = String::from("<p>E2E chapter one content.</p>");
    for i in 0..60 {
        ch1_body.push_str(&format!(
            "<p>Paragraph {i} — lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
        ));
    }
    ch1_body.push_str(r#"<img src="pic.png" alt="inline"/>"#);
    zip.write_all(chapter("Ch1", &ch1_body).as_bytes())?;

    // EPUB3 nav document — gives the TOC real labels for the web-reader TOC
    // e2e (toc.spec.ts). The parser flattens, so all three appear flat in
    // document order; `Section 1.1`'s `#sec` fragment is stripped and it
    // resolves to spine index 1 (same as Chapter One) — a duplicate-index
    // entry the highlight test relies on. Not in the spine, so total_chapters
    // stays 2.
    zip.start_file("OEBPS/nav.xhtml", deflated)?;
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body>
  <nav epub:type="toc" id="toc">
    <ol>
      <li><a href="ch0.xhtml">Chapter Zero</a></li>
      <li><a href="ch1.xhtml">Chapter One</a></li>
      <li><a href="ch1.xhtml#sec">Section 1.1</a></li>
    </ol>
  </nav>
</body></html>"#,
    )?;

    // A tiny valid PNG for the inline image referenced by chapter 1.
    let img = image::RgbImage::from_pixel(4, 4, image::Rgb([10, 120, 200]));
    let mut png_bytes = Vec::new();
    {
        use image::ImageEncoder;
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder.write_image(img.as_raw(), 4, 4, image::ExtendedColorType::Rgb8)?;
    }
    zip.start_file("OEBPS/pic.png", deflated)?;
    zip.write_all(&png_bytes)?;

    zip.finish()?;
    Ok(())
}

fn new_book(n: u32, cbz_path: &Path, epub_path: &Path) -> Book {
    let id = book_id(n);
    let is_cbz = n == CBZ_N;
    let added_at = if n == TIE_HIGH_N {
        added_at_for(TIE_LOW_N)
    } else {
        added_at_for(n)
    };

    Book {
        title: format!("Book {n:03}"),
        author: format!("Author {n:03}"),
        file_path: if is_cbz {
            cbz_path.to_string_lossy().to_string()
        } else if n == EPUB_N {
            epub_path.to_string_lossy().to_string()
        } else {
            format!("{id}.epub")
        },
        cover_path: None,
        total_chapters: if n == ZERO_CHAPTERS_N {
            0
        } else if is_cbz || n == EPUB_N {
            // CBZ: 2 pages. EPUB: 2 chapters — keeping the EPUB at the same
            // count is deliberate (see `build_test_epub`): turning to the last
            // chapter must not satisfy the Continue Reading guard.
            2
        } else {
            10
        },
        added_at,
        format: if is_cbz {
            BookFormat::Cbz
        } else {
            BookFormat::Epub
        },
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
        is_imported: false,
        want_to_read: n == WANT_TO_READ_N,
        id,
    }
}

fn seed(
    conn: &rusqlite::Connection,
    cbz_path: &Path,
    epub_path: &Path,
) -> Result<(), Box<dyn Error>> {
    for n in 1..=TOTAL_BOOKS {
        let book = new_book(n, cbz_path, epub_path);
        let added_at = book.added_at;
        db::insert_book(conn, &book)?;

        let progress_chapter_index = if n == ZERO_CHAPTERS_N {
            Some(3)
        } else if PROGRESS_NS.contains(&n) {
            Some(4)
        } else {
            None
        };

        if let Some(chapter_index) = progress_chapter_index {
            db::upsert_reading_progress(
                conn,
                &ReadingProgress {
                    book_id: book.id,
                    chapter_index,
                    scroll_position: 0.0,
                    last_read_at: added_at,
                },
            )?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    // Kept alive for the process lifetime — the harness never cleans up
    // its own temp dir (Playwright/CI kill the process instead), matching
    // how other ephemeral e2e fixtures are handled.
    let tempdir = tempfile::Builder::new().prefix("folio-e2e-").tempdir()?;

    let db_path = tempdir.path().join("library.db");
    let data_dir = tempdir.path().join("appdata");
    std::fs::create_dir_all(&data_dir)?;
    let cache_dir = tempdir.path().join("appcache");
    std::fs::create_dir_all(&cache_dir)?;

    let cbz_path = tempdir.path().join("test-book.cbz");
    build_test_cbz(&cbz_path)?;

    let epub_path = tempdir.path().join("test-book.epub");
    build_test_epub(&epub_path)?;

    let pool = db::create_pool(&db_path)?;
    {
        let conn = pool.get()?;
        seed(&conn, &cbz_path, &epub_path)?;
    }

    let state = WebState {
        pool: Arc::new(Mutex::new(pool)),
        data_dir,
        cache_dir,
        pin_hash: Arc::new(Mutex::new(None)),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        login_limiter: Arc::new(auth::RateLimiter::new(5, 300)),
        // This harness has no profile-switch flow of its own, so it's
        // always the (unlocked, lock-free) "default" profile — the
        // soft-lock gate (A-M2) must stay a no-op here.
        active_profile_name: Arc::new(Mutex::new("default".to_string())),
        unlocked_profiles: Arc::new(Mutex::new(std::collections::HashSet::from([
            "default".to_string()
        ]))),
        private_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let router = web_server::build_router(
        state,
        ServerModes {
            web_ui: true,
            opds: false,
        },
    );

    let port: u16 = std::env::var("FOLIO_E2E_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7810);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Printed AFTER binding so a reader watching stdout knows the port is
    // actually open. Playwright's own readiness check polls the
    // `webServer.url` over HTTP rather than parsing this line, but it's
    // useful for local debugging.
    println!("listening on http://127.0.0.1:{port}");
    std::io::stdout().flush().ok();

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    // Keep the guard alive until the (never-returning, in practice) serve
    // future above completes.
    drop(tempdir);
    Ok(())
}

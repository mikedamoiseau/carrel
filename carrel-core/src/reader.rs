//! Chapter reads for the text formats, behind one interface.
//!
//! # Why this module exists
//!
//! Before it, every adapter re-derived the same sequence for reading a
//! chapter — resolve the path, `match book.format`, pick between the
//! path-based and the cache-based parser entry point, map the error — and the
//! copies drifted. The desktop IPC adapter read through an archive LRU; the
//! LAN web adapter called the path-based functions, which reopen and reparse
//! the file on *every* chapter request. Same operation, two behaviours, and no
//! single place to fix either.
//!
//! Callers hand in the caches rather than letting this module own them, for
//! three reasons that are all load-bearing:
//!
//! 1. Byte budgets are configured once at app startup (`set_max_bytes` in the
//!    desktop shell's `lib.rs`); core must not re-decide them.
//! 2. The desktop `get_unified_cache_stats` / `clear_all_caches` commands
//!    report on and clear these exact `Arc`s through
//!    [`crate::cache::MemoryCacheAdapter`]. Injecting the same handle means
//!    those commands keep working untouched.
//! 3. An adapter with no cache-management surface (the web server) can hold
//!    its own caches with its own capacity without inheriting a registry.
//!
//! # Locking
//!
//! [`crate::epub::CachedEpubArchive`] carries an `unsafe impl Send` whose
//! soundness argument is that every access happens under the `Mutex` guarding
//! its cache — which now lives in [`ArchiveCaches`], shared by every adapter.
//! Take the guard, read, drop it. An async caller must never hold it across an
//! `.await`.

use std::sync::{Arc, Mutex};

use crate::cache::LruCache;
use crate::epub;
use crate::error::{CarrelError, CarrelResult};
use crate::models::BookFormat;
use crate::storage::Storage;

/// Parsed-archive caches, keyed by resolved file path, owned by the caller.
#[derive(Clone)]
pub struct ArchiveCaches {
    pub epub: Arc<Mutex<LruCache<epub::CachedEpubArchive>>>,
    /// Holds the post-parse MOBI view (HTML parts + image resources), which
    /// can run to hundreds of MB on an illustrated AZW3 — hence the byte-sized
    /// inserts in [`ensure_mobi_cached`] rather than entry counting.
    #[cfg(feature = "mobi")]
    pub mobi: Arc<Mutex<LruCache<crate::mobi::CachedMobiBook>>>,
}

impl ArchiveCaches {
    /// Empty caches holding at most `entries` parsed archives each.
    ///
    /// For callers that own no caches of their own (a standalone server, a
    /// test harness). No byte budget is applied — set one on the field if the
    /// caller cares, the way the desktop shell does for MOBI.
    pub fn with_capacity(entries: usize) -> Self {
        Self {
            epub: Arc::new(Mutex::new(LruCache::new(entries))),
            #[cfg(feature = "mobi")]
            mobi: Arc::new(Mutex::new(LruCache::new(entries))),
        }
    }
}

/// Read one chapter's sanitized HTML.
///
/// Dispatches on `format`, keeps the parsed archive in `caches` so a repeat
/// read of the same book does not reopen the file, and extracts inline images
/// into `images` under `{book_id}/{chapter_index}/…`.
///
/// Errors with [`CarrelError::invalid`] for the image-only formats (PDF, CBZ,
/// CBR), which have no chapters to read.
pub fn chapter_html(
    format: BookFormat,
    file_path: &str,
    chapter_index: usize,
    images: &dyn Storage,
    book_id: &str,
    caches: &ArchiveCaches,
) -> CarrelResult<String> {
    match format {
        BookFormat::Epub => {
            let mut cache = caches.epub.lock()?;
            ensure_epub_cached(&mut cache, file_path)?;
            let cached = cache
                .get_mut(file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open EPUB archive"))?;
            Ok(epub::get_chapter_content_from_cache(
                cached,
                chapter_index,
                images,
                book_id,
            )?)
        }
        #[cfg(feature = "mobi")]
        BookFormat::Mobi => {
            let mut cache = caches.mobi.lock()?;
            ensure_mobi_cached(&mut cache, file_path)?;
            let cached = cache
                .get(file_path)
                .ok_or_else(|| CarrelError::internal("Failed to open MOBI book"))?;
            Ok(crate::mobi::get_chapter_content_from_cache(
                cached,
                chapter_index,
                images,
                book_id,
            )?)
        }
        #[cfg(not(feature = "mobi"))]
        BookFormat::Mobi => Err(CarrelError::invalid(
            "MOBI support is not enabled in this build",
        )),
        other => Err(CarrelError::invalid(format!(
            "chapter reads are not supported for format {other}"
        ))),
    }
}

/// Ensure `file_path`'s archive is in `cache`, touching it if already present.
///
/// The open error is propagated rather than swallowed. It carries the kind the
/// callers' error mapping depends on — a missing book file must stay a
/// `NotFound` (404 on the web adapter), not collapse into a generic internal
/// failure, which is what discarding it here produced.
fn ensure_epub_cached(
    cache: &mut LruCache<epub::CachedEpubArchive>,
    file_path: &str,
) -> CarrelResult<()> {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return Ok(());
    }
    let archive = epub::CachedEpubArchive::open(file_path)?;
    cache.insert(file_path.to_string(), archive);
    Ok(())
}

/// MOBI counterpart of [`ensure_epub_cached`]. Returns the open error rather
/// than swallowing it, because `cache.get()` only signals presence and a
/// libmobi parse failure is worth surfacing to the caller as itself.
///
/// Inserts via `insert_with_size` so the byte budget the caller configured on
/// the cache actually drives eviction — entry counting alone would let a
/// handful of illustrated books pin multi-GB of RAM.
#[cfg(feature = "mobi")]
fn ensure_mobi_cached(
    cache: &mut LruCache<crate::mobi::CachedMobiBook>,
    file_path: &str,
) -> CarrelResult<()> {
    if cache.get(file_path).is_some() {
        cache.touch(file_path);
        return Ok(());
    }
    let cached = crate::mobi::CachedMobiBook::open(file_path)?;
    let size = cached.byte_size();
    cache.insert_with_size(file_path.to_string(), cached, size);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caches() -> ArchiveCaches {
        ArchiveCaches {
            epub: Arc::new(Mutex::new(LruCache::new(4))),
            #[cfg(feature = "mobi")]
            mobi: Arc::new(Mutex::new(LruCache::new(4))),
        }
    }

    /// One-chapter EPUB with a relative `<img src>`, so a read exercises the
    /// spine lookup and the inline-image rewrite.
    fn write_epub(dir: &std::path::Path) -> String {
        let path = dir.join("book.epub");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();

        zip.start_file("mimetype", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Reader Test</dc:title>
  </metadata>
  <manifest>
    <item id="ch0" href="ch0.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="img.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch0"/></spine>
</package>"#,
        )
        .unwrap();
        zip.start_file("ch0.xhtml", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<html><body><p>Hello</p><img src="img.png"/></body></html>"#,
        )
        .unwrap();
        zip.start_file("img.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"\x89PNG\r\n\x1a\n").unwrap();
        zip.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    fn images(dir: &std::path::Path) -> crate::storage::LocalStorage {
        crate::storage::LocalStorage::new(dir.join("images")).unwrap()
    }

    /// The point of the module: a second read of the same book is served from
    /// the cached archive, not by reopening the file. Deleting the file between
    /// the two reads is what makes that observable — a reopen would fail, and a
    /// test that merely asserted "both calls returned Ok" would pass even with
    /// the caching removed.
    #[test]
    fn second_chapter_read_does_not_reopen_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_epub(dir.path());
        let store = images(dir.path());
        let caches = caches();

        let first = chapter_html(BookFormat::Epub, &path, 0, &store, "bk", &caches).unwrap();
        assert_eq!(caches.epub.lock().unwrap().len(), 1);

        std::fs::remove_file(&path).unwrap();

        let second = chapter_html(BookFormat::Epub, &path, 0, &store, "bk", &caches).unwrap();
        assert_eq!(second, first);
        assert_eq!(caches.epub.lock().unwrap().len(), 1);
    }

    /// A missing book file must stay a `NotFound`, because that is the kind
    /// the adapters map to 404. Swallowing the open error and reporting a
    /// generic internal failure instead turned that into a 500 and tripped the
    /// web adapter's LAN-hardening test.
    #[test]
    fn a_missing_file_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = images(dir.path());
        let err = chapter_html(
            BookFormat::Epub,
            &dir.path().join("nope.epub").to_string_lossy(),
            0,
            &store,
            "bk",
            &caches(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "NotFound", "unexpected error: {err}");
    }

    #[test]
    fn image_only_formats_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = images(dir.path());
        for format in [BookFormat::Pdf, BookFormat::Cbz, BookFormat::Cbr] {
            let name = format.to_string();
            let err = chapter_html(format, "/nope", 0, &store, "bk", &caches()).unwrap_err();
            assert!(
                err.to_string().contains("not supported"),
                "unexpected error for {name}: {err}"
            );
        }
    }
}

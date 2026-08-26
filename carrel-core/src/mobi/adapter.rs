//! High-level adapter mapping libmobi output to Carrel's EPUB-style model.
//!
//! The wrapper in [`super`] exposes libmobi primitives (parts, resources,
//! cover); this module composes them into the same interface the Reader
//! already uses for EPUBs — metadata, ordered chapters, sanitized HTML with
//! inline-image URLs in whatever scheme the caller's policy asks for
//! (`asset://` for the desktop — see [`crate::reader::asset_localhost_url`]),
//! and per-chapter word counts.
//!
//! The module has no Tauri / IPC dependencies; it is consumed by both the
//! desktop commands and the OPDS web server.

use std::collections::HashMap;
use std::path::Path;
use std::str;

use ammonia::clean;

use super::{MobiBook, PartKind};
use crate::epub::{count_words, strip_html_tags, BookMetadata, ChapterInfo, ExtractedCover};
use crate::error::{CarrelError, CarrelResult};
use crate::models::ChapterMeta;
use crate::storage::Storage;

/// Metadata collected from the MOBI/AZW/AZW3 EXTH records, shaped to match
/// `epub::BookMetadata` so the import path stays polymorphic.
pub type MobiMetadata = BookMetadata;

/// Parse a MOBI/AZW/AZW3 file and return the metadata needed to build a
/// `Book` row at import time. Title and author fall back to empty strings
/// (the Book-building layer substitutes the filename stem) rather than
/// erroring, which matches the EPUB adapter's behavior on minimal files.
pub fn parse_mobi_metadata(file_path: &str) -> CarrelResult<MobiMetadata> {
    let book = MobiBook::open(Path::new(file_path))?;
    Ok(BookMetadata {
        title: book.title().unwrap_or_default(),
        author: book.author().unwrap_or_default(),
        language: book.language().unwrap_or_default(),
        description: book.description(),
        isbn: book.isbn(),
        genres: parse_subject_list(book.subject()),
    })
}

/// Extract the full-size cover image, if the EXTH cover record is present
/// and points at a recognizable raster. See [`super::MobiBook::cover`] for
/// the detection rules.
pub fn extract_cover(file_path: &str) -> CarrelResult<Option<ExtractedCover>> {
    let book = MobiBook::open(Path::new(file_path))?;
    Ok(book.cover().map(|c| ExtractedCover {
        bytes: c.data.to_vec(),
        ext: c.extension.to_string(),
    }))
}

/// Ordered list of reading-order HTML chapters. For KF8 this is the per-
/// file split libmobi reconstructs from the SKEL/FRAG indexes; for legacy
/// Mobipocket it is a single blob.
///
/// Titles are synthetic ("Chapter N") — MOBI doesn't expose a reliable
/// per-chapter title without parsing the NCX, which is a separate
/// follow-up.
pub fn get_chapter_list(file_path: &str) -> CarrelResult<Vec<ChapterInfo>> {
    let cached = CachedMobiBook::open(file_path)?;
    Ok(get_chapter_list_from_cache(&cached))
}

/// Like [`get_chapter_list`] but operates on a pre-parsed [`CachedMobiBook`].
pub fn get_chapter_list_from_cache(cached: &CachedMobiBook) -> Vec<ChapterInfo> {
    cached
        .parts
        .iter()
        .enumerate()
        .map(|(index, part)| ChapterInfo {
            index,
            title: format!("Chapter {}", index + 1),
            href: format!("part{:05}.html", part.uid),
        })
        .collect()
}

/// Extract sanitized HTML for a single chapter and rewrite inline image
/// references (`src="resource{NNNNN}.{ext}"`) to `asset://` URLs pointing
/// at images extracted into `storage` under `{book_id}/{chapter_index}/`.
///
/// The `{book_id}/{chapter_index}/` layout matches the EPUB adapter and
/// the web reader's asset-serving contract (`api.rs` resolves inline
/// images via `/api/books/{id}/images/{chapter}/{filename}`), so the same
/// extracted files back both surfaces. The *URLs* are the desktop's
/// (`asset://`); the web server reads through
/// [`get_chapter_content_from_cache_with_url_policy`] and supplies its own.
///
/// Only raster formats (jpeg/png/gif/bmp) are rewritten; SVG and CSS
/// references are left alone because they aren't meaningful for the
/// Reader's current render path. Images whose write to `storage` fails
/// (disk full, permission denied) keep their original `resource…`
/// reference so a single broken asset doesn't poison the whole chapter
/// with URLs that point at files that never landed on disk.
pub fn get_chapter_content(
    file_path: &str,
    chapter_index: usize,
    storage: &dyn Storage,
    book_id: &str,
) -> CarrelResult<String> {
    let cached = CachedMobiBook::open(file_path)?;
    get_chapter_content_from_cache(&cached, chapter_index, storage, book_id)
}

/// Like [`get_chapter_content`] but operates on a pre-parsed
/// [`CachedMobiBook`], avoiding the libmobi reopen/reparse on each call.
/// Used by desktop hot paths (continuous-scroll loads, per-chapter
/// search) where the same book is touched many times in a row.
///
/// Emits the desktop's [`crate::reader::asset_localhost_url`] URLs. This
/// signature is frozen for out-of-repo consumers, so a caller that needs
/// different image URLs uses
/// [`get_chapter_content_from_cache_with_url_policy`] instead.
pub fn get_chapter_content_from_cache(
    cached: &CachedMobiBook,
    chapter_index: usize,
    storage: &dyn Storage,
    book_id: &str,
) -> CarrelResult<String> {
    get_chapter_content_from_cache_with_url_policy(
        cached,
        chapter_index,
        storage,
        book_id,
        &crate::reader::asset_localhost_url,
    )
}

/// [`get_chapter_content_from_cache`] with the inline-image URL policy
/// supplied by the caller instead of hard-coded here (M5).
///
/// The MOBI half of the same change as
/// [`crate::epub::get_chapter_content_from_cache_with_url_policy`] — see that
/// function and [`crate::reader`]'s module docs. Both formats emit these URLs,
/// so threading the policy through EPUB alone would have left the web reader
/// with broken images for MOBI books only.
pub fn get_chapter_content_from_cache_with_url_policy(
    cached: &CachedMobiBook,
    chapter_index: usize,
    storage: &dyn Storage,
    book_id: &str,
    image_url: &dyn Fn(&str, &std::path::Path) -> String,
) -> CarrelResult<String> {
    let part = cached.parts.get(chapter_index).ok_or_else(|| {
        CarrelError::invalid(format!(
            "MOBI chapter index {chapter_index} out of range (have {} HTML parts)",
            cached.parts.len()
        ))
    })?;
    let image_resources = cached.image_resources_borrowed();
    render_chapter_html(
        &part.data,
        &image_resources,
        storage,
        book_id,
        chapter_index,
        image_url,
    )
}

/// Sanitize and rewrite a single chapter's HTML. Shared between the
/// path-based [`get_chapter_content`] and the cache-based
/// [`get_chapter_content_from_cache`] so both paths use identical
/// rendering — the only difference is where the input bytes come from.
fn render_chapter_html(
    part_data: &[u8],
    image_resources: &HashMap<String, &[u8]>,
    storage: &dyn Storage,
    book_id: &str,
    chapter_index: usize,
    image_url: &dyn Fn(&str, &std::path::Path) -> String,
) -> CarrelResult<String> {
    // Sanitize first so ammonia strips the XML prologue, `<html>/<head>/
    // <body>`, scripts, and external link rels before our rewriter sees
    // the markup. The result is fragment-level HTML suitable for the
    // Reader pane — and it has to happen before the image URLs are
    // injected, because `asset://` is not in ammonia's default scheme
    // allowlist. See [`crate::epub::get_chapter_content`].
    let raw_html = str::from_utf8(part_data)
        .map_err(|e| CarrelError::invalid(format!("MOBI chapter is not valid UTF-8: {e}")))?;
    let cleaned = clean(raw_html);

    let key_prefix = format!("{book_id}/{chapter_index}");
    Ok(rewrite_mobi_image_refs_with_url_policy(
        &cleaned,
        storage,
        &key_prefix,
        image_resources,
        image_url,
    ))
}

/// Per-chapter word counts, matching `epub::get_chapter_word_counts`.
/// Used by the Reader to draw the reading-progress bar.
pub fn get_chapter_word_counts(file_path: &str) -> CarrelResult<Vec<usize>> {
    let cached = CachedMobiBook::open(file_path)?;
    get_chapter_word_counts_from_cache(&cached)
}

/// Like [`get_chapter_word_counts`] but operates on a pre-parsed
/// [`CachedMobiBook`].
pub fn get_chapter_word_counts_from_cache(cached: &CachedMobiBook) -> CarrelResult<Vec<usize>> {
    cached
        .parts
        .iter()
        .map(|p| chapter_word_count(&p.data))
        .collect()
}

fn chapter_word_count(html_bytes: &[u8]) -> CarrelResult<usize> {
    let html = str::from_utf8(html_bytes)
        .map_err(|e| CarrelError::invalid(format!("MOBI chapter is not valid UTF-8: {e}")))?;
    let cleaned = clean(html);
    Ok(count_words(&strip_html_tags(&cleaned)))
}

/// Combined chapter index, title, and word count in a single pass.
pub fn get_chapter_metadata_batch_from_cache(
    cached: &CachedMobiBook,
) -> CarrelResult<Vec<ChapterMeta>> {
    cached
        .parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            Ok(ChapterMeta {
                index,
                title: format!("Chapter {}", index + 1),
                word_count: chapter_word_count(&part.data)?,
            })
        })
        .collect()
}

/// Owned, parsed snapshot of a MOBI/AZW/AZW3 file: the bytes for every
/// HTML chapter and every referenced image resource, copied out of
/// libmobi's `MOBIData` and `MOBIRawml` structures so the libmobi
/// handles can be dropped before this struct is returned.
///
/// Why owned bytes:
/// - libmobi's borrowed slices tie the lifetime of `MobiPart::data` to
///   the `MobiRawml`, which itself borrows from `MobiBook`. Caching that
///   chain in an `LruCache<MobiBook>` would require a self-referential
///   struct or `ouroboros`; copying once at open time is simpler and
///   keeps the cache `Send` for use behind a Mutex.
/// - Re-running `mobi_load_filename` + `mobi_parse_rawml` for every
///   chapter request scales poorly on KF8 books because the SKEL/FRAG
///   reconstruction dominates per-call cost. Caching the post-parse
///   bytes amortizes that across page turns, search, and the
///   continuous-scroll/full-book read path.
pub struct CachedMobiBook {
    parts: Vec<CachedMobiPart>,
    image_resources: HashMap<String, Vec<u8>>,
}

/// One HTML markup part lifted out of libmobi's rawml. `uid` is preserved
/// so the synthesized chapter href (`partNNNNN.html`) stays stable
/// between the path-based and cache-based code paths.
struct CachedMobiPart {
    uid: usize,
    data: Vec<u8>,
}

impl CachedMobiBook {
    /// Parse a MOBI/AZW/AZW3 file from disk and copy its HTML parts and
    /// image resources into owned buffers. The libmobi `MobiBook` and
    /// `MobiRawml` are dropped before this returns.
    pub fn open(file_path: &str) -> CarrelResult<Self> {
        let book = MobiBook::open(Path::new(file_path))?;
        let rawml = book.rawml()?;
        let parts: Vec<CachedMobiPart> = rawml
            .parts()
            .filter(|p| matches!(p.kind, PartKind::Html))
            .map(|p| CachedMobiPart {
                uid: p.uid,
                data: p.data.to_vec(),
            })
            .collect();
        // Index image resources by their canonical filename
        // (`resource00001.jpg`) so the rewriter can look up only the ones
        // a given chapter actually references. Mirrors the layout the
        // path-based path already used.
        let image_resources: HashMap<String, Vec<u8>> = rawml
            .resources()
            .filter(|p| p.kind.is_image())
            .filter_map(|r| {
                let ext = r.kind.extension()?;
                Some((format!("resource{:05}.{ext}", r.uid), r.data.to_vec()))
            })
            .collect();
        Ok(Self {
            parts,
            image_resources,
        })
    }

    /// Number of HTML chapters in the cached book. Mirrors the length of
    /// `get_chapter_list_from_cache(self)`.
    pub fn chapter_count(&self) -> usize {
        self.parts.len()
    }

    /// Total bytes the cache entry holds: chapter HTML + image resources.
    /// Used by the desktop `mobi_cache` (`LruCache::insert_with_size`) so
    /// the LRU can evict by total memory rather than only entry count —
    /// without this, a few illustrated AZW3s at hundreds of MB each would
    /// pin gigabytes of resident memory.
    pub fn byte_size(&self) -> usize {
        let parts: usize = self.parts.iter().map(|p| p.data.len()).sum();
        let images: usize = self.image_resources.values().map(Vec::len).sum();
        parts + images
    }

    /// Build a borrowed view of the image resources for the rewriter,
    /// which uses `&[u8]` so it can also work with libmobi's borrowed
    /// data on the path-based code path.
    fn image_resources_borrowed(&self) -> HashMap<String, &[u8]> {
        self.image_resources
            .iter()
            .map(|(k, v)| (k.clone(), v.as_slice()))
            .collect()
    }
}

/// Walk the sanitized HTML looking for quoted attribute values that match
/// libmobi's `resource{NNNNN}.{ext}` naming, persist only the referenced
/// images to `storage`, and replace the references with the URLs `image_url`
/// returns (M5 — the desktop's `asset://localhost/…` is
/// [`crate::reader::asset_localhost_url`], one policy among others).
///
/// Persisting inside the rewriter — as opposed to eagerly writing every
/// image in the rawml — prevents on-disk amplification: illustrated MOBI
/// books with N chapters and M images would otherwise store N×M copies
/// instead of M (at most, one per chapter that references the image).
///
/// This is a byte-level scanner rather than a real HTML parser for two
/// reasons: (a) the input is already ammonia-cleaned so we know tags and
/// attribute quoting are normalized, and (b) the naming convention is
/// deterministic and restrictive enough that a false positive would
/// require a literal `"resource00000.jpg"` string inside text content,
/// which isn't a realistic collision.
fn rewrite_mobi_image_refs_with_url_policy(
    html: &str,
    storage: &dyn Storage,
    key_prefix: &str,
    image_resources: &HashMap<String, &[u8]>,
    image_url: &dyn Fn(&str, &std::path::Path) -> String,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("\"resource") {
        // Copy everything up to and including the opening quote, then try
        // to rewrite whatever the quoted value is.
        out.push_str(&rest[..=pos]);
        let after = &rest[pos + 1..];
        let Some(end) = after.find('"') else {
            // Unbalanced quotes — bail and emit the remainder verbatim so
            // we don't eat half the document on malformed input.
            out.push_str(after);
            return out;
        };
        let value = &after[..end];
        match mobi_resource_to_url(value, storage, key_prefix, image_resources, image_url) {
            Some(url) => out.push_str(&url),
            None => out.push_str(value),
        }
        out.push('"');
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Try to resolve a libmobi resource filename (`resource00001.jpg`) to the
/// URL `image_url` gives for it, persisting the underlying bytes to
/// `storage` on first use. Returns `None` when the reference does not match
/// libmobi's naming, when there is no matching entry in `image_resources`,
/// or when the persist fails — in each case the caller leaves the original
/// reference untouched so the Reader shows a single broken-image icon
/// rather than emitting a URL that points at a file that never landed on
/// disk. `image_url` is therefore never called for an image that isn't
/// there.
fn mobi_resource_to_url(
    value: &str,
    storage: &dyn Storage,
    key_prefix: &str,
    image_resources: &HashMap<String, &[u8]>,
    image_url: &dyn Fn(&str, &std::path::Path) -> String,
) -> Option<String> {
    let rest = value.strip_prefix("resource")?;
    let (digits, ext) = rest.split_once('.')?;
    // libmobi always pads the numeric id to 5 digits.
    if digits.len() != 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" => {}
        _ => return None,
    }
    let bytes = image_resources.get(value)?;
    let key = format!("{key_prefix}/{value}");
    if !storage.exists(&key).unwrap_or(false) {
        if let Err(e) = storage.put(&key, bytes) {
            log::warn!("failed to persist MOBI image resource {key}: {e}");
            return None;
        }
    }
    let path = storage.local_path(&key).ok()?;
    Some(image_url(&key, &path))
}

/// Split a MOBI subject string into a list of genres. EXTH subject fields
/// can be either a single comma/semicolon-separated string or a single
/// untouched genre — we split conservatively and trim whitespace.
fn parse_subject_list(subject: Option<String>) -> Vec<String> {
    let Some(s) = subject else {
        return Vec::new();
    };
    s.split([';', ','])
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn fixture(name: &str) -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("src-tauri")
            .join("test-fixtures")
            .join(name);
        path.exists().then_some(path)
    }

    #[test]
    fn parse_mobi_metadata_extracts_title_and_author() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let meta = parse_mobi_metadata(path.to_str().unwrap()).expect("parse");
        assert!(meta.title.to_lowercase().contains("alice"));
        assert!(meta.author.to_lowercase().contains("carroll"));
        assert_eq!(meta.language, "en");
    }

    #[test]
    fn extract_cover_yields_non_empty_bytes() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let cover = extract_cover(path.to_str().unwrap())
            .expect("cover result")
            .expect("cover present");
        assert!(!cover.bytes.is_empty());
        assert!(
            matches!(cover.ext.as_str(), "jpg" | "png" | "gif" | "bmp"),
            "ext: {}",
            cover.ext
        );
    }

    #[test]
    fn get_chapter_list_kf8_yields_per_file_split() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let chapters = get_chapter_list(path.to_str().unwrap()).expect("chapters");
        assert!(
            chapters.len() > 1,
            "KF8 alice.mobi should split into multiple chapters, got {}",
            chapters.len()
        );
        assert_eq!(chapters[0].index, 0);
        assert!(chapters[0].href.starts_with("part"));
    }

    #[test]
    fn get_chapter_list_legacy_yields_single_chapter() {
        let Some(path) = fixture("alice-legacy.mobi") else {
            return;
        };
        let chapters = get_chapter_list(path.to_str().unwrap()).expect("chapters");
        assert_eq!(chapters.len(), 1, "legacy MOBI has one markup blob");
    }

    #[test]
    fn get_chapter_content_sanitizes_and_returns_html() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let html = get_chapter_content(path.to_str().unwrap(), 1, &storage, "bk").expect("chapter");
        // Ammonia strips <?xml?> / <html>/<head>/<body> wrappers, leaving
        // fragment-level HTML.
        assert!(!html.is_empty());
        assert!(
            !html.contains("<?xml"),
            "XML prologue should have been stripped"
        );
        assert!(
            !html.contains("<head>"),
            "head wrapper should have been stripped"
        );
    }

    #[test]
    fn get_chapter_word_counts_is_positive_for_body_chapter() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let counts = get_chapter_word_counts(path.to_str().unwrap()).expect("word counts");
        assert!(!counts.is_empty());
        // The cover-only first part has very few words; later body
        // chapters should have many more.
        assert!(
            counts.iter().any(|&c| c > 200),
            "expected at least one chapter with >200 words, got {:?}",
            &counts[..counts.len().min(5)]
        );
    }

    #[test]
    fn chapter_word_count_rejects_invalid_utf8() {
        let err = chapter_word_count(b"\xFF\xFEbroken").expect_err("invalid utf8 must error");
        assert!(
            err.to_string().contains("not valid UTF-8"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rewrite_mobi_image_refs_replaces_resource_urls() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFFdata";
        let mut resources: HashMap<String, &[u8]> = HashMap::new();
        resources.insert("resource00000.jpg".into(), bytes);

        let html = r#"<p><img src="resource00000.jpg" alt="x"/></p>"#;
        let out = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url,
        );
        assert!(
            out.contains("asset://localhost/"),
            "expected asset:// URL, got {out}"
        );
        assert!(
            !out.contains("\"resource00000.jpg\""),
            "original reference should have been replaced, got {out}"
        );
        // Persisted under the per-chapter key as a side effect of the
        // rewrite.
        assert!(storage.exists("bk/0/resource00000.jpg").unwrap());
    }

    /// M5: the MOBI arm's URL policy is the caller's too. Both formats emit
    /// these URLs, so a policy threaded through EPUB alone would leave the
    /// web reader with broken images for MOBI books only — and no EPUB test
    /// would catch it. It has to be a synthetic resource map rather than a
    /// fixture read: `alice.mobi`'s 19 chapters reference no image resources
    /// at all (verified by driving `get_chapter_content` over every chapter
    /// and finding storage empty), so a fixture-based test could not tell a
    /// working policy from a missing one.
    #[test]
    fn rewrite_mobi_image_refs_applies_the_callers_url_policy() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFFdata";
        let mut resources: HashMap<String, &[u8]> = HashMap::new();
        resources.insert("resource00000.jpg".into(), bytes);

        let html = r#"<p><img src="resource00000.jpg" alt="x"/></p>"#;
        let out = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/0",
            &resources,
            &|key: &str, _local_path: &std::path::Path| format!("web:{key}"),
        );

        assert_eq!(
            out,
            r#"<p><img src="web:bk/0/resource00000.jpg" alt="x"/></p>"#
        );
        assert!(
            !out.contains("asset://"),
            "core must not inject a scheme of its own: {out}"
        );
        // Still persisted under the per-chapter key — the policy decides the
        // URL, not whether the bytes land on disk.
        assert!(storage.exists("bk/0/resource00000.jpg").unwrap());
    }

    #[test]
    fn rewrite_mobi_image_refs_skips_rewrite_when_resource_not_in_rawml() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        // Empty resources map: the chapter HTML references
        // `resource00000.jpg` but the MOBI rawml didn't yield a matching
        // entry (corrupt book, or a stale reference left over from a
        // different language split). The reference is left as-is so the
        // Reader shows a broken-image icon rather than a bogus
        // `asset://` URL pointing at nothing.
        let resources: HashMap<String, &[u8]> = HashMap::new();
        let html = r#"<p><img src="resource00000.jpg" alt="x"/></p>"#;
        let out = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url,
        );
        assert!(out.contains("\"resource00000.jpg\""), "got {out}");
        assert!(!out.contains("asset://localhost"), "got {out}");
        // Nothing was persisted — the chapter doesn't own this image.
        assert!(!storage.exists("bk/0/resource00000.jpg").unwrap());
    }

    #[test]
    fn rewrite_mobi_image_refs_writes_only_images_referenced_by_chapter() {
        // Regression test for the per-chapter image duplication bug:
        // a book with dozens of images would bulk-write every one into
        // every chapter dir on every `get_chapter_content` call. The
        // fix persists only images the chapter HTML actually references.
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let img0: &[u8] = b"\xFF\xD8\xFFzero";
        let img1: &[u8] = b"\xFF\xD8\xFFone";
        let img2: &[u8] = b"\xFF\xD8\xFFtwo";
        let mut resources: HashMap<String, &[u8]> = HashMap::new();
        resources.insert("resource00000.jpg".into(), img0);
        resources.insert("resource00001.jpg".into(), img1);
        resources.insert("resource00002.jpg".into(), img2);

        let html = r#"<p><img src="resource00001.jpg"/></p>"#;
        let _ = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/5",
            &resources,
            &crate::reader::asset_localhost_url,
        );

        assert!(
            !storage.exists("bk/5/resource00000.jpg").unwrap(),
            "unreferenced image must not be persisted"
        );
        assert!(
            storage.exists("bk/5/resource00001.jpg").unwrap(),
            "referenced image must be persisted"
        );
        assert!(
            !storage.exists("bk/5/resource00002.jpg").unwrap(),
            "unreferenced image must not be persisted"
        );
    }

    #[test]
    fn rewrite_mobi_image_refs_persists_each_image_once_even_when_referenced_twice() {
        // A chapter that references the same image twice should still
        // only call `put` once — the `exists` check is what makes this
        // idempotent and avoids redundant disk I/O.
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFFtwice";
        let mut resources: HashMap<String, &[u8]> = HashMap::new();
        resources.insert("resource00001.jpg".into(), bytes);

        let html = r#"<img src="resource00001.jpg"/><img src="resource00001.jpg"/>"#;
        let out = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url,
        );

        assert!(storage.exists("bk/0/resource00001.jpg").unwrap());
        // Both references rewritten to an asset URL.
        assert_eq!(out.matches("asset://localhost/").count(), 2);
    }

    #[test]
    fn rewrite_mobi_image_refs_leaves_non_image_refs_alone() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let resources: HashMap<String, &[u8]> = HashMap::new();
        let html = r#"<link href="flow00001.css"/><img src="resource00001.svg"/><img src="flow00004.svg"/>"#;
        let out = rewrite_mobi_image_refs_with_url_policy(
            html,
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url,
        );
        // CSS and SVG references pass through untouched.
        assert!(out.contains("flow00001.css"));
        assert!(out.contains("flow00004.svg"));
        assert!(out.contains("resource00001.svg"));
        assert!(!out.contains("asset://localhost"));
    }

    #[test]
    fn parse_subject_list_splits_and_trims() {
        assert!(parse_subject_list(None).is_empty());
        assert_eq!(
            parse_subject_list(Some("Fantasy; Adventure".into())),
            vec!["Fantasy", "Adventure"]
        );
        assert_eq!(
            parse_subject_list(Some("Sci-fi, Space opera,  ".into())),
            vec!["Sci-fi", "Space opera"]
        );
        assert_eq!(parse_subject_list(Some("  ".into())), Vec::<String>::new());
    }

    #[test]
    fn mobi_resource_to_asset_url_rejects_non_image_extensions() {
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let bytes: &[u8] = b"data";
        let mut resources: HashMap<String, &[u8]> = HashMap::new();
        resources.insert("resource00000.css".into(), bytes);
        resources.insert("resource00000.svg".into(), bytes);
        resources.insert("resource00.jpg".into(), bytes);
        resources.insert("flow00000.jpg".into(), bytes);
        assert!(mobi_resource_to_url(
            "resource00000.css",
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url
        )
        .is_none());
        assert!(mobi_resource_to_url(
            "resource00000.svg",
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url
        )
        .is_none());
        assert!(mobi_resource_to_url(
            "resource00.jpg",
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url
        )
        .is_none());
        assert!(mobi_resource_to_url(
            "flow00000.jpg",
            &storage,
            "bk/0",
            &resources,
            &crate::reader::asset_localhost_url
        )
        .is_none());
    }

    #[test]
    fn get_chapter_content_only_persists_referenced_images_per_chapter() {
        // End-to-end version of the duplication regression test: drive
        // `get_chapter_content` over every MOBI chapter and confirm the
        // total image count in storage is bounded by the per-chapter
        // references, not by N_chapters × N_images.
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let book_id = "bk";

        let chapter_count = get_chapter_list(path.to_str().unwrap())
            .expect("chapters")
            .len();
        let total_images = {
            let book = MobiBook::open(&path).expect("open MOBI");
            let rawml = book.rawml().expect("rawml");
            rawml.resources().filter(|p| p.kind.is_image()).count()
        };

        for ch in 0..chapter_count {
            let _ = get_chapter_content(path.to_str().unwrap(), ch, &storage, book_id)
                .expect("get_chapter_content");
        }

        let keys = storage.list(&format!("{book_id}/")).expect("list storage");
        let worst_case_amplified = chapter_count * total_images;
        assert!(
            keys.len() < worst_case_amplified,
            "after fix, total image writes ({}) should be strictly less than \
             the worst-case duplicated count ({})",
            keys.len(),
            worst_case_amplified
        );
    }

    /// End-to-end smoke test that drives the full MOBI pipeline the Reader
    /// exercises at runtime: metadata, cover, chapter list, sanitized HTML
    /// content, word counts, and full-text search. If any single step
    /// regresses this test fails with a pointed message rather than
    /// surfacing at import time in the desktop app.
    ///
    /// Both MOBI variants are covered: legacy Mobipocket (`alice-legacy.mobi`,
    /// file version 6) and KF8 / AZW3 (`alice.mobi`, file version 8). Run
    /// `scripts/fetch-mobi-test-corpus.sh` to populate the fixtures before
    /// running the test; without the fixtures the test is skipped.
    fn run_full_pipeline_smoke(fixture_name: &str, expected_file_version: usize) {
        use crate::search::search_chapters;
        use crate::storage::LocalStorage;

        let Some(path) = fixture(fixture_name) else {
            eprintln!(
                "skipping: {fixture_name} not present — run scripts/fetch-mobi-test-corpus.sh"
            );
            return;
        };
        let path_str = path.to_str().expect("fixture path is UTF-8");

        // 1. Open and sanity-check the file variant. Mismatched file
        //    versions almost always mean the fetch script pulled the
        //    wrong URL (KF8 vs legacy are both served under `.mobi`).
        let book = MobiBook::open(&path).expect("open MOBI");
        assert_eq!(
            book.file_version(),
            expected_file_version,
            "{fixture_name}: expected file version {expected_file_version}, got {}",
            book.file_version()
        );

        // 2. Metadata round-trip. Gutenberg ships Alice with title and
        //    author populated; an empty string means the EXTH record
        //    didn't decode and downstream imports would show the
        //    filename as the title.
        let meta = parse_mobi_metadata(path_str).expect("parse metadata");
        assert!(
            meta.title.to_lowercase().contains("alice"),
            "title missing 'alice': {:?}",
            meta.title
        );
        assert!(
            meta.author.to_lowercase().contains("carroll"),
            "author missing 'carroll': {:?}",
            meta.author
        );

        // 3. Cover extraction. Alice ships with a cover; a missing one
        //    here means EXTH traversal broke.
        let cover = extract_cover(path_str)
            .expect("cover result")
            .expect("alice ships with a cover");
        assert!(!cover.bytes.is_empty(), "cover bytes are empty");

        // 4. Chapter list. Both variants must produce at least one
        //    chapter; KF8 splits into multiple markup parts via SKEL/FRAG.
        let chapters = get_chapter_list(path_str).expect("chapter list");
        assert!(!chapters.is_empty(), "chapter list is empty");

        // 5. Chapter content is fragment-level sanitized HTML with the
        //    XML prologue and document wrappers stripped.
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let book_id = "smoke-book";
        let html = get_chapter_content(path_str, 0, &storage, book_id).expect("chapter 0");
        assert!(!html.is_empty(), "chapter 0 is empty");
        assert!(
            !html.contains("<?xml"),
            "chapter 0 still has XML prologue: {html:.200}"
        );

        // 6. Word counts are populated — the Reader uses these to paint
        //    the progress bar. At least one chapter should be > 50
        //    words (Alice is a novel).
        let counts = get_chapter_word_counts(path_str).expect("word counts");
        assert_eq!(counts.len(), chapters.len(), "word counts ≠ chapters");
        assert!(
            counts.iter().any(|&c| c > 50),
            "no chapter with > 50 words: {counts:?}"
        );

        // 7. Search surface: "Alice" must match in at least one chapter.
        //    Uses the same aggregator the MOBI arm of `search_book_content`
        //    uses in production, so a regression there is caught here.
        let results = search_chapters(0..(chapters.len() as u32), "Alice", book_id, |idx| {
            get_chapter_content(path_str, idx as usize, &storage, book_id)
        })
        .expect("search");
        assert!(
            !results.is_empty(),
            "'Alice' not found in any chapter of {fixture_name}"
        );
    }

    #[test]
    fn mobi_kf8_pipeline_smoke() {
        run_full_pipeline_smoke("alice.mobi", 8);
    }

    #[test]
    fn mobi_legacy_pipeline_smoke() {
        run_full_pipeline_smoke("alice-legacy.mobi", 6);
    }

    /// `CachedMobiBook::open` must produce the same chapter count as the
    /// path-based `get_chapter_list`. This is the structural invariant
    /// the cache relies on — every from_cache variant addresses chapters
    /// by index, so a divergence here would silently shift content under
    /// the Reader.
    #[test]
    fn cached_mobi_book_chapter_count_matches_path_version() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let chapters = get_chapter_list(path.to_str().unwrap()).expect("chapters");
        let cached = CachedMobiBook::open(path.to_str().unwrap()).expect("open cached");
        assert_eq!(cached.chapter_count(), chapters.len());
    }

    /// Round-trip equivalence: `get_chapter_list_from_cache` returns the
    /// same vec as the path-based `get_chapter_list`. Indexes, synthetic
    /// titles, and `partNNNNN.html` hrefs (which encode libmobi's part
    /// uid) must all match — anything else means the desktop and OPDS
    /// paths see different views of the same book.
    #[test]
    fn get_chapter_list_from_cache_matches_path_version() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let path_chapters = get_chapter_list(path.to_str().unwrap()).expect("chapters");
        let cached = CachedMobiBook::open(path.to_str().unwrap()).expect("open cached");
        let cached_chapters = get_chapter_list_from_cache(&cached);
        assert_eq!(cached_chapters.len(), path_chapters.len());
        for (a, b) in cached_chapters.iter().zip(path_chapters.iter()) {
            assert_eq!(a.index, b.index);
            assert_eq!(a.title, b.title);
            assert_eq!(a.href, b.href, "synthesized hrefs must match");
        }
    }

    /// Round-trip equivalence: `get_chapter_word_counts_from_cache` must
    /// return the same per-chapter counts as the path-based version. The
    /// Reader's progress bar consumes these values, so any divergence
    /// would visibly mis-paint the gauge.
    #[test]
    fn get_chapter_word_counts_from_cache_matches_path_version() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let path_counts = get_chapter_word_counts(path.to_str().unwrap()).expect("path counts");
        let cached = CachedMobiBook::open(path.to_str().unwrap()).expect("open cached");
        let cached_counts = get_chapter_word_counts_from_cache(&cached).expect("cached counts");
        assert_eq!(cached_counts, path_counts);
    }

    /// Round-trip equivalence: `get_chapter_content_from_cache` must
    /// return byte-identical HTML to the path-based version for every
    /// chapter. Image rewriting writes to the same on-disk layout because
    /// both code paths funnel through `render_chapter_html`, which uses
    /// the `{book_id}/{chapter_index}` key prefix.
    #[test]
    fn get_chapter_content_from_cache_matches_path_version() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        // Two storage roots so the rewriter's idempotency check
        // (`storage.exists` before `put`) can't influence the text the
        // function returns. Both calls write fresh.
        let dir_a = tempdir().unwrap();
        let storage_a = LocalStorage::new(dir_a.path().to_path_buf()).unwrap();
        let dir_b = tempdir().unwrap();
        let storage_b = LocalStorage::new(dir_b.path().to_path_buf()).unwrap();
        let book_id = "round-trip";

        let cached = CachedMobiBook::open(path.to_str().unwrap()).expect("open cached");
        let chapter_count = cached.chapter_count();
        assert!(chapter_count > 0, "fixture must yield at least one chapter");

        for ch in 0..chapter_count {
            let path_html =
                get_chapter_content(path.to_str().unwrap(), ch, &storage_a, book_id).expect("path");
            let cache_html =
                get_chapter_content_from_cache(&cached, ch, &storage_b, book_id).expect("cached");
            assert_eq!(
                path_html, cache_html,
                "chapter {ch} content diverged between path and cache paths"
            );
        }
    }

    /// `get_chapter_content_from_cache` must surface the same out-of-
    /// range error the path-based version returns, so callers don't need
    /// to special-case the cached arm.
    #[test]
    fn get_chapter_content_from_cache_rejects_out_of_range_index() {
        let Some(path) = fixture("alice.mobi") else {
            return;
        };
        let dir = tempdir().unwrap();
        let storage = LocalStorage::new(dir.path().to_path_buf()).unwrap();
        let cached = CachedMobiBook::open(path.to_str().unwrap()).expect("open cached");
        let bad_index = cached.chapter_count() + 10;
        let err = get_chapter_content_from_cache(&cached, bad_index, &storage, "bk")
            .expect_err("out-of-range index must error");
        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {err}"
        );
    }
}

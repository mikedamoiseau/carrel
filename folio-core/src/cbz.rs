use base64::{engine::general_purpose, Engine as _};
use std::path::Path;
use zip::ZipArchive;

use crate::error::{FolioError, FolioResult};

fn is_image(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".webp")
        || lower.ends_with(".gif")
}

fn collect_image_names(archive: &mut ZipArchive<std::fs::File>) -> Vec<String> {
    let mut names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let file = archive.by_index(i).ok()?;
            let name = file.name().to_string();
            // Skip macOS resource forks, directory entries, and non-image files.
            if name.starts_with("__MACOSX/") || name.ends_with('/') || !is_image(&name) {
                return None;
            }
            Some(name)
        })
        .collect();
    names.sort();
    names
}

fn open_archive(path: &str) -> FolioResult<ZipArchive<std::fs::File>> {
    let file =
        std::fs::File::open(path).map_err(|e| FolioError::io(format!("Cannot open file: {e}")))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| FolioError::invalid(format!("Not a valid ZIP/CBZ archive: {e}")))?;
    crate::epub::validate_archive(&mut archive)?;
    Ok(archive)
}

/// Read one page entry, bounded by [`crate::epub::MAX_ENTRY_SIZE`].
///
/// Shared by [`get_page_image`] and [`get_page_image_bytes`] so both page
/// readers are capped identically. The cap is what stops a page that understates
/// its decompressed size in the central directory — which `open_archive`'s
/// pre-scan cannot detect — from expanding at deflate's full ratio.
fn read_page_bytes(archive: &mut ZipArchive<std::fs::File>, name: &str) -> FolioResult<Vec<u8>> {
    let entry = archive
        .by_name(name)
        .map_err(|e| FolioError::not_found(format!("Cannot read page '{name}': {e}")))?;
    Ok(crate::epub::read_entry_capped(
        entry,
        crate::epub::MAX_ENTRY_SIZE,
        name,
    )?)
}

#[derive(Debug)]
pub struct CbzMeta {
    pub title: String,
    pub page_count: u32,
    pub author: Option<String>,
    pub year: Option<u16>,
    pub series: Option<String>,
    pub volume: Option<u32>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    pub summary: Option<String>,
    pub genre: Option<String>,
}

/// Opens a CBZ archive and returns its title (filename stem) and page count.
/// Also parses ComicInfo.xml if present for additional metadata.
/// Returns an error if the file is not a valid ZIP or contains no supported images.
pub fn import_cbz(path: &str) -> FolioResult<CbzMeta> {
    let mut archive = open_archive(path)?;
    let images = collect_image_names(&mut archive);
    if images.is_empty() {
        return Err(FolioError::invalid(
            "CBZ archive contains no supported image files",
        ));
    }
    let title = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    // Try to parse ComicInfo.xml for metadata
    let mut author = None;
    let mut year = None;
    let mut comic_title = None;
    let mut series = None;
    let mut volume = None;
    let mut language = None;
    let mut publisher = None;
    let mut summary = None;
    let mut genre = None;
    if let Ok(entry) = archive.by_name("ComicInfo.xml") {
        // Bounded read: `open_archive`'s pre-scan only sees the size the central
        // directory declares, so an entry that understates it would otherwise
        // decompress unbounded. A limit breach is fatal; a non-UTF-8 ComicInfo
        // stays non-fatal (import proceeds without its metadata), as before.
        let bytes = crate::epub::read_entry_capped(
            entry,
            crate::epub::MAX_TEXT_ENTRY_SIZE,
            "ComicInfo.xml",
        )?;
        if let Ok(xml) = String::from_utf8(bytes) {
            if let Some(writer) = crate::epub::extract_tag_text_decoded(&xml, "Writer") {
                author = Some(writer);
            }
            if let Some(t) = crate::epub::extract_tag_text_decoded(&xml, "Title") {
                comic_title = Some(t);
            }
            if let Some(y) = crate::epub::extract_tag_text(&xml, "Year") {
                year = y.parse::<u16>().ok();
            }
            series = crate::epub::extract_tag_text_decoded(&xml, "Series");
            volume =
                crate::epub::extract_tag_text(&xml, "Volume").and_then(|v| v.parse::<u32>().ok());
            language = crate::epub::extract_tag_text(&xml, "LanguageISO").map(|s| s.to_string());
            publisher = crate::epub::extract_tag_text_decoded(&xml, "Publisher");
            summary = crate::epub::extract_tag_text_decoded(&xml, "Summary");
            genre = crate::epub::extract_tag_text_decoded(&xml, "Genre");
        }
    }

    Ok(CbzMeta {
        title: comic_title.unwrap_or(title),
        page_count: images.len() as u32,
        author,
        year,
        series,
        volume,
        language,
        publisher,
        summary,
        genre,
    })
}

/// Returns the number of image pages in a CBZ archive.
pub fn get_page_count(path: &str) -> FolioResult<u32> {
    let mut archive = open_archive(path)?;
    let images = collect_image_names(&mut archive);
    Ok(images.len() as u32)
}

/// Canonical sorted page-entry names for a CBZ, using the exact same
/// filter/sort as [`get_page_image_bytes`]. The page cache relies on this
/// so a page cached at index `i` is byte-identical to an on-demand read of
/// index `i` (see `page_cache`).
pub(crate) fn collect_page_names(path: &str) -> FolioResult<Vec<String>> {
    let mut archive = open_archive(path)?;
    Ok(collect_image_names(&mut archive))
}

/// Extracts a single page image and returns it as a base64 data URI
/// (e.g. `data:image/jpeg;base64,...`).
pub fn get_page_image(path: &str, page_index: u32) -> FolioResult<String> {
    let mut archive = open_archive(path)?;
    let images = collect_image_names(&mut archive);

    let name = images
        .get(page_index as usize)
        .ok_or_else(|| {
            FolioError::not_found(format!(
                "Page index {page_index} out of range (total pages: {})",
                images.len()
            ))
        })?
        .clone();

    let data = read_page_bytes(&mut archive, &name)?;

    let lower = name.to_lowercase();
    let mime = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    };

    let encoded = general_purpose::STANDARD.encode(&data);
    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Extracts a single page image and returns raw bytes + mime type.
/// Avoids the base64 encode/decode round-trip for web serving.
///
/// When `target_width` is `Some(w)` and the source image is wider than
/// `w`, the page is downscaled to width `w` (preserving aspect) and
/// re-encoded as JPEG. When `None`, or the source is already at or
/// below the target, the original archive bytes are returned untouched.
pub fn get_page_image_bytes(
    path: &str,
    page_index: u32,
    target_width: Option<u32>,
) -> FolioResult<(Vec<u8>, String)> {
    let mut archive = open_archive(path)?;
    let images = collect_image_names(&mut archive);

    let name = images
        .get(page_index as usize)
        .ok_or_else(|| {
            FolioError::not_found(format!(
                "Page index {page_index} out of range (total pages: {})",
                images.len()
            ))
        })?
        .clone();

    let data = read_page_bytes(&mut archive, &name)?;

    let lower = name.to_lowercase();
    let mime = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    };

    crate::image_util::maybe_resize_to_jpeg(data, mime.to_string(), target_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn is_image_accepts_common_formats() {
        assert!(is_image("page01.jpg"));
        assert!(is_image("page02.jpeg"));
        assert!(is_image("page03.png"));
        assert!(is_image("page04.webp"));
        assert!(is_image("page05.gif"));
    }

    #[test]
    fn is_image_case_insensitive() {
        assert!(is_image("cover.JPG"));
        assert!(is_image("cover.PNG"));
        assert!(is_image("cover.Webp"));
    }

    #[test]
    fn is_image_rejects_non_images() {
        assert!(!is_image("readme.txt"));
        assert!(!is_image("metadata.xml"));
        assert!(!is_image("comic.cbz"));
        assert!(!is_image(""));
    }

    #[test]
    fn collect_image_names_filters_and_sorts() {
        // Create a temp CBZ with known contents
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("test.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();

            // Add images in unsorted order
            zip.start_file("page03.jpg", options).unwrap();
            zip.write_all(b"fake jpg 3").unwrap();
            zip.start_file("page01.jpg", options).unwrap();
            zip.write_all(b"fake jpg 1").unwrap();
            zip.start_file("page02.png", options).unwrap();
            zip.write_all(b"fake png 2").unwrap();

            // Add non-image and macOS junk
            zip.start_file("__MACOSX/.DS_Store", options).unwrap();
            zip.write_all(b"junk").unwrap();
            zip.start_file("metadata.xml", options).unwrap();
            zip.write_all(b"<xml/>").unwrap();

            zip.finish().unwrap();
        }

        let mut archive = open_archive(cbz_path.to_str().unwrap()).unwrap();
        let names = collect_image_names(&mut archive);

        assert_eq!(names, vec!["page01.jpg", "page02.png", "page03.jpg"]);
    }

    #[test]
    fn import_cbz_extracts_title_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("My Comic Book.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("page01.jpg", options).unwrap();
            zip.write_all(b"fake").unwrap();
            zip.finish().unwrap();
        }

        let meta = import_cbz(cbz_path.to_str().unwrap()).unwrap();
        assert_eq!(meta.title, "My Comic Book");
        assert_eq!(meta.page_count, 1);
    }

    #[test]
    fn import_cbz_empty_archive_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("empty.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let zip = zip::ZipWriter::new(file);
            zip.finish().unwrap();
        }

        let result = import_cbz(cbz_path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no supported image files"));
    }

    #[test]
    fn get_page_image_returns_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("test.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("page01.png", options).unwrap();
            zip.write_all(b"fake png data").unwrap();
            zip.finish().unwrap();
        }

        let uri = get_page_image(cbz_path.to_str().unwrap(), 0).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn get_page_image_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("test.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("page01.jpg", options).unwrap();
            zip.write_all(b"fake").unwrap();
            zip.finish().unwrap();
        }

        let result = get_page_image(cbz_path.to_str().unwrap(), 5);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of range"));
    }

    #[test]
    fn import_cbz_with_accented_filename() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir
            .path()
            .join("Boule et Bill - 40 - Bill à Facettes (Cazenove, Bastide).cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("page01.jpg", options).unwrap();
            zip.write_all(b"fake jpg").unwrap();
            zip.finish().unwrap();
        }

        let meta = import_cbz(cbz_path.to_str().unwrap()).unwrap();
        assert!(meta.title.contains("Bill"));
        assert!(meta.title.contains("Facettes"));
        assert_eq!(meta.page_count, 1);
    }

    #[test]
    fn import_cbz_with_apostrophe_filename() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir
            .path()
            .join("Boule et Bill - 39 - Y a d'la Promenade dans l'Air.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("page01.jpg", options).unwrap();
            zip.write_all(b"fake jpg").unwrap();
            zip.finish().unwrap();
        }

        let meta = import_cbz(cbz_path.to_str().unwrap()).unwrap();
        assert!(meta.title.contains("Promenade"));
        assert_eq!(meta.page_count, 1);
    }

    #[test]
    fn import_cbz_with_accented_image_names_inside_archive() {
        let dir = tempfile::tempdir().unwrap();
        let cbz_path = dir.path().join("test_accents.cbz");
        {
            let file = std::fs::File::create(&cbz_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("café/page01.jpg", options).unwrap();
            zip.write_all(b"fake jpg 1").unwrap();
            zip.start_file("résumé/page02.jpg", options).unwrap();
            zip.write_all(b"fake jpg 2").unwrap();
            zip.finish().unwrap();
        }

        let meta = import_cbz(cbz_path.to_str().unwrap()).unwrap();
        assert_eq!(meta.page_count, 2);
    }

    // ---- Entry-read caps ----
    //
    // `open_archive` pre-scans the central directory, which only sees *declared*
    // sizes. An entry that understates its decompressed size passes that scan,
    // so the reads themselves have to be capped as well.

    /// Build a CBZ with the given entries. Bytes are deflated, so a highly
    /// compressible payload stays small on disk.
    fn build_cbz(dir: &std::path::Path, name: &str, entries: &[(&str, Vec<u8>)]) -> String {
        let cbz_path = dir.join(name);
        let file = std::fs::File::create(&cbz_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (entry_name, bytes) in entries {
            zip.start_file(*entry_name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        cbz_path.to_string_lossy().into_owned()
    }

    #[test]
    fn import_cbz_rejects_comicinfo_lying_about_decompressed_size() {
        let dir = tempfile::tempdir().unwrap();
        let oversize = crate::epub::MAX_TEXT_ENTRY_SIZE as usize + 1024;
        let path = build_cbz(
            dir.path(),
            "liar.cbz",
            &[
                ("page01.jpg", b"fake jpg".to_vec()),
                ("ComicInfo.xml", vec![b'A'; oversize]),
            ],
        );
        crate::epub::tests::forge_declared_size(&path, "ComicInfo.xml", 1024);

        let err = match import_cbz(&path) {
            Err(e) => e,
            Ok(_) => panic!("import_cbz accepted a size-understating ComicInfo.xml"),
        };
        assert!(
            matches!(err, FolioError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    #[test]
    fn page_reads_reject_entry_lying_about_decompressed_size() {
        // One fixture, both page readers: the payload has to exceed
        // MAX_ENTRY_SIZE to prove the cap, so building it twice is wasteful.
        let dir = tempfile::tempdir().unwrap();
        let oversize = crate::epub::MAX_ENTRY_SIZE as usize + 1024;
        let path = build_cbz(
            dir.path(),
            "liar.cbz",
            &[("page01.jpg", vec![b'A'; oversize])],
        );
        crate::epub::tests::forge_declared_size(&path, "page01.jpg", 1024);

        let bytes_err = match get_page_image_bytes(&path, 0, None) {
            Err(e) => e,
            Ok(_) => panic!("get_page_image_bytes accepted a size-understating page"),
        };
        assert!(
            matches!(bytes_err, FolioError::InvalidInput(_)),
            "expected InvalidInput, got {bytes_err:?}"
        );

        let uri_err = match get_page_image(&path, 0) {
            Err(e) => e,
            Ok(_) => panic!("get_page_image accepted a size-understating page"),
        };
        assert!(
            matches!(uri_err, FolioError::InvalidInput(_)),
            "expected InvalidInput, got {uri_err:?}"
        );
    }

    #[test]
    fn import_cbz_still_reads_normal_comicinfo() {
        let dir = tempfile::tempdir().unwrap();
        let path = build_cbz(
            dir.path(),
            "good.cbz",
            &[
                ("page01.jpg", b"fake jpg".to_vec()),
                (
                    "ComicInfo.xml",
                    br#"<ComicInfo><Title>Capped Comic</Title><Writer>A Writer</Writer><Year>2026</Year></ComicInfo>"#
                        .to_vec(),
                ),
            ],
        );

        let meta = import_cbz(&path).unwrap();
        assert_eq!(meta.title, "Capped Comic");
        assert_eq!(meta.author.as_deref(), Some("A Writer"));
        assert_eq!(meta.year, Some(2026));
    }
}

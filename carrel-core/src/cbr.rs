use base64::{engine::general_purpose, Engine as _};
use std::path::Path;

use crate::error::{CarrelError, CarrelResult};

/// Maximum number of entries allowed in a CBR archive.
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
/// Maximum decompressed size per archive entry (100 MB).
const MAX_ENTRY_SIZE: u64 = 100 * 1024 * 1024;

fn rar_err(ctx: &str, e: impl std::fmt::Display) -> CarrelError {
    CarrelError::invalid(format!("{ctx}: {e}"))
}

/// Validate a RAR archive: reject archives with too many entries or oversized entries.
fn validate_archive(path: &str) -> CarrelResult<()> {
    let archive = unrar::Archive::new(path)
        .open_for_listing()
        .map_err(|e| rar_err("Cannot open RAR archive", e))?;

    let mut count: usize = 0;
    for result in archive {
        let entry = result.map_err(|e| rar_err("Error reading RAR entry", e))?;
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(CarrelError::invalid(format!(
                "Archive has more than {} entries (maximum {})",
                MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_ENTRIES
            )));
        }
        if entry.unpacked_size > MAX_ENTRY_SIZE {
            return Err(CarrelError::invalid(format!(
                "Archive entry '{}' decompressed size ({} MB) exceeds limit ({} MB)",
                entry.filename.to_string_lossy(),
                entry.unpacked_size / (1024 * 1024),
                MAX_ENTRY_SIZE / (1024 * 1024)
            )));
        }
    }
    Ok(())
}

fn is_image(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".webp")
        || lower.ends_with(".gif")
}

fn mime_for(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

/// Collect sorted image entry names from a RAR archive.
fn collect_image_names(path: &str) -> CarrelResult<Vec<String>> {
    let archive = unrar::Archive::new(path)
        .open_for_listing()
        .map_err(|e| rar_err("Cannot open RAR archive", e))?;

    let mut names: Vec<String> = archive
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.filename.to_string_lossy().to_string();
            if entry.is_directory() || name.starts_with("__MACOSX/") || !is_image(&name) {
                return None;
            }
            Some(name)
        })
        .collect();

    names.sort();
    Ok(names)
}

pub struct CbrMeta {
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

/// Extract ComicInfo.xml content from a RAR archive, if present.
fn extract_comic_info(path: &str) -> Option<String> {
    let archive = unrar::Archive::new(path).open_for_processing().ok()?;
    let mut cursor = archive;
    loop {
        let header = cursor.read_header().ok()?;
        match header {
            None => return None,
            Some(entry) => {
                let name = entry.entry().filename.to_string_lossy().to_string();
                if name == "ComicInfo.xml" || name.ends_with("/ComicInfo.xml") {
                    let (data, _) = entry.read().ok()?;
                    return String::from_utf8(data).ok();
                } else {
                    cursor = entry.skip().ok()?;
                }
            }
        }
    }
}

pub fn import_cbr(path: &str) -> CarrelResult<CbrMeta> {
    validate_archive(path)?;
    let images = collect_image_names(path)?;
    if images.is_empty() {
        return Err(CarrelError::invalid(
            "CBR archive contains no supported image files",
        ));
    }
    let title = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let mut author = None;
    let mut year = None;
    let mut comic_title = None;
    let mut series = None;
    let mut volume = None;
    let mut language = None;
    let mut publisher = None;
    let mut summary = None;
    let mut genre = None;

    if let Some(xml) = extract_comic_info(path) {
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
        volume = crate::epub::extract_tag_text(&xml, "Volume").and_then(|v| v.parse::<u32>().ok());
        language = crate::epub::extract_tag_text(&xml, "LanguageISO").map(|s| s.to_string());
        publisher = crate::epub::extract_tag_text_decoded(&xml, "Publisher");
        summary = crate::epub::extract_tag_text_decoded(&xml, "Summary");
        genre = crate::epub::extract_tag_text_decoded(&xml, "Genre");
    }

    Ok(CbrMeta {
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

pub fn get_page_count(path: &str) -> CarrelResult<u32> {
    let images = collect_image_names(path)?;
    Ok(images.len() as u32)
}

/// Canonical sorted page-entry names for a CBR, using the exact same
/// filter/sort as [`get_page_image_bytes`]. The page cache relies on this
/// so a page cached at index `i` is byte-identical to an on-demand read of
/// index `i` (see `page_cache`).
pub(crate) fn collect_page_names(path: &str) -> CarrelResult<Vec<String>> {
    collect_image_names(path)
}

pub fn get_page_image(path: &str, page_index: u32) -> CarrelResult<String> {
    let images = collect_image_names(path)?;
    let target_name = images
        .get(page_index as usize)
        .ok_or_else(|| {
            CarrelError::not_found(format!(
                "Page index {page_index} out of range (total pages: {})",
                images.len()
            ))
        })?
        .clone();

    // Open for processing — walk entries until we find the target, then .read() it
    let archive = unrar::Archive::new(path)
        .open_for_processing()
        .map_err(|e| rar_err("Cannot open RAR archive", e))?;

    let mut cursor = archive;
    loop {
        let header = cursor
            .read_header()
            .map_err(|e| rar_err("Error reading RAR entry", e))?;
        match header {
            None => {
                return Err(CarrelError::not_found(format!(
                    "Page '{}' not found in archive",
                    target_name
                )))
            }
            Some(entry) => {
                let name = entry.entry().filename.to_string_lossy().to_string();
                if name == target_name {
                    let (data, _) = entry
                        .read()
                        .map_err(|e| rar_err("Cannot extract page", e))?;
                    let mime = mime_for(&target_name);
                    let encoded = general_purpose::STANDARD.encode(&data);
                    return Ok(format!("data:{mime};base64,{encoded}"));
                } else {
                    cursor = entry
                        .skip()
                        .map_err(|e| rar_err("Error skipping RAR entry", e))?;
                }
            }
        }
    }
}

/// Extracts a single page image and returns raw bytes + mime type.
///
/// When `target_width` is `Some(w)` and the source image is wider than
/// `w`, the page is downscaled and re-encoded as JPEG; otherwise the
/// original archive bytes are returned untouched.
pub fn get_page_image_bytes(
    path: &str,
    page_index: u32,
    target_width: Option<u32>,
) -> CarrelResult<(Vec<u8>, String)> {
    let images = collect_image_names(path)?;
    let target_name = images
        .get(page_index as usize)
        .ok_or_else(|| {
            CarrelError::not_found(format!(
                "Page index {page_index} out of range (total pages: {})",
                images.len()
            ))
        })?
        .clone();

    let archive = unrar::Archive::new(path)
        .open_for_processing()
        .map_err(|e| rar_err("Cannot open RAR archive", e))?;

    let mut cursor = archive;
    loop {
        let header = cursor
            .read_header()
            .map_err(|e| rar_err("Error reading RAR entry", e))?;
        match header {
            None => {
                return Err(CarrelError::not_found(format!(
                    "Page '{}' not found in archive",
                    target_name
                )))
            }
            Some(entry) => {
                let name = entry.entry().filename.to_string_lossy().to_string();
                if name == target_name {
                    let (data, _) = entry
                        .read()
                        .map_err(|e| rar_err("Cannot extract page", e))?;
                    let mime = mime_for(&target_name).to_string();
                    return crate::image_util::maybe_resize_to_jpeg(data, mime, target_width);
                } else {
                    cursor = entry
                        .skip()
                        .map_err(|e| rar_err("Error skipping RAR entry", e))?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_image_accepts_common_formats() {
        assert!(is_image("page.jpg"));
        assert!(is_image("page.jpeg"));
        assert!(is_image("page.png"));
        assert!(is_image("page.webp"));
        assert!(is_image("page.gif"));
    }

    #[test]
    fn is_image_case_insensitive() {
        assert!(is_image("COVER.JPG"));
        assert!(is_image("Cover.PNG"));
    }

    #[test]
    fn is_image_rejects_non_images() {
        assert!(!is_image("readme.txt"));
        assert!(!is_image("data.xml"));
        assert!(!is_image(""));
    }

    #[test]
    fn mime_for_png() {
        assert_eq!(mime_for("page.png"), "image/png");
        assert_eq!(mime_for("page.PNG"), "image/png");
    }

    #[test]
    fn mime_for_webp() {
        assert_eq!(mime_for("page.webp"), "image/webp");
    }

    #[test]
    fn mime_for_gif() {
        assert_eq!(mime_for("page.gif"), "image/gif");
    }

    #[test]
    fn mime_for_jpeg_default() {
        assert_eq!(mime_for("page.jpg"), "image/jpeg");
        assert_eq!(mime_for("page.jpeg"), "image/jpeg");
        // Unknown extension defaults to jpeg
        assert_eq!(mime_for("page.bmp"), "image/jpeg");
    }
}

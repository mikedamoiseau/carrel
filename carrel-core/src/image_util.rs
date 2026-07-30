//! Image resize utilities shared across page-image readers.
//!
//! Comic-format pages (CBZ / CBR) ship the original image bytes from the
//! archive — a 4000×6000 px scan can easily weigh 5–10 MB even after
//! archive-level deflate. Sending that across IPC and decoding it in a
//! browser viewport that is at most 1600 px wide wastes memory, CPU, and
//! transfer time at every page turn.
//!
//! [`maybe_resize_to_jpeg`] is a single entry point that callers use to
//! optionally clamp page width to a viewport target. When the caller
//! passes `None`, or the source is already at or below the target,
//! bytes pass through untouched (keeping the original mime). When the
//! source is wider than the target, the image is decoded, downscaled
//! with Lanczos3, and re-encoded as JPEG quality 90 — the same encoder
//! settings the PDF render path already uses.
//!
//! Animation-capable formats (GIF, WebP) bypass resize entirely so
//! multi-frame pages keep their frames. Images with alpha are
//! composited over a white background before JPEG encode because
//! JPEG has no alpha channel — a naive RGB drop would render
//! transparent regions as black.

use crate::error::{CarrelError, CarrelResult};
use std::io::Cursor;

/// Re-encode quality used when a downscale happens. Matches PDF render path.
const JPEG_QUALITY: u8 = 90;

/// Re-encode quality for grid thumbnails. Lower than [`JPEG_QUALITY`] —
/// thumbnails render in a ~160 px card, so q80 is visually indistinguishable
/// while shaving file size.
const THUMB_QUALITY: u8 = 80;

/// Produce a small JPEG thumbnail of `bytes` clamped to `target_width`.
///
/// Returns `Ok(None)` — meaning "no thumbnail needed, use the original" —
/// when the source is already at or below `target_width`, or when the
/// format is animation-capable (GIF/WebP, which may carry frames). The
/// `None` path costs only a header probe, never a full decode, so callers
/// can cheaply re-check every cover on each startup without paying to
/// decode the many already-small covers.
///
/// Returns `Ok(Some(jpeg_bytes))` when a real downscale happened: decode,
/// Lanczos3 to `(target_width, scaled_height)`, composite alpha over white,
/// encode JPEG quality 80.
pub fn make_thumbnail(bytes: &[u8], target_width: u32) -> CarrelResult<Option<Vec<u8>>> {
    if target_width == 0 {
        return Ok(None);
    }

    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| CarrelError::invalid(format!("cannot probe image format: {e}")))?;

    // Animation-capable formats may carry multiple frames; a single-frame
    // decode would silently drop the rest. Covers are virtually never
    // animated, so keeping the original is the safe trade-off.
    if matches!(
        reader.format(),
        Some(image::ImageFormat::Gif) | Some(image::ImageFormat::WebP)
    ) {
        return Ok(None);
    }

    let (src_w, src_h) = reader
        .into_dimensions()
        .map_err(|e| CarrelError::invalid(format!("cannot read image dimensions: {e}")))?;
    if src_w <= target_width {
        return Ok(None);
    }

    let img = image::load_from_memory(bytes)
        .map_err(|e| CarrelError::invalid(format!("image decode failed: {e}")))?;

    let target_h = (((src_h as u64) * (target_width as u64)) / (src_w as u64)).max(1) as u32;
    let resized = img.resize_exact(
        target_width,
        target_h,
        image::imageops::FilterType::Lanczos3,
    );

    let rgb = if resized.color().has_alpha() {
        composite_over_white(&resized)
    } else {
        resized.to_rgb8()
    };

    let mut out: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY);
    encoder
        .encode_image(&rgb)
        .map_err(|e| CarrelError::internal(format!("JPEG thumbnail encode failed: {e}")))?;

    Ok(Some(out))
}

/// Clamp page image width to `target_width` when both are known and the
/// source is wider. Returns the (possibly transformed) bytes + mime.
///
/// Pass-through cases (returns input unchanged):
/// - `target_width` is `None` or `Some(0)`
/// - the source image width is already ≤ `target_width`
/// - the source format is GIF or WebP (may carry animation frames)
/// - the bytes fail to probe or decode (a corrupt-but-renderable page must
///   not become an error just because a resize was requested)
///
/// Resize case:
/// - decode, downscale to `(target_width, scaled_height)` via Lanczos3
/// - if the decoded image has alpha, composite over white
/// - re-encode as JPEG quality 90; output mime is `image/jpeg`
pub fn maybe_resize_to_jpeg(
    bytes: Vec<u8>,
    current_mime: String,
    target_width: Option<u32>,
) -> CarrelResult<(Vec<u8>, String)> {
    let Some(target) = target_width else {
        return Ok((bytes, current_mime));
    };
    if target == 0 {
        return Ok((bytes, current_mime));
    }

    // Cheap dimension probe — avoids a full decode when we're already
    // below the target width.
    //
    // Probe/decode failures below fall back to the original bytes rather than
    // erroring: a corrupt-but-browser-renderable page (truncated JPEG — common
    // in scanned comics) serves fine at full size, and requesting a resize
    // must never turn that working page into an error response.
    let reader = match image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(_) => return Ok((bytes, current_mime)),
    };

    // GIF/WebP may carry multiple frames; `load_from_memory` decodes
    // only the first one, so resizing would silently drop animation.
    // Static comics in these formats are rare enough that paying the
    // full transfer cost is the right trade-off vs corrupting content.
    if matches!(
        reader.format(),
        Some(image::ImageFormat::Gif) | Some(image::ImageFormat::WebP)
    ) {
        return Ok((bytes, current_mime));
    }

    let (src_w, src_h) = match reader.into_dimensions() {
        Ok(dims) => dims,
        Err(_) => return Ok((bytes, current_mime)),
    };
    if src_w <= target {
        return Ok((bytes, current_mime));
    }

    let img = match image::load_from_memory(&bytes) {
        Ok(img) => img,
        Err(_) => return Ok((bytes, current_mime)),
    };

    let target_h = (((src_h as u64) * (target as u64)) / (src_w as u64)).max(1) as u32;
    let resized = img.resize_exact(target, target_h, image::imageops::FilterType::Lanczos3);

    // JPEG has no alpha; transparent pixels must be composited over a
    // known background or they decode as black in the encoder output.
    // White matches typical comic-page expectations.
    let rgb = if resized.color().has_alpha() {
        composite_over_white(&resized)
    } else {
        resized.to_rgb8()
    };

    let mut out: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
    encoder
        .encode_image(&rgb)
        .map_err(|e| CarrelError::internal(format!("JPEG re-encode failed: {e}")))?;

    Ok((out, "image/jpeg".to_string()))
}

fn composite_over_white(img: &image::DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let [r, g, b, a] = pixel.0;
        let af = a as u32;
        let inv = 255 - af;
        let cr = ((r as u32 * af + 255 * inv) / 255) as u8;
        let cg = ((g as u32 * af + 255 * inv) / 255) as u8;
        let cb = ((b as u32 * af + 255 * inv) / 255) as u8;
        out.put_pixel(x, y, image::Rgb([cr, cg, cb]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    #[test]
    fn undecodable_bytes_pass_through_unchanged_when_width_requested() {
        // A corrupt-but-browser-renderable page (e.g. truncated JPEG) must not
        // start failing just because a resize was requested — serving the
        // original bytes beats a 4xx for a page that displays fine full-size.
        let truncated: Vec<u8> = encode_jpeg(600, 400)[..64].to_vec();
        let (bytes, mime) =
            maybe_resize_to_jpeg(truncated.clone(), "image/jpeg".to_string(), Some(100)).unwrap();
        assert_eq!(bytes, truncated);
        assert_eq!(mime, "image/jpeg");

        let garbage = vec![0u8; 32];
        let (bytes, mime) =
            maybe_resize_to_jpeg(garbage.clone(), "image/jpeg".to_string(), Some(100)).unwrap();
        assert_eq!(bytes, garbage);
        assert_eq!(mime, "image/jpeg");
    }

    fn encode_jpeg(w: u32, h: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgb<u8>, _> =
            ImageBuffer::from_fn(w, h, |x, y| Rgb([((x + y) % 256) as u8, 0, 0]));
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        encoder.encode_image(&buf).unwrap();
        out
    }

    fn encode_png(w: u32, h: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgb<u8>, _> =
            ImageBuffer::from_fn(w, h, |x, y| Rgb([0, ((x ^ y) % 256) as u8, 0]));
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        use image::ImageEncoder;
        encoder
            .write_image(buf.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    /// Fully transparent left half, opaque red right half.
    fn encode_png_with_transparency(w: u32, h: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_fn(w, h, |x, _y| {
            if x < w / 2 {
                Rgba([0, 0, 0, 0])
            } else {
                Rgba([200, 30, 30, 255])
            }
        });
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        use image::ImageEncoder;
        encoder
            .write_image(buf.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn encode_gif(w: u32, h: u32) -> Vec<u8> {
        let buf: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_fn(w, h, |x, y| Rgba([((x + y) % 256) as u8, 10, 20, 255]));
        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            let frame = image::Frame::new(buf);
            encoder.encode_frame(frame).unwrap();
        }
        out
    }

    fn dims_of(bytes: &[u8]) -> (u32, u32) {
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    }

    #[test]
    fn passthrough_when_target_is_none() {
        let src = encode_jpeg(1000, 1500);
        let (out, mime) = maybe_resize_to_jpeg(src.clone(), "image/jpeg".into(), None).unwrap();
        assert_eq!(out, src);
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn passthrough_when_target_is_zero() {
        let src = encode_jpeg(1000, 1500);
        let (out, mime) = maybe_resize_to_jpeg(src.clone(), "image/jpeg".into(), Some(0)).unwrap();
        assert_eq!(out, src);
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn passthrough_when_source_already_below_target() {
        let src = encode_jpeg(800, 1200);
        let (out, mime) =
            maybe_resize_to_jpeg(src.clone(), "image/jpeg".into(), Some(1000)).unwrap();
        assert_eq!(out, src, "bytes must be unchanged when source ≤ target");
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn passthrough_when_source_equals_target_exactly() {
        let src = encode_jpeg(1000, 1500);
        let (out, _mime) =
            maybe_resize_to_jpeg(src.clone(), "image/jpeg".into(), Some(1000)).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn downscales_jpeg_and_preserves_aspect() {
        let src = encode_jpeg(2400, 3600);
        let (out, mime) = maybe_resize_to_jpeg(src, "image/jpeg".into(), Some(800)).unwrap();
        assert_eq!(mime, "image/jpeg");
        let (w, h) = dims_of(&out);
        assert_eq!(w, 800);
        // 2400×3600 → aspect 1.5; 800 wide → 1200 tall.
        assert_eq!(h, 1200);
    }

    #[test]
    fn downscaling_png_transcodes_to_jpeg() {
        let src = encode_png(2000, 1000);
        let (out, mime) = maybe_resize_to_jpeg(src, "image/png".into(), Some(500)).unwrap();
        assert_eq!(mime, "image/jpeg", "downscale always re-encodes as JPEG");
        let (w, _h) = dims_of(&out);
        assert_eq!(w, 500);
    }

    #[test]
    fn smaller_output_when_significantly_downscaled() {
        let src = encode_jpeg(3000, 4500);
        let src_len = src.len();
        let (out, _mime) = maybe_resize_to_jpeg(src, "image/jpeg".into(), Some(600)).unwrap();
        assert!(
            out.len() < src_len,
            "downscaled output should be smaller; src={src_len}B out={}B",
            out.len()
        );
    }

    #[test]
    fn invalid_bytes_pass_through_unchanged() {
        // Contract change (offline-mode review): undecodable bytes fall back
        // to the original payload instead of erroring — a corrupt-but-
        // renderable page must keep serving when a resize is requested. See
        // undecodable_bytes_pass_through_unchanged_when_width_requested.
        let (bytes, mime) =
            maybe_resize_to_jpeg(b"not an image".to_vec(), "image/jpeg".into(), Some(500)).unwrap();
        assert_eq!(bytes, b"not an image".to_vec());
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn transparent_png_resized_composites_over_white() {
        // 1000 wide → resize to 400 → transparent half must render white,
        // not black, in the resulting JPEG.
        let src = encode_png_with_transparency(1000, 400);
        let (out, mime) = maybe_resize_to_jpeg(src, "image/png".into(), Some(400)).unwrap();
        assert_eq!(mime, "image/jpeg");

        let decoded = image::load_from_memory(&out).unwrap().to_rgb8();
        let (w, h) = decoded.dimensions();
        assert_eq!(w, 400);

        // Sample well inside the (originally transparent) left quarter.
        let px = decoded.get_pixel(w / 8, h / 2).0;
        assert!(
            px[0] > 220 && px[1] > 220 && px[2] > 220,
            "transparent region should composite near white, got {px:?}"
        );
    }

    #[test]
    fn gif_passthrough_even_when_wider_than_target() {
        // Animation-capable formats must not be transcoded — resizing
        // would drop all but the first frame.
        let src = encode_gif(1200, 800);
        let (out, mime) = maybe_resize_to_jpeg(src.clone(), "image/gif".into(), Some(500)).unwrap();
        assert_eq!(out, src, "GIF bytes must be unchanged");
        assert_eq!(mime, "image/gif");
    }

    #[test]
    fn thumbnail_none_when_source_at_or_below_target() {
        // Already-small cover → no thumbnail, caller uses the original.
        assert!(make_thumbnail(&encode_jpeg(320, 480), 320)
            .unwrap()
            .is_none());
        assert!(make_thumbnail(&encode_jpeg(200, 300), 320)
            .unwrap()
            .is_none());
    }

    #[test]
    fn thumbnail_none_when_target_is_zero() {
        assert!(make_thumbnail(&encode_jpeg(1000, 1500), 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn thumbnail_downscales_to_target_width_jpeg() {
        let out = make_thumbnail(&encode_jpeg(1920, 2880), 320)
            .unwrap()
            .expect("wide cover must produce a thumbnail");
        let (w, h) = dims_of(&out);
        assert_eq!(w, 320);
        // 1920×2880 → aspect 1.5 → 320 wide → 480 tall.
        assert_eq!(h, 480);
    }

    #[test]
    fn thumbnail_from_png_transcodes_and_shrinks() {
        let src = encode_png(2000, 3000);
        let out = make_thumbnail(&src, 320)
            .unwrap()
            .expect("should thumbnail");
        let (w, _h) = dims_of(&out);
        assert_eq!(w, 320);
        assert!(
            out.len() < src.len(),
            "thumbnail must be smaller than source"
        );
        // Output is JPEG regardless of PNG input.
        assert_eq!(
            image::ImageReader::new(Cursor::new(&out))
                .with_guessed_format()
                .unwrap()
                .format(),
            Some(image::ImageFormat::Jpeg)
        );
    }

    #[test]
    fn thumbnail_transparent_png_composites_over_white() {
        let out = make_thumbnail(&encode_png_with_transparency(1000, 400), 320)
            .unwrap()
            .expect("should thumbnail");
        let decoded = image::load_from_memory(&out).unwrap().to_rgb8();
        let (w, h) = decoded.dimensions();
        let px = decoded.get_pixel(w / 8, h / 2).0;
        assert!(
            px[0] > 220 && px[1] > 220 && px[2] > 220,
            "transparent region should composite near white, got {px:?}"
        );
    }

    #[test]
    fn thumbnail_gif_returns_none() {
        assert!(make_thumbnail(&encode_gif(1200, 800), 320)
            .unwrap()
            .is_none());
    }

    #[test]
    fn thumbnail_invalid_bytes_error() {
        assert!(matches!(
            make_thumbnail(b"not an image", 320),
            Err(CarrelError::InvalidInput { .. })
        ));
    }
}

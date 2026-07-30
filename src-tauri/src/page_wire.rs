//! Wire format for the binary page-image Tauri commands.
//!
//! Tauri v2 commands can return raw bytes via `tauri::ipc::Response`,
//! but the response is a plain byte stream — there is no header
//! channel to carry the image mime type. The frontend needs the mime
//! to construct a `Blob` with the correct `type` so the browser
//! decoder picks the right pipeline and so `Content-Type` is right if
//! the blob URL is ever fetched again.
//!
//! We append a single mime-tag byte at the **end** of the byte stream:
//!
//! ```text
//! [ image bytes …………………………………………… ] [ tag: u8 ]
//! ```
//!
//! Appending (vs prefixing) lets us reuse the page bytes returned by
//! `carrel_core` parsers in place — one `Vec::push` — instead of
//! allocating a new buffer and copying the payload. The frontend
//! mirror lives at `src/lib/pageWire.ts` and must stay in sync with
//! the constants below.
//!
//! Unknown mimes degrade to JPEG, matching the resize fallback in
//! `carrel_core::image_util` and the PDF render path.
#![cfg_attr(not(test), allow(dead_code))]

pub const MIME_TAG_JPEG: u8 = 0;
pub const MIME_TAG_PNG: u8 = 1;
pub const MIME_TAG_WEBP: u8 = 2;
pub const MIME_TAG_GIF: u8 = 3;

/// Map a content-type string to its single-byte wire tag.
pub fn mime_tag(mime: &str) -> u8 {
    match mime {
        "image/png" => MIME_TAG_PNG,
        "image/webp" => MIME_TAG_WEBP,
        "image/gif" => MIME_TAG_GIF,
        _ => MIME_TAG_JPEG,
    }
}

/// Append the mime tag to the image byte stream.
pub fn append_tag(mut bytes: Vec<u8>, mime: &str) -> Vec<u8> {
    bytes.push(mime_tag(mime));
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_tag_maps_known_formats() {
        assert_eq!(mime_tag("image/jpeg"), MIME_TAG_JPEG);
        assert_eq!(mime_tag("image/png"), MIME_TAG_PNG);
        assert_eq!(mime_tag("image/webp"), MIME_TAG_WEBP);
        assert_eq!(mime_tag("image/gif"), MIME_TAG_GIF);
    }

    #[test]
    fn mime_tag_falls_back_to_jpeg() {
        assert_eq!(mime_tag(""), MIME_TAG_JPEG);
        assert_eq!(mime_tag("image/avif"), MIME_TAG_JPEG);
        assert_eq!(mime_tag("application/pdf"), MIME_TAG_JPEG);
    }

    #[test]
    fn append_tag_adds_one_trailing_byte() {
        let body = vec![1u8, 2, 3];
        let out = append_tag(body.clone(), "image/png");
        assert_eq!(out.len(), body.len() + 1);
        assert_eq!(&out[..body.len()], body.as_slice());
        assert_eq!(*out.last().unwrap(), MIME_TAG_PNG);
    }

    #[test]
    fn append_tag_jpeg_on_empty_payload() {
        let out = append_tag(Vec::new(), "image/jpeg");
        assert_eq!(out, vec![MIME_TAG_JPEG]);
    }
}

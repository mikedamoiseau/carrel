//! Test-only EPUB fixtures, shared by the IPC-adapter and web-adapter tests.
//!
//! Both adapters' chapter-read tests need the same thing — a small but valid
//! EPUB with a real spine entry and an inline image — and neither is the
//! natural owner of it, so it lives here rather than being copied into each
//! test module.

/// A one-chapter EPUB with a relative `<img src>`, so a chapter read
/// exercises the spine lookup and the inline-image rewrite. Returns the path.
pub(crate) fn write_epub_with_image_chapter(
    dir: &std::path::Path,
    name: &str,
) -> std::path::PathBuf {
    let path = dir.join(format!("{name}.epub"));
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    zip.start_file("mimetype", options).unwrap();
    std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();

    zip.start_file("META-INF/container.xml", options).unwrap();
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

    zip.start_file("content.opf", options).unwrap();
    std::io::Write::write_all(
        &mut zip,
        br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Chapter Read Test Book</dc:title>
    <dc:creator>Test Author</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch0" href="ch0.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="img.png" media-type="image/png"/>
  </manifest>
  <spine>
    <itemref idref="ch0"/>
  </spine>
</package>"#,
    )
    .unwrap();

    zip.start_file("ch0.xhtml", options).unwrap();
    std::io::Write::write_all(
        &mut zip,
        br#"<html><body><p>Hello</p><img src="img.png" alt="a"/></body></html>"#,
    )
    .unwrap();

    // Smallest thing the rewrite will accept as an image payload; the bytes
    // are never decoded on this path, only extracted to storage.
    zip.start_file("img.png", options).unwrap();
    std::io::Write::write_all(&mut zip, b"\x89PNG\r\n\x1a\n").unwrap();

    zip.finish().unwrap();
    path
}

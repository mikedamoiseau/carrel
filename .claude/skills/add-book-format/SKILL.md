---
name: add-book-format
description: Use when adding support for a new e-book or comic format to Carrel (e.g. FB2, DJVU, a new archive type) — creating a format parser module, adding a BookFormat variant, or wiring import/render dispatch for a file type the app can't yet open.
---

# Add a Book Format

## Overview

Formats are an enum-driven pipeline. The compiler is your checklist: add a
`BookFormat` variant and every `match` on it becomes a non-exhaustive error
until you handle the new arm. Follow the errors.

Existing parsers to copy from: `carrel-core/src/{epub,pdf,cbz,cbr}.rs` and the
`carrel-core/src/mobi/` module (the feature-gated example).

## Steps

### 1. Add the enum variant — `carrel-core/src/models.rs`

```rust
pub enum BookFormat {
    Epub, Cbz, Cbr, Pdf, Mobi,
    Fb2,   // <-- new
}
```

Also extend the `Display` impl in the same file (`BookFormat::Fb2 => write!(f, "fb2")`).
This alone produces compiler errors at every other match site — that list IS
your remaining work.

### 2. Create the parser module — `carrel-core/src/fb2.rs`

Mirror an existing parser's public surface. A parser extracts: metadata
(title/author/etc.), content (sanitized HTML chapters for text formats, or
sorted image bytes for comic formats), and a cover image. Register the module
in `carrel-core/src/lib.rs` (`pub mod fb2;`).

- Text/HTML formats MUST sanitize server-side with `ammonia` (see `epub.rs`).
- Heavy/optional native deps: gate behind a Cargo feature like `mobi` does, and
  keep the enum variant always present so existing library rows stay readable.

### 3. Detect on import — `src-tauri/src/commands.rs`

In `import_book`, the `match extension.as_str()` block maps extensions to
`BookFormat`. Add your arm:

```rust
"fb2" => BookFormat::Fb2,
```

Then handle the new `BookFormat::Fb2` arm in the import processing match
(extract cover/metadata, copy into the library). Comic-ish archives that may be
mislabeled use a magic-byte fallback — see the `cbz | cbr` arm.

### 4. Handle every other match arm

Render/content commands also match on `BookFormat` (search `BookFormat::` in
`commands.rs`). Clippy/`cargo build` will flag each unhandled arm. Resolve all.

### 5. Advertise the extension

Add the extension to `supported_import_extensions()` so the import dialog and
`get_supported_formats` offer it.

## Verify

```bash
cargo test -p carrel-core                                # core parser tests
cargo clippy --workspace --all-targets -- -D warnings
# feature-gated parser? also:
cargo test -p carrel-core --features <feature>
```

Add a fixture-backed test (real sample file) following the parser tests in
`carrel-core/src/`. Gate it to skip cleanly when the fixture is absent (see the
MOBI corpus pattern in CLAUDE.md) so fresh clones stay green.

## Common Mistakes

| Mistake | Symptom |
|---------|---------|
| Skipped a `BookFormat::` match arm | Build error (good) — handle it, don't `_ =>` it away |
| Used catch-all `_ =>` for new variant | Silently wrong behavior; handle the variant explicitly |
| No HTML sanitization on a text format | XSS risk — `ammonia` server-side is mandatory |
| Forgot `supported_import_extensions()` | File type not offered in the import dialog |
| Forgot `pub mod fb2;` in carrel-core lib.rs | "unresolved module" build error |

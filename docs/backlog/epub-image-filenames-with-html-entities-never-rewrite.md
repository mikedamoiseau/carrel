# EPUB images whose filename contains `&`, `"` or U+00A0 are never rewritten

**Status:** open. Pre-existing — not introduced by the M5 URL-policy change,
and verified byte-identical on both sides of that commit.

`rewrite_img_srcs` (`carrel-core/src/epub.rs`) reads the `src` value out of the
**ammonia-serialized** chapter with `extract_attr_value`, which returns the
attribute text *entity-encoded*. It then looks that string up as a zip entry.

So an EPUB whose image entry is `a b&c.png` is referenced in the cleaned markup
as `src="a b&amp;c.png"`, and `resolve_zip_path` goes looking for a literal
`a b&amp;c.png` — which is not in the archive. The lookup fails, the `<img>` tag
is left exactly as it was (the deliberate "one broken asset must not abort the
chapter" fallback), and what reaches the reader is a *relative* path that
neither Tauri's asset protocol nor the browser can resolve. Permanently broken
image, no error anywhere.

Affects any filename containing a character ammonia escapes in attribute
values: `&`, `"`, U+00A0.

Both surfaces are affected identically, because the fault is below the URL
policy — the policy is never called for these images at all.

## Fix

HTML-entity-decode the extracted attribute value before `resolve_zip_path`.

Two things to check when doing it:

- It widens what reaches the caller's URL policy, so re-read
  `reader::attr_safe_url` — a filename containing `"` becomes reachable at the
  splice for the first time, which is the case that guard exists for.
- The keys are hashed with `short_zip_path_hash(resolved_zip_path)`, so books
  already imported keep working; nothing on disk is invalidated by decoding
  earlier.

## Not to be confused with

A filename containing a literal `%`: that one *is* rewritten, and then
`get_epub_image` 404s on it because axum percent-decodes the path segment back
(`%2F` → `/`). Also pre-existing, also unchanged by M5, but a different bug in
a different layer.

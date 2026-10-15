# Desktop PDF private mode still records the read

**Status:** open (noted 2026-10-15, during M5 of the backlog-sweep epic).

## What

M5 made a private comic read write nothing: no page, no manifest, no
`last_accessed` bump, no staged source copy, on desktop and web alike. The
user's decision was "match PDF", but desktop PDF does not actually meet that
bar, so comics are now stricter than desktop PDF:

- `prepare_pdf` (`src-tauri/src/commands.rs`) calls
  `page_cache::ensure_pdf_prewarmed` regardless of private mode. On a book with
  no manifest that *creates* one (`book_id`, `last_accessed`); on a warm book it
  rewrites `last_accessed`. Both are a durable "book X was opened at time T".
- The PDF page-read path bumps `last_accessed` on a private miss against an
  existing manifest (the page bytes are not written, but the timestamp is).
- Private eviction still runs in `prepare_pdf` (harmless on its own, but it is
  only there because a manifest may have been written).

The prewarm is desktop-only (the web PDF arm does not prewarm). The
`last_accessed` bump on a private PDF miss is not: it lives in
`page_cache::get_or_render_pdf_page` (`suppress_write`), shared by both
surfaces, and is deliberate there — it keeps a privately read book from
looking cold to eviction. Comics take the opposite choice (a private read
never touches the manifest), so decide which rule should win rather than
assume PDF is the reference. Source
staging is no longer part of it: M5 gated staging on private mode for both
formats on both surfaces.

## Also: a comic background pass ignores a mid-pass switch

`prepare_comic` spawns `extract_comic_remaining` for a non-private open. If
private mode is switched on while that pass runs, it keeps extracting and
writing the manifest. The PDF background pass checks the live
`private_mode` flag (`bg_private`) and stops. Low impact — the open itself was
not private, so the book is already recorded — but the two formats disagree.
`extract_comic_remaining` is public carrel-core API (Carrel Server pins it), so
an abort hook would have to be additive.

## Worth knowing before picking it up

- Decide whether desktop private PDF should mirror comics exactly (no manifest
  at all, page count from `pdf::get_page_count`) or keep a manifest for the
  page count. The comic path's `private_comic_manifest` is the template.
- Tests for `prepare_pdf` / `prepare_comic` need an `AppHandle`; check whether
  `mock_app_with_state` (`commands.rs`) can drive them without resolving the
  real user cache dir via `app_cache_dir()`.

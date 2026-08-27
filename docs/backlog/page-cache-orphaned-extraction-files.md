# Reclaim page files left behind by a failed comic extraction

**Status:** open (noted 2026-08-26, during the M1 comic-page-reads review).

## What

`page_cache::extract_comic_full` (used by `ensure_cached`, and by extension
the web reader's `carrel_core::reader::page_image`) writes each decoded page
to `storage` one at a time via `extract_comic_subset`, then writes the
manifest only after every page has succeeded. If decoding fails partway
through — a truncated archive, a corrupt entry past the first few pages, an
`I/O` error mid-read — the function returns `Err` and no manifest is ever
written for that `book_hash`. The pages that *did* extract successfully
before the failure are already on disk under that hash's directory, with
nothing referencing them: no manifest lists them, so `run_eviction` (which
walks manifests to decide what to reclaim) never sees them, and no other code
path deletes them either. They sit on disk permanently, invisible to the
cache's own size accounting.

This is pre-existing behavior in `page_cache.rs`, not something introduced by
the M1 web route. It was previously reachable only through the desktop's
`prepare_comic`; the web reader's `page_image` (this milestone) reaches the
same `ensure_cached` call and so exercises the same gap, on a wider set of
inputs (anything reachable over the LAN can trigger an extraction).

## Why it wasn't fixed here

Cleaning this up means either deleting the partial output on the `Err` path
inside `extract_comic_subset`/`extract_comic_full`, or having `run_eviction`
learn to find and reclaim directory content with no matching manifest —
both are changes to `page_cache`'s core extraction/eviction protocol, which
this milestone's brief explicitly ruled out reworking (see the "known
limitation" note in `carrel-core/src/reader.rs`'s module doc for the related
F2 finding, decided the same way). It is also a pre-existing bug rather than
a regression from this milestone's diff, so fixing it here would mix an
unrelated fix into a milestone that is supposed to be reviewable as one
change.

## Worth knowing before picking it up

- The straightforward fix is cleanup-on-error inside `extract_comic_subset`:
  track which page keys were `put` successfully and `delete` them if the
  function returns early with an error, so a failed extraction leaves no
  trace rather than a manifest-less orphan.
- An orphan only accumulates on a genuine mid-extraction failure (corrupt
  archive, disk full, I/O error) — not on the ordinary paths, which either
  succeed completely or fail before writing anything. Low frequency, but
  each occurrence is permanent until someone notices the cache directory is
  larger than `run_eviction`'s accounting believes it is.
- Any reclaim-by-scanning-the-directory approach needs to distinguish a
  genuinely orphaned page from one that belongs to an in-flight extraction
  that simply hasn't written its manifest yet — the same completeness
  question `page_cache::complete_manifest` already answers for a different
  purpose, and the same window `EXTRACTION_LOCKS` (in `reader.rs`) exists to
  narrow on the web side.

# Comic page-cache writes don't consult private mode

**Status:** open (noted 2026-08-26, during the M1 comic-page-reads review).

## What

Private mode (B-M1, `AppState::private_mode` / `WebState`'s mirror of it) is
"don't track this session" — the convention documented on the field in
`commands.rs` is that a passive write/emit site reads it once per request via
`is_private` and passes an explicit `bool` into the pure `carrel-core`
functions it calls, so the read/write happens deterministically rather than
being decided deep inside shared code. Comic page caching does not follow
that convention on either surface: desktop's `prepare_comic` (`commands.rs`)
calls `page_cache::ensure_comic_fast` with no `is_private` check at all, and
the web reader's `carrel_core::reader::page_image` (this milestone) reaches
the same `page_cache::ensure_cached` the same way. Turning private mode on
does not stop either path from extracting a comic's pages to disk and writing
a manifest that records the book was opened.

This is a pre-existing gap in `prepare_comic`, not something this milestone
introduced. The web route reaches it because `page_image` was written to
mirror `prepare_comic`'s own caching choice (whole-book `ensure_cached`
priming) rather than reinvent one — see `reader.rs`'s module doc. Fixing it
only on the web side would not close the gap; it would make the two surfaces
disagree about whether private mode applies to comic reading, which is worse
than the current consistent (if incomplete) behavior.

## Why it wasn't fixed here

The milestone's brief was the web adapter's page/page-count routes, not an
audit of every write path against B-M1. `is_private` needs to reach
`page_cache`'s call sites on *both* surfaces to close this coherently, and
`prepare_comic`'s gap predates this branch — fixing it here would mix an
unrelated desktop-side change into a diff that is supposed to be reviewable
as one thing, and risks silently changing desktop behavior no one asked this
milestone to touch.

## Worth knowing before picking it up

- Check how private mode is already handled for the *other* on-disk caches
  (chapter/image extraction, bookmarks, reading progress — wherever
  `is_private` is read today) before designing this; the comic page cache
  should follow the same shape, not invent its own.
- The natural fix threads `is_private: bool` through `ensure_cached` /
  `ensure_comic_fast` (or their callers) the same way `book_id`/`book_hash`
  already are, and skips the write (or writes to a scratch location that
  never joins the manifest) when true. Decide as part of that work whether
  "private" should still serve pages (just not cache them) or refuse to open
  comics at all in that mode — the current code doesn't distinguish, since
  it doesn't check at all.
- Whatever the fix, it must update `prepare_comic` and
  `carrel_core::reader::page_image` together, or reopen the exact
  cross-surface disagreement this note exists to avoid.

## Related: source staging ignores private mode too (noted 2026-08-26, M2 review round 3)

`web_server/api.rs`'s page-image route calls
`commands::ensure_web_source_staged` unconditionally, *before* the
format-specific arms that do consult `is_private`. On a network-mounted
library that copies the whole book to
`{cache_dir}/source-cache/{hash}.{ext}` and bumps its mtime — a durable
on-disk record that the book was read, left behind by a session that asked
not to be recorded, and one that survives independently of the page cache.

M2 wired `is_private` through the PDF page-cache write and made private
reads stop *creating* a page-cache entry, so the page cache no longer
records a private read of a book that was never opened before. The source
cache still does.

This belongs with the fix above rather than on its own: the same question —
"does private mode mean don't persist, or mean don't read at all?" — decides
both, and gating staging on `is_private` would make a private read of a
network-mounted book slow in a way a user may not expect or want. Note also
that the desktop has its own staging path, so the cross-surface rule at the
end of the section above applies here too.

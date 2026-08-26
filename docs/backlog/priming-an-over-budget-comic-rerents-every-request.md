# Priming a comic bigger than the cache budget re-extracts on every request

**Status:** open (noted 2026-08-26, during the M3 round-5 review).

## What

`carrel_core::reader::page_image`'s comic priming path (`OnMiss::Prime`, the
web adapter) extracts a whole archive into the page cache on a miss, then
fires the caller's eviction hook. When the book itself is larger than
`page_cache_max_size_mb`, that sweep evicts the book it just wrote, so the
next request for the same book finds a cold cache and extracts it again.
Unbounded, per request.

This is not new to M3 — the in-range path has behaved this way since M1,
and `a_book_larger_than_the_eviction_budget_still_serves_its_pages` pins the
*serving* half of it deliberately (M1 finding F1: the request must degrade to
a direct decode rather than 404). M3 round 4 extended the same property to
the erroring path, because a failed read-back leaves an equally real archive
on disk and the hook is the only thing bounding it.

So today there are two bad options and the code takes the less bad one:

- fire the hook: the cache stays inside its budget, and an over-budget book
  pays a full extraction per request;
- don't fire it: the extraction is paid once, and the cache grows past its
  budget with nothing reclaiming it until an unrelated request sweeps.

Disk filling silently is worse than repeated work, hence the current choice.

## What would actually fix it

Don't extract at all for a request that cannot be served from the result.
Two separable pieces:

- **Out-of-range indices.** `GET /books/{id}/pages/999` on a cold 800 MB
  comic currently extracts the whole archive before discovering the index is
  invalid. A page count read from the archive's directory (`cbz::get_page_count`
  reads the zip central directory; `cbr` lists entries) is orders of magnitude
  cheaper than extraction and would reject the request before any write. Note
  this would invert `out_of_range_page_on_a_cold_book_still_fires_on_extracted`
  — with no extraction there is correctly nothing to sweep — so that test
  encodes today's rule, not a permanent one.
- **Books over budget.** Decide up front, from the archive's declared size,
  whether priming can survive the configured cap, and take the direct-decode
  path from the start instead of extract-then-evict. `ensure_cached` would
  need to report or accept a size bound for this.

## Also worth knowing: a narrow TOCTOU on the "did we extract" predicate

`extracted` is read from `page_cache::complete_manifest` immediately before
`ensure_cached` runs. The per-hash extraction lock stops two callers
extracting the same book at once, but it does not stop *another* book's
inline `run_eviction` from evicting this one in the two statements between
the check and the call. When that happens the predicate says "already
complete" while `ensure_cached` really does extract, and the sweep for that
write is skipped until the next one.

The window is two statements wide and the bytes are reclaimed by any later
sweep, so it has not been worth a fix. The clean version is for
`ensure_cached` to report whether it did real work, rather than having the
caller infer it — that is a `page_cache` signature change and should happen
with the work above, not on its own.

## Related: a partially-written extraction is not swept promptly (M3 round 6)

`extract_comic_full` writes page blobs first and the manifest last, with no
cleanup on error, so a failure partway through leaves manifest-less bytes on
disk. `page_image` deliberately does **not** fire the eviction hook on that
path.

Round 5 tried to: the reasoning was that an archive had been written and
something must bound it. Round 6 showed that reversing it was wrong, because
`extracted` means "the manifest was not complete", not "bytes were written",
and the two come apart exactly here — `extract_comic_full` reads the entry
list before writing anything, so a corrupt CBZ or an unopenable CBR fails
having written nothing at all. Firing there gave an unauthenticated LAN
client a full-cache eviction walk per request against a single broken comic.
Telling the two cases apart cheaply is not possible in that spot either:
`Storage::list` walks the whole storage root, so probing on every error would
cost more than the sweep it was guarding.

**Resolved in round 7, and the reasoning above was wrong twice over.**

`extract_comic_full` now cleans up its own partial writes, so a failed
extraction leaves the cache as it found it and there is nothing for the
caller to infer. `a_failed_extraction_leaves_nothing_behind` pins it.

The claim that lingering bytes were "self-healing" was false, and round 8
caught it: `run_eviction`'s `evict_orphan_prefixes` deliberately spares
manifest-less page files — it removes only an orphaned `text-index.json`,
since a prefix without a manifest may be an extraction still in flight, and
`orphan_sweep_spares_manifestless_page_files` asserts exactly that. Nor does
`collect_cached_books` count them, so they never reach the size budget
either. Such bytes survive until `clear_cache`. That pass's own comment
defers orphan handling to "the prewarm/extraction paths", and the round-7
cleanup is that handling for comics.

What remains: the cleanup is best effort. If it fails for the same reason the
write did — a full disk — manifest-less pages persist with nothing reclaiming
them. Widening `evict_orphan_prefixes` to collect page files whose prefix has
no manifest *and* no writer in flight would close it, but distinguishing
"abandoned" from "in flight" is what that pass avoids today, and it would need
an explicit in-flight marker to do safely.

## Still open: `ensure_cached`'s corrupt-manifest branch walks the cache per request (M3 round 9)

`extract_comic_full`'s cleanup is now guarded on a write having landed, so a
corrupt archive no longer costs a whole-cache walk per request. The sibling
call one frame up is not guarded: `ensure_cached`'s corrupt/partial-manifest
branch calls `evict_book` unconditionally and discards the result with
`let _`. `evict_book` lists the book's prefix, and `LocalStorage::list` walks
the entire cache root before filtering.

If those deletes keep failing — a read-only cache mount, or a manifest that
survived a partially-failed `evict_book` — the manifest stays on disk and
every later request from the same client repeats the full walk. The web
route's PIN is optional, so that is loopable.

Pre-existing, and untouched by M3. Fixing it means either making
`evict_book` cheap (a prefix-scoped listing rather than a whole-root walk —
see `Storage::list`'s implementation, which is the shared root cause of
several findings in this epic) or not re-attempting eviction for a hash whose
last attempt failed. The first is the better fix and would help every caller.

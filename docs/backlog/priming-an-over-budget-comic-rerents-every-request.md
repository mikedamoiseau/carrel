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

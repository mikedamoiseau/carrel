# The OPDS feed handlers share an unextracted ETag envelope

Found during the review of milestone 3 of the library-query epic (PR #142),
which gave `/opds/search` the paging and conditional-request handling the other
feeds already had. Deferred deliberately: the fix touches handlers that
milestone did not write.

## Status

M1 of the OPDS-feed-one-renderer epic fixed the actual bug this duplication
was hiding: every page of a feed got the same ETag, so a client that cached
page 0's validator got an empty `304` body back for `?page=1`. The fix folds
each request's own feed URL (`self_href`) into `feed_etag`'s digest, so two
page URLs can no longer produce the same tag, while the digest still hashes
the whole matching set — a library change still invalidates every page at
once.

M2 closed both paging-correctness bugs `all_books` still had. `db::list_books`
now breaks an `added_at` tie by `id`, the same fix `list_books_grid` already
carried, so `/opds/all` can no longer serve one book twice or skip it when a
batch import ties several `added_at` values. `/opds/new` never paginated and
so was never exposed to that; what it gains is a stable answer to *which* 25
books it shows when several tie. `all_books`'s paging arithmetic
(`page * OPDS_PAGE_SIZE`, `start + OPDS_PAGE_SIZE`) now uses
`saturating_mul`/`saturating_add`, matching `search_books`, so a very large
`?page=` value returns an empty page instead of panicking (debug) or wrapping
into a bogus `rel="next"` link (release).

Note what the overflow test does *not* prove: `cargo test` builds debug, where
the pre-fix code panics on the multiply, so the executed test pins the debug
path only. Its `rel="next"` assertion would also catch the release-mode wrap,
but nothing in the gate table runs these tests in release.

M3 gave `carrel-core::opds_feed` the interface this document argues for:
`render_feed(&FeedOptions) -> RenderedFeed` renders a page and digests the
same value it rendered, so no caller can hash one input and emit another.
The digest covers every `FeedOptions` field plus the rendering module's own
source text (`include_str!`), which means a future change to the emitted
shape invalidates cached feeds without anyone maintaining a list of what to
hash. Three earlier attempts at that guarantee were each incomplete in a
different place — a registry of template constants left the timestamp format,
the ATOM type constants and the entry join separator outside the digest — and
the seam that proves the property (`feed_etag_over`) exists because the first
guard passed with the hash loop narrowed to one element.

M4 deleted the desktop's copy. `xml_escape`, `book_to_entry`, `wrap_feed`,
`cover_mime` and the `ATOM_*` constants are gone from `src-tauri` (241
lines), and all six routes render through the core crate.
`OPENSEARCH_DESC_TYPE` deliberately stays desktop-local: it types the
descriptor *response*, which is the web server's business, not the feed
renderer's. Output is
pinned byte-for-byte against baselines captured from the pre-adoption tree,
including the two routes that had no baseline at all until the review.

One cost this epic introduced: `render_feed` always computes the digest, and
every caller in *this* repo discards it — `wrap_feed` returns `.body`, and all
five desktop handlers keep their own whole-set tag. `wrap_feed` on `main`
computed no digest at all, so an out-of-repo consumer picks this up per
request on repin.

Most of it was re-hashing this module's own ~55 KB of source text on every
render — the same bytes every time, for the life of the process.
`self_src_digest` caches that read behind a `OnceLock` and the digest hashes
the 32-byte result instead. Equivalent (a change to the source changes its
digest, which changes every feed's tag) and it gives up no property, no
signature and no struct shape.

Measured in release, on a full 50-entry page with realistic entries
(664 B mean, 33 KB total):

| | before | after |
|---|---|---|
| digest | 258.5 µs | 97.3 µs |
| body | 17.5 µs | 17.5 µs |
| whole call | ~276 µs | 114.8 µs |

**Read the absolute number, not the ratio.** The saving is a constant ~161 µs
— the source hash — so the ratio is whatever the entry payload makes it: 15.6x
on a near-empty synthetic feed, 2.4x on a full page. The first bench here used
38-byte entries and produced the 15.6x figure, which describes a feed nobody
serves; the consuming project's session caught that the implied SHA-256
throughput (~345 MB/s, consistent across both measurements) could not account
for 50 real entries in the residue. Requote the full-page numbers.

The residue is real and stays: ~97 µs is almost entirely entry text, which
varies per call and cannot be cached. Removing it needs one of the two
changes this epic rejected — a body-only entry point (restores the
hash-one-render-another hazard) or a lazy `etag` field (changes
`RenderedFeed`'s shape, forbidden while a consumer is pinned). Neither is
worth it against a feed request's DB work.

Worth recording how this was found: the module had already written the cost
off as unavoidable, in a doc comment, having considered exactly two fixes.
The third — cache the digest, not the source — came from the consuming
project's session reading that comment and not accepting its premise.

Still open: the `page` type mismatch between feeds, the whole-set-vs-per-page
digest trade-off (the desktop deliberately keeps its own whole-set `ETag`
rather than `RenderedFeed`'s narrower one, so an edit on a later page still
invalidates earlier ones), the `xml_escape` control-character gap, and
`collection_feed`'s own missing tie-break — it orders by `bc.added_at DESC`
with no unique column, which is the same gap `list_books` just closed. That
one is **latent rather than live**: `collection_feed` takes no page
parameter, so nothing slices its order today. It would become live the moment
that feed learns to paginate.

The envelope itself is no longer duplicated in the sense this document was
filed for, but the *conditional-request* half of it — build `pairs`, build
`rendered_ids`, compute the tag, answer `304` — is still repeated per handler
in `src-tauri`. That was left alone on purpose: `self_href`'s shape differs
per handler, and folding it in would have meant moving the desktop's
whole-set digest policy into core, which is the trade-off in the previous
paragraph.

## What is duplicated

`all_books`, `collection_feed` and now `search_books` in
`src-tauri/src/web_server/opds_feed.rs` each carry the same envelope:

```rust
let pairs = db::book_etag_pairs(&conn)?;
let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
let etag = feed_etag("<feed id>", &self_href, &rendered_ids, &pairs);
if if_none_match_matches(&headers, &etag) {
    return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
}
// ... entries ...
Ok(([(header::CONTENT_TYPE, ...), (header::ETAG, etag)], xml).into_response())
```

Applying the deletion test separates the two halves:

- **The envelope concentrates complexity.** It needs `feed_id`, `self_href`,
  `rendered_ids`, `pairs` and `headers`, and it has three call sites. Worth
  extracting — though `self_href`, added by M1, is the one input whose *shape*
  differs per handler (`?page=N` vs `?q=X&amp;page=N` vs a constant), which is
  the same reason the next bullet leaves the href builders inline.
- **The href builders only move it.** `?page=N` and `?q=X&amp;page=N` are
  genuinely different shapes, and the second needs XML escaping the first does
  not. Leave them inline.

## The bug the duplication is already hiding — closed in M2

`all_books` read through `db::list_books`, which was `ORDER BY added_at DESC`
with **no `id` tie-break**. Books sharing an `added_at` — routine after a
batch import — therefore had no stable order across two requests, so paging
`/opds/all` could serve one book on two pages and skip another entirely. This
was exactly the failure that
`search_paging_over_tied_added_at_is_stable_no_repeat_no_skip` already guarded
for `/opds/search`, and the same argument was already documented on
`book_sort_order_sql` in `carrel-core/src/db.rs`.

M2 fixed `list_books` itself rather than routing `all_books` through
`db::query_books` as first proposed here: `list_books`'s `ORDER BY` now ends
in `, id`, the same tie-break `list_books_grid` already had, which is a
narrower change than moving to the query-module path and leaves `list_books`'s
signature and column list untouched. `/opds/new` shares `all_books`'s fix
since it also reads through `list_books`. `collection_feed` orders by its own
join (`bc.added_at DESC`) and was not touched — it still wants checking rather
than assuming.

## Also noticed, separate and pre-existing

`xml_escape` escapes `&`, `<`, `>` and `"` but strips no C0 control
characters, so `?q=%01` puts a raw control byte inside `<title>Search: …</title>`
and the feed is not well-formed XML. Unchanged by milestone 3 — the same was
true before it — but a strict OPDS client would reject the response.

## Added after the round-2 review of the same milestone

Three more reasons these handlers want looking at together.

**`all_books` had the same paging overflow `search_books` was fixed for —
closed in M2.** `start = page * OPDS_PAGE_SIZE` and `start + OPDS_PAGE_SIZE`
were unsaturated, and `page` comes off the wire. A large-but-parseable
`?page=` panicked in debug — a 500, with no `CatchPanicLayer` in front of the
web server — and wrapped in release, where the wrapped sum read as "there is a
next page" and emitted a `rel="next"` link back to page 0. `all_books` now
uses `saturating_mul`/`saturating_add`, matching `search_books`, and has its
own test at `usize::MAX`.

**The two feeds disagree on how strict a `page` is.** `SearchQuery::page` is a
lenient `String`, because a strictly-typed optional param turns one malformed
value a proxy appended into a dead endpoint — the reasoning `api.rs`'s
`BookQuery` documents on `want_to_read`. `PaginationQuery::page` is still
`Option<usize>`, so `/opds/all?page=abc` returns 400 where
`/opds/search?q=x&page=abc` now serves page 0. The lenient one is the better
convention and the strict one guards the feed clients actually walk, so this
is the wrong way round. Neither tolerates a *repeated* `?page=1&page=2`, since
serde's derive rejects the duplicate field first; that is at least consistent
with how a repeated `?q=` has always behaved.

**SQL paging is available if the whole-set digest is given up.** The reason
`search_books` fetches every matching row to render fifty is that its tag
covers the whole filtered set, so it needs the complete id list. A per-page tag
would also be *correct* — it hashes the ids actually rendered, and a deletion
on an earlier page changes which ids those are — so `LIMIT`/`OFFSET` paging
via `db::query_books`'s existing `limit`/`offset` is on the table. What would
be lost is uniform invalidation: one library change moving every page's tag
at once, which the whole-set digest still does today (each page's tag is now
also unique to its own URL, per the fix in "Status" above, but the digest
itself still covers the whole filtered set, not just the page). Worth
deciding once, for all four feeds, rather than per handler.

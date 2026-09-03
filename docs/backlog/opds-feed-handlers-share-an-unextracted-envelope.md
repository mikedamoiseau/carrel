# The OPDS feed handlers share an unextracted ETag envelope

Found during the review of milestone 3 of the library-query epic (PR #142),
which gave `/opds/search` the paging and conditional-request handling the other
feeds already had. Deferred deliberately: the fix touches handlers that
milestone did not write.

## What is duplicated

`all_books`, `collection_feed` and now `search_books` in
`src-tauri/src/web_server/opds_feed.rs` each carry the same envelope:

```rust
let pairs = db::book_etag_pairs(&conn)?;
let rendered_ids: Vec<&str> = books.iter().map(|b| b.id.as_str()).collect();
let etag = feed_etag("<feed id>", &rendered_ids, &pairs);
if if_none_match_matches(&headers, &etag) {
    return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
}
// ... entries ...
Ok(([(header::CONTENT_TYPE, ...), (header::ETAG, etag)], xml).into_response())
```

Applying the deletion test separates the two halves:

- **The envelope concentrates complexity.** It needs only `feed_id`,
  `rendered_ids`, `pairs` and `headers`, and it has three call sites. Worth
  extracting.
- **The href builders only move it.** `?page=N` and `?q=X&amp;page=N` are
  genuinely different shapes, and the second needs XML escaping the first does
  not. Leave them inline.

## The bug the duplication is already hiding

`all_books` still reads through `db::list_books`, which is
`ORDER BY added_at DESC` with **no `id` tie-break**. Books sharing an
`added_at` — routine after a batch import — therefore have no stable order
across two requests, so paging `/opds/all` can serve one book on two pages and
skip another entirely. This is exactly the failure that
`search_paging_over_tied_added_at_is_stable_no_repeat_no_skip` now guards for
`/opds/search`, and the same argument is documented on `book_sort_order_sql`
in `carrel-core/src/db.rs`.

The fix is the same one milestone 3 applied to search: read through
`db::query_books` with `BookSort::DateAdded`, whose `ORDER BY` ends in `b.id`.
`/opds/new` takes 25 by recency and `collection_feed` orders by its own
join, so they want checking too rather than assuming.

## Also noticed, separate and pre-existing

`xml_escape` escapes `&`, `<`, `>` and `"` but strips no C0 control
characters, so `?q=%01` puts a raw control byte inside `<title>Search: …</title>`
and the feed is not well-formed XML. Unchanged by milestone 3 — the same was
true before it — but a strict OPDS client would reject the response.

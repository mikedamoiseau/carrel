# Automated collection rules bind their parameters in the wrong order

> **FIXED** in milestone 6 of PR #142 (`build_rule_query` now returns its
> parameters join-first, matching the order their placeholders appear in the
> SQL its callers assemble). Kept as a record of what the bug was and how it
> was found. The two follow-ups at the end — collapsing
> `get_books_in_collection_grid` and `list_books_grid` into the query module —
> are **not** done.

**Silent wrong results, pre-existing, user-facing.** Found during the review of
milestone 4 of the library-query epic (PR #142), which added a second caller of
`build_rule_query` and so made an existing bug reachable from one more path.
Initially deferred, then fixed in the same PR at the user's direction.

## The bug

`build_rule_query` (`carrel-core/src/db.rs`) returns
`(joins, where_str, param_values)`. It walks the rules once and, per rule,
pushes either into `join_clauses` (the `tag` and reading-progress rules) or into
`where_clauses` (every plain field rule) — while pushing **every** rule's
parameter onto one flat `param_values` in **rule order**.

Callers then assemble the SQL as `... FROM books b {joins} {where_str}`, so
every join placeholder precedes every where placeholder in the statement text.
Bound parameters are positional. The two orders disagree whenever a join-rule
follows a where-rule.

Verified by execution, not inspection. For
`[series equals "Dune", tag contains "scifi"]`:

```
joins  = JOIN book_tags bt1 ON bt1.book_id = b.id
         JOIN tags tt1 ON tt1.id = bt1.tag_id AND tt1.name LIKE ?
where  = WHERE b.series = ?
params = ["Dune", "%scifi%"]
```

The first placeholder in the text belongs to the tag JOIN, and the first bound
value is `"Dune"`. So the query actually runs `tt1.name LIKE 'Dune'` and
`b.series = '%scifi%'` — it returns the wrong books, with no error anywhere.

## Who is affected

- `db::get_books_in_collection_grid` — the desktop's automated-collection view
  (`src-tauri/src/commands.rs`) and the web UI's `/api/collections/{id}/books`.
- `db::get_books_in_collection` — the non-grid sibling, same SQL shape. Fixed by
  the same change, but no test exercises it directly; the ordering contract is
  asserted through the grid variant and the preview instead.
- `db::preview_collection_rules` — the rule builder's own preview, so the
  preview is wrong in the same way, which is why this has been easy to miss.
- `db::query_books_in_collection_grid` (added in the milestone above) inherited
  it. Its own predicate params were always bound correctly — they are textually
  last — and only the rule params ahead of them were mis-ordered.

A single-rule collection is fine. So is any collection whose rules are all
field rules, or all tag/progress rules, or which happens to list its tag rules
first. The bug needs a mixed rule set in the wrong order, which is why
automated collections mostly work.

## The fix, as applied

`build_rule_query` now collects into two vectors — `join_params` and
`where_params` — and returns `join_params` followed by `where_params`. Since
every caller already assembles `{joins} {where_str}` and binds the returned
vector positionally and unchanged, **no call site needed changing**: the
ordering contract now lives in the one function that knows both halves.

Only the two `tag` arms ever put a parameter in a JOIN. The
`reading_progress` arms add joins too, but their where-clauses are
parameterless, so they were never affected.

Two tests, both written red first and both verified to fail when the
concatenation is reversed:

- `automated_collection_binds_join_and_where_params_in_sql_order` — a field
  rule before a tag rule, through `get_books_in_collection_grid` *and*
  `query_books_in_collection_grid`, plus the tag-first order that already
  worked so the fix cannot regress it. Before the fix it returned no books at
  all where one matched.
- `preview_collection_rules_counts_join_and_where_rules_together` — the third
  caller, which is why this was easy to miss.

While there, `get_books_in_collection_grid` could delegate to
`query_books_in_collection_grid` with a `BookQuery::default()` — the two now
carry the same two SQL shapes, and the default predicate contributes no
clauses, so the SQL is identical. That removes the duplication rather than
moving it, and it means one place to fix rather than two.

The same applies to `list_books_grid` against `query_books_grid`: with a
default `BookQuery` the predicate is empty and the order is `added_at DESC, id`
in both, so the older function contributes nothing the newer one does not. Both
wrappers are worth collapsing in the same pass — a whole-branch review of the
epic above applied the deletion test to them and reached the same conclusion.
Neither collapse is free, though: `list_books_grid` and
`get_books_in_collection_grid` are called from the desktop IPC layer
(`src-tauri/src/commands.rs`), so the change needs the Tauri-side tests run,
not just `carrel-core`'s.

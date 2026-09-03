# Three client-side book-filter copies, one of them dead

Found while reviewing the library-query epic (PR #142), which consolidated
every *server-side* copy of "case-insensitively match title or author" into
`carrel_core::db::query_books*`. The client-side copies were out of that epic's
scope and stay for now; this records what is left and which of it is worth
acting on.

## What exists

| Where | Status |
|---|---|
| `src/screens/Library.tsx` | **Keep.** Filters an already-loaded grid and feeds `computeTagBookCounts`, so the pre-tag-filter set has to exist in the browser. Routing it through SQL would mean an IPC round-trip per keystroke plus a facet-count API, for no user-visible gain on a local database. This was the epic's stated out-of-scope. |
| `src/components/BookPickerModal.tsx` | **Probably keep.** Filters a small list the modal was handed. Real duplication, but the alternative is a query API for a picker over data already in memory. |
| `src/lib/utils.ts` — `filterBooks` | **Delete.** Imported by nothing but `src/lib/utils.test.ts`. |

## `filterBooks` is the one to act on

It is a `books.filter(...)` over search, format, status and rating, extracted
as a pure function — and no production code calls it. `Library.tsx` has its own
inline copy of the same logic (`:571-605`) and does not import this one, so the
two can drift silently and only one of them affects users.

It has ten tests in `utils.test.ts`, which is what makes it look maintained.
They pass whatever `Library.tsx` does, because they do not touch it. This is
the shape the `codebase-design` vocabulary calls a shallow module extracted for
testability while the real behaviour — and any real bug — lives in the caller.

Deleting the function and its tests removes a copy of the filter rules without
moving them anywhere, and drops the false signal that this path is covered. The
alternative, if the tests are considered worth keeping, is the opposite move:
have `Library.tsx` actually call `filterBooks`, which makes the tests mean
something. Either is an improvement; keeping both as they are is not.

Check `git log -- src/lib/utils.ts` before deleting, in case a caller was
removed recently and is expected back.

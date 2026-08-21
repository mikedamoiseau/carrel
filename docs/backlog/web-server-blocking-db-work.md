# Move the web server's SQLite work off the async executor

**Status:** deferred (2026-08-21). Scoped out of the LAN-hardening epic
(PR #136) after the site count turned out to be more than twice what the plan
assumed.

## What

Every `api.rs` handler that touches the database calls `state.conn()` and then
runs `rusqlite` queries directly on the Tokio worker thread that is serving the
request. There is no `spawn_blocking` anywhere on the DB path — the only
`spawn_blocking` calls in `web_server/` are for filesystem work (`fs::metadata`,
cover-thumb resolution). A slow query therefore occupies a runtime worker for
its whole duration instead of a blocking-pool thread.

The intended shape is a `WebState::with_conn(|conn| …)` helper that wraps the
pool checkout and the closure in `tokio::task::spawn_blocking`, then converting
the call sites to it.

## Why it was deferred

The milestone plan said 37 call sites. The actual count is **77** `state.conn()`
calls in `api.rs`. Converting them means restructuring each handler that holds
a `PooledConnection` across later code — a mechanical diff far past the size a
reviewer can hold in one pass, for a payoff that is *responsiveness under load*,
not hardening. It does not belong in an epic about what the server hands to an
untrusted LAN client; it belongs in its own epic, where the win can be measured
rather than asserted.

## Worth knowing before picking it up

- SQLite on a local file is sub-millisecond for most of these queries; the
  cases that actually hurt are the ones that scan (library list with filters,
  search, stats) and anything holding a connection while touching a
  network-mounted book file. A first pass could convert only those and measure,
  rather than converting all 77 on principle.
- `WebState.pool` is `Arc<Mutex<DbPool>>` and is swapped on profile switch, so
  a closure moved into `spawn_blocking` must clone the `Arc` and check the pool
  out *inside* the closure — checking out first and moving the connection in
  would pin a connection across the hop.
- `r2d2::PooledConnection` is not `Send`-friendly to hold across an `.await`;
  handlers that currently interleave DB reads with `await`s (page staging,
  cover resolution) need the DB work grouped into one closure per hop, not one
  closure per query, or the hop count becomes the new cost.
- Measure first: `CARREL_LOG` at debug already timestamps request handling, and
  the page-cache work in the reader-perf epic (PR #111/#112) established the
  pattern of proving a bottleneck before restructuring for it.

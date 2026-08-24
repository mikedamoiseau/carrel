# Cap the error log a LAN client can grow

**Status:** open (noted 2026-08-21, during the LAN-hardening epic, PR #136).

## What

`observability.rs` sets up a daily-rolling file appender with no retention
limit: old files are never pruned, and nothing bounds a single day's size. The
web server is reachable by anything on the user's LAN that gets past the PIN —
and the PIN is optional — so request-driven log lines are attacker-paced.

Milestone 3 of the LAN-hardening epic took the sting out of the specific case a
reviewer found: `book_file_status`'s `NotFound`/`InvalidInput`/
`PermissionDenied` arms log at `warn` rather than `error`, because a 404 on a
page or cover route is an ordinary outcome rather than a server fault. That
changed which level the noise arrives at. It did not bound the volume.

## Why it wasn't done in that epic

Retention is a logging-configuration change with its own failure modes — a cap
that is too aggressive throws away the diagnostics a real bug report needs, and
the appender's rotation interacts with whatever the user's disk and backup
arrangements are. It also wants a decision about what to keep (N days? N MB?
both?), which is a product question rather than a defect fix. Bundling it into a
security epic would have meant deciding that quietly.

## Worth knowing before picking it up

- `tracing-appender`'s rolling file appender takes a max-files setting; a
  size-based cap needs either a different appender or a periodic prune.
- `CARREL_LOG` controls the level filter, so a user who raised it to debug will
  generate far more per request than the default `info` — any cap should be
  chosen for the noisiest supported configuration, not the default one.
- Rate-limiting at the *source* is the alternative shape: collapse repeated
  identical warnings from one client into a count. That keeps a real
  investigation readable, which a size cap does not.

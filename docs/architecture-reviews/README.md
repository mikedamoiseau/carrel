# Architecture reviews

Reports from `/mattpocock-skills:improve-codebase-architecture` live here.

That command writes its HTML report to the OS temp directory by default. On
macOS that is `/var/folders/…/T/`, which the system purges — the review of
2026-08-25 was lost that way, and nothing recoverable was left behind (the
report is written straight to disk, so no session transcript holds a copy).

So when running it, tell it where to put the file:

```
/mattpocock-skills:improve-codebase-architecture Write the HTML report to
docs/architecture-reviews/ inside the repo, not to $TMPDIR.
```

## Reports

- `architecture-review-20260903-113724.html` — 10 candidates across the IPC
  adapter, the LAN web server, and both user interfaces. Top recommendation:
  give "filter and sort a library" a home in `carrel-core`. Excludes the reader
  path, deepened in PR #141.

The 2026-08-25 report was lost before this directory existed. Its top
recommendation was "Deepen: core Reader", which shipped as PR #141 on
2026-08-27: `carrel_core::reader` is now the deep module both the Tauri IPC
adapter and the embedded web server read books through.

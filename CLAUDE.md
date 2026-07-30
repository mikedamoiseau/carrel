# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

Lint and formatting are checked workspace-wide from the repo root (CI-enforced). Running them scoped to `src-tauri/` only covers the `carrel` crate, not `carrel-core`; omitting `--all-targets` skips test/example targets:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features mobi -- -D warnings  # libmobi-gated paths
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml` (currently `1.96.0`); CI uses the same version via `dtolnay/rust-toolchain@1.96.0`, so local and CI rustfmt/clippy never drift. Bump both together.

Running `cargo test` from `src-tauri/` only exercises the `carrel` crate with *default* features — `carrel-core` has its own test binary that is not compiled by that invocation, and the `mobi` feature changes which assertions compile. There are therefore **three** distinct Rust test invocations, and CI runs all three:
```bash
cd src-tauri && cargo test                                  # carrel, default features
cd src-tauri && cargo test --features mobi                  # carrel, libmobi-gated arms
cargo test -p carrel-core --features mobi -- --test-threads=1   # carrel-core (workspace root)
```

The middle one is easy to miss and is not redundant: `src-tauri/src/commands.rs` and `web_server/api.rs` carry paired `#[cfg(feature = "mobi")]` / `#[cfg(not(feature = "mobi"))]` test arms, so the default run and the `--features mobi` run assert *different* things. The `--test-threads=1` on the carrel-core run mirrors CI's Linux job, which serialises it because libmobi's C-side cleanup races across threads there — parallel drops can double-free shared internal state and SIGABRT *after* a passing test (see the comment on that step in `ci.yml`). CI's Windows job omits the flag; keeping it locally is harmless and matches the strictest job.

`pnpm run test:e2e` runs against a seeded harness (`src-tauri/examples/web_e2e_server.rs`); Playwright manages the server's lifecycle (build, start, health-check, teardown), so no manual setup is needed. First run in a fresh clone needs `pnpm exec playwright install --with-deps chromium`.

MOBI tests require a public-domain test corpus under `src-tauri/test-fixtures/` (gitignored). Populate once with `./scripts/fetch-mobi-test-corpus.sh`. Fixture-gated tests skip with a clear message when fixtures are absent, so fresh clones stay green without the corpus.

## Architecture

**Tauri v2 desktop app** (branded "Carrel") — Rust backend + React 19 frontend communicating via IPC. All data flows through Tauri's `invoke()` IPC bridge. Commands are registered in `src-tauri/src/lib.rs` via `invoke_handler` — every new command must be added there.

The backend is two crates: **`carrel`** (`src-tauri/src/`) — the Tauri shell, IPC commands, and web server — and **`carrel-core`** (`carrel-core/src/`) — parsing, DB, and models, with no Tauri dependency.

### Persistence-boundary identifiers — keep stable

The app was renamed Folio → Carrel after `v2.11.1`. `3.0.0` shipped the
user-visible half; the rename was then completed in full, including the bundle
identifier and every persisted key, so no `folio` identifiers remain. `CHANGELOG.md`,
`docs/superpowers/`, and `src-tauri/.pr-reviews/` still say Folio because they
are historical records of work that shipped under that name — leave them.

What follows is **not** a do-not-rename list any more. It is the list of strings
that key data living *outside* the repo — on disk, in the OS keychain, in a
browser, on a user's own remote. Changing one of them does not migrate the data
it names; it orphans it. Treat each as a stable identifier and change it only
together with a migration.

| Identifier | Where | Changing it would… |
|---|---|---|
| `com.mike.carrel` | `tauri.conf.json` `identifier` | orphan every install's app-data dir, macOS prefs domain, and keychain entries |
| `com.mike.carrel.profile-lock` | `carrel-core/src/profile_lock.rs` | orphan every profile-lock password |
| `carrel-backup-{provider}-{key}` | `carrel-core/src/backup.rs` | orphan every configured backup's stored SFTP/S3 credentials |
| `carrel-web-server` | `web_server/auth.rs` | orphan the stored web-UI PIN |
| `Carrel Library` | `carrel-core/src/paths.rs` | silently relocate the library for every install that never set `library_folder` (it is an unwritten *fallback*, not a stored setting) |
| `.carrel-sync/…` | `carrel-core/src/sync.rs` | orphan sync state already written to the user's own remote |
| `urn:carrel:*` | `web_server/opds_feed.rs` | break OPDS clients, which cache on feed/entry ids |
| `carrel_session` cookie, `x-carrel-profile` header | `web_server/` + `static/` | break offline-cached `app.js`/`sw.js`, which still send and read the old names |
| `carrel-shell-*`, `carrel-offline-book-*`, `carrel-offline-scope` | `static/sw.js`, `static/app.js` | orphan every offline-saved book on every user's device |
| `carrel-*` / `carrel_*` localStorage keys | `src/context/ThemeContext.tsx`, `src/screens/Library.tsx`, `static/app.js`, … | reset every user's theme, typography, filters, and onboarding state |
| `CARREL_APTABASE_KEY`, `CARREL_LOG`, `CARREL_DEBUG_PAGES`, `CARREL_E2E_PORT` | `build.rs`, `analytics.rs`, CI | break the GitHub Actions repo variable and existing local/CI env |
| `carrel-core`, `CarrelError`, `CarrelResult`, `CarrelEvent` | `carrel-core/` and every caller | break Carrel Server, which consumes this crate as a git dependency pinned to a release tag |
| `carrel-offline` IndexedDB database | `static/app.js` | orphan every offline-saved book's blobs (separate from the Cache Storage entry above) |
| `21c2cdba-327a-5023-94aa-a2fbf307774c` | `tauri.conf.json` `bundle.windows.wix.upgradeCode` | make every Windows MSI install **side-by-side** with the user's existing install instead of upgrading it in place |

The WiX upgrade code deserves a note, since it is the one entry that is a bare
UUID rather than a readable string. Tauri derives it by default from
`uuid5(DNS, "{productName}.exe.app.x64")`, so it moves whenever `productName`
moves. It is pinned instead, to `uuid5(DNS, "Folio.exe.app.x64")` — the value
every shipped Folio MSI carries, confirmed by reading the `UpgradeCode` property
out of `Folio_2.11.1_x64_en-US.msi`. WiX `MajorUpgrade` matches on this code
alone, so pinning it is what lets a new MSI upgrade an existing install in place
regardless of what the product is called. Check it with
`npx tauri inspect wix-upgrade-code`, which prints both the derived default and
the override; `tauri_config_test.rs` asserts the pin. Never change this value.

The GitHub repo is `mikedamoiseau/carrel` (renamed from `mikedamoiseau/folio`
2026-07-30). The slug is load-bearing in `src-tauri/src/update.rs`'s
release-URL allowlist, `UpdateModal.tsx`'s `isTrustedReleaseUrl` /
`isTrustedChangelogUrl`, and the dictionary-download URL in `commands.rs` —
those three must agree or update checks start rejecting valid releases.

Two identifiers here fail **silently** if changed carelessly, so they are worth
singling out:

- `CARREL_APTABASE_KEY` is read via `option_env!`, so a missing variable is not
  a build error — it compiles to an empty key and the analytics SDK quietly
  disables itself. It is a GitHub Actions repo *variable* (not a secret); renaming
  it means creating the new variable first.
- `Carrel Library` is an unwritten fallback rather than a stored setting, so an
  install that never set `library_folder` re-derives it every launch. Changing
  the string relocates that library with no error anywhere.

The embedded web UI (`src-tauri/src/web_server/static/`: `index.html` + `app.js` + `app.css`, served via `include_str!`/`include_bytes!`) is a hand-written vanilla-JS SPA, independent of the React desktop frontend — it shares no code or styling with `src/`. Its service worker's `CACHE_VERSION` (`static/sw.js`) is a content hash of the shell assets, enforced by a test — bump it whenever those files change.

### Book Storage

Books are copied into an app-managed library folder (default `~/Documents/Carrel Library/`). The `file_path` in the DB points to the library-internal copy. Covers are extracted to `{app_data_dir}/covers/{book_id}/`.

## Adding Common Things

Covered by project skills — invoke them instead of working from memory: `add-tauri-command` (new IPC command), `add-book-format` (new e-book/comic format), `db-schema-migration` (SQLite schema changes).

## Format Support

PDF support requires pdfium binaries bundled in `src-tauri/resources/`. The `scripts/download-pdfium.sh` script fetches them. Run `./scripts/download-pdfium.sh` before first `pnpm run tauri dev` — PDF import/rendering won't work without it.

### macOS Tahoe C++ Header Fix

On macOS Tahoe (26.x), the Xcode Command Line Tools have a broken C++ header search path — clang can't find `<new>` and other standard headers, which breaks compilation of `unrar_sys` (and potentially other native crates). The fix is:

```bash
export CPLUS_INCLUDE_PATH="/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/c++/v1"
```

This is added to Mike's `~/.zshrc`. If builds fail with `fatal error: 'new' file not found`, ensure this env var is set.

## Coding Principles

**Think first.** State assumptions before coding. If multiple interpretations exist, present them — don't pick silently. If a simpler approach exists, say so. If something is unclear, stop and ask.

**Simplicity over cleverness.** Write the minimum code that solves the problem. No speculative features, no abstractions for single-use code, no "just in case" error handling. If 200 lines could be 50, rewrite it.

**Surgical changes only.** Every changed line should trace directly to what was asked. Don't improve adjacent code, comments, or formatting. Don't refactor things that aren't broken. Match existing style. If you notice unrelated issues, mention them — don't fix them silently.

**Verify before claiming done.** Transform tasks into verifiable goals: "fix the bug" means write a test that reproduces it, then make it pass. Run the actual commands (the gate table under "CI" below) and confirm output before saying something works. Evidence before assertions.

## Security

- EPUB HTML is sanitized server-side (ammonia) and client-side (DOMPurify)
- CSP configured in `tauri.conf.json`
- Asset protocol scoped to `$APPDATA/**`
- File deduplication uses SHA-256 hash (`file_hash` column in `books` table)
- Archive bounds are enforced at `carrel-core`'s API boundary, not by caller diligence: every path-based EPUB entry point opens via `epub::open_validated` (entry-count + declared-size pre-scan), and entry reads are capped through `Read::take` — `MAX_TEXT_ENTRY_SIZE` (16 MB) for text entries, `MAX_ENTRY_SIZE` (100 MB) for binary ones. The pre-scan alone is not a bound: the zip crate limits a read by an entry's *compressed* size, so a size-understating entry needs the read cap. Covers EPUB and CBZ. CBR relies on unrar truncating output at the declared `unpacked_size`; MOBI has no archive layer and is bounded inside libmobi. Callers of the `*_from_archive` / `*_from_cache` variants must validate themselves
- MOBI/AZW parsing uses libmobi (C) via `unsafe` FFI on untrusted input; the from-source builds (Windows + arm64-macOS release) pin `LIBMOBI_VERSION` (tag v0.12, drift-enforced by `release_workflow_test.rs`) while package-manager builds (Linux/macOS CI, local dev) track the distro version — see the security note atop `carrel-core/src/mobi/mod.rs` for the trust boundary and bump process

## CI

**The package manager is `pnpm`, not npm.** CI runs `pnpm install` and `pnpm run …`; `pnpm-lock.yaml` is the lockfile CI resolves against. A stale `package-lock.json` is also checked in and `package.json` has no `packageManager` field, so nothing stops `npm install` from succeeding locally with a *different* dependency tree than CI installed. Use pnpm.

### The full gate set

This is the authoritative list — every command CI runs, and the directory it runs from. Run all of them before pushing:

| Gate | Command | From |
|---|---|---|
| Format | `cargo fmt --all --check` | repo root |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | repo root |
| Lint (mobi) | `cargo clippy --workspace --all-targets --features mobi -- -D warnings` | repo root |
| Rust tests | `cargo test` | `src-tauri/` |
| Rust tests (mobi) | `cargo test --features mobi` | `src-tauri/` |
| Core tests (mobi) | `cargo test -p carrel-core --features mobi -- --test-threads=1` | repo root |
| Type-check | `pnpm run type-check` | repo root |
| Frontend tests | `pnpm run test` (Vitest) | repo root |
| Web UI e2e | `pnpm run test:e2e` (Playwright) | repo root |

Two `--workspace`/`-p` distinctions bite anything that scopes to one crate: clippy run inside `src-tauri/` never sees `carrel-core`, and `cargo test` inside `src-tauri/` never compiles carrel-core's test binary. Both need the workspace-level invocation above.

### The pre-push hook is weaker than CI

A pre-push hook runs `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`, `cargo test` (in `src-tauri/`), `npm run type-check`, `npm run test`. That is **five of the nine gates** — it does not run either mobi clippy/test variant, the carrel-core test binary, or the e2e suite, and it still shells out to `npm`. A green hook is not a green CI. Run the table above yourself; do not treat the hook as the gate, and never bypass it with `--no-verify`.

### CI only triggers on `main`

`.github/workflows/ci.yml` is `on: push: branches: [main]` and `pull_request: branches: [main]`. **Pushing a feature or epic branch triggers no run at all.** An absent run looks identical to a passing one in `gh run list` output, so a branch push with no PR reads as "no failures" when it means "nothing was checked". To get CI on a branch, open a PR targeting `main` (a draft PR is enough) and watch `gh pr checks <branch>`.

Jobs: `Rust Tests` (ubuntu), `Rust Lint`, `Rust Tests (macOS, --features mobi)`, `Rust Tests (Windows, --features mobi)`, `Frontend TypeScript Check`, `Web UI E2E`.

### User-facing docs to update alongside a change

There is no `ROADMAP.md` in this repo. The docs that track user-visible change are `CHANGELOG.md` (entries accrue under `## [Unreleased]` until a release is cut), `README.md` (only if the feature list or a documented behavior changed), `docs/USER_GUIDE.md`, and `docs/backlog/` for deferred work.

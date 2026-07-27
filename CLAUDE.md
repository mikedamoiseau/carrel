# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

Lint and formatting are checked workspace-wide from the repo root (CI-enforced). Running them scoped to `src-tauri/` only covers the `folio` crate, not `folio-core`; omitting `--all-targets` skips test/example targets:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features mobi -- -D warnings  # libmobi-gated paths
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml` (currently `1.96.0`); CI uses the same version via `dtolnay/rust-toolchain@1.96.0`, so local and CI rustfmt/clippy never drift. Bump both together.

Running `cargo test` from `src-tauri/` only exercises the `folio` crate — `folio-core` has its own test binary that is not compiled by that invocation. For MOBI changes always also run (from the workspace root):
```bash
cargo test -p folio-core --features mobi
```

`npm run test:e2e` runs against a seeded harness (`src-tauri/examples/web_e2e_server.rs`); Playwright manages the server's lifecycle (build, start, health-check, teardown), so no manual setup is needed.

MOBI tests require a public-domain test corpus under `src-tauri/test-fixtures/` (gitignored). Populate once with `./scripts/fetch-mobi-test-corpus.sh`. Fixture-gated tests skip with a clear message when fixtures are absent, so fresh clones stay green without the corpus.

## Architecture

**Tauri v2 desktop app** (branded "Folio") — Rust backend + React 19 frontend communicating via IPC. All data flows through Tauri's `invoke()` IPC bridge. Commands are registered in `src-tauri/src/lib.rs` via `invoke_handler` — every new command must be added there.

The backend is two crates: **`folio`** (`src-tauri/src/`) — the Tauri shell, IPC commands, and web server — and **`folio-core`** (`folio-core/src/`) — parsing, DB, and models, with no Tauri dependency.

The embedded web UI (`src-tauri/src/web_server/static/`: `index.html` + `app.js` + `app.css`, served via `include_str!`/`include_bytes!`) is a hand-written vanilla-JS SPA, independent of the React desktop frontend — it shares no code or styling with `src/`. Its service worker's `CACHE_VERSION` (`static/sw.js`) is a content hash of the shell assets, enforced by a test — bump it whenever those files change.

### Book Storage

Books are copied into an app-managed library folder (default `~/Documents/folio/`). The `file_path` in the DB points to the library-internal copy. Covers are extracted to `{app_data_dir}/covers/{book_id}/`.

## Adding Common Things

Covered by project skills — invoke them instead of working from memory: `add-tauri-command` (new IPC command), `add-book-format` (new e-book/comic format), `db-schema-migration` (SQLite schema changes).

## Format Support

PDF support requires pdfium binaries bundled in `src-tauri/resources/`. The `scripts/download-pdfium.sh` script fetches them. Run `./scripts/download-pdfium.sh` before first `npm run tauri dev` — PDF import/rendering won't work without it.

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

**Verify before claiming done.** Transform tasks into verifiable goals: "fix the bug" means write a test that reproduces it, then make it pass. Run the actual commands (`cargo test`, `npm run test`, `npm run type-check`) and confirm output before saying something works. Evidence before assertions.

## Security

- EPUB HTML is sanitized server-side (ammonia) and client-side (DOMPurify)
- CSP configured in `tauri.conf.json`
- Asset protocol scoped to `$APPDATA/**`
- File deduplication uses SHA-256 hash (`file_hash` column in `books` table)
- Archive bounds are enforced at `folio-core`'s API boundary, not by caller diligence: every path-based EPUB entry point opens via `epub::open_validated` (entry-count + declared-size pre-scan), and entry reads are capped through `Read::take` — `MAX_TEXT_ENTRY_SIZE` (16 MB) for text entries, `MAX_ENTRY_SIZE` (100 MB) for binary ones. The pre-scan alone is not a bound: the zip crate limits a read by an entry's *compressed* size, so a size-understating entry needs the read cap. Covers EPUB and CBZ. CBR relies on unrar truncating output at the declared `unpacked_size`; MOBI has no archive layer and is bounded inside libmobi. Callers of the `*_from_archive` / `*_from_cache` variants must validate themselves
- MOBI/AZW parsing uses libmobi (C) via `unsafe` FFI on untrusted input; the from-source builds (Windows + arm64-macOS release) pin `LIBMOBI_VERSION` (tag v0.12, drift-enforced by `release_workflow_test.rs`) while package-manager builds (Linux/macOS CI, local dev) track the distro version — see the security note atop `folio-core/src/mobi/mod.rs` for the trust boundary and bump process

## CI

**Before pushing:** Always run the full CI check suite locally. A pre-push git hook enforces this:
`cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` (from repo root — both cover folio-core), then `cargo test` (in `src-tauri/`), then `npm run type-check && npm run test` (in root). When touching MOBI code also run `cargo test -p folio-core --features mobi` from the workspace root — `src-tauri/`'s `cargo test` does not compile folio-core's test binary.

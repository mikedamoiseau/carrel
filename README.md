# Carrel

A local-first desktop app for people who want to read and organize the books they already own.

![Carrel library](screenshots/01-library-light.png)

Carrel is a cross-platform reader for EPUB, MOBI / AZW / AZW3, PDF, CBZ, and CBR. It keeps your library on your machine and gives you the tools to actually use it well: solid reading controls, sensible organization, metadata cleanup, highlights, profiles, backup, and OPDS catalog support.

## Why Carrel?

A lot of reading apps try to funnel you into a store, an account, or somebody else's ecosystem.

Carrel is for the opposite case: you already have the files, and you want a better home for them.

- Local-first: your books and reading data stay on your machine
- Built for owned files: EPUBs, PDFs, and comics without vendor lock-in
- Good to read in: typography, themes, focus mode, highlights, bookmarks, and progress tracking
- Good to manage: collections, tags, metadata editing, profiles, ratings, backups, and activity history

## Highlights

### Reading
- EPUB 2 & 3 reader with sanitized HTML rendering
- PDF support via bundled Pdfium
- CBZ and CBR comic reading
- EPUB paginated mode and continuous scroll mode
- Table of contents sidebar and chapter navigation
- Full-text search inside EPUB and PDF books (`⌘/Ctrl+F`), with PDF text indexed on disk for instant search every session
- Focus mode for distraction-free reading
- Bookmarks, highlights, and highlight notes
- Select, copy, and highlight text in PDFs too (desktop reader), sharing the same highlights panel and quote cards as EPUB
- Share a highlight as a styled quote-card image — copy to clipboard or save as PNG
- Offline in-reader dictionary — select a word and Define it (Princeton WordNet 3.1, downloaded on demand)
- Vocabulary builder — save the words you look up and review them with spaced-repetition flashcards
- Time-to-finish estimate for EPUB books
- Adjustable font size and advanced typography controls
- Built-in themes: Light, Sepia, Dark, Auto
- Custom fonts and custom CSS override for EPUB content

### Library
- Built for large libraries — a virtualized cover grid and lightweight thumbnails stay smooth with thousands of books
- Import via file picker, drag-and-drop, direct URL, or folder scan
- Copy-on-import into an app-managed library folder
- Duplicate detection using SHA-256 file hashing
- Undo for accidental deletes (single, bulk, and remove-from-collection)
- Search by title or author
- Sort by date added, last read, title, author, progress, or rating
- Filter by format, reading status, minimum rating, and "want to read"
- "Want to read" flag — mark books to read next, with a dedicated filter and an optional home shelf (desktop and web UI)
- Manual and rule-based collections
- Tags with autocomplete
- Metadata editing: title, author, series, language, publisher, cover, rating, tags
- Recently opened books for quick resume
- Per-book reading insights (time spent, sessions, start/finish dates) in the book details view
- Multiple profiles with separate libraries — optionally lock a profile behind a password to keep it out of casual view

### Catalogs, metadata, and backup
- OPDS catalog browsing
- Built-in catalogs such as Project Gutenberg and Standard Ebooks
- Add custom OPDS catalog URLs
- One-click download from catalogs into your library
- Metadata enrichment via OpenLibrary and provider-based scanning
- Library export and backup
- Restore from backup archive
- Activity log for imports, edits, deletes, collection changes, and more
- Reading stats dashboard, including a year-long reading heatmap, a yearly reading goal with a progress ring and pace, and a daily reading-minutes goal

### Remote access
- Read your library from any device on the same WiFi — phone, tablet, or another computer, no install required
- Built-in web reader with QR-code pairing and PIN login, matching the desktop app's design with light/dark/system themes — installable as a PWA, with per-book **Save for offline** reading over a secure (HTTPS/localhost) connection
- Adjustable reading typography (font size, line spacing, font family incl. OpenDyslexic, column width) for EPUB/MOBI via an **Aa** control — embedded fonts, remembered across books
- Table of contents panel (EPUB/MOBI), bookmarks in every format, and text highlighting with notes in 5 colors (EPUB/MOBI) — all shared with the desktop app
- Keyboard shortcuts (`/` to search, arrow-key reader navigation, a shortcuts overlay) and a fast, paginated library with search, filters, and sort
- Reading progress syncs back to your library, with progress badges on book covers and animated swipe page-turns on touch devices
- Installable as a home-screen web app (PWA, including iOS Add to Home Screen) for an app-like feel
- OPDS server so ebook apps (KOReader, Thorium, Calibre, Moon+ Reader) can connect directly
- Sanitized, and limited to reading-state writes (progress, want-to-read, bookmarks, highlights) — no importing, metadata editing, or library changes from remote; PIN hashed in your OS keychain, with rate-limited logins and a login audit trail
- System tray toggles to flip the Web UI and OPDS server on or off

### Extensibility
- Plugin system — small sandboxed scripts that react to events (book imported, highlight created, book finished, …)
- Deny-by-default permissions with an explicit consent dialog per plugin
- Bundled example plugins: auto-tag on import, finish notifications, Markdown highlight export, OPDS auto-download
- See [`docs/PLUGINS.md`](docs/PLUGINS.md) to write your own

### Interface
- Multi-language UI — English and French, with OS-language auto-detection
- "Don't track this session" mode — an app-wide toggle that pauses passive tracking (reading position, session stats, recently-read, activity log) while keeping your highlights and bookmarks; resets off on restart
- Cross-platform desktop app for macOS, Windows, and Linux
- Update check — a tray "Check for Updates" item plus a quiet check on startup (toggleable in Settings) tell you when a newer release is on GitHub, showing release notes and a download link; check-only, nothing is installed for you
- Privacy — local-first by default; optional anonymous usage analytics are **off unless you opt in**, send only a single `app_started` event per launch (OS, app version, locale — never your library or reading data), and can be switched off anytime in Settings. See [Privacy](docs/PRIVACY.md)

## Screenshots

<details open>
<summary>Dark mode</summary>

![Library in dark mode](screenshots/03-library-dark.png)
</details>

<details>
<summary>EPUB reader</summary>

![Reading an EPUB](screenshots/10-reader-epub.png)
</details>

<details>
<summary>Highlights & annotations</summary>

![Highlights panel](screenshots/12-reader-highlights.png)
</details>

<details>
<summary>Reading stats</summary>

![Reading statistics](screenshots/07-reading-stats.png)
</details>

<details>
<summary>Collections</summary>

![Collections sidebar](screenshots/09-collections.png)
</details>

<details>
<summary>Book details</summary>

![Book detail modal](screenshots/15-book-detail.png)
</details>

<details>
<summary>OPDS catalogs</summary>

![Book catalogs](screenshots/08-catalogs.png)
</details>

<details>
<summary>Remote access</summary>

![Remote access & backup settings](screenshots/21-settings-remote-backup.png)
</details>

<details>
<summary>Keyboard shortcuts</summary>

![Keyboard shortcuts](screenshots/14-keyboard-shortcuts.png)
</details>

## Docs

- User guide: [`docs/USER_GUIDE.md`](docs/USER_GUIDE.md)
- Privacy: [`docs/PRIVACY.md`](docs/PRIVACY.md)
- Web server / API reference: [`docs/WEB_SERVER_API.md`](docs/WEB_SERVER_API.md)
- Changelog: [`docs/changelog.html`](docs/changelog.html) ([raw](CHANGELOG.md))
- Roadmap: [`docs/ROADMAP.md`](docs/ROADMAP.md)

## Installation

Pre-built binaries are available on the [GitHub Releases page](https://github.com/mikedamoiseau/folio/releases).

### macOS

Open the `.dmg`, drag **Carrel.app** to **Applications**, then launch it.

#### macOS Gatekeeper: "damaged" / "unidentified developer" warning

Because Carrel is not currently notarized with an Apple Developer certificate, macOS may block it on first launch.

**Recommended — right-click to open (no Terminal):**

1. In **Applications**, right-click (or Control-click) **Carrel.app**.
2. Choose **Open** from the menu.
3. In the dialog, click **Open** again.

This is only needed the first time. macOS remembers the choice, and afterwards you can launch Carrel normally. On macOS 15 (Sequoia) and later, if there is no **Open** option in the right-click menu, double-click the app once, then go to **System Settings → Privacy & Security** and click **Open Anyway**.

**Alternative — clear the quarantine flag from Terminal:**

```bash
xattr -cr /Applications/Carrel.app
```

Then launch the app normally.

#### macOS SMB shares: import fails for accented filenames

Importing from an SMB network share (NAS) can fail with `No such file or directory (os error 2)` for files whose names contain accented characters (`é`, `à`, …). This is a macOS SMB-client bug, not a Carrel one — the file is intact on the server but macOS cannot open it by name, in any application. Workarounds (rename on the server, copy via SSH, or mount over NFS) are described in the [User Guide](docs/USER_GUIDE.md#macos--import-from-a-network-share-fails-with-no-such-file-or-directory-os-error-2).

### Windows

Run the `.msi` installer and follow the prompts. MOBI support is statically linked into `Carrel.exe` — no separate libmobi install is needed.

### Linux

Use the provided `.AppImage` or `.deb` release artifact. For MOBI support, install libmobi via your package manager (`sudo apt install libmobi0` on Debian/Ubuntu — the `.deb` package declares this as a dependency).

## Supported formats

| Format | Notes |
|---|---|
| EPUB 2 / EPUB 3 | Reflowable reading with search, themes, typography, highlights |
| MOBI / AZW / AZW3 | Mobipocket and Kindle formats via libmobi (Linux, arm64 macOS, Windows; not Intel macOS) |
| PDF | Page-based reading via Pdfium |
| CBZ | Comic archive (ZIP) |
| CBR | Comic archive (RAR) |

## Tech stack

### Backend
- Rust
- Tauri v2
- SQLite (`rusqlite` + `r2d2`)
- `ammonia` for EPUB sanitization
- `pdfium-render` for PDF support
- `unrar` for CBR support
- `reqwest` for network operations

### Frontend
- React 19
- TypeScript
- Vite
- Tailwind CSS v4
- DOMPurify

## Development

### Requirements
- [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- Node.js 18+
- Rust stable

### Install dependencies

```bash
npm install
```

### Pdfium setup

PDF support requires Pdfium binaries. Download them before running the app in development:

```bash
./scripts/download-pdfium.sh
```

### Run the app

```bash
npm run tauri dev
```

### Build for production

```bash
npm run tauri build
```

### Useful commands

```bash
npm run type-check
npm run build
npm run test
npm run test:e2e     # Playwright web-UI suite against a seeded local harness (Playwright manages the server; first run builds it)
```

Rust-only commands from `src-tauri/`:

```bash
cargo test
```

Lint and formatting are checked workspace-wide from the repo root (a `src-tauri/`-scoped run misses `folio-core`, and dropping `--all-targets` skips test targets):

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### MOBI test fixtures

MOBI / AZW / AZW3 tests are gated on a public-domain test corpus that is
**not** checked into the repository — the fixtures live under
`src-tauri/test-fixtures/` (gitignored). Populate them once before running
the MOBI tests:

```bash
./scripts/fetch-mobi-test-corpus.sh
```

The script downloads Alice's Adventures in Wonderland from Project
Gutenberg in both legacy Mobipocket (v6) and KF8 (v8 / AZW3) form. Tests
that require a fixture skip with a clear message when it is absent, so the
suite stays green on fresh clones.

## Project structure

- `src/` — React frontend
- `src-tauri/src/commands.rs` — Tauri command handlers / IPC surface
- `src-tauri/src/lib.rs`, `main.rs` — app setup and command registration
- `src-tauri/src/tray.rs` — system tray + menu
- `src-tauri/src/web_server/` — embedded HTTP server, OPDS feed, and the embedded web UI SPA
- `folio-core/src/` — reusable Rust crate: `db`, `models`, `error`, `paths`, parsers (`epub`, `pdf`, `cbz`, `cbr`, `mobi`), `page_cache`, `enrichment`, providers, `opds`, `openlibrary`, `backup`, `sync`, `storage`, `search`
- `e2e/` — Playwright web-UI end-to-end tests
- `src-tauri/examples/web_e2e_server.rs` — seeded web-server harness the e2e suite runs against
- `docs/` — user-facing docs and roadmap

## CI

![CI](https://github.com/mikedamoiseau/folio/actions/workflows/ci.yml/badge.svg)

## License

MIT

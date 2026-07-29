# Folio Web Server API

Folio embeds an HTTP server that lets you browse and read your library from any device on the local network.

## Getting Started

1. Open **Settings > Remote Access** in the desktop app
2. Set a PIN and click **Save PIN**
3. Click **Start Server**
4. Scan the QR code or type the URL on your phone/tablet

Default port: **7788** (configurable).

## Authentication

### PIN Login

```
POST /api/auth
Content-Type: application/json

{ "pin": "1234" }
```

Returns `{ "token": "uuid" }` and sets an `HttpOnly` cookie (`folio_session`).

Rate limited: 5 attempts per 5 minutes per IP. Returns `429 Too Many Requests` when exceeded.

### Session Cookie

After login, the `folio_session` cookie is sent automatically by browsers. Valid for 24 hours.

### HTTP Basic Auth (OPDS clients)

For OPDS reader apps (KOReader, Calibre, etc.) that don't support cookie-based auth:

```
Authorization: Basic base64(any_username:your_pin)
```

The username is ignored; only the password (PIN) is checked.

### No PIN Mode

If no PIN is configured, all endpoints are accessible without authentication.

---

## JSON API

### Books

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/books` | List books. Supports `?q=` search, `?series=` filter, `?want_to_read=true` (show only books flagged "want to read"; presence-only — any other value or omission leaves the filter off), `?sort=` (`date_added` \| `title` \| `author` \| `last_read` \| `rating`), and pagination via `?limit=&offset=` — the response carries an `X-Total-Count` header with the post-filter total. Omitting `limit` returns the full filtered/sorted list unchanged (backward-compatible). |
| GET | `/api/books/:id` | Get a single book by ID |
| GET | `/api/books/:id/cover` | Cover image (binary). Add `?size=thumb` for a downscaled thumbnail (falls back to the full cover if a thumbnail can't be generated). |
| GET | `/api/books/:id/download` | Download the original file |
| GET | `/api/books/continue-reading` | Most-recently-read, in-progress books for the home "Continue Reading" shelf. Supports `?limit=` (default 12, max 50). |
| GET | `/api/series` | List of series (name + book count) |

### EPUB Content

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/books/:id/chapters` | Table of contents |
| GET | `/api/books/:id/chapters/:index` | Chapter HTML (sanitized, images rewritten) |
| GET | `/api/books/:id/images/:chapter/:filename` | Inline EPUB image |

### PDF / CBZ / CBR Pages

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/books/:id/pages/:index` | Page image (JPEG for PDF, original format for comics). Optional `?width=` (64–2048, clamped): PDF renders at that width; JPEG/PNG comic pages downscale to it (never upscale) and re-encode as JPEG. GIF and WebP pages are always served unchanged (resizing would drop animation frames), as are pages that fail to decode. Invalid or duplicate `width` values are ignored — PDFs then render at the server's default width (1200 px), comics return their original bytes. |
| GET | `/api/books/:id/page-count` | Returns `{ "count": N }` |

### Reading Progress

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/books/:id/progress` | Current reading progress for a book (`null` if none saved) |
| PUT | `/api/books/:id/progress` | Save reading progress. Body: `{ "chapter_index": N, "scroll_position": 0..1 }` (`chapter_index` doubles as the page index for PDF/CBZ/CBR) |
| GET | `/api/reading-progress` | All reading-progress rows, keyed by book ID — used to render progress badges on library grid cards |
| GET | `/api/books/:id/bookmarks` | List a book's bookmarks (oldest first). 404 if the book is unknown |
| POST | `/api/books/:id/bookmarks` | Create a bookmark. Body: `{ "chapter_index": N, "scroll_position": 0..1, "note"?: string }`. Returns the created bookmark (201). Persisted regardless of private mode |
| PUT | `/api/books/:id/bookmarks/:bookmark_id` | Rename a bookmark. Body: `{ "name": string \| null }` (empty/whitespace clears the name; truncated to 100 chars). 404 if the bookmark isn't in this book |
| DELETE | `/api/books/:id/bookmarks/:bookmark_id` | Soft-delete a bookmark (idempotent; 204). Scoped to the book, so it can't delete another book's bookmark |

### Highlights

Bodies and responses are **camelCase** (matching the `Highlight` model's serialization), unlike the snake_case bookmark bodies. Offsets are UTF-16 code-unit offsets into the chapter's plain text. EPUB/MOBI only in the web reader.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/books/:id/highlights` | List a book's live (not soft-deleted) highlights. 404 if the book is unknown |
| POST | `/api/books/:id/highlights` | Create a highlight. Body: `{ "chapterIndex": N, "text": string, "color": string, "startOffset": N, "endOffset": N, "note"?: string }`. Returns the created highlight (201). `400` on a malformed body, empty `text`, `endOffset <= startOffset`, an unknown `color`, or a `note` over 2000 characters. Colors are limited to the five reader swatches (`#f6c445`, `#7bc47f`, `#6ba3d6`, `#e88baf`, `#e8a55d`). Persisted regardless of private mode; emits the same `HighlightCreated` event as the desktop app |
| PUT | `/api/books/:id/highlights/:highlight_id` | Update a highlight's note and/or color. Body must contain at least one of `note` (string, or `null` to clear) and `color`. Absent keys are left unchanged. 404 if the highlight isn't a live highlight of this book |
| DELETE | `/api/books/:id/highlights/:highlight_id` | Soft-delete a highlight (idempotent; 204). Scoped to the book, so it can't delete another book's highlight |

### Want to Read

| Method | Endpoint | Description |
|--------|----------|-------------|
| PUT | `/api/books/:id/want-to-read` | Set the manual "want to read" flag. Body: `{ "want_to_read": true \| false }`. Returns `400` on a malformed body and `404` for an unknown book. Combine with `GET /api/books?want_to_read=true` to list flagged books. |

### Collections

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/collections` | List all collections |
| GET | `/api/collections/:id/books` | Books in a collection |

### Profiles

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/profiles` | List profiles: `[{ "name": string, "active": bool, "locked": bool, "switchable": bool }]`, sorted by name. `locked` = has a stored profile lock; `switchable` = no lock, or already unlocked on the desktop this session. |
| POST | `/api/profile` | Switch the active profile. Body: `{ "name": string }` (must be `Content-Type: application/json`). Returns `{ "active": name }`. |

`POST /api/profile` status codes:

| Status | Meaning |
|--------|---------|
| `200` | Switched. The active profile changed for **every** client — the desktop app and all other web/OPDS sessions share one active profile. |
| `400` | Malformed or non-JSON body. |
| `404` | No such profile. |
| `423 Locked` | The profile has a lock and hasn't been unlocked on the desktop this session. The profile password is never accepted over HTTP, so there is nothing to retry with — unlock it once in the desktop app. |
| `401` | Not authenticated. |
| `503` | This server has no profile backend (only happens in test harnesses). |

Switching changes the served library, so refetch anything cached client-side
afterwards. Book ids are per-profile, so clients that cache book content by id
must scope that cache by profile name.

Every response carries an `x-folio-profile` header identifying the active profile
(the profile name, hex-encoded — arbitrary names can't be sent verbatim in a
header). It is meant for comparison, not decoding: record the value your client
first sees, and when a later response differs, the active profile moved (another
client switched it, or the desktop did) and anything you hold keyed by book id is
stale. The bundled web UI reloads itself on that signal.

### System

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/health` | Health check (always 200, no auth required) |

---

## OPDS Catalog

Compatible with KOReader, Calibre, Moon+ Reader, and other OPDS clients.

| Endpoint | Description |
|----------|-------------|
| GET `/opds` | Root navigation feed |
| GET `/opds/all` | All books (paginated, 50 per page, `?page=N`) |
| GET `/opds/new` | 25 most recently added books |
| GET `/opds/collections/:id` | Books in a collection |
| GET `/opds/search?q=term` | Search by title or author |

OPDS feeds use Atom XML. Pagination uses `rel="next"` links.

### OPDS Client Configuration

- **URL:** `http://<your-ip>:7788/opds`
- **Auth:** HTTP Basic (username: anything, password: your PIN)

---

## Web UI

Open `http://<your-ip>:7788/` in a browser for a built-in reading interface. It matches the desktop app's design (warm paper/terracotta palette, serif/sans type) and behavior:

- PIN login screen, light/dark/system theme toggle, and keyboard shortcuts (`/` to focus search, grid/reader navigation, a shortcuts overlay)
- Paginated, infinite-scroll book grid with server-side search, series/collection filters, and sort — fast even on large libraries
- Home shelves for "Continue Reading" and "Recently Added", with reading-progress badges on grid and shelf cards
- Book detail page with a progress bar and Continue / Start-over
- EPUB reader with chapter navigation (neighbouring chapters are prefetched in the background, so turning to the next chapter on a phone is instant); PDF/CBZ/CBR page-image reader with animated swipe page-turns on touch devices (reduced-motion aware)
- Table-of-contents navigation for reflowable books: the reader consumes `GET /api/books/:id/chapters` and offers a **Contents** panel (replacing the numeric chapter slider) to jump to any chapter; degrades to a plain chapter label when the TOC has ≤1 entry or can't be fetched
- Adjustable reading typography for reflowable books (EPUB/MOBI) via an **Aa** toolbar control: font size, line spacing, reading font (Lora, Literata, DM Sans, OpenDyslexic), and column width. Settings are stored client-side (one `folio-web-typography` localStorage key, global across books) and reading position is preserved across the reflow. The four faces are embedded, content-addressed `woff2` served from `/fonts/*.woff2` as public, `immutable` shell assets and precached by the service worker (best-effort, so a font hiccup never blocks the SW install)
- Reading progress syncs back to the library, so a book picks up where a desktop or other device session left off
- Installable as a PWA (web app manifest, service worker) and supports iOS "Add to Home Screen". The service worker only registers on a secure context (`https` or `localhost`), so offline shell caching does not activate over a plain-HTTP LAN URL — Add-to-Home-Screen and the manifest still work there
- **Save for offline** (secure context only): per-book download into browser storage (Cache Storage for content, IndexedDB for the manifest/progress queue). When the server is unreachable the app boots into a library of downloaded books and reads them fully offline; progress made offline syncs back with a compare-then-push rule on reconnect; evicted downloads are detected and pruned on next launch. The service worker serves saved-book requests network-first (cache only on failure/offline), so online auth and freshness are unchanged
- Loading skeletons, friendly empty states, and broken-cover placeholders

All assets are embedded in the app (no CDN dependencies). The app shell works offline once cached by the service worker; API content is served network-first and only falls back to a per-book offline cache for books the user explicitly saved.

---

## Security

- PIN hashed with SHA-256, stored in OS keychain
- Session tokens: UUID v4, 24-hour TTL, HttpOnly + SameSite=Strict cookies
- Rate limiting: 5 failed login attempts per 5 min per IP
- CSP headers on all responses
- EPUB HTML sanitized with ammonia (no scripts, no event handlers)
- Path traversal protection on image endpoints
- File downloads streamed (no memory exhaustion)
- Server binds to `0.0.0.0` (all interfaces) for LAN access
- A locked profile can never be entered over HTTP: the profile password is not
  accepted by any endpoint, and `POST /api/profile` refuses a locked profile
  with `423` unless it was already unlocked in the desktop app this session
- `POST /api/profile` requires a JSON body. Combined with the `SameSite=Strict`
  session cookie, that closes cross-site switching: a cross-site `fetch` with
  `Content-Type: application/json` needs a CORS preflight this server never
  answers, and the form-encoded POST that *would* skip the preflight is
  rejected with `400` — which matters because HTTP Basic credentials (accepted
  on every `/api` path for OPDS clients) are replayed cross-site by browsers,
  unlike the cookie

## Tauri Commands

For the desktop frontend (React):

```typescript
invoke<string>("web_server_start", { port: 7788 })  // returns URL
invoke("web_server_stop")
invoke<WebServerStatus>("web_server_status")         // { running, url, port }
invoke("web_server_set_pin", { pin: "1234" })
invoke<string>("web_server_get_qr")                  // SVG string
```

# Feature #22: Built-in Web Server for Remote Library Access

## Context

Folio is a Tauri v2 desktop ebook reader. Users want to browse and read their library from other devices (phone, tablet) on their local network without installing the desktop app. This feature embeds an HTTP server in the desktop app that serves a read-only web UI, a JSON API, and an OPDS catalog endpoint.

## Scope

- HTTP server on configurable port (default 7788), bound to 0.0.0.0
- PIN-based authentication (stored in OS keychain)
- JSON REST API for library data and book content
- OPDS Atom XML catalog at `/opds` for OPDS reader apps
- Embedded web UI (vanilla HTML/CSS/JS) for browsing + reading
- Settings UI in SettingsPanel.tsx with toggle, PIN, QR code
- Serves the currently-active profile's library
- Read-only (no imports, no metadata editing from web)

## Files to Create

| File | Purpose |
|------|---------|
| `src-tauri/src/web_server/mod.rs` | Server lifecycle, shared state, local IP detection |
| `src-tauri/src/web_server/auth.rs` | PIN storage (keyring), session tokens, auth middleware |
| `src-tauri/src/web_server/api.rs` | JSON API route handlers |
| `src-tauri/src/web_server/opds_feed.rs` | OPDS Atom XML generation + routes |
| `src-tauri/src/web_server/web_ui.rs` | Embedded static file serving |
| `src-tauri/src/web_server/static/index.html` | Web UI shell |
| `src-tauri/src/web_server/static/app.js` | Web UI vanilla JS SPA |
| `src-tauri/src/web_server/static/app.css` | Web UI styles |

## Files to Modify

| File | Changes |
|------|---------|
| `src-tauri/Cargo.toml` | Add axum, tower-http, mime_guess, qrcode |
| `src-tauri/src/lib.rs` | Register `web_server` module, add Tauri commands, auto-start logic |
| `src-tauri/src/commands.rs` | Add `shared_active_pool` + `web_server_handle` to AppState, new Tauri commands, update `switch_profile` |
| `src-tauri/src/pdf.rs` | Add `get_page_image_bytes()` returning raw JPEG Vec<u8> |
| `src-tauri/src/cbz.rs` | Add `get_page_image_bytes()` returning raw image Vec<u8> + mime |
| `src-tauri/src/cbr.rs` | Add `get_page_image_bytes()` returning raw image Vec<u8> + mime |
| `src/components/SettingsPanel.tsx` | New "Remote Access" accordion section |
| `src/locales/en.json` | New i18n keys |
| `src/locales/fr.json` | New i18n keys |

## Architecture

### Shared State

Add two fields to `AppState` in `commands.rs`:

```rust
pub shared_active_pool: Arc<std::sync::Mutex<DbPool>>,
pub web_server_handle: std::sync::Mutex<Option<web_server::WebServerHandle>>,
```

`shared_active_pool` is initialized in `setup()` with the default pool. When `switch_profile` is called, it also updates this pool. The web server clones the `Arc` and reads from it on each request.

`WebServerHandle` holds a shutdown channel + JoinHandle + URL/port info.

### Web Server State (axum)

```rust
#[derive(Clone)]
pub struct WebState {
    pub pool: Arc<std::sync::Mutex<DbPool>>,
    pub data_dir: PathBuf,
    pub pin_hash: Arc<std::sync::Mutex<Option<String>>>,
    pub sessions: Arc<std::sync::Mutex<HashMap<String, std::time::Instant>>>,
}
```

### Authentication

- PIN stored via `keyring` crate (pattern from `backup.rs`): service=`carrel-web-server`, user=`pin`, value=SHA-256 hash
- `POST /api/auth` with `{ "pin": "1234" }` returns `{ "token": "uuid-v4" }`
- Token stored in `sessions` map (24h TTL)
- Middleware checks `Authorization: Bearer <token>` header or `carrel_session` cookie
- OPDS routes also accept HTTP Basic Auth (`Authorization: Basic base64(any:pin)`) for reader app compatibility
- Routes exempt from auth: `POST /api/auth`, `GET /` (login page only when unauthenticated)

### Endpoints

**JSON API:**
- `GET /api/books` — library listing (supports `?q=` search)
- `GET /api/books/:id` — single book
- `GET /api/books/:id/cover` — cover image (binary)
- `GET /api/books/:id/chapters` — TOC (EPUB)
- `GET /api/books/:id/chapters/:index` — chapter HTML (EPUB, images rewritten to HTTP URLs)
- `GET /api/books/:id/images/:chapter/:filename` — EPUB inline image from disk
- `GET /api/books/:id/pages/:index` — page image binary (PDF/CBZ/CBR)
- `GET /api/books/:id/page-count` — total pages
- `GET /api/books/:id/download` — original file download
- `GET /api/collections` — collections list
- `GET /api/collections/:id/books` — books in collection

**OPDS:**
- `GET /opds` — root navigation feed
- `GET /opds/all` — all books acquisition feed
- `GET /opds/new` — recently added
- `GET /opds/collections/:id` — collection feed
- `GET /opds/search?q=` — search

**Web UI:**
- `GET /` — HTML shell (login or library)
- `GET /app.js`, `/app.css` — embedded assets

### Tauri Commands

```
web_server_start(port: Option<u16>) -> Result<String, String>  // returns URL
web_server_stop() -> Result<(), String>
web_server_status() -> Result<WebServerStatus, String>
web_server_set_pin(pin: String) -> Result<(), String>
web_server_get_qr() -> Result<String, String>  // SVG string
```

### EPUB Image URL Rewriting

Call existing `get_chapter_content_from_cache()` (extracts images to disk + returns HTML with `asset://localhost/` URLs), then post-process:

```rust
fn rewrite_asset_urls_to_http(html: &str, book_id: &str, chapter_index: usize) -> String
```

Replaces `asset://localhost/{encoded_path}` with `/api/books/{book_id}/images/{chapter_index}/{filename}`. The images are already on disk at `{data_dir}/images/{book_id}/{chapter_index}/{filename}`.

### Raw Bytes Variants

Add to `pdf.rs`, `cbz.rs`, `cbr.rs`:

```rust
pub fn get_page_image_bytes(path: &str, page_index: u32, ...) -> Result<(Vec<u8>, &'static str), String>
```

Returns `(bytes, mime_type)` instead of base64 data URI. Avoids encode+decode round-trip for web serving. Existing `get_page_image` can be refactored to call these internally.

For comics, `page_cache::get_cached_page()` already returns `(Vec<u8>, String)` — reuse directly.

### Local IP Detection

```rust
fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}
```

### Web UI Design

Minimal vanilla JS SPA with 4 views:
1. **Login** — PIN input, submit
2. **Library** — responsive book grid with covers, search bar, format filter
3. **Book detail** — cover, metadata, "Read" / "Download" buttons
4. **Reader** — EPUB: chapter nav + HTML content; PDF/Comic: page image viewer with prev/next

Embedded via `include_str!()`. Styles use a small embedded CSS (no CDN dependencies for LAN usage).

## Implementation Order

### Step 1: Dependencies + module skeleton
- Add axum, tower-http, mime_guess, qrcode to Cargo.toml
- Create `web_server/mod.rs` with WebState, WebServerHandle, start/stop stubs
- Register module in `lib.rs`
- Verify it compiles

### Step 2: AppState changes + Tauri commands
- Add `shared_active_pool` and `web_server_handle` to AppState
- Initialize in `setup()`
- Update `switch_profile` to sync shared pool
- Implement `web_server_start`, `web_server_stop`, `web_server_status`, `web_server_set_pin`, `web_server_get_qr`
- Register commands in invoke_handler

### Step 3: Auth middleware
- PIN storage via keyring in `auth.rs`
- Session token management
- axum middleware layer
- HTTP Basic Auth support for OPDS

### Step 4: JSON API
- All `/api/` routes in `api.rs`
- Book listing, covers, content serving
- EPUB chapter content with URL rewriting
- EPUB image serving from disk
- PDF/comic page serving (add `_bytes` variants)

### Step 5: OPDS server
- Atom XML generation in `opds_feed.rs`
- Navigation + acquisition feeds
- Search endpoint

### Step 6: Web UI
- Create static HTML/CSS/JS files
- Embed and serve via `web_ui.rs`
- Login, library grid, reader views

### Step 7: Frontend settings UI
- "Remote Access" accordion in SettingsPanel.tsx
- Toggle, PIN input, port, URL display, QR code
- i18n keys in en.json + fr.json

### Step 8: Polish
- Auto-start if previously enabled (check `web_server_enabled` setting in `setup()`)
- Graceful shutdown on app exit
- Port-in-use error handling
- Activity log entries for server start/stop

## Verification

1. `cargo test` — existing tests still pass
2. `cargo clippy -- -D warnings` — no warnings
3. `npm run type-check` — frontend compiles
4. Manual: enable remote access in settings, open URL on phone browser, browse library, read a book
5. Manual: point KOReader or Calibre at `http://ip:port/opds`, browse catalog, download a book
6. Manual: switch profiles in desktop app, verify web server reflects new library
